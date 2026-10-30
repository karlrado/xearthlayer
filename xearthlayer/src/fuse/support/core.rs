//! The backend-neutral core of the ortho union filesystem.
//!
//! [`OrthoUnionCore`] holds the state and logic that are independent of the
//! kernel it is mounted through: the union index, the DDS generation
//! pipeline, the lit texture source, per-open handle memoisation, the
//! pinned-tile budget, and the gauges around them. `fuse3::Fuse3OrthoUnionFS`
//! is a thin adapter over this core: it owns the kernel identity (inodes)
//! and the FUSE init handshake parameters and delegates everything else.
//! The WinFSP backend (Phase 2) will drive the same core with file contexts
//! instead of inodes.
//!
//! See `docs/dev/windows-backend-split.md` §3.1–§3.3 for the split tables
//! and §4.4 for the seam API.

use bytes::Bytes;
use dashmap::DashMap;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::OnceCell;
use tracing::{debug, warn, Instrument};

use crate::coord::TileCoord;
use crate::executor::{DdsClient, StorageConcurrencyLimiter};
use crate::fuse::coalesce::RequestCoalescer;
use crate::fuse::support::attrs::{
    attrs_from_metadata, root_dir_attrs, virtual_dds_attrs, virtual_dir_attrs, DirEntry,
    FileAttributes, StatFsInfo, VirtualDdsConfig,
};
use crate::fuse::support::error::FsError;
use crate::fuse::support::node::FsNode;
use crate::fuse::support::requestor::DdsRequestor;
use crate::fuse::{get_default_placeholder, parse_dds_filename, DdsFilename, TextureKind};
use crate::geo_index::GeoIndex;
use crate::night::{BlackLitSource, LitTextureSource};
use crate::ortho_union::OrthoUnionIndex;
use crate::pinned_budget::PinnedTileBudget;
use crate::prefetch::{PrefetchStateObserver, TileRequestCallback};

/// Per-open state for one virtual DDS file.
///
/// The kernel caps each FUSE read at 1 MiB, so X-Plane reading one
/// 11.17 MB texture arrives as 12-23 ranged calls. Resolving the tile once
/// per open and slicing it turns those into a single executor round trip.
pub struct DdsHandle {
    /// Which tile this handle refers to.
    pub coords: DdsFilename,
    /// The tile, produced on first read and reused by every later read.
    /// `Bytes`: the handle shares the cache's allocation rather than owning
    /// a copy, and `Bytes::slice` serves a ranged read without allocating.
    pub tile: OnceCell<Bytes>,
}

/// The backend-neutral core of the ortho union filesystem.
///
/// This is the state shared by every backend (FUSE today, WinFSP in Phase
/// 2). It carries no kernel concept: identity is the backend's, and the
/// backend maps its kernel handles onto this core's `fh` numbering and
/// [`DdsHandle`] memoisation.
pub struct OrthoUnionCore {
    /// Union index mapping virtual paths to real file locations
    index: Arc<OrthoUnionIndex>,
    /// Geospatial reference database for region-level ownership queries.
    ///
    /// Used to determine if a scenery file falls within a patch-owned region.
    /// When set, FUSE filters lazy resolution to only serve patch sources in
    /// those regions, hiding package files that would cause X-Plane conflicts.
    geo_index: Option<Arc<GeoIndex>>,
    /// Observer for prefetch state divergence (#176).
    state_observer: Option<Arc<PrefetchStateObserver>>,
    /// Client for DDS generation requests (new daemon architecture)
    dds_client: Arc<dyn DdsClient>,
    /// Configuration for virtual DDS attributes
    virtual_dds_config: VirtualDdsConfig,
    /// Timeout for DDS generation
    generation_timeout: Duration,
    /// Limiter for concurrent disk I/O operations
    disk_io_limiter: Arc<StorageConcurrencyLimiter>,
    /// Optional callback for tile request tracking
    tile_request_callback: Option<TileRequestCallback>,
    /// Request coalescer for deduplicating concurrent requests
    request_coalescer: Arc<RequestCoalescer>,
    /// Source of lit textures (`<stem>_LIT.dds`). Lit inodes never reach the
    /// DDS client, coalescer or caches above (night lighting, R8). Defaults
    /// to [`BlackLitSource`], which serves the minimal black DDS.
    lit_source: Arc<dyn LitTextureSource>,
    /// Per-open state for virtual DDS files, keyed by file handle.
    dds_handles: DashMap<u64, Arc<DdsHandle>>,
    /// Source of file handles. Starts at 1: 0 means "no handle".
    next_fh: AtomicU64,
    /// Bytes of DDS tiles currently pinned by open handles, and the ceiling.
    ///
    /// Shared rather than private because prefetch reads the same figure as its
    /// yield signal (#246). See [`PinnedTileBudget`].
    pinned: Arc<PinnedTileBudget>,
    /// Highest concurrent handle count seen. The current count drains to near
    /// zero between bursts, so the peak is what the pinned ceiling has to be
    /// sized against.
    peak_handles_open: AtomicU64,
    /// Whether the budget-exhausted warning has been emitted this session.
    ///
    /// Latched, because the cap engages for every open in a burst rather than
    /// once: an unlatched warn would bury the log it exists to serve. The
    /// per-occurrence count goes to metrics instead (#236).
    budget_warning_logged: AtomicBool,
    /// Optional metrics client for reporting FUSE-level metrics.
    ///
    /// When set, reports coalesced requests and other FUSE-specific metrics.
    metrics_client: Option<crate::metrics::MetricsClient>,
}

