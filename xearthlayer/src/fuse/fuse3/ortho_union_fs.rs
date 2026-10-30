//! Consolidated ortho union FUSE filesystem.
//!
//! This module provides a FUSE filesystem that presents all ortho sources
//! (patches AND regional packages) as a single unified view. It uses
//! [`OrthoUnionIndex`] to merge files with priority-based collision resolution.
//!
//! # Architecture
//!
//! ```text
//! ~/.xearthlayer/
//! ├── patches/                  ─┐
//! │   ├── A_KDEN_Mesh/           │
//! │   └── B_KLAX_Mesh/           │
//! └── packages/                  │   OrthoUnionIndex
//!     ├── na_ortho/              ├────────────────────► Fuse3OrthoUnionFS
//!     ├── eu_ortho/              │                              │
//!     └── sa_ortho/             ─┘                              ▼
//!                                                     FUSE Mount Point
//!                                                Custom Scenery/zzXEL_ortho/
//! ```
//!
//! # Priority Resolution
//!
//! Sources are sorted alphabetically by `sort_key`:
//! 1. Patches: `_patches/{folder_name}` (underscore sorts first)
//! 2. Packages: `{region}` (e.g., "eu", "na", "sa")
//!
//! First source wins on collision.
//!
//! # DDS Texture Generation
//!
//! When X-Plane requests a DDS texture:
//! 1. Check if the texture exists in any source → passthrough read
//! 2. If not, parse the filename for coordinates → generate via DdsHandler
//!
//! This ensures sources can include pre-built textures, but XEL generates
//! missing textures dynamically using its configured imagery provider.

use super::inode::InodeManager;
use super::shared::{to_file_attr, FileAttrBuilder, VirtualDdsConfig, TTL};
use super::types::{Fuse3Error, Fuse3Result};
use crate::executor::{DdsClient, StorageConcurrencyLimiter};
use crate::fuse::support::attrs::{FileAttributes, NodeKind};
use crate::fuse::support::core::OrthoUnionCore;
use crate::fuse::support::error::FsError;
use crate::fuse::support::node::FsNode;
use crate::geo_index::GeoIndex;
use crate::night::LitTextureSource;
use crate::ortho_union::OrthoUnionIndex;
use crate::pinned_budget::PinnedTileBudget;
use crate::prefetch::{PrefetchStateObserver, TileRequestCallback};
use fuse3::raw::prelude::*;
use fuse3::raw::reply::{
    DirectoryEntry, DirectoryEntryPlus, ReplyAttr, ReplyData, ReplyDirectory, ReplyDirectoryPlus,
    ReplyEntry, ReplyInit, ReplyOpen, ReplyStatFs,
};
use fuse3::raw::Filesystem;
use fuse3::{Errno, MountOptions, Result as Fuse3InternalResult};
use futures::stream::{self, Stream, StreamExt};
use std::ffi::{OsStr, OsString};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tracing::{debug, trace};

/// FUSE open flag: bypass kernel page cache for this file.
///
/// From `linux/fuse.h`: `#define FOPEN_DIRECT_IO (1 << 0)`
///
/// When set in `ReplyOpen::flags`, the kernel sends every `read()` through
/// the FUSE handler instead of serving from its page cache. Used for virtual
/// DDS files so that every X-Plane read is counted (`MetricEvent::FuseRead`)
/// and none is answered from a cache this filesystem cannot invalidate.
#[cfg_attr(target_os = "macos", allow(dead_code))]
const FOPEN_DIRECT_IO: u32 = 1;

/// Open flags reported for virtual DDS files.
///
/// Linux uses [`FOPEN_DIRECT_IO`]: every `read()` reaches this handler, so the
/// FUSE read counters are exact and the page cache never holds a second copy
/// of a tile the memory cache already has.
///
/// **macOS must not.** X-Plane memory-maps texture files, and macFUSE faults
/// with `EXC_BAD_ACCESS` when a `direct_io` file is `mmap`ed, because `mmap`
/// needs the page cache that `direct_io` bypasses. macOS therefore serves
/// virtual DDS through the page cache, trading per-read visibility for not
/// crashing the sim's texture loader. The one consequence is that the FUSE
/// read counters under-report there: a tile's cold first read reaches this
/// handler and repeat reads are answered by the kernel. Nothing functional
/// depends on seeing every read (#204): prefetch is driven by telemetry, and
/// the cold reads that trigger generation still arrive.
#[cfg(not(target_os = "macos"))]
const VIRTUAL_DDS_OPEN_FLAGS: u32 = FOPEN_DIRECT_IO;
#[cfg(target_os = "macos")]
const VIRTUAL_DDS_OPEN_FLAGS: u32 = 0;

/// Consolidated ortho union FUSE filesystem.
///
/// This filesystem merges all ortho sources (patches + regional packages) into
/// a single virtual view:
///
/// - Files from sources are passed through from their real locations
/// - DDS textures that don't exist are generated via the async pipeline
/// - Priority is determined by alphabetical `sort_key` ordering
///   - Patches (`_patches/*`) always sort before packages
///   - Packages sort alphabetically by region (eu < na < sa)
///
/// # Backend split (Phase 1)
///
/// All state and behaviour that is independent of the FUSE kernel lives in
/// [`OrthoUnionCore`](crate::fuse::support::core::OrthoUnionCore); this
/// struct is the thin FUSE adapter that adds the kernel identity (inodes)
/// and the FUSE init handshake parameters. The public API is unchanged by
/// the split — see `docs/dev/windows-backend-split.md` §4.5.
///
/// # Example
///
/// ```ignore
/// use xearthlayer::fuse::fuse3::Fuse3OrthoUnionFS;
/// use xearthlayer::ortho_union::OrthoUnionIndexBuilder;
///
/// let index = OrthoUnionIndexBuilder::new()
///     .with_patches_dir("/home/user/.xearthlayer/patches")
///     .add_packages(installed_packages)
///     .build()?;
///
/// let fs = Fuse3OrthoUnionFS::new(index, dds_handler, expected_dds_size);
/// fs.mount_spawned("/path/to/mountpoint").await?;
/// ```
pub struct Fuse3OrthoUnionFS {
    /// The backend-neutral core: state and logic shared with the WinFSP
    /// backend (Phase 2). See `docs/dev/windows-backend-split.md` §4.4.
    core: OrthoUnionCore,
    /// Inode manager for path mappings
    inode_manager: InodeManager,
    /// Maximum pending background FUSE requests (kernel limit).
    /// When set, overrides the kernel default (12) in the FUSE init handshake.
    fuse_max_background: Option<u16>,
    /// Congestion threshold for background FUSE requests (kernel limit).
    /// When set, overrides the kernel default (9) in the FUSE init handshake.
    fuse_congestion_threshold: Option<u16>,
}

// =============================================================================
// Adapter-side seam mappers (FUSE identity, ownership, and error policy)
// =============================================================================

/// Map the core's neutral error onto FUSE's (windows-backend-split.md §4.3).
fn errno_from(err: FsError) -> Errno {
    match err {
        FsError::NotFound => Errno::from(libc::ENOENT),
        FsError::NotADirectory => Errno::from(libc::ENOTDIR),
        FsError::Io(_) => Errno::from(libc::EIO),
    }
}

/// Whether a resolved core node is a real file of the union, as opposed to
/// a virtual directory or a generated texture.
fn is_real_file(node: &FsNode, attrs: &FileAttributes) -> bool {
    matches!(node, FsNode::RealFile(_)) && attrs.kind != NodeKind::Directory
}

/// Apply the adapter's identity and time policy to a core node's neutral
/// attributes (windows-backend-split.md §4.5): real files keep the
/// source's uid/gid and report `UNIX_EPOCH` as `ctime`; everything else
/// (virtual directories, generated textures, and file entries whose
/// metadata could not be read) is owned by the mounting process and
/// reports the current time.
fn file_attr(ino: u64, real_file: bool, attrs: &FileAttributes) -> FileAttr {
    if real_file {
        to_file_attr(ino, attrs, UNIX_EPOCH, attrs.uid, attrs.gid)
    } else {
        to_file_attr(
            ino,
            attrs,
            SystemTime::now(),
            unsafe { libc::getuid() },
            unsafe { libc::getgid() },
        )
    }
}

impl Fuse3OrthoUnionFS {
    /// Map a neutral core node onto the FUSE inode that represents it.
    fn inode_for(&self, node: &FsNode) -> u64 {
        match node {
            FsNode::VirtualDds(coords) => self.inode_manager.create_virtual_inode(coords.clone()),
            // Root is always inode 1.
            FsNode::Directory(p) if p.as_os_str().is_empty() => 1,
            _ => {
                let path = node.path().expect("Directory/RealFile nodes carry a path");
                self.inode_manager.get_or_create_inode(path)
            }
        }
    }

    /// Create a new consolidated ortho union filesystem.
    ///
    /// # Arguments
    ///
    /// * `index` - Pre-built union index of all ortho sources
    /// * `dds_client` - Client for DDS generation requests (daemon architecture)
    /// * `expected_dds_size` - Expected size of generated DDS files
    ///
    /// # Example
    ///
    /// ```ignore
    /// let fs = Fuse3OrthoUnionFS::new(index, dds_client, 11_174_016);
    /// ```
    pub fn new(
        index: OrthoUnionIndex,
        dds_client: Arc<dyn DdsClient>,
        expected_dds_size: usize,
    ) -> Self {
        // Use a virtual root path for the inode manager
        let virtual_root = PathBuf::from("/");

        Self {
            core: OrthoUnionCore::new(index, dds_client, expected_dds_size),
            inode_manager: InodeManager::new(virtual_root),
            fuse_max_background: None,
            fuse_congestion_threshold: None,
        }
    }

    /// Create with custom disk I/O limiter.
    ///
    /// Use this when you need to share a disk I/O limiter across multiple
    /// filesystems or customize the concurrency limits.
    pub fn with_disk_io_limiter(
        index: OrthoUnionIndex,
        dds_client: Arc<dyn DdsClient>,
        expected_dds_size: usize,
        disk_io_limiter: Arc<StorageConcurrencyLimiter>,
    ) -> Self {
        let virtual_root = PathBuf::from("/");

        Self {
            core: OrthoUnionCore::with_disk_io_limiter(
                index,
                dds_client,
                expected_dds_size,
                disk_io_limiter,
            ),
            inode_manager: InodeManager::new(virtual_root),
            fuse_max_background: None,
            fuse_congestion_threshold: None,
        }
    }

    /// Share the pinned-tile budget with the prefetch backpressure signal.
    ///
    /// Without this the filesystem accounts against a budget only it can see,
    /// which is correct for the memoisation cap but leaves prefetch blind to
    /// demand on the mount (#246). Passing the runtime's budget makes the same
    /// number answer both questions.
    pub fn with_pinned_budget(mut self, budget: Arc<PinnedTileBudget>) -> Self {
        self.core = self.core.with_pinned_budget(budget);
        self
    }

    /// Serve lit textures from `source` instead of the black default.
    pub fn with_lit_source(mut self, source: Arc<dyn LitTextureSource>) -> Self {
        self.core = self.core.with_lit_source(source);
        self
    }

