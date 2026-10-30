//! Neutral file-attribute types and the shared virtual-DDS configuration.
//!
//! These types describe *what* a file's attributes are without naming the
//! kernel that reports them. The FUSE adapter maps them onto
//! `fuse3::FileAttr` (adding the inode number, the adapter's `ctime`/uid/gid
//! policy — `UNIX_EPOCH` and the source's uid/gid for real files, the
//! current time and the mounting process for virtual nodes — and the
//! macOS-only `crtime`/`flags` fields); the WinFSP adapter (Phase 2) maps
//! them onto WinFSP's attribute types.
//!
//! See `docs/dev/windows-backend-split.md` §4.3 for the design.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::coord::TileCoord;
use crate::fuse::DdsFilename;

use super::node::FsNode;

// =============================================================================
// NodeKind + FileAttributes — the neutral attribute seam
// =============================================================================

/// The kind of a filesystem node, independent of backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    /// A directory.
    Directory,
    /// A regular file.
    RegularFile,
    /// A symlink.
    Symlink,
}

/// File attributes in the neutral form shared by all backends.
///
/// `uid`/`gid` are carried for the backends that report them: the FUSE
/// mapper fills them from the file metadata (real files) or the process
/// (virtual files), so `libc`/`MetadataExt` calls stay out of core logic.
/// On Windows they are harmless and the WinFSP adapter can map them to the
/// launching user or drop them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileAttributes {
    /// File size in bytes.
    pub size: u64,
    /// Number of 512-byte blocks allocated (POSIX convention).
    pub blocks: u64,
    /// Last access time.
    pub atime: SystemTime,
    /// Last modification time.
    pub mtime: SystemTime,
    /// Node kind.
    pub kind: NodeKind,
    /// Permission bits (e.g. `0o444`).
    pub perm: u16,
    /// Link count.
    pub nlink: u32,
    /// Owner user id.
    pub uid: u32,
    /// Owner group id.
    pub gid: u32,
    /// Device number (0 for non-device files).
    pub rdev: u32,
    /// Filesystem block size.
    pub blksize: u32,
}

/// Map `std::fs::Metadata` onto neutral attributes.
///
/// Uses the portable `Metadata` surface; on Unix it additionally carries
/// the extended fields (`blocks`, `perm`, `nlink`, `uid`, `gid`, `rdev`)
/// that the FUSE backend reports today. Other platforms get the neutral
/// defaults for those fields.
pub fn attrs_from_metadata(metadata: &std::fs::Metadata) -> FileAttributes {
    let kind = if metadata.is_dir() {
        NodeKind::Directory
    } else if metadata.is_symlink() {
        NodeKind::Symlink
    } else {
        NodeKind::RegularFile
    };

    let mut attrs = FileAttributes {
        size: metadata.len(),
        blocks: 0,
        atime: metadata.accessed().unwrap_or(UNIX_EPOCH),
        mtime: metadata.modified().unwrap_or(UNIX_EPOCH),
        kind,
        perm: 0o444,
        nlink: 1,
        uid: 0,
        gid: 0,
        rdev: 0,
        blksize: 4096,
    };

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        attrs.blocks = metadata.blocks();
        attrs.perm = (metadata.mode() & 0o7777) as u16;
        attrs.nlink = metadata.nlink() as u32;
        attrs.uid = metadata.uid();
        attrs.gid = metadata.gid();
        attrs.rdev = metadata.rdev() as u32;
    }

    attrs
}

/// Attributes for a generated (virtual) DDS file: read-only, single link.
pub fn virtual_dds_attrs(config: &VirtualDdsConfig) -> FileAttributes {
    let now = SystemTime::now();
    FileAttributes {
        size: config.size(),
        blocks: config.blocks(),
        atime: now,
        mtime: now,
        kind: NodeKind::RegularFile,
        perm: 0o444, // Read-only
        nlink: 1,
        uid: 0,
        gid: 0,
        rdev: 0,
        blksize: config.blksize(),
    }
}

/// Attributes for a virtual directory in the union view.
pub fn virtual_dir_attrs() -> FileAttributes {
    let now = SystemTime::now();
    FileAttributes {
        size: 4096,
        blocks: 1,
        atime: now,
        mtime: now,
        kind: NodeKind::Directory,
        perm: 0o555,
        nlink: 2,
        uid: 0,
        gid: 0,
        rdev: 0,
        blksize: 4096,
    }
}

/// Attributes for the root directory.
///
/// Identical to a virtual directory; the inode number is an adapter-side
/// concept and is not part of [`FileAttributes`].
pub fn root_dir_attrs() -> FileAttributes {
    virtual_dir_attrs()
}

// =============================================================================
// Neutral listing / volume types
// =============================================================================

/// One entry in a directory listing, in neutral form.
#[derive(Debug, Clone, PartialEq)]
pub struct DirEntry {
    /// Entry name (as the kernel sees it).
    pub name: String,
    /// Whether the entry is a directory.
    pub is_dir: bool,
    /// The child's node identity (drives the `.`/`..` mapping).
    pub node: FsNode,
    /// The child's attributes.
    pub attrs: FileAttributes,
}

