//! simctl: the device list, boot, the simulator window, install, launch,
//! terminate, pid fallback, and the deadlines on those calls.
//! See `docs/design/dap-proxy.md` §4 (phases 2, 5-7).

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use anyhow::{bail, Context};
use serde_json::Value;
use tokio::process::Command;

use crate::engine::pipeline::OutputSink;
use crate::util::logging;

/// A simctl call that runs under a deadline. One that misses it is killed
/// (`kill_on_drop`) and fails with a message naming the step and its fix;
/// `boot` has its own budget in the pipeline (`BOOT_TIMEOUT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// `xcrun simctl list devices --json` (phase 2).
    List,
    /// `xcrun simctl install` (phase 5).
    Install,
    /// `xcrun simctl launch` (phase 6).
    Launch,
}

impl Step {
    fn deadline(self) -> Duration {
        Duration::from_secs(match self {
            Step::List => 30,
            Step::Install => 300,
            Step::Launch => 60,
        })
    }

    fn command(self) -> &'static str {
        match self {
            Step::List => "xcrun simctl list",
            Step::Install => "xcrun simctl install",
            Step::Launch => "xcrun simctl launch",
        }
    }

    /// What the user reads when the step misses its deadline. `udid` is the
    /// simulator the step works on (unused for `List`, which runs before one
    /// is picked).
    fn timeout_message(self, udid: &str) -> String {
        let fix = match self {
            // A hung list means CoreSimulator itself is stuck, not one device;
            // launchd starts the service again on the next simctl call.
            Step::List => "the CoreSimulator service may be stuck: run \
                 \"killall -9 com.apple.CoreSimulator.CoreSimulatorService\" and press ⌘R again."
                .to_string(),
            Step::Install | Step::Launch => format!(
                "the simulator may be wedged: run \"xcrun simctl shutdown {udid}\" and press ⌘R again."
            ),
        };
        format!(
            "{} did not finish in {} s; {fix}",
            self.command(),
            self.deadline().as_secs()
        )
    }
}

/// Deadline for the quick helpers around the simulator window and the
/// first-launch check (`xcode-select -p`, `xcodebuild
/// -checkFirstLaunchStatus`): each answers in a second or two, and a hung one
/// gives up long before the boot phase's budget runs out.
const HELPER_DEADLINE: Duration = Duration::from_secs(15);

/// Deadline for one `open` of the simulator window. The first launch of an
/// app inside a freshly installed Xcode can take a minute while macOS
/// verifies the bundle; the window opens concurrently with the `bootstatus`
/// wait, so this patience costs a run nothing in the common case.
const WINDOW_OPEN_DEADLINE: Duration = Duration::from_secs(60);

