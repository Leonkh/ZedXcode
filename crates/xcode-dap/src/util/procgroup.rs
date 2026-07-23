//! Process-group helpers: spawn children in their own group (setpgid) and
//! kill the whole group so e.g. `swift-frontend` children of xcodebuild die
//! too. See `docs/design/dap-proxy.md` §3.3 ("Stop mid-build").

use std::io;

/// Configure `cmd` to start in a new process group (pre_exec + setpgid).
///
/// After `spawn()`, the child's pid doubles as its group id (pgid) and can
/// be passed to [`term_group`] / [`kill_group`].
pub fn spawn_in_new_group(cmd: &mut tokio::process::Command) -> &mut tokio::process::Command {
    // SAFETY: setpgid(0, 0) is async-signal-safe and runs in the freshly
    // forked child before exec; nothing else runs there yet.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd
}

/// SIGTERM the entire process group `pgid`.
pub fn term_group(pgid: i32) {
    signal_group(pgid, libc::SIGTERM);
}

/// SIGKILL the entire process group `pgid`.
pub fn kill_group(pgid: i32) {
    signal_group(pgid, libc::SIGKILL);
}

fn signal_group(pgid: i32, sig: libc::c_int) {
    if pgid > 0 {
        // SAFETY: kill(2) with a negative pid signals the whole group.
        unsafe {
            libc::kill(-pgid, sig);
        }
    }
}
