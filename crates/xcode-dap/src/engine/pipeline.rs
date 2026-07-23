//! preflight -> build -> install -> launch -> pid pipeline, shared by
//! dap mode and the CLI. See `docs/design/dap-proxy.md` §4.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::engine::config::LaunchConfig;
use crate::engine::{compile_store, selection, simctl, xcactivitylog, xcodebuild};
use crate::setup::build_server::{write_build_server_json, Change};
use crate::setup::project::{build_server_opted_in, git_exclude_build_server};
use crate::util::paths::{buildserver_stale, mtime, workspace_mtime};
use crate::util::procgroup;

// Re-exported from `util::paths` so callers across `commands/` and `dap/`
// can keep importing it from the pipeline module.
pub use crate::util::paths::zedxcode_home;

/// Generous cap on the whole boot phase (`simctl boot` + retries +
/// Simulator.app open + `bootstatus` wait): a cold boot finishes in well
/// under a minute, so hitting this means `bootstatus` is wedged and the
/// device needs a manual `simctl shutdown` instead of an endless wait.
const BOOT_TIMEOUT: Duration = Duration::from_secs(240);

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

/// Phases 1-4: preflight -> resolve simulator -> (optional pre-boot) ->
/// build -> app path. Returns `(udid, app_path)`.
///
/// This is the whole `xcode-dap build` command (`boot: false` — building
/// for a `-destination ...,id=<udid>` does not require a booted device).
pub async fn run_build(
    cfg: &LaunchConfig,
    sink: &dyn OutputSink,
    cancel: CancellationToken,
) -> anyhow::Result<(String, PathBuf)> {
    // The runtime selection overlay (select-scheme / select-device) is
    // re-read from disk on every entry, so a new selection applies to the
    // very next build without touching .zed/debug.json or tasks.json.
    let cfg = selection::overlaid(cfg, sink);
    build_phases(&cfg, sink, cancel, false).await
}

async fn build_phases(
    cfg: &LaunchConfig,
    sink: &dyn OutputSink,
    cancel: CancellationToken,
    boot: bool,
) -> anyhow::Result<(String, PathBuf)> {
    // Phase 1: preflight (only if the workspace is missing).
    preflight(cfg, sink, &cancel).await?;

    // Phase 2: resolve simulator (+ visible pre-boot for run/debug).
    let udid = simctl::resolve_device(cfg.device.as_deref(), cfg.os.as_deref()).await?;
    sink.line("console", &format!("Simulator: {udid}"));
    if boot {
        sink.line("console", "Booting simulator (visible)...");
        tokio::select! {
            r = tokio::time::timeout(BOOT_TIMEOUT, simctl::boot(&udid)) => match r {
                Ok(r) => r?,
                Err(_) => bail!(
                    "simulator did not finish booting in {}s — try \
                     `xcrun simctl shutdown {udid}` and rerun",
                    BOOT_TIMEOUT.as_secs()
                ),
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

    // Phase 4: locate the .app product.
    let app = xcodebuild::app_path(cfg, &udid).await?;
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
fn ingest_build_log(cfg: &LaunchConfig, app: &Path) {
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
/// mid-preflight and mid-build (kills the respective process group).
pub async fn run_pipeline(
    cfg: &LaunchConfig,
    debug: bool,
    sink: &dyn OutputSink,
    cancel: CancellationToken,
) -> anyhow::Result<LaunchedApp> {
    // Selection overlay first (see run_build): in DAP mode this runs on
    // every `launch`, including Zed's Rerun of a stale in-memory scenario.
    let cfg = &selection::overlaid(cfg, sink);
    let (udid, app_path) = build_phases(cfg, sink, cancel.clone(), true).await?;

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
    let bundle_id = bundle_id(&app_path).await?;
    log::info!(target: "pipeline", "bundle id: {bundle_id}");
    if cancel.is_cancelled() {
        bail!("cancelled");
    }
    sink.line("console", &format!("Installing {bundle_id}..."));
    simctl::install(&udid, &app_path).await?;

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
    let pid = simctl::launch(
        &udid,
        &bundle_id,
        app_name,
        debug,
        &stdout_file,
        &stderr_file,
    )
    .await?;
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

/// Phase 1: if the workspace is missing and a preflight command is
/// configured, run it verbatim via `sh -c` (e.g. for a Makefile-based
/// Tuist project, setup writes `make project CI=true`). The binary never
/// invents a command. Cancellation kills the preflight process group.
async fn preflight(
    cfg: &LaunchConfig,
    sink: &dyn OutputSink,
    cancel: &CancellationToken,
) -> anyhow::Result<()> {
    if cfg.workspace.exists() {
        log::info!(
            target: "pipeline",
            "preflight skipped: workspace {} exists",
            cfg.workspace.display()
        );
        return Ok(());
    }
    let Some(preflight) = cfg.preflight.as_deref() else {
        bail!(
            "workspace {} not found and no \"preflight\" command is configured \
             to generate it\nhint: generate the project first (e.g. `make project \
             CI=true` for Tuist setups), set \"preflight\" in .zed/debug.json, \
             or fix the path via --workspace / the \"workspace\" key; \
             `xcode-dap refresh` regenerates and refreshes go-to-definition",
            cfg.workspace.display()
        );
    };
    log::info!(
        target: "pipeline",
        "preflight: workspace {} missing — running `{preflight}`",
        cfg.workspace.display()
    );
    sink.line(
        "console",
        &format!(
            "Workspace {} missing — running preflight: {preflight}",
            cfg.workspace.display()
        ),
    );
    let mut cmd = Command::new("/bin/sh");
    cmd.arg("-c").arg(preflight);
    if let Some(dir) = cfg.workspace.parent().filter(|p| p.is_dir()) {
        cmd.current_dir(dir);
    }
    stream_to_sink(cmd, sink, cancel)
        .await
        .with_context(|| format!("preflight `{preflight}` failed"))?;
    if !cfg.workspace.exists() {
        bail!(
            "preflight `{preflight}` completed but workspace {} still does \
             not exist",
            cfg.workspace.display()
        );
    }
    Ok(())
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
    cfg: &LaunchConfig,
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
                    "buildServer.json regen skipped: cannot resolve build_root: {e:#}"
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
