//! Platform-abstracted FUSE mount-table operations.
//!
//! Linux unmounts FUSE filesystems with `fusermount3`/`fusermount` and exposes
//! the mount table as `/proc/mounts`, while macOS (macFUSE) uses the BSD
//! `umount(8)` and `mount(8)`. Windows (WinFSP) has no kernel mount table at
//! all — a mount lives in its owning process — so it unmounts through the
//! owning session, and the in-process session registry below is the
//! authoritative record of our mounts (design doc §4.5). Everything that
//! needs to unmount a mount or ask "is this path mounted?" shares the same
//! platform logic, so it lives here once: the panic handler, the
//! spawned-mount Drop fallback, and the package manager's status display.

use std::path::Path;

// The unmount binaries and their stderr parsing exist only off-Windows;
// on Windows the unmount goes through the owning WinFSP session, and the
// "is something mounted here" probe is a reparse-point check on the mountpoint.
#[cfg(not(target_os = "windows"))]
use std::process::Command;
#[cfg(not(target_os = "windows"))]
use tracing::debug;
use tracing::warn;

#[cfg(target_os = "windows")]
use std::collections::HashMap;
#[cfg(target_os = "windows")]
use std::path::PathBuf;
#[cfg(target_os = "windows")]
use std::sync::{Mutex, OnceLock};

#[cfg(target_os = "windows")]
use crate::fuse::winfsp::WinFspSession;

/// Unmount the FUSE filesystem at `mountpoint`.
///
/// `force` requests the most aggressive unmount the platform offers — Linux's
/// lazy `-uz` (detach now, clean up when idle) or macOS's `-f` (force). It is
/// used as an escalation when a graceful unmount fails because the mount is
/// still busy.
///
/// Returns `true` if the mountpoint ended up unmounted, including the benign
/// case where it was already not mounted.
pub fn unmount_fuse(mountpoint: &Path, force: bool) -> bool {
    #[cfg(target_os = "windows")]
    {
        // WinFSP unmounts through the owning session — there is no kernel
        // unmount command (design doc §4.5). Take the session registered for
        // this mountpoint (which also clears the registration) and stop it.
        // `force` is advisory: WinFSP's stop is the same operation at both
        // levels. A path with no registered session is already in the end
        // state we want (nothing of ours mounted there).
        let _ = force;
        match take_winfsp_mount(mountpoint) {
            Some(session) => {
                session.unmount();
                true
            }
            None => true,
        }
    }

    #[cfg(target_os = "macos")]
    {
        // macFUSE unmounts via BSD umount(8). This helper is only reached as a
        // fallback from the panic handler and the Drop path, where the daemon
        // may be wedged and we just want the mount gone — so we always force
        // (`-f`) to detach immediately rather than risk blocking. The normal,
        // clean teardown goes through the async `MountHandle::unmount`, not
        // this path; `force` is therefore advisory on macOS.
        let _ = force;
        run_unmount(Command::new("umount").arg("-f").arg(mountpoint), mountpoint)
    }

    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        let flag = if force { "-uz" } else { "-u" };
        // Prefer fusermount3 (libfuse3), fall back to fusermount (libfuse2).
        let mut cmd = Command::new("fusermount3");
        cmd.arg(flag).arg(mountpoint);
        if run_unmount(&mut cmd, mountpoint) {
            return true;
        }
        let mut fallback = Command::new("fusermount");
        fallback.arg(flag).arg(mountpoint);
        run_unmount(&mut fallback, mountpoint)
    }
}

