//! The backend-neutral contract shared by every mount backend.
//!
//! [`UnionFilesystem`] is the seam a backend (FUSE today, WinFSP in Phase 2,
//! FSKit on macOS in the future) mounts the union view through. It names no
//! kernel: identity is a [`FsNode`], attributes are [`FileAttributes`], and a
//! failure is an [`FsError`]. The concrete state and logic live in
//! [`crate::vfs::support::core::OrthoUnionCore`], which implements this
//! trait; a backend holds the core plus its own kernel identity (inodes on
//! FUSE, file contexts on WinFSP) and delegates every operation here.
//!
//! `Send + Sync` so a core can be shared across the backend's thread pool.
//! The I/O-bearing operations are `async` so a backend that runs on an async
//! runtime never blocks a worker thread on disk or on a DDS generation
//! round trip; the handle lifecycle (`open`/`release`) and `statfs` are
//! synchronous because they touch only in-memory state.

use bytes::Bytes;

use super::attrs::{DirEntry, FileAttributes, StatFsInfo};
use super::error::FsError;
use super::node::FsNode;

/// A union filesystem a backend can mount.
///
/// Every backend drives the *same* implementation through this trait, so the
/// behaviour is defined once and each backend is only responsible for
/// translating these neutral calls onto its kernel. See
/// `docs/dev/windows-support.md` §3 for the interface shape and §6.1 for the
/// core that implements it.
#[allow(async_fn_in_trait)] // internal seam; the project allows async-in-trait on such traits (see DdsRequestor)
pub trait UnionFilesystem: Send + Sync {
    /// Resolve `name` inside the directory `node`.
    ///
    /// Returns the resolved child node; callers that also need the child's
    /// attributes follow up with [`attrs`](Self::attrs), keeping this op
    /// focused on name-to-node resolution.
    async fn lookup(&self, node: &FsNode, name: &str) -> Result<FsNode, FsError>;

    /// The attributes of `node` in the union view.
    async fn attrs(&self, node: &FsNode) -> Result<FileAttributes, FsError>;

    /// Read up to `size` bytes of `node` starting at `offset`.
    ///
    /// A ranged read: short at the end of the file, empty at or past the end.
    /// `fh` is the per-open handle from [`open`](Self::open) (0 for backends
    /// that serve without per-open state).
    async fn read(
        &self,
        node: &FsNode,
        fh: u64,
        offset: u64,
        size: usize,
    ) -> Result<Bytes, FsError>;

    /// List the entries of the directory `node`.
    async fn list_directory(&self, node: &FsNode) -> Result<Vec<DirEntry>, FsError>;

    /// Open `node` for reading, returning a per-open file handle.
    ///
    /// A handle of `0` means the file needs no per-open state and reads
    /// should not carry a handle.
    fn open(&self, node: &FsNode) -> Result<u64, FsError>;

    /// Release the per-open state of a handle returned by
    /// [`open`](Self::open).
    fn release(&self, fh: u64) -> Result<(), FsError>;

    /// Volume statistics for the union view.
    fn statfs(&self) -> StatFsInfo;
}
