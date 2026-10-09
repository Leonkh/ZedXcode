//! preflight -> build -> install -> launch -> pid pipeline, shared by
//! dap mode and the CLI. See `docs/design/dap-proxy.md` §4.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::engine::destinations::{self, Device};
use crate::engine::project::{self, Project};
use crate::engine::selection::{self, Options, Request};
use crate::engine::xcodebuild::Target;
use crate::engine::{compile_store, schemes, simctl, xcactivitylog, xcodebuild};
use crate::setup::build_server::{write_build_server_json, Change};
use crate::setup::project::{build_server_opted_in, git_exclude_build_server};
use crate::util::paths::{buildserver_stale, mtime, workspace_mtime};
use crate::util::procgroup;

// Re-exported from `util::paths` so callers across `commands/` and `dap/`
// can keep importing it from the pipeline module.
pub use crate::util::paths::zedxcode_home;

/// Generous cap on the whole boot phase (`simctl boot` + retries + the
/// simulator window + `bootstatus` wait): a cold boot finishes in well
/// under a minute, so hitting this means `bootstatus` is wedged and the
/// device needs a manual `simctl shutdown` instead of an endless wait.
const BOOT_TIMEOUT: Duration = Duration::from_secs(240);

/// A debug launch cancelled mid-way first gets this long to finish: killing
/// the `simctl launch` client does not withdraw a launch the simulator has
/// already accepted, so a terminate sent at once could run before the app
/// exists and leave it suspended under `--wait-for-debugger`.
const CANCELLED_LAUNCH_SETTLE: Duration = Duration::from_secs(1);

/// Then this long to terminate the app it may have started (pid possibly
/// not yet handed back). Both bounded so a wedged simctl cannot delay the
/// Stop: together they stay under the DAP proxy's wait for a cancelled
/// pipeline (4 s).
const CANCELLED_LAUNCH_TERMINATE_GRACE: Duration = Duration::from_secs(2);

/// Where pipeline output lines go: dap mode -> DAP `output` events;
/// CLI mode -> plain stderr/stdout.
pub trait OutputSink: Send + Sync {
    /// Emit one line; `category` is a DAP output category
    /// (`"console"`, `"stdout"`, `"stderr"`) or one of the internal
    /// sub-categories `"build"` / `"oslog"` / `"preflight"` (emitted to
    /// DAP as `"console"`, but kept out of xcode-dap.log at INFO). `text`
    /// has no trailing newline but may contain embedded newlines (batched
    /// build output arrives newline-joined, one call per batch).
    fn line(&self, category: &str, text: &str);
}

/// Result of a successful pipeline run.
#[derive(Debug)]
pub struct LaunchedApp {
    pub pid: i64,
    pub udid: String,
    pub bundle_id: String,
    pub app_path: PathBuf,
    pub stdout_file: PathBuf,
    pub stderr_file: PathBuf,
}

/// What the pipeline works on once a [`Request`] is resolved and validated.
#[derive(Debug)]
pub struct Prepared {
    pub target: Target,
    /// The simulator; `None` when the caller did not ask for one (clean).
    pub device: Option<Device>,
}