impl OrthoUnionCore {
    /// Create a new consolidated ortho union core.
    ///
    /// # Arguments
    ///
    /// * `index` - Pre-built union index of all ortho sources
    /// * `dds_client` - Client for DDS generation requests (daemon architecture)
    /// * `expected_dds_size` - Expected size of generated DDS files
    pub fn new(
        index: OrthoUnionIndex,
        dds_client: Arc<dyn DdsClient>,
        expected_dds_size: usize,
    ) -> Self {
        let disk_io_limiter = Arc::new(StorageConcurrencyLimiter::with_defaults(
            "ortho_union_disk_io",
        ));
        let request_coalescer = Arc::new(RequestCoalescer::new());
        debug!(
            max_concurrent = disk_io_limiter.max_concurrent(),
            sources = index.source_count(),
            files = index.file_count(),
            "Consolidated ortho union FUSE filesystem initialized"
        );

        Self {
            index: Arc::new(index),
            geo_index: None,
            state_observer: None,
            dds_client,
            virtual_dds_config: VirtualDdsConfig::new(expected_dds_size as u64),
            generation_timeout: Duration::from_secs(30),
            disk_io_limiter,
            tile_request_callback: None,
            request_coalescer,
            lit_source: Arc::new(BlackLitSource),
            dds_handles: DashMap::new(),
            next_fh: AtomicU64::new(1),
            pinned: Arc::new(PinnedTileBudget::new()),
            peak_handles_open: AtomicU64::new(0),
            budget_warning_logged: AtomicBool::new(false),
            metrics_client: None,
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
        let request_coalescer = Arc::new(RequestCoalescer::new());

        Self {
            index: Arc::new(index),
            geo_index: None,
            state_observer: None,
            dds_client,
            virtual_dds_config: VirtualDdsConfig::new(expected_dds_size as u64),
            generation_timeout: Duration::from_secs(30),
            disk_io_limiter,
            tile_request_callback: None,
            request_coalescer,
            lit_source: Arc::new(BlackLitSource),
            dds_handles: DashMap::new(),
            next_fh: AtomicU64::new(1),
            pinned: Arc::new(PinnedTileBudget::new()),
            peak_handles_open: AtomicU64::new(0),
            budget_warning_logged: AtomicBool::new(false),
            metrics_client: None,
        }
    }

    /// Share the pinned-tile budget with the prefetch backpressure signal.
    ///
    /// Without this the filesystem accounts against a budget only it can see,
    /// which is correct for the memoisation cap but leaves prefetch blind to
    /// demand on the mount (#246). Passing the runtime's budget makes the same
    /// number answer both questions.
    pub fn with_pinned_budget(mut self, budget: Arc<PinnedTileBudget>) -> Self {
        self.pinned = budget;
        self
    }

    /// Serve lit textures from `source` instead of the black default.
    pub fn with_lit_source(mut self, source: Arc<dyn LitTextureSource>) -> Self {
        self.lit_source = source;
        self
    }

    /// Set the timeout for DDS generation.
    ///
    /// After this timeout, a placeholder texture is returned to prevent
    /// X-Plane from blocking indefinitely.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.generation_timeout = timeout;
        self
    }