/// Whether `path` is currently a mountpoint, per the OS mount table.
///
/// Platform-specific because the mount table lives in different places: Linux
/// exposes it as `/proc/mounts`; macOS has no such file and must be queried via
/// `mount(8)`; Windows has no mount table at all and is answered from the
/// in-process WinFSP session registry, falling back to a reparse-point check
/// on the path (the mountpoint carries a WinFSP volume mount point). A wrong
/// answer here is not cosmetic — callers gate real
/// unmount escalation (`SpawnedMountHandle`) and the user-facing package mount
/// status on it, so a Linux-only implementation silently mis-reports every
/// macOS mount.
///
/// This answers "is the kernel mount-table entry present", which is also `true`
/// for a wedged macFUSE mount (daemon dead, entry lingering) — exactly the
/// zombie the unmount fallback must still act on. Use [`is_stale_fuse_mount`] to
/// distinguish a healthy mount from a wedged one.
pub fn is_path_mounted(path: &Path) -> bool {
    #[cfg(target_os = "windows")]
    {
        // A session registered for this path in this process is
        // authoritative (the panic handler and the manager register under
        // the same path string).
        if is_winfsp_mount_registered(path) {
            return true;
        }
        // Otherwise ask the OS: a path on a WinFSP volume is a mount, whether
        // ours in another process or a corpse of a dead one.
        is_winfsp_volume(path)
    }

    #[cfg(not(target_os = "windows"))]
    {
        let path_str = path.to_string_lossy();

        #[cfg(target_os = "macos")]
        {
            match Command::new("/sbin/mount").output() {
                Ok(output) => {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    mount_output_contains(&stdout, &path_str)
                }
                Err(_) => false,
            }
        }

        #[cfg(not(target_os = "macos"))]
        {
            match std::fs::read_to_string("/proc/mounts") {
                Ok(mounts) => proc_mounts_contains(&mounts, &path_str),
                Err(_) => false,
            }
        }
    }
}

/// Whether `/proc/mounts` content lists `path` as a mountpoint.
///
/// Pure function (no I/O) so the parsing is unit-testable. Each line is
/// `device mountpoint fstype opts ...`; we match field 2. Linux-only: the
/// mount table it parses exists only on Linux (Windows has no mount table
/// at all; the test below runs where the function exists).
#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
fn proc_mounts_contains(mounts: &str, path: &str) -> bool {
    mounts.lines().any(|line| {
        let mut fields = line.split_whitespace();
        fields.next(); // device
        fields.next() == Some(path)
    })
}

/// Whether BSD `mount(8)` output lists `path` as a mountpoint.
///
/// Pure function (no I/O) so the parsing is unit-testable. Lines look like
/// `device on /mount/point (fstype, opts...)`; the mountpoint sits between
/// " on " and the trailing " (". Splitting on the last " (" keeps mountpoints
/// that contain spaces (e.g. `X-Plane 12`) intact.
#[cfg(target_os = "macos")]
fn mount_output_contains(output: &str, path: &str) -> bool {
    output.lines().any(|line| {
        line.split_once(" on ")
            .and_then(|(_, rest)| rest.rsplit_once(" (").map(|(mp, _)| mp))
            .map(|mp| mp == path)
            .unwrap_or(false)
    })
}

/// Detect a stale FUSE mount at `mountpoint`.
///
/// A FUSE mount whose daemon died without unmounting (crash, SIGKILL,
/// system sleep) stays in the kernel mount table, but every operation on
/// it fails — on macOS/macFUSE with `ENXIO` ("Device not configured"),
/// on Linux with `ENOTCONN` ("Transport endpoint is not connected").
/// Mounting over such a corpse fails, so it must be cleared first.
///
/// A healthy mount, an ordinary directory, or a missing path are all
/// reported as not stale.
///
/// Detection uses `opendir(2)` (via `read_dir`), not `stat(2)`: on macOS
/// the kernel serves cached attributes for a dead macFUSE mountpoint, so
/// `stat` succeeds while any real operation — including reading the
/// directory — fails with `ENXIO` (verified empirically against a killed
/// mount).
pub fn is_stale_fuse_mount(mountpoint: &Path) -> bool {
    #[cfg(target_os = "windows")]
    {
        // A WinFSP volume at the path whose owning process is gone: the
        // kernel's mount-point reparse survives a dead service (spike F5),
        // but no session in this process serves it. `is_winfsp_volume`
        // reads the on-disk reparse data (`read_link`), not the mount's I/O
        // — on a corpse a file operation would hang until the pending-IRP
        // deadline (F5), while the reparse target is answered by the
        // kernel's on-disk metadata.
        is_winfsp_volume(mountpoint) && !is_winfsp_mount_registered(mountpoint)
    }

    #[cfg(not(target_os = "windows"))]
    {
        match std::fs::read_dir(mountpoint) {
            Ok(_) => false,
            Err(e) => is_stale_mount_error(&e),
        }
    }
}