    /// Set the timeout for DDS generation.
    ///
    /// After this timeout, a placeholder texture is returned to prevent
    /// X-Plane from blocking indefinitely.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.core = self.core.with_timeout(timeout);
        self
    }

    /// Set the callback for tile request tracking.
    ///
    /// This callback is invoked whenever a tile is requested, enabling
    /// the prefetch system to infer aircraft position from FUSE requests.
    pub fn with_tile_request_callback(mut self, callback: TileRequestCallback) -> Self {
        self.core = self.core.with_tile_request_callback(callback);
        self
    }

    /// Set the metrics client for reporting FUSE-level metrics.
    ///
    /// When set, reports coalesced requests and other FUSE-specific metrics
    /// to the metrics system.
    pub fn with_metrics(mut self, metrics: crate::metrics::MetricsClient) -> Self {
        self.core = self.core.with_metrics(metrics);
        self
    }

    /// Set the geospatial reference index for region-level ownership queries.
    ///
    /// When set, FUSE uses the GeoIndex to determine if a scenery file falls
    /// within a patch-owned DSF region. Files in patched regions are resolved
    /// only from patch sources, hiding package files that could conflict.
    pub fn with_geo_index(mut self, geo_index: Arc<GeoIndex>) -> Self {
        self.core = self.core.with_geo_index(geo_index);
        self
    }

    /// Set the prefetch state observer.
    ///
    /// When set, an on-demand generation in a region prefetch marked
    /// `Prefetched` or `NoCoverage` demotes that region.
    pub fn with_state_observer(mut self, observer: Arc<PrefetchStateObserver>) -> Self {
        self.core = self.core.with_state_observer(observer);
        self
    }

    /// Set the FUSE kernel background request limits.
    ///
    /// These values are sent to the kernel during the FUSE init handshake to
    /// control how many concurrent background requests (readahead, async reads)
    /// are allowed before throttling. The kernel defaults (12/9) are too low
    /// for X-Plane's concurrent scenery reads.
    pub fn with_fuse_limits(mut self, max_background: u16, congestion_threshold: u16) -> Self {
        self.fuse_max_background = Some(max_background);
        self.fuse_congestion_threshold = Some(congestion_threshold);
        self
    }

    /// Returns the disk I/O limiter for monitoring/metrics.
    pub fn disk_io_limiter(&self) -> &Arc<StorageConcurrencyLimiter> {
        self.core.disk_io_limiter()
    }

    /// Highest concurrent open handle count seen.
    pub fn peak_dds_handles(&self) -> u64 {
        self.core.peak_dds_handles()
    }

    /// Highest pinned tile byte total seen.
    pub fn peak_pinned_tile_bytes(&self) -> u64 {
        self.core.peak_pinned_tile_bytes()
    }

    /// Number of virtual DDS files currently open.
    pub fn open_dds_handles(&self) -> usize {
        self.core.open_dds_handles()
    }

    /// Bytes of DDS tiles currently pinned by open handles.
    pub fn pinned_tile_bytes(&self) -> u64 {
        self.core.pinned_tile_bytes()
    }

    /// Get the ortho union index.
    pub fn index(&self) -> &OrthoUnionIndex {
        self.core.index()
    }

    /// Mount the filesystem at the given path.
    ///
    /// This is a blocking operation that runs until the filesystem is unmounted.
    /// For non-blocking usage, see [`mount_spawned`](Self::mount_spawned).
    pub async fn mount(self, mountpoint: &str) -> Fuse3Result<super::types::MountHandle> {
        let mount_path = PathBuf::from(mountpoint);
        let mount_options = mount_options(&mount_path);

        // State the ceiling once, so a flight log is self-describing: a reader
        // comparing dds_pinned_peak_mb against it does not have to know the
        // constant, and does not have to guess which build it came from (#236).
        tracing::info!(
            dds_pinned_cap_mb = self.core.pinned().ceiling_bytes() / (1024 * 1024),
            dds_pinned_cap_tiles =
                self.core.pinned().ceiling_bytes() / self.core.virtual_dds_config().size().max(1),
            "Virtual DDS handle memoisation budget"
        );

        #[cfg(target_os = "linux")]
        let handle = fuse3::raw::Session::new(mount_options)
            .mount_with_unprivileged(self, mount_path)
            .await
            .map_err(|e| Fuse3Error::MountFailed(e.to_string()))?;

        #[cfg(not(target_os = "linux"))]
        let handle = fuse3::raw::Session::new(mount_options)
            .mount(self, mount_path)
            .await
            .map_err(|e| Fuse3Error::MountFailed(e.to_string()))?;

        Ok(super::types::MountHandle::new(handle))
    }

    /// Mount the filesystem as a spawned background task.
    ///
    /// Returns a handle that can be used to unmount the filesystem later.
    /// The filesystem runs in the background until unmounted.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let handle = fs.mount_spawned("/path/to/mountpoint").await?;
    /// // ... filesystem is running ...
    /// handle.unmount().await?;
    /// ```
    pub async fn mount_spawned(
        self,
        mountpoint: &str,
    ) -> Fuse3Result<super::types::SpawnedMountHandle> {
        let mount_path = PathBuf::from(mountpoint);
        let mount_options = mount_options(&mount_path);

        #[cfg(target_os = "linux")]
        let handle = fuse3::raw::Session::new(mount_options)
            .mount_with_unprivileged(self, mount_path.clone())
            .await
            .map_err(|e| Fuse3Error::MountFailed(e.to_string()))?;

        #[cfg(not(target_os = "linux"))]
        let handle = fuse3::raw::Session::new(mount_options)
            .mount(self, mount_path.clone())
            .await
            .map_err(|e| Fuse3Error::MountFailed(e.to_string()))?;

        Ok(super::types::SpawnedMountHandle::spawn_from_handle(
            handle, mount_path,
        ))
    }
}

/// Mount options shared by [`Fuse3OrthoUnionFS::mount`] and
/// [`Fuse3OrthoUnionFS::mount_spawned`].
fn mount_options(mount_path: &Path) -> MountOptions {
    let mut mount_options = MountOptions::default();
    mount_options.read_only(true);
    mount_options.force_readdir_plus(false);
    // Tell kernel we don't implement opendir - it should call readdir directly
    mount_options.no_open_dir_support(true);
    if let Some(volname) = volume_name_option(mount_path, cfg!(target_os = "macos")) {
        mount_options.custom_options(volname);
    }
    mount_options
}

/// The macFUSE `volname` option naming the volume after its mount-point
/// folder (#305).
///
/// Finder shows a mounted volume's name in place of the folder it is mounted
/// on, and macFUSE's default is `macFUSE Volume 0 (<process>)`, so without
/// this `zzXEL_ortho` appears in Custom Scenery under that name. `None` off
/// macOS, which has no volume names, and for a folder name containing a
/// comma, which macFUSE would split into a second option.
fn volume_name_option(mount_path: &Path, macos: bool) -> Option<String> {
    if !macos {
        return None;
    }
    let name = mount_path.file_name()?.to_str()?;
    (!name.contains(',')).then(|| format!("volname={name}"))
}

// =============================================================================
// Trait Implementations for Shared FUSE Functionality
// =============================================================================

impl FileAttrBuilder for Fuse3OrthoUnionFS {
    fn virtual_dds_config(&self) -> &VirtualDdsConfig {
        self.core.virtual_dds_config()
    }
}

// The `DdsRequestor` pipeline (parse, coalesce, submit, timeout, validate) is
// backend-neutral and lives on the core: `impl DdsRequestor for
// OrthoUnionCore` in `fuse::support::core` (windows-backend-split.md §3.3).

impl Filesystem for Fuse3OrthoUnionFS {
    async fn init(&self, _req: Request) -> Fuse3InternalResult<ReplyInit> {
        debug!(
            sources = self.core.index().source_count(),
            files = self.core.index().file_count(),
            fuse_max_background = ?self.fuse_max_background,
            fuse_congestion_threshold = ?self.fuse_congestion_threshold,
            "fuse3 ortho union: init"
        );
        let mut reply = ReplyInit::new(NonZeroU32::new(1024 * 1024).unwrap());
        reply.max_background = self.fuse_max_background;
        reply.congestion_threshold = self.fuse_congestion_threshold;
        Ok(reply)
    }

    async fn destroy(&self, _req: Request) {
        debug!("fuse3 ortho union: destroy");
    }

    async fn lookup(
        &self,
        _req: Request,
        parent: u64,
        name: &OsStr,
    ) -> Fuse3InternalResult<ReplyEntry> {
        trace!(parent = parent, name = ?name, "fuse3 ortho union: lookup");

        let parent_node = if parent == 1 {
            FsNode::root()
        } else {
            let path = self
                .inode_manager
                .get_path(parent)
                .ok_or(Errno::from(libc::ENOENT))?;
            FsNode::Directory(path)
        };

        // Resolution order, the water-mask guard and all are the core's
        // business (windows-backend-split.md §4.4).
        let name_str = name.to_string_lossy();
        let (node, attrs) = self
            .core
            .lookup(&parent_node, &name_str)
            .await
            .map_err(errno_from)?;
        let inode = self.inode_for(&node);

        Ok(ReplyEntry {
            ttl: TTL,
            attr: file_attr(inode, is_real_file(&node, &attrs), &attrs),
            generation: 0,
        })
    }

    async fn getattr(
        &self,
        _req: Request,
        ino: u64,
        _fh: Option<u64>,
        _flags: u32,
    ) -> Fuse3InternalResult<ReplyAttr> {
        trace!(ino = ino, "fuse3 ortho union: getattr");

        let node = if ino == 1 {
            FsNode::root()
        } else if InodeManager::is_virtual_inode(ino) {
            let coords = self
                .inode_manager
                .get_virtual_dds(ino)
                .ok_or(Errno::from(libc::ENOENT))?;
            FsNode::VirtualDds(coords)
        } else {
            // Path inodes: the kernel also addresses directory inodes as
            // paths, so this is RealFile for both; the core's attrs() sorts
            // directory vs. file out, as the op always did.
            let path = self
                .inode_manager
                .get_path(ino)
                .ok_or(Errno::from(libc::ENOENT))?;
            FsNode::RealFile(path)
        };

        let attrs = self.core.attrs(&node).await.map_err(errno_from)?;

        Ok(ReplyAttr {
            ttl: TTL,
            attr: file_attr(ino, is_real_file(&node, &attrs), &attrs),
        })
    }

    async fn read(
        &self,
        _req: Request,
        ino: u64,
        fh: u64,
        offset: u64,
        size: u32,
    ) -> Fuse3InternalResult<ReplyData> {
        trace!(
            ino = ino,
            offset = offset,
            size = size,
            "fuse3 ortho union: read"
        );

        let node = if InodeManager::is_virtual_inode(ino) {
            let coords = self
                .inode_manager
                .get_virtual_dds(ino)
                .ok_or(Errno::from(libc::ENOENT))?;
            FsNode::VirtualDds(coords)
        } else {
            let path = self
                .inode_manager
                .get_path(ino)
                .ok_or(Errno::from(libc::ENOENT))?;
            FsNode::RealFile(path)
        };

        let data = self
            .core
            .read(&node, fh, offset, size)
            .await
            .map_err(errno_from)?;

        Ok(ReplyData { data })
    }

    async fn readdir(
        &self,
        _req: Request,
        ino: u64,
        _fh: u64,
        offset: i64,
    ) -> Fuse3InternalResult<
        ReplyDirectory<impl Stream<Item = Fuse3InternalResult<DirectoryEntry>> + Send + '_>,
    > {
        tracing::debug!(ino = ino, offset = offset, "FUSE readdir called");

        let dir_node = if ino == 1 {
            FsNode::root()
        } else {
            let path = self
                .inode_manager
                .get_path(ino)
                .ok_or(Errno::from(libc::ENOENT))?;
            FsNode::Directory(path)
        };

        let entries = self
            .core
            .list_directory(&dir_node)
            .await
            .map_err(errno_from)?;

        // An entry's offset is its 1-based position in the full list, so a
        // re-read at the offset resumes where the previous read stopped.
        // Plain readdir stays I/O-free: file entries' placeholder attrs are
        // never sent, only name/kind/inode/offset are.
        let entries: Vec<_> = entries
            .into_iter()
            .enumerate()
            .skip(offset as usize)
            .map(|(index, entry)| {
                Ok(DirectoryEntry {
                    inode: self.inode_for(&entry.node),
                    kind: if entry.is_dir {
                        FileType::Directory
                    } else {
                        FileType::RegularFile
                    },
                    name: OsString::from(entry.name),
                    offset: (index + 1) as i64,
                })
            })
            .collect();

        Ok(ReplyDirectory {
            entries: stream::iter(entries).boxed(),
        })
    }

