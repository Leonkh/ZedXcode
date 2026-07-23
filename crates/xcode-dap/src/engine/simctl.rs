//! simctl: device resolution, boot, install, launch, terminate, pid fallback.
//! See `docs/design/dap-proxy.md` §4 (phases 2, 5-7).

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context};
use serde_json::Value;
use tokio::process::Command;

use crate::util::logging;

/// `xcrun simctl list devices --json` -> match by udid-or-name (+ optional
/// OS runtime), prefer Booted, deterministic sort. Returns the UDID.
///
/// The special query `"booted"` matches any currently booted device.
/// `device: None` = default resolution: prefer the booted iPhone, else the
/// newest available iPhone (highest OS, then last name in sort order).
pub async fn resolve_device(device: Option<&str>, os: Option<&str>) -> anyhow::Result<String> {
    let mut cmd = Command::new("xcrun");
    cmd.args(["simctl", "list", "devices", "--json"]);
    let out = output_logged(&mut cmd, "xcrun simctl list devices --json").await?;
    if !out.status.success() {
        bail!(
            "`xcrun simctl list devices --json` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let json: Value =
        serde_json::from_slice(&out.stdout).context("parsing simctl device list JSON")?;
    pick_device(&json, device, os)
}

#[derive(Debug)]
struct Candidate {
    udid: String,
    name: String,
    booted: bool,
    os: (u32, u32),
}

/// Pure device selection over the parsed `simctl list devices --json`
/// output (separated from the shell-out for unit testing).
///
/// `query: None` candidates are all iPhones; the sort below then yields
/// "booted iPhone first, else newest available iPhone".
fn pick_device(json: &Value, query: Option<&str>, os: Option<&str>) -> anyhow::Result<String> {
    let devices = json
        .get("devices")
        .and_then(Value::as_object)
        .context("malformed simctl JSON: missing `devices` object")?;
    // "26.3" -> runtime key suffix "iOS-26-3"
    let os_suffix = os.map(|v| format!("iOS-{}", v.replace('.', "-")));

    let mut candidates: Vec<Candidate> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for (runtime, devs) in devices {
        // iOS simulators only (skips watchOS/tvOS runtimes).
        if !runtime.contains("SimRuntime.iOS") {
            continue;
        }
        if let Some(sfx) = &os_suffix {
            if !runtime.ends_with(sfx.as_str()) {
                continue;
            }
        }
        let Some(arr) = devs.as_array() else { continue };
        for d in arr {
            if !d
                .get("isAvailable")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                continue;
            }
            let (Some(udid), Some(name), Some(state)) = (
                d.get("udid").and_then(Value::as_str),
                d.get("name").and_then(Value::as_str),
                d.get("state").and_then(Value::as_str),
            ) else {
                continue;
            };
            let version = runtime_version(runtime);
            seen.push(format!("{name} (iOS {}.{}, {state})", version.0, version.1));
            let booted = state == "Booted";
            let matches = match query {
                Some(q) => {
                    q.eq_ignore_ascii_case(udid)
                        || q.eq_ignore_ascii_case(name)
                        || (q.eq_ignore_ascii_case("booted") && booted)
                }
                None => name.starts_with("iPhone"),
            };
            if matches {
                candidates.push(Candidate {
                    udid: udid.to_string(),
                    name: name.to_string(),
                    booted,
                    os: version,
                });
            }
        }
    }
    if candidates.is_empty() {
        let os_note = os.map(|o| format!(" (iOS {o})")).unwrap_or_default();
        let what = match query {
            Some(q) => format!("matching \"{q}\""),
            None => "(no \"device\" configured: looked for an iPhone)".to_string(),
        };
        // `seen` is empty when the runtime filter excluded every device —
        // an empty "available devices" list would be useless guidance.
        if seen.is_empty() {
            let why = match os {
                Some(o) => format!("no iOS {o} simulator runtime is installed"),
                None => "no iOS simulator devices are available".to_string(),
            };
            bail!(
                "no available simulator {what}{os_note}: {why}\n\
                 hint: install the runtime and create a device in Xcode \
                 (Settings → Components, Window → Devices and Simulators)"
            );
        }
        seen.sort();
        bail!(
            "no available simulator {what}{os_note}; available devices:\n  {}\n\
             hint: pass --device \"<name or udid>\" (or set \"device\" in .zed/debug.json)",
            seen.join("\n  ")
        );
    }
    // Deterministic: Booted first, then newest OS, then name, then udid.
    // With an explicit query the name tie-break is ascending (candidates
    // usually share one name anyway); with `query: None` it is descending,
    // so the newest iPhone model wins within the same OS.
    candidates.sort_by(|a, b| {
        let name_order = match query {
            Some(_) => a.name.cmp(&b.name),
            None => b.name.cmp(&a.name),
        };
        b.booted
            .cmp(&a.booted)
            .then(b.os.cmp(&a.os))
            .then(name_order)
            .then(a.udid.cmp(&b.udid))
    });
    Ok(candidates.remove(0).udid)
}

/// "com.apple.CoreSimulator.SimRuntime.iOS-26-3" -> (26, 3)
fn runtime_version(runtime: &str) -> (u32, u32) {
    let tail = runtime.rsplit("iOS-").next().unwrap_or("");
    let mut parts = tail.split('-').filter_map(|p| p.parse::<u32>().ok());
    (parts.next().unwrap_or(0), parts.next().unwrap_or(0))
}

/// Time budget for retrying a `simctl boot` racing a shutdown in flight
/// ("Unable to boot device in current state: Shutting Down"). A real
/// simulator shutdown takes 10-30 s on a loaded machine (the typical
/// trigger: quitting Simulator.app and immediately rerunning), so the
/// budget must cover that — a handful of attempts would give up too early.
const BOOT_RETRY_BUDGET: Duration = Duration::from_secs(30);
const BOOT_RETRY_DELAY: Duration = Duration::from_secs(2);

/// `xcrun simctl boot <udid>` (tolerating "already booted/booting",
/// retrying a "Shutting Down" race) + `open -a Simulator` (visible
/// window) + `xcrun simctl bootstatus <udid>` (blocks until ready,
/// no-op when already booted).
///
/// Ordered this way because opening Simulator.app first lets it auto-boot
/// the same device concurrently, making a `bootstatus -b` inner boot fail
/// with SimError 405 "Unable to boot device in current state: Booted".
pub async fn boot(udid: &str) -> anyhow::Result<()> {
    let retry_started = std::time::Instant::now();
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let mut cmd = Command::new("xcrun");
        cmd.args(["simctl", "boot", udid]);
        let out = output_logged(&mut cmd, "xcrun simctl boot").await?;
        if out.status.success() {
            break;
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        if boot_error_is_benign(&stderr) {
            log::info!(
                target: "simctl",
                "simctl boot tolerated (already booted/booting): {}",
                stderr.trim()
            );
            break;
        }
        if boot_error_is_retryable(&stderr) && retry_started.elapsed() < BOOT_RETRY_BUDGET {
            log::warn!(
                target: "simctl",
                "simctl boot attempt {attempt} hit a shutdown in flight — \
                 retrying in {}s ({}s of the {}s budget used): {}",
                BOOT_RETRY_DELAY.as_secs(),
                retry_started.elapsed().as_secs(),
                BOOT_RETRY_BUDGET.as_secs(),
                stderr.trim()
            );
            tokio::time::sleep(BOOT_RETRY_DELAY).await;
            continue;
        }
        bail!(
            "`xcrun simctl boot` failed ({}): {}",
            out.status,
            stderr.trim()
        );
    }
    run_ok(
        Command::new("open").args(["-a", "Simulator"]),
        "open -a Simulator",
    )
    .await?;
    run_ok(
        Command::new("xcrun").args(["simctl", "bootstatus", udid]),
        "xcrun simctl bootstatus",
    )
    .await?;
    Ok(())
}

/// `simctl boot` fails with SimError 405 when the device is already
/// Booted (or mid-boot, e.g. raced by Simulator.app); that's success for us.
fn boot_error_is_benign(stderr: &str) -> bool {
    stderr.contains("Unable to boot device in current state: Booted")
        || stderr.contains("Unable to boot device in current state: Booting")
}

/// `simctl boot` also fails with SimError 405 while a previous session's
/// shutdown is still in flight; that state resolves by itself — retry.
fn boot_error_is_retryable(stderr: &str) -> bool {
    stderr.contains("Unable to boot device in current state: Shutting Down")
}

/// `xcrun simctl install <udid> <app>`.
pub async fn install(udid: &str, app: &Path) -> anyhow::Result<()> {
    run_ok(
        Command::new("xcrun")
            .args(["simctl", "install", udid])
            .arg(app),
        "xcrun simctl install",
    )
    .await
    .with_context(|| {
        format!(
            "installing {} on simulator {udid} \
             (if the simulator is in a bad state, try `xcrun simctl shutdown {udid}` \
             and rerun, or Device → Erase All Content and Settings)",
            app.display()
        )
    })
}

/// `xcrun simctl launch [--wait-for-debugger] --terminate-running-process
/// --stdout=... --stderr=... <udid> <bundle>`. Returns the app PID
/// (parsed from "<bundle>: <pid>", with the ps-poll fallback).
///
/// `app_name` is the `.app` wrapper stem (e.g. `"MyApp"`), used only by the
/// ps fallback. `stdout_file`/`stderr_file` must be absolute paths; they are
/// pre-truncated by the pipeline before launch.
pub async fn launch(
    udid: &str,
    bundle_id: &str,
    app_name: &str,
    wait_for_debugger: bool,
    stdout_file: &Path,
    stderr_file: &Path,
) -> anyhow::Result<i64> {
    // Snapshot pre-launch pids for the PID fallback.
    let before = ps_app_pids(udid, app_name).await.unwrap_or_default();

    let mut cmd = Command::new("xcrun");
    cmd.args(["simctl", "launch", "--terminate-running-process"]);
    if wait_for_debugger {
        cmd.arg("--wait-for-debugger");
    }
    cmd.arg(format!("--stdout={}", stdout_file.display()));
    cmd.arg(format!("--stderr={}", stderr_file.display()));
    cmd.arg(udid).arg(bundle_id);
    let out = output_logged(&mut cmd, "xcrun simctl launch").await?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stderr = stderr.trim();
        // FBSOpenApplicationServiceErrorDomain (e.g. code 4: the installed
        // bundle is broken/stale) is usually fixed by a clean reinstall.
        let hint = if stderr.contains("FBSOpenApplicationServiceErrorDomain") {
            format!(
                "\nhint: the installed app looks stale or damaged — run \
                 `xcrun simctl uninstall {udid} {bundle_id}` and rerun \
                 (the next run reinstalls the app)"
            )
        } else {
            String::new()
        };
        bail!("simctl launch of {bundle_id} failed: {stderr}{hint}");
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    if let Some(pid) = parse_launch_pid(&stdout, bundle_id) {
        return Ok(pid);
    }
    // Fallback: poll ps for a pid that wasn't there before the launch.
    log::warn!(
        target: "simctl",
        "simctl launch printed no pid line — falling back to ps polling"
    );
    for iteration in 1..=5 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let now = ps_app_pids(udid, app_name).await.unwrap_or_default();
        if let Some(pid) = now.difference(&before).max() {
            log::warn!(
                target: "simctl",
                "pid {pid} found via ps fallback after {iteration} poll(s)"
            );
            return Ok(*pid);
        }
    }
    bail!(
        "could not determine the PID of {bundle_id} after launch \
         (simctl output: {:?})",
        stdout.trim()
    );
}

