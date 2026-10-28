//! Path resolution for the WinFSP backend, in pure and cross-platform form.
//!
//! WinFSP passes each callback a **full relative path** (e.g.
//! `terrain\NA\us\ca\...\x.dds`), unlike FUSE's parent-inode + single-name
//! model, so the adapter walks the path component by component through the
//! core's per-name `lookup` (which may stat real source files, and so is
//! async). Everything here is std-only — no `winfsp` types — so CI unit-tests
//! it on every OS even though the caller (`context.rs`) is Windows-only
//! (design `docs/dev/winfsp-backend.md` §4.3, §7.6).

use crate::fuse::support::attrs::FileAttributes;
use crate::fuse::support::error::FsError;
use crate::fuse::support::node::FsNode;

/// Split a driver path into its components.
///
/// WinFSP paths are relative to the volume root, may carry a leading
/// separator, and may mix `\` and `/` (both are legal separators on
/// Windows). Empty components (leading or doubled separators) are dropped,
/// so `""`, `"\\"` and `"/"` all name the root (no components).
///
/// Component case and spelling are preserved: the union index is keyed on
/// the exact names we enumerate, and X-Plane reads back those same names
/// (design §8, case-sensitivity row).
pub fn split_components(path: &str) -> Vec<String> {
    path.split(['\\', '/'])
        .filter(|component| !component.is_empty())
        .map(str::to_string)
        .collect()
}

/// Walk `components` from the volume root through a per-name `lookup`,
/// returning the final node.
///
/// Generic over the lookup so the walk itself is unit-testable without a
/// core (the production lookup is the adapter's `block_on` of
/// `OrthoUnionCore::lookup`, design §3.4; the test fake is a table below).
/// The first `NotFound`/`NotADirectory` from the lookup stops the walk and
/// is reported unchanged — exactly the error a single FUSE `lookup` of the
/// same name would have produced.
pub fn walk<L>(components: &[String], mut lookup: L) -> Result<FsNode, FsError>
where
    L: FnMut(&FsNode, &str) -> Result<(FsNode, FileAttributes), FsError>,
{
    let mut node = FsNode::root();
    for name in components {
        let (child, _attrs) = lookup(&node, name)?;
        node = child;
    }
    Ok(node)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::time::UNIX_EPOCH;

    use crate::fuse::support::attrs::NodeKind;

    fn dummy_attrs() -> FileAttributes {
        FileAttributes {
            size: 0,
            blocks: 0,
            atime: UNIX_EPOCH,
            mtime: UNIX_EPOCH,
            kind: NodeKind::RegularFile,
            perm: 0,
            nlink: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
            blksize: 4096,
        }
    }

    // --- split_components ---

    #[test]
    fn a_single_name_is_its_own_component() {
        assert_eq!(split_components("textures"), vec!["textures"]);
    }

    #[test]
    fn backslash_paths_split_per_component() {
        assert_eq!(
            split_components(r"terrain\NA\us\ca"),
            vec!["terrain", "NA", "us", "ca"]
        );
    }

    #[test]
    fn forward_slashes_and_mixed_separators_are_equivalent() {
        assert_eq!(split_components("a/b\\c/d"), vec!["a", "b", "c", "d"]);
    }

    #[test]
    fn the_root_is_zero_components_however_spelled() {
        assert_eq!(split_components(""), Vec::<String>::new());
        assert_eq!(split_components("\\"), Vec::<String>::new());
        assert_eq!(split_components("/"), Vec::<String>::new());
        assert_eq!(split_components(r"\"), Vec::<String>::new());
    }

    #[test]
    fn a_leading_separator_names_the_same_components() {
        // The driver sometimes passes a leading separator; it must not
        // become a phantom first component.
        assert_eq!(split_components(r"\textures"), vec!["textures"]);
        assert_eq!(split_components("/textures"), vec!["textures"]);
    }

    #[test]
    fn doubled_separators_are_dropped() {
        assert_eq!(split_components(r"a\\b///c"), vec!["a", "b", "c"]);
    }

    #[test]
    fn case_and_spelling_are_preserved() {
        // The index is keyed on the exact enumerated names; no
        // normalisation may happen here.
        let path = r"Textures\NA\US\CA\SFO\10\62\62\10201062010620.0.0.dds";
        let components = split_components(path);
        assert_eq!(components[0], "Textures");
        assert_eq!(components.last().unwrap(), "10201062010620.0.0.dds");
    }

    // --- walk ---

    /// A fake union view: root → directory `a` → file `a/b`.
    /// Anything else is not found, and `b` (a file) cannot contain names —
    /// the same shape the core's `lookup` has.
    fn fake_lookup() -> impl FnMut(&FsNode, &str) -> Result<(FsNode, FileAttributes), FsError> {
        |parent, name| match (parent, name) {
            (FsNode::Directory(p), n) if p.as_os_str().is_empty() && n == "a" => {
                Ok((FsNode::Directory(PathBuf::from(n)), dummy_attrs()))
            }
            (FsNode::Directory(p), n) if p == Path::new("a") && n == "b" => {
                Ok((FsNode::RealFile(PathBuf::from("a").join(n)), dummy_attrs()))
            }
            _ => Err(FsError::NotFound),
        }
    }

    #[test]
    fn walking_zero_components_is_the_root() {
        let components: Vec<String> = Vec::new();
        let node = walk(&components, fake_lookup()).unwrap();
        assert_eq!(node, FsNode::root());
    }

    #[test]
    fn walking_a_directory_chain_ends_at_that_directory() {
        let components = split_components(r"a");
        let node = walk(&components, fake_lookup()).unwrap();
        assert_eq!(node, FsNode::Directory(PathBuf::from("a")));
    }

    #[test]
    fn walking_to_a_file_ends_at_the_file() {
        let components = split_components(r"a\b");
        let node = walk(&components, fake_lookup()).unwrap();
        assert_eq!(node, FsNode::RealFile(PathBuf::from("a/b")));
    }

    #[test]
    fn an_unknown_name_reports_not_found() {
        let components = split_components(r"missing");
        let err = walk(&components, fake_lookup()).unwrap_err();
        assert!(matches!(err, FsError::NotFound));
    }

    #[test]
    fn a_missing_intermediate_stops_the_whole_walk() {
        // The walk must not fall back to the last component or skip ahead.
        let components = split_components(r"missing\b");
        let err = walk(&components, fake_lookup()).unwrap_err();
        assert!(matches!(err, FsError::NotFound));
    }

    #[test]
    fn a_file_is_not_a_valid_parent() {
        // `b` exists only inside `a`; asking for it at the root is not
        // found, mirroring the core's NotADirectory/NotFound behaviour.
        let components = split_components(r"b");
        let err = walk(&components, fake_lookup()).unwrap_err();
        assert!(matches!(err, FsError::NotFound));
    }
}
