//! Neutral mount seam.
//!
//! [`MountBackend`] and [`MountSession`] define how callers mount the union
//! filesystem and manage the resulting mount without naming a concrete
//! platform backend. Platform adapters implement [`MountBackend`]; callers
//! (such as the mount manager) only ever see these traits and [`MountError`].

use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use thiserror::Error;

use crate::vfs::support::core::OrthoUnionCore;

/// An error that can occur while mounting or unmounting the union filesystem.
#[derive(Debug, Error)]
pub enum MountError {
    /// The mount (or unmount) failed with a backend-specific message.
    #[error("mount failed: {0}")]
    MountFailed(String),
    /// A platform I/O error surfaced by the backend.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// A live mount of the union filesystem.
///
/// Callers do not need to know which backend produced the session; they only
/// need the mount point, an unmount operation, and a liveness check.
pub trait MountSession: Send {
    /// The directory the filesystem is mounted at.
    fn mountpoint(&self) -> &Path;

    /// Unmount the filesystem, consuming the session.
    fn unmount(self: Box<Self>) -> Pin<Box<dyn Future<Output = std::io::Result<()>> + Send>>;

    /// Whether the mount point currently reports a live mount.
    fn is_mounted(&self) -> bool;
}

/// The future returned by [`MountBackend::mount`]: resolves to a live mount
/// session or a [`MountError`].
pub type MountFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Box<dyn MountSession>, MountError>> + Send + 'a>>;

/// A platform backend that can mount the union filesystem.
pub trait MountBackend: Send + Sync {
    /// Mount `core` at `at`, returning a handle to the live mount.
    fn mount(&self, core: OrthoUnionCore, at: &Path) -> MountFuture<'_>;
}