/// Resolve `req` (the project's container, the selection layers), run the
/// scenario's preflight when the container is missing, then settle the
/// scheme, the configuration and, with `destination`, the simulator: all
/// three after any generating preflight and before anything boots. One
/// `Scheme: … | Destination: … | Configuration: …` line goes to `sink`.
pub async fn prepare(
    req: &Request,
    sink: &dyn OutputSink,
    cancel: &CancellationToken,
    destination: bool,
) -> anyhow::Result<Prepared> {
    let resolution = selection::resolve(req)?;
    for warning in &resolution.picks.warnings {
        sink.line("console", warning);
    }
    let picks = &resolution.picks;
    let container = ensure_container(&resolution.project, &req.options, sink, cancel).await?;

    // The scheme list is cached per container; a cache miss runs
    // `xcodebuild -list`, which a Stop must not wait out.
    let list = unless_cancelled(
        cancel,
        "reading the scheme list",
        schemes::list(&container, sink),
    )
    .await;
    let list = match list {
        Ok(list) => Some(list),
        // A chosen scheme still builds when the list cannot be read:
        // xcodebuild reports a wrong one itself.
        Err(e) if picks.scheme.is_some() && !cancel.is_cancelled() => {
            log::warn!(target: "pipeline", "scheme list unavailable: {e:#}");
            sink.line(
                "console",
                &format!("Could not read the scheme list, so the scheme is not checked: {e:#}"),
            );
            None
        }
        Err(e) => return Err(e),
    };
    let (scheme, configuration) = match &list {
        Some(list) => (
            selection::settle_scheme(picks.scheme.as_ref(), list, &container)?,
            selection::settle_configuration(picks.configuration.as_ref(), list, &container)?,
        ),
        None => (
            picks.scheme.clone().expect("checked above"),
            picks.configuration.clone(),
        ),
    };

    // `simctl list` hangs while CoreSimulatorService restarts; a Stop must
    // not wait for it.
    let device = if destination {
        let inventory =
            unless_cancelled(cancel, "resolving the simulator", destinations::inventory()).await?;
        let (device, warnings) =
            selection::settle_destination(picks.destination.as_ref(), &inventory)?;
        for warning in &warnings {
            sink.line("console", warning);
        }
        Some(device)
    } else {
        None
    };

    let summary = selection::summary(&scheme, device.as_ref(), configuration.as_ref());
    log::info!(target: "pipeline", "{summary}");
    sink.line("console", &summary);
    Ok(Prepared {
        target: Target {
            workspace: container,
            scheme: scheme.value,
            configuration: configuration.map(|c| c.value),
            derived_data: req.options.derived_data.clone(),
            build_output: req.options.build_output,
        },
        device: device.map(|d| d.value),
    })
}

/// Phases 1-4: resolve and validate (with the preflight), then build and
/// locate the app. Returns `(udid, app_path)`.
///
/// This is the whole `xcode-dap build` command (`boot: false` — building
/// for a `-destination ...,id=<udid>` does not require a booted device).
pub async fn run_build(
    req: &Request,
    sink: &dyn OutputSink,
    cancel: CancellationToken,
) -> anyhow::Result<(String, PathBuf)> {
    // The selection store is re-read on every entry, so a new pick applies
    // to the very next build without touching .zed/debug.json or tasks.json.
    let prepared = prepare(req, sink, &cancel, true).await?;
    build_phases(&prepared, sink, cancel, false).await
}

async fn build_phases(
    prepared: &Prepared,
    sink: &dyn OutputSink,
    cancel: CancellationToken,
    boot: bool,
) -> anyhow::Result<(String, PathBuf)> {
    let cfg = &prepared.target;
    let udid = prepared
        .device
        .as_ref()
        .map(|d| d.udid.clone())
        .context("no simulator was resolved")?;
    sink.line("console", &format!("Simulator: {udid}"));

    // Phase 2: visible pre-boot for run/debug.
    if boot {
        sink.line("console", "Booting simulator (visible)...");
        tokio::select! {
            r = tokio::time::timeout(BOOT_TIMEOUT, simctl::boot(&udid, sink)) => match r {
                Ok(r) => r?,
                Err(_) => {
                    // The first-launch check behind the message spawns
                    // xcodebuild, so a Stop must still cut it short.
                    let error = simctl::timed_out(format!(
                        "the simulator did not finish booting in {} s; it may be wedged: \
                         run \"xcrun simctl shutdown {udid}\" and press ⌘R again.",
                        BOOT_TIMEOUT.as_secs()
                    ));
                    tokio::select! {
                        e = error => return Err(e),
                        _ = cancel.cancelled() => bail!("cancelled while booting simulator"),
                    }
                }
            },
            _ = cancel.cancelled() => bail!("cancelled while booting simulator"),
        }
    }
    if cancel.is_cancelled() {
        bail!("cancelled");
    }

    // Keep buildServer.json fresh before building (go-to-definition
    // durability; consuming repos' clean scripts delete it).
    ensure_build_server(cfg, sink, &cancel).await?;

    // Phase 3: build.
    sink.line("console", &format!("Building scheme \"{}\"...", cfg.scheme));
    xcodebuild::build(cfg, &udid, sink, cancel.clone()).await?;

    // Phase 4: locate the .app product (a cache miss runs
    // `xcodebuild -showBuildSettings`, which a Stop must not wait out).
    let app = unless_cancelled(
        &cancel,
        "locating the app",
        xcodebuild::app_path(cfg, &udid),
    )
    .await?;
    sink.line("console", &format!("App: {}", app.display()));

    // Feed the compile-args store from the just-captured build log so
    // `xcode-dap bsp` serves go-to-definition for CLI builds too (Xcode 26.3
    // xcodebuild writes no `.xcactivitylog` into an existing DerivedData).
    ingest_build_log(cfg, &app);

    Ok((udid, app))
}