/// Volume statistics in neutral form (FUSE `statfs` / WinFSP `GetVolumeInfo`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatFsInfo {
    /// Total data blocks.
    pub blocks: u64,
    /// Free blocks.
    pub bfree: u64,
    /// Free blocks available to unprivileged users.
    pub bavail: u64,
    /// Total inodes.
    pub files: u64,
    /// Free inodes.
    pub ffree: u64,
    /// Block size.
    pub bsize: u32,
    /// Maximum filename length.
    pub namelen: u32,
    /// Fundamental block size.
    pub frsize: u32,
}

// =============================================================================
// VirtualDdsConfig - Shared DDS file configuration
// =============================================================================

/// Configuration for virtual DDS file attributes.
///
/// Virtual DDS files are generated on-demand by XEarthLayer. This struct
/// holds the configuration for reporting their attributes to the kernel.
#[derive(Debug, Clone)]
pub struct VirtualDdsConfig {
    /// Expected size of generated DDS files (bytes).
    size: u64,
    /// Block size for filesystem reporting.
    blksize: u32,
}

impl VirtualDdsConfig {
    /// Create a new virtual DDS configuration.
    ///
    /// # Arguments
    ///
    /// * `size` - Expected size of generated DDS files in bytes
    pub fn new(size: u64) -> Self {
        Self {
            size,
            blksize: 4096,
        }
    }

    /// Get the expected file size.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Get the block size.
    pub fn blksize(&self) -> u32 {
        self.blksize
    }

    /// Calculate the number of blocks for this file size.
    pub fn blocks(&self) -> u64 {
        self.size.div_ceil(self.blksize as u64)
    }
}

impl Default for VirtualDdsConfig {
    fn default() -> Self {
        // Default DDS size: 4096x4096 BC1 with mipmaps ≈ 11MB
        Self::new(11_184_952)
    }
}

// =============================================================================
// Tile Coordinate Conversion
// =============================================================================

