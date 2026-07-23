//! `xcode-dap doctor` — environment checks: Darwin, Xcode + `xcrun -f
//! lldb-dap`, simctl, sourcekit-lsp, Zed, buildServer.json
//! presence/freshness/contents + `argv` (when in a project), stale
//! pidfiles. Exit code is non-zero when any ✗ check fails.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::bail;

use crate::engine::pipeline::zedxcode_home;
use crate::setup::project::{build_server_opted_in, find_in_path};
use crate::util::paths::{mtime, workspace_mtime};

#[derive(Default)]
struct Doctor {
    failures: usize,
    warnings: usize,
}

impl Doctor {
    fn ok(&mut self, label: &str, detail: &str) {
        println!("✓ {label}{}", fmt_detail(detail));
    }
    fn fail(&mut self, label: &str, detail: &str) {
        self.failures += 1;
        println!("✗ {label}{}", fmt_detail(detail));
    }
    fn warn(&mut self, label: &str, detail: &str) {
        self.warnings += 1;
        println!("! {label}{}", fmt_detail(detail));
    }
    fn note(&mut self, label: &str, detail: &str) {
        println!("– {label}{}", fmt_detail(detail));
    }
}

fn fmt_detail(detail: &str) -> String {
    if detail.is_empty() {
        String::new()
    } else {
        format!(": {detail}")
    }
}

pub async fn run() -> anyhow::Result<()> {
    println!(
        "xcode-dap {} ({}, built {}) — doctor\n",
        env!("CARGO_PKG_VERSION"),
        env!("XCODE_DAP_GIT_HASH"),
        env!("XCODE_DAP_BUILD_TS")
    );
    let mut d = Doctor::default();

    // --- core toolchain -----------------------------------------------------
    if cfg!(target_os = "macos") {
        d.ok("macOS", "");
    } else {
        d.fail("macOS", "xcode-dap is macOS-only");
    }

    match cmd_first_line("xcode-select", &["-p"]).await {
        Some(path) => d.ok("Xcode developer dir", &path),
        None => d.fail(
            "Xcode developer dir",
            "xcode-select -p failed — install Xcode, then `sudo xcode-select -s <path>`",
        ),
    }

    match cmd_first_line("xcodebuild", &["-version"]).await {
        Some(version) => d.ok("xcodebuild", &version),
        None => d.fail("xcodebuild", "not runnable — is Xcode installed?"),
    }

    check_simctl(&mut d).await;

    for (tool, hint) in [
        ("lldb-dap", "ships with Xcode 16+ — update Xcode"),
        ("sourcekit-lsp", "ships with Xcode — update Xcode"),
    ] {
        match cmd_first_line("xcrun", &["-f", tool]).await {
            Some(path) => d.ok(tool, &path),
            None => d.fail(tool, &format!("`xcrun -f {tool}` failed; {hint}")),
        }
    }

    check_zed(&mut d);
    check_zed_binary_override(&mut d);

    // --- xcode-dap itself (warning only: generated tasks embed an
    // absolute binary path and work without PATH) ----------------------------
    match find_in_path("xcode-dap") {
        Some(path) => d.ok("xcode-dap on PATH", &path.display().to_string()),
        None => d.warn(
            "xcode-dap on PATH",
            "not found — terminal use (`xcode-dap doctor`, `xcode-dap console`) \
             needs it; run ./install.sh in the ZedXcode repo",
        ),
    }

    // --- current project ----------------------------------------------------
    check_project(&mut d);

    // --- housekeeping ---------------------------------------------------------
    check_pidfiles(&mut d);
    check_log_file(&mut d);

    println!("\n{} failure(s), {} warning(s)", d.failures, d.warnings);
    if d.failures > 0 {
        bail!("doctor found {} failure(s)", d.failures);
    }
    Ok(())
}

async fn cmd_first_line(bin: &str, args: &[&str]) -> Option<String> {
    let out = tokio::process::Command::new(bin)
        .args(args)
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned();
    (!line.is_empty()).then_some(line)
}

async fn check_simctl(d: &mut Doctor) {
    let out = tokio::process::Command::new("xcrun")
        .args(["simctl", "list", "devices", "--json"])
        .output()
        .await;
    let Ok(out) = out else {
        d.fail("simctl", "xcrun not runnable");
        return;
    };
    if !out.status.success() {
        d.fail(
            "simctl",
            &format!(
                "simctl list failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ),
        );
        return;
    }
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
        d.fail("simctl", "simctl list output is not JSON");
        return;
    };
    let (available, booted) = count_ios_simulators(&v);
    if available == 0 {
        d.fail(
            "simulators",
            "no available iPhone/iPad simulators — install a simulator runtime \
             in Xcode (Settings ▸ Components)",
        );
    } else {
        d.ok(
            "simulators",
            &format!("{available} available, {booted} booted"),
        );
    }
}

