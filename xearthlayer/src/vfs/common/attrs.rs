//! Neutral attribute, entry and volume types shared by all backends.
//!
//! See `docs/dev/filesystem-backend-split.md` §Union Filesystem. These are
//! the shapes the core answers with; each backend maps them onto its
//! kernel's reply type (the fuse3 adapter builds a `FileAttr`, filling the
//! macFUSE-only fields per its `FusePlatform` policy; a WinFSP backend
//! would fill a `FILE_ATTRIBUTE_DATA`).

use std::ffi::OsString;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::coord::TileCoord;
use crate::vfs::DdsFilename;

use super::node::FsNode;

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
/// `ctime` is the POSIX status-change time, carried for the FUSE `FileAttr`
/// it feeds. `crtime` (creation time) exists as a `FileAttr` field only in
/// macFUSE; the core supplies it here, and the platform policy decides
/// whether it reaches the kernel (see `vfs/fuse3/fuse_platform.rs`).
///
/// `uid`/`gid` are carried for the backends that report them; the core
/// fills them from file metadata (real files) or the process (virtual
/// files), keeping `libc` calls out of core logic's reply types. On Windows
/// they are harmless and the WinFSP adapter can map them to the launching
/// user or drop them.
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
    /// Last metadata or status change time.
    pub ctime: SystemTime,
    /// Creation time.
    pub crtime: SystemTime,
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

/// A resolved node with its attributes: the answer to `lookup`.
///
/// Carrying the attributes with the node matches the kernel's
/// lookup-then-getattr pattern in one round trip, and lets a directory
/// listing hand each entry its attributes for readdirplus-style replies
/// without a second resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The resolved node's identity.
    pub node: FsNode,
    /// The resolved node's attributes.
    pub attrs: FileAttributes,
}

/// One entry in a directory listing, in neutral form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// Entry name (as the kernel sees it).
    pub name: OsString,
    /// The child it names, with its attributes.
    pub entry: Entry,
}

/// Volume statistics in neutral form (FUSE `statfs` / WinFSP
/// `GetVolumeInfo`).
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
// Attribute mappers — the neutral answers the core gives
// =============================================================================

/// Map `std::fs::Metadata` onto neutral attributes.
///
/// Uses the portable `Metadata` surface; on Unix it additionally carries
/// the extended fields (`blocks`, `perm`, `nlink`, `uid`, `gid`, `rdev`)
/// that the FUSE backend reports today. Other platforms get the neutral
/// defaults for those fields. `ctime`/`crtime` follow the adapter values
/// this replaced: FUSE's `ctime` is `UNIX_EPOCH` for real files, and so is
/// macFUSE's `crtime` (there is no portable creation time to read).
pub fn attrs_from_metadata(metadata: &std::fs::Metadata) -> FileAttributes {
    let kind = if metadata.is_dir() {
        NodeKind::Directory
    } else if metadata.is_symlink() {
        NodeKind::Symlink
    } else {
        NodeKind::RegularFile
    };

    let base = FileAttributes {
        size: metadata.len(),
        blocks: 0,
        atime: metadata.accessed().unwrap_or(UNIX_EPOCH),
        mtime: metadata.modified().unwrap_or(UNIX_EPOCH),
        ctime: UNIX_EPOCH,
        crtime: UNIX_EPOCH,
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
        FileAttributes {
            blocks: metadata.blocks(),
            perm: (metadata.mode() & 0o7777) as u16,
            nlink: metadata.nlink() as u32,
            uid: metadata.uid(),
            gid: metadata.gid(),
            rdev: metadata.rdev() as u32,
            ..base
        }
    }
    #[cfg(not(unix))]
    {
        base
    }
}

/// Attributes for a generated (virtual) DDS file: read-only, single link.
///
/// The file is owned by the process serving the mount, as the fuse3
/// attribute builders this replaced reported it. `libc` is a portable OS
/// call, not a backend symbol, so the fill is neutral; the WinFSP backend
/// will override with the launching user.
pub fn virtual_dds_attrs(config: &VirtualDdsConfig) -> FileAttributes {
    let now = SystemTime::now();
    FileAttributes {
        size: config.size(),
        blocks: config.blocks(),
        atime: now,
        mtime: now,
        ctime: now,
        crtime: now,
        kind: NodeKind::RegularFile,
        perm: 0o444, // Read-only
        nlink: 1,
        uid: process_uid(),
        gid: process_gid(),
        rdev: 0,
        blksize: config.blksize(),
    }
}

/// Attributes for a virtual directory in the union view.
///
/// Owned by the serving process, as in [`virtual_dds_attrs`].
pub fn virtual_dir_attrs() -> FileAttributes {
    let now = SystemTime::now();
    FileAttributes {
        size: 4096,
        blocks: 1,
        atime: now,
        mtime: now,
        ctime: now,
        crtime: now,
        kind: NodeKind::Directory,
        perm: 0o555,
        nlink: 2,
        uid: process_uid(),
        gid: process_gid(),
        rdev: 0,
        blksize: 4096,
    }
}

