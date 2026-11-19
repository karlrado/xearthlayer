//! The fuse3 mount backends (`docs/dev/filesystem-backend-split.md`
//! §MountBackend).
//!
//! A backend is a unit struct: all its state is the mountpoint it was
//! handed. `LinuxFuse` and `MacFuse` carry identical bodies here because
//! every decision they would differ on — which session mount call, which
//! unmount helper, which volume-name option — already lives in
//! [`fuse_platform`](super::fuse_platform), the module that owns the
//! crate's platform `cfg` attributes. [`select_backend`](crate::vfs::backend::select_backend)
//! is the only site that picks between the two types, and the session
//! machinery below is the machinery [`SpawnedMountHandle`] already proved
//! in production — it is wrapped, not rewritten.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use super::fuse_platform;
use super::ortho_union_fs::Fuse3OrthoUnionFS;
use super::types::SpawnedMountHandle;
use crate::vfs::backend::{MountBackend, MountError, MountFuture, MountRequest, MountSession};
use crate::vfs::common::UnionFilesystem;

/// The Linux fuse3 backend: `mount_with_unprivileged`, which goes through
/// `fusermount3` (setuid) rather than the privileged `mount` syscall
/// (#F13: in the pinned fork this is the real API difference, not naming).
#[derive(Debug)]
pub struct LinuxFuse;

/// The macOS fuse3 backend: the plain `Session::mount` call, which goes
/// through macFUSE's own privileged helper — what every XEL macOS build
/// has done since macFUSE support landed.
#[derive(Debug)]
pub struct MacFuse;

/// The live fuse3 mount behind the seam: the event-loop task and its
/// signalling channel, held as a [`SpawnedMountHandle`].
///
/// Drop unmounts: the inner handle's `Drop` runs the proven graceful-then-
/// forced teardown. `MountSession::unmount` consumes the box and takes the
/// task and channel out of the inner handle, so the drop that follows an
/// explicit unmount is inert — no double teardown.
struct Fuse3MountSession {
    inner: SpawnedMountHandle,
    mountpoint: PathBuf,
}

impl MountSession for Fuse3MountSession {
    fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }

    fn unmount(self: Box<Self>) -> Pin<Box<dyn Future<Output = Result<(), MountError>> + Send>> {
        Box::pin(async move {
            let Fuse3MountSession { inner, .. } = *self;
            inner.unmount().await.map_err(MountError::Unmount)
        })
    }

    fn is_mounted(&self) -> bool {
        crate::system::is_path_mounted(&self.mountpoint)
    }
}

/// Mount through the platform's session call and box the live mount.
///
/// Shared by both fuse3 backends: the adapter is built here because the
/// seam hands over an `Arc<dyn UnionFilesystem>` and only this module may
/// give it a kernel shape.
async fn do_mount(
    fs: Arc<dyn UnionFilesystem>,
    request: MountRequest,
) -> Result<Box<dyn MountSession>, MountError> {
    let adapter = Fuse3OrthoUnionFS::new(fs).with_fuse_limits(
        request.fuse_limits.max_background,
        request.fuse_limits.congestion_threshold,
    );
    let options = super::ortho_union_fs::mount_options(&request.mountpoint);

    let handle = fuse_platform::mount(options, adapter, &request.mountpoint)
        .await
        .map_err(MountError::Mount)?;

    Ok(Box::new(Fuse3MountSession {
        mountpoint: request.mountpoint.clone(),
        inner: SpawnedMountHandle::spawn_from_handle(handle, request.mountpoint),
    }))
}

/// Run a teardown helper that must not panic: the panic hook is the caller,
/// and a panic there aborts with the mount still up (see the no-panic
/// contract on [`MountBackend`]).
fn no_panic<T: Default>(f: impl FnOnce() -> T + std::panic::UnwindSafe) -> T {
    std::panic::catch_unwind(f).unwrap_or_default()
}

