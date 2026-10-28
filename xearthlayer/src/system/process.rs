//! Process liveness and the instance lock.

use std::path::Path;

/// The PID recorded in an instance lock file, if it holds one.
///
/// An unreadable or unparseable lock reads as absent: a corrupt lock should be
/// replaced, never treated as a live owner that can never be cleared.
pub fn recorded_pid(lock_path: &Path) -> Option<u32> {
    std::fs::read_to_string(lock_path).ok()?.trim().parse().ok()
}

/// The PID of another live `xearthlayer run` that holds this instance lock.
///
/// Our own PID is not "another instance", and a PID that is no longer alive is
/// a stale lock.
pub fn running_instance(lock_path: &Path) -> Option<u32> {
    recorded_pid(lock_path).filter(|&pid| pid != std::process::id() && is_alive(pid))
}

/// Is a process with this PID currently running?
///
/// Unix uses signal 0, which performs the permission and existence checks of
/// `kill(2)` without delivering anything. `EPERM` counts as alive: the process
/// exists, it simply belongs to another user.
///
/// Windows opens the process for limited information and reads its exit code;
/// a live process reports `STILL_ACTIVE`. A process that has exited but not
/// yet been reaped reports no exit code, which counts as dead: it holds
/// nothing a live owner would.
pub fn is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        if pid == 0 {
            // Signal 0 to PID 0 addresses the caller's whole process group,
            // which is never the question being asked here.
            return false;
        }
        // A PID no process can have. Cast, it would turn negative and address
        // every process we may signal, or a process group.
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        // SAFETY: kill() with signal 0 sends nothing. It only reports whether
        // the PID exists and whether we may signal it.
        let rc = unsafe { libc::kill(pid, 0) };
        if rc == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        const STILL_ACTIVE: u32 = 259; // the exit code of a process that has not exited
                                       // An invalid or foreign PID is refused by OpenProcess itself, so the
                                       // Unix pid_t range guard has no Windows counterpart.
        let Some(handle) =
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()
        else {
            return false;
        };
        let mut exit_code = 0u32;
        // A live process reports STILL_ACTIVE; a real exit code means it is
        // gone. A process that has exited but not been reaped answers no
        // exit code at all, which also counts as gone.
        let alive = unsafe { GetExitCodeProcess(handle, &mut exit_code) }
            .is_ok_and(|()| exit_code == STILL_ACTIVE);
        let _ = unsafe { CloseHandle(handle) };
        alive
    }
    #[cfg(not(any(unix, target_os = "windows")))]
    {
        let _ = pid;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_current_process_is_alive() {
        assert!(is_alive(std::process::id()));
    }

    #[test]
    fn pid_zero_is_never_reported_alive() {
        // PID 0 means "my process group" to kill(2), so answering from the
        // syscall would be answering a different question.
        assert!(!is_alive(0));
    }

    #[test]
    fn a_pid_beyond_pid_t_is_never_reported_alive() {
        // Cast to pid_t these are negative: -1 addresses every process we
        // may signal, and other negatives address a process group.
        assert!(!is_alive(u32::MAX));
        assert!(!is_alive(i32::MAX as u32 + 1));
    }

    /// A child that lives long enough for the assertion, on every platform.
    /// `sleep` is not a Windows command, so there it is `ping` against the
    /// loopback, which idles for ~30 s and is killed before it finishes.
    fn long_running_child() -> std::process::Child {
        let (program, args): (&str, Vec<&str>) = if cfg!(windows) {
            ("ping", vec!["-n", "31", "127.0.0.1"])
        } else {
            ("sleep", vec!["30"])
        };
        std::process::Command::new(program)
            .args(args)
            .spawn()
            .expect("spawn a long-running child")
    }

    #[test]
    fn a_running_child_is_alive() {
        // A real assertion in the direction that can be made deterministic.
        // The dead-PID direction is deliberately not asserted here: after
        // reaping, a busy machine may recycle the PID, and a test that is
        // usually right is worse than one that is always meaningful. The logic
        // that depends on a dead PID is covered where the predicate is
        // injected, in the running-instance preflight check.
        let mut child = long_running_child();
        let pid = child.id();
        assert!(is_alive(pid), "a child we have not reaped must be alive");
        child.kill().expect("kill");
        child.wait().expect("reap");
    }

    fn lock_with(contents: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("xearthlayer.lock");
        std::fs::write(&path, contents).expect("write lock");
        (dir, path)
    }

    #[test]
    fn recorded_pid_reads_a_pid_with_whitespace() {
        let (_dir, path) = lock_with("4242\n");
        assert_eq!(recorded_pid(&path), Some(4242));
    }

    #[test]
    fn recorded_pid_is_none_for_a_missing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(recorded_pid(&dir.path().join("absent.lock")), None);
    }

    #[test]
    fn recorded_pid_is_none_for_garbage() {
        let (_dir, path) = lock_with("not a pid");
        assert_eq!(recorded_pid(&path), None);
    }

    #[test]
    fn our_own_pid_is_not_another_instance() {
        let (_dir, path) = lock_with(&std::process::id().to_string());
        assert_eq!(running_instance(&path), None);
    }

    #[test]
    fn a_pid_no_process_can_have_is_not_a_running_instance() {
        // A reaped child's PID could be recycled; one beyond pid_t never is.
        let (_dir, path) = lock_with("4294967295");
        assert_eq!(running_instance(&path), None);
    }

    #[test]
    fn a_live_child_is_a_running_instance() {
        let mut child = long_running_child();
        let pid = child.id();
        let (_dir, path) = lock_with(&pid.to_string());
        let found = running_instance(&path);
        child.kill().expect("kill");
        child.wait().expect("reap");
        assert_eq!(found, Some(pid));
    }
}
