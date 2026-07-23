//! App stdout/stderr file tailers (§5.1) and the optional OSLog pump
//! (§5.3). Tailers poll-read every 75 ms (kqueue is unreliable for files
//! appended by another process). Used by `xcode-dap run` for the live
//! console; DAP mode reuses both for Debug Console `output` events.

use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, BufReader};
use tokio_util::sync::CancellationToken;

use crate::engine::pipeline::OutputSink;
use crate::util::procgroup;

const POLL_INTERVAL: Duration = Duration::from_millis(75);

/// Handles to the running tailer tasks. Call [`stop`](Self::stop) on
/// teardown — merely dropping detaches the tasks and leaves their poll
/// loops running until the process exits.
pub struct Tailers {
    cancel: CancellationToken,
    handles: Vec<tokio::task::JoinHandle<()>>,
}

impl Tailers {
    /// Stop tailing; each tailer drains any final bytes (including a
    /// trailing unterminated line) before exiting.
    pub async fn stop(self) {
        self.cancel.cancel();
        for handle in self.handles {
            let _ = handle.await;
        }
    }
}

/// Start tailing the app's stdout/stderr capture files, emitting whole
/// lines as `"stdout"` / `"stderr"` output lines on `sink`. Files that
/// don't exist yet are retried on every poll.
pub fn start_tailers(stdout_file: &Path, stderr_file: &Path, sink: Arc<dyn OutputSink>) -> Tailers {
    let cancel = CancellationToken::new();
    let handles = vec![
        tokio::spawn(tail_file(
            stdout_file.to_path_buf(),
            "stdout",
            sink.clone(),
            cancel.clone(),
        )),
        tokio::spawn(tail_file(
            stderr_file.to_path_buf(),
            "stderr",
            sink,
            cancel.clone(),
        )),
    ];
    Tailers { cancel, handles }
}

/// Handle to the running OSLog pump task. Call [`stop`](Self::stop) on
/// teardown — merely dropping detaches the task, which keeps running and
/// owns the live `log stream` child (so its `kill_on_drop` never fires).
pub struct OslogPump {
    cancel: CancellationToken,
    handle: tokio::task::JoinHandle<()>,
}

impl OslogPump {
    /// Stop the `log stream` process group and wait for the pump task to
    /// exit.
    pub async fn stop(self) {
        self.cancel.cancel();
        let _ = self.handle.await;
    }
}

/// Default OSLog predicate: the app's own logging only. Matches the app's
/// unified-logging subsystem (apps conventionally log with
/// `Logger(subsystem: Bundle.bundleIdentifier, ...)`) plus anything
/// emitted by a binary image inside the .app bundle (main executable,
/// embedded frameworks, app extensions), which also covers NSLog/os_log
/// without a subsystem. System daemons and
/// third-party SDKs logging under their own subsystems from system images
/// (the Debug Console noise this exists to kill) match neither clause.
pub fn default_oslog_predicate(bundle_id: &str, app_name: &str) -> String {
    format!("subsystem == \"{bundle_id}\" OR senderImagePath CONTAINS \"/{app_name}.app/\"")
}

/// Start the OSLog pump (design §5.3, `"oslog": true`): spawn
/// `xcrun simctl spawn <udid> log stream --style compact --color none
/// --level debug --predicate <predicate>` and emit its lines as
/// `"console"` output on `sink`. `predicate` is the scenario's
/// `oslogPredicate` or [`default_oslog_predicate`]. Call after the PID is
/// known (i.e. post-launch). If the stream dies while the session is still
/// running (e.g. a malformed predicate makes `log stream` exit at once),
/// one diagnostic `oslog: ...` line with its exit status and stderr is
/// emitted instead of failing silently.
pub fn start_oslog_pump(udid: &str, predicate: &str, sink: Arc<dyn OutputSink>) -> OslogPump {
    let cancel = CancellationToken::new();
    let handle = tokio::spawn(pump_oslog(
        udid.to_string(),
        predicate.to_string(),
        sink,
        cancel.clone(),
    ));
    OslogPump { cancel, handle }
}

