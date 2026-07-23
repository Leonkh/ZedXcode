//! File logger behind the `log` facade: `~/.zedxcode/logs/xcode-dap.log`.
//!
//! In DAP mode stdout carries only framed DAP messages (see `util/mod.rs`),
//! so diagnostics go to this file; ERROR records additionally tee to
//! stderr, which Zed surfaces in its debug-adapter log view. Init failures
//! silently disable logging — diagnostics must never break a DAP session,
//! and must never touch stdout.
//!
//! Hygiene rules for callers:
//! - never log environment variables wholesale (user env may hold
//!   secrets); single well-known variables such as
//!   `XCODE_DAP_RESOLVED_FROM` are fine;
//! - DAP frame bodies only at TRACE, truncated to 2 KB ([`truncate`]);
//!   compact summaries (`peek::summarize`) at DEBUG.
//!
//! Level comes from `XCODE_DAP_LOG` (`error|warn|info|debug|trace`,
//! default `info`); the `verboseLogging` scenario key raises a session to
//! `trace`. Rotation is one generation: a file over 5 MB is renamed to
//! `xcode-dap.log.old` at init.

use std::borrow::Cow;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use log::{Level, LevelFilter, Log, Metadata, Record};

use crate::util::paths::zedxcode_home;

const LOG_FILE_NAME: &str = "xcode-dap.log";
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

/// Whether [`init`] actually installed the file logger (false: every
/// `log::...!` call hits the noop logger — callers can skip work such as
/// the `verboseLogging` level raise).
static ACTIVE: AtomicBool = AtomicBool::new(false);

pub fn is_active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// Log-file state captured by [`init`] *before* it opened/rotated the file.
/// `main` runs init before every subcommand, so a post-init stat always
/// shows the file freshly touched by this very process — doctor reports
/// this snapshot instead.
#[derive(Clone, Copy, Debug)]
pub struct PreInitLogFile {
    pub existed: bool,
    pub len: u64,
    pub modified: Option<SystemTime>,
}

static PRE_INIT: OnceLock<PreInitLogFile> = OnceLock::new();

/// The pre-init snapshot; `None` when init never got as far as stat-ing
/// the file (no HOME / unwritable log dir).
pub fn pre_init_log_file() -> Option<PreInitLogFile> {
    PRE_INIT.get().copied()
}

/// `~/.zedxcode/logs/xcode-dap.log` (does not create anything).
pub fn log_file_path() -> anyhow::Result<PathBuf> {
    Ok(zedxcode_home()?.join("logs").join(LOG_FILE_NAME))
}

struct FileLogger {
    file: Mutex<File>,
    pid: u32,
    mode: String,
}

impl Log for FileLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        // One write_all per record (newline included): writeln!'s separate
        // line + newline writes can interleave across concurrent processes
        // appending to the same file, O_APPEND notwithstanding.
        let line = format!(
            "{} {} [pid {} {}] {}\n",
            utc_timestamp(),
            record.level(),
            self.pid,
            self.mode,
            record.args()
        );
        if record.level() == Level::Error {
            eprint!("{line}");
        }
        if let Ok(mut file) = self.file.lock() {
            let _ = file.write_all(line.as_bytes());
        }
    }

    fn flush(&self) {
        if let Ok(mut file) = self.file.lock() {
            let _ = file.flush();
        }
    }
}

/// Install the file logger and write the session header line. `mode` is
/// `"dap"` or the subcommand name; it tags every log line. Any failure
/// (no HOME, unwritable directory, logger already set) leaves logging
/// disabled without a diagnostic.
pub fn init(mode: &str) {
    let Some(file) = open_log_file() else { return };
    let logger = FileLogger {
        file: Mutex::new(file),
        pid: std::process::id(),
        mode: mode.to_string(),
    };
    if log::set_logger(Box::leak(Box::new(logger))).is_err() {
        return;
    }
    ACTIVE.store(true, Ordering::Relaxed);
    log::set_max_level(level_from_env());
    let resolved_from = match std::env::var("XCODE_DAP_RESOLVED_FROM") {
        Ok(v) if !v.is_empty() => format!(" resolved-from {v}"),
        _ => String::new(),
    };
    log::info!(
        "session start: xcode-dap {} ({}, built {}) cwd {} argv {:?}{}",
        env!("CARGO_PKG_VERSION"),
        env!("XCODE_DAP_GIT_HASH"),
        env!("XCODE_DAP_BUILD_TS"),
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "?".to_string()),
        std::env::args().collect::<Vec<_>>(),
        resolved_from,
    );
}