/// `(available, booted)` iPhone/iPad simulators from `simctl list devices
/// --json`. Only iOS runtimes and iPhone/iPad device names count — the
/// pipeline's default resolution and `select-device` both require an
/// iPhone/iPad on an iOS runtime (see [`crate::commands::select`]), so a
/// machine with only watchOS/tvOS/visionOS runtimes (or only Apple
/// Watch/TV-style devices) must not read as a passing "simulators" check.
fn count_ios_simulators(v: &serde_json::Value) -> (usize, usize) {
    let Some(devices) = v.get("devices").and_then(|x| x.as_object()) else {
        return (0, 0);
    };
    let mut available = 0usize;
    let mut booted = 0usize;
    for (runtime, list) in devices {
        // "...SimRuntime.iOS-26-3" -> iOS runtimes only.
        if runtime
            .rsplit('.')
            .next()
            .is_none_or(|r| !r.starts_with("iOS-"))
        {
            continue;
        }
        let Some(list) = list.as_array() else {
            continue;
        };
        for dev in list {
            let usable = dev
                .get("isAvailable")
                .and_then(|a| a.as_bool())
                .unwrap_or(false)
                && dev
                    .get("name")
                    .and_then(|n| n.as_str())
                    .is_some_and(|n| n.starts_with("iPhone") || n.starts_with("iPad"));
            if usable {
                available += 1;
                if dev.get("state").and_then(|s| s.as_str()) == Some("Booted") {
                    booted += 1;
                }
            }
        }
    }
    (available, booted)
}

fn check_zed(d: &mut Doctor) {
    let home = std::env::var("HOME").unwrap_or_default();
    let user_apps = PathBuf::from(&home).join("Applications");
    // Zed's release channels each install under their own bundle name
    // (Preview/Nightly/Dev never reuse "Zed.app") — probe them all so doctor
    // does not false-FAIL for a non-stable channel.
    let found = [
        "Zed.app",
        "Zed Preview.app",
        "Zed Nightly.app",
        "Zed Dev.app",
    ]
    .iter()
    .flat_map(|b| [PathBuf::from("/Applications").join(b), user_apps.join(b)])
    .find(|p| p.exists());
    if let Some(found) = found {
        d.ok("Zed", &found.display().to_string());
    } else if let Some(cli) = ["zed", "zed-preview", "zed-nightly"]
        .iter()
        .find_map(|c| find_in_path(c))
    {
        d.ok("Zed", &format!("cli at {}", cli.display()));
    } else {
        d.fail("Zed", "Zed.app not found — install from https://zed.dev");
    }
}

/// `dap.Xcode.binary` in the Zed user settings pins Zed to an absolute
/// adapter path, bypassing the extension's own resolution (PATH / GitHub
/// releases): a dangling path breaks every debug session, and a
/// `target/debug` dev build silently misses later fixes.
fn check_zed_binary_override(d: &mut Doctor) {
    let Ok(dir) = crate::setup::user::zed_config_dir() else {
        d.note(
            "dap.Xcode.binary",
            "cannot locate Zed settings (HOME not set)",
        );
        return;
    };
    let settings_path = dir.join("settings.json");
    let Ok(text) = fs::read_to_string(&settings_path) else {
        d.note(
            "dap.Xcode.binary",
            "no Zed settings.json — extension resolves xcode-dap from PATH / GitHub releases",
        );
        return;
    };
    let Ok(v) = crate::setup::jsonc::parse_jsonc(&text) else {
        d.warn(
            "dap.Xcode.binary",
            &format!(
                "{} does not parse as JSONC — cannot check the override",
                settings_path.display()
            ),
        );
        return;
    };
    let Some(binary) = zed_dap_binary_override(&v) else {
        d.note(
            "dap.Xcode.binary",
            "not set — extension resolves xcode-dap from PATH / GitHub releases",
        );
        return;
    };
    let path = Path::new(&binary);
    match classify_binary_override(path, path.exists()) {
        BinaryOverride::Relative { dev_build } => {
            d.note(
                "dap.Xcode.binary",
                &format!(
                    "{binary} is worktree-relative — Zed resolves it against \
                     each worktree root; cannot verify from here"
                ),
            );
            if dev_build {
                d.warn(
                    "dap.Xcode.binary",
                    &format!(
                        "dev-build override {binary} — this binary goes stale \
                         silently; point it at a stable install (e.g. \
                         $(brew --prefix)/bin/xcode-dap — see install.sh) or \
                         rebuild after every change"
                    ),
                );
            }
        }
        BinaryOverride::Missing => d.fail(
            "dap.Xcode.binary",
            &format!(
                "{binary} does not exist — debug sessions will fail to start; \
                 fix or remove the \"dap\" → \"Xcode\" → \"binary\" override in {}",
                settings_path.display()
            ),
        ),
        BinaryOverride::DevBuild => {
            let built = mtime(path)
                .map(crate::util::logging::format_system_time)
                .unwrap_or_else(|| "unknown".to_string());
            d.warn(
                "dap.Xcode.binary",
                &format!(
                    "dev-build override {binary} (mtime {built}) — this binary goes \
                     stale silently; point it at a stable install (e.g. \
                     $(brew --prefix)/bin/xcode-dap — see install.sh) or rebuild \
                     after every change"
                ),
            );
        }
        BinaryOverride::Ok => d.ok("dap.Xcode.binary", &binary),
    }
}

