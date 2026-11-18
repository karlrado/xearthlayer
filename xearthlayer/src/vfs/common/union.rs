//! The backend-neutral contract shared by every mount backend.
//!
//! [`UnionFilesystem`] is the seam a backend (fuse3 today, WinFSP or FSKit
//! later) mounts the union view through. It names no kernel: identity is a
//! [`FsNode`], attributes are [`FileAttributes`], failure is an
//! [`FsError`]. The concrete state and logic live in the `OrthoUnionCore`
//! (`super::core`, added by the next step of the split), which implements
//! this trait; a backend holds the core plus its own kernel identity (inodes on
//! FUSE, file contexts on WinFSP) and translates every kernel request onto
//! these operations.
//!
//! The async operations return hand-boxed futures
//! (`Pin<Box<dyn Future + Send + '_>>`) rather than `async fn` so the trait
//! stays object-safe: backends hold an `Arc<dyn UnionFilesystem>` and the
//! mount seam passes one around. No `async-trait` dependency (see
//! `docs/dev/filesystem-backend-split.md` §Union Filesystem).
//! `Send + Sync` so a core can be shared across a backend's thread pool.
//! The handle lifecycle (`open`/`release`) and `statfs` are synchronous
//! because they touch only in-memory state.

use std::future::Future;
use std::pin::Pin;

use bytes::Bytes;

use super::attrs::{DirEntry, Entry, FileAttributes, StatFsInfo};
use super::error::FsError;
use super::node::FsNode;