fn open_log_file() -> Option<File> {
    let dir = zedxcode_home().ok()?.join("logs");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(LOG_FILE_NAME);
    // Snapshot the file's state before rotation/open (doctor's report).
    let pre = match std::fs::metadata(&path) {
        Ok(m) => PreInitLogFile {
            existed: true,
            len: m.len(),
            modified: m.modified().ok(),
        },
        Err(_) => PreInitLogFile {
            existed: false,
            len: 0,
            modified: None,
        },
    };
    let _ = PRE_INIT.set(pre);
    // The stat -> rename -> open sequence is a cross-process TOCTOU (two
    // concurrent inits can clobber a freshly rotated .old and strand a live
    // session's fd on it) — serialize it with flock on a sidecar lock file.
    // Fail-silent: no lock -> proceed unguarded rather than disable logging.
    let lock = rotation_lock(&dir);
    let oversized = std::fs::metadata(&path)
        .map(|m| m.len() > MAX_LOG_BYTES)
        .unwrap_or(false);
    // One-generation rotation: over 5 MB -> `xcode-dap.log.old`
    // (replacing any previous `.old`).
    if oversized {
        let _ = std::fs::rename(&path, path.with_extension("log.old"));
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok();
    drop(lock); // released only after the log file is open
    file
}

/// `flock(LOCK_EX)` on `<dir>/xcode-dap.log.lock`; the lock is released
/// when the returned `File` drops (closing the fd unlocks).
fn rotation_lock(dir: &Path) -> Option<File> {
    use std::os::fd::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(format!("{LOG_FILE_NAME}.lock")))
        .ok()?;
    // SAFETY: `file` is a valid open fd for the lifetime of the call.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return None;
    }
    Some(file)
}

/// `XCODE_DAP_LOG` -> level filter (`error|warn|info|debug|trace`,
/// case-insensitive); unset or unrecognized -> `info`.
fn level_from_env() -> LevelFilter {
    match std::env::var("XCODE_DAP_LOG")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "error" => LevelFilter::Error,
        "warn" => LevelFilter::Warn,
        "debug" => LevelFilter::Debug,
        "trace" => LevelFilter::Trace,
        _ => LevelFilter::Info,
    }
}

/// Truncate to at most `max` bytes on a char boundary, appending a
/// byte-count marker when cut (DAP bodies at TRACE are capped at 2 KB).
pub fn truncate(s: &str, max: usize) -> Cow<'_, str> {
    if s.len() <= max {
        return Cow::Borrowed(s);
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    Cow::Owned(format!(
        "{}… [truncated, {} bytes total]",
        &s[..end],
        s.len()
    ))
}

/// Render a command + args for a log line (`xcrun simctl boot <udid>`).
pub fn describe_command(cmd: &tokio::process::Command) -> String {
    let std_cmd = cmd.as_std();
    let mut rendered = std_cmd.get_program().to_string_lossy().into_owned();
    for arg in std_cmd.get_args() {
        rendered.push(' ');
        rendered.push_str(&arg.to_string_lossy());
    }
    rendered
}

/// `SystemTime` -> "YYYY-MM-DDTHH:MM:SSZ" (doctor's log-file mtime row).
pub fn format_system_time(t: SystemTime) -> String {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => format!("{}Z", format_epoch_secs(d.as_secs())),
        Err(_) => "before-epoch".to_string(),
    }
}

/// Current UTC time with milliseconds, e.g. "2026-07-13T12:34:56.789Z".
fn utc_timestamp() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!(
        "{}.{:03}Z",
        format_epoch_secs(now.as_secs()),
        now.subsec_millis()
    )
}

/// Epoch seconds -> "YYYY-MM-DDTHH:MM:SS" (no zone suffix). Date part via
/// Howard Hinnant's civil_from_days — no chrono dependency.
fn format_epoch_secs(secs: u64) -> String {
    let (y, m, d) = civil_from_days((secs / 86_400) as i64);
    let rem = secs % 86_400;
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}

/// Days since 1970-01-01 -> (year, month, day) in the proleptic Gregorian
/// calendar (<https://howardhinnant.github.io/date_algorithms.html>).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 {
        (mp + 3) as u32
    } else {
        (mp - 9) as u32
    };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_formatting() {
        assert_eq!(format_epoch_secs(0), "1970-01-01T00:00:00");
        assert_eq!(format_epoch_secs(86_399), "1970-01-01T23:59:59");
        // Leap day.
        assert_eq!(format_epoch_secs(951_782_400), "2000-02-29T00:00:00");
        assert_eq!(format_epoch_secs(1_700_000_000), "2023-11-14T22:13:20");
    }

    #[test]
    fn level_parsing_defaults_to_info() {
        // level_from_env reads the process env — only the pure default is
        // asserted here (tests must not mutate global env).
        if std::env::var_os("XCODE_DAP_LOG").is_none() {
            assert_eq!(level_from_env(), LevelFilter::Info);
        }
    }

    #[test]
    fn truncate_keeps_short_and_cuts_long() {
        assert_eq!(truncate("short", 10), "short");
        let cut = truncate("0123456789abcdef", 8);
        assert!(cut.starts_with("01234567"));
        assert!(cut.contains("16 bytes total"));
        // Never splits a multi-byte char.
        let s = "aé"; // 'é' is 2 bytes starting at index 1
        let cut = truncate(s, 2);
        assert!(cut.starts_with('a'));
        assert!(!cut.contains('\u{FFFD}'));
    }
}