/// Verdict for a `dap.Xcode.binary` override path.
#[derive(Debug, PartialEq)]
enum BinaryOverride {
    /// Relative — the extension resolves it against each worktree root, so
    /// existence cannot be verified from doctor's cwd (note, plus the
    /// dev-build warning when it points into target/debug).
    Relative {
        dev_build: bool,
    },
    /// Absolute and missing — every debug session fails to start.
    Missing,
    /// Exists but points into target/debug — goes stale silently.
    DevBuild,
    Ok,
}

/// Pure classification of the override (`exists` is the caller's stat, only
/// meaningful for absolute paths).
fn classify_binary_override(path: &Path, exists: bool) -> BinaryOverride {
    let dev_build = path.to_string_lossy().contains("target/debug/");
    if path.is_relative() {
        return BinaryOverride::Relative { dev_build };
    }
    if !exists {
        return BinaryOverride::Missing;
    }
    if dev_build {
        return BinaryOverride::DevBuild;
    }
    BinaryOverride::Ok
}

/// `dap.Xcode.binary` from parsed Zed settings (`None` when not overridden).
fn zed_dap_binary_override(settings: &serde_json::Value) -> Option<String> {
    settings
        .get("dap")?
        .get("Xcode")?
        .get("binary")?
        .as_str()
        .map(str::to_owned)
}

/// In a project dir (has *.xcworkspace / *.xcodeproj / .zed): check
/// buildServer.json presence, freshness relative to the workspace, its
/// recorded build_root (DerivedData) and scheme.
fn check_project(d: &mut Doctor) {
    let Ok(cwd) = std::env::current_dir() else {
        return;
    };
    let workspace = find_container(&cwd);
    let in_project = workspace.is_some() || cwd.join(".zed").is_dir();
    if !in_project {
        d.note(
            "project",
            "cwd is not an Xcode project — skipping project checks",
        );
        return;
    }
    check_tasks_command(d, &cwd);
    let build_server = cwd.join("buildServer.json");
    if !build_server.exists() {
        if missing_build_server_is_failure(&cwd, workspace.is_some()) {
            d.fail(
                "buildServer.json",
                "missing, and a root Package.swift exists — sourcekit-lsp will \
                 fall back to SPM on Package.swift: macOS args, 'Could not load \
                 module' errors, wrong jumps. Fix: `xcode-dap refresh`, then \
                 `editor: restart language server` in Zed (repo clean scripts \
                 may delete buildServer.json)",
            );
        } else {
            d.warn(
                "buildServer.json",
                "missing — run `xcode-dap setup --project .` (or build once to \
                 auto-generate)",
            );
        }
        return;
    }
    match (
        mtime(&build_server),
        workspace.as_deref().and_then(workspace_mtime),
    ) {
        (Some(bs), Some(ws)) if bs < ws => d.warn(
            "buildServer.json",
            "older than the workspace (regenerated project?) — run `xcode-dap refresh`",
        ),
        _ => d.ok("buildServer.json", "present"),
    }
    check_build_server_contents(d, &cwd, &build_server);
}