/// `xcrun simctl list devices --json`, parsed, under the list deadline.
/// [`crate::engine::destinations`] turns it into the simulator inventory.
pub async fn list_devices_json() -> anyhow::Result<Value> {
    let mut cmd = Command::new("xcrun");
    cmd.args(["simctl", "list", "devices", "--json"]);
    let out = output_step(&mut cmd, "xcrun simctl list devices --json", Step::List, "").await?;
    if !out.status.success() {
        bail!(
            "`xcrun simctl list devices --json` failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    serde_json::from_slice(&out.stdout).context("parsing simctl device list JSON")
}

/// Time budget for retrying a `simctl boot` racing a shutdown in flight
/// ("Unable to boot device in current state: Shutting Down"). A real
/// simulator shutdown takes 10-30 s on a loaded machine (the typical
/// trigger: quitting Simulator.app and immediately rerunning), so the
/// budget must cover that — a handful of attempts would give up too early.
const BOOT_RETRY_BUDGET: Duration = Duration::from_secs(30);
const BOOT_RETRY_DELAY: Duration = Duration::from_secs(2);

/// `xcrun simctl boot <udid>` (tolerating "already booted/booting",
/// retrying a "Shutting Down" race), then the simulator window
/// ([`open_simulator_window`], never fatal) concurrently with `xcrun simctl
/// bootstatus <udid>` (blocks until ready, no-op when already booted).
///
/// The window opens only after `simctl boot`: opening it first lets it
/// auto-boot the same device concurrently, making a `bootstatus -b` inner
/// boot fail with SimError 405 "Unable to boot device in current state:
/// Booted".
pub async fn boot(udid: &str, sink: &dyn OutputSink) -> anyhow::Result<()> {
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
    // The window opens while `bootstatus` waits: it is cosmetic, and a slow
    // first launch of the window app must not hold up the boot.
    let mut bootstatus = Command::new("xcrun");
    bootstatus.args(["simctl", "bootstatus", udid]);
    let ((), status) = tokio::join!(
        open_simulator_window(sink),
        run_ok(&mut bootstatus, "xcrun simctl bootstatus"),
    );
    status
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

/// The developer dir of the selected Xcode: `DEVELOPER_DIR` when set, else
/// `xcode-select -p`. `None` when neither names one.
async fn developer_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("DEVELOPER_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    let mut cmd = Command::new("xcode-select");
    cmd.arg("-p");
    let out = output_within(&mut cmd, "xcode-select -p", HELPER_DEADLINE)
        .await
        .ok()??;
    if !out.status.success() {
        return None;
    }
    let dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!dir.is_empty()).then(|| PathBuf::from(dir))
}

/// The apps that show the simulator for the developer dir `dev`, in the
/// order to try: `<dev>/Applications/Simulator.app` (Xcode 26 and earlier),
/// then `<dev>/../Applications/DeviceHub.app` (Xcode 27, where Device Hub
/// replaced Simulator). Only bundles that exist are listed; when none opens,
/// [`open_simulator_window`] falls back to `open -a Simulator`. `dev` may
/// also name the Xcode app itself, which `DEVELOPER_DIR` accepts too.
fn simulator_window_candidates(dev: &Path) -> Vec<PathBuf> {
    let inside_app = dev.join("Contents").join("Developer");
    let dev = if inside_app.is_dir() {
        inside_app
    } else {
        dev.to_path_buf()
    };
    // `<dev>/..` without the `..` in the path the log line shows.
    let contents = match dev.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => dev.join(".."),
    };
    [
        dev.join("Applications").join("Simulator.app"),
        contents.join("Applications").join("DeviceHub.app"),
    ]
    .into_iter()
    .filter(|app| app.is_dir())
    .collect()
}

/// Bring up the window that shows the booted simulator: the first of
/// [`simulator_window_candidates`] that opens, else `open -a Simulator` (any
/// Simulator.app Launch Services knows, e.g. an older Xcode next to this
/// one). Without a window the simulator keeps running headless and the run
/// goes on, so a failure is a warning plus one console line, never an error.
async fn open_simulator_window(sink: &dyn OutputSink) {
    let candidates = match developer_dir().await {
        Some(dev) => simulator_window_candidates(&dev),
        None => {
            log::info!(
                target: "simctl",
                "simulator window: no developer dir (DEVELOPER_DIR unset, xcode-select -p \
                 gave none); trying open -a Simulator"
            );
            Vec::new()
        }
    };
    // The reason the preferred app did not open is the one worth showing.
    let mut first_error: Option<String> = None;
    for app in &candidates {
        log::info!(target: "simctl", "simulator window: opening {}", app.display());
        let mut cmd = Command::new("open");
        cmd.arg(app);
        match open_app(&mut cmd, "open <simulator app>").await {
            Ok(()) => {
                log::info!(target: "simctl", "simulator window: opened {}", app.display());
                return;
            }
            Err(OpenError::NoAnswer(e)) => {
                // Still launching, most likely: trying the next app now could
                // open a second window app next to it.
                log::warn!(
                    target: "simctl",
                    "simulator window: {} did not answer in {} s; it may still be starting",
                    app.display(),
                    WINDOW_OPEN_DEADLINE.as_secs()
                );
                let line = window_failure_message(&e);
                sink.line("console", &line);
                return;
            }
            Err(OpenError::Failed(e)) => {
                // A warning even when a later app opens: on Xcode 27 an older
                // Simulator.app reached through `open -a Simulator` hides a
                // Device Hub that does not open.
                log::warn!(
                    target: "simctl",
                    "simulator window: could not open {}: {e}",
                    app.display()
                );
                first_error.get_or_insert(e);
            }
        }
    }
    let mut cmd = Command::new("open");
    cmd.args(["-a", "Simulator"]);
    match open_app(&mut cmd, "open -a Simulator").await {
        Ok(()) => log::info!(
            target: "simctl",
            "simulator window: opened Simulator (open -a Simulator)"
        ),
        Err(OpenError::NoAnswer(e) | OpenError::Failed(e)) => {
            let line = window_failure_message(first_error.as_deref().unwrap_or(&e));
            log::warn!(target: "simctl", "{line}");
            sink.line("console", &line);
        }
    }
}

/// Why one `open` of the simulator window did not succeed, as a one-line
/// reason.
enum OpenError {
    /// `open` did not return within [`WINDOW_OPEN_DEADLINE`].
    NoAnswer(String),
    /// `open` failed or could not be run.
    Failed(String),
}

/// Run one `open` for the simulator window.
async fn open_app(cmd: &mut Command, what: &str) -> Result<(), OpenError> {
    match output_within(cmd, what, WINDOW_OPEN_DEADLINE).await {
        Ok(Some(out)) if out.status.success() => Ok(()),
        Ok(Some(out)) => Err(OpenError::Failed(failure_reason(
            &out.status.to_string(),
            &String::from_utf8_lossy(&out.stderr),
        ))),
        Ok(None) => Err(OpenError::NoAnswer(format!(
            "no answer in {} s",
            WINDOW_OPEN_DEADLINE.as_secs()
        ))),
        Err(e) => Err(OpenError::Failed(failure_reason(&format!("{e:#}"), ""))),
    }
}

/// `stderr` (or `status` when stderr is empty) on one line.
fn failure_reason(status: &str, stderr: &str) -> String {
    let stderr = stderr.split_whitespace().collect::<Vec<_>>().join(" ");
    if stderr.is_empty() {
        status.split_whitespace().collect::<Vec<_>>().join(" ")
    } else {
        stderr
    }
}

/// The console line for a simulator window that did not open.
fn window_failure_message(err: &str) -> String {
    format!(
        "Could not open the simulator window ({err}); the simulator keeps running headless. \
         Open Device Hub (Xcode 27) or Simulator (Xcode 26) to see it."
    )
}

/// `xcrun simctl install <udid> <app>`.
pub async fn install(udid: &str, app: &Path) -> anyhow::Result<()> {
    let mut cmd = Command::new("xcrun");
    cmd.args(["simctl", "install", udid]).arg(app);
    let out = output_step(&mut cmd, "xcrun simctl install", Step::Install, udid).await?;
    ensure_success(&out, "xcrun simctl install").with_context(|| {
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
/// pre-truncated by the pipeline before launch. `env` is the app's launch
/// environment, plus `NSUnbufferedIO=YES` unless it sets that itself
/// ([`simctl_child_env`]).
pub async fn launch(
    udid: &str,
    bundle_id: &str,
    app_name: &str,
    wait_for_debugger: bool,
    env: &BTreeMap<String, String>,
    stdout_file: &Path,
    stderr_file: &Path,
) -> anyhow::Result<i64> {
    // Snapshot pre-launch pids for the PID fallback.
    let before = ps_app_pids(udid, app_name).await.unwrap_or_default();

    let mut cmd = launch_command(
        udid,
        bundle_id,
        wait_for_debugger,
        env,
        stdout_file,
        stderr_file,
    );
    // Keys only: environment values are never logged.
    let keys: Vec<&str> = cmd
        .as_std()
        .get_envs()
        .filter_map(|(key, _)| key.to_str()?.strip_prefix(SIMCTL_CHILD_PREFIX))
        .collect();
    log::info!(target: "simctl", "launch environment: {}", keys.join(", "));
    let out = output_step(&mut cmd, "xcrun simctl launch", Step::Launch, udid).await?;
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

/// simctl hands every `SIMCTL_CHILD_<KEY>` variable of its own environment to
/// the app it launches as `<KEY>`.
const SIMCTL_CHILD_PREFIX: &str = "SIMCTL_CHILD_";

/// Foundation's switch for an unbuffered stdout. The app writes its stdout to
/// the `--stdout` file, not to a terminal, so without it every `print()` line
/// waits in stdio's block buffer and reaches out.log, and the Debug Console,
/// only once that buffer fills.
const UNBUFFERED_IO: &str = "NSUnbufferedIO";

/// The variables to set on the `simctl launch` process so that the app starts
/// with the launch environment `env`: each entry as `SIMCTL_CHILD_<KEY>`, and
/// `NSUnbufferedIO=YES` unless `env` sets `NSUnbufferedIO` itself (its own
/// value wins, whatever it is).
fn simctl_child_env(env: &BTreeMap<String, String>) -> Vec<(String, String)> {
    let mut vars: Vec<(String, String)> = env
        .iter()
        .map(|(key, value)| (format!("{SIMCTL_CHILD_PREFIX}{key}"), value.clone()))
        .collect();
    if !env.contains_key(UNBUFFERED_IO) {
        vars.push((
            format!("{SIMCTL_CHILD_PREFIX}{UNBUFFERED_IO}"),
            "YES".to_string(),
        ));
    }
    vars
}

/// The `simctl launch` command for [`launch`], with the launch environment
/// `env` applied ([`simctl_child_env`]).
fn launch_command(
    udid: &str,
    bundle_id: &str,
    wait_for_debugger: bool,
    env: &BTreeMap<String, String>,
    stdout_file: &Path,
    stderr_file: &Path,
) -> Command {
    let mut cmd = Command::new("xcrun");
    cmd.args(["simctl", "launch", "--terminate-running-process"]);
    if wait_for_debugger {
        cmd.arg("--wait-for-debugger");
    }
    cmd.arg(format!("--stdout={}", stdout_file.display()));
    cmd.arg(format!("--stderr={}", stderr_file.display()));
    cmd.arg(udid).arg(bundle_id);
    cmd.envs(simctl_child_env(env));
    cmd
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
        .stdin(Stdio::null())
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
    ensure_success(&out, what)
}

/// Fail with the command's stderr unless it exited 0.
fn ensure_success(out: &Output, what: &str) -> anyhow::Result<()> {
    if !out.status.success() {
        bail!(
            "`{what}` failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Run a simctl `step` under its deadline. A missed deadline becomes the
/// step's timeout message, plus the first-launch line when that is the
/// likely cause ([`timed_out`]).
async fn output_step(
    cmd: &mut Command,
    what: &str,
    step: Step,
    udid: &str,
) -> anyhow::Result<Output> {
    match output_within(cmd, what, step.deadline()).await? {
        Some(out) => Ok(out),
        None => Err(timed_out(step.timeout_message(udid)).await),
    }
}

/// [`output_logged`] under a deadline: `Ok(None)` when it passed, after the
/// child was killed (`kill_on_drop` fires when the timed-out future drops).
async fn output_within(
    cmd: &mut Command,
    what: &str,
    deadline: Duration,
) -> anyhow::Result<Option<Output>> {
    match tokio::time::timeout(deadline, output_logged(cmd, what)).await {
        Ok(out) => out.map(Some),
        Err(_) => {
            log::warn!(
                target: "simctl",
                "`{what}` did not finish in {} s; killed it",
                deadline.as_secs()
            );
            Ok(None)
        }
    }
}

/// The error for a simulator step that missed its deadline: `message` (the
/// step and its fix), followed by the first-launch line when `xcodebuild
/// -checkFirstLaunchStatus` reports Xcode's components missing — simctl can
/// hang on an Xcode whose first launch never finished.
pub async fn timed_out(message: String) -> anyhow::Error {
    let first_launch = first_launch_note().await;
    anyhow::anyhow!(compose_timeout_error(&message, first_launch.as_deref()))
}

fn compose_timeout_error(message: &str, first_launch: Option<&str>) -> String {
    match first_launch {
        Some(note) => format!("{message}\n{note}"),
        None => message.to_string(),
    }
}

/// `xcodebuild -checkFirstLaunchStatus` -> the first-launch line, or `None`
/// when the components are installed or the check itself gave no answer.
async fn first_launch_note() -> Option<String> {
    let mut cmd = Command::new("xcodebuild");
    cmd.arg("-checkFirstLaunchStatus");
    let code = match output_within(
        &mut cmd,
        "xcodebuild -checkFirstLaunchStatus",
        HELPER_DEADLINE,
    )
    .await
    {
        Ok(Some(out)) => out.status.code(),
        _ => None,
    };
    if !first_launch_incomplete(code) {
        return None;
    }
    let dev = developer_dir().await;
    Some(first_launch_message(dev.as_deref()))
}

/// `-checkFirstLaunchStatus` exits non-zero while the first launch is
/// outstanding. No exit code (it could not run, timed out or was killed)
/// proves nothing either way.
fn first_launch_incomplete(exit_code: Option<i32>) -> bool {
    matches!(exit_code, Some(code) if code != 0)
}

fn first_launch_message(dev: Option<&Path>) -> String {
    let dev = dev
        .map(|d| d.display().to_string())
        .unwrap_or_else(|| "the selected Xcode".to_string());
    format!(
        "Xcode's components are not fully installed for {dev}: run \
         \"xcodebuild -runFirstLaunch\" (or open Xcode once), then retry."
    )
}

/// Run `cmd` to completion, logging the full command, exit status and
/// duration at INFO (stderr at DEBUG on failure). stdout and stderr are
/// captured; stdin is closed, because in DAP mode the inherited stdin is
/// Zed's request stream.
async fn output_logged(cmd: &mut Command, what: &str) -> anyhow::Result<std::process::Output> {
    let rendered = logging::describe_command(cmd);
    let started = std::time::Instant::now();
    let out = cmd
        .stdin(Stdio::null())
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
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn sandbox() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zedxcode-simctl-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// `<root>/Xcode.app/Contents` with the given bundles under it (paths
    /// relative to `Contents`); returns the developer dir inside it.
    fn xcode_layout(root: &Path, bundles: &[&str]) -> PathBuf {
        let contents = root.join("Xcode.app").join("Contents");
        let dev = contents.join("Developer");
        fs::create_dir_all(&dev).unwrap();
        for bundle in bundles {
            fs::create_dir_all(contents.join(bundle)).unwrap();
        }
        dev
    }

    #[test]
    fn window_candidates_xcode_26_layout() {
        let root = sandbox();
        let dev = xcode_layout(&root, &["Developer/Applications/Simulator.app"]);
        assert_eq!(
            simulator_window_candidates(&dev),
            vec![dev.join("Applications/Simulator.app")]
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn window_candidates_xcode_27_layout() {
        let root = sandbox();
        let dev = xcode_layout(&root, &["Applications/DeviceHub.app"]);
        // `<dev>/../Applications/DeviceHub.app`, shown without the `..`.
        assert_eq!(
            simulator_window_candidates(&dev),
            vec![root.join("Xcode.app/Contents/Applications/DeviceHub.app")]
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn window_candidates_keep_simulator_before_device_hub() {
        let root = sandbox();
        let dev = xcode_layout(
            &root,
            &[
                "Applications/DeviceHub.app",
                "Developer/Applications/Simulator.app",
            ],
        );
        assert_eq!(
            simulator_window_candidates(&dev),
            vec![
                dev.join("Applications/Simulator.app"),
                root.join("Xcode.app/Contents/Applications/DeviceHub.app"),
            ]
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn window_candidates_empty_without_either_app() {
        let root = sandbox();
        let dev = xcode_layout(&root, &[]);
        assert!(simulator_window_candidates(&dev).is_empty());
        // A developer dir that does not exist at all (a stale DEVELOPER_DIR).
        assert!(simulator_window_candidates(&root.join("missing/Developer")).is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn window_candidates_accept_the_xcode_app_as_developer_dir() {
        // DEVELOPER_DIR may name the app bundle instead of Contents/Developer.
        let root = sandbox();
        xcode_layout(&root, &["Applications/DeviceHub.app"]);
        assert_eq!(
            simulator_window_candidates(&root.join("Xcode.app")),
            vec![root.join("Xcode.app/Contents/Applications/DeviceHub.app")]
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn window_failure_message_reads_as_one_console_line() {
        assert_eq!(
            window_failure_message("Unable to find application named 'Simulator'"),
            "Could not open the simulator window (Unable to find application named \
             'Simulator'); the simulator keeps running headless. Open Device Hub (Xcode 27) \
             or Simulator (Xcode 26) to see it."
        );
    }

    #[test]
    fn failure_reason_is_one_line() {
        // stderr wins, whitespace and newlines collapsed.
        assert_eq!(
            failure_reason(
                "exit status: 1",
                "The application cannot be opened\n  for an unexpected reason\n"
            ),
            "The application cannot be opened for an unexpected reason"
        );
        // Empty stderr: the exit status.
        assert_eq!(failure_reason("exit status: 1", "  \n"), "exit status: 1");
    }

    #[test]
    fn step_deadlines() {
        assert_eq!(Step::List.deadline(), Duration::from_secs(30));
        assert_eq!(Step::Install.deadline(), Duration::from_secs(300));
        assert_eq!(Step::Launch.deadline(), Duration::from_secs(60));
    }

    #[test]
    fn step_timeout_messages_name_the_step_and_the_fix() {
        let udid = "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE";
        assert_eq!(
            Step::Install.timeout_message(udid),
            "xcrun simctl install did not finish in 300 s; the simulator may be wedged: \
             run \"xcrun simctl shutdown AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE\" and press \
             ⌘R again."
        );
        assert_eq!(
            Step::Launch.timeout_message(udid),
            "xcrun simctl launch did not finish in 60 s; the simulator may be wedged: \
             run \"xcrun simctl shutdown AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE\" and press \
             ⌘R again."
        );
        let list = Step::List.timeout_message("");
        assert!(list.starts_with("xcrun simctl list did not finish in 30 s; "));
        assert!(list.contains("CoreSimulatorService"));
        assert!(list.ends_with("press ⌘R again."));
    }

    #[test]
    fn first_launch_classification() {
        // Exit 0: the components are installed.
        assert!(!first_launch_incomplete(Some(0)));
        // Any other exit code: the first launch is outstanding.
        assert!(first_launch_incomplete(Some(1)));
        assert!(first_launch_incomplete(Some(69)));
        // No exit code (not runnable, timed out, killed): no claim.
        assert!(!first_launch_incomplete(None));
    }

    #[test]
    fn first_launch_message_names_the_developer_dir() {
        assert_eq!(
            first_launch_message(Some(Path::new(
                "/Applications/Xcode-27.0.app/Contents/Developer"
            ))),
            "Xcode's components are not fully installed for \
             /Applications/Xcode-27.0.app/Contents/Developer: run \
             \"xcodebuild -runFirstLaunch\" (or open Xcode once), then retry."
        );
        assert!(first_launch_message(None).contains("for the selected Xcode: run"));
    }

    #[test]
    fn timeout_error_appends_the_first_launch_line() {
        let step = "xcrun simctl launch did not finish in 60 s; the simulator may be wedged.";
        assert_eq!(compose_timeout_error(step, None), step);
        assert_eq!(
            compose_timeout_error(step, Some("Xcode's components are not fully installed")),
            format!("{step}\nXcode's components are not fully installed")
        );
    }

    #[tokio::test]
    async fn output_within_kills_the_command_at_the_deadline() {
        let root = sandbox();
        let pidfile = root.join("pid");
        let started = std::time::Instant::now();
        // `exec` keeps the shell's pid, so the file names the sleep itself.
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo $$ >\"$1\"; exec sleep 30", "sh"])
            .arg(&pidfile);
        let out = output_within(&mut cmd, "sleep 30", Duration::from_secs(1))
            .await
            .unwrap();
        assert!(out.is_none(), "a missed deadline reads as None");
        assert!(started.elapsed() < Duration::from_secs(10));

        // kill_on_drop sent SIGKILL: the sleep is gone, or a zombie until the
        // runtime reaps it.
        let pid = fs::read_to_string(&pidfile).unwrap().trim().to_string();
        let give_up = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let ps = std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", &pid])
                .output()
                .unwrap();
            let stat = String::from_utf8_lossy(&ps.stdout).trim().to_string();
            if stat.is_empty() || stat.starts_with('Z') {
                break;
            }
            assert!(
                std::time::Instant::now() < give_up,
                "the timed-out command (pid {pid}) is still running ({stat})"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn output_within_returns_a_finished_command() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo done; exit 3"]);
        let out = output_within(&mut cmd, "sh -c", Duration::from_secs(30))
            .await
            .unwrap()
            .expect("finished before the deadline");
        assert_eq!(out.status.code(), Some(3));
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "done");
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

    fn child_env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        child_env(pairs).into_iter().collect()
    }

    #[test]
    fn child_env_adds_unbuffered_io_to_an_empty_environment() {
        assert_eq!(
            simctl_child_env(&BTreeMap::new()),
            child_env(&[("SIMCTL_CHILD_NSUnbufferedIO", "YES")])
        );
    }

    #[test]
    fn child_env_prefixes_every_entry_and_adds_unbuffered_io() {
        assert_eq!(
            simctl_child_env(&env(&[("MYAPP_FLAG", "1"), ("API_HOST", "example.com")])),
            child_env(&[
                ("SIMCTL_CHILD_API_HOST", "example.com"),
                ("SIMCTL_CHILD_MYAPP_FLAG", "1"),
                ("SIMCTL_CHILD_NSUnbufferedIO", "YES"),
            ])
        );
    }

    #[test]
    fn child_env_keeps_an_unbuffered_io_the_environment_sets() {
        // Its own value wins, NO included; no second entry is added.
        assert_eq!(
            simctl_child_env(&env(&[("NSUnbufferedIO", "NO"), ("MYAPP_FLAG", "1")])),
            child_env(&[
                ("SIMCTL_CHILD_MYAPP_FLAG", "1"),
                ("SIMCTL_CHILD_NSUnbufferedIO", "NO"),
            ])
        );
        assert_eq!(
            simctl_child_env(&env(&[("NSUnbufferedIO", "")])),
            child_env(&[("SIMCTL_CHILD_NSUnbufferedIO", "")])
        );
    }

    #[test]
    fn child_env_matches_the_unbuffered_io_name_exactly() {
        // Environment names are case-sensitive: another spelling is just
        // another variable, and the launch still adds its own.
        assert_eq!(
            simctl_child_env(&env(&[("NSUNBUFFEREDIO", "NO")])),
            child_env(&[
                ("SIMCTL_CHILD_NSUNBUFFEREDIO", "NO"),
                ("SIMCTL_CHILD_NSUnbufferedIO", "YES"),
            ])
        );
    }

    #[test]
    fn launch_command_applies_the_launch_environment() {
        let udid = "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE";
        let run = format!("/Users/Jane/.zedxcode/run/{udid}");
        let cmd = launch_command(
            udid,
            "com.example.MyApp",
            true,
            &env(&[("MYAPP_FLAG", "1")]),
            &Path::new(&run).join("out.log"),
            &Path::new(&run).join("err.log"),
        );
        let std_cmd = cmd.as_std();
        assert_eq!(std_cmd.get_program(), "xcrun");
        let args: Vec<String> = std_cmd
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "simctl".to_string(),
                "launch".to_string(),
                "--terminate-running-process".to_string(),
                "--wait-for-debugger".to_string(),
                format!("--stdout={run}/out.log"),
                format!("--stderr={run}/err.log"),
                udid.to_string(),
                "com.example.MyApp".to_string(),
            ]
        );
        // The environment rides on the simctl process, never on its arguments.
        let envs: Vec<(String, Option<String>)> = std_cmd
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert_eq!(
            envs,
            [
                ("SIMCTL_CHILD_MYAPP_FLAG".to_string(), Some("1".to_string())),
                (
                    "SIMCTL_CHILD_NSUnbufferedIO".to_string(),
                    Some("YES".to_string())
                ),
            ]
        );
    }

    #[test]
    fn launch_command_without_the_debugger_wait() {
        let cmd = launch_command(
            "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE",
            "com.example.MyApp",
            false,
            &BTreeMap::new(),
            Path::new("/Users/x/out.log"),
            Path::new("/Users/x/err.log"),
        );
        let std_cmd = cmd.as_std();
        assert!(!std_cmd.get_args().any(|arg| arg == "--wait-for-debugger"));
        let envs: Vec<_> = std_cmd.get_envs().collect();
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0].0, "SIMCTL_CHILD_NSUnbufferedIO");
        assert_eq!(envs[0].1, Some(std::ffi::OsStr::new("YES")));
    }
}