/// After a successful build, fold the just-captured xcodebuild stdout
/// (`~/.zedxcode/logs/build-latest.log`) into the compile-args store for
/// `(build_root, scheme)`. The bsp poll loop's `.xcactivitylog` path only
/// covers Xcode.app builds; this covers `xcode-dap build`/`run`. Gated by the
/// same opt-in as the buildServer regen (never create a store for a repo that
/// never configured this adapter) and best-effort — it never fails the build.
fn ingest_build_log(cfg: &Target, app: &Path) {
    let Ok(ws) = std::path::absolute(&cfg.workspace) else {
        return;
    };
    let Some(dir) = ws.parent() else {
        return;
    };
    if !build_server_opted_in(dir, &dir.join("buildServer.json")) {
        return;
    }
    let Some(build_root) = xcodebuild::build_root_from_app(cfg.derived_data.as_deref(), app) else {
        return;
    };
    let Ok(log_path) = xcodebuild::build_log_path() else {
        return;
    };
    let Ok(text) = std::fs::read_to_string(&log_path) else {
        return;
    };
    let modules = xcactivitylog::parse_text_lines(&text);
    if modules.is_empty() {
        return; // filtered output / null build — nothing to ingest, no-op
    }
    let ingested = modules.len();
    // Cross-process-safe read-merge-write: the bsp poll loop (a separate
    // process) folds Xcode.app `.xcactivitylog` builds into the same
    // `(build_root, scheme)` store concurrently. A shared advisory lock keeps
    // either side from clobbering the other's modules. `Watermark::Keep` leaves
    // bsp's poll watermark untouched — this ingests a different log source
    // (xcodebuild stdout), not `.xcactivitylog`. Best-effort: never fails the
    // build.
    let merged = compile_store::CompileStore::merge_save_locked(
        &build_root,
        &cfg.scheme,
        vec![modules],
        compile_store::Watermark::Keep,
    );
    log::info!(
        target: "pipeline",
        "ingested {ingested} compile module(s) from the build log (store now {})",
        merged.store.module_count()
    );
}