async fn pump_oslog(
    udid: String,
    predicate: String,
    sink: Arc<dyn OutputSink>,
    cancel: CancellationToken,
) {
    let mut cmd = tokio::process::Command::new("xcrun");
    cmd.args(["simctl", "spawn", &udid, "log", "stream"])
        .args(["--style", "compact"])
        .args(["--color", "none"])
        .args(["--level", "debug"])
        .args(["--predicate", &predicate])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Own process group: teardown must reach the in-simulator `log`
    // process too — SIGKILLing only the `xcrun` host leaves it running
    // until its next write hits the closed pipe.
    procgroup::spawn_in_new_group(&mut cmd);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            sink.line(
                "console",
                &format!("oslog: failed to spawn log stream: {e}"),
            );
            return;
        }
    };
    let pgid = child.id().map(|p| p as i32).unwrap_or(0);
    // Collect stderr in the background so an early failure (e.g. an
    // NSPredicate parse error) can be reported after the stream ends.
    let stderr = child.stderr.take();
    let stderr_task = tokio::spawn(async move {
        let mut buf = String::new();
        if let Some(mut stderr) = stderr {
            let _ = stderr.read_to_string(&mut buf).await;
        }
        buf
    });
    let Some(stdout) = child.stdout.take() else {
        procgroup::kill_group(pgid);
        let _ = child.wait().await;
        stderr_task.abort();
        return;
    };
    let mut lines = BufReader::new(stdout).lines();
    loop {
        tokio::select! {
            line = lines.next_line() => match line {
                // Internal category "oslog": DapSink emits it as DAP
                // "console" but keeps the stream out of the log at INFO.
                Ok(Some(l)) => sink.line("oslog", &l),
                // EOF / read error: log stream exited on its own.
                _ => break,
            },
            _ = cancel.cancelled() => break,
        }
    }
    let exited_on_its_own = !cancel.is_cancelled();
    // SIGTERM the group, then escalate to SIGKILL after 3 s (mirrors the
    // xcodebuild teardown). Harmless when the stream already exited.
    procgroup::term_group(pgid);
    let status = match tokio::time::timeout(Duration::from_secs(3), child.wait()).await {
        Ok(res) => res.ok(),
        Err(_) => {
            procgroup::kill_group(pgid);
            child.wait().await.ok()
        }
    };
    if exited_on_its_own {
        // The session is still running but oslog output stopped: say why,
        // otherwise e.g. a malformed `oslogPredicate` produces no output
        // and no diagnostic anywhere. The timeout guards against a pipe
        // fd surviving in a process the group signals did not reach.
        let stderr_text = tokio::time::timeout(Duration::from_secs(1), stderr_task)
            .await
            .ok()
            .and_then(|joined| joined.ok())
            .unwrap_or_default();
        let status = status
            .map(|s| s.to_string())
            .unwrap_or_else(|| "unknown status".to_string());
        let mut msg = format!("oslog: log stream exited ({status})");
        let stderr_text = stderr_text.trim();
        if !stderr_text.is_empty() {
            msg.push_str(&format!(": {stderr_text}"));
        }
        sink.line("console", &msg);
    } else {
        stderr_task.abort();
    }
}

async fn tail_file(
    path: PathBuf,
    category: &'static str,
    sink: Arc<dyn OutputSink>,
    cancel: CancellationToken,
) {
    let mut offset: u64 = 0;
    let mut partial: Vec<u8> = Vec::new();
    loop {
        let stopping = tokio::select! {
            _ = tokio::time::sleep(POLL_INTERVAL) => false,
            _ = cancel.cancelled() => true,
        };
        drain_new(&path, &mut offset, &mut partial, category, &*sink).await;
        if stopping {
            if !partial.is_empty() {
                let text = String::from_utf8_lossy(&partial);
                sink.line(category, text.trim_end_matches('\r'));
            }
            return;
        }
    }
}

/// Read bytes appended since `offset`, emit complete lines, keep the rest
/// in `partial`. Tolerates the file not existing yet and truncation.
async fn drain_new(
    path: &Path,
    offset: &mut u64,
    partial: &mut Vec<u8>,
    category: &str,
    sink: &dyn OutputSink,
) {
    let Ok(meta) = tokio::fs::metadata(path).await else {
        return; // not created yet
    };
    let len = meta.len();
    if len < *offset {
        // Truncated (e.g. relaunch pre-truncates the capture files).
        *offset = 0;
        partial.clear();
    }
    if len == *offset {
        return;
    }
    let Ok(mut file) = tokio::fs::File::open(path).await else {
        return;
    };
    if file.seek(SeekFrom::Start(*offset)).await.is_err() {
        return;
    }
    let mut buf = Vec::new();
    let Ok(n) = (&mut file).take(len - *offset).read_to_end(&mut buf).await else {
        return;
    };
    *offset += n as u64;
    partial.extend_from_slice(&buf);
    while let Some(pos) = partial.iter().position(|&b| b == b'\n') {
        let line: Vec<u8> = partial.drain(..=pos).collect();
        let text = String::from_utf8_lossy(&line[..line.len() - 1]);
        sink.line(category, text.trim_end_matches('\r'));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_oslog_predicate_scopes_to_app_subsystem_and_bundle() {
        assert_eq!(
            default_oslog_predicate("com.example.myapp", "MyApp"),
            "subsystem == \"com.example.myapp\" \
             OR senderImagePath CONTAINS \"/MyApp.app/\""
        );
    }
}
