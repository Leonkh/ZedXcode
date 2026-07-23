//! Embeds build metadata for the log session header and `doctor`:
//! `XCODE_DAP_BUILD_TS` (ISO-8601 UTC) and `XCODE_DAP_GIT_HASH`
//! (`git rev-parse --short HEAD`, `"unknown"` outside a checkout).

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("cargo:rustc-env=XCODE_DAP_BUILD_TS={}", iso8601_utc(secs));

    let hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|hash| !hash.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=XCODE_DAP_GIT_HASH={hash}");

    // Refresh the embedded hash when HEAD moves. <git-dir>/HEAD only
    // changes on branch switches; <git-dir>/logs/HEAD (the reflog) is
    // appended on every commit/checkout, so same-branch commits refresh
    // the stamp too. Only emitted for paths that exist — a missing
    // rerun-if-changed path makes cargo rerun the script on every build.
    if let Ok(out) = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .output()
    {
        if out.status.success() {
            let git_dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !git_dir.is_empty() {
                for tracked in ["HEAD", "logs/HEAD"] {
                    let path = std::path::Path::new(&git_dir).join(tracked);
                    if path.exists() {
                        println!("cargo:rerun-if-changed={}", path.display());
                    }
                }
            }
        }
    }
}

/// Epoch seconds -> "YYYY-MM-DDTHH:MM:SSZ" (Howard Hinnant's
/// civil_from_days; duplicated in `src/util/logging.rs` — build scripts
/// cannot use crate code).
fn iso8601_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        rem % 3_600 / 60,
        rem % 60
    )
}