/// Run the full pipeline (phases 1-7; the caller starts the phase-8
/// tailers via `consoles::start_tailers` on the returned file paths).
/// `debug: true` launches with `--wait-for-debugger` (DAP mode);
/// `false` is the plain `xcode-dap run`. Cancellation is honored
/// mid-preflight and mid-build (kills the respective process group), and
/// while locating the app, installing and launching (the helper is dropped,
/// which kills it; a cancelled debug launch also terminates the app).
pub async fn run_pipeline(
    req: &Request,
    debug: bool,
    sink: &dyn OutputSink,
    cancel: CancellationToken,
) -> anyhow::Result<LaunchedApp> {
    // The selection store is read on every `launch` (see run_build),
    // including Zed's Rerun of a stale in-memory scenario.
    let prepared = prepare(req, sink, &cancel, true).await?;
    let (udid, app_path) = build_phases(&prepared, sink, cancel.clone(), true).await?;

    // In DAP mode, supersede any previous session on this simulator BEFORE
    // installing: on the simulator `simctl install` blocks while that session's
    // app is still running under lldb (it does not replace or kill it), so a
    // Rerun would otherwise stall for as long as the old app lives. Signalling
    // now lets the predecessor tear down (terminating its own app) and unblocks
    // our install. Best-effort — a failure only risks the install stalling
    // until the old session is stopped by hand. The plain `xcode-dap run`
    // (debug == false) does not participate in the DAP pidfile.
    if debug {
        if let Err(e) = crate::util::pidfile::kill_old(&udid) {
            log::warn!(target: "pipeline", "pre-install supersede signal failed: {e:#}");
        }
    }

    // Phase 5: bundle id + install.
    let bundle_id =
        unless_cancelled(&cancel, "reading the bundle id", bundle_id(&app_path)).await?;
    log::info!(target: "pipeline", "bundle id: {bundle_id}");
    sink.line("console", &format!("Installing {bundle_id}..."));
    unless_cancelled(&cancel, "installing", simctl::install(&udid, &app_path)).await?;
    // A Stop that lands as the install finishes must not go on to launch.
    if cancel.is_cancelled() {
        bail!("cancelled after installing {bundle_id}");
    }

    // Phase 6-7: launch (+ PID). Console capture files live under
    // ~/.zedxcode/run/<udid>/, absolute and pre-truncated.
    let run_dir = zedxcode_home()?.join("run").join(&udid);
    tokio::fs::create_dir_all(&run_dir)
        .await
        .with_context(|| format!("creating {}", run_dir.display()))?;
    let stdout_file = run_dir.join("out.log");
    let stderr_file = run_dir.join("err.log");
    for f in [&stdout_file, &stderr_file] {
        tokio::fs::File::create(f)
            .await
            .with_context(|| format!("truncating {}", f.display()))?;
    }
    let app_name = app_path
        .file_stem()
        .and_then(|s| s.to_str())
        .context("app path has no file stem")?;

    sink.line(
        "console",
        &format!(
            "Launching {bundle_id}{}...",
            if debug { " (waiting for debugger)" } else { "" }
        ),
    );
    // The app's launch environment. Empty: no scheme environment or scenario
    // "env" is read yet; the launch still adds NSUnbufferedIO=YES, so print()
    // lines reach the console as they are written.
    let launch_env = BTreeMap::new();
    // Boxed so a cancelled launch can be dropped (killing simctl) after its
    // bounded settle, before the terminate.
    let mut launch = Box::pin(simctl::launch(
        &udid,
        &bundle_id,
        app_name,
        debug,
        &launch_env,
        &stdout_file,
        &stderr_file,
    ));
    let launched = tokio::select! {
        biased;
        _ = cancel.cancelled() => None,
        r = &mut launch => Some(r),
    };
    let Some(launched) = launched else {
        // simctl may already have started the app, suspended under
        // `--wait-for-debugger`, without handing back its pid: let a launch
        // under way finish (bounded), then terminate the app by bundle id so
        // it is not left frozen. (The plain `run` launches no suspended app,
        // and its Ctrl-C leaves the app running.)
        if debug {
            let _ = tokio::time::timeout(CANCELLED_LAUNCH_SETTLE, &mut launch).await;
            drop(launch);
            let _ = tokio::time::timeout(
                CANCELLED_LAUNCH_TERMINATE_GRACE,
                simctl::terminate(&udid, &bundle_id),
            )
            .await;
        }
        bail!("cancelled while launching {bundle_id}");
    };
    drop(launch);
    let pid = launched?;
    sink.line("console", &format!("Launched {bundle_id} (pid {pid})"));

    Ok(LaunchedApp {
        pid,
        udid,
        bundle_id,
        app_path,
        stdout_file,
        stderr_file,
    })
}

/// Run one pipeline step unless `cancel` fires first (a fired token wins
/// even when the step could finish at once). Dropping the step's future kills
/// its child process (every helper runs with `kill_on_drop`), so a Stop never
/// waits for a slow simctl or xcodebuild call to return.
async fn unless_cancelled<T>(
    cancel: &CancellationToken,
    what: &str,
    step: impl std::future::Future<Output = anyhow::Result<T>>,
) -> anyhow::Result<T> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => bail!("cancelled while {what}"),
        r = step => r,
    }
}

