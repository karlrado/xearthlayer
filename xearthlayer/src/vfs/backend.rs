//! Neutral mount seam.
//!
//! [`MountBackend`] and [`MountSession`] define how callers mount the union
//! filesystem and manage the resulting mount without naming a concrete
//! platform backend. Platform adapters implement [`MountBackend`]; callers
//! (such as the mount manager) only ever see these traits, [`MountRequest`],
//! and [`MountError`].
//!
//! [`select_backend`] is the single place in the crate that names a concrete
//! backend: it returns the adapter for the OS this binary runs on. Everything
//! else mounts through the traits alone.

use std::future::Future;
use std::path::{Path, PathBuf};
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

/// Parameters for a single mount request, handed to [`MountBackend::mount`].
///
/// A request names the directory to mount at and carries optional tuning that
/// a backend may or may not understand. The FUSE background-request limits are
/// meaningful to the Linux/macOS FUSE backends and are ignored by backends
/// that manage their own request queue (for example WinFSP), so the seam stays
/// neutral while each adapter applies what it supports.
#[derive(Debug, Clone)]
pub struct MountRequest {
    /// The directory the union filesystem is mounted at.
    pub mountpoint: PathBuf,
    /// Maximum in-flight background requests, for backends that expose one.
    pub max_background: Option<u16>,
    /// Background-request congestion threshold, for backends that expose one.
    pub congestion_threshold: Option<u16>,
}

impl MountRequest {
    /// A request that mounts at `mountpoint`, leaving backend tuning to the
    /// backend's own defaults.
    pub fn new(mountpoint: impl Into<PathBuf>) -> Self {
        Self {
            mountpoint: mountpoint.into(),
            max_background: None,
            congestion_threshold: None,
        }
    }

    /// Set the FUSE background-request limits, used by the Linux/macOS FUSE
    /// backends and ignored by backends that manage their own request queue.
    pub fn with_fuse_limits(mut self, max_background: u16, congestion_threshold: u16) -> Self {
        self.max_background = Some(max_background);
        self.congestion_threshold = Some(congestion_threshold);
        self
    }
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
    /// Mount `core` according to `req`, returning a handle to the live mount.
    fn mount(&self, core: OrthoUnionCore, req: MountRequest) -> MountFuture<'_>;
}

/// The mount backend for the OS this binary runs on.
///
/// This is the one place in the crate that names a concrete backend. Callers
/// ask for a backend here and then work only with [`MountBackend`] and
/// [`MountSession`].
pub fn select_backend() -> Box<dyn MountBackend> {
    #[cfg(target_os = "macos")]
    {
        Box::new(crate::vfs::fuse3::MacFuse)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Box::new(crate::vfs::fuse3::LinuxFuse)
    }
}