/// Parse "<bundle>: <pid>" from `simctl launch` stdout.
fn parse_launch_pid(stdout: &str, bundle_id: &str) -> Option<i64> {
    for line in stdout.lines() {
        if let Some(rest) = line.trim().strip_prefix(bundle_id) {
            if let Some(pid_str) = rest.strip_prefix(':') {
                if let Ok(pid) = pid_str.trim().parse::<i64>() {
                    return Some(pid);
                }
            }
        }
    }
    None
}

/// Pids of processes whose executable lives under this simulator's container
/// and inside `<app_name>.app/` (ps `comm` is the full executable path on
/// macOS).
async fn ps_app_pids(udid: &str, app_name: &str) -> anyhow::Result<HashSet<i64>> {
    let out = Command::new("ps")
        .args(["axww", "-o", "pid=,comm="])
        .kill_on_drop(true)
        .output()
        .await
        .context("running ps")?;
    let needle_dev = format!("CoreSimulator/Devices/{udid}/");
    let needle_app = format!("/{app_name}.app/");
    let mut pids = HashSet::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let line = line.trim_start();
        let Some((pid_str, path)) = line.split_once(' ') else {
            continue;
        };
        if path.contains(&needle_dev) && path.contains(&needle_app) {
            if let Ok(pid) = pid_str.trim().parse::<i64>() {
                pids.insert(pid);
            }
        }
    }
    Ok(pids)
}

