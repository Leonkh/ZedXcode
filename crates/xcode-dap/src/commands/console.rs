//! `xcode-dap console` — print/tail the app console capture files of the
//! current (or most recent) run: `~/.zedxcode/run/<udid>/{out,err}.log`.
//! Lets a plain task terminal watch app logs (the "Xcode: Console" task);
//! the same files feed the Debug Console `output` events in DAP mode.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{bail, Context, Result};

use crate::engine::consoles;
use crate::engine::pipeline::{zedxcode_home, OutputSink};

#[derive(clap::Args, Debug)]
pub struct ConsoleArgs {
    /// Simulator UDID (default: the most recently launched run)
    #[arg(long)]
    pub udid: Option<String>,
    /// Keep following the logs like `tail -f` (default: print what is
    /// there and exit)
    #[arg(long, short = 'f')]
    pub follow: bool,
}

/// Sink for follow mode: `[out]` / `[err]` prefixes, one line per line.
/// Rust's stdout is line-buffered, so lines appear as they arrive.
struct PrefixSink;

impl OutputSink for PrefixSink {
    fn line(&self, category: &str, text: &str) {
        println!("{} {text}", prefix(category));
    }
}

fn prefix(category: &str) -> &'static str {
    if category == "stderr" {
        "[err]"
    } else {
        "[out]"
    }
}

pub async fn run(args: ConsoleArgs) -> Result<()> {
    let run_root = zedxcode_home()?.join("run");
    let dir = match &args.udid {
        Some(udid) => {
            let dir = run_root.join(udid);
            if !dir.is_dir() {
                bail!(
                    "no run logs for simulator {udid} ({} does not exist) — \
                     launch the app first (CMD+R in Zed, or `xcode-dap run`)",
                    dir.display()
                );
            }
            dir
        }
        None => newest_run_dir(&run_root)?,
    };
    let out_log = dir.join("out.log");
    let err_log = dir.join("err.log");
    eprintln!("Console logs: {}", dir.display());

    if args.follow {
        eprintln!("Following (Ctrl-C to stop)...");
        let sink: Arc<dyn OutputSink> = Arc::new(PrefixSink);
        // Tailers start at offset 0: existing content prints first, then
        // new lines as the app writes them (survives relaunch truncation).
        let tailers = consoles::start_tailers(&out_log, &err_log, sink);
        tokio::signal::ctrl_c()
            .await
            .context("waiting for Ctrl-C")?;
        tailers.stop().await; // final drain
    } else {
        dump(&out_log, prefix("stdout"))?;
        dump(&err_log, prefix("stderr"))?;
    }
    Ok(())
}

/// The run dir whose logs were written to most recently (newest of
/// out.log / err.log mtimes per `~/.zedxcode/run/<udid>/` subdirectory).
fn newest_run_dir(root: &Path) -> Result<PathBuf> {
    const HINT: &str = "no run logs found under ~/.zedxcode/run — launch the app first \
         (CMD+R in Zed, or `xcode-dap run`)";
    let Ok(entries) = std::fs::read_dir(root) else {
        bail!("{HINT}"); // run dir not created yet
    };
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(mtime) = ["out.log", "err.log"]
            .iter()
            .filter_map(|f| {
                std::fs::metadata(path.join(f))
                    .and_then(|m| m.modified())
                    .ok()
            })
            .max()
        else {
            continue; // no capture files in this dir
        };
        if best.as_ref().is_none_or(|(t, _)| mtime > *t) {
            best = Some((mtime, path));
        }
    }
    best.map(|(_, path)| path).context(HINT)
}

/// Print the whole current content of one capture file with a prefix
/// (non-follow mode). A missing file is fine — nothing captured yet.
fn dump(path: &Path, prefix: &str) -> Result<()> {
    let Ok(bytes) = std::fs::read(path) else {
        return Ok(());
    };
    let text = String::from_utf8_lossy(&bytes);
    let stdout = std::io::stdout().lock();
    let mut stdout = std::io::BufWriter::new(stdout);
    for line in text.lines() {
        writeln!(stdout, "{prefix} {line}")?;
    }
    stdout.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn sandbox() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zedxcode-console-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn newest_run_dir_picks_most_recent_logs() {
        let root = sandbox();
        let old = root.join("UDID-OLD");
        let new = root.join("UDID-NEW");
        let empty = root.join("UDID-EMPTY"); // no capture files: ignored
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::write(old.join("out.log"), "old\n").unwrap();
        // Distinct mtime without sleeping.
        let earlier = filetime_set_back(&old.join("out.log"));
        assert!(earlier, "could not age the old log file");
        std::fs::write(new.join("out.log"), "new\n").unwrap();
        assert_eq!(newest_run_dir(&root).unwrap(), new);
    }

    #[test]
    fn newest_run_dir_errors_when_empty() {
        let root = sandbox();
        let err = newest_run_dir(&root.join("missing")).unwrap_err();
        assert!(err.to_string().contains("no run logs found"));
        let err = newest_run_dir(&root).unwrap_err();
        assert!(err.to_string().contains("no run logs found"));
    }

    /// Set a file's mtime one hour into the past via `utimes(2)`.
    fn filetime_set_back(path: &Path) -> bool {
        use std::os::unix::ffi::OsStrExt;
        let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        let now = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as libc::time_t;
        let times = [
            libc::timeval {
                tv_sec: now - 3600,
                tv_usec: 0,
            },
            libc::timeval {
                tv_sec: now - 3600,
                tv_usec: 0,
            },
        ];
        // SAFETY: valid NUL-terminated path and a 2-element timeval array.
        unsafe { libc::utimes(c_path.as_ptr(), times.as_ptr()) == 0 }
    }
}