/// Phase 1: the container to build, generating it first when it is missing
/// and the scenario configures a preflight. The preflight runs verbatim via
/// `sh -c` (e.g. `xcodegen generate`); the binary never invents a command.
/// Cancellation kills the preflight process group.
async fn ensure_container(
    project: &Project,
    options: &Options,
    sink: &dyn OutputSink,
    cancel: &CancellationToken,
) -> anyhow::Result<PathBuf> {
    if let Some(container) = &project.container {
        let workspace = &container.path;
        if workspace.exists() {
            log::info!(
                target: "pipeline",
                "preflight skipped: workspace {} exists",
                workspace.display()
            );
            return Ok(workspace.clone());
        }
        let Some(preflight) = options.preflight.as_deref() else {
            bail!(
                "workspace {} not found and no \"preflight\" command is configured \
                 to generate it\nhint: generate the project first (e.g. `xcodegen \
                 generate` or `tuist generate --no-open`), set \"preflight\" in \
                 .zed/debug.json, or fix the path via --workspace / the \"workspace\" \
                 key; `xcode-dap refresh` regenerates and refreshes go-to-definition",
                workspace.display()
            );
        };
        let missing = format!("Workspace {} missing", workspace.display());
        let cwd = workspace.parent().filter(|p| p.is_dir());
        run_preflight(preflight, &missing, cwd, sink, cancel).await?;
        if !workspace.exists() {
            bail!(
                "preflight `{preflight}` completed but workspace {} still does \
                 not exist",
                workspace.display()
            );
        }
        return Ok(workspace.clone());
    }
    // Nothing found: without a preflight, say why (no project at all, or one
    // its generator has not written yet).
    let Some(preflight) = options.preflight.as_deref() else {
        return project.require_container().map(|c| c.path.clone());
    };
    let missing = format!("No Xcode project in {} yet", project.root.display());
    let cwd = project
        .generator
        .as_ref()
        .map_or(project.root.as_path(), |g| g.dir.as_path());
    run_preflight(preflight, &missing, Some(cwd), sink, cancel).await?;
    let generated = project::discover(&project.root).map_err(|tie| anyhow!(tie))?;
    match generated.container {
        Some(container) => Ok(container.path),
        None => bail!(
            "preflight `{preflight}` completed but no .xcworkspace or .xcodeproj appeared in {}",
            project.root.display()
        ),
    }
}

/// Run `preflight` in `cwd` (the process cwd when `None`), its output
/// streamed to `sink`. `missing` says what it is for.
async fn run_preflight(
    preflight: &str,
    missing: &str,
    cwd: Option<&Path>,
    sink: &dyn OutputSink,
    cancel: &CancellationToken,
) -> anyhow::Result<()> {
    log::info!(target: "pipeline", "preflight: {missing} — running `{preflight}`");
    sink.line(
        "console",
        &format!("{missing} — running preflight: {preflight}"),
    );
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(preflight);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    stream_to_sink(cmd, sink, cancel)
        .await
        .with_context(|| format!("preflight `{preflight}` failed"))
}