/// The boxed future returned by every async [`UnionFilesystem`] operation.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A union filesystem a backend can mount.
///
/// Every backend drives the *same* implementation through this trait, so
/// behaviour is defined once and each backend is only responsible for
/// translating these neutral calls onto its kernel. See
/// `docs/dev/filesystem-backend-split.md` §Union Filesystem for the
/// contract and §MountBackend for how a backend receives one.
pub trait UnionFilesystem: Send + Sync {
    /// Resolve `name` inside the directory `node`.
    ///
    /// Returns the child's node and attributes together: the kernel's
    /// lookup-then-getattr pattern is served in one resolution, and the
    /// attributes must agree with a later [`attrs`](Self::attrs) call for
    /// the same node.
    ///
    /// The borrowed node and name must outlive the returned future: every
    /// argument and `&self` share one lifetime `'a`, which the boxed future
    /// borrows.
    fn lookup<'a>(
        &'a self,
        node: &'a FsNode,
        name: &'a str,
    ) -> BoxFuture<'a, Result<Entry, FsError>>;

    /// The attributes of `node` in the union view.
    fn attrs<'a>(&'a self, node: &'a FsNode) -> BoxFuture<'a, Result<FileAttributes, FsError>>;

    /// Read up to `size` bytes of `node` starting at `offset`.
    ///
    /// A ranged read: short at the end of the file, empty at or past the
    /// end. `fh` is the per-open handle from [`open`](Self::open); a `0`
    /// handle means the read resolves the tile per call (see
    /// [`open`](Self::open)).
    fn read<'a>(
        &'a self,
        node: &'a FsNode,
        fh: u64,
        offset: u64,
        size: usize,
    ) -> BoxFuture<'a, Result<Bytes, FsError>>;

    /// List the entries of the directory `node`.
    ///
    /// The core lists only real children: the `.` and `..` entries, the
    /// listing offset and entry ordering are the backend's kernel-facing
    /// business.
    fn list_directory<'a>(
        &'a self,
        node: &'a FsNode,
    ) -> BoxFuture<'a, Result<Vec<DirEntry>, FsError>>;

    /// Open `node` for reading, returning a per-open file handle.
    ///
    /// A handle of `0` means no handle state was allocated. `0` is a valid
    /// return for four cases:
    ///
    /// 1. real files (no handle state),
    /// 2. lit textures (served by range, pin nothing),
    /// 3. unknown virtual inodes (the gap surfaces as
    ///    [`FsError::NotFound`] in [`read`](Self::read), not a failed open),
    /// 4. the exhausted pinned-tile budget.
    ///
    /// A `0` handle means [`read`](Self::read) resolves the tile per call.
    fn open(&self, node: &FsNode) -> Result<u64, FsError>;

    /// Release the per-open state of a handle returned by
    /// [`open`](Self::open).
    ///
    /// `release(0)` is a no-op: there is no handle to drop. A backend must
    /// not treat `0` as a real handle or error on `release(0)`.
    fn release(&self, fh: u64) -> Result<(), FsError>;

    /// Volume statistics for the union view.
    fn statfs(&self) -> StatFsInfo;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::common::attrs::NodeKind;
    use crate::vfs::TextureKind;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;
    use std::time::UNIX_EPOCH;

    /// A minimal [`UnionFilesystem`] standing in for `OrthoUnionCore` until
    /// the core is extracted: enough behaviour to prove the trait is
    /// implementable, object-safe and drivable as `Arc<dyn …>`.
    struct MockUnion {
        next_fh: AtomicU64,
    }

    impl MockUnion {
        fn new() -> Self {
            Self {
                next_fh: AtomicU64::new(1),
            }
        }

        fn test_attrs() -> FileAttributes {
            FileAttributes {
                size: 4,
                blocks: 1,
                atime: UNIX_EPOCH,
                mtime: UNIX_EPOCH,
                ctime: UNIX_EPOCH,
                crtime: UNIX_EPOCH,
                kind: NodeKind::RegularFile,
                perm: 0o444,
                nlink: 1,
                uid: 0,
                gid: 0,
                rdev: 0,
                blksize: 4096,
            }
        }
    }

    impl UnionFilesystem for MockUnion {
        fn lookup<'a>(
            &'a self,
            node: &'a FsNode,
            name: &'a str,
        ) -> BoxFuture<'a, Result<Entry, FsError>> {
            Box::pin(async move {
                match name {
                    "tile.dsf" => Ok(Entry {
                        node: FsNode::RealFile(PathBuf::from(name)),
                        attrs: Self::test_attrs(),
                    }),
                    _ if node.is_root() => Err(FsError::NotFound),
                    _ => Err(FsError::NotADirectory),
                }
            })
        }

        fn attrs<'a>(
            &'a self,
            _node: &'a FsNode,
        ) -> BoxFuture<'a, Result<FileAttributes, FsError>> {
            Box::pin(async { Ok(Self::test_attrs()) })
        }

        fn read<'a>(
            &'a self,
            _node: &'a FsNode,
            _fh: u64,
            offset: u64,
            size: usize,
        ) -> BoxFuture<'a, Result<Bytes, FsError>> {
            Box::pin(async move {
                let all = Bytes::from_static(b"1234");
                let start = (offset as usize).min(all.len());
                let end = (start + size).min(all.len());
                Ok(all.slice(start..end))
            })
        }

        fn list_directory<'a>(
            &'a self,
            node: &'a FsNode,
        ) -> BoxFuture<'a, Result<Vec<DirEntry>, FsError>> {
            Box::pin(async move {
                if !node.is_directory() {
                    return Err(FsError::NotADirectory);
                }
                Ok(vec![DirEntry {
                    name: "tile.dsf".into(),
                    entry: Entry {
                        node: FsNode::RealFile(PathBuf::from("tile.dsf")),
                        attrs: Self::test_attrs(),
                    },
                }])
            })
        }

        fn open(&self, node: &FsNode) -> Result<u64, FsError> {
            // Real files get no handle, matching the contract's case 1.
            match node {
                FsNode::RealFile(_) => Ok(0),
                FsNode::VirtualDds(_) => Ok(self.next_fh.fetch_add(1, Ordering::Relaxed)),
                FsNode::Directory(_) => Err(FsError::NotFound),
            }
        }

        fn release(&self, _fh: u64) -> Result<(), FsError> {
            Ok(())
        }

        fn statfs(&self) -> StatFsInfo {
            StatFsInfo {
                blocks: 1_000_000,
                bfree: 0,
                bavail: 0,
                files: 1,
                ffree: 0,
                bsize: 4096,
                namelen: 255,
                frsize: 4096,
            }
        }
    }

    #[test]
    fn trait_is_object_safe() {
        // The seam's whole purpose: backends hold Arc<dyn UnionFilesystem>.
        let fs: Arc<dyn UnionFilesystem> = Arc::new(MockUnion::new());
        assert_eq!(fs.statfs().namelen, 255);
    }

    #[tokio::test]
    async fn contract_drives_through_a_trait_object() {
        let fs: Arc<dyn UnionFilesystem> = Arc::new(MockUnion::new());

        let entry = fs.lookup(&FsNode::root(), "tile.dsf").await.unwrap();
        assert_eq!(entry.node, FsNode::RealFile(PathBuf::from("tile.dsf")));

        let attrs = fs.attrs(&entry.node).await.unwrap();
        assert_eq!(attrs, entry.attrs);

        let fh = fs.open(&entry.node).unwrap();
        let data = fs.read(&entry.node, fh, 1, 2).await.unwrap();
        assert_eq!(&data[..], b"23");
        fs.release(fh).unwrap();

        let listing = fs.list_directory(&FsNode::root()).await.unwrap();
        assert_eq!(listing.len(), 1);
        assert_eq!(listing[0].entry.node, entry.node);
    }

    #[tokio::test]
    async fn errors_travel_the_contract() {
        let fs: Arc<dyn UnionFilesystem> = Arc::new(MockUnion::new());

        assert!(matches!(
            fs.lookup(&FsNode::root(), "missing.dsf").await,
            Err(FsError::NotFound)
        ));
        let file = FsNode::RealFile(PathBuf::from("tile.dsf"));
        assert!(matches!(
            fs.list_directory(&file).await,
            Err(FsError::NotADirectory)
        ));
    }

    #[tokio::test]
    async fn zero_handle_is_a_valid_open_answer() {
        // Contract case 1: a real file opens with no handle state, reads
        // carry 0, and release(0) is a no-op.
        let fs: Arc<dyn UnionFilesystem> = Arc::new(MockUnion::new());
        let file = FsNode::RealFile(PathBuf::from("tile.dsf"));

        let fh = fs.open(&file).unwrap();
        assert_eq!(fh, 0);
        let data = fs.read(&file, 0, 0, 4).await.unwrap();
        assert_eq!(&data[..], b"1234");
        fs.release(0).unwrap();
    }

    #[tokio::test]
    async fn virtual_dds_gets_real_handles() {
        // The memoising case: distinct opens allocate distinct non-zero
        // handles (OrthoUnionCore will key its DdsHandle memo on them).
        let fs: Arc<dyn UnionFilesystem> = Arc::new(MockUnion::new());
        let dds = FsNode::VirtualDds(crate::vfs::DdsFilename {
            row: 1,
            col: 2,
            zoom: 12,
            map_type: "BI".to_string(),
            kind: TextureKind::Ortho,
        });

        let fh1 = fs.open(&dds).unwrap();
        let fh2 = fs.open(&dds).unwrap();
        assert_ne!(fh1, 0);
        assert_ne!(fh1, fh2);
    }
}