    /// Set the callback for tile request tracking.
    ///
    /// This callback is invoked whenever a tile is requested, enabling
    /// the prefetch system to infer aircraft position from FUSE requests.
    pub fn with_tile_request_callback(mut self, callback: TileRequestCallback) -> Self {
        self.tile_request_callback = Some(callback);
        self
    }

    /// Set the metrics client for reporting FUSE-level metrics.
    ///
    /// When set, reports coalesced requests and other FUSE-specific metrics
    /// to the metrics system.
    pub fn with_metrics(mut self, metrics: crate::metrics::MetricsClient) -> Self {
        self.metrics_client = Some(metrics);
        self
    }

    /// Set the geospatial reference index for region-level ownership queries.
    ///
    /// When set, FUSE uses the GeoIndex to determine if a scenery file falls
    /// within a patch-owned DSF region. Files in patched regions are resolved
    /// only from patch sources, hiding package files that could conflict.
    pub fn with_geo_index(mut self, geo_index: Arc<GeoIndex>) -> Self {
        self.geo_index = Some(geo_index);
        self
    }

    /// Set the prefetch state observer.
    ///
    /// When set, an on-demand generation in a region prefetch marked
    /// `Prefetched` or `NoCoverage` demotes that region.
    pub fn with_state_observer(mut self, observer: Arc<PrefetchStateObserver>) -> Self {
        self.state_observer = Some(observer);
        self
    }

    // =========================================================================
    // Accessors
    // =========================================================================

    /// Get the ortho union index.
    pub fn index(&self) -> &OrthoUnionIndex {
        &self.index
    }

    /// Returns the disk I/O limiter for monitoring/metrics.
    pub fn disk_io_limiter(&self) -> &Arc<StorageConcurrencyLimiter> {
        &self.disk_io_limiter
    }

    /// The pinned-tile budget shared with the prefetch backpressure signal.
    pub fn pinned(&self) -> &Arc<PinnedTileBudget> {
        &self.pinned
    }

    /// Per-open virtual-DDS handle state, keyed by file handle.
    ///
    /// The core ops drive the map directly; this accessor is the white-box
    /// surface the adapter's tests use to inspect a live tile (e.g. the
    /// zero-copy read test), so it has no production caller yet.
    #[allow(dead_code)]
    pub fn dds_handles(&self) -> &DashMap<u64, Arc<DdsHandle>> {
        &self.dds_handles
    }

    /// The virtual DDS attribute configuration.
    pub fn virtual_dds_config(&self) -> &VirtualDdsConfig {
        &self.virtual_dds_config
    }

    /// Highest concurrent open handle count seen.
    pub fn peak_dds_handles(&self) -> u64 {
        self.peak_handles_open.load(Ordering::Relaxed)
    }

    /// Highest pinned tile byte total seen.
    pub fn peak_pinned_tile_bytes(&self) -> u64 {
        self.pinned.peak_bytes()
    }

    /// Number of virtual DDS files currently open.
    pub fn open_dds_handles(&self) -> usize {
        self.dds_handles.len()
    }

    /// Bytes of DDS tiles currently pinned by open handles.
    pub fn pinned_tile_bytes(&self) -> u64 {
        self.pinned.pinned_bytes()
    }

    // =========================================================================
    // Neutral operations (moved verbatim from the FUSE adapter)
    // =========================================================================

