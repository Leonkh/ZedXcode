//! Per-simulator pidfile (`~/.zedxcode/run/sim-<udid>.pid`): SIGTERM the
//! previous proxy instance on start (Rerun can race the old session's
//! teardown).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::util::paths::zedxcode_home;

/// Path of the pidfile for a simulator UDID (creates `~/.zedxcode/run/`).
pub fn pidfile_path(udid: &str) -> Result<PathBuf> {
    let dir = zedxcode_home()?.join("run");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    Ok(dir.join(format!("sim-{udid}.pid")))
}

/// SIGTERM the previous owner (if any) and write our own pid.
pub fn kill_old_and_remember(udid: &str) -> Result<()> {
    claim(&pidfile_path(udid)?, std::process::id())
}

/// SIGTERM the previous owner (if any) WITHOUT taking ownership — the pidfile
/// is left untouched, so the predecessor still reads itself as the owner and,
/// on the resulting teardown, terminates its own (running, debugged) app.
///
/// Called after a successful build and before `simctl install`: on the
/// simulator, `install` blocks while a previous session's app is still running
/// under lldb (it does not replace/kill it), so a Rerun would otherwise stall
/// for as long as the old app lives. Signalling early lets the predecessor
/// tear down and free the bundle so our install can proceed. Ownership is
/// still taken later, post-launch, via [`kill_old_and_remember`].
pub fn kill_old(udid: &str) -> Result<()> {
    signal_old(&pidfile_path(udid)?, std::process::id());
    Ok(())
}

/// Remove our pidfile on clean teardown (only if it is still ours —
/// a newer instance may have re-claimed it).
pub fn remove(udid: &str) -> Result<()> {
    remove_at(&pidfile_path(udid)?, std::process::id())
}

/// SIGTERM the pidfile's current owner if it is a *different* live xcode-dap
/// proxy. A missing, stale, foreign, or self pid is a no-op.
///
/// Never signal init or ourselves; stale garbage is just overwritten by the
/// caller. Only signal a pid that still belongs to an xcode-dap proxy: after a
/// crash/SIGKILL/reboot the pidfile outlives its writer, and macOS reassigns
/// pids densely (and wraps at ~99999), so a bare pid can now own an unrelated
/// same-user process (Terminal, an editor, …). Killing that would be the bug —
/// treat any non-proxy owner as stale garbage.
fn signal_old(path: &Path, my_pid: u32) {
    if let Some(old) = read_pid(path) {
        if old > 1 && old != my_pid as i32 && pid_is_xcode_dap(old) {
            log::info!(target: "pidfile", "SIGTERM previous instance (pid {old})");
            // SAFETY: plain kill(2) with a valid signal; failure (e.g. the
            // process is already gone) is irrelevant.
            unsafe {
                libc::kill(old, libc::SIGTERM);
            }
        }
    }
}

fn claim(path: &Path, my_pid: u32) -> Result<()> {
    signal_old(path, my_pid);
    std::fs::write(path, format!("{my_pid}\n"))
        .with_context(|| format!("writing pidfile {}", path.display()))
}

/// True when `pid`'s executable is an `xcode-dap` proxy binary. A pid we
/// cannot resolve — gone, or not ours — is treated as not-ours (never
/// signaled), so a recycled pid can't make us kill an unrelated process.
fn pid_is_xcode_dap(pid: i32) -> bool {
    proc_exe_path(pid).is_some_and(|p| exe_is_xcode_dap(&p))
}

/// Executable path of a live pid via `proc_pidpath(2)`; `None` when the
/// process is gone or the path is unreadable. Works without privilege for
/// same-user pids, which is all a proxy ever writes into a pidfile.
fn proc_exe_path(pid: i32) -> Option<PathBuf> {
    let mut buf = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    // SAFETY: proc_pidpath writes at most buf.len() bytes and returns the
    // byte length written (> 0), or <= 0 on error (dead pid, EPERM).
    let len =
        unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr() as *mut libc::c_void, buf.len() as u32) };
    if len <= 0 {
        return None;
    }
    let path = std::str::from_utf8(&buf[..len as usize]).ok()?;
    Some(PathBuf::from(path))
}

