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
//! machinery below is the machinery `SpawnedMountHandle` proved in
//! production — ported here whole, and retired there, now that the seam is
//! the only mount path.

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use super::fuse_platform;
use super::ortho_union_fs::Fuse3OrthoUnionFS;
use crate::vfs::backend::{MountBackend, MountError, MountFuture, MountRequest, MountSession};
use crate::vfs::common::UnionFilesystem;
use tracing::{debug, warn};

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
/// signalling channel — the machinery `SpawnedMountHandle` proved in
/// production, carried here now that the seam is the only mount path.
///
/// Drop unmounts: `Drop` runs the proven synchronous ladder (signal, poll
/// the task, verify the mount table, escalate graceful-then-forced).
/// `MountSession::unmount` consumes the box and takes the task and channel
/// OUT, so the drop that follows an explicit unmount is inert — no double
/// teardown.
struct Fuse3MountSession {
    /// The spawned FUSE event-loop task; `None` once consumed.
    task: Option<JoinHandle<io::Result<()>>>,
    /// Channel to signal unmount; `None` once sent.
    unmount_tx: Option<oneshot::Sender<()>>,
    /// Mountpoint for the fallback unmount via the platform helper.
    mountpoint: PathBuf,
}

impl Fuse3MountSession {
    fn spawn(handle: fuse3::raw::MountHandle, mountpoint: PathBuf) -> Self {
        let (unmount_tx, unmount_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(Self::mount_task(handle, unmount_rx));
        Self {
            task: Some(task),
            unmount_tx: Some(unmount_tx),
            mountpoint,
        }
    }

    /// The async task that runs the FUSE event loop and handles unmount.
    ///
    /// Uses `poll_fn` to poll the handle without taking ownership, so the
    /// unmount arm can `drop(handle)` — triggering fuse3's internal unmount
    /// (MountHandle::drop -> spawn inner_unmount), which drains pending
    /// kernel operations (including `release()` for open file handles).
    /// Generic over the handle future so tests can drive it with a mock.
    async fn mount_task<F>(handle: F, unmount_rx: oneshot::Receiver<()>) -> io::Result<()>
    where
        F: Future<Output = io::Result<()>> + Unpin,
    {
        let mut handle = handle;
        tokio::select! {
            result = std::future::poll_fn(|cx| Pin::new(&mut handle).poll(cx)) => result,
            _ = unmount_rx => {
                // Unmount signal received — the handle is still owned because
                // poll_fn only borrowed it. Drop it to trigger fuse3's
                // internal unmount.
                drop(handle);
                Ok(())
            },
        }
    }

    /// Synchronous teardown for `Drop`: signal the task, give it 5s to
    /// finish (fuse3's inner unmount is its own spawned task), then VERIFY
    /// the mount table — task completion does not prove the kernel released
    /// the mount (routinely wedged on macFUSE) — and escalate
    /// graceful-then-forced. Ported verbatim with the handle it came from.
    fn unmount_sync(&mut self) {
        let mountpoint = &self.mountpoint;
        let mountpoint_str = mountpoint.to_string_lossy().to_string();

        if let Some(tx) = self.unmount_tx.take() {
            debug!(mountpoint = %mountpoint_str, "Sending unmount signal");
            let _ = tx.send(());
        }

        if let Some(task) = self.task.take() {
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut finished = false;
            while Instant::now() < deadline {
                if task.is_finished() {
                    debug!(mountpoint = %mountpoint_str, "Mount task completed");
                    finished = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            if !finished {
                warn!(
                    mountpoint = %mountpoint_str,
                    "Mount task did not complete within 5s, aborting"
                );
                task.abort();
            }
        }

        if !super::unmount::is_path_mounted(mountpoint) {
            debug!(mountpoint = %mountpoint_str, "Unmounted cleanly");
            return;
        }

        debug!(mountpoint = %mountpoint_str, "Still mounted, forcing unmount");
        if super::unmount::unmount_fuse(mountpoint, false) {
            debug!(mountpoint = %mountpoint_str, "Graceful unmount succeeded");
            return;
        }

        std::thread::sleep(Duration::from_millis(500));
        if !super::unmount::is_path_mounted(mountpoint) {
            return;
        }
        warn!(
            mountpoint = %mountpoint_str,
            "Graceful unmount failed (likely busy), escalating to lazy/forced unmount"
        );
        if super::unmount::unmount_fuse(mountpoint, true) {
            debug!(mountpoint = %mountpoint_str, "Lazy unmount succeeded");
        } else {
            warn!(
                mountpoint = %mountpoint_str,
                "Lazy unmount also failed - mount may require manual cleanup"
            );
        }
    }
}

impl Drop for Fuse3MountSession {
    fn drop(&mut self) {
        // Dropped without explicit unmount: run the synchronous teardown.
        // After a consumed `unmount` both Options are None, making this inert.
        if self.task.is_some() {
            self.unmount_sync();
        }
    }
}

impl MountSession for Fuse3MountSession {
    fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }

    fn unmount(self: Box<Self>) -> Pin<Box<dyn Future<Output = Result<(), MountError>> + Send>> {
        Box::pin(async move {
            // Fuse3MountSession implements Drop, so its fields are taken
            // out through the box rather than destructured; the explicit
            // drop that follows is inert (the Drop ladder only runs when a
            // task is still in hand).
            let mut this = self;
            let task = this.task.take();
            let unmount_tx = this.unmount_tx.take();
            let mountpoint = this.mountpoint.clone();
            drop(this);
            if let Some(tx) = unmount_tx {
                let _ = tx.send(());
            }
            if let Some(t) = task {
                match t.await {
                    Ok(result) => result.map_err(MountError::Unmount)?,
                    Err(e) => {
                        return Err(MountError::Unmount(io::Error::other(format!(
                            "Mount task panicked: {e}"
                        ))))
                    }
                }
            }
            // The mount task finishing does NOT prove the kernel released
            // the mount: fuse3's MountHandle::drop spawned the actual detach
            // as its own task. Confirm it, escalating like the Drop path
            // does — otherwise is_mounted() could honestly answer `true`
            // after this future resolved Ok (live_mount found this race).
            confirm_unmounted(&mountpoint).await
        })
    }

    fn is_mounted(&self) -> bool {
        super::unmount::is_path_mounted(&self.mountpoint)
    }
}

/// Wait for the kernel to drop the mount, escalating graceful-then-forced
/// past a deadline — the same ladder `unmount_sync` walks from a
/// synchronous Drop.
async fn confirm_unmounted(mountpoint: &Path) -> Result<(), MountError> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if !super::unmount::is_path_mounted(mountpoint) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    warn!(mountpoint = %mountpoint.display(), "Mount still present 5s after unmount signal, forcing");
    if super::unmount::unmount_fuse(mountpoint, false) {
        return Ok(());
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    if !super::unmount::is_path_mounted(mountpoint) {
        return Ok(());
    }
    if super::unmount::unmount_fuse(mountpoint, true) {
        return Ok(());
    }
    Err(MountError::Unmount(io::Error::other(
        "mount still present after forced unmount",
    )))
}

/// Mount through the platform's session call and box the live mount.
///
/// Shared by both fuse3 backends: the adapter is built here because the
/// seam hands over an `Arc<dyn UnionFilesystem>` and only this module may
/// give it a kernel shape.
/// Mount options for every fuse3 session this backend opens: read-only,
/// kernel readdirplus off, no opendir support, and the mountpoint-folder
/// volume name where the platform wants one (the #305 Finder naming).
fn mount_options(mount_path: &Path) -> fuse3::MountOptions {
    let mut mount_options = fuse3::MountOptions::default();
    mount_options.read_only(true);
    mount_options.force_readdir_plus(false);
    // Tell kernel we don't implement opendir - it should call readdir directly
    mount_options.no_open_dir_support(true);
    if let Some(volname) = fuse_platform::volume_name_option(mount_path) {
        mount_options.custom_options(volname);
    }
    mount_options
}

async fn do_mount(
    fs: Arc<dyn UnionFilesystem>,
    request: MountRequest,
) -> Result<Box<dyn MountSession>, MountError> {
    let adapter = Fuse3OrthoUnionFS::new(fs).with_fuse_limits(
        request.fuse_limits.max_background,
        request.fuse_limits.congestion_threshold,
    );
    let options = mount_options(&request.mountpoint);

    let handle = fuse_platform::mount(options, adapter, &request.mountpoint)
        .await
        .map_err(MountError::Mount)?;

    Ok(Box::new(Fuse3MountSession::spawn(
        handle,
        request.mountpoint,
    )))
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
        no_panic(|| super::unmount::recover_stale_fuse_mount(mountpoint))
    }

    fn force_unmount(&self, path: &Path, force: bool) -> bool {
        no_panic(|| super::unmount::unmount_fuse(path, force))
    }
}

impl MountBackend for MacFuse {
    fn mount(&self, fs: Arc<dyn UnionFilesystem>, request: MountRequest) -> MountFuture<'_> {
        Box::pin(do_mount(fs, request))
    }