    async fn readdirplus(
        &self,
        _req: Request,
        ino: u64,
        _fh: u64,
        offset: u64,
        _lock_owner: u64,
    ) -> Fuse3InternalResult<
        ReplyDirectoryPlus<impl Stream<Item = Fuse3InternalResult<DirectoryEntryPlus>> + Send + '_>,
    > {
        tracing::debug!(ino = ino, offset = offset, "FUSE readdirplus called");

        let dir_node = if ino == 1 {
            FsNode::root()
        } else {
            let path = self
                .inode_manager
                .get_path(ino)
                .ok_or(Errno::from(libc::ENOENT))?;
            FsNode::Directory(path)
        };

        let dir_entries = self
            .core
            .list_directory(&dir_node)
            .await
            .map_err(errno_from)?;

        // File entries arrive with placeholder attributes; readdirplus is
        // the one op that upgrades them to real metadata -- accurate sizes
        // are what DSF and other passthrough files need reported to
        // X-Plane. Plain readdir above stays I/O-free, as it always did.
        let mut entries: Vec<DirectoryEntryPlus> = Vec::with_capacity(dir_entries.len());
        for (index, entry) in dir_entries.into_iter().enumerate() {
            let inode = self.inode_for(&entry.node);
            let (attrs, real_file) = if entry.is_dir {
                (entry.attrs, false)
            } else {
                match self.core.attrs(&entry.node).await {
                    Ok(real_attrs) => (real_attrs, true),
                    // Fallback if metadata read fails: the placeholder, with
                    // the virtual policy, as before.
                    Err(_) => (entry.attrs, false),
                }
            };

            entries.push(DirectoryEntryPlus {
                inode,
                generation: 0,
                kind: if entry.is_dir {
                    FileType::Directory
                } else {
                    FileType::RegularFile
                },
                name: OsString::from(entry.name),
                offset: (index + 1) as i64,
                attr: file_attr(inode, real_file, &attrs),
                entry_ttl: TTL,
                attr_ttl: TTL,
            });
        }

        // Skip entries based on offset
        let entries: Vec<_> = entries.into_iter().skip(offset as usize).map(Ok).collect();

        Ok(ReplyDirectoryPlus {
            entries: stream::iter(entries).boxed(),
        })
    }

    async fn open(&self, _req: Request, inode: u64, _flags: u32) -> Fuse3InternalResult<ReplyOpen> {
        if !InodeManager::is_virtual_inode(inode) {
            // Real passthrough files: use default kernel caching, no handle state.
            return Ok(ReplyOpen { fh: 0, flags: 0 });
        }

        // Virtual DDS files: bypass the kernel page cache so every read()
        // reaches this handler (see VIRTUAL_DDS_OPEN_FLAGS).
        let Some(coords) = self.inode_manager.get_virtual_dds(inode) else {
            // An inode we cannot map to a tile gets no handle rather than an
            // error: read() is where a genuinely unknown one becomes ENOENT.
            return Ok(ReplyOpen {
                fh: 0,
                flags: VIRTUAL_DDS_OPEN_FLAGS,
            });
        };

        // Handle allocation -- lit-source passthrough, the pinned-bytes budget
        // refusal, and the gauges around them -- is the core's business.
        let fh = self.core.open(&FsNode::VirtualDds(coords));

        Ok(ReplyOpen {
            fh,
            flags: VIRTUAL_DDS_OPEN_FLAGS,
        })
    }

    async fn release(
        &self,
        _req: Request,
        _inode: u64,
        fh: u64,
        _flags: u32,
        _lock_owner: u64,
        _flush: bool,
    ) -> Fuse3InternalResult<()> {
        // Drop the memoised tile; without this every open file would pin
        // 11.17 MB for the life of the mount.
        self.core.release(fh);
        Ok(())
    }

    async fn opendir(
        &self,
        _req: Request,
        ino: u64,
        _flags: u32,
    ) -> Fuse3InternalResult<ReplyOpen> {
        tracing::debug!(ino = ino, "FUSE opendir called");
        // Return success with fh=0 for stateless directory I/O
        Ok(ReplyOpen { fh: 0, flags: 0 })
    }

    async fn access(&self, _req: Request, _ino: u64, _mask: u32) -> Fuse3InternalResult<()> {
        Ok(())
    }

    async fn flush(
        &self,
        _req: Request,
        _ino: u64,
        _fh: u64,
        _lock_owner: u64,
    ) -> Fuse3InternalResult<()> {
        Ok(())
    }

    async fn fsync(
        &self,
        _req: Request,
        _ino: u64,
        _fh: u64,
        _datasync: bool,
    ) -> Fuse3InternalResult<()> {
        Ok(())
    }