    /// Check if a scenery filename falls in a geo-filtered (patch-owned) region.
    ///
    /// Returns true when the file's DSF region has [`PatchCoverage`] in the
    /// GeoIndex, meaning only patch sources should serve files for this region.
    fn is_geo_filtered(&self, filename: &str) -> bool {
        let geo_index = match self.geo_index {
            Some(ref gi) => gi,
            None => return false,
        };

        use crate::geo_index::{DsfRegion, PatchCoverage};
        use crate::prefetch::tile_based::DsfTileCoord;

        DsfTileCoord::from_scenery_filename(filename)
            .map(|dsf| {
                let region = DsfRegion::new(dsf.lat, dsf.lon);
                geo_index.contains::<PatchCoverage>(&region)
            })
            .unwrap_or(false)
    }

    /// Resolve a lazy path with geospatial awareness.
    ///
    /// If the file is in a patch-owned region (per GeoIndex), patch sources are
    /// searched first. If the file isn't found in patches, all sources are tried
    /// as fallback — this handles cross-region DSF references where a non-patched
    /// DSF references terrain (e.g., `_sea.ter`) in a patched region.
    ///
    /// DDS generation blocking is handled separately by the passthrough gate in
    /// `lookup()`, not here.
    ///
    /// This is the composition point: GeoIndex (geography) + OrthoUnionIndex (files).
    pub fn resolve_lazy_geo(&self, virtual_path: &Path, filename: &str) -> Option<PathBuf> {
        if self.is_geo_filtered(filename) {
            // Patched region: patches first, packages fill gaps
            self.index
                .resolve_lazy_filtered(virtual_path, |s| s.is_patch())
                .or_else(|| self.index.resolve_lazy(virtual_path))
        } else {
            // Normal: all sources
            self.index.resolve_lazy(virtual_path)
        }
    }

    /// Produce a whole DDS tile.
    ///
    /// Called once per open when the handle memoises, once per read when it
    /// does not.
    pub async fn resolve_tile(&self, coords: &DdsFilename) -> Bytes {
        // request_dds parses a filename; Display carries the correct zoom.
        self.request_dds(&format!("{}.dds", coords))
            .await
            .unwrap_or_else(get_default_placeholder)
    }

    /// Request DDS generation by filename string.
    ///
    /// Wrapper around the trait method that parses the filename first.
    async fn request_dds(&self, name_str: &str) -> Option<Bytes> {
        let coords = parse_dds_filename(name_str).ok()?;
        Some(self.request_dds_impl(&coords).await)
    }

    /// Neutral attributes for a virtual DDS, by kind.
    ///
    /// Ortho textures report the configured expected size; lit textures
    /// report the lit source's size, which is what the source will serve
    /// (night lighting R4/R7). The backend maps the result onto its kernel
    /// attribute type (FUSE adds the inode number and the macOS-only fields).
    pub async fn virtual_attrs_for(&self, coords: &DdsFilename) -> FileAttributes {
        let mut attrs = virtual_dds_attrs(&self.virtual_dds_config);
        if coords.kind == TextureKind::Lit {
            let size = self.lit_source.size(coords).await;
            attrs.size = size;
            attrs.blocks = size.div_ceil(u64::from(attrs.blksize));
        }
        attrs
    }

    /// Allocate the next file handle and register a fresh per-open DDS
    /// handle under it.
    ///
    /// Handle numbering is backend-agnostic: just a `u64` key into
    /// [`Self::dds_handles`] starting at 1, so `0` remains "no handle".
    pub fn new_dds_handle(&self, coords: DdsFilename) -> u64 {
        let fh = self.next_fh.fetch_add(1, Ordering::Relaxed);
        self.dds_handles.insert(
            fh,
            Arc::new(DdsHandle {
                coords,
                tile: OnceCell::new(),
            }),
        );
        fh
    }

    /// Publish the handle gauge. Called on open and release -- two events per
    /// texture, against the 12-23 reads it takes to serve one.
    pub fn report_handle_gauge(&self) {
        let open = self.dds_handles.len() as u64;
        let pinned = self.pinned.pinned_bytes();
        self.peak_handles_open.fetch_max(open, Ordering::Relaxed);

        if let Some(metrics) = &self.metrics_client {
            metrics.fuse_handles(
                open,
                pinned,
                self.peak_handles_open.load(Ordering::Relaxed),
                self.pinned.peak_bytes(),
            );
        }
    }