/// Missing buildServer.json escalates from warn to failure only when a
/// root Package.swift exists next to an actual workspace/project container
/// AND the project opted into the build server (same signal as the build
/// pipeline's [`build_server_opted_in`] regen gate — here, an "Xcode"
/// scenario in `.zed/debug.json`, since buildServer.json itself is
/// missing): sourcekit-lsp then silently serves the (targets-less) package
/// in SPM mode instead of the app targets. Without a container (a pure SPM
/// repo, possibly with a `.zed/` dir) — or in a hybrid package-first repo
/// that ships a demo/example container but never configured this adapter —
/// SPM mode on the root Package.swift is the correct mode: warn only, so
/// doctor keeps exiting 0 there.
fn missing_build_server_is_failure(project_dir: &Path, has_container: bool) -> bool {
    has_container
        && project_dir.join("Package.swift").is_file()
        && build_server_opted_in(project_dir, &project_dir.join("buildServer.json"))
}

/// Parse buildServer.json (plain JSON written by `xcode-dap`) and check what
/// it records: `argv` must launch our bsp Build Server, `build_root` must
/// still hold a build (repo clean scripts wipe DerivedData) or a compile
/// store, and `scheme` must match the effective selected scheme (a stale one
/// answers sourcekit-lsp with another scheme's products).
fn check_build_server_contents(d: &mut Doctor, dir: &Path, path: &Path) {
    let parsed = fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok());
    let Some(v) = parsed else {
        d.warn(
            "buildServer.json",
            "does not parse as JSON — run `xcode-dap refresh` to regenerate it",
        );
        return;
    };
    check_build_server_argv(d, &v);
    let recorded_scheme = v.get("scheme").and_then(serde_json::Value::as_str);
    if let Some(root) = v.get("build_root").and_then(|b| b.as_str()) {
        let root = Path::new(root);
        if !root.is_dir() {
            d.warn(
                "buildServer.json build_root",
                &format!(
                    "{} does not exist — DerivedData wiped; build once to \
                     restore navigation",
                    root.display()
                ),
            );
        } else if !build_root_has_activity_log(root) && compile_store_missing(root, recorded_scheme)
        {
            // CLI builds (`xcode-dap build`/`run`) don't always write a
            // .xcactivitylog; the compile store for (build_root, scheme) is the
            // alternative health signal — only warn when BOTH are absent.
            d.warn(
                "buildServer.json build_root",
                "no *.xcactivitylog under <build_root>/Logs/Build and no compile \
                 store yet — build once (`xcode-dap build`) to populate \
                 go-to-definition",
            );
        }
    }
    let selected = crate::commands::select::current_scheme(dir);
    if let Some((recorded, selected)) = build_server_scheme_mismatch(&v, selected.as_deref()) {
        d.warn(
            "buildServer.json scheme",
            &format!(
                "records \"{recorded}\" but the selected scheme is \"{selected}\" \
                 — go-to-definition answers with the other scheme's products; \
                 run `xcode-dap refresh`"
            ),
        );
    }
}

/// Verdict for a buildServer.json `argv` (`exec` = the caller's
/// [`is_executable`] stat of argv[0]). Mirrors [`classify_binary_override`]:
/// a missing / non-executable argv[0] is fatal (sourcekit-lsp cannot launch
/// the build server), a `target/debug` argv[0] goes stale silently.
#[derive(Debug, PartialEq)]
enum ArgvVerdict {
    /// No argv[0] at all.
    Missing,
    /// argv[0] does not exist or is not executable.
    NotExecutable,
    /// argv[1] is not `"bsp"`.
    WrongSubcommand,
    /// argv[0] points into `target/debug` — goes stale silently.
    DevBuild,
    Ok,
}

/// Pure classification of a buildServer.json `argv`.
fn classify_build_server_argv(argv: &[&str], exec: bool) -> ArgvVerdict {
    let Some(bin) = argv.first() else {
        return ArgvVerdict::Missing;
    };
    if !exec {
        return ArgvVerdict::NotExecutable;
    }
    if argv.get(1) != Some(&"bsp") {
        return ArgvVerdict::WrongSubcommand;
    }
    if bin.contains("target/debug/") {
        return ArgvVerdict::DevBuild;
    }
    ArgvVerdict::Ok
}