/// Regenerate `<workspace-parent>/buildServer.json` when it is missing or
/// older than the workspace (same freshness logic as `doctor`): consuming
/// repos' clean scripts delete it (and wipe DerivedData), after which
/// sourcekit-lsp silently falls back to SPM mode on a root Package.swift —
/// macOS fallback args, "Could not load module" errors, wrong jumps.
/// A missing buildServer.json is only regenerated when the project opted
/// in ([`build_server_opted_in`]: an existing buildServer.json, or an
/// "Xcode" scenario in `.zed/debug.json`) — a plain `xcode-dap build` in a
/// repo that never configured this adapter must not write one (it would
/// dirty the checkout and silently flip sourcekit-lsp out of SPM mode).
/// When a first-create does happen (Zed-modal / hand-written debug.json —
/// setup's `.git/info/exclude` step never ran there), the new file is
/// git-ignored via [`git_exclude_build_server`]; and the restart hint is
/// only surfaced when the parsed argv/build_root actually differ
/// ([`Outcome::restart_hint`] — a scheme-only change reloads via bsp, and
/// mtime-bump-per-build setups otherwise re-prompt a pointless restart).
/// build_root is resolved by [`xcodebuild::resolve_build_root`] (cached) and
/// the file is written by the pure-Rust [`write_build_server_json`] — no
/// external build server. The common warm path costs only the mtime stat
/// calls. Errors only on cancellation (a Stop mid-regen must not delay the
/// disconnect by the settings resolution's runtime).
async fn ensure_build_server(
    cfg: &Target,
    sink: &dyn OutputSink,
    cancel: &CancellationToken,
) -> anyhow::Result<()> {
    let Ok(ws) = std::path::absolute(&cfg.workspace) else {
        return Ok(());
    };
    let Some(dir) = ws.parent() else {
        return Ok(());
    };
    let build_server = dir.join("buildServer.json");
    if !buildserver_stale(mtime(&build_server), workspace_mtime(&ws)) {
        return Ok(());
    }
    if !build_server_opted_in(dir, &build_server) {
        log::info!(
            target: "pipeline",
            "{} missing but the project never opted in (no \"Xcode\" scenario \
             in .zed/debug.json) — skipping regen",
            build_server.display()
        );
        return Ok(());
    }
    // Resolve build_root before the build (no `.app` yet). The settings
    // subprocess is raced against cancellation (its future is dropped —
    // kill_on_drop kills the process). A resolution failure is non-fatal:
    // skip the regen rather than fail the build.
    let build_root = tokio::select! {
        r = xcodebuild::resolve_build_root(
            &ws, &cfg.scheme, cfg.configuration.as_deref(), cfg.derived_data.as_deref(),
        ) => match r {
            Ok(br) => br,
            Err(e) => {
                log::warn!(
                    target: "pipeline",
                    "buildServer.json regen skipped (retried on the next run): cannot resolve \
                     build_root: {e:#}"
                );
                return Ok(());
            }
        },
        _ = cancel.cancelled() => bail!("cancelled while resolving build_root for buildServer.json"),
    };
    log::info!(
        target: "pipeline",
        "{} missing or older than the workspace — regenerating for scheme \"{}\"",
        build_server.display(),
        cfg.scheme
    );
    let outcome = match write_build_server_json(dir, &ws, &cfg.scheme, &build_root) {
        Ok(o) => o,
        Err(e) => {
            log::warn!(target: "pipeline", "buildServer.json write failed (non-fatal): {e:#}");
            sink.line(
                "console",
                &format!("buildServer.json regeneration failed (non-fatal): {e}"),
            );
            return Ok(());
        }
    };
    if outcome.change == Change::Created {
        // First-create outside setup (whose .git/info/exclude step never
        // ran): git-ignore the new file so the write does not dirty
        // `git status`.
        git_exclude_build_server(dir);
    }
    if outcome.restart_hint {
        sink.line(
            "console",
            "buildServer.json regenerated — run 'editor: restart language server' \
             in Zed to restore code navigation",
        );
    } else {
        log::info!(
            target: "pipeline",
            "buildServer.json regenerated with unchanged argv/build_root — \
             skipping the restart-language-server hint"
        );
    }
    Ok(())
}