    /// Claims the one-shot budget warning: true on the first call of the
    /// session, false ever after.
    ///
    /// The cap engages for every open in a burst, not once, so the warning is
    /// latched and the per-occurrence count goes to metrics instead (#236).
    pub fn claim_budget_warning(&self) -> bool {
        !self.budget_warning_logged.swap(true, Ordering::Relaxed)
    }

    /// Record one FUSE `read()` call for the amplification metric.
    ///
    /// `returned` is what this call hands back to the kernel; `materialised`
    /// is what the handler had to obtain to produce it. The kernel caps each read
    /// at 1 MiB, so a handler that builds the whole object per call inflates
    /// the second against the first. See #233 and #234.
    pub fn record_read(&self, returned: u64, materialised: u64, virtual_dds: bool) {
        if let Some(metrics) = &self.metrics_client {
            metrics.fuse_read(returned, materialised, virtual_dds);
        }
    }

    // =========================================================================
    // Neutral ops: every backend (FUSE today, WinFSP in Phase 2) calls
    // exactly these. See windows-backend-split.md §3.4 and §4.4.
    // =========================================================================

    /// Resolve `name` inside the directory `parent` of the union view.
    ///
    /// Resolution order, exactly as the FUSE `lookup` op has always done it:
    ///
    /// 1. a real file in the union index (a metadata failure falls through,
    ///    it is not an error);
    /// 2. a virtual directory of the union;
    /// 3. lazy geospatial resolution — in patch-owned regions only patch
    ///    sources are searched, so package files that reference DDS we will
    ///    not generate stay invisible (#68 lineage);
    /// 4. a texture we can generate. The water-mask guard (#68) runs here:
    ///    when the real file is a `.png` on disk, the `.dds` name is
    ///    reported missing so X-Plane falls back to the PNG via lazy
    ///    resolution instead of getting satellite imagery.
    ///
    /// `name` is the kernel's lossy-UTF-8 view of the entry name; joining
    /// and parsing it here is the same lossy path the adapter used to take.
    pub async fn lookup(
        &self,
        parent: &FsNode,
        name: &str,
    ) -> Result<(FsNode, FileAttributes), FsError> {
        let parent_path = match parent {
            FsNode::Directory(p) => p.clone(),
            _ => return Err(FsError::NotADirectory),
        };
        let child_path = parent_path.join(name);

        // 1. Real file from a source.
        if let Some(source) = self.index.resolve(&child_path) {
            if let Ok(metadata) = fs::metadata(&source.real_path).await {
                return Ok((FsNode::RealFile(child_path), attrs_from_metadata(&metadata)));
            }
        }

        // 2. Virtual directory in the union.
        if self.index.is_directory(&child_path) {
            return Ok((FsNode::Directory(child_path), virtual_dir_attrs()));
        }

        // 3. Lazy resolution for terrain/textures directories. These
        //    directories are not fully scanned at startup for performance,
        //    so we resolve files on-demand by checking the real filesystem.
        if let Some(real_path) = self.resolve_lazy_geo(&child_path, name) {
            if let Ok(metadata) = fs::metadata(&real_path).await {
                return Ok((FsNode::RealFile(child_path), attrs_from_metadata(&metadata)));
            }
        }

        // 4. A DDS file we can generate.
        //
        // In patched regions, patch DDS files are served via
        // resolve_lazy_geo() above (passthrough from the patch source). If we
        // reach here, no source has this DDS on disk — generate it. This
        // handles boundary cases where package `.ter` files reference DDS
        // textures the patch doesn't provide (different naming convention).
        if name.ends_with(".dds") {
            // Water mask guard (#68): X-Plane requests BORDER_TEX as .dds
            // first, but the actual water mask is a .png on disk. If we
            // generate a virtual DDS, X-Plane gets satellite imagery instead
            // of the alpha mask. Report missing so X-Plane falls back to the
            // .png via lazy resolution.
            let png_name = name.replace(".dds", ".png");
            let png_path = parent_path.join(&png_name);
            if self.resolve_lazy_geo(&png_path, &png_name).is_some() {
                debug!(file = %name, "Water mask PNG exists, skipping DDS generation");
                return Err(FsError::NotFound);
            }

            if let Ok(coords) = parse_dds_filename(name) {
                let attrs = self.virtual_attrs_for(&coords).await;
                return Ok((FsNode::VirtualDds(coords), attrs));
            }
        }

        Err(FsError::NotFound)
    }