    async fn statfs(&self, _req: Request, _ino: u64) -> Fuse3InternalResult<ReplyStatFs> {
        let stats = self.core.statfs();
        Ok(ReplyStatFs {
            blocks: stats.blocks,
            bfree: stats.bfree,
            bavail: stats.bavail,
            files: stats.files,
            ffree: stats.ffree,
            bsize: stats.bsize,
            namelen: stats.namelen,
            frsize: stats.frsize,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::shared::chunk_to_tile_coords;
    use super::*;
    use crate::coord::TileCoord;
    use crate::executor::{DdsClientError, Priority};
    use crate::fuse::support::requestor::DdsRequestor;
    use crate::ortho_union::OrthoUnionIndexBuilder;
    use crate::runtime::{DdsResponse, JobRequest, RequestOrigin};
    use bytes::Bytes;
    use semver::Version;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::sync::{mpsc, oneshot};
    use tokio_util::sync::CancellationToken;
    use xearthlayer_package::{InstalledPackage, Package, PackageType};

    /// #305: Finder shows a mounted volume's name in place of its mount-point
    /// folder, and macFUSE's default is `macFUSE Volume 0 (<process>)`.
    #[test]
    fn on_macos_the_volume_is_named_after_the_mount_point_folder() {
        let mount = Path::new("/X-Plane 12/Custom Scenery/zzXEL_ortho");

        assert_eq!(
            volume_name_option(mount, true).as_deref(),
            Some("volname=zzXEL_ortho")
        );
    }

    #[test]
    fn off_macos_there_is_no_volume_name() {
        let mount = Path::new("/X-Plane 12/Custom Scenery/zzXEL_ortho");

        assert_eq!(volume_name_option(mount, false), None);
    }

    /// macFUSE splits its option list on commas, so a name containing one
    /// would leak its tail in as a separate option.
    #[test]
    fn a_folder_name_with_a_comma_is_not_passed_as_a_volume_name() {
        let mount = Path::new("/Custom Scenery/zzXEL_ortho,allow_other");

        assert_eq!(volume_name_option(mount, true), None);
    }

    /// Mock DdsClient for testing
    struct MockDdsClient {
        tx: mpsc::Sender<JobRequest>,
    }

    impl MockDdsClient {
        fn new() -> (Arc<Self>, mpsc::Receiver<JobRequest>) {
            let (tx, rx) = mpsc::channel(10);
            (Arc::new(Self { tx }), rx)
        }
    }

    impl DdsClient for MockDdsClient {
        fn submit(&self, request: JobRequest) -> Result<(), DdsClientError> {
            self.tx
                .try_send(request)
                .map_err(|_| DdsClientError::ChannelClosed)
        }

        fn request_dds(
            &self,
            tile: TileCoord,
            cancellation: CancellationToken,
        ) -> oneshot::Receiver<DdsResponse> {
            let (tx, rx) = oneshot::channel();
            let request = JobRequest {
                tile,
                priority: Priority::ON_DEMAND,
                cancellation,
                response_tx: Some(tx),
                origin: RequestOrigin::Fuse,
                created: std::time::Instant::now(),
            };
            let _ = self.tx.try_send(request);
            rx
        }

        fn request_dds_with_options(
            &self,
            tile: TileCoord,
            priority: Priority,
            origin: RequestOrigin,
            cancellation: CancellationToken,
        ) -> oneshot::Receiver<DdsResponse> {
            let (tx, rx) = oneshot::channel();
            let request = JobRequest {
                tile,
                priority,
                cancellation,
                response_tx: Some(tx),
                origin,
                created: std::time::Instant::now(),
            };
            let _ = self.tx.try_send(request);
            rx
        }

        fn is_connected(&self) -> bool {
            !self.tx.is_closed()
        }
    }

    fn create_test_client() -> Arc<dyn DdsClient> {
        let (client, _rx) = MockDdsClient::new();
        client
    }

    fn create_test_patch(temp: &TempDir, name: &str) {
        let patch_dir = temp.path().join(name);
        std::fs::create_dir_all(patch_dir.join("Earth nav data/+30-120")).unwrap();
        std::fs::write(
            patch_dir.join("Earth nav data/+30-120/+33-119.dsf"),
            b"fake dsf",
        )
        .unwrap();
        std::fs::create_dir_all(patch_dir.join("terrain")).unwrap();
        std::fs::write(patch_dir.join("terrain/test.ter"), b"fake terrain").unwrap();
    }

    fn create_test_package(temp: &TempDir, region: &str) -> InstalledPackage {
        let pkg_dir = temp.path().join(format!("{}_ortho", region));
        std::fs::create_dir_all(pkg_dir.join("Earth nav data/+40-080")).unwrap();
        std::fs::write(
            pkg_dir.join("Earth nav data/+40-080/+40-074.dsf"),
            b"pkg dsf",
        )
        .unwrap();
        std::fs::create_dir_all(pkg_dir.join("terrain")).unwrap();
        std::fs::write(pkg_dir.join("terrain/package.ter"), b"pkg terrain").unwrap();

        InstalledPackage::new(
            Package::new(region, PackageType::Ortho, Version::new(1, 0, 0)),
            &pkg_dir,
        )
    }

    #[test]
    fn test_ortho_union_fs_creation_with_patches() {
        let temp = TempDir::new().unwrap();
        create_test_patch(&temp, "TestPatch");

        let index = OrthoUnionIndexBuilder::new()
            .with_patches_dir(temp.path())
            .build()
            .unwrap();

        let client = create_test_client();
        let fs = Fuse3OrthoUnionFS::new(index, client, 1024);

        assert_eq!(fs.index().source_count(), 1);
        assert!(fs.index().file_count() > 0);
    }

    #[test]
    fn test_ortho_union_fs_creation_with_packages() {
        let temp = TempDir::new().unwrap();
        let na_pkg = create_test_package(&temp, "na");

        let index = OrthoUnionIndexBuilder::new()
            .add_package(na_pkg)
            .build()
            .unwrap();

        let client = create_test_client();
        let fs = Fuse3OrthoUnionFS::new(index, client, 1024);

        assert_eq!(fs.index().source_count(), 1);
        assert!(fs.index().file_count() > 0);
    }

    #[test]
    fn test_ortho_union_fs_creation_with_both() {
        let temp = TempDir::new().unwrap();

        // Create patches directory
        let patches_dir = temp.path().join("patches");
        std::fs::create_dir_all(&patches_dir).unwrap();
        create_test_patch(&TempDir::new_in(&patches_dir).unwrap(), "TestPatch");

        // Create package
        let na_pkg = create_test_package(&temp, "na");

        let index = OrthoUnionIndexBuilder::new()
            .with_patches_dir(&patches_dir)
            .add_package(na_pkg)
            .build()
            .unwrap();

        let client = create_test_client();
        let fs = Fuse3OrthoUnionFS::new(index, client, 1024);

        // At least 1 source (package is guaranteed)
        assert!(fs.index().source_count() >= 1);
    }

    #[test]
    fn test_virtual_dds_config() {
        let config = VirtualDdsConfig::new(11_174_016);
        assert_eq!(config.size(), 11_174_016);
        assert_eq!(config.blksize(), 4096);
        assert_eq!(config.blocks(), 2729);
    }

    #[test]
    fn test_chunk_to_tile_coords() {
        let coords = crate::fuse::DdsFilename {
            row: 160000,
            col: 84000,
            zoom: 20,
            map_type: "BI".to_string(),
            kind: crate::fuse::TextureKind::Ortho,
        };

        let tile = chunk_to_tile_coords(&coords);

        assert_eq!(tile.row, 10000);
        assert_eq!(tile.col, 5250);
        assert_eq!(tile.zoom, 16);
    }

    #[test]
    fn test_with_disk_io_limiter() {
        let temp = TempDir::new().unwrap();
        let na_pkg = create_test_package(&temp, "na");

        let index = OrthoUnionIndexBuilder::new()
            .add_package(na_pkg)
            .build()
            .unwrap();

        let client = create_test_client();
        let limiter = Arc::new(StorageConcurrencyLimiter::with_defaults("test"));

        let fs = Fuse3OrthoUnionFS::with_disk_io_limiter(index, client, 1024, limiter.clone());

        assert!(Arc::ptr_eq(fs.disk_io_limiter(), &limiter));
    }

    #[test]
    fn test_with_timeout() {
        let temp = TempDir::new().unwrap();
        let na_pkg = create_test_package(&temp, "na");

        let index = OrthoUnionIndexBuilder::new()
            .add_package(na_pkg)
            .build()
            .unwrap();

        let client = create_test_client();
        let fs = Fuse3OrthoUnionFS::new(index, client, 1024).with_timeout(Duration::from_secs(60));

        assert_eq!(fs.core.generation_timeout(), Duration::from_secs(60));
    }

    /// Test that readdirplus returns correct file sizes for passthrough files.
    ///
    /// This is a regression test for a bug where readdirplus was using virtual_dds_attr()
    /// for ALL non-directory files, causing passthrough files (like DSF) to report
    /// incorrect sizes (~11MB instead of actual size). X-Plane reads DSF files based
    /// on the reported size, so incorrect sizes caused dsf_ErrMissingAtom crashes.
    #[tokio::test]
    async fn test_readdirplus_returns_correct_file_sizes_for_passthrough_files() {
        use fuse3::raw::Filesystem;

        let temp = TempDir::new().unwrap();

        // Create a package with a DSF file of known size
        let pkg_dir = temp.path().join("test_ortho");
        let dsf_dir = pkg_dir.join("Earth nav data/+40-080");
        std::fs::create_dir_all(&dsf_dir).unwrap();

        // Create a DSF file with specific content (size = 27 bytes)
        let dsf_content = b"this is fake dsf content!!";
        let dsf_path = dsf_dir.join("+40-074.dsf");
        std::fs::write(&dsf_path, dsf_content).unwrap();

        let pkg = InstalledPackage::new(
            Package::new("test", PackageType::Ortho, Version::new(1, 0, 0)),
            &pkg_dir,
        );

        let index = OrthoUnionIndexBuilder::new()
            .add_package(pkg)
            .build()
            .unwrap();

        let client = create_test_client();
        let fs = Fuse3OrthoUnionFS::new(index, client, 1024);

        // Get the inode for the directory containing the DSF
        let dsf_dir_virtual = std::path::Path::new("Earth nav data/+40-080");
        let dir_inode = fs.inode_manager.get_or_create_inode(dsf_dir_virtual);

        // Create a fake request (uid/gid don't matter for this test)
        let req = fuse3::raw::Request {
            unique: 1,
            uid: 1000,
            gid: 1000,
            pid: 1000,
        };

        // Call readdirplus (req, ino, fh, offset, lock_owner)
        let result = fs.readdirplus(req, dir_inode, 0, 0, 0).await.unwrap();

        // Collect entries from the stream
        let entries: Vec<_> = result.entries.collect().await;

        // Find the DSF file entry
        let dsf_entry = entries
            .iter()
            .filter_map(|e| e.as_ref().ok())
            .find(|e| e.name.to_string_lossy().ends_with(".dsf"))
            .expect("DSF file should be in directory listing");

        // The critical assertion: file size should match actual file content size,
        // NOT the virtual DDS size (~11MB)
        let actual_size = dsf_content.len() as u64;
        let virtual_dds_size = 11_174_016u64; // VirtualDdsConfig::default().size()

        assert_eq!(
            dsf_entry.attr.size, actual_size,
            "DSF file size should be {} bytes (actual), not {} bytes (virtual DDS)",
            actual_size, dsf_entry.attr.size
        );
        assert_ne!(
            dsf_entry.attr.size, virtual_dds_size,
            "DSF file should NOT have virtual DDS size"
        );
    }

    // ========================================================================
    // Virtual DDS handle reuse (#234)
    // ========================================================================

    /// A `DdsClient` that answers instantly and counts how many tile requests
    /// it was asked for. The count is the point: the kernel splits one X-Plane
    /// texture read into 12-23 ranged calls, and each one used to become a
    /// separate executor round trip.
    struct CountingDdsClient {
        requests: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl CountingDdsClient {
        fn new() -> (Arc<Self>, Arc<std::sync::atomic::AtomicUsize>) {
            let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            (
                Arc::new(Self {
                    requests: Arc::clone(&requests),
                }),
                requests,
            )
        }

        /// A DDS that passes `validate_dds_or_placeholder`: right magic, right
        /// length, and a body whose byte at index i is `i as u8` so a slice
        /// proves which window was served.
        fn valid_dds() -> Vec<u8> {
            let mut data: Vec<u8> = (0..crate::fuse::EXPECTED_DDS_SIZE)
                .map(|i| i as u8)
                .collect();
            data[0..4].copy_from_slice(b"DDS ");
            data
        }
    }

    impl DdsClient for CountingDdsClient {
        fn submit(&self, _request: JobRequest) -> Result<(), DdsClientError> {
            Ok(())
        }

        fn request_dds(
            &self,
            _tile: TileCoord,
            _cancellation: CancellationToken,
        ) -> oneshot::Receiver<DdsResponse> {
            self.requests
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(DdsResponse::cache_hit(
                Self::valid_dds().into(),
                Duration::from_millis(1),
            ));
            rx
        }

        fn request_dds_with_options(
            &self,
            tile: TileCoord,
            _priority: Priority,
            _origin: RequestOrigin,
            cancellation: CancellationToken,
        ) -> oneshot::Receiver<DdsResponse> {
            self.request_dds(tile, cancellation)
        }

        fn is_connected(&self) -> bool {
            true
        }
    }

    fn virtual_dds_fixture() -> (
        Fuse3OrthoUnionFS,
        Arc<std::sync::atomic::AtomicUsize>,
        TempDir,
    ) {
        let temp = TempDir::new().unwrap();
        let pkg_dir = temp.path().join("test_ortho");
        std::fs::create_dir_all(pkg_dir.join("Earth nav data/+40-080")).unwrap();
        let pkg = InstalledPackage::new(
            Package::new("test", PackageType::Ortho, Version::new(1, 0, 0)),
            &pkg_dir,
        );
        let index = OrthoUnionIndexBuilder::new()
            .add_package(pkg)
            .build()
            .unwrap();

        let (client, requests) = CountingDdsClient::new();
        let fs = Fuse3OrthoUnionFS::new(index, client, crate::fuse::EXPECTED_DDS_SIZE);
        (fs, requests, temp)
    }

    fn virtual_inode(fs: &Fuse3OrthoUnionFS) -> u64 {
        fs.inode_manager
            .create_virtual_inode(crate::fuse::DdsFilename {
                row: 12754,
                col: 5279,
                zoom: 16,
                map_type: "BI".to_string(),
                kind: crate::fuse::TextureKind::Ortho,
            })
    }

    fn lit_name() -> &'static std::ffi::OsStr {
        std::ffi::OsStr::new("25712_12960_BI16_LIT.dds")
    }

    fn textures_inode(fs: &Fuse3OrthoUnionFS) -> u64 {
        fs.inode_manager
            .get_or_create_inode(std::path::Path::new("textures"))
    }

    #[tokio::test]
    async fn test_lit_lookup_reports_the_minimal_black_size_and_its_own_inode() {
        use fuse3::raw::Filesystem;
        let (fs, _requests, _temp) = virtual_dds_fixture();
        let parent = textures_inode(&fs);

        let lit = fs.lookup(test_request(), parent, lit_name()).await.unwrap();
        assert_eq!(lit.attr.size, 152);

        let ortho = fs
            .lookup(
                test_request(),
                parent,
                std::ffi::OsStr::new("25712_12960_BI16.dds"),
            )
            .await
            .unwrap();
        assert_eq!(
            ortho.attr.size,
            crate::fuse::EXPECTED_DDS_SIZE as u64,
            "ortho unchanged"
        );
        assert_ne!(lit.attr.ino, ortho.attr.ino);
    }

    #[tokio::test]
    async fn test_lit_getattr_matches_lookup() {
        use fuse3::raw::Filesystem;
        let (fs, _requests, _temp) = virtual_dds_fixture();
        let entry = fs
            .lookup(test_request(), textures_inode(&fs), lit_name())
            .await
            .unwrap();
        let attr = fs
            .getattr(test_request(), entry.attr.ino, None, 0)
            .await
            .unwrap();
        assert_eq!(attr.attr.size, 152);
    }

    #[tokio::test]
    async fn test_lit_read_serves_black_without_touching_the_executor() {
        use fuse3::raw::Filesystem;
        let (fs, requests, _temp) = virtual_dds_fixture();
        let ino = fs
            .lookup(test_request(), textures_inode(&fs), lit_name())
            .await
            .unwrap()
            .attr
            .ino;

        let opened = fs.open(test_request(), ino, 0).await.unwrap();
        assert_eq!(opened.fh, 0, "lit files hold no memoising handle");
        assert!(fs.core.dds_handles().is_empty());

        let data = fs
            .read(test_request(), ino, opened.fh, 0, 1 << 20)
            .await
            .unwrap()
            .data;
        assert_eq!(data, crate::night::minimal_black_dds());
        assert_eq!(
            requests.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "no DDS request"
        );
    }

    #[tokio::test]
    async fn test_request_dds_impl_serves_black_for_a_lit_name_without_a_request() {
        let (fs, requests, _temp) = virtual_dds_fixture();
        let coords = crate::fuse::DdsFilename {
            row: 25712,
            col: 12960,
            zoom: 16,
            map_type: "BI".to_string(),
            kind: crate::fuse::TextureKind::Lit,
        };

        let data = fs.core.request_dds_impl(&coords).await;

        assert_eq!(data, crate::night::minimal_black_dds());
        assert_eq!(data.len(), 152);
        assert_eq!(
            requests.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a lit name must never reach the DDS client"
        );
    }

    #[tokio::test]
    async fn test_real_lit_file_on_disk_wins_over_the_virtual_lit_texture() {
        use fuse3::raw::Filesystem;
        let temp = TempDir::new().unwrap();
        let pkg_dir = temp.path().join("test_ortho");
        std::fs::create_dir_all(pkg_dir.join("Earth nav data/+40-080")).unwrap();
        std::fs::create_dir_all(pkg_dir.join("textures")).unwrap();
        std::fs::write(pkg_dir.join("textures/25712_12960_BI16_LIT.dds"), [7u8; 10]).unwrap();
        let pkg = InstalledPackage::new(
            Package::new("test", PackageType::Ortho, Version::new(1, 0, 0)),
            &pkg_dir,
        );
        let index = OrthoUnionIndexBuilder::new()
            .add_package(pkg)
            .build()
            .unwrap();
        let (client, _requests) = CountingDdsClient::new();
        let fs = Fuse3OrthoUnionFS::new(index, client, crate::fuse::EXPECTED_DDS_SIZE);

        let entry = fs
            .lookup(test_request(), textures_inode(&fs), lit_name())
            .await
            .unwrap();

        assert_eq!(entry.attr.size, 10, "the real file, not the 152-byte black");
        assert!(
            entry.attr.ino < crate::fuse::support::inode::VIRTUAL_INODE_BASE,
            "a real file has a real inode"
        );
    }

    #[tokio::test]
    async fn test_lit_source_is_injectable() {
        use fuse3::raw::Filesystem;
        struct Fixed;
        impl crate::night::LitTextureSource for Fixed {
            fn size<'a>(
                &'a self,
                _n: &'a crate::fuse::DdsFilename,
            ) -> futures::future::BoxFuture<'a, u64> {
                Box::pin(async { 1000 })
            }
            fn read<'a>(
                &'a self,
                _n: &'a crate::fuse::DdsFilename,
                _o: u64,
                _l: u32,
            ) -> futures::future::BoxFuture<'a, Bytes> {
                Box::pin(async { Bytes::from_static(b"lit") })
            }
        }
        let (fs, _requests, _temp) = virtual_dds_fixture();
        let fs = fs.with_lit_source(Arc::new(Fixed));
        let entry = fs
            .lookup(test_request(), textures_inode(&fs), lit_name())
            .await
            .unwrap();
        assert_eq!(entry.attr.size, 1000);
        let data = fs
            .read(test_request(), entry.attr.ino, 0, 0, 16)
            .await
            .unwrap()
            .data;
        assert_eq!(&data[..], b"lit");
    }

    /// Fixture wired to an observable metrics channel.
    fn virtual_dds_fixture_with_metrics() -> (
        Fuse3OrthoUnionFS,
        mpsc::UnboundedReceiver<crate::metrics::MetricEvent>,
        TempDir,
    ) {
        let (fs, _requests, temp) = virtual_dds_fixture();
        let (tx, rx) = mpsc::unbounded_channel();
        (
            fs.with_metrics(crate::metrics::MetricsClient::new(tx)),
            rx,
            temp,
        )
    }

    fn budget_exhausted_events(
        rx: &mut mpsc::UnboundedReceiver<crate::metrics::MetricEvent>,
    ) -> usize {
        let mut n = 0;
        while let Ok(event) = rx.try_recv() {
            if matches!(
                event,
                crate::metrics::MetricEvent::FuseHandleBudgetExhausted
            ) {
                n += 1;
            }
        }
        n
    }

    /// Past the ceiling `open()` hands back fh 0, so `read()` resolves per
    /// call. Slower, but correct -- the bound costs throughput, never data.
    #[tokio::test]
    async fn test_open_past_budget_refuses_memoisation() {
        use fuse3::raw::Filesystem;

        let (fs, _rx, _temp) = virtual_dds_fixture_with_metrics();
        let ino = virtual_inode(&fs);

        // One tile short of the ceiling: pinned + expected must exceed it.
        fs.core.pinned().pin(fs.core.pinned().ceiling_bytes());

        let opened = fs.open(test_request(), ino, 0).await.unwrap();
        assert_eq!(opened.fh, 0, "refused opens get no memoising handle");
        assert_eq!(
            opened.flags, VIRTUAL_DDS_OPEN_FLAGS,
            "the file must still bypass the page cache"
        );
        assert!(
            fs.core.dds_handles().is_empty(),
            "a refused open must not leave a handle behind"
        );
    }

    /// #236: the fallback was visible only under `--debug`, so a flight could
    /// not tell "the cap engaged" from "the #234 fix stopped working".
    #[tokio::test]
    async fn test_open_past_budget_counts_every_refusal() {
        use fuse3::raw::Filesystem;

        let (fs, mut rx, _temp) = virtual_dds_fixture_with_metrics();
        let ino = virtual_inode(&fs);
        fs.core.pinned().pin(fs.core.pinned().ceiling_bytes());

        for _ in 0..3 {
            let opened = fs.open(test_request(), ino, 0).await.unwrap();
            assert_eq!(opened.fh, 0);
        }

        // Every refusal counts: the counter measures lost memoisations, and
        // one event could not be told from an assignment.
        assert_eq!(budget_exhausted_events(&mut rx), 3);
    }

    /// The warning latches. Once the cap engages it engages for every open in
    /// the burst, and a per-open warn would bury the log it is meant to serve.
    #[tokio::test]
    async fn test_budget_warning_is_latched() {
        use fuse3::raw::Filesystem;

        let (fs, _rx, _temp) = virtual_dds_fixture_with_metrics();
        let ino = virtual_inode(&fs);
        fs.core.pinned().pin(fs.core.pinned().ceiling_bytes());

        // Assert the claim, not the flag. A flag assertion passes even if the
        // warn is emitted unconditionally, so long as something sets it.
        fs.open(test_request(), ino, 0).await.unwrap();
        assert!(
            !fs.core.claim_budget_warning(),
            "the first refusal must have consumed the one-shot claim"
        );

        for _ in 0..3 {
            fs.open(test_request(), ino, 0).await.unwrap();
            assert!(
                !fs.core.claim_budget_warning(),
                "later refusals must not re-arm the warning"
            );
        }
    }

    /// The latch in isolation: true exactly once, whatever the caller does.
    #[test]
    fn test_claim_budget_warning_is_one_shot() {
        let (fs, _requests, _temp) = virtual_dds_fixture();

        assert!(fs.core.claim_budget_warning(), "first claim must succeed");
        for _ in 0..5 {
            assert!(
                !fs.core.claim_budget_warning(),
                "every later claim must fail"
            );
        }
    }

    /// One open, many reads, one tile request.
    ///
    /// This is the defect: `read()` re-entered the executor on every ranged
    /// call, so a single texture cost 12-23 round trips, each of which cloned
    /// the whole 11.17 MB tile twice. Fails against the pre-fix handler with
    /// one request per read.
    #[tokio::test]
    async fn test_virtual_dds_resolves_once_per_open() {
        use fuse3::raw::Filesystem;

        let (fs, requests, _temp) = virtual_dds_fixture();
        let ino = virtual_inode(&fs);

        let opened = fs.open(test_request(), ino, 0).await.unwrap();

        const CHUNK: u32 = 1024 * 1024;
        for i in 0..5u64 {
            let reply = fs
                .read(test_request(), ino, opened.fh, i * CHUNK as u64, CHUNK)
                .await
                .unwrap();
            assert_eq!(reply.data.len(), CHUNK as usize, "read {i} short");
        }

        assert_eq!(
            requests.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "five ranged reads of one open file must cost one tile request"
        );
    }

    /// Serving from the memoised tile must still return the right window.
    #[tokio::test]
    async fn test_virtual_dds_reads_serve_the_requested_window() {
        use fuse3::raw::Filesystem;

        let (fs, _requests, _temp) = virtual_dds_fixture();
        let ino = virtual_inode(&fs);
        let opened = fs.open(test_request(), ino, 0).await.unwrap();

        // Byte i of the tile is `i as u8`, except the four magic bytes.
        for offset in [4096u64, 1_000_000, 8_000_000] {
            let reply = fs
                .read(test_request(), ino, opened.fh, offset, 256)
                .await
                .unwrap();
            assert_eq!(reply.data.len(), 256);
            assert_eq!(reply.data[0], (offset % 256) as u8, "offset {offset}");
            assert_eq!(reply.data[255], ((offset + 255) % 256) as u8);
        }
    }

    /// The last window is short, and reads past the tile return nothing.
    #[tokio::test]
    async fn test_virtual_dds_read_at_and_past_tile_end() {
        use fuse3::raw::Filesystem;

        let (fs, _requests, _temp) = virtual_dds_fixture();
        let ino = virtual_inode(&fs);
        let opened = fs.open(test_request(), ino, 0).await.unwrap();
        let size = crate::fuse::EXPECTED_DDS_SIZE as u64;

        let tail = fs
            .read(test_request(), ino, opened.fh, size - 100, 4096)
            .await
            .unwrap();
        assert_eq!(tail.data.len(), 100);

        let past = fs
            .read(test_request(), ino, opened.fh, size, 4096)
            .await
            .unwrap();
        assert!(past.data.is_empty());
    }

    /// A registered virtual inode gets a real, non-zero handle.
    #[tokio::test]
    async fn test_open_registered_virtual_dds_returns_a_handle() {
        use fuse3::raw::Filesystem;

        let (fs, _requests, _temp) = virtual_dds_fixture();
        let ino = virtual_inode(&fs);

        let reply = fs.open(test_request(), ino, 0).await.unwrap();

        assert_ne!(reply.fh, 0, "a known tile must get a memoising handle");
        assert_eq!(reply.flags, VIRTUAL_DDS_OPEN_FLAGS);
        assert_eq!(fs.open_dds_handles(), 1);
        // open() must not produce the tile: generation can take up to the
        // configured timeout, and X-Plane may open a file it never reads.
        assert_eq!(fs.pinned_tile_bytes(), 0);
    }

    /// Peaks must survive the drain that hides the current gauges.
    ///
    /// The live counts are sampled on open, on tile production and on release,
    /// so a periodic reader almost always catches them just after a release and
    /// sees zero -- which is exactly what the first KDEN run reported while 31
    /// files were open. `MAX_PINNED_TILE_BYTES` has to be sized against the
    /// ceiling, so the ceiling is tracked separately.
    #[tokio::test]
    async fn test_virtual_dds_handle_peaks_outlive_the_current_gauges() {
        use fuse3::raw::Filesystem;

        let (fs, _requests, _temp) = virtual_dds_fixture();

        let mut opened = Vec::new();
        for col in 0..3u32 {
            let ino = fs
                .inode_manager
                .create_virtual_inode(crate::fuse::DdsFilename {
                    row: 12754,
                    col: 5279 + col,
                    zoom: 16,
                    map_type: "BI".to_string(),
                    kind: crate::fuse::TextureKind::Ortho,
                });
            let reply = fs.open(test_request(), ino, 0).await.unwrap();
            fs.read(test_request(), ino, reply.fh, 0, 4096)
                .await
                .unwrap();
            opened.push((ino, reply.fh));
        }

        assert_eq!(fs.open_dds_handles(), 3);
        let pinned_at_peak = fs.pinned_tile_bytes();
        assert_eq!(pinned_at_peak, 3 * crate::fuse::EXPECTED_DDS_SIZE as u64);

        for (ino, fh) in opened {
            fs.release(test_request(), ino, fh, 0, 0, false)
                .await
                .unwrap();
        }

        // Current gauges drain...
        assert_eq!(fs.open_dds_handles(), 0);
        assert_eq!(fs.pinned_tile_bytes(), 0);
        // ...peaks do not.
        assert_eq!(fs.peak_dds_handles(), 3);
        assert_eq!(fs.peak_pinned_tile_bytes(), pinned_at_peak);
    }

    /// The amplification metric must reflect the fix, not the old shape.
    ///
    /// Only the read that produces the tile materialises it; later reads slice
    /// what is already held and obtain nothing. Counting the tile on every
    /// call would pin `fuse_dds_alloc_mb / fuse_dds_read_mb` near 12-23 and
    /// report the fix as ineffective.
    #[tokio::test]
    async fn test_virtual_dds_metric_charges_only_what_it_delivers() {
        use fuse3::raw::Filesystem;

        let temp = TempDir::new().unwrap();
        let pkg_dir = temp.path().join("test_ortho");
        std::fs::create_dir_all(pkg_dir.join("Earth nav data/+40-080")).unwrap();
        let pkg = InstalledPackage::new(
            Package::new("test", PackageType::Ortho, Version::new(1, 0, 0)),
            &pkg_dir,
        );
        let index = OrthoUnionIndexBuilder::new()
            .add_package(pkg)
            .build()
            .unwrap();
        let (client, _requests) = CountingDdsClient::new();
        let (metrics_tx, mut metrics_rx) = tokio::sync::mpsc::unbounded_channel();
        let fs = Fuse3OrthoUnionFS::new(index, client, crate::fuse::EXPECTED_DDS_SIZE)
            .with_metrics(crate::metrics::MetricsClient::new(metrics_tx));
        let ino = virtual_inode(&fs);
        let opened = fs.open(test_request(), ino, 0).await.unwrap();

        const CHUNK: u32 = 1024 * 1024;
        const READS: u64 = 4;
        for i in 0..READS {
            fs.read(test_request(), ino, opened.fh, i * CHUNK as u64, CHUNK)
                .await
                .unwrap();
        }

        let mut returned_total = 0u64;
        let mut materialised_total = 0u64;
        let mut events = 0;
        while let Ok(event) = metrics_rx.try_recv() {
            if let crate::metrics::MetricEvent::FuseRead {
                returned,
                materialised,
                virtual_dds,
            } = event
            {
                assert!(virtual_dds);
                returned_total += returned;
                materialised_total += materialised;
                events += 1;
            }
        }

        assert_eq!(events, READS, "one FuseRead per read call");
        assert_eq!(returned_total, READS * CHUNK as u64);
        // #237 changed the contract. The handler now takes a refcount on the
        // tile and slices it, so it obtains exactly what it returns and the
        // amplification ratio is 1.0. Charging the tile here instead would
        // report allocation that no longer happens.
        assert_eq!(
            materialised_total, returned_total,
            "a zero-copy handler obtains exactly what it delivers"
        );
    }

    /// The ratio is only honest if the delivery really is zero-copy, and the
    /// counter cannot prove that -- it is charged by code we control. Pointer
    /// identity can: the reply must alias the memoised tile, not copy it.
    #[tokio::test]
    async fn test_virtual_dds_read_aliases_the_memoised_tile() {
        use fuse3::raw::Filesystem;

        let (fs, _requests, _temp) = virtual_dds_fixture();
        let ino = virtual_inode(&fs);
        let opened = fs.open(test_request(), ino, 0).await.unwrap();

        const OFFSET: u64 = 4096;
        let reply = fs
            .read(test_request(), ino, opened.fh, OFFSET, 256)
            .await
            .unwrap();

        let handle = fs
            .core
            .dds_handles()
            .get(&opened.fh)
            .expect("handle is live");
        let tile = handle.tile.get().expect("first read produced the tile");

        // The reply's buffer must sit inside the tile's allocation, at the
        // requested offset -- not be a fresh 256-byte copy of it.
        let expected = unsafe { tile.as_ptr().add(OFFSET as usize) };
        assert_eq!(
            reply.data.as_ptr(),
            expected,
            "the reply must alias the tile at the read offset, not copy from it"
        );
    }

    /// `release()` must drop the tile, or every open file leaks 11.17 MB.
    #[tokio::test]
    async fn test_virtual_dds_release_frees_the_handle() {
        use fuse3::raw::Filesystem;

        let (fs, _requests, _temp) = virtual_dds_fixture();
        let ino = virtual_inode(&fs);
        let opened = fs.open(test_request(), ino, 0).await.unwrap();
        fs.read(test_request(), ino, opened.fh, 0, 4096)
            .await
            .unwrap();

        assert_eq!(fs.open_dds_handles(), 1);
        assert!(fs.pinned_tile_bytes() > 0);

        fs.release(test_request(), ino, opened.fh, 0, 0, false)
            .await
            .unwrap();

        assert_eq!(fs.open_dds_handles(), 0);
        assert_eq!(fs.pinned_tile_bytes(), 0);
    }

    /// A read with no handle still works, one request per read.
    ///
    /// The kernel always calls `open()` first, but a handler that returns
    /// ENOENT when it does not recognise a handle would turn any bookkeeping
    /// slip into a black texture.
    #[tokio::test]
    async fn test_virtual_dds_read_without_a_handle_falls_back() {
        use fuse3::raw::Filesystem;

        let (fs, requests, _temp) = virtual_dds_fixture();
        let ino = virtual_inode(&fs);

        for _ in 0..3 {
            let reply = fs.read(test_request(), ino, 0, 0, 4096).await.unwrap();
            assert_eq!(reply.data.len(), 4096);
        }

        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    // ========================================================================
    // Passthrough read amplification (#233)
    // ========================================================================

    /// Build a one-package index containing a single file of `size` bytes and
    /// return the mounted FS, its inode, and the metrics receiver.
    fn passthrough_fixture(
        temp: &TempDir,
        size: usize,
    ) -> (
        Fuse3OrthoUnionFS,
        u64,
        tokio::sync::mpsc::UnboundedReceiver<crate::metrics::MetricEvent>,
    ) {
        // Byte i = i as u8, so a slice's content proves which range was served.
        let content: Vec<u8> = (0..size).map(|i| i as u8).collect();
        passthrough_fixture_with(temp, &content)
    }

    /// Build a one-package index containing a single file with `content` and
    /// return the mounted FS, its inode, and the metrics receiver.
    fn passthrough_fixture_with(
        temp: &TempDir,
        content: &[u8],
    ) -> (
        Fuse3OrthoUnionFS,
        u64,
        tokio::sync::mpsc::UnboundedReceiver<crate::metrics::MetricEvent>,
    ) {
        let pkg_dir = temp.path().join("test_ortho");
        let dsf_dir = pkg_dir.join("Earth nav data/+40-080");
        std::fs::create_dir_all(&dsf_dir).unwrap();
        std::fs::write(dsf_dir.join("+40-074.dsf"), content).unwrap();

        let pkg = InstalledPackage::new(
            Package::new("test", PackageType::Ortho, Version::new(1, 0, 0)),
            &pkg_dir,
        );
        let index = OrthoUnionIndexBuilder::new()
            .add_package(pkg)
            .build()
            .unwrap();

        let (metrics_tx, metrics_rx) = tokio::sync::mpsc::unbounded_channel();
        let fs = Fuse3OrthoUnionFS::new(index, create_test_client(), 1024)
            .with_metrics(crate::metrics::MetricsClient::new(metrics_tx));
        let inode = fs
            .inode_manager
            .get_or_create_inode(std::path::Path::new("Earth nav data/+40-080/+40-074.dsf"));

        (fs, inode, metrics_rx)
    }

    /// A DSF written by the publisher is a 7z container (#124). FUSE never
    /// parses DSF content, so the bytes X-Plane receives must be exactly the
    /// bytes on disk, signature included.
    #[tokio::test]
    async fn passthrough_serves_7z_dsf_byte_identical() {
        use std::io::Write;

        use sevenz_rust2::{ArchiveEntry, ArchiveWriter};

        let temp = TempDir::new().unwrap();
        let raw_bytes: Vec<u8> = (0..64 * 1024u32).map(|i| ((i / 5) % 253) as u8).collect();
        let compressed = temp.path().join("compressed.dsf");
        // Built here rather than by the publisher's DsfCompressor: this test
        // only needs an opaque 7z blob, and the runtime must not depend on the
        // publisher even in tests (#284).
        let mut writer = ArchiveWriter::new(std::io::BufWriter::new(
            std::fs::File::create(&compressed).unwrap(),
        ))
        .unwrap();
        writer
            .push_archive_entry(
                ArchiveEntry::new_file("raw.dsf"),
                Some(std::io::Cursor::new(raw_bytes)),
            )
            .unwrap();
        writer.finish().unwrap().flush().unwrap();
        let expected = std::fs::read(&compressed).unwrap();
        // The 7z signature, which is what X-Plane checks before decoding.
        const SEVENZ_MAGIC: [u8; 6] = [0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C];
        assert!(expected.starts_with(&SEVENZ_MAGIC));

        let (fs, ino, _rx) = passthrough_fixture_with(&temp, &expected);

        let reply = fs
            .read(test_request(), ino, 0, 0, expected.len() as u32)
            .await
            .unwrap();
        assert_eq!(reply.data.len(), expected.len());
        assert_eq!(&reply.data[..], expected.as_slice());
    }

    fn test_request() -> fuse3::raw::Request {
        fuse3::raw::Request {
            unique: 1,
            uid: 1000,
            gid: 1000,
            pid: 1000,
        }
    }

    /// #246: the filesystem must account against the budget it was given, not
    /// the one it made for itself, or prefetch reads a gauge nothing writes to.
    #[test]
    fn with_pinned_budget_accounts_against_the_injected_budget() {
        let budget = Arc::new(PinnedTileBudget::with_ceiling(1000));
        let (fs, _rx, _temp) = virtual_dds_fixture_with_metrics();
        let fs = fs.with_pinned_budget(Arc::clone(&budget));

        budget.pin(400);

        assert_eq!(
            fs.pinned_tile_bytes(),
            400,
            "the filesystem must read the injected budget"
        );
        assert!(
            budget.would_exceed(601),
            "and the ceiling must come from it too"
        );
    }

    /// Drain the single `FuseRead` event a read is expected to emit.
    fn drain_one_read(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::metrics::MetricEvent>,
    ) -> (u64, u64, bool) {
        match rx
            .try_recv()
            .expect("read() must emit exactly one FuseRead")
        {
            crate::metrics::MetricEvent::FuseRead {
                returned,
                materialised,
                virtual_dds,
            } => (returned, materialised, virtual_dds),
            other => panic!("expected FuseRead, got {other:?}"),
        }
    }

    /// A ranged read must not materialise more than it returns.
    ///
    /// The kernel caps every FUSE read at 1 MiB, so X-Plane asking for a whole
    /// 4 MiB file still arrives as several ranged calls. Answering each by
    /// reading the entire file makes the handler move N times what it delivers
    /// -- 12-23x at DDS size, up to 238x on the largest installed ortho DSF.
    ///
    /// This test fails against the pre-fix handler, which reports
    /// materialised == 4 MiB for a 64 KiB request.
    #[tokio::test]
    async fn test_passthrough_read_materialises_only_the_requested_range() {
        use fuse3::raw::Filesystem;

        const FILE_SIZE: usize = 4 * 1024 * 1024;
        const READ_SIZE: u32 = 64 * 1024;

        let temp = TempDir::new().unwrap();
        let (fs, inode, mut metrics_rx) = passthrough_fixture(&temp, FILE_SIZE);

        let reply = fs
            .read(test_request(), inode, 0, 0, READ_SIZE)
            .await
            .unwrap();
        assert_eq!(reply.data.len(), READ_SIZE as usize);

        let (returned, materialised, virtual_dds) = drain_one_read(&mut metrics_rx);
        assert!(
            !virtual_dds,
            "a real file on disk is not a virtual DDS tile"
        );
        assert_eq!(returned, READ_SIZE as u64);
        assert_eq!(
            materialised, returned,
            "serving {READ_SIZE} bytes must not read {materialised} bytes from a {FILE_SIZE}-byte file"
        );
    }

    /// The same guarantee mid-file: seeking must not re-read the prefix.
    #[tokio::test]
    async fn test_passthrough_read_mid_file_does_not_read_the_prefix() {
        use fuse3::raw::Filesystem;

        const FILE_SIZE: usize = 1024 * 1024;
        const OFFSET: u64 = 768 * 1024;
        const READ_SIZE: u32 = 128 * 1024;

        let temp = TempDir::new().unwrap();
        let (fs, inode, mut metrics_rx) = passthrough_fixture(&temp, FILE_SIZE);

        let reply = fs
            .read(test_request(), inode, 0, OFFSET, READ_SIZE)
            .await
            .unwrap();

        // Content proves the right window was served, not just the right length.
        assert_eq!(reply.data.len(), READ_SIZE as usize);
        assert_eq!(reply.data[0], (OFFSET as usize % 256) as u8);

        let (returned, materialised, _) = drain_one_read(&mut metrics_rx);
        assert_eq!(returned, READ_SIZE as u64);
        assert_eq!(materialised, returned);
    }

    /// A range that runs off the end returns a short read, not an error.
    #[tokio::test]
    async fn test_passthrough_read_spanning_eof_is_clamped() {
        use fuse3::raw::Filesystem;

        const FILE_SIZE: usize = 100_000;
        const OFFSET: u64 = 90_000;

        let temp = TempDir::new().unwrap();
        let (fs, inode, mut metrics_rx) = passthrough_fixture(&temp, FILE_SIZE);

        let reply = fs
            .read(test_request(), inode, 0, OFFSET, 65536)
            .await
            .unwrap();

        let expected = FILE_SIZE as u64 - OFFSET;
        assert_eq!(reply.data.len() as u64, expected);

        let (returned, materialised, _) = drain_one_read(&mut metrics_rx);
        assert_eq!(returned, expected);
        assert_eq!(materialised, returned);
    }

    /// Reading at or past EOF returns empty rather than erroring.
    #[tokio::test]
    async fn test_passthrough_read_at_and_past_eof_returns_empty() {
        use fuse3::raw::Filesystem;

        const FILE_SIZE: usize = 4096;

        let temp = TempDir::new().unwrap();
        let (fs, inode, mut metrics_rx) = passthrough_fixture(&temp, FILE_SIZE);

        for offset in [FILE_SIZE as u64, FILE_SIZE as u64 * 4] {
            let reply = fs
                .read(test_request(), inode, 0, offset, 4096)
                .await
                .unwrap();
            assert!(reply.data.is_empty(), "offset {offset} is at or past EOF");

            let (returned, materialised, _) = drain_one_read(&mut metrics_rx);
            assert_eq!(returned, 0);
            assert_eq!(materialised, 0, "an empty reply must allocate nothing");
        }
    }

    // ========================================================================
    // Patched region passthrough gate tests (Issue #51)
    // ========================================================================

    /// Test that lookup returns ENOENT for DDS files in patched regions.
    ///
    /// When a patch owns a region (per GeoIndex), FUSE should never generate
    /// DDS textures for that region — it should return ENOENT for missing files.
    #[tokio::test]
    /// Test that DDS from a patch source is served via passthrough in patched regions.
    ///
    /// When a patch has the DDS file on disk, `resolve_lazy_geo()` serves it directly
    /// (passthrough). DDS generation is never needed.
    async fn test_lookup_patched_region_serves_patch_dds_via_passthrough() {
        use fuse3::raw::Filesystem;

        use crate::geo_index::{DsfRegion, GeoIndex, PatchCoverage};

        let temp = TempDir::new().unwrap();

        // Create a patch with DSF in region (33, -119) and a real DDS file
        let patches_dir = temp.path().join("patches");
        let patch_dir = patches_dir.join("LIPX_Mesh");
        let nav_dir = patch_dir.join("Earth nav data/+30-120");
        std::fs::create_dir_all(&nav_dir).unwrap();
        std::fs::write(nav_dir.join("+33-119.dsf"), b"fake dsf").unwrap();
        let patch_textures = patch_dir.join("textures");
        std::fs::create_dir_all(&patch_textures).unwrap();

        // Place a DDS file in the patch using GO2 convention at zoom 18.
        // Patches commonly use GO218 (Google GO2 at ZL18) naming.
        use crate::coord::to_tile_coords;
        let tc = to_tile_coords(33.5, -118.5, 18).unwrap();
        let dds_name = format!("{}_{}_GO218.dds", tc.row, tc.col);
        std::fs::write(patch_textures.join(&dds_name), b"fake dds data").unwrap();

        // Create a package so we have a textures/ directory
        let na_pkg = create_test_package(&temp, "na");

        let index = OrthoUnionIndexBuilder::new()
            .with_patches_dir(&patches_dir)
            .add_package(na_pkg)
            .build()
            .unwrap();

        // Build GeoIndex with PatchCoverage for region (33, -119)
        let geo_index = Arc::new(GeoIndex::new());
        geo_index.populate(vec![(
            DsfRegion::new(33, -119),
            PatchCoverage {
                patch_name: "LIPX_Mesh".to_string(),
            },
        )]);

        let client = create_test_client();
        let fs = Fuse3OrthoUnionFS::new(index, client, 1024).with_geo_index(Arc::clone(&geo_index));

        // Get inode for "textures" directory
        let textures_inode = fs
            .inode_manager
            .get_or_create_inode(std::path::Path::new("textures"));

        let req = fuse3::raw::Request {
            unique: 1,
            uid: 1000,
            gid: 1000,
            pid: 1000,
        };

        // Verify this filename maps to the patched region
        use crate::prefetch::tile_based::DsfTileCoord;
        let dsf = DsfTileCoord::from_scenery_filename(&dds_name).unwrap();
        assert_eq!(dsf.lat, 33, "DDS filename should map to lat 33");
        assert_eq!(dsf.lon, -119, "DDS filename should map to lon -119");

        // Lookup should succeed — the patch has this DDS on disk, served via passthrough
        let result = fs
            .lookup(req, textures_inode, std::ffi::OsStr::new(&dds_name))
            .await;

        assert!(
            result.is_ok(),
            "Lookup for DDS from patch should succeed via passthrough, got: {:?}",
            result
        );
    }

    /// Test that package terrain files in patched regions are served as fallback.
    ///
    /// When a patch owns a region but doesn't include a specific file (e.g., _sea.ter),
    /// FUSE should fall through to the package source instead of returning ENOENT.
    /// This handles cross-region DSF references where a non-patched DSF references
    /// terrain in a patched region.
    #[tokio::test]
    async fn test_lookup_patched_region_falls_through_to_package_for_terrain() {
        use fuse3::raw::Filesystem;

        use crate::geo_index::{DsfRegion, GeoIndex, PatchCoverage};

        let temp = TempDir::new().unwrap();

        // Create a patch owning region (33, -119)
        let patches_dir = temp.path().join("patches");
        let patch_dir = patches_dir.join("LIPX_Mesh");
        let nav_dir = patch_dir.join("Earth nav data/+30-120");
        std::fs::create_dir_all(&nav_dir).unwrap();
        std::fs::write(nav_dir.join("+33-119.dsf"), b"fake dsf").unwrap();
        std::fs::create_dir_all(patch_dir.join("terrain")).unwrap();

        // Create a package with a _sea.ter file whose chunk coords fall in region (33, -119).
        // This simulates X-Plane's cross-region DSF references where a non-patched DSF
        // references sea terrain in a patched region.
        let na_pkg = create_test_package(&temp, "na");

        // Place a terrain file in the package's terrain dir with coords in the patched region
        use crate::coord::to_tile_coords;
        let tc = to_tile_coords(33.5, -118.5, 16).unwrap();
        let sea_ter_name = format!("{}_{}_BI16_sea.ter", tc.row, tc.col);
        let pkg_terrain_dir = temp.path().join("na_ortho/terrain");
        std::fs::write(pkg_terrain_dir.join(&sea_ter_name), b"sea terrain data").unwrap();

        // Verify this filename maps to the patched region
        use crate::prefetch::tile_based::DsfTileCoord;
        let dsf = DsfTileCoord::from_scenery_filename(&sea_ter_name).unwrap();
        assert_eq!(dsf.lat, 33, "sea.ter filename should map to lat 33");
        assert_eq!(dsf.lon, -119, "sea.ter filename should map to lon -119");

        let index = OrthoUnionIndexBuilder::new()
            .with_patches_dir(&patches_dir)
            .add_package(na_pkg)
            .build()
            .unwrap();

        // Build GeoIndex with PatchCoverage for region (33, -119)
        let geo_index = Arc::new(GeoIndex::new());
        geo_index.populate(vec![(
            DsfRegion::new(33, -119),
            PatchCoverage {
                patch_name: "LIPX_Mesh".to_string(),
            },
        )]);

        let client = create_test_client();
        let fs = Fuse3OrthoUnionFS::new(index, client, 1024).with_geo_index(Arc::clone(&geo_index));

        let terrain_inode = fs
            .inode_manager
            .get_or_create_inode(std::path::Path::new("terrain"));

        let req = fuse3::raw::Request {
            unique: 1,
            uid: 1000,
            gid: 1000,
            pid: 1000,
        };

        // Lookup should succeed: the file exists in the package, and even though
        // the region is patched, FUSE should fall through to the package source
        // when the patch doesn't have the file.
        let result = fs
            .lookup(req, terrain_inode, std::ffi::OsStr::new(&sea_ter_name))
            .await;

        assert!(
            result.is_ok(),
            "Lookup for package terrain in patched region should fall through, got: {:?}",
            result
        );
    }

    /// Test that DDS generation is allowed in patched regions when the patch doesn't
    /// provide the DDS file.
    ///
    /// This handles boundary cases: a non-patched DSF references package terrain in a
    /// patched region. That terrain's `.ter` references a DDS (e.g., `BI16.dds`) that
    /// doesn't exist on disk. Since the patch uses different filenames (e.g., `GO218.dds`),
    /// the patch can't provide this DDS. XEL should generate it rather than returning ENOENT.
    #[tokio::test]
    async fn test_lookup_patched_region_allows_dds_generation_for_package_textures() {
        use fuse3::raw::Filesystem;

        use crate::geo_index::{DsfRegion, GeoIndex, PatchCoverage};

        let temp = TempDir::new().unwrap();

        // Create a patch owning region (33, -119)
        let patches_dir = temp.path().join("patches");
        let patch_dir = patches_dir.join("LIPX_Mesh");
        let nav_dir = patch_dir.join("Earth nav data/+30-120");
        std::fs::create_dir_all(&nav_dir).unwrap();
        std::fs::write(nav_dir.join("+33-119.dsf"), b"fake dsf").unwrap();
        std::fs::create_dir_all(patch_dir.join("terrain")).unwrap();

        // Create a package so we have a textures/ directory
        let na_pkg = create_test_package(&temp, "na");

        let index = OrthoUnionIndexBuilder::new()
            .with_patches_dir(&patches_dir)
            .add_package(na_pkg)
            .build()
            .unwrap();

        // Build GeoIndex with PatchCoverage for region (33, -119)
        let geo_index = Arc::new(GeoIndex::new());
        geo_index.populate(vec![(
            DsfRegion::new(33, -119),
            PatchCoverage {
                patch_name: "LIPX_Mesh".to_string(),
            },
        )]);

        let client = create_test_client();
        let fs = Fuse3OrthoUnionFS::new(index, client, 1024).with_geo_index(Arc::clone(&geo_index));

        // Get inode for "textures" directory
        let textures_inode = fs
            .inode_manager
            .get_or_create_inode(std::path::Path::new("textures"));

        let req = fuse3::raw::Request {
            unique: 1,
            uid: 1000,
            gid: 1000,
            pid: 1000,
        };

        // DDS filename in the patched region, using package naming convention (BI16).
        // The patch uses GO218 convention, so it doesn't have this DDS.
        // XEL should allow generation rather than blocking with ENOENT.
        use crate::coord::to_tile_coords;
        let tc = to_tile_coords(33.5, -118.5, 16).unwrap();
        let dds_name = format!("{}_{}_BI16.dds", tc.row, tc.col);

        // Verify this filename maps to the patched region
        use crate::prefetch::tile_based::DsfTileCoord;
        let dsf = DsfTileCoord::from_scenery_filename(&dds_name).unwrap();
        assert_eq!(dsf.lat, 33, "DDS filename should map to lat 33");
        assert_eq!(dsf.lon, -119, "DDS filename should map to lon -119");

        // Lookup should SUCCEED — the DDS gets a virtual inode for generation,
        // even though the region is patched. The patch doesn't provide this DDS.
        let result = fs
            .lookup(req, textures_inode, std::ffi::OsStr::new(&dds_name))
            .await;

        assert!(
            result.is_ok(),
            "DDS in patched region should allow generation when patch doesn't provide it, got: {:?}",
            result
        );
    }

    /// Test DDS generation in patched regions at a different zoom level (ZL14).
    ///
    /// Patches can be at any zoom level. This verifies the DDS gate removal works
    /// correctly for higher zoom levels where coordinate scale differs.
    #[tokio::test]
    async fn test_lookup_patched_region_allows_dds_generation_at_different_zoom() {
        use fuse3::raw::Filesystem;

        use crate::geo_index::{DsfRegion, GeoIndex, PatchCoverage};

        let temp = TempDir::new().unwrap();

        let patches_dir = temp.path().join("patches");
        let patch_dir = patches_dir.join("LIPX_Mesh");
        let nav_dir = patch_dir.join("Earth nav data/+30-120");
        std::fs::create_dir_all(&nav_dir).unwrap();
        std::fs::write(nav_dir.join("+33-119.dsf"), b"fake dsf").unwrap();
        std::fs::create_dir_all(patch_dir.join("terrain")).unwrap();

        let na_pkg = create_test_package(&temp, "na");

        let index = OrthoUnionIndexBuilder::new()
            .with_patches_dir(&patches_dir)
            .add_package(na_pkg)
            .build()
            .unwrap();

        let geo_index = Arc::new(GeoIndex::new());
        geo_index.populate(vec![(
            DsfRegion::new(33, -119),
            PatchCoverage {
                patch_name: "LIPX_Mesh".to_string(),
            },
        )]);

        let client = create_test_client();
        let fs = Fuse3OrthoUnionFS::new(index, client, 1024).with_geo_index(Arc::clone(&geo_index));

        let textures_inode = fs
            .inode_manager
            .get_or_create_inode(std::path::Path::new("textures"));

        let req = fuse3::raw::Request {
            unique: 1,
            uid: 1000,
            gid: 1000,
            pid: 1000,
        };

        // DDS at zoom 14 (different from the standard ZL16 package convention)
        use crate::coord::to_tile_coords;
        let tc = to_tile_coords(33.5, -118.5, 14).unwrap();
        let dds_name = format!("{}_{}_BI14.dds", tc.row, tc.col);

        use crate::prefetch::tile_based::DsfTileCoord;
        let dsf = DsfTileCoord::from_scenery_filename(&dds_name).unwrap();
        assert_eq!(dsf.lat, 33, "DDS filename should map to lat 33");
        assert_eq!(dsf.lon, -119, "DDS filename should map to lon -119");

        let result = fs
            .lookup(req, textures_inode, std::ffi::OsStr::new(&dds_name))
            .await;

        assert!(
            result.is_ok(),
            "DDS at ZL14 in patched region should allow generation, got: {:?}",
            result
        );
    }

    // ========================================================================
    // Water mask guard tests (Issue #68)
    // ========================================================================

    /// Test that lookup returns ENOENT for DDS when a PNG water mask exists.
    ///
    /// X-Plane requests BORDER_TEX water masks as `.dds` first. If a `.png`
    /// with the same stem exists on disk, we must return ENOENT so X-Plane
    /// falls back to the real PNG water mask instead of getting satellite imagery.
    #[tokio::test]
    async fn test_lookup_returns_enoent_for_dds_when_png_water_mask_exists() {
        use fuse3::raw::Filesystem;

        let temp = TempDir::new().unwrap();

        // Create a package with a PNG water mask in textures/
        let pkg_dir = temp.path().join("eu_ortho");
        std::fs::create_dir_all(pkg_dir.join("Earth nav data/+40-080")).unwrap();
        std::fs::write(
            pkg_dir.join("Earth nav data/+40-080/+40-074.dsf"),
            b"pkg dsf",
        )
        .unwrap();
        std::fs::create_dir_all(pkg_dir.join("textures")).unwrap();
        std::fs::create_dir_all(pkg_dir.join("terrain")).unwrap();

        // Place a PNG water mask (the kind referenced by _sea_overlay.ter BORDER_TEX)
        let png_name = "24496_33152_ZL16.png";
        std::fs::write(
            pkg_dir.join("textures").join(png_name),
            b"fake png water mask",
        )
        .unwrap();

        let pkg = InstalledPackage::new(
            Package::new("eu", PackageType::Ortho, Version::new(1, 0, 0)),
            &pkg_dir,
        );

        let index = OrthoUnionIndexBuilder::new()
            .add_package(pkg)
            .build()
            .unwrap();

        let client = create_test_client();
        let fs = Fuse3OrthoUnionFS::new(index, client, 1024);

        let textures_inode = fs
            .inode_manager
            .get_or_create_inode(std::path::Path::new("textures"));

        let req = fuse3::raw::Request {
            unique: 1,
            uid: 1000,
            gid: 1000,
            pid: 1000,
        };

        // Request the DDS version — should fail because the PNG water mask exists
        let dds_name = "24496_33152_ZL16.dds";
        let result = fs
            .lookup(req, textures_inode, std::ffi::OsStr::new(dds_name))
            .await;

        assert!(
            result.is_err(),
            "Lookup for DDS should return ENOENT when PNG water mask exists, got: {:?}",
            result
        );
    }

    /// Test that DDS generation still works when no PNG water mask exists.
    ///
    /// This verifies the water mask guard doesn't block normal DDS generation
    /// for ortho tile textures (which have no corresponding PNG on disk).
    #[tokio::test]
    async fn test_lookup_allows_dds_generation_when_no_png_exists() {
        use fuse3::raw::Filesystem;

        let temp = TempDir::new().unwrap();

        // Create a package with textures/ but no PNG for this tile
        let pkg_dir = temp.path().join("eu_ortho");
        std::fs::create_dir_all(pkg_dir.join("Earth nav data/+40-080")).unwrap();
        std::fs::write(
            pkg_dir.join("Earth nav data/+40-080/+40-074.dsf"),
            b"pkg dsf",
        )
        .unwrap();
        std::fs::create_dir_all(pkg_dir.join("textures")).unwrap();
        std::fs::create_dir_all(pkg_dir.join("terrain")).unwrap();

        let pkg = InstalledPackage::new(
            Package::new("eu", PackageType::Ortho, Version::new(1, 0, 0)),
            &pkg_dir,
        );

        let index = OrthoUnionIndexBuilder::new()
            .add_package(pkg)
            .build()
            .unwrap();

        let client = create_test_client();
        let fs = Fuse3OrthoUnionFS::new(index, client, 1024);

        let textures_inode = fs
            .inode_manager
            .get_or_create_inode(std::path::Path::new("textures"));

        let req = fuse3::raw::Request {
            unique: 1,
            uid: 1000,
            gid: 1000,
            pid: 1000,
        };

        // Request a DDS with no corresponding PNG — should succeed (virtual inode)
        let result = fs
            .lookup(
                req,
                textures_inode,
                std::ffi::OsStr::new("10000_5000_BI16.dds"),
            )
            .await;

        assert!(
            result.is_ok(),
            "Lookup for DDS without PNG water mask should succeed for generation, got: {:?}",
            result
        );
    }

    /// Test that lookup in a non-patched region still creates a virtual inode for DDS generation.
    #[tokio::test]
    async fn test_lookup_non_patched_region_creates_virtual_inode() {
        use fuse3::raw::Filesystem;

        let temp = TempDir::new().unwrap();

        // Create a package (no patches = no patched regions)
        let na_pkg = create_test_package(&temp, "na");

        let index = OrthoUnionIndexBuilder::new()
            .add_package(na_pkg)
            .build()
            .unwrap();

        let client = create_test_client();
        let fs = Fuse3OrthoUnionFS::new(index, client, 1024);

        let textures_inode = fs
            .inode_manager
            .get_or_create_inode(std::path::Path::new("textures"));

        let req = fuse3::raw::Request {
            unique: 1,
            uid: 1000,
            gid: 1000,
            pid: 1000,
        };

        // Any DDS filename should succeed (virtual inode for generation)
        let result = fs
            .lookup(
                req,
                textures_inode,
                std::ffi::OsStr::new("10000_5000_BI16.dds"),
            )
            .await;

        assert!(
            result.is_ok(),
            "Lookup for DDS in non-patched region should succeed for generation"
        );
    }

    // ========================================================================
    // FOPEN_DIRECT_IO tests (Issue #65)
    // ========================================================================

    #[tokio::test]
    async fn test_open_virtual_dds_returns_direct_io() {
        use super::VIRTUAL_DDS_OPEN_FLAGS;
        use fuse3::raw::Filesystem;

        let temp = TempDir::new().unwrap();
        let na_pkg = create_test_package(&temp, "na");

        let index = OrthoUnionIndexBuilder::new()
            .add_package(na_pkg)
            .build()
            .unwrap();

        let client = create_test_client();
        let fs = Fuse3OrthoUnionFS::new(index, client, 1024);

        let req = fuse3::raw::Request {
            unique: 1,
            uid: 1000,
            gid: 1000,
            pid: 1000,
        };

        // Virtual DDS inode (above VIRTUAL_INODE_BASE)
        use crate::fuse::support::inode::VIRTUAL_INODE_BASE;
        let virtual_inode = VIRTUAL_INODE_BASE + 42;
        let result: Fuse3InternalResult<ReplyOpen> =
            fs.open(req, virtual_inode, libc::O_RDONLY as u32).await;

        let reply = result.expect("open on virtual DDS inode should succeed");
        assert_eq!(
            reply.flags, VIRTUAL_DDS_OPEN_FLAGS,
            "virtual DDS files should use the platform's virtual-DDS open flags \
             (direct I/O on Linux, page cache on macOS for mmap compatibility)"
        );
        // This inode was synthesised, not registered by lookup, so there is no
        // tile to memoise and no handle to hand out. Reads still work (#234).
        assert_eq!(
            reply.fh, 0,
            "an unregistered virtual inode gets no memoising handle"
        );
    }

    #[tokio::test]
    async fn test_open_real_inode_returns_default_flags() {
        use fuse3::raw::Filesystem;

        let temp = TempDir::new().unwrap();
        let na_pkg = create_test_package(&temp, "na");

        let index = OrthoUnionIndexBuilder::new()
            .add_package(na_pkg)
            .build()
            .unwrap();

        let client = create_test_client();
        let fs = Fuse3OrthoUnionFS::new(index, client, 1024);

        let req = fuse3::raw::Request {
            unique: 1,
            uid: 1000,
            gid: 1000,
            pid: 1000,
        };

        // Real inode (below VIRTUAL_INODE_BASE)
        let real_inode = 42u64;
        let result: Fuse3InternalResult<ReplyOpen> =
            fs.open(req, real_inode, libc::O_RDONLY as u32).await;

        let reply = result.expect("open on real inode should succeed");
        assert_eq!(reply.fh, 0, "file handle should be stateless");
        assert_eq!(
            reply.flags, 0,
            "real passthrough files should have default flags (no DIRECT_IO)"
        );
    }

    // ─────────────────────────────────────────────────────────────────────────
    // PrefetchStateObserver wiring (#176)
    //
    // The observer's own policy is unit-tested in `prefetch::state_observer`.
    // What those tests cannot see is whether FUSE ever *calls* it: every one
    // of them invokes `observe()` directly. These two exercise the real
    // chain — `DdsRequestor::request_dds_impl` → `do_request` →
    // `on_dds_response` → `observer.observe` → `GeoIndex` — over a real
    // `Fuse3OrthoUnionFS` and a real `GeoIndex`, so cutting any link in it
    // fails a test instead of silently disabling the feature.
    //
    // The final link (`manager::mounts` attaching the observer at mount
    // construction) is not covered: reaching it needs a whole
    // `XEarthLayerService` and a live FUSE mount.
    // ─────────────────────────────────────────────────────────────────────────

    /// Drive one full FUSE DDS request against a `Prefetched` region and
    /// return whether that region still carries prefetch state afterwards.
    ///
    /// `cache_hit` is what the stand-in executor reports back, so the caller
    /// controls the single input the observer's policy turns on.
    async fn run_dds_request_over_prefetched_region(cache_hit: bool) -> bool {
        use crate::geo_index::{DsfRegion, PrefetchedRegion};

        let temp = TempDir::new().unwrap();
        let index = OrthoUnionIndexBuilder::new()
            .add_package(create_test_package(&temp, "na"))
            .build()
            .unwrap();

        // A region prefetch claims is fully cached.
        let tile = crate::coord::to_tile_coords(33.5, -118.5, 12).unwrap();
        let (lat, lon) = tile.to_lat_lon();
        let region = DsfRegion::from_lat_lon(lat, lon);
        let geo_index = Arc::new(GeoIndex::new());
        geo_index.insert::<PrefetchedRegion>(region, PrefetchedRegion::prefetched());

        let (client, mut rx) = MockDdsClient::new();
        let fs = Fuse3OrthoUnionFS::new(index, client as Arc<dyn DdsClient>, 1024)
            .with_state_observer(Arc::new(PrefetchStateObserver::new(Arc::clone(&geo_index))));

        // Stand in for the executor daemon: answer the one request FUSE makes.
        let responder = tokio::spawn(async move {
            let request = rx.recv().await.expect("FUSE must submit a DDS request");
            request
                .response_tx
                .expect("FUSE requests carry a response channel")
                .send(DdsResponse::new(
                    vec![0u8; 16].into(),
                    cache_hit,
                    Duration::from_millis(1),
                    true,
                ))
                .ok();
        });

        let coords = crate::fuse::DdsFilename {
            row: tile.row * 16,
            col: tile.col * 16,
            zoom: tile.zoom + 4,
            map_type: "BI".to_string(),
            kind: crate::fuse::TextureKind::Ortho,
        };
        let _ = fs.core.request_dds_impl(&coords).await;
        responder.await.unwrap();

        geo_index.get::<PrefetchedRegion>(&region).is_some()
    }

    #[tokio::test]
    async fn test_fuse_on_demand_generation_demotes_prefetched_region() {
        assert!(
            !run_dds_request_over_prefetched_region(false).await,
            "a FUSE on-demand generation inside a Prefetched region must reach \
             the observer and demote it — if this passes only because the \
             observer was never called, the whole feature is inert"
        );
    }

    #[tokio::test]
    async fn test_fuse_cache_hit_leaves_prefetched_region_alone() {
        // Guards the `cache_hit` argument specifically: a hook that called
        // the observer but hardcoded `false` would pass the test above.
        assert!(
            run_dds_request_over_prefetched_region(true).await,
            "serving a prefetched tile from cache is the system working — \
             the region must keep its state"
        );
    }
}