/// Classify an I/O error from `stat(2)` on a mountpoint as "stale FUSE
/// mount" or not. Extracted as a pure function for testability.
#[cfg(not(target_os = "windows"))]
fn is_stale_mount_error(error: &std::io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::ENXIO) | Some(libc::ENOTCONN)
    )
}

/// Detect and clear a stale FUSE mount at `mountpoint`.
///
/// Returns `true` if a stale mount was found and successfully unmounted,
/// `false` if the mountpoint was healthy (nothing to do). Logs a warning
/// if a stale mount is found but cannot be cleared.
pub fn recover_stale_fuse_mount(mountpoint: &Path) -> bool {
    if !is_stale_fuse_mount(mountpoint) {
        return false;
    }
    #[cfg(target_os = "windows")]
    {
        // A corpse's session is dead, so there is nothing in this process to
        // unmount it with (spike F5: killing the process leaks the mount
        // until reboot). Report it and leave the directory alone — it may
        // still be referenced by zombie driver state.
        warn!(
            mountpoint = %mountpoint.display(),
            "Stale WinFSP mount from a previous (killed) run; it cannot be \
             unmounted from here. Manual cleanup: remove the directory, or \
             reboot if it is locked, then retry the mount"
        );
        false
    }

    #[cfg(not(target_os = "windows"))]
    {
        warn!(
            mountpoint = %mountpoint.display(),
            "Stale FUSE mount detected from a previous run; force-unmounting"
        );
        let cleared = unmount_fuse(mountpoint, true);
        if !cleared {
            warn!(
                mountpoint = %mountpoint.display(),
                "Could not clear stale FUSE mount; mounting will likely fail. \
                 Unmount manually with: umount <mountpoint>"
            );
        }
        cleared
    }
}

// ---------------------------------------------------------------------------
// Windows (WinFSP) mount model (design doc §4.5)
//
// WinFSP has no kernel mount table to query: a mount lives in the owning
// process's user-mode service. So the in-process session registry below is
// the authoritative record of *our* mounts — the panic handler, the unmount
// fallback, and the package status display all ask it first — and a
// reparse-point check on the path is the best-effort "is *something* WinFSP
// mounted at this path" test (another process's mount, or a corpse of a dead
// one). A WinFSP folder mount is a directory carrying a MUP mount-point
// reparse point whose target is the volume device name
// `\Device\Volume{GUID}\`; `read_link` reads exactly that (the on-disk
// reparse data, with no I/O to the — possibly dead — owning service).
// ---------------------------------------------------------------------------

/// The live WinFSP sessions this process owns, keyed by the mountpoint path
/// they were registered under (exact `PathBuf` equality — the same path
/// string the panic handler's registry uses).
#[cfg(target_os = "windows")]
static WINFSP_SESSIONS: OnceLock<Mutex<HashMap<PathBuf, WinFspSession>>> = OnceLock::new();