    /// The attributes of `node` in the union view.
    ///
    /// Path nodes (either variant) resolve exactly as the FUSE `getattr` op
    /// has always done: root, then virtual directory, then real file via the
    /// index, then lazy geospatial resolution. A path that used to resolve
    /// but whose source has disappeared is reported missing (ENOENT), which
    /// is what the FUSE backend has always reported.
    pub async fn attrs(&self, node: &FsNode) -> Result<FileAttributes, FsError> {
        match node {
            FsNode::VirtualDds(coords) => Ok(self.virtual_attrs_for(coords).await),
            FsNode::Directory(p) | FsNode::RealFile(p) => self.path_attrs(p).await,
        }
    }

    /// Attributes for a path node of the union view (see [`Self::attrs`]).
    async fn path_attrs(&self, path: &Path) -> Result<FileAttributes, FsError> {
        if path.as_os_str().is_empty() {
            return Ok(root_dir_attrs());
        }

        if self.index.is_directory(path) {
            return Ok(virtual_dir_attrs());
        }

        let filename = path.file_name().and_then(|f| f.to_str()).unwrap_or("");
        let real_path = self
            .index
            .resolve(path)
            .map(|s| s.real_path.clone())
            .or_else(|| self.resolve_lazy_geo(path, filename))
            .ok_or(FsError::NotFound)?;

        // A source that vanished between lookup and getattr: missing, not an
        // I/O failure -- the kernel treats both as "nothing here" for a
        // read-only view, and ENOENT is what the FUSE op has always sent.
        let metadata = fs::metadata(&real_path)
            .await
            .map_err(|_| FsError::NotFound)?;

        Ok(attrs_from_metadata(&metadata))
    }

    /// Read `size` bytes of `node` starting at `offset`, serving whatever the
    /// source has (a short read at the end, empty past the end).
    ///
    /// Virtual DDS: lit textures come from the lit source alone (no
    /// executor, no memoising handle, no DDS read metrics); ortho textures
    /// resolve the tile once per open and slice it, so the kernel's 12-23
    /// ranged reads of one texture cost a single executor round trip. Real
    /// files read only the requested window under the disk I/O limiter --
    /// reading the whole file here would move it once per call (#233).
    pub async fn read(
        &self,
        node: &FsNode,
        fh: u64,
        offset: u64,
        size: u32,
    ) -> Result<Bytes, FsError> {
        match node {
            FsNode::VirtualDds(coords) => self.read_virtual_dds(coords, fh, offset, size).await,
            FsNode::RealFile(p) | FsNode::Directory(p) => {
                self.read_real_file(p, offset, size).await
            }
        }
    }

    /// Serve one ranged read of a virtual DDS (see [`Self::read`]).
    async fn read_virtual_dds(
        &self,
        coords: &DdsFilename,
        fh: u64,
        offset: u64,
        size: u32,
    ) -> Result<Bytes, FsError> {
        // Lit textures come from the lit source alone: no executor, no
        // memoising handle, no DDS read metrics.
        if coords.kind == TextureKind::Lit {
            return Ok(self.lit_source.read(coords, offset, size).await);
        }

        // The span is backend-neutral now: the inode number was a FUSE
        // concept and is no longer in scope; offset/size identify the call.
        let fuse_read_span =
            tracing::debug_span!(target: "profiling", "fuse_read", offset = offset, size = size);

        // Resolve the tile once per open. The kernel splits one X-Plane
        // texture read into 12-23 ranged calls (1 MiB ceiling), and this
        // branch used to re-enter the executor on every one of them.
        let handle = self.dds_handles.get(&fh).map(|h| Arc::clone(&h));

        let tile = match handle {
            Some(handle) => handle
                .tile
                .get_or_init(|| async {
                    let data = self.resolve_tile(&handle.coords).await;
                    self.pinned.pin(data.len() as u64);
                    // Report here, not at open(): the tile is produced
                    // on first read, so a gauge sampled only at open
                    // would never see the bytes it is meant to bound.
                    self.report_handle_gauge();
                    data
                })
                .instrument(fuse_read_span)
                .await
                .clone(),
            // No handle: either the memoisation budget was exhausted at
            // open() time, or the kernel sent a read we cannot attribute.
            // Serve it the old way rather than fail -- a missing handle
            // must never become a black texture.
            None => self.resolve_tile(coords).instrument(fuse_read_span).await,
        };

        let offset = offset as usize;
        let size = size as usize;

        let end = std::cmp::min(offset.saturating_add(size), tile.len());
        // Refcount, not memcpy: the reply borrows the tile's allocation and
        // the kernel gets its window without one byte being copied (#237).
        let slice = if offset < end {
            tile.slice(offset..end)
        } else {
            Bytes::new()
        };

        // The handler obtains exactly what it returns. The tile is produced
        // whole by the generation pipeline, which this counter does not --
        // and should not -- charge to the read path, so the ratio is 1.0
        // when delivery is zero-copy and climbs the moment it is not.
        self.record_read(slice.len() as u64, slice.len() as u64, true);

        Ok(slice)
    }