/// `plutil -extract CFBundleIdentifier raw <app>/Info.plist`.
async fn bundle_id(app: &std::path::Path) -> anyhow::Result<String> {
    let plist = app.join("Info.plist");
    let out = Command::new("plutil")
        .args(["-extract", "CFBundleIdentifier", "raw"])
        .arg(&plist)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .context("running plutil")?;
    if !out.status.success() {
        bail!(
            "reading CFBundleIdentifier from {} failed: {}",
            plist.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if id.is_empty() {
        bail!("empty CFBundleIdentifier in {}", plist.display());
    }
    Ok(id)
}

/// Spawn `cmd` and stream its stdout/stderr lines to `sink` (used for the
/// moderate-volume preflight output; the build has its own filter/throttle).
///
/// The command runs in its own process group so that cancellation can kill
/// the whole tree (project generators spawn helpers, and `kill_on_drop`
/// does not survive this process exiting): SIGTERM the group, wait up to
/// 3 s, then SIGKILL — mirroring [`xcodebuild::build`].
async fn stream_to_sink(
    mut cmd: Command,
    sink: &dyn OutputSink,
    cancel: &CancellationToken,
) -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    procgroup::spawn_in_new_group(&mut cmd);
    let mut child = cmd.spawn().context("spawning preflight command")?;
    // setpgid(0, 0) makes the child's pid its pgid.
    let pgid = child.id().map(|p| p as i32).unwrap_or(0);
    let stdout = child.stdout.take().context("stdout not piped")?;
    let stderr = child.stderr.take().context("stderr not piped")?;
    let mut out_lines = BufReader::new(stdout).lines();
    let mut err_lines = BufReader::new(stderr).lines();
    let mut out_done = false;
    let mut err_done = false;
    while !(out_done && err_done) {
        tokio::select! {
            // "preflight" (not "console"): project generators print
            // thousands of lines, which would rotate prior diagnostics out
            // of xcode-dap.log at INFO (DapSink logs it at DEBUG only).
            //
            // A reader error (e.g. non-UTF-8 output from the generator) must
            // not propagate with `?` while the child runs: that unwinds
            // leaving only `kill_on_drop` to SIGKILL the direct `/bin/sh`,
            // orphaning the generator and its helpers (a child of `sh` in the
            // new process group). Tear the group down like the cancel arm.
            line = out_lines.next_line(), if !out_done => match line {
                Ok(Some(l)) => sink.line("preflight", &l),
                Ok(None) => out_done = true,
                Err(e) => return Err(fail_preflight(&mut child, pgid, sink, e).await),
            },
            line = err_lines.next_line(), if !err_done => match line {
                Ok(Some(l)) => sink.line("preflight", &l),
                Ok(None) => err_done = true,
                Err(e) => return Err(fail_preflight(&mut child, pgid, sink, e).await),
            },
            _ = cancel.cancelled() => {
                sink.line("console", "Preflight cancelled — stopping");
                terminate_group(&mut child, pgid).await;
                bail!("preflight cancelled");
            }
        }
    }
    let status = child.wait().await?;
    log::info!(target: "pipeline", "preflight command exited {status}");
    if !status.success() {
        bail!("exited with {status}");
    }
    Ok(())
}

/// Graceful teardown of the preflight process group: SIGTERM, wait up to 3 s
/// for the whole tree to exit, then SIGKILL. Mirrors the cancellation path in
/// [`xcodebuild::build`].
async fn terminate_group(child: &mut tokio::process::Child, pgid: i32) {
    procgroup::term_group(pgid);
    if tokio::time::timeout(Duration::from_secs(3), child.wait())
        .await
        .is_err()
    {
        procgroup::kill_group(pgid);
        let _ = child.wait().await;
    }
}

/// Tear the preflight process group down after a reader error and turn the
/// error into the failure returned from [`stream_to_sink`] (so the whole tree
/// is reaped rather than orphaned by `kill_on_drop`).
async fn fail_preflight(
    child: &mut tokio::process::Child,
    pgid: i32,
    sink: &dyn OutputSink,
    err: std::io::Error,
) -> anyhow::Error {
    sink.line("console", "Preflight output unreadable — stopping");
    terminate_group(child, pgid).await;
    anyhow::Error::new(err).context("reading preflight output")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unless_cancelled_returns_when_the_token_fires_mid_step() {
        // A step that never finishes on its own (a hung `simctl list`).
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            trigger.cancel();
        });
        let started = std::time::Instant::now();
        let step = std::future::pending::<anyhow::Result<()>>();
        let err = unless_cancelled(&cancel, "resolving the simulator", step)
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "cancelled while resolving the simulator");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn unless_cancelled_prefers_a_fired_token_over_a_finished_step() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let step = async { Ok::<_, anyhow::Error>(7) };
        assert!(unless_cancelled(&cancel, "installing", step).await.is_err());
    }

    #[tokio::test]
    async fn unless_cancelled_passes_the_step_result_through() {
        let cancel = CancellationToken::new();
        let ok = unless_cancelled(&cancel, "installing", async { Ok::<_, anyhow::Error>(7) });
        assert_eq!(ok.await.unwrap(), 7);
        let failed = unless_cancelled(&cancel, "installing", async {
            Err::<(), _>(anyhow::anyhow!("simctl install failed"))
        });
        assert_eq!(
            failed.await.unwrap_err().to_string(),
            "simctl install failed"
        );
    }
}