/// `xcrun simctl terminate <udid> <bundle>` (callers may ignore failure).
pub async fn terminate(udid: &str, bundle_id: &str) -> anyhow::Result<()> {
    run_ok(
        Command::new("xcrun").args(["simctl", "terminate", udid, bundle_id]),
        "xcrun simctl terminate",
    )
    .await
}

/// Run a short helper command to completion, failing with its stderr.
async fn run_ok(cmd: &mut Command, what: &str) -> anyhow::Result<()> {
    let out = output_logged(cmd, what).await?;
    if !out.status.success() {
        bail!(
            "`{what}` failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Run `cmd` to completion, logging the full command, exit status and
/// duration at INFO (stderr at DEBUG on failure).
async fn output_logged(cmd: &mut Command, what: &str) -> anyhow::Result<std::process::Output> {
    let rendered = logging::describe_command(cmd);
    let started = std::time::Instant::now();
    let out = cmd
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("running `{what}`"))?;
    log::info!(
        target: "simctl",
        "{rendered} -> {} in {} ms",
        out.status,
        started.elapsed().as_millis()
    );
    if !out.status.success() {
        log::debug!(
            target: "simctl",
            "{what} stderr: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> Value {
        json!({
            "devices": {
                "com.apple.CoreSimulator.SimRuntime.iOS-18-2": [
                    { "udid": "AAAA-18", "name": "iPhone 15 Pro Max",
                      "state": "Shutdown", "isAvailable": true },
                    { "udid": "BBBB-18", "name": "iPhone SE (3rd generation)",
                      "state": "Booted", "isAvailable": true }
                ],
                "com.apple.CoreSimulator.SimRuntime.iOS-26-3": [
                    { "udid": "CCCC-26", "name": "iPhone 15 Pro Max",
                      "state": "Shutdown", "isAvailable": true },
                    { "udid": "DDDD-26", "name": "iPhone 16 Pro",
                      "state": "Shutdown", "isAvailable": false },
                    { "udid": "EEEE-26", "name": "iPhone 16 Pro",
                      "state": "Shutdown", "isAvailable": true }
                ],
                "com.apple.CoreSimulator.SimRuntime.watchOS-11-0": [
                    { "udid": "WWWW", "name": "Apple Watch Ultra 2 (49mm)",
                      "state": "Shutdown", "isAvailable": true }
                ]
            }
        })
    }

    #[test]
    fn name_match_prefers_newest_os() {
        // Same name in iOS 18.2 and 26.3, none booted -> newest OS wins.
        let udid = pick_device(&fixture(), Some("iPhone 15 Pro Max"), None).unwrap();
        assert_eq!(udid, "CCCC-26");
    }

    #[test]
    fn os_narrowing_selects_runtime() {
        let udid = pick_device(&fixture(), Some("iPhone 15 Pro Max"), Some("18.2")).unwrap();
        assert_eq!(udid, "AAAA-18");
    }

    #[test]
    fn udid_match_and_case_insensitive_name() {
        assert_eq!(
            pick_device(&fixture(), Some("aaaa-18"), None).unwrap(),
            "AAAA-18"
        );
        assert_eq!(
            pick_device(&fixture(), Some("iphone 16 pro"), None).unwrap(),
            "EEEE-26" // DDDD-26 is unavailable
        );
    }

    #[test]
    fn booted_query_matches_booted_device() {
        assert_eq!(
            pick_device(&fixture(), Some("booted"), None).unwrap(),
            "BBBB-18"
        );
    }

    #[test]
    fn booted_preferred_over_newer_os() {
        let mut v = fixture();
        // Boot the iOS 18.2 iPhone 15 Pro Max; it must win over the 26.3 one.
        v["devices"]["com.apple.CoreSimulator.SimRuntime.iOS-18-2"][0]["state"] = json!("Booted");
        assert_eq!(
            pick_device(&v, Some("iPhone 15 Pro Max"), None).unwrap(),
            "AAAA-18"
        );
    }

    #[test]
    fn boot_error_benign_for_already_booted_and_booting() {
        let msg = "An error was encountered processing the command \
                   (domain=com.apple.CoreSimulator.SimError, code=405):\n\
                   Unable to boot device in current state: Booted";
        assert!(boot_error_is_benign(msg));
        assert!(boot_error_is_benign(
            "Unable to boot device in current state: Booting"
        ));
    }

    #[test]
    fn boot_error_not_benign_otherwise() {
        assert!(!boot_error_is_benign("Invalid device: 1234"));
        // Shutting Down is retryable, not benign.
        assert!(!boot_error_is_benign(
            "Unable to boot device in current state: Shutting Down"
        ));
        assert!(!boot_error_is_benign(""));
    }

    #[test]
    fn boot_error_retryable_only_for_shutting_down() {
        let msg = "An error was encountered processing the command \
                   (domain=com.apple.CoreSimulator.SimError, code=405):\n\
                   Unable to boot device in current state: Shutting Down";
        assert!(boot_error_is_retryable(msg));
        assert!(!boot_error_is_retryable(
            "Unable to boot device in current state: Booted"
        ));
        assert!(!boot_error_is_retryable("Invalid device: 1234"));
        assert!(!boot_error_is_retryable(""));
    }

    #[test]
    fn no_match_lists_devices() {
        let err = pick_device(&fixture(), Some("iPhone 99"), None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no available simulator"));
        assert!(msg.contains("iPhone 15 Pro Max"));
    }

    #[test]
    fn missing_os_runtime_is_called_out() {
        // The os filter excludes every runtime: instead of an empty
        // "available devices" list, name the missing runtime.
        let err = pick_device(&fixture(), Some("iPhone 15 Pro Max"), Some("99.0")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no iOS 99.0 simulator runtime is installed"));
        assert!(!msg.contains("available devices"));
    }

    #[test]
    fn default_prefers_booted_iphone() {
        // No query: the booted iPhone SE (iOS 18.2) wins over every
        // shutdown iPhone on a newer OS.
        assert_eq!(pick_device(&fixture(), None, None).unwrap(), "BBBB-18");
    }

    #[test]
    fn default_none_booted_picks_newest_iphone() {
        let mut v = fixture();
        v["devices"]["com.apple.CoreSimulator.SimRuntime.iOS-18-2"][1]["state"] = json!("Shutdown");
        // Nothing booted -> newest OS (26.3); within it the name-descending
        // tie-break picks "iPhone 16 Pro" over "iPhone 15 Pro Max"
        // (DDDD-26 is unavailable, so EEEE-26).
        assert_eq!(pick_device(&v, None, None).unwrap(), "EEEE-26");
    }

    #[test]
    fn default_ignores_booted_non_iphone() {
        let mut v = fixture();
        v["devices"]["com.apple.CoreSimulator.SimRuntime.iOS-18-2"][1]["state"] = json!("Shutdown");
        // A booted iPad must not be picked by the iPhone default.
        v["devices"]["com.apple.CoreSimulator.SimRuntime.iOS-26-3"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "udid": "IPAD-26", "name": "iPad Pro 13-inch (M4)",
                          "state": "Booted", "isAvailable": true }));
        assert_eq!(pick_device(&v, None, None).unwrap(), "EEEE-26");
    }

    #[test]
    fn default_no_iphone_lists_devices() {
        let v = json!({
            "devices": {
                "com.apple.CoreSimulator.SimRuntime.iOS-26-3": [
                    { "udid": "IPAD-26", "name": "iPad Pro 13-inch (M4)",
                      "state": "Shutdown", "isAvailable": true }
                ]
            }
        });
        let err = pick_device(&v, None, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no \"device\" configured"));
        assert!(msg.contains("iPad Pro 13-inch (M4)"));
    }

    #[test]
    fn launch_pid_parsing() {
        assert_eq!(
            parse_launch_pid("com.example.myapp: 12345\n", "com.example.myapp"),
            Some(12345)
        );
        assert_eq!(
            parse_launch_pid("something else\n", "com.example.myapp"),
            None
        );
        // A different bundle's line must not match.
        assert_eq!(
            parse_launch_pid("com.example.myapp.widgets: 7\n", "com.example.myapp"),
            None
        );
    }

    #[test]
    fn runtime_version_parsing() {
        assert_eq!(
            runtime_version("com.apple.CoreSimulator.SimRuntime.iOS-26-3"),
            (26, 3)
        );
        assert_eq!(
            runtime_version("com.apple.CoreSimulator.SimRuntime.iOS-9-0"),
            (9, 0)
        );
    }
}
