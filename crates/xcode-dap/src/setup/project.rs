//! Per-project setup: `.zed/debug.json`, `.zed/tasks.json`,
//! `buildServer.json` (via [`crate::setup::build_server`]), `.git/info/exclude`.
//! See `docs/design/dap-proxy.md` §6.1.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::setup::jsonc;
use crate::util::paths::container_flag;

/// CLI overrides for [`setup_project`]; every `None` is auto-detected.
#[derive(Debug, Clone, Default)]
pub struct ProjectFlags {
    pub workspace: Option<PathBuf>,
    pub scheme: Option<String>,
    pub device: Option<String>,
    pub os: Option<String>,
    pub preflight: Option<String>,
    /// `--oslog`: force `"oslog": true` in the generated debug.json. When
    /// `false` (flag absent) the value already present in an existing
    /// `.zed/debug.json` is preserved, so a re-run never silently drops a
    /// previously enabled (or hand-edited) `oslog`.
    pub oslog: bool,
    /// `--derived-data`: explicit DerivedData directory, written as
    /// `"derivedData"` into the generated debug.json (`None` = omit).
    pub derived_data: Option<PathBuf>,
}

/// Resolved per-project configuration written into `.zed/`.
#[derive(Debug, Clone)]
pub struct ProjectConfig {
    /// Workspace/project file name, relative to the project dir
    /// (e.g. `MyApp.xcworkspace`). Absolute only when `--workspace`
    /// pointed outside the project dir (rendered verbatim then).
    pub workspace: String,
    pub scheme: String,
    pub device: String,
    pub os: Option<String>,
    /// Project-generation command run when the workspace is missing.
    /// Auto-detected as `make project CI=true` when the Makefile has a
    /// `project:` target (Tuist-style generated workspaces — the `CI=true`
    /// form is mandatory).
    pub preflight: Option<String>,
    /// Pump OSLog (`log stream`) into the Debug Console (`"oslog"` in
    /// debug.json). From `--oslog`, else preserved from the existing file.
    pub oslog: bool,
    /// Explicit DerivedData directory (`"derivedData"` in debug.json), from
    /// `--derived-data`. `None` = omit (xcodebuild's default location).
    pub derived_data: Option<String>,
}

/// Apply (or re-apply, idempotently) the per-project config in `dir`.
pub async fn setup_project(dir: &Path, flags: ProjectFlags) -> Result<()> {
    let dir = dir
        .canonicalize()
        .with_context(|| format!("project dir {} does not exist", dir.display()))?;
    let cfg = detect(&dir, flags).await?;
    println!("project: {}", dir.display());
    println!(
        "  workspace: {}  scheme: {}  device: {}{}{}",
        cfg.workspace,
        cfg.scheme,
        cfg.device,
        cfg.os
            .as_deref()
            .map(|o| format!("  os: {o}"))
            .unwrap_or_default(),
        cfg.preflight
            .as_deref()
            .map(|p| format!("  preflight: {p}"))
            .unwrap_or_default()
    );
    write_zed_files(&dir, &cfg, &current_exe_path())?;
    ensure_git_exclude(&dir)?;
    generate_build_server(&dir, &cfg).await;
    println!("\nNext: open the project in Zed; first CMD+R opens the New Session modal —");
    println!("pick \"Run on simulator\" once, then CMD+R reruns it.");
    Ok(())
}

// ---------------------------------------------------------------------------
// detection
// ---------------------------------------------------------------------------

async fn detect(dir: &Path, flags: ProjectFlags) -> Result<ProjectConfig> {
    let workspace = match flags.workspace {
        Some(w) => normalize_workspace_flag(dir, &w),
        None => find_workspace(dir)?,
    };
    let scheme = match flags.scheme {
        Some(s) => s,
        None => detect_scheme(dir, &workspace).await?,
    };
    let (device, os) = match flags.device {
        Some(d) => (d, flags.os),
        None => {
            let (d, detected_os) = detect_device().await?;
            (d, flags.os.or(detected_os))
        }
    };
    let preflight = match flags.preflight {
        Some(p) if p.trim().is_empty() => None,
        Some(p) => Some(p),
        None => makefile_preflight(dir),
    };
    let oslog = flags.oslog || existing_oslog(dir);
    let derived_data = flags
        .derived_data
        .map(|p| p.to_string_lossy().into_owned())
        .or_else(|| existing_derived_data(dir));
    Ok(ProjectConfig {
        workspace,
        scheme,
        device,
        os,
        preflight,
        oslog,
        derived_data,
    })
}

/// Normalize a `--workspace` value for [`ProjectConfig`]: an absolute path
/// under the (canonicalized) project dir becomes the relative remainder
/// (so the rendered `$ZED_WORKTREE_ROOT/<ws>` resolves correctly); an
/// absolute path outside the project dir stays absolute (rendered
/// verbatim, see [`render_debug_json`]). Relative input is kept
/// byte-identical.
fn normalize_workspace_flag(dir: &Path, workspace: &Path) -> String {
    if !workspace.is_absolute() {
        return workspace.to_string_lossy().into_owned();
    }
    let ws = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    match ws.strip_prefix(&dir) {
        Ok(rel) => rel.to_string_lossy().into_owned(),
        Err(_) => ws.to_string_lossy().into_owned(),
    }
}