/// buildServer.json `argv` checks: argv[0] exists + is executable, argv[1] is
/// `"bsp"`, and argv[0] is not a stale dev build. When it is Ok, note when it
/// differs from the running binary (a possibly-stale path to refresh).
fn check_build_server_argv(d: &mut Doctor, v: &serde_json::Value) {
    let argv: Vec<&str> = v
        .get("argv")
        .and_then(serde_json::Value::as_array)
        .map(|a| a.iter().filter_map(serde_json::Value::as_str).collect())
        .unwrap_or_default();
    let exec = argv
        .first()
        .map(|b| is_executable(Path::new(b)))
        .unwrap_or(false);
    match classify_build_server_argv(&argv, exec) {
        ArgvVerdict::Missing => d.fail(
            "buildServer.json argv",
            "no argv — sourcekit-lsp cannot launch the build server; run \
             `xcode-dap refresh`",
        ),
        ArgvVerdict::NotExecutable => d.fail(
            "buildServer.json argv[0]",
            &format!(
                "{} does not exist or is not executable — cmd+click \
                 go-to-definition will not work; re-run `xcode-dap setup \
                 --project .` (or ./install.sh) then `xcode-dap refresh`",
                argv.first().copied().unwrap_or("")
            ),
        ),
        ArgvVerdict::WrongSubcommand => d.fail(
            "buildServer.json argv",
            "argv[1] is not \"bsp\" — sourcekit-lsp will not start the build \
             server; run `xcode-dap refresh`",
        ),
        ArgvVerdict::DevBuild => d.warn(
            "buildServer.json argv[0]",
            &format!(
                "{} is a dev build (target/debug) — goes stale silently; point \
                 at a stable install (./install.sh) then `xcode-dap refresh`",
                argv[0]
            ),
        ),
        ArgvVerdict::Ok => match std::env::current_exe().and_then(|p| p.canonicalize()) {
            Ok(cur) if cur.to_string_lossy() != argv[0] => d.note(
                "buildServer.json argv[0]",
                &format!(
                    "{} (this xcode-dap is {}) — run `xcode-dap refresh` if \
                         that path is stale",
                    argv[0],
                    cur.display()
                ),
            ),
            _ => d.ok("buildServer.json argv[0]", argv[0]),
        },
    }
}

/// `true` when no compile store exists yet for `(build_root, scheme)` — the
/// alternative health signal to a `.xcactivitylog` (a CLI `xcode-dap build`
/// populates the store from stdout without writing an activity log). An
/// unknown scheme counts as missing.
fn compile_store_missing(build_root: &Path, scheme: Option<&str>) -> bool {
    let Some(scheme) = scheme else {
        return true;
    };
    crate::engine::compile_store::CompileStore::store_path(build_root, scheme)
        .map(|p| !p.exists())
        .unwrap_or(true)
}

/// `true` when `<build_root>/Logs/Build` holds at least one
/// *.xcactivitylog — the signal that DerivedData still contains a build.
fn build_root_has_activity_log(build_root: &Path) -> bool {
    fs::read_dir(build_root.join("Logs").join("Build"))
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .any(|e| e.path().extension().is_some_and(|x| x == "xcactivitylog"))
        })
        .unwrap_or(false)
}

/// `(recorded, selected)` when the parsed buildServer.json records a
/// scheme different from the effective selected one; `None` when they
/// match or either side is unknown.
fn build_server_scheme_mismatch(
    build_server: &serde_json::Value,
    selected: Option<&str>,
) -> Option<(String, String)> {
    let recorded = build_server.get("scheme")?.as_str()?;
    let selected = selected?;
    (recorded != selected).then(|| (recorded.to_owned(), selected.to_owned()))
}

/// The `command` of every task in `.zed/tasks.json` must exist and be
/// executable — a dangling/bare command makes CMD+B exit 127 in Zed's task
/// terminal. Generated files embed an absolute path; older ones used a
/// bare `xcode-dap`, which only works when it resolves on PATH.
fn check_tasks_command(d: &mut Doctor, dir: &Path) {
    let tasks_path = dir.join(".zed").join("tasks.json");
    let Ok(text) = fs::read_to_string(&tasks_path) else {
        return; // no tasks.json — `setup --project` not run yet
    };
    let Ok(v) = crate::setup::jsonc::parse_jsonc(&text) else {
        d.warn(
            ".zed/tasks.json",
            "does not parse as JSONC — tasks will not load",
        );
        return;
    };
    let mut commands: Vec<String> = v
        .as_array()
        .map(|tasks| {
            tasks
                .iter()
                .filter_map(|t| t.get("command").and_then(|c| c.as_str()))
                // Reverse the shell quoting setup added for paths with spaces
                // (or an embedded apostrophe).
                .map(unquote_command)
                .collect()
        })
        .unwrap_or_default();
    commands.sort();
    commands.dedup();
    for command in commands {
        if command.contains('/') {
            if is_executable(Path::new(&command)) {
                d.ok(".zed/tasks.json command", &command);
            } else {
                d.fail(
                    ".zed/tasks.json command",
                    &format!(
                        "{command} does not exist or is not executable — tasks \
                         (CMD+B/CMD+Shift+K) will exit 127; re-run \
                         `xcode-dap setup --project .`"
                    ),
                );
            }
        } else if let Some(resolved) = find_in_path(&command) {
            d.ok(
                ".zed/tasks.json command",
                &format!("{command} (on PATH: {})", resolved.display()),
            );
        } else {
            d.fail(
                ".zed/tasks.json command",
                &format!(
                    "`{command}` is not on PATH — tasks (CMD+B/CMD+Shift+K) will \
                     exit 127; re-run `xcode-dap setup --project .` (embeds the \
                     absolute path) or ./install.sh"
                ),
            );
        }
    }
}

