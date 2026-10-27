//! Neutral filesystem-node identity shared by all backends.
//!
//! This module is backend-agnostic: it names *what a node is* (a directory,
//! a real file, or a generated texture) without naming *how a kernel
//! identifies it*. The FUSE backend maps kernel inode numbers onto
//! [`FsNode`] via [`crate::fuse::support::inode::InodeManager`]; the WinFSP
//! backend (Windows) stores an [`FsNode`] in each file-context object. The
//! core operates only on [`FsNode`], which is what keeps it free of any
//! FUSE concept.
//!
//! See `docs/dev/windows-backend-split.md` §4.2 for the design.

use crate::fuse::DdsFilename;
use std::path::{Path, PathBuf};

/// A filesystem node in the ortho union view, independent of backend.
///
/// - [`Directory`](FsNode::Directory): a directory in the union view. The
///   root is the empty path.
/// - [`RealFile`](FsNode::RealFile): a real file from a scenery source,
///   addressed by its *virtual* path. Resolution to the source's real path
///   happens inside the core ops (union index, then lazy geospatial
///   resolution) — the node carries no source identity.
/// - [`VirtualDds`](FsNode::VirtualDds): a generated texture, ortho or lit
///   (see [`DdsFilename::kind`]).
///
/// # Example
///
/// ```ignore
/// use xearthlayer::fuse::support::node::FsNode;
/// use std::path::PathBuf;
///
/// let root = FsNode::root();
/// assert!(root.is_directory());
/// assert!(root.is_root());
///
/// let textures = FsNode::Directory(PathBuf::from("textures"));
/// assert!(!textures.is_root());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsNode {
    /// A directory in the union view. Root = empty path.
    Directory(PathBuf),
    /// A real file from a source, by its virtual path.
    RealFile(PathBuf),
    /// A generated texture: ortho or lit.
    VirtualDds(DdsFilename),
}

impl FsNode {
    /// The root directory (FUSE root inode 1 / the WinFSP volume root).
    pub fn root() -> Self {
        FsNode::Directory(PathBuf::new())
    }

    /// Whether this node is a directory.
    ///
    /// Part of the neutral API surface; the FUSE adapter keys off `NodeKind`
    /// today, so this stays unused until the WinFSP backend (Phase 2).
    #[allow(dead_code)]
    pub fn is_directory(&self) -> bool {
        matches!(self, FsNode::Directory(_))
    }

    /// Whether this node is the root directory.
    ///
    /// Only directories can be the root; a virtual DDS or real file is never
    /// root.
    ///
    /// Part of the neutral API surface; unused until the WinFSP backend
    /// (Phase 2) needs it.
    #[allow(dead_code)]
    pub fn is_root(&self) -> bool {
        match self {
            FsNode::Directory(p) => p.as_os_str().is_empty(),
            _ => false,
        }
    }

    /// The virtual path for directory and real-file nodes.
    ///
    /// Returns `None` for [`VirtualDds`](FsNode::VirtualDds) nodes, which
    /// carry coordinates rather than a path.
    pub fn path(&self) -> Option<&Path> {
        match self {
            FsNode::Directory(p) | FsNode::RealFile(p) => Some(p),
            FsNode::VirtualDds(_) => None,
        }
    }

    /// The node the `..` entry of a directory listing points at: the parent
    /// directory, or the node itself when it is the root.
    ///
    /// Defined for [`Directory`](FsNode::Directory) nodes. For other node
    /// kinds (which never appear as a listing's parent) this returns the
    /// node unchanged, so callers always get a well-formed [`FsNode`].
    ///
    /// # Example
    ///
    /// ```ignore
    /// use xearthlayer::fuse::support::node::FsNode;
    /// use std::path::PathBuf;
    ///
    /// // The root's parent is itself.
    /// assert_eq!(FsNode::root().parent_node(), FsNode::root());
    ///
    /// // One level down, `..` is the root.
    /// let textures = FsNode::Directory(PathBuf::from("textures"));
    /// assert_eq!(textures.parent_node(), FsNode::root());
    ///
    /// // Two levels down, `..` is the parent directory.
    /// let deep = FsNode::Directory(PathBuf::from("a/b"));
    /// assert_eq!(deep.parent_node(), FsNode::Directory(PathBuf::from("a")));
    /// ```
    pub fn parent_node(&self) -> FsNode {
        match self {
            // Path::parent of "a/b" is "a"; of "a" it is "" (the root);
            // of "" (root) it is None, which we map back to the root.
            FsNode::Directory(p) => {
                let parent = p.parent().map(PathBuf::from).unwrap_or_default();
                FsNode::Directory(parent)
            }
            other => other.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fuse::TextureKind;

    fn coords(row: u32, col: u32) -> DdsFilename {
        DdsFilename {
            row,
            col,
            zoom: 16,
            map_type: "BI".to_string(),
            kind: TextureKind::Ortho,
        }
    }

    #[test]
    fn root_is_an_empty_directory() {
        let root = FsNode::root();

        assert!(root.is_directory());
        assert!(root.is_root());
        assert_eq!(root.path(), Some(Path::new("")));
    }

    #[test]
    fn node_kind_predicates() {
        assert!(FsNode::Directory(PathBuf::from("textures")).is_directory());
        assert!(!FsNode::RealFile(PathBuf::from("textures/tile.dsf")).is_directory());
        assert!(!FsNode::VirtualDds(coords(1, 2)).is_directory());

        assert!(FsNode::RealFile(PathBuf::from("f.dsf")).path().is_some());
        assert!(FsNode::VirtualDds(coords(1, 2)).path().is_none());
    }

    #[test]
    fn only_the_empty_directory_is_root() {
        assert!(FsNode::root().is_root());
        assert!(!FsNode::Directory(PathBuf::from("textures")).is_root());
        assert!(!FsNode::RealFile(PathBuf::new()).is_root());
        assert!(!FsNode::VirtualDds(coords(1, 2)).is_root());
    }

    #[test]
    fn parent_of_root_is_root() {
        assert_eq!(FsNode::root().parent_node(), FsNode::root());
    }

    #[test]
    fn parent_of_a_first_level_directory_is_root() {
        let textures = FsNode::Directory(PathBuf::from("textures"));

        assert_eq!(textures.parent_node(), FsNode::root());
    }

    #[test]
    fn parent_of_a_nested_directory_is_its_parent() {
        let deep = FsNode::Directory(PathBuf::from("a/b"));

        assert_eq!(deep.parent_node(), FsNode::Directory(PathBuf::from("a")));
    }

    #[test]
    fn parent_of_a_three_level_directory() {
        let deep = FsNode::Directory(PathBuf::from("a/b/c"));

        assert_eq!(deep.parent_node(), FsNode::Directory(PathBuf::from("a/b")));
    }

    #[test]
    fn parent_node_of_non_directories_is_the_node_itself() {
        // `..` synthesis only applies to directories; the total function
        // still returns a well-formed node for every other kind.
        let file = FsNode::RealFile(PathBuf::from("textures/tile.dsf"));
        assert_eq!(file.parent_node(), file);

        let dds = FsNode::VirtualDds(coords(100, 200));
        assert_eq!(dds.parent_node(), dds);
    }

    #[test]
    fn virtual_dds_carries_coords_and_never_a_path() {
        let coords = coords(25712, 12960);
        let node = FsNode::VirtualDds(coords.clone());

        assert!(!node.is_directory());
        assert!(!node.is_root());
        assert_eq!(node.path(), None);
        match &node {
            FsNode::VirtualDds(c) => assert_eq!(*c, coords),
            _ => unreachable!(),
        }
    }
}