#[cfg(target_os = "windows")]
fn winfsp_sessions() -> &'static Mutex<HashMap<PathBuf, WinFspSession>> {
    WINFSP_SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register the session that serves `mountpoint` (call right after a
/// successful mount, with the same path string the rest of the app uses for
/// that mount). Re-registering the same path replaces the old session, whose
/// remaining handles still unmount on drop.
#[cfg(target_os = "windows")]
pub fn register_winfsp_mount(mountpoint: impl Into<PathBuf>, session: WinFspSession) {
    winfsp_sessions()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(mountpoint.into(), session);
}

/// Take the session registered for `mountpoint`, if any. A successful
/// unmount removes the entry with its session; the panic handler's
/// `unmount_fuse` takes it the same way.
#[cfg(target_os = "windows")]
fn take_winfsp_mount(mountpoint: &Path) -> Option<WinFspSession> {
    winfsp_sessions()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(mountpoint)
}

/// Whether a session is registered for `mountpoint` in this process.
#[cfg(target_os = "windows")]
fn is_winfsp_mount_registered(mountpoint: &Path) -> bool {
    winfsp_sessions()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .contains_key(mountpoint)
}

/// Whether `path` is a WinFSP (folder) mountpoint — this process's mount,
/// another process's, or a corpse of a dead one.
///
/// A WinFSP folder mount is a directory carrying a MUP mount-point reparse
/// point whose target is the volume device name (`\Device\Volume{GUID}\`).
/// `read_link` reads that on-disk reparse data — it needs no I/O to the
/// owning (possibly dead) service, which is exactly why it still answers for
/// a corpse. Non-reparse paths error, and NTFS junctions/symlinks name a
/// filesystem path (`\??\...`) rather than a volume device, so only a real
/// volume mount matches.
#[cfg(target_os = "windows")]
fn is_winfsp_volume(path: &Path) -> bool {
    std::fs::read_link(path)
        .map(|target| is_winfsp_volume_target(&target.to_string_lossy()))
        .unwrap_or(false)
}

/// Whether a reparse-point target names a mounted volume (a WinFSP volume,
/// or any other volume mounted onto a folder) rather than a filesystem path
/// (an NTFS junction or a symlink). Pure function (no I/O) so the
/// classification is unit-testable on every OS.
///
/// Its only production caller ([`is_winfsp_volume`]) is Windows-only, so on
/// other platforms this is reachable solely from the test below — silence the
/// `dead_code` lint there (it stays enforced on Windows, where it is used).
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn is_winfsp_volume_target(target: &str) -> bool {
    target.trim_start().starts_with(r"\Device\Volume{")
}

/// Run one unmount command, treating "already unmounted" as success.
#[cfg(not(target_os = "windows"))]
fn run_unmount(cmd: &mut Command, mountpoint: &Path) -> bool {
    match cmd.output() {
        Ok(output) if output.status.success() => true,
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // An already-detached mount is the outcome we want, so a "not
            // mounted"/"not currently mounted" error still counts as success.
            if stderr.contains("not mounted")
                || stderr.contains("not currently mounted")
                || stderr.contains("not found")
            {
                true
            } else {
                debug!(
                    mountpoint = %mountpoint.display(),
                    stderr = %stderr,
                    "unmount command failed"
                );
                false
            }
        }
        Err(e) => {
            warn!(
                mountpoint = %mountpoint.display(),
                error = %e,
                "failed to run unmount command"
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unmounting a path that isn't a mountpoint must be treated as success
    /// (the desired end state — nothing mounted there — already holds). This
    /// also exercises the real platform unmount binary and its stderr parsing
    /// without needing a live FUSE mount, so it is safe in CI.
    #[test]
    fn unmount_of_non_mountpoint_reports_success() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(
            unmount_fuse(dir.path(), false),
            "non-mountpoint should be reported as already unmounted"
        );
    }

    /// The stale-mount errno classification: ENXIO (macFUSE) and ENOTCONN
    /// (Linux FUSE) mean a dead mount; anything else does not. A real dead
    /// mount cannot be created in CI, so the policy is tested via the pure
    /// classifier.
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn stale_mount_error_classification() {
        let stale_macos = std::io::Error::from_raw_os_error(libc::ENXIO);
        let stale_linux = std::io::Error::from_raw_os_error(libc::ENOTCONN);
        let missing = std::io::Error::from_raw_os_error(libc::ENOENT);
        let denied = std::io::Error::from_raw_os_error(libc::EACCES);

        assert!(is_stale_mount_error(&stale_macos), "ENXIO is stale");
        assert!(is_stale_mount_error(&stale_linux), "ENOTCONN is stale");
        assert!(!is_stale_mount_error(&missing), "ENOENT is not stale");
        assert!(!is_stale_mount_error(&denied), "EACCES is not stale");
    }

    /// A healthy directory is not a stale mount.
    #[test]
    fn healthy_directory_is_not_stale() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!is_stale_fuse_mount(dir.path()));
    }

    /// A missing path is not a stale mount (nothing to recover).
    #[test]
    fn missing_path_is_not_stale() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("does-not-exist");
        assert!(!is_stale_fuse_mount(&missing));
    }

    /// Recovery on a healthy mountpoint is a no-op that reports false.
    #[test]
    fn recover_on_healthy_mountpoint_is_noop() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!recover_stale_fuse_mount(dir.path()));
    }

    /// A freshly created directory is not a mountpoint. Exercises the real
    /// platform mount-table query end to end without needing a live mount.
    #[test]
    fn ordinary_directory_is_not_mounted() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!is_path_mounted(dir.path()));
    }

    /// The reparse-target classification: a target that names a mounted
    /// volume (`\Device\Volume{GUID}\`) is a WinFSP/volume mountpoint; a
    /// target that names a filesystem path (an NTFS junction or a symlink)
    /// is not, and neither is an empty string.
    #[test]
    fn winfsp_volume_target_classification() {
        assert!(
            is_winfsp_volume_target(r"\Device\Volume{aa1f6090-d2f0-11f1-9be2-d8b32f2db1cc}\"),
            "a volume device name is a mount"
        );
        assert!(
            is_winfsp_volume_target(r"\Device\Volume{00000000-0000-0000-0000-000000000000}\"),
            "any volume GUID shape is a mount"
        );
        assert!(
            is_winfsp_volume_target(r"  \Device\Volume{aa1f6090-d2f0-11f1-9be2-d8b32f2db1cc}\"),
            "tolerates leading whitespace"
        );
        assert!(
            !is_winfsp_volume_target(r"\??\C:\X-Plane 12\Custom Scenery\zzXEL"),
            "an NTFS junction path is not a volume mount"
        );
        assert!(
            !is_winfsp_volume_target(r"\\?\C:\some\dir"),
            "a symlink path is not a volume mount"
        );
        assert!(
            !is_winfsp_volume_target(r"relative\path"),
            "a relative path is not a volume mount"
        );
        assert!(
            !is_winfsp_volume_target(""),
            "an empty target is not a volume mount"
        );
    }

    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    #[test]
    fn proc_mounts_contains_detects_mountpoint() {
        let mounts = "proc /proc proc rw,nosuid 0 0\n\
                      fuse.xel /mnt/zzXEL_ortho fuse rw 0 0\n\
                      /dev/sda1 / ext4 rw 0 0\n";
        assert!(proc_mounts_contains(mounts, "/mnt/zzXEL_ortho"));
        assert!(proc_mounts_contains(mounts, "/"));
        assert!(!proc_mounts_contains(mounts, "/mnt/not_mounted"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn mount_output_contains_detects_macfuse_mountpoint() {
        // Real macFUSE line shape, including a mountpoint that contains spaces.
        let output = "/dev/disk1s1 on / (apfs, local, journaled)\n\
                      xearthlayer@macfuse0 on /Users/me/X-Plane 12/Custom Scenery/zzXEL_ortho (macfuse, nodev, nosuid, synchronous, mounted by me)\n";
        assert!(mount_output_contains(
            output,
            "/Users/me/X-Plane 12/Custom Scenery/zzXEL_ortho"
        ));
        assert!(mount_output_contains(output, "/"));
        assert!(!mount_output_contains(output, "/Users/me/elsewhere"));
    }
}
