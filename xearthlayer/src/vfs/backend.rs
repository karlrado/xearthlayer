//! The mount seam: how a union filesystem becomes a live mount.
//!
//! [`MountBackend`] is how the crate mounts: a caller builds the neutral
//! [`UnionFilesystem`](crate::vfs::common::UnionFilesystem), asks
//! [`select_backend`] for the backend this binary runs on, and gets back a
//! [`MountSession`] — a live mount it can query, unmount, or simply drop.
//! Nothing between the manager and this seam names a kernel; the concrete
//! backends (`LinuxFuse`/`MacFuse` today, WinFSP/FSKit later) live in the
//! backend modules and are reachable only through `select_backend`, the
//! crate's one platform choice (see `docs/dev/filesystem-backend-split.md`
//! §MountBackend).

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use crate::vfs::common::UnionFilesystem;

/// A live mount of the union filesystem.
///
/// Callers do not need to know which backend produced the session; they only
/// need the mount point, an unmount operation, and a liveness check.
///
/// When a `MountSession` is dropped, the filesystem must be unmounted by the
/// backend — drop is the guaranteed teardown path (the manager's `Drop` and
/// the shutdown unwinding both rely on it), so every backend has a test that
/// a dropped session leaves nothing mounted.
pub trait MountSession: Send {
    /// The directory the filesystem is mounted at.
    fn mountpoint(&self) -> &Path;

    /// Unmount the filesystem, consuming the session.
    fn unmount(self: Box<Self>) -> Pin<Box<dyn Future<Output = Result<(), MountError>> + Send>>;

    /// Whether the mount point currently reports a live mount.
    fn is_mounted(&self) -> bool;
}

/// The future returned by [`MountBackend::mount`]: resolves to a live mount
/// session or a [`MountError`].
pub type MountFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Box<dyn MountSession>, MountError>> + Send + 'a>>;

/// Failure to establish or tear down a live mount.
#[derive(Debug, thiserror::Error)]
pub enum MountError {
    /// The backend could not mount (e.g. `mount_with_unprivileged` failed).
    #[error("mount failed: {0}")]
    Mount(io::Error),
    /// The backend could not unmount.
    #[error("unmount failed: {0}")]
    Unmount(io::Error),
}

/// A platform backend that can mount the union filesystem.
pub trait MountBackend: Send + Sync {
    /// Mount `fs` according to `req`, returning a handle to the live mount.
    fn mount(&self, fs: Arc<dyn UnionFilesystem>, req: MountRequest) -> MountFuture<'_>;

    /// Whether a stale mount at `mountpoint` was recovered. Synchronous,
    /// needs no runtime, and must not panic (see the panic-hook contract
    /// below).
    fn recover_stale(&self, mountpoint: &Path) -> bool;

    /// Force-detach the mount at `path`. Returns `true` if the mountpoint
    /// ended up unmounted (including the benign already-unmounted case).
    /// Synchronous, needs no runtime, and must not panic.
    ///
    /// The crash path is the panic hook, not the mount manager: it calls
    /// `force_unmount` graceful first, then forced, and the `bool` outcome
    /// is what lets it log which attempt worked. A panic inside a panic
    /// hook aborts the process with the mount still up, so these two
    /// operations are the one place in a backend where the no-panic
    /// guarantee matters; the fuse3 backend honours it with `catch_unwind`.
    fn force_unmount(&self, path: &Path, force: bool) -> bool;
}

/// What a [`MountBackend::mount`] call establishes: where, and with which
/// kernel limits.
pub struct MountRequest {
    /// The directory the union filesystem is mounted at.
    pub mountpoint: PathBuf,
    /// Kernel background-request limits (fuse3 `max_background` /
    /// `congestion_threshold`); backends that have no equivalent ignore
    /// them.
    pub fuse_limits: FuseLimits,
}

/// FUSE kernel limits, from the `[fuse]` config section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FuseLimits {
    pub max_background: u16,
    pub congestion_threshold: u16,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_error_carries_the_io_error() {
        let err = MountError::Mount(io::Error::from_raw_os_error(libc::EPERM));
        assert!(err.to_string().contains("mount failed"));
        let err = MountError::Unmount(io::Error::from_raw_os_error(libc::EBUSY));
        assert!(err.to_string().contains("unmount failed"));
    }

    #[test]
    fn mount_request_carries_mountpoint_and_limits() {
        let req = MountRequest {
            mountpoint: PathBuf::from("/tmp/union"),
            fuse_limits: FuseLimits {
                max_background: 64,
                congestion_threshold: 48,
            },
        };
        assert_eq!(req.mountpoint, PathBuf::from("/tmp/union"));
        assert_eq!(req.fuse_limits.max_background, 64);
        assert_eq!(req.fuse_limits.congestion_threshold, 48);
    }

    /// The seam's own shape, the same proof `UnionFilesystem` carries: the
    /// manager holds `Box<dyn MountSession>` and `Box<dyn MountBackend>`, so
    /// both must be object-safe.
    #[test]
    fn seam_is_object_safe() {
        fn _held(_: Box<dyn MountSession>, _b: &dyn MountBackend) {}
    }
}