/// Owner uid of the process serving the mount (0 where there is none).
fn process_uid() -> u32 {
    #[cfg(unix)]
    unsafe {
        libc::getuid()
    }
    #[cfg(not(unix))]
    {
        0
    }
}

/// Owner gid of the process serving the mount (0 where there is none).
fn process_gid() -> u32 {
    #[cfg(unix)]
    unsafe {
        libc::getgid()
    }
    #[cfg(not(unix))]
    {
        0
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
    use crate::vfs::TextureKind;
    use std::path::PathBuf;
    use std::time::UNIX_EPOCH;

    fn test_attrs(kind: NodeKind) -> FileAttributes {
        FileAttributes {
            size: 4096,
            blocks: 8,
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            crtime: UNIX_EPOCH,
            kind,
            perm: 0o444,
            nlink: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
            blksize: 4096,
        }
    }

    #[test]
    fn entry_pairs_a_node_with_its_attributes() {
        let node = FsNode::Directory(PathBuf::from("textures"));
        let entry = Entry {
            node: node.clone(),
            attrs: test_attrs(NodeKind::Directory),
        };

        assert_eq!(entry.node, node);
        assert_eq!(entry.attrs.kind, NodeKind::Directory);
    }

    #[test]
    fn dir_entry_names_a_child_entry() {
        let entry = DirEntry {
            name: OsString::from("tile.dsf"),
            entry: Entry {
                node: FsNode::RealFile(PathBuf::from("textures/tile.dsf")),
                attrs: test_attrs(NodeKind::RegularFile),
            },
        };

        assert_eq!(entry.name, OsString::from("tile.dsf"));
        assert_eq!(
            entry.entry.node,
            FsNode::RealFile(PathBuf::from("textures/tile.dsf"))
        );
        assert_eq!(entry.entry.attrs.kind, NodeKind::RegularFile);
    }

    #[test]
    fn dir_entry_name_is_os_independent() {
        // Names reach backends as the kernel sees them; keep the neutral
        // form a byte string, not a String (Windows clients can ask for
        // names that are not valid UTF-8).
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let odd = DirEntry {
                name: OsString::from_vec(vec![0x66, 0x69, 0x6c, 0xff]),
                entry: Entry {
                    node: FsNode::RealFile(PathBuf::from("fil\u{fffd}")),
                    attrs: test_attrs(NodeKind::RegularFile),
                },
            };
            assert_eq!(odd.name.as_encoded_bytes(), &[0x66, 0x69, 0x6c, 0xff]);
        }
    }

    #[test]
    fn file_attributes_carry_both_change_and_creation_times() {
        // FUSE's FileAttr wants ctime on every platform and crtime on
        // macFUSE; both must be present in the neutral form so no backend
        // is missing a field it may need to fill.
        let attrs = test_attrs(NodeKind::RegularFile);
        assert_eq!(attrs.ctime, UNIX_EPOCH);
        assert_eq!(attrs.crtime, UNIX_EPOCH);
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

    #[test]
    fn virtual_dds_node_survives_entry_round_trip() {
        let coords = crate::vfs::DdsFilename {
            row: 25712,
            col: 12960,
            zoom: 16,
            map_type: "BI".to_string(),
            kind: TextureKind::Ortho,
        };
        let entry = Entry {
            node: FsNode::VirtualDds(coords.clone()),
            attrs: test_attrs(NodeKind::RegularFile),
        };

        assert_eq!(entry, entry.clone());
        match entry.node {
            FsNode::VirtualDds(c) => assert_eq!(c, coords),
            _ => panic!("expected VirtualDds node"),
        }
    }

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
            kind: TextureKind::Ortho,
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
            kind: TextureKind::Ortho,
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
            kind: TextureKind::Ortho,
        };
        let tile = chunk_to_tile_coords(&coords);
        assert_eq!(tile.zoom, 0); // saturating_sub prevents underflow
    }

    // --- neutral attribute mappers ---

    #[test]
    fn attrs_from_metadata_maps_kind_and_times() {
        // ctime/crtime follow the fuse3 values this replaces: UNIX_EPOCH
        // for real files on every platform.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("tile.dsf");
        std::fs::write(&file, b"1234").unwrap();

        let attrs = attrs_from_metadata(&file.metadata().unwrap());
        assert_eq!(attrs.kind, NodeKind::RegularFile);
        assert_eq!(attrs.size, 4);
        assert_eq!(attrs.blksize, 4096);
        assert_eq!(attrs.ctime, UNIX_EPOCH);
        assert_eq!(attrs.crtime, UNIX_EPOCH);
        assert_eq!(attrs.atime, file.metadata().unwrap().accessed().unwrap());

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
        // uid/gid are the serving process, as the fuse3 builders this
        // replaced reported them.
        #[cfg(unix)]
        unsafe {
            assert_eq!(attrs.uid, libc::getuid());
            assert_eq!(attrs.gid, libc::getgid());
        }
        #[cfg(not(unix))]
        {
            assert_eq!(attrs.uid, 0);
            assert_eq!(attrs.gid, 0);
        }
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
        // (adapter-side) differs. (atime/mtime/ctime/crtime come from the
        // clock per call, so compare the stable fields.)
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
}