    /// Read a ranged window of a real file from its source (see
    /// [`Self::read`]).
    async fn read_real_file(
        &self,
        virtual_path: &Path,
        offset: u64,
        size: u32,
    ) -> Result<Bytes, FsError> {
        let filename = virtual_path
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("");
        let real_path = self
            .index
            .resolve(virtual_path)
            .map(|s| s.real_path.clone())
            .or_else(|| self.resolve_lazy_geo(virtual_path, filename))
            .ok_or(FsError::NotFound)?;

        // Acquire disk I/O permit
        let _permit = self.disk_io_limiter.acquire().await;

        // Read only the requested window. The kernel caps each read at
        // max_pages * PAGE_SIZE (1 MiB on Linux), so reading the whole file
        // here would move it once per call -- 238x for the largest installed
        // ortho DSF. See #233.
        let mut file = fs::File::open(&real_path).await?;
        file.seek(SeekFrom::Start(offset)).await?;

        // `take` bounds the read at `size`; `read_to_end` stops early at EOF,
        // which is how a range spanning the end becomes a short read.
        let mut buf = Vec::with_capacity(size as usize);
        (&mut file).take(size as u64).read_to_end(&mut buf).await?;

        // Seeking past EOF is legal and yields nothing, so this also covers
        // the at-or-past-EOF case without a separate branch.
        self.record_read(buf.len() as u64, buf.len() as u64, false);

        Ok(Bytes::from(buf))
    }

    /// List the union view's entries of directory `node`.
    ///
    /// `.` (the directory itself; the root's parent is itself), `..` (the
    /// parent, or the root), then the index's entries in their order.
    ///
    /// Directory entries carry their virtual attributes; file entries
    /// carry the placeholder (virtual DDS) attributes. A backend that needs
    /// real file sizes (FUSE `readdirplus`, WinFSP `ReadDirectory`) upgrades
    /// each file entry through [`Self::attrs`], exactly as `readdirplus`
    /// has always done -- while plain `readdir` stays I/O-free, as it has
    /// always been.
    pub async fn list_directory(&self, node: &FsNode) -> Result<Vec<DirEntry>, FsError> {
        let path = match node {
            FsNode::Directory(p) => p.clone(),
            _ => return Err(FsError::NotADirectory),
        };
        if !path.as_os_str().is_empty() && !self.index.is_directory(&path) {
            return Err(FsError::NotADirectory);
        }

        let mut entries = Vec::new();

        // Add . and ..
        entries.push(DirEntry {
            name: ".".to_string(),
            is_dir: true,
            node: node.clone(),
            attrs: virtual_dir_attrs(),
        });

        // The root's parent is itself.
        entries.push(DirEntry {
            name: "..".to_string(),
            is_dir: true,
            node: node.parent_node(),
            attrs: virtual_dir_attrs(),
        });

        // Get entries from union index
        for dir_entry in self.index.list_directory(&path) {
            let child_path = path.join(&dir_entry.name);
            let is_dir = dir_entry.is_dir;
            let child_node = if is_dir {
                FsNode::Directory(child_path.clone())
            } else {
                FsNode::RealFile(child_path.clone())
            };

            entries.push(DirEntry {
                name: dir_entry.name.to_string_lossy().into_owned(),
                is_dir,
                node: child_node,
                attrs: if is_dir {
                    virtual_dir_attrs()
                } else {
                    virtual_dds_attrs(&self.virtual_dds_config)
                },
            });
        }

        Ok(entries)
    }