/// The proxy binary is named `xcode-dap` (its Cargo bin name); match its
/// basename so a symlinked/PATH/cached install all count.
fn exe_is_xcode_dap(path: &Path) -> bool {
    path.file_name().is_some_and(|n| n == "xcode-dap")
}

fn remove_at(path: &Path, my_pid: u32) -> Result<()> {
    if read_pid(path) == Some(my_pid as i32) {
        std::fs::remove_file(path)
            .with_context(|| format!("removing pidfile {}", path.display()))?;
    }
    Ok(())
}

fn read_pid(path: &Path) -> Option<i32> {
    std::fs::read_to_string(path)
        .ok()?
        .trim()
        .parse::<i32>()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_pidfile(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("xdap-pidfile-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn claim_writes_our_pid() {
        let path = temp_pidfile("claim.pid");
        let _ = std::fs::remove_file(&path);
        claim(&path, 4242).unwrap();
        assert_eq!(read_pid(&path), Some(4242));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn claim_overwrites_garbage() {
        let path = temp_pidfile("garbage.pid");
        std::fs::write(&path, "not a pid\n").unwrap();
        claim(&path, 4242).unwrap();
        assert_eq!(read_pid(&path), Some(4242));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn claim_with_own_pid_does_not_signal_self() {
        // Old owner == our pid (e.g. re-claim after Rerun) must not SIGTERM us.
        let path = temp_pidfile("self.pid");
        let me = std::process::id();
        std::fs::write(&path, format!("{me}\n")).unwrap();
        claim(&path, me).unwrap();
        assert_eq!(read_pid(&path), Some(me as i32));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn exe_basename_identifies_the_proxy() {
        assert!(exe_is_xcode_dap(Path::new("/opt/homebrew/bin/xcode-dap")));
        assert!(exe_is_xcode_dap(Path::new("xcode-dap")));
        assert!(!exe_is_xcode_dap(Path::new("/usr/bin/login")));
        assert!(!exe_is_xcode_dap(Path::new(
            "/Applications/Foo.app/Contents/MacOS/Foo"
        )));
    }

    #[test]
    fn unrelated_live_pid_is_not_the_proxy() {
        // The test runner is a live same-user process that is NOT xcode-dap;
        // a pidfile pointing at it must classify as an unrelated owner so
        // claim() overwrites rather than SIGTERMs it.
        assert!(!pid_is_xcode_dap(std::process::id() as i32));
    }

    #[test]
    fn dead_pid_is_not_the_proxy() {
        // Above the macOS pid ceiling (~99999): never resolves, never ours.
        assert!(!pid_is_xcode_dap(999_999));
    }

    #[test]
    fn claim_overwrites_unrelated_live_owner_without_signaling() {
        // A stale pidfile whose pid was recycled by an unrelated process (here
        // the test runner) must be overwritten, never signaled.
        let path = temp_pidfile("recycled.pid");
        let unrelated = std::process::id() as i32;
        std::fs::write(&path, format!("{unrelated}\n")).unwrap();
        // Claim from some other pid; the unrelated live owner is not xcode-dap,
        // so no SIGTERM is sent and the pidfile is simply overwritten.
        claim(&path, 4242).unwrap();
        assert_eq!(read_pid(&path), Some(4242));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn signal_old_never_takes_ownership() {
        // Unlike `claim`, `signal_old` (used by `kill_old`) must NOT write:
        // the predecessor keeps owning the pidfile so its own teardown reads
        // `superseded() == false` and terminates its app, unblocking our
        // install. The test-runner pid is a live, non-proxy owner, so no
        // SIGTERM is sent and the file is left exactly as-is.
        let path = temp_pidfile("signal.pid");
        let owner = std::process::id() as i32;
        std::fs::write(&path, format!("{owner}\n")).unwrap();
        signal_old(&path, 4242);
        assert_eq!(read_pid(&path), Some(owner));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn remove_only_removes_own_pidfile() {
        let path = temp_pidfile("remove.pid");
        claim(&path, 4242).unwrap();
        // Someone else's pid: left in place.
        remove_at(&path, 9999).unwrap();
        assert!(path.exists());
        // Ours: removed.
        remove_at(&path, 4242).unwrap();
        assert!(!path.exists());
        // Removing a nonexistent pidfile is fine.
        remove_at(&path, 4242).unwrap();
    }
}
