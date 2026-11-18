//! Neutral attribute, entry and volume types shared by all backends.
//!
//! See `docs/dev/filesystem-backend-split.md` §Union Filesystem. These are
//! the shapes the core answers with; each backend maps them onto its
//! kernel's reply type (the fuse3 adapter builds a `FileAttr`, filling the
//! macFUSE-only fields per its `FusePlatform` policy; a WinFSP backend
//! would fill a `FILE_ATTRIBUTE_DATA`).

use std::ffi::OsString;
use std::time::SystemTime;

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
}