    /// Open `node` for reading and return the per-open file handle.
    ///
    /// Real passthrough files and lit textures carry no per-open state and
    /// get handle 0. Ortho virtual DDS get a memoising handle, unless the
    /// pinned-bytes ceiling refuses: handle 0 then makes reads resolve per
    /// call, which is slower but correct.
    pub fn open(&self, node: &FsNode) -> u64 {
        let coords = match node {
            // Real passthrough files: use default kernel caching, no
            // handle state.
            FsNode::VirtualDds(c) => c,
            _ => return 0,
        };

        // Lit textures are small or served by range from their source; they
        // pin nothing and need no handle.
        if coords.kind == TextureKind::Lit {
            return 0;
        }

        // Refuse to memoise past the pinned-bytes ceiling. fh 0 makes read()
        // resolve per call, which is slower but correct.
        let expected = self.virtual_dds_config.size();
        if self.pinned.would_exceed(expected) {
            // Count every refusal: this is what lets a flight log tell "the
            // cap engaged" from "the #234 fix stopped working", which look
            // identical otherwise -- both show fuse_dds_alloc_mb climbing.
            if let Some(metrics) = self.metrics_client() {
                metrics.fuse_handle_budget_exhausted();
            }
            // Warn once, at default level. Before #236 this was debug-only,
            // so the degradation was invisible on an ordinary flight.
            if self.claim_budget_warning() {
                warn!(
                    pinned_mb = self.pinned.pinned_bytes() / (1024 * 1024),
                    cap_mb = self.pinned.ceiling_bytes() / (1024 * 1024),
                    open_handles = self.dds_handles.len(),
                    "DDS handle budget exhausted - serving opens without memoisation. \
                     Reads now resolve their tile once per call. Further occurrences \
                     are counted in dds_budget_exhausted on the Memory sample line."
                );
            }
            return 0;
        }

        let fh = self.new_dds_handle(coords.clone());

        self.report_handle_gauge();

        fh
    }

    /// Release the per-open state of handle `fh`.
    pub fn release(&self, fh: u64) {
        // Drop the memoised tile. Without this every open file would pin
        // 11.17 MB for the life of the mount.
        if let Some((_, handle)) = self.dds_handles.remove(&fh) {
            if let Some(tile) = handle.tile.get() {
                self.pinned.release(tile.len() as u64);
            }
            self.report_handle_gauge();
        }
    }

    /// Volume statistics: the union's file count with the current
    /// (constant) block figures, exactly as the FUSE `statfs` op reports.
    pub fn statfs(&self) -> StatFsInfo {
        StatFsInfo {
            blocks: 1_000_000,
            bfree: 0,
            bavail: 0,
            files: self.index.file_count() as u64,
            ffree: 0,
            bsize: 4096,
            namelen: 255,
            frsize: 4096,
        }
    }
}

impl DdsRequestor for OrthoUnionCore {
    fn dds_client(&self) -> &Arc<dyn DdsClient> {
        &self.dds_client
    }

    fn generation_timeout(&self) -> Duration {
        self.generation_timeout
    }

    fn context_label(&self) -> &'static str {
        "ortho_union"
    }

    fn tile_request_callback(&self) -> Option<&TileRequestCallback> {
        self.tile_request_callback.as_ref()
    }

    fn request_coalescer(&self) -> Option<&Arc<RequestCoalescer>> {
        Some(&self.request_coalescer)
    }

    fn metrics_client(&self) -> Option<&crate::metrics::MetricsClient> {
        self.metrics_client.as_ref()
    }

    fn on_dds_response(&self, tile: TileCoord, cache_hit: bool) {
        if let Some(ref observer) = self.state_observer {
            observer.observe(tile, cache_hit);
        }
    }
}
