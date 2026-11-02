//! Concrete [`MountBackend`] implementations for the shared fuse3 translation.
//!
//! [`LinuxFuse`] and [`MacFuse`] mount the same [`Fuse3OrthoUnionFS`], differing
//! only in the [`FusePlatform`] policy they pass to it. The neutral seam's
//! [`select_backend`] (see `crate::vfs::backend`) is the one place that picks
//! which of the two to use for the current OS.

use std::future::Future;
use std::io;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use super::fuse_platform::{FusePlatform, Linux, Mac};
use super::ortho_union_fs::Fuse3OrthoUnionFS;
use super::types::SpawnedMountHandle;
use crate::vfs::backend::{MountBackend, MountError, MountFuture, MountRequest, MountSession};
use crate::vfs::support::core::OrthoUnionCore;

/// A live fuse3 mount: a background FUSE task that can be unmounted or probed.
struct FuseMountSession {
    handle: SpawnedMountHandle,
}

impl MountSession for FuseMountSession {
    fn mountpoint(&self) -> &Path {
        self.handle.mountpoint()
    }

    fn unmount(self: Box<Self>) -> Pin<Box<dyn Future<Output = io::Result<()>> + Send>> {
        let Self { handle } = *self;
        Box::pin(async move { handle.unmount().await })
    }

    fn is_mounted(&self) -> bool {
        crate::system::is_path_mounted(self.mountpoint())
    }
}

/// Mount `core` under this platform's FUSE policy and return the live session.
///
/// Shared by [`LinuxFuse`] and [`MacFuse`]; they differ only in the
/// `platform` they pass in. Any FUSE background-request limits in `req` are
/// applied; a backend that manages its own request queue would simply not
/// read them.
async fn mount_fuse(
    platform: Arc<dyn FusePlatform>,
    core: OrthoUnionCore,
    req: MountRequest,
) -> Result<Box<dyn MountSession>, MountError> {
    let mut fs = Fuse3OrthoUnionFS::with_core(core, platform);

    if let Some(max_background) = req.max_background {
        let congestion_threshold = req.congestion_threshold.unwrap_or(max_background);
        fs = fs.with_fuse_limits(max_background, congestion_threshold);
    }

    let mountpoint = req.mountpoint.to_string_lossy();
    let handle = fs
        .mount_spawned(&mountpoint)
        .await
        .map_err(|e| MountError::MountFailed(e.to_string()))?;

    Ok(Box::new(FuseMountSession { handle }))
}

/// The fuse3 translation on Linux.
pub struct LinuxFuse;

impl MountBackend for LinuxFuse {
    fn mount(&self, core: OrthoUnionCore, req: MountRequest) -> MountFuture<'_> {
        Box::pin(mount_fuse(Arc::new(Linux), core, req))
    }
}

/// The fuse3 translation on macOS.
pub struct MacFuse;

impl MountBackend for MacFuse {
    fn mount(&self, core: OrthoUnionCore, req: MountRequest) -> MountFuture<'_> {
        Box::pin(mount_fuse(Arc::new(Mac), core, req))
    }
}