/// Find the single top-level `*.xcworkspace` (else `*.xcodeproj`) in `dir`.
fn find_workspace(dir: &Path) -> Result<String> {
    let mut workspaces = vec![];
    let mut projects = vec![];
    for entry in fs::read_dir(dir).with_context(|| format!("cannot read {}", dir.display()))? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if name.ends_with(".xcworkspace") {
            workspaces.push(name);
        } else if name.ends_with(".xcodeproj") {
            projects.push(name);
        }
    }
    workspaces.sort();
    projects.sort();
    let pick = |mut v: Vec<String>, kind: &str| -> Result<String> {
        match v.len() {
            1 => Ok(v.remove(0)),
            _ => bail!(
                "multiple {kind} files found ({}) — pass --workspace",
                v.join(", ")
            ),
        }
    };
    if !workspaces.is_empty() {
        return pick(workspaces, ".xcworkspace");
    }
    if !projects.is_empty() {
        return pick(projects, ".xcodeproj");
    }
    bail!(
        "no .xcworkspace/.xcodeproj found in {} — pass --workspace (it may not be generated yet)",
        dir.display()
    )
}

/// `xcodebuild -list -json` -> the project's schemes; unambiguous or bail.
async fn detect_scheme(dir: &Path, workspace: &str) -> Result<String> {
    let container_flag = container_flag(Path::new(workspace));
    let out = tokio::process::Command::new("xcodebuild")
        .args(["-list", "-json", container_flag, workspace])
        .current_dir(dir)
        .output()
        .await
        .context("failed to run `xcodebuild -list -json` — is Xcode installed?")?;
    if !out.status.success() {
        bail!(
            "`xcodebuild -list -json {container_flag} {workspace}` failed:\n{}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).context("xcodebuild -list output is not JSON")?;
    let schemes: Vec<String> = v
        .get("workspace")
        .or_else(|| v.get("project"))
        .and_then(|c| c.get("schemes"))
        .and_then(|s| s.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    match schemes.len() {
        0 => bail!("xcodebuild -list reported no schemes for {workspace}"),
        1 => Ok(schemes.into_iter().next().unwrap()),
        _ => bail!(
            "multiple schemes — pass --scheme \"<name>\":\n  {}",
            schemes.join("\n  ")
        ),
    }
}

/// Pick a simulator: the booted iOS device if any, else the newest available
/// iPhone (deterministic: highest OS version, then last name in sort order).
/// Returns `(device_name, os_version)`.
async fn detect_device() -> Result<(String, Option<String>)> {
    let out = tokio::process::Command::new("xcrun")
        .args(["simctl", "list", "devices", "--json"])
        .output()
        .await
        .context("failed to run `xcrun simctl list devices --json`")?;
    if !out.status.success() {
        bail!(
            "simctl list failed:\n{}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).context("simctl list output is not JSON")?;
    let devices = v
        .get("devices")
        .and_then(|d| d.as_object())
        .context("simctl list output has no devices")?;
    let mut candidates: Vec<(Vec<u32>, String, String)> = vec![]; // (version, name, os)
    for (runtime, list) in devices {
        // "com.apple.CoreSimulator.SimRuntime.iOS-26-3" -> "26.3"
        let Some(os) = runtime
            .rsplit('.')
            .next()
            .and_then(|r| r.strip_prefix("iOS-"))
            .map(|r| r.replace('-', "."))
        else {
            continue; // non-iOS runtime
        };
        let Some(list) = list.as_array() else {
            continue;
        };
        for dev in list {
            let available = dev
                .get("isAvailable")
                .and_then(|a| a.as_bool())
                .unwrap_or(false);
            let name = dev.get("name").and_then(|n| n.as_str()).unwrap_or_default();
            let state = dev
                .get("state")
                .and_then(|s| s.as_str())
                .unwrap_or_default();
            if !available || name.is_empty() {
                continue;
            }
            if state == "Booted" {
                return Ok((name.to_owned(), Some(os.clone())));
            }
            if name.starts_with("iPhone") {
                let version: Vec<u32> = os.split('.').map(|p| p.parse().unwrap_or(0)).collect();
                candidates.push((version, name.to_owned(), os.clone()));
            }
        }
    }
    candidates.sort();
    match candidates.pop() {
        Some((_, name, os)) => Ok((name, Some(os))),
        None => bail!("no available iPhone simulator found — pass --device \"<name or udid>\""),
    }
}

/// `"oslog"` of the first scenario in an existing `.zed/debug.json`
/// (best-effort; `false` when the file/key is missing). Re-running setup
/// must preserve an enabled OSLog pump rather than reset it to the default.
fn existing_oslog(dir: &Path) -> bool {
    let Ok(text) = fs::read_to_string(dir.join(".zed").join("debug.json")) else {
        return false;
    };
    jsonc::parse_jsonc(&text)
        .ok()
        .and_then(|v| v.as_array()?.iter().find_map(|s| s.get("oslog")?.as_bool()))
        .unwrap_or(false)
}

/// `"derivedData"` of the first scenario in an existing `.zed/debug.json`
/// (best-effort; `None` when the file/key is missing). Re-running setup
/// without `--derived-data` must preserve a previously set (or hand-edited)
/// DerivedData rather than drop it, mirroring [`existing_oslog`].
fn existing_derived_data(dir: &Path) -> Option<String> {
    let text = fs::read_to_string(dir.join(".zed").join("debug.json")).ok()?;
    jsonc::parse_jsonc(&text).ok().and_then(|v| {
        v.as_array()?
            .iter()
            .find_map(|s| s.get("derivedData")?.as_str().map(str::to_owned))
    })
}

/// `make project CI=true` when the Makefile has a `project:` target
/// (Tuist-style generated workspaces).
fn makefile_preflight(dir: &Path) -> Option<String> {
    let text = fs::read_to_string(dir.join("Makefile")).ok()?;
    text.lines()
        // A `project:` rule, not a `project:=` / `project::=` variable
        // assignment (which defines no target — the preflight would then
        // fail with "No rule to make target 'project'").
        .any(|l| {
            l.strip_prefix("project:")
                .is_some_and(|rest| !rest.starts_with('=') && !rest.starts_with(":="))
        })
        .then(|| "make project CI=true".to_owned())
}

// ---------------------------------------------------------------------------
// .zed/ file rendering (pure; byte-tested)
// ---------------------------------------------------------------------------

fn json_str(s: &str) -> String {
    serde_json::to_string(s).expect("string serialization cannot fail")
}

/// Absolute path of the running `xcode-dap` binary (canonicalized), embedded
/// as the task `command` in the generated `.zed/tasks.json`. Zed task shells
/// do not necessarily have a dev install on PATH — a bare `xcode-dap`
/// command exits 127 there. Falls back to the bare name only when the
/// executable path cannot be resolved at all.
pub(crate) fn current_exe_path() -> String {
    std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "xcode-dap".to_owned())
}

/// Quote an argument for Zed's shell-joined task command line.
pub(crate) fn shell_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-./:=".contains(c));
    if safe {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// `.zed/debug.json`: one flat "Xcode" scenario (adapter-specific keys are
/// flattened at top level — `DebugScenario.config` is `#[serde(flatten)]`,
/// see `docs/design/extension-api.md` §4). Key names match the shared
/// `xcode-dap-config::LaunchConfig` serde (camelCase).
pub fn render_debug_json(cfg: &ProjectConfig) -> String {
    let mut s = String::new();
    s.push_str("// Generated by `xcode-dap setup --project`. Edit freely; re-running setup\n");
    s.push_str("// backs this file up and rewrites it.\n");
    s.push_str("[\n  {\n");
    s.push_str("    \"adapter\": \"Xcode\",\n");
    s.push_str("    \"label\": \"Run on simulator\",\n");
    s.push_str("    \"request\": \"launch\",\n");
    // Relative workspaces are anchored to the worktree root; an absolute
    // one (outside the project dir) must be emitted verbatim — prefixing
    // it would resolve `$ZED_WORKTREE_ROOT//abs/path` under the worktree.
    let workspace = if Path::new(&cfg.workspace).is_absolute() {
        cfg.workspace.clone()
    } else {
        format!("$ZED_WORKTREE_ROOT/{}", cfg.workspace)
    };
    s.push_str(&format!("    \"workspace\": {},\n", json_str(&workspace)));
    s.push_str(&format!("    \"scheme\": {},\n", json_str(&cfg.scheme)));
    s.push_str(&format!("    \"device\": {}", json_str(&cfg.device)));
    if let Some(os) = &cfg.os {
        s.push_str(&format!(",\n    \"os\": {}", json_str(os)));
    }
    if let Some(p) = &cfg.preflight {
        s.push_str(&format!(",\n    \"preflight\": {}", json_str(p)));
    }
    if cfg.oslog {
        s.push_str(",\n    \"oslog\": true");
    }
    if let Some(dd) = &cfg.derived_data {
        s.push_str(&format!(",\n    \"derivedData\": {}", json_str(dd)));
    }
    s.push_str("\n  }\n]\n");
    s
}

/// `.zed/tasks.json`: "Xcode: Build" / "Xcode: Clean" / "Xcode: Refresh" /
/// "Xcode: Console" / "Xcode: Choose Scheme" / "Xcode: Choose Destination"
/// calling `xcode-dap` subcommands with cwd `$ZED_WORKTREE_ROOT`. Labels
/// must match the `task::Spawn` bindings in `setup/user.rs`. `xcode_dap` is
/// the command invoked by every task — normally the absolute binary path
/// from [`current_exe_path`], so tasks don't depend on PATH (a bare name
/// there exits 127 in Zed task shells).
pub fn render_tasks_json(cfg: &ProjectConfig, xcode_dap: &str) -> String {
    let command = shell_quote(xcode_dap);
    let ws = shell_quote(&cfg.workspace);
    let scheme = shell_quote(&cfg.scheme);
    let device = shell_quote(&cfg.device);

    let mut build_args: Vec<String> = vec![
        "build".into(),
        "--workspace".into(),
        ws.clone(),
        "--scheme".into(),
        scheme.clone(),
        "--device".into(),
        device,
    ];
    if let Some(os) = &cfg.os {
        build_args.push("--os".into());
        build_args.push(shell_quote(os));
    }
    if let Some(dd) = &cfg.derived_data {
        build_args.push("--derived-data".into());
        build_args.push(shell_quote(dd));
    }
    let mut clean_args: Vec<String> = vec![
        "clean".into(),
        "--workspace".into(),
        ws.clone(),
        "--scheme".into(),
        scheme,
    ];
    if let Some(dd) = &cfg.derived_data {
        clean_args.push("--derived-data".into());
        clean_args.push(shell_quote(dd));
    }
    let refresh_args: Vec<String> = vec!["refresh".into()];
    let console_args: Vec<String> = vec!["console".into(), "--follow".into()];
    // Interactive pickers (Zed task terminals have working stdin); both
    // write the .zed/.zedx/selection.json overlay used by the next
    // build/run. --workspace pins the container the Build task uses.
    let choose_scheme_args: Vec<String> = vec!["select-scheme".into(), "--workspace".into(), ws];
    let choose_device_args: Vec<String> = vec!["select-device".into()];

    let mut s = String::new();
    s.push_str("// Generated by `xcode-dap setup --project`. Edit freely; re-running setup\n");
    s.push_str("// backs this file up and rewrites it.\n");
    s.push_str("[\n");
    s.push_str(&task_entry("Xcode: Build", &command, &build_args));
    s.push_str(",\n");
    s.push_str(&task_entry("Xcode: Clean", &command, &clean_args));
    s.push_str(",\n");
    s.push_str(&task_entry("Xcode: Refresh", &command, &refresh_args));
    s.push_str(",\n");
    s.push_str(&task_entry("Xcode: Console", &command, &console_args));
    s.push_str(",\n");
    s.push_str(&task_entry(
        "Xcode: Choose Scheme",
        &command,
        &choose_scheme_args,
    ));
    s.push_str(",\n");
    s.push_str(&task_entry(
        "Xcode: Choose Destination",
        &command,
        &choose_device_args,
    ));
    s.push_str("\n]\n");
    s
}

fn task_entry(label: &str, command: &str, args: &[String]) -> String {
    let args_json = args
        .iter()
        .map(|a| json_str(a))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "  {{\n    \"label\": {},\n    \"command\": {},\n    \"args\": [{}],\n    \"cwd\": \"$ZED_WORKTREE_ROOT\"\n  }}",
        json_str(label),
        json_str(command),
        args_json
    )
}

/// Write `.zed/debug.json` + `.zed/tasks.json` (backup-then-rewrite when an
/// existing file differs; no-op when identical). `xcode_dap` is the task
/// command path (see [`render_tasks_json`]).
pub fn write_zed_files(dir: &Path, cfg: &ProjectConfig, xcode_dap: &str) -> Result<()> {
    let zed = dir.join(".zed");
    fs::create_dir_all(&zed).with_context(|| format!("cannot create {}", zed.display()))?;
    write_generated(&zed.join("debug.json"), &render_debug_json(cfg))?;
    write_generated(&zed.join("tasks.json"), &render_tasks_json(cfg, xcode_dap))?;
    Ok(())
}

fn write_generated(path: &Path, content: &str) -> Result<()> {
    if path.exists() {
        let existing = fs::read_to_string(path)?;
        if existing == content {
            println!("✓ {}: unchanged", path.display());
            return Ok(());
        }
        let backup = jsonc::backup_file(path, &existing)?;
        jsonc::atomic_write(path, content)?;
        println!(
            "✓ {}: rewritten (backup: {})",
            path.display(),
            backup.display()
        );
    } else {
        jsonc::atomic_write(path, content)?;
        println!("✓ {}: written", path.display());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// git + buildServer.json
// ---------------------------------------------------------------------------

/// Append `.zed/` and `buildServer.json` (both written by setup) to
/// `.git/info/exclude` unless each is already git-ignored.
pub fn ensure_git_exclude(dir: &Path) -> Result<()> {
    let run_git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
    };
    let Ok(repo) = run_git(&["rev-parse", "--git-dir"]) else {
        println!("– git not found; skipping .git/info/exclude");
        return Ok(());
    };
    if !repo.status.success() {
        println!("– not a git repository; skipping .git/info/exclude");
        return Ok(());
    }
    let out = run_git(&["rev-parse", "--git-path", "info/exclude"])
        .context("git rev-parse --git-path failed")?;
    let rel = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let exclude_path = if Path::new(&rel).is_absolute() {
        PathBuf::from(rel)
    } else {
        dir.join(rel)
    };
    // (check-ignore pathname, exclude line to append)
    for (target, line) in [(".zed", ".zed/"), ("buildServer.json", "buildServer.json")] {
        if run_git(&["check-ignore", "-q", target])
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            println!("✓ {line} already git-ignored");
            continue;
        }
        let existing = fs::read_to_string(&exclude_path).unwrap_or_default();
        if existing
            .lines()
            .any(|l| l.trim().trim_start_matches('/').trim_end_matches('/') == target)
        {
            println!("✓ {line} already listed in {}", exclude_path.display());
            continue;
        }
        if let Some(parent) = exclude_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut new = existing;
        if !new.is_empty() && !new.ends_with('\n') {
            new.push('\n');
        }
        new.push_str(line);
        new.push('\n');
        fs::write(&exclude_path, new)
            .with_context(|| format!("cannot write {}", exclude_path.display()))?;
        println!("✓ appended {line} to {}", exclude_path.display());
    }
    Ok(())
}

/// Quiet, best-effort single-target variant of [`ensure_git_exclude`] for
/// the auto-regen paths (build pipeline, `select-scheme`) that can
/// first-create `buildServer.json` in a repo where setup — and so its
/// `.git/info/exclude` step — never ran (Zed's New Session modal and
/// hand-editing also produce `.zed/debug.json`): git-ignore the generated
/// file so the write does not dirty `git status`. Prints nothing (the
/// pipeline's stdout is the DAP wire); failures are logged and swallowed.
/// Returns whether a line was appended.
pub(crate) fn git_exclude_build_server(dir: &Path) -> bool {
    let run_git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
    };
    if !run_git(&["rev-parse", "--git-dir"]).is_ok_and(|o| o.status.success()) {
        return false; // git missing or not a repo — nothing to dirty
    }
    if run_git(&["check-ignore", "-q", "buildServer.json"]).is_ok_and(|o| o.status.success()) {
        return false; // already git-ignored
    }
    let Ok(out) = run_git(&["rev-parse", "--git-path", "info/exclude"]) else {
        return false;
    };
    let rel = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let exclude_path = if Path::new(&rel).is_absolute() {
        PathBuf::from(rel)
    } else {
        dir.join(rel)
    };
    let existing = fs::read_to_string(&exclude_path).unwrap_or_default();
    if existing
        .lines()
        .any(|l| l.trim().trim_start_matches('/').trim_end_matches('/') == "buildServer.json")
    {
        return false;
    }
    if let Some(parent) = exclude_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let mut new = existing;
    if !new.is_empty() && !new.ends_with('\n') {
        new.push('\n');
    }
    new.push_str("buildServer.json\n");
    match fs::write(&exclude_path, new) {
        Ok(()) => {
            log::info!(
                target: "setup",
                "appended buildServer.json to {}",
                exclude_path.display()
            );
            true
        }
        Err(e) => {
            log::warn!(
                target: "setup",
                "cannot git-ignore buildServer.json ({}): {e}",
                exclude_path.display()
            );
            false
        }
    }
}

/// Opt-in gate shared by the build pipeline's buildServer.json auto-regen,
/// the `select-scheme` regen, and doctor's missing-file escalation:
/// refreshing an existing (stale) buildServer.json is always allowed;
/// recreating a deleted one is allowed only when `.zed/debug.json` holds an
/// `"Xcode"` adapter scenario (written by setup, saved from Zed's New
/// Session modal, or hand-edited — all mean the project uses this adapter).
/// A repo with neither — including one whose debug.json only configures
/// other adapters — never opted in.
pub(crate) fn build_server_opted_in(dir: &Path, build_server: &Path) -> bool {
    build_server.exists() || debug_json_has_xcode_scenario(dir)
}

/// Does `.zed/debug.json` under `dir` contain an `"Xcode"` adapter
/// scenario? Best-effort: an unreadable or unparseable file counts as no.
fn debug_json_has_xcode_scenario(dir: &Path) -> bool {
    let Ok(text) = fs::read_to_string(dir.join(".zed").join("debug.json")) else {
        return false;
    };
    jsonc::parse_jsonc(&text).is_ok_and(|v| {
        v.as_array().is_some_and(|a| {
            a.iter()
                .any(|s| s.get("adapter").and_then(|a| a.as_str()) == Some("Xcode"))
        })
    })
}

/// Locate an executable on PATH (also used by doctor/refresh).
pub fn find_in_path(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file()
            && fs::metadata(&candidate)
                .map(|m| m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        {
            return Some(candidate);
        }
    }
    None
}

/// Generate `buildServer.json` as the last step of `setup --project`
/// (replaces the old external build-server `config` invocation). Non-fatal by
/// design — setup must not die on its last step: a missing workspace or an
/// unresolvable build_root prints a hint and skips.
async fn generate_build_server(dir: &Path, cfg: &ProjectConfig) {
    use crate::setup::build_server::{regenerate, Regen};
    let derived_data = cfg.derived_data.as_deref().map(Path::new);
    match regenerate(
        dir,
        Path::new(&cfg.workspace),
        &cfg.scheme,
        None,
        derived_data,
    )
    .await
    {
        Regen::Written(_) => {
            println!("✓ buildServer.json → sourcekit-lsp go-to-definition ready")
        }
        Regen::MissingWorkspace => {
            println!(
                "! {} does not exist yet — skipping buildServer.json;",
                cfg.workspace
            );
            println!("  run `xcode-dap refresh` after the project is generated.");
        }
        Regen::Failed(e) => {
            println!("! could not generate buildServer.json (non-fatal): {e}");
            println!("  run `xcode-dap refresh` once the project builds.");
        }
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn sandbox() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zedxcode-project-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sample_config() -> ProjectConfig {
        ProjectConfig {
            workspace: "myapp.xcworkspace".into(),
            scheme: "MyApp (staging)".into(),
            device: "iPhone 15 Pro Max".into(),
            os: Some("26.3".into()),
            preflight: Some("make project CI=true".into()),
            oslog: false,
            derived_data: None,
        }
    }

    const SAMPLE_DEBUG_JSON: &str = r#"// Generated by `xcode-dap setup --project`. Edit freely; re-running setup
// backs this file up and rewrites it.
[
  {
    "adapter": "Xcode",
    "label": "Run on simulator",
    "request": "launch",
    "workspace": "$ZED_WORKTREE_ROOT/myapp.xcworkspace",
    "scheme": "MyApp (staging)",
    "device": "iPhone 15 Pro Max",
    "os": "26.3",
    "preflight": "make project CI=true"
  }
]
"#;

    /// Task command used by the byte-expected fixtures (a stable stand-in
    /// for the embedded `current_exe_path()` value).
    const XCODE_DAP_BIN: &str = "/opt/zedxcode/bin/xcode-dap";

    const SAMPLE_TASKS_JSON: &str = r#"// Generated by `xcode-dap setup --project`. Edit freely; re-running setup
// backs this file up and rewrites it.
[
  {
    "label": "Xcode: Build",
    "command": "/opt/zedxcode/bin/xcode-dap",
    "args": ["build", "--workspace", "myapp.xcworkspace", "--scheme", "'MyApp (staging)'", "--device", "'iPhone 15 Pro Max'", "--os", "26.3"],
    "cwd": "$ZED_WORKTREE_ROOT"
  },
  {
    "label": "Xcode: Clean",
    "command": "/opt/zedxcode/bin/xcode-dap",
    "args": ["clean", "--workspace", "myapp.xcworkspace", "--scheme", "'MyApp (staging)'"],
    "cwd": "$ZED_WORKTREE_ROOT"
  },
  {
    "label": "Xcode: Refresh",
    "command": "/opt/zedxcode/bin/xcode-dap",
    "args": ["refresh"],
    "cwd": "$ZED_WORKTREE_ROOT"
  },
  {
    "label": "Xcode: Console",
    "command": "/opt/zedxcode/bin/xcode-dap",
    "args": ["console", "--follow"],
    "cwd": "$ZED_WORKTREE_ROOT"
  },
  {
    "label": "Xcode: Choose Scheme",
    "command": "/opt/zedxcode/bin/xcode-dap",
    "args": ["select-scheme", "--workspace", "myapp.xcworkspace"],
    "cwd": "$ZED_WORKTREE_ROOT"
  },
  {
    "label": "Xcode: Choose Destination",
    "command": "/opt/zedxcode/bin/xcode-dap",
    "args": ["select-device"],
    "cwd": "$ZED_WORKTREE_ROOT"
  }
]
"#;

    #[test]
    fn debug_json_is_byte_expected_for_sample_config() {
        assert_eq!(render_debug_json(&sample_config()), SAMPLE_DEBUG_JSON);
        // valid JSONC; flat scenario keys; LaunchConfig-compatible names
        let v = jsonc::parse_jsonc(SAMPLE_DEBUG_JSON).unwrap();
        let scenario = &v.as_array().unwrap()[0];
        assert_eq!(scenario["adapter"], "Xcode");
        assert_eq!(scenario["label"], "Run on simulator");
        assert_eq!(
            scenario["workspace"],
            "$ZED_WORKTREE_ROOT/myapp.xcworkspace"
        );
        assert_eq!(scenario["preflight"], "make project CI=true");
        assert!(scenario.get("config").is_none(), "keys must be flat");
    }

    #[test]
    fn debug_json_omits_optional_fields() {
        let cfg = ProjectConfig {
            workspace: "App.xcodeproj".into(),
            scheme: "App".into(),
            device: "iPhone 17".into(),
            os: None,
            preflight: None,
            oslog: false,
            derived_data: None,
        };
        let text = render_debug_json(&cfg);
        assert!(!text.contains("\"os\""));
        assert!(!text.contains("\"preflight\""));
        assert!(!text.contains("\"oslog\""));
        assert!(!text.contains("\"derivedData\""));
        jsonc::parse_jsonc(&text).unwrap();
    }

    #[test]
    fn debug_json_includes_oslog_when_enabled() {
        let mut cfg = sample_config();
        cfg.oslog = true;
        let text = render_debug_json(&cfg);
        let v = jsonc::parse_jsonc(&text).unwrap();
        assert_eq!(v.as_array().unwrap()[0]["oslog"], true);
    }

    #[test]
    fn derived_data_flows_into_debug_and_task_json() {
        let mut cfg = sample_config();
        cfg.derived_data = Some("/Users/x/dd".into());

        // debug.json scenario carries "derivedData" (so ⌘R uses it).
        let debug = render_debug_json(&cfg);
        let v = jsonc::parse_jsonc(&debug).unwrap();
        assert_eq!(v.as_array().unwrap()[0]["derivedData"], "/Users/x/dd");

        // Build + Clean tasks pass --derived-data through.
        let tasks = render_tasks_json(&cfg, XCODE_DAP_BIN);
        let v = jsonc::parse_jsonc(&tasks).unwrap();
        let tasks = v.as_array().unwrap();
        for label in ["Xcode: Build", "Xcode: Clean"] {
            let task = tasks
                .iter()
                .find(|t| t["label"] == label)
                .unwrap_or_else(|| panic!("{label} task present"));
            let args: Vec<&str> = task["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a.as_str().unwrap())
                .collect();
            let i = args
                .iter()
                .position(|a| *a == "--derived-data")
                .unwrap_or_else(|| panic!("{label} has --derived-data"));
            assert_eq!(args[i + 1], "/Users/x/dd");
        }
    }

    #[test]
    fn normalize_workspace_flag_relative_stays_verbatim() {
        let dir = sandbox();
        assert_eq!(
            normalize_workspace_flag(&dir, Path::new("app.xcworkspace")),
            "app.xcworkspace"
        );
        assert_eq!(
            normalize_workspace_flag(&dir, Path::new("sub/app.xcodeproj")),
            "sub/app.xcodeproj"
        );
    }

    #[test]
    fn normalize_workspace_flag_strips_project_dir_prefix() {
        // sandbox() lives under the symlinked macOS temp dir, so this also
        // exercises the canonicalization (/var vs /private/var).
        let dir = sandbox();
        fs::create_dir_all(dir.join("sub/app.xcworkspace")).unwrap();
        assert_eq!(
            normalize_workspace_flag(&dir, &dir.join("sub/app.xcworkspace")),
            "sub/app.xcworkspace"
        );
    }

    #[test]
    fn normalize_workspace_flag_keeps_outside_paths_absolute() {
        let dir = sandbox();
        let outside = sandbox().join("other.xcworkspace");
        fs::create_dir_all(&outside).unwrap();
        let norm = normalize_workspace_flag(&dir, &outside);
        assert!(Path::new(&norm).is_absolute(), "{norm}");
        assert!(norm.ends_with("other.xcworkspace"), "{norm}");
    }

    #[test]
    fn debug_json_renders_absolute_workspace_verbatim() {
        let mut cfg = sample_config();
        cfg.workspace = "/elsewhere/other.xcworkspace".into();
        let text = render_debug_json(&cfg);
        assert!(!text.contains("$ZED_WORKTREE_ROOT"), "{text}");
        let v = jsonc::parse_jsonc(&text).unwrap();
        assert_eq!(
            v.as_array().unwrap()[0]["workspace"],
            "/elsewhere/other.xcworkspace"
        );
    }

    #[test]
    fn existing_oslog_is_preserved_on_rerun() {
        let dir = sandbox();
        // No .zed/debug.json yet -> default off.
        assert!(!existing_oslog(&dir));
        // Generated file with oslog enabled -> preserved.
        let mut cfg = sample_config();
        cfg.oslog = true;
        write_zed_files(&dir, &cfg, XCODE_DAP_BIN).unwrap();
        assert!(existing_oslog(&dir));
        // Hand-edited back to false -> preserved as false.
        cfg.oslog = false;
        write_zed_files(&dir, &cfg, XCODE_DAP_BIN).unwrap();
        assert!(!existing_oslog(&dir));
    }

    #[test]
    fn tasks_json_is_byte_expected_for_sample_config() {
        assert_eq!(
            render_tasks_json(&sample_config(), XCODE_DAP_BIN),
            SAMPLE_TASKS_JSON
        );
        let v = jsonc::parse_jsonc(SAMPLE_TASKS_JSON).unwrap();
        let tasks = v.as_array().unwrap();
        assert_eq!(tasks.len(), 6);
        // labels must match the task::Spawn bindings in setup/user.rs
        assert_eq!(tasks[0]["label"], "Xcode: Build");
        assert_eq!(tasks[1]["label"], "Xcode: Clean");
        assert_eq!(tasks[2]["label"], "Xcode: Refresh");
        assert_eq!(tasks[3]["label"], "Xcode: Console");
        assert_eq!(tasks[4]["label"], "Xcode: Choose Scheme");
        assert_eq!(tasks[5]["label"], "Xcode: Choose Destination");
        for t in tasks {
            // absolute path, never a bare `xcode-dap` (exit-127 prevention)
            assert_eq!(t["command"], XCODE_DAP_BIN);
            assert_eq!(t["cwd"], "$ZED_WORKTREE_ROOT");
        }
    }

    #[test]
    fn tasks_json_shell_quotes_command_with_spaces() {
        let text = render_tasks_json(&sample_config(), "/Users/Jane Doe/bin/xcode-dap");
        let v = jsonc::parse_jsonc(&text).unwrap();
        for t in v.as_array().unwrap() {
            assert_eq!(t["command"], "'/Users/Jane Doe/bin/xcode-dap'");
        }
    }

    #[test]
    fn current_exe_path_is_absolute_and_existing() {
        // In tests the current exe is the test binary — still absolute,
        // canonicalized, and existing, which is all the contract requires.
        let path = current_exe_path();
        assert!(Path::new(&path).is_absolute(), "not absolute: {path}");
        assert!(Path::new(&path).exists(), "does not exist: {path}");
    }

    #[test]
    fn write_zed_files_writes_then_noops_then_backs_up_on_change() {
        let dir = sandbox();
        let cfg = sample_config();
        write_zed_files(&dir, &cfg, XCODE_DAP_BIN).unwrap();
        let debug = fs::read_to_string(dir.join(".zed/debug.json")).unwrap();
        let tasks = fs::read_to_string(dir.join(".zed/tasks.json")).unwrap();
        assert_eq!(debug, SAMPLE_DEBUG_JSON);
        assert_eq!(tasks, SAMPLE_TASKS_JSON);

        // idempotent second run: bytes unchanged, no backups
        write_zed_files(&dir, &cfg, XCODE_DAP_BIN).unwrap();
        assert_eq!(
            fs::read_to_string(dir.join(".zed/debug.json")).unwrap(),
            SAMPLE_DEBUG_JSON
        );
        let backups = fs::read_dir(dir.join(".zed"))
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains("zedxcode-backup")
            })
            .count();
        assert_eq!(backups, 0, "no backups for unchanged files");

        // changed config: rewrite + backup of the old file
        let mut changed = cfg.clone();
        changed.scheme = "MyApp (production)".into();
        write_zed_files(&dir, &changed, XCODE_DAP_BIN).unwrap();
        let backups = fs::read_dir(dir.join(".zed"))
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains("zedxcode-backup")
            })
            .count();
        assert_eq!(backups, 2, "both files changed -> 2 backups");
        assert!(fs::read_to_string(dir.join(".zed/debug.json"))
            .unwrap()
            .contains("MyApp (production)"));
    }

    #[test]
    fn git_exclude_appended_once() {
        let dir = sandbox();
        let git_ok = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&dir)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(git_ok, "git init failed in sandbox");
        ensure_git_exclude(&dir).unwrap();
        let exclude = fs::read_to_string(dir.join(".git/info/exclude")).unwrap();
        assert_eq!(exclude.matches(".zed/").count(), 1);
        assert_eq!(exclude.matches("buildServer.json").count(), 1);
        // second run: still exactly one entry each (check-ignore now
        // short-circuits)
        ensure_git_exclude(&dir).unwrap();
        let exclude = fs::read_to_string(dir.join(".git/info/exclude")).unwrap();
        assert_eq!(exclude.matches(".zed/").count(), 1);
        assert_eq!(exclude.matches("buildServer.json").count(), 1);
    }

    #[test]
    fn git_exclude_build_server_appends_once() {
        let dir = sandbox();
        let git_ok = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&dir)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(git_ok, "git init failed in sandbox");
        // First-create path: appends the exclude line (quietly).
        assert!(git_exclude_build_server(&dir));
        let exclude = fs::read_to_string(dir.join(".git/info/exclude")).unwrap();
        assert_eq!(exclude.matches("buildServer.json").count(), 1);
        // Now git-ignored -> a second call is a no-op (no duplicate line).
        assert!(!git_exclude_build_server(&dir));
        let exclude = fs::read_to_string(dir.join(".git/info/exclude")).unwrap();
        assert_eq!(exclude.matches("buildServer.json").count(), 1);
    }

    #[test]
    fn build_server_regen_is_opt_in() {
        let dir = sandbox();
        let build_server = dir.join("buildServer.json");
        // Never ran setup, no buildServer.json -> never create one.
        assert!(!build_server_opted_in(&dir, &build_server));
        // A debug.json without an "Xcode" scenario (empty, or another
        // adapter's — Zed saves every adapter's scenarios there) does not
        // opt the project into buildServer.json writes.
        fs::create_dir_all(dir.join(".zed")).unwrap();
        fs::write(dir.join(".zed/debug.json"), "[]").unwrap();
        assert!(!build_server_opted_in(&dir, &build_server));
        fs::write(
            dir.join(".zed/debug.json"),
            r#"[{ "adapter": "CodeLLDB", "request": "launch" }]"#,
        )
        .unwrap();
        assert!(!build_server_opted_in(&dir, &build_server));
        // An "Xcode" scenario (setup-written with // comments,
        // Zed-modal-saved, or hand-edited) opts in.
        fs::write(
            dir.join(".zed/debug.json"),
            "// generated\n[\n  { \"adapter\": \"Xcode\", \"request\": \"launch\" }\n]\n",
        )
        .unwrap();
        assert!(build_server_opted_in(&dir, &build_server));
        // An existing (stale) buildServer.json alone also counts.
        fs::remove_file(dir.join(".zed/debug.json")).unwrap();
        fs::write(&build_server, "{}").unwrap();
        assert!(build_server_opted_in(&dir, &build_server));
    }

    #[test]
    fn makefile_preflight_detection() {
        let dir = sandbox();
        assert_eq!(makefile_preflight(&dir), None);
        fs::write(dir.join("Makefile"), "build:\n\techo hi\n").unwrap();
        assert_eq!(makefile_preflight(&dir), None);
        fs::write(
            dir.join("Makefile"),
            "project:\n\ttuist generate\n\nbuild:\n\techo hi\n",
        )
        .unwrap();
        assert_eq!(
            makefile_preflight(&dir).as_deref(),
            Some("make project CI=true")
        );
        // `project:=` / `project::=` are variable assignments, not targets.
        fs::write(
            dir.join("Makefile"),
            "project:=MyApp\n\nbuild:\n\techo hi\n",
        )
        .unwrap();
        assert_eq!(makefile_preflight(&dir), None);
        fs::write(dir.join("Makefile"), "project::=MyApp\n").unwrap();
        assert_eq!(makefile_preflight(&dir), None);
    }

    #[test]
    fn shell_quote_quotes_only_when_needed() {
        assert_eq!(shell_quote("myapp.xcworkspace"), "myapp.xcworkspace");
        assert_eq!(shell_quote("26.3"), "26.3");
        assert_eq!(shell_quote("MyApp (staging)"), "'MyApp (staging)'");
        assert_eq!(shell_quote("a'b"), r"'a'\''b'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn find_workspace_prefers_xcworkspace_and_detects_ambiguity() {
        let dir = sandbox();
        fs::create_dir(dir.join("app.xcodeproj")).unwrap();
        assert_eq!(find_workspace(&dir).unwrap(), "app.xcodeproj");
        fs::create_dir(dir.join("app.xcworkspace")).unwrap();
        assert_eq!(find_workspace(&dir).unwrap(), "app.xcworkspace");
        fs::create_dir(dir.join("other.xcworkspace")).unwrap();
        assert!(find_workspace(&dir).is_err());
    }
}