impl MountBackend for LinuxFuse {
    fn mount(&self, fs: Arc<dyn UnionFilesystem>, request: MountRequest) -> MountFuture<'_> {
        Box::pin(do_mount(fs, request))
    }

    fn recover_stale(&self, mountpoint: &Path) -> bool {
        no_panic(|| crate::system::recover_stale_fuse_mount(mountpoint))
    }

    fn force_unmount(&self, path: &Path, force: bool) -> bool {
        no_panic(|| crate::system::unmount_fuse(path, force))
    }
}

impl MountBackend for MacFuse {
    fn mount(&self, fs: Arc<dyn UnionFilesystem>, request: MountRequest) -> MountFuture<'_> {
        Box::pin(do_mount(fs, request))
    }

    fn recover_stale(&self, mountpoint: &Path) -> bool {
        no_panic(|| crate::system::recover_stale_fuse_mount(mountpoint))
    }

    fn force_unmount(&self, path: &Path, force: bool) -> bool {
        no_panic(|| crate::system::unmount_fuse(path, force))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::backend::FuseLimits;

    /// The panic-hook helpers on a path that is nothing at all. The contract
    /// under test is "answers, never panics" — the process is already dying
    /// when the hook calls these, and the bool only selects the log line.
    #[test]
    fn teardown_helpers_answer_on_absent_paths_without_panicking() {
        let absent = Path::new("/nonexistent-xearthlayer-mountpoint");
        for backend in [&LinuxFuse as &dyn MountBackend, &MacFuse] {
            assert!(
                !backend.recover_stale(absent),
                "an absent path has nothing stale to recover"
            );
            let _ = backend.force_unmount(absent, false);
            let _ = backend.force_unmount(absent, true);
        }
    }

    /// `select_backend` returns a usable backend on every host — and it is
    /// `cfg!`, so this same test exercises the other platform's unit struct
    /// too: both types compile and answer on both OSes.
    #[test]
    fn select_backend_answers_on_the_host() {
        let backend = crate::vfs::backend::select_backend();
        assert!(
            !backend.recover_stale(Path::new("/nonexistent-xearthlayer-mountpoint")),
            "the selected backend must answer the helper contract"
        );
    }

    /// The seam's per-backend drop-unmount contract on a real kernel mount
    /// (docs/dev/filesystem-backend-split.md): building the session is
    /// ignored work without FUSE, so these run under `make live-fuse`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires /dev/fuse + fusermount3 (Linux) or macFUSE (macOS)"]
    async fn linux_fuse_session_unmounts_on_drop() {
        drop_unmount_probe(&LinuxFuse).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "requires /dev/fuse + fusermount3 (Linux) or macFUSE (macOS)"]
    async fn mac_fuse_session_unmounts_on_drop() {
        drop_unmount_probe(&MacFuse).await;
    }

    async fn drop_unmount_probe(backend: &dyn MountBackend) {
        use crate::ortho_union::OrthoUnionIndex;
        use crate::vfs::common::conformance::FixtureDdsClient;
        use crate::vfs::common::OrthoUnionCore;

        let temp = tempfile::TempDir::new().unwrap();
        let mountpoint = temp.path().join("mnt");
        std::fs::create_dir(&mountpoint).unwrap();

        let fs: Arc<dyn UnionFilesystem> = Arc::new(OrthoUnionCore::new(
            OrthoUnionIndex::default(),
            Arc::new(FixtureDdsClient::new()),
            1024,
        ));
        let request = MountRequest {
            mountpoint: mountpoint.clone(),
            fuse_limits: FuseLimits {
                max_background: 12,
                congestion_threshold: 9,
            },
        };

        let session = backend
            .mount(fs, request)
            .await
            .expect("live mount over a fresh tempdir");
        assert!(session.is_mounted(), "mounted until the box is dropped");

        drop(session);

        // Drop escalates graceful-then-forced synchronously; give the kernel
        // a beat, then check the mount table — the same verification
        // unmount_sync makes before it escalates.
        for _ in 0..100 {
            if !crate::system::is_path_mounted(&mountpoint) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            !crate::system::is_path_mounted(&mountpoint),
            "dropping the session must leave nothing mounted"
        );
    }
}