    fn recover_stale(&self, mountpoint: &Path) -> bool {
        no_panic(|| super::unmount::recover_stale_fuse_mount(mountpoint))
    }

    fn force_unmount(&self, path: &Path, force: bool) -> bool {
        no_panic(|| super::unmount::unmount_fuse(path, force))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::backend::FuseLimits;

    // ====================================================================
    // Session machinery (ported with SpawnedMountHandle's tests)
    // ====================================================================

    #[tokio::test]
    async fn session_holds_task_and_channel() {
        let (tx, _rx) = oneshot::channel();
        let task = tokio::spawn(async { Ok(()) });
        let mountpoint = PathBuf::from("/test/mount");

        let session = Fuse3MountSession {
            task: Some(task),
            unmount_tx: Some(tx),
            mountpoint: mountpoint.clone(),
        };

        assert!(session.task.is_some());
        assert!(session.unmount_tx.is_some());
        assert_eq!(session.mountpoint, mountpoint);
        // Not a live mount: drain Drop's ladder by taking the task out, so
        // no fusermount runs against a path that was never mounted.
        drop({
            let mut s = session;
            s.task.take();
            s.unmount_tx.take();
            s
        });
    }

    #[tokio::test]
    async fn session_unmount_signals_and_awaits_the_task() {
        let (tx, rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = rx.await;
            Ok(())
        });
        let session = Box::new(Fuse3MountSession {
            task: Some(task),
            unmount_tx: Some(tx),
            mountpoint: PathBuf::from("/test/mount"),
        });

        // /test/mount is not in the mount table, so confirm_unmounted
        // answers immediately: the future must not wait out the 5s ladder.
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), session.unmount())
            .await
            .expect("unmount completes promptly")
            .expect("unmount succeeds");
        let _ = result;
    }

    #[tokio::test]
    async fn mount_task_drops_handle_on_unmount_signal() {
        let (_stop_tx, stop_rx) = oneshot::channel::<()>();
        let mock_handle = async move {
            let _ = stop_rx.await;
            Ok::<(), io::Error>(())
        };

        let (unmount_tx, unmount_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(Fuse3MountSession::mount_task(
            Box::pin(mock_handle),
            unmount_rx,
        ));
        let session = Box::new(Fuse3MountSession {
            task: Some(task),
            unmount_tx: Some(unmount_tx),
            mountpoint: PathBuf::from("/test/spawn_from_handle"),
        });

        let result = tokio::time::timeout(std::time::Duration::from_secs(2), session.unmount())
            .await
            .expect("unmount completes within timeout");
        assert!(result.is_ok(), "signalled unmount succeeds");
    }

    #[tokio::test]
    async fn mount_task_exits_naturally_when_handle_completes() {
        let mock_handle = async { Ok::<(), io::Error>(()) };

        let (unmount_tx, unmount_rx) = oneshot::channel::<()>();
        let task = tokio::spawn(Fuse3MountSession::mount_task(
            Box::pin(mock_handle),
            unmount_rx,
        ));
        let mut session = Fuse3MountSession {
            task: Some(task),
            unmount_tx: Some(unmount_tx),
            mountpoint: PathBuf::from("/test/natural_exit"),
        };

        // The task completes on its own (simulates an external unmount).
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let task = session.task.take().expect("task present");
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), task).await;
        assert!(result.is_ok(), "task should have completed naturally");
        // Neutralize Drop: no task in hand means no teardown ladder.
        session.unmount_tx.take();
    }

    #[tokio::test]
    async fn unmount_is_ok_when_task_was_already_consumed() {
        let (tx, _rx) = oneshot::channel();
        let mut session = Box::new(Fuse3MountSession {
            task: Some(tokio::spawn(async { Ok(()) })),
            unmount_tx: Some(tx),
            mountpoint: PathBuf::from("/test/mount"),
        });
        // Simulate a consumed teardown: nothing left for unmount to await.
        session.task.take();
        session.unmount_tx.take();

        assert!(session.unmount().await.is_ok(), "empty unmount succeeds");
    }

    // ====================================================================
    // Backend helper contract
    // ====================================================================

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
            if !crate::vfs::fuse3::is_path_mounted(&mountpoint) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            !crate::vfs::fuse3::is_path_mounted(&mountpoint),
            "dropping the session must leave nothing mounted"
        );
    }
}