/// Convert chunk-level DDS coordinates to tile-level coordinates.
///
/// DDS filenames use chunk coordinates (16x smaller grid than tiles).
/// This function converts chunk coordinates to tile coordinates for
/// the DDS generation pipeline.
///
/// # Coordinate Relationship
///
/// ```text
/// Chunk zoom = Tile zoom + 4
/// Chunk row  = Tile row * 16 + offset
/// Chunk col  = Tile col * 16 + offset
/// ```
///
/// # Arguments
///
/// * `coords` - Parsed DDS filename with chunk coordinates
///
/// # Returns
///
/// Tile coordinates suitable for the DDS handler
pub fn chunk_to_tile_coords(coords: &DdsFilename) -> TileCoord {
    let tile_zoom = coords.zoom.saturating_sub(4);
    TileCoord {
        row: coords.row / 16,
        col: coords.col / 16,
        zoom: tile_zoom,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- VirtualDdsConfig (moved from fuse3/shared.rs) ---

    #[test]
    fn test_virtual_dds_config_new() {
        let config = VirtualDdsConfig::new(1024);
        assert_eq!(config.size(), 1024);
        assert_eq!(config.blksize(), 4096);
    }

    #[test]
    fn test_virtual_dds_config_blocks() {
        // Exact block boundary
        let config = VirtualDdsConfig::new(4096);
        assert_eq!(config.blocks(), 1);

        // One byte over
        let config = VirtualDdsConfig::new(4097);
        assert_eq!(config.blocks(), 2);

        // Large file
        let config = VirtualDdsConfig::new(11_184_952);
        assert_eq!(config.blocks(), 2731);
    }

    #[test]
    fn test_virtual_dds_config_default() {
        let config = VirtualDdsConfig::default();
        assert_eq!(config.size(), 11_184_952);
    }

    // --- chunk_to_tile_coords (moved from fuse3/shared.rs) ---

    #[test]
    fn test_chunk_to_tile_coords() {
        // Standard zoom 18 chunk
        let coords = DdsFilename {
            row: 100000,
            col: 125184,
            zoom: 18,
            map_type: "BI".to_string(),
            kind: crate::fuse::TextureKind::Ortho,
        };
        let tile = chunk_to_tile_coords(&coords);
        assert_eq!(tile.row, 100000 / 16);
        assert_eq!(tile.col, 125184 / 16);
        assert_eq!(tile.zoom, 14);
    }

    #[test]
    fn test_chunk_to_tile_coords_zoom_16() {
        let coords = DdsFilename {
            row: 25264,
            col: 10368,
            zoom: 16,
            map_type: "GO2".to_string(),
            kind: crate::fuse::TextureKind::Ortho,
        };
        let tile = chunk_to_tile_coords(&coords);
        assert_eq!(tile.row, 25264 / 16);
        assert_eq!(tile.col, 10368 / 16);
        assert_eq!(tile.zoom, 12);
    }

    #[test]
    fn test_chunk_to_tile_coords_low_zoom() {
        // Edge case: zoom less than 4
        let coords = DdsFilename {
            row: 100,
            col: 200,
            zoom: 2,
            map_type: "BI".to_string(),
            kind: crate::fuse::TextureKind::Ortho,
        };
        let tile = chunk_to_tile_coords(&coords);
        assert_eq!(tile.zoom, 0); // saturating_sub prevents underflow
    }

    // --- neutral attribute mappers ---

    #[test]
    fn virtual_dds_attrs_match_the_config() {
        let config = VirtualDdsConfig::new(11_184_952);
        let attrs = virtual_dds_attrs(&config);

        assert_eq!(attrs.size, 11_184_952);
        assert_eq!(attrs.blocks, config.blocks());
        assert_eq!(attrs.blksize, 4096);
        assert_eq!(attrs.kind, NodeKind::RegularFile);
        assert_eq!(attrs.perm, 0o444);
        assert_eq!(attrs.nlink, 1);
        assert_eq!(attrs.rdev, 0);
        // uid/gid are filled by the FUSE mapper (process identity).
        assert_eq!(attrs.uid, 0);
        assert_eq!(attrs.gid, 0);
    }

    #[test]
    fn virtual_and_root_dir_attrs_are_read_only_directories() {
        let dir = virtual_dir_attrs();

        assert_eq!(dir.size, 4096);
        assert_eq!(dir.blocks, 1);
        assert_eq!(dir.kind, NodeKind::Directory);
        assert_eq!(dir.perm, 0o555);
        assert_eq!(dir.nlink, 2);
        assert_eq!(dir.rdev, 0);
        assert_eq!(dir.blksize, 4096);

        // The root dir's attributes are identical; only the inode
        // (adapter-side) differs. (atime/mtime are taken from the clock
        // independently in each call, so compare the stable fields.)
        let root = root_dir_attrs();
        assert_eq!(root.size, dir.size);
        assert_eq!(root.blocks, dir.blocks);
        assert_eq!(root.kind, dir.kind);
        assert_eq!(root.perm, dir.perm);
        assert_eq!(root.nlink, dir.nlink);
        assert_eq!(root.uid, dir.uid);
        assert_eq!(root.gid, dir.gid);
        assert_eq!(root.rdev, dir.rdev);
        assert_eq!(root.blksize, dir.blksize);
    }

    #[test]
    fn attrs_from_metadata_reports_kind_size_and_blksize() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tile.dsf");
        std::fs::write(&file, b"1234").unwrap();

        let file_attrs = attrs_from_metadata(&file.metadata().unwrap());
        assert_eq!(file_attrs.kind, NodeKind::RegularFile);
        assert_eq!(file_attrs.size, 4);
        assert_eq!(file_attrs.blksize, 4096);
        assert_eq!(
            file_attrs.atime,
            file.metadata().unwrap().accessed().unwrap()
        );

        let dir_attrs = attrs_from_metadata(&dir.path().metadata().unwrap());
        assert_eq!(dir_attrs.kind, NodeKind::Directory);
    }

    #[cfg(unix)]
    #[test]
    fn attrs_from_metadata_carries_the_unix_ext_fields() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tile.dsf");
        std::fs::write(&file, b"1234").unwrap();

        let meta = file.metadata().unwrap();
        let attrs = attrs_from_metadata(&meta);

        use std::os::unix::fs::MetadataExt;
        assert_eq!(attrs.blocks, meta.blocks());
        assert_eq!(attrs.perm, (meta.mode() & 0o7777) as u16);
        assert_eq!(attrs.nlink, meta.nlink() as u32);
        assert_eq!(attrs.uid, meta.uid());
        assert_eq!(attrs.gid, meta.gid());
        assert_eq!(attrs.rdev, meta.rdev() as u32);
    }

    // --- neutral listing / volume types ---

    #[test]
    fn dir_entry_carries_name_node_and_attrs() {
        let entry = DirEntry {
            name: "textures".to_string(),
            is_dir: true,
            node: FsNode::root(),
            attrs: virtual_dir_attrs(),
        };

        assert_eq!(entry.name, "textures");
        assert!(entry.is_dir);
        assert_eq!(entry.node, FsNode::root());
        assert_eq!(entry.attrs.kind, NodeKind::Directory);
    }

    #[test]
    fn stat_fs_info_carries_volume_fields() {
        let stat = StatFsInfo {
            blocks: 1_000_000,
            bfree: 0,
            bavail: 0,
            files: 42,
            ffree: 0,
            bsize: 4096,
            namelen: 255,
            frsize: 4096,
        };

        assert_eq!(stat.blocks, 1_000_000);
        assert_eq!(stat.files, 42);
        assert_eq!(stat.bsize, 4096);
        assert_eq!(stat.namelen, 255);
        assert_eq!(stat.frsize, 4096);
    }
}