/// Reverse [`crate::setup::project::shell_quote`] for the executability
/// check: an unquoted command is returned as-is; a single-quoted one has its
/// outer quotes removed and the `'\''` escape (an embedded apostrophe)
/// restored. A blanket `trim_matches('\'')` cannot undo that escape and would
/// leave `'\''` mid-string, false-FAILing paths that contain an apostrophe.
fn unquote_command(c: &str) -> String {
    match c.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        Some(inner) => inner.replace(r"'\''", "'"),
        None => c.to_owned(),
    }
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

/// The build container the freshness check compares against: the first
/// `.xcworkspace` in sort order, else the first `.xcodeproj`. Workspaces
/// win regardless of name order (matching `find_workspace` in
/// `setup/project.rs` and `commands/select.rs`) — buildServer.json is
/// generated from the workspace when one exists.
fn find_container(dir: &Path) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .map(|x| x == "xcworkspace" || x == "xcodeproj")
                .unwrap_or(false)
        })
        .collect();
    candidates.sort();
    candidates
        .iter()
        .find(|p| p.extension().is_some_and(|x| x == "xcworkspace"))
        .cloned()
        .or_else(|| candidates.into_iter().next())
}

/// Stale pidfiles under `~/.zedxcode/run` (owner process no longer alive).
fn check_pidfiles(d: &mut Doctor) {
    let Ok(run_dir) = zedxcode_home().map(|home| home.join("run")) else {
        d.ok("pidfiles", "none (~/.zedxcode/run does not exist yet)");
        return;
    };
    let Ok(entries) = fs::read_dir(&run_dir) else {
        d.ok("pidfiles", "none (~/.zedxcode/run does not exist yet)");
        return;
    };
    let mut stale: Vec<String> = vec![];
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        if !name.ends_with(".pid") && !name.ends_with(".dappid") {
            continue;
        }
        let alive = fs::read_to_string(&path)
            .ok()
            .and_then(|t| t.trim().parse::<i32>().ok())
            .map(pid_alive)
            .unwrap_or(false);
        if !alive {
            stale.push(name);
        }
    }
    if stale.is_empty() {
        d.ok("pidfiles", "no stale pidfiles");
    } else {
        d.warn(
            "pidfiles",
            &format!(
                "stale: {} (in {}) — safe to delete",
                stale.join(", "),
                run_dir.display()
            ),
        );
    }
}

/// The proxy's diagnostic log (`~/.zedxcode/logs/xcode-dap.log`): path,
/// size and mtime. Present = ok, absent = note — never a failure.
///
/// `main` runs `logging::init` before doctor, so a live stat always shows
/// the file freshly touched by this very process — report the snapshot
/// init captured *before* opening/rotating instead, falling back to the
/// live stat only when init never got that far.
fn check_log_file(d: &mut Doctor) {
    let Ok(path) = crate::util::logging::log_file_path() else {
        d.note("xcode-dap.log", "HOME not set");
        return;
    };
    if let Some(pre) = crate::util::logging::pre_init_log_file() {
        if pre.existed {
            let modified = pre
                .modified
                .map(crate::util::logging::format_system_time)
                .unwrap_or_else(|| "unknown".to_string());
            d.ok(
                "xcode-dap.log",
                &format!(
                    "{} ({} KB, modified {modified}, before this run)",
                    path.display(),
                    pre.len / 1024
                ),
            );
        } else {
            d.note(
                "xcode-dap.log",
                &format!("not created before this run ({})", path.display()),
            );
        }
        return;
    }
    match fs::metadata(&path) {
        Ok(meta) => {
            let modified = meta
                .modified()
                .map(crate::util::logging::format_system_time)
                .unwrap_or_else(|_| "unknown".to_string());
            d.ok(
                "xcode-dap.log",
                &format!(
                    "{} ({} KB, modified {modified})",
                    path.display(),
                    meta.len() / 1024
                ),
            );
        }
        Err(_) => d.note(
            "xcode-dap.log",
            &format!("not created yet ({})", path.display()),
        ),
    }
}

fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn sandbox() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zedxcode-doctor-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_build_server_escalates_with_root_package_swift() {
        let dir = sandbox();
        // No Package.swift -> stays a warning.
        assert!(!missing_build_server_is_failure(&dir, true));
        // Root Package.swift next to a container but no Xcode scenario in
        // .zed/debug.json (hybrid package-first repo, e.g. an OSS library
        // shipping a demo .xcodeproj) — SPM mode is what that user wants
        // -> stays a warning (doctor exits 0, as at baseline).
        fs::write(dir.join("Package.swift"), "// swift-tools-version:5.9\n").unwrap();
        assert!(!missing_build_server_is_failure(&dir, true));
        // The project opted into the build server (Xcode scenario in
        // .zed/debug.json) -> silent SPM fallback risk -> failure.
        fs::create_dir_all(dir.join(".zed")).unwrap();
        fs::write(dir.join(".zed/debug.json"), r#"[{ "adapter": "Xcode" }]"#).unwrap();
        assert!(missing_build_server_is_failure(&dir, true));
        // Pure SPM repo (no .xcworkspace/.xcodeproj, e.g. .zed-only) —
        // SPM mode is correct there -> stays a warning.
        assert!(!missing_build_server_is_failure(&dir, false));
        // A Package.swift directory (pathological) does not count.
        let other = sandbox();
        fs::create_dir(other.join("Package.swift")).unwrap();
        assert!(!missing_build_server_is_failure(&other, true));
    }

    #[test]
    fn scheme_mismatch_detection() {
        let bs = json!({ "scheme": "MyApp (staging)", "build_root": "/dd" });
        assert_eq!(
            build_server_scheme_mismatch(&bs, Some("MyApp (production)")),
            Some(("MyApp (staging)".into(), "MyApp (production)".into()))
        );
        // Matching scheme, unknown selection, or no recorded scheme -> None.
        assert_eq!(
            build_server_scheme_mismatch(&bs, Some("MyApp (staging)")),
            None
        );
        assert_eq!(build_server_scheme_mismatch(&bs, None), None);
        assert_eq!(build_server_scheme_mismatch(&json!({}), Some("X")), None);
        assert_eq!(
            build_server_scheme_mismatch(&json!({ "scheme": 1 }), Some("X")),
            None
        );
    }

    #[test]
    fn build_root_activity_log_detection() {
        let root = sandbox();
        // No Logs/Build at all -> wiped.
        assert!(!build_root_has_activity_log(&root));
        // Empty Logs/Build -> still wiped.
        let logs = root.join("Logs/Build");
        fs::create_dir_all(&logs).unwrap();
        assert!(!build_root_has_activity_log(&root));
        // Unrelated files don't count; an .xcactivitylog does.
        fs::write(logs.join("LogStoreManifest.plist"), "").unwrap();
        assert!(!build_root_has_activity_log(&root));
        fs::write(logs.join("0-abc.xcactivitylog"), "").unwrap();
        assert!(build_root_has_activity_log(&root));
    }

    #[test]
    fn binary_override_classification() {
        // Relative override: unverifiable from doctor's cwd — never FAIL,
        // even when the stat (against cwd) says it does not exist.
        assert_eq!(
            classify_binary_override(Path::new("target/debug/xcode-dap"), false),
            BinaryOverride::Relative { dev_build: true }
        );
        assert_eq!(
            classify_binary_override(Path::new("bin/xcode-dap"), false),
            BinaryOverride::Relative { dev_build: false }
        );
        // Absolute paths: missing -> FAIL, dev build -> warn, else ok.
        assert_eq!(
            classify_binary_override(Path::new("/nope/xcode-dap"), false),
            BinaryOverride::Missing
        );
        assert_eq!(
            classify_binary_override(Path::new("/repo/target/debug/xcode-dap"), true),
            BinaryOverride::DevBuild
        );
        assert_eq!(
            classify_binary_override(Path::new("/opt/homebrew/bin/xcode-dap"), true),
            BinaryOverride::Ok
        );
    }

    #[test]
    fn build_server_argv_classification() {
        // Missing / non-executable argv[0] -> fatal.
        assert_eq!(classify_build_server_argv(&[], false), ArgvVerdict::Missing);
        assert_eq!(
            classify_build_server_argv(&["/opt/xcode-dap", "bsp"], false),
            ArgvVerdict::NotExecutable
        );
        // Executable but wrong subcommand -> fatal.
        assert_eq!(
            classify_build_server_argv(&["/opt/xcode-dap", "doctor"], true),
            ArgvVerdict::WrongSubcommand
        );
        assert_eq!(
            classify_build_server_argv(&["/opt/xcode-dap"], true),
            ArgvVerdict::WrongSubcommand
        );
        // Executable target/debug build with bsp -> warn.
        assert_eq!(
            classify_build_server_argv(&["/repo/target/debug/xcode-dap", "bsp"], true),
            ArgvVerdict::DevBuild
        );
        // Stable install with bsp -> ok.
        assert_eq!(
            classify_build_server_argv(&["/opt/homebrew/bin/xcode-dap", "bsp"], true),
            ArgvVerdict::Ok
        );
    }

    #[test]
    fn compile_store_missing_signal() {
        // Unknown scheme -> treated as missing.
        assert!(compile_store_missing(Path::new("/dd"), None));
        // A build_root/scheme with no store file on disk -> missing (the
        // store lives under $HOME/.zedxcode/cache and is not created here).
        assert!(compile_store_missing(
            Path::new("/nonexistent-build-root-xyz"),
            Some("MyApp")
        ));
    }

    #[test]
    fn ios_simulator_count_ignores_non_ios_and_non_iphone_ipad() {
        let v = json!({
            "devices": {
                "com.apple.CoreSimulator.SimRuntime.iOS-26-3": [
                    { "name": "iPhone 16 Pro", "state": "Booted", "isAvailable": true },
                    { "name": "iPad Pro 13-inch (M4)", "state": "Shutdown", "isAvailable": true },
                    { "name": "iPhone 17", "state": "Shutdown", "isAvailable": false }
                ],
                "com.apple.CoreSimulator.SimRuntime.watchOS-11-0": [
                    { "name": "Apple Watch Ultra 2 (49mm)", "state": "Booted", "isAvailable": true }
                ]
            }
        });
        // One iPhone + one iPad available, one booted; the unavailable iPhone
        // and the (booted) watchOS device are excluded.
        assert_eq!(count_ios_simulators(&v), (2, 1));

        // A machine with only a watchOS runtime reads as zero usable
        // simulators (the case doctor previously false-PASSed).
        let watch_only = json!({
            "devices": {
                "com.apple.CoreSimulator.SimRuntime.watchOS-11-0": [
                    { "name": "Apple Watch Ultra 2 (49mm)", "state": "Shutdown", "isAvailable": true }
                ]
            }
        });
        assert_eq!(count_ios_simulators(&watch_only), (0, 0));
    }

    #[test]
    fn unquote_command_reverses_shell_quote() {
        use crate::setup::project::shell_quote;
        // Round-trips the writer side, including a path with an embedded
        // apostrophe (the case a blanket trim_matches('\'') mangled).
        for s in [
            "/opt/homebrew/bin/xcode-dap",
            "/a b/target/release/xcode-dap",
            "/Users/x/O'Brien dev/xcode-dap",
        ] {
            assert_eq!(unquote_command(&shell_quote(s)), s);
        }
        // A bare (unquoted) command is returned unchanged.
        assert_eq!(unquote_command("xcode-dap"), "xcode-dap");
    }

    #[test]
    fn binary_override_extracted_when_set() {
        let v = json!({ "dap": { "Xcode": { "binary": "/opt/homebrew/bin/xcode-dap" } } });
        assert_eq!(
            zed_dap_binary_override(&v).as_deref(),
            Some("/opt/homebrew/bin/xcode-dap")
        );
    }

    #[test]
    fn binary_override_none_when_unset_or_wrong_shape() {
        assert_eq!(zed_dap_binary_override(&json!({})), None);
        assert_eq!(zed_dap_binary_override(&json!({ "dap": {} })), None);
        assert_eq!(
            zed_dap_binary_override(&json!({ "dap": { "Xcode": {} } })),
            None
        );
        // Non-string binary is treated as unset.
        assert_eq!(
            zed_dap_binary_override(&json!({ "dap": { "Xcode": { "binary": 1 } } })),
            None
        );
        // A different adapter's override must not match.
        assert_eq!(
            zed_dap_binary_override(&json!({ "dap": { "CodeLLDB": { "binary": "/x" } } })),
            None
        );
    }
}
