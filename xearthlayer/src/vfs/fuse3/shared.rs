//! Shared traits and types for fuse3 filesystem implementations.
//!
//! This module provides common abstractions used by the fuse3 filesystem:
//! - [`Fuse3OrthoUnionFS`](super::Fuse3OrthoUnionFS) - Consolidated ortho mount
//!
//! # Design Principles
//!
//! These traits follow SOLID principles:
//! - **Single Responsibility**: Each trait handles one concern
//! - **Interface Segregation**: Small, focused interfaces
//! - **Dependency Inversion**: Filesystems depend on abstractions

use std::os::unix::fs::MetadataExt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use crate::vfs::support::attrs::{chunk_to_tile_coords, VirtualDdsConfig};
use crate::vfs::support::attrs::{FileAttributes, NodeKind};
pub use crate::vfs::support::requestor::DdsRequestor;
use fuse3::raw::reply::FileAttr;
use fuse3::FileType;

/// Time-to-live for FUSE attribute caching.
///
/// This value is shared across all filesystem implementations.
pub const TTL: Duration = Duration::from_secs(1);

// =============================================================================
// FileAttrBuilder - Trait for building FUSE file attributes
// =============================================================================

/// Trait for building FUSE file attributes.
///
/// This trait provides methods to convert various sources into [`FileAttr`]
/// structs suitable for FUSE responses. Implementations should use this
/// trait to ensure consistent attribute generation across filesystems.
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
        let kind = if metadata.is_dir() {
            FileType::Directory
        } else if metadata.is_symlink() {
            FileType::Symlink
        } else {
            FileType::RegularFile
        };

        FileAttr {
            ino,
            size: metadata.len(),
            blocks: metadata.blocks(),
            atime: metadata.accessed().unwrap_or(UNIX_EPOCH).into(),
            mtime: metadata.modified().unwrap_or(UNIX_EPOCH).into(),
            ctime: UNIX_EPOCH.into(),
            kind,
            perm: (metadata.mode() & 0o7777) as u16,
            nlink: metadata.nlink() as u32,
            uid: metadata.uid(),
            gid: metadata.gid(),
            rdev: metadata.rdev() as u32,
            blksize: 4096,
            // macFUSE's FileAttr carries the macOS-only creation time and
            // BSD file flags. Mirror ctime for crtime; we expose no flags.
            #[cfg(target_os = "macos")]
            crtime: UNIX_EPOCH.into(),
            #[cfg(target_os = "macos")]
            flags: 0,
        }
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
        let config = self.virtual_dds_config();
        let now = SystemTime::now().into();

        FileAttr {
            ino,
            size: config.size(),
            blocks: config.blocks(),
            atime: now,
            mtime: now,
            ctime: now,
            kind: FileType::RegularFile,
            perm: 0o444, // Read-only
            nlink: 1,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            blksize: config.blksize(),
            #[cfg(target_os = "macos")]
            crtime: now,
            #[cfg(target_os = "macos")]
            flags: 0,
        }
    }

    /// Create attributes for the root directory.
    fn root_dir_attr(&self) -> FileAttr {
        let now = SystemTime::now().into();

        FileAttr {
            ino: 1,
            size: 4096,
            blocks: 1,
            atime: now,
            mtime: now,
            ctime: now,
            kind: FileType::Directory,
            perm: 0o555,
            nlink: 2,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            blksize: 4096,
            #[cfg(target_os = "macos")]
            crtime: now,
            #[cfg(target_os = "macos")]
            flags: 0,
        }
    }

    /// Create attributes for a virtual directory.
    ///
    /// # Arguments
    ///
    /// * `ino` - Inode number to assign
    fn virtual_dir_attr(&self, ino: u64) -> FileAttr {
        let now = SystemTime::now().into();

        FileAttr {
            ino,
            size: 4096,
            blocks: 1,
            atime: now,
            mtime: now,
            ctime: now,
            kind: FileType::Directory,
            perm: 0o555,
            nlink: 2,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            blksize: 4096,
            #[cfg(target_os = "macos")]
            crtime: now,
            #[cfg(target_os = "macos")]
            flags: 0,
        }
    }
}

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
        #[cfg(target_os = "macos")]
        crtime: ctime,
        #[cfg(target_os = "macos")]
        flags: 0,
    }
}
