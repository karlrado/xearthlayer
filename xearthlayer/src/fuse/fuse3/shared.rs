//! Shared traits and types for fuse3 filesystem implementations.
//!
//! This module provides common abstractions used by the fuse3 filesystem:
//! - [`Fuse3OrthoUnionFS`](super::Fuse3OrthoUnionFS) - Consolidated ortho mount
//!
//! The backend-neutral pieces (virtual-DDS configuration, coordinate
//! conversion, and the DDS request pipeline) live in
//! [`crate::fuse::support`] and are re-exported here so existing import
//! paths keep working (see `docs/dev/windows-backend-split.md` §6, commit 2).
//!
//! # Design Principles
//!
//! These traits follow SOLID principles:
//! - **Single Responsibility**: Each trait handles one concern
//! - **Interface Segregation**: Small, focused interfaces
//! - **Dependency Inversion**: Filesystems depend on abstractions

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuse3::raw::reply::FileAttr;
use fuse3::FileType;

use crate::fuse::support::attrs::{
    attrs_from_metadata, root_dir_attrs, virtual_dds_attrs, virtual_dir_attrs, FileAttributes,
    NodeKind,
};

/// Time-to-live for FUSE attribute caching.
///
/// This value is shared across all filesystem implementations.
pub const TTL: Duration = Duration::from_secs(1);

// Re-exports of the neutral core types moved to fuse/support by the
// backend split (windows-backend-split.md §6, commit 2) so existing
// import paths (`fuse::fuse3::shared::*`, `fuse::fuse3::*`) keep
// working unchanged.
pub use crate::fuse::support::attrs::{chunk_to_tile_coords, VirtualDdsConfig};
pub use crate::fuse::support::requestor::DdsRequestor;

// =============================================================================
// FileAttrBuilder - Trait for building FUSE file attributes
// =============================================================================

/// Trait for building FUSE file attributes.
///
/// This trait provides methods to convert various sources into [`FileAttr`]
/// structs suitable for FUSE responses. Implementations should use this
/// trait to ensure consistent attribute generation across filesystems.
///
/// The default implementations route through the neutral attribute mappers
/// in [`crate::fuse::support::attrs`] and [`to_file_attr`], so there is
/// exactly one place that knows how `std::fs::Metadata` and the virtual
/// configs become attributes (windows-backend-split.md §4.5).
///
/// # Example
///
/// ```ignore
/// impl FileAttrBuilder for MyFilesystem {
///     fn virtual_dds_config(&self) -> &VirtualDdsConfig {
///         &self.dds_config
///     }
/// }
///
/// // Now use the default implementations:
/// let attr = fs.virtual_dds_attr(inode);
/// let dir_attr = fs.root_dir_attr();
/// ```
pub trait FileAttrBuilder {
    /// Get the virtual DDS configuration.
    fn virtual_dds_config(&self) -> &VirtualDdsConfig;

    /// Convert filesystem metadata to FUSE file attributes.
    ///
    /// # Arguments
    ///
    /// * `ino` - Inode number to assign
    /// * `metadata` - Standard library filesystem metadata
    fn metadata_to_attr(&self, ino: u64, metadata: &std::fs::Metadata) -> FileAttr {
        let attrs = attrs_from_metadata(metadata);
        to_file_attr(ino, &attrs, UNIX_EPOCH, attrs.uid, attrs.gid)
    }

    /// Create attributes for a virtual DDS file.
    ///
    /// Virtual DDS files are generated on-demand and don't exist on disk.
    /// This method creates appropriate attributes for FUSE responses.
    ///
    /// # Arguments
    ///
    /// * `ino` - Inode number to assign
    fn virtual_dds_attr(&self, ino: u64) -> FileAttr {
        let attrs = virtual_dds_attrs(self.virtual_dds_config());
        to_file_attr(
            ino,
            &attrs,
            SystemTime::now(),
            unsafe { libc::getuid() },
            unsafe { libc::getgid() },
        )
    }

    /// Create attributes for the root directory.
    fn root_dir_attr(&self) -> FileAttr {
        to_file_attr(
            1,
            &root_dir_attrs(),
            SystemTime::now(),
            unsafe { libc::getuid() },
            unsafe { libc::getgid() },
        )
    }

    /// Create attributes for a virtual directory.
    ///
    /// # Arguments
    ///
    /// * `ino` - Inode number to assign
    fn virtual_dir_attr(&self, ino: u64) -> FileAttr {
        to_file_attr(
            ino,
            &virtual_dir_attrs(),
            SystemTime::now(),
            unsafe { libc::getuid() },
            unsafe { libc::getgid() },
        )
    }
}

/// Map neutral [`FileAttributes`] onto a fuse3 [`FileAttr`].
///
/// The neutral type deliberately carries no kernel concept, so the caller
/// supplies the adapter-side values: the inode number, `ctime` (real files
/// report `UNIX_EPOCH`; virtual nodes report the current time), and
/// ownership (real files keep the source's uid/gid; virtual nodes are owned
/// by the mounting process). The macOS-only `crtime` mirrors `ctime`; no
/// BSD flags are exposed.
///
/// This is the single place that knows how the neutral attribute seam
/// (windows-backend-split.md §4.3) becomes a FUSE attribute; the WinFSP
/// backend (Phase 2) will have its own equivalent mapper.
pub fn to_file_attr(
    ino: u64,
    attrs: &FileAttributes,
    ctime: SystemTime,
    uid: u32,
    gid: u32,
) -> FileAttr {
    let kind = match attrs.kind {
        NodeKind::Directory => FileType::Directory,
        NodeKind::Symlink => FileType::Symlink,
        NodeKind::RegularFile => FileType::RegularFile,
    };
    let ctime = ctime.into();

    FileAttr {
        ino,
        size: attrs.size,
        blocks: attrs.blocks,
        atime: attrs.atime.into(),
        mtime: attrs.mtime.into(),
        ctime,
        kind,
        perm: attrs.perm,
        nlink: attrs.nlink,
        uid,
        gid,
        rdev: attrs.rdev,
        blksize: attrs.blksize,
        // macFUSE's FileAttr carries the macOS-only creation time and
        // BSD file flags. Mirror ctime for crtime; we expose no flags.
        #[cfg(target_os = "macos")]
        crtime: ctime,
        #[cfg(target_os = "macos")]
        flags: 0,
    }
}
