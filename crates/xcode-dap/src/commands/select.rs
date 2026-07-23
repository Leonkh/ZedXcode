//! `xcode-dap select-scheme` / `select-device` — Xcode-like scheme and
//! destination pickers for a Zed task terminal. Both write the runtime
//! selection overlay (`.zed/.zedx/selection.json`, see `engine/selection.rs`)
//! which the engine re-reads on every build/run/clean and DAP launch, so a
//! new selection applies to the next cmd-r / cmd-b without touching
//! `.zed/debug.json` or `.zed/tasks.json`.
//!
//! Picker UX: print a numbered list (current selection marked), then read
//! stdin line by line — text filters the list and reprints it, a number
//! selects, `q` quits. Works on a tty and piped (`printf "pro\n2\n" | ...`).
//! Non-interactive paths: `--set <name>` and `--list`.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::process::Command;

use super::refresh;
use crate::engine::pipeline::zedxcode_home;
use crate::engine::selection;
use crate::setup::build_server::{regenerate, Regen};
use crate::setup::jsonc;
use crate::setup::project::{build_server_opted_in, git_exclude_build_server};
use crate::util::hash::fnv1a64;
use crate::util::paths::container_flag;

// ---------------------------------------------------------------------------
// select-scheme
// ---------------------------------------------------------------------------

#[derive(clap::Args, Debug)]
pub struct SelectSchemeArgs {
    /// Path to .xcworkspace / .xcodeproj (default: auto-detect in the project root)
    #[arg(long, short = 'w')]
    pub workspace: Option<PathBuf>,
    /// Set the scheme non-interactively (exact name, case-insensitive)
    #[arg(long)]
    pub set: Option<String>,
    /// Print the scheme list (one per line) and exit
    #[arg(long)]
    pub list: bool,
}

pub async fn run_select_scheme(args: SelectSchemeArgs) -> Result<()> {
    let project = project_dir()?;
    let workspace = match args.workspace {
        Some(w) => std::path::absolute(&w).unwrap_or(w),
        None => find_workspace(&project)?,
    };
    let schemes = list_schemes(&workspace).await?;

    if args.list {
        for s in &schemes {
            println!("{s}");
        }
        return Ok(());
    }

    let chosen = if let Some(query) = args.set {
        match schemes
            .iter()
            .find(|s| **s == query)
            .or_else(|| schemes.iter().find(|s| s.eq_ignore_ascii_case(&query)))
        {
            Some(s) => s.clone(),
            None => {
                let near: Vec<&str> = schemes
                    .iter()
                    .filter(|s| s.to_lowercase().contains(&query.to_lowercase()))
                    .take(10)
                    .map(String::as_str)
                    .collect();
                let hint = if near.is_empty() {
                    format!(
                        "run `xcode-dap select-scheme --list` to see all \
                         {} schemes",
                        schemes.len()
                    )
                } else {
                    format!("did you mean:\n  {}", near.join("\n  "))
                };
                bail!(
                    "no scheme named \"{query}\" in {} — {hint}",
                    workspace.display()
                );
            }
        }
    } else {
        let current = current_scheme(&project);
        let items: Vec<Item> = schemes
            .iter()
            .map(|s| Item {
                label: s.clone(),
                marked: false,
                current: current.as_deref() == Some(s.as_str()),
            })
            .collect();
        println!(
            "{} schemes in {}:",
            items.len(),
            workspace.file_name().unwrap_or_default().to_string_lossy()
        );
        match pick_interactive(&items, "Scheme")? {
            Some(i) => schemes[i].clone(),
            None => {
                println!("No scheme selected — selection unchanged.");
                return Ok(());
            }
        }
    };

    let mut sel = selection::load(&project);
    sel.scheme = Some(chosen.clone());
    let path = selection::save(&project, &sel)?;
    println!("✓ Scheme: {chosen}");
    println!(
        "  Saved to {} — applies to the next build/run (cmd-b / cmd-r).",
        path.display()
    );
    // buildServer.json records the scheme — a stale one keeps answering
    // sourcekit-lsp with another scheme's products, so regenerate it now.
    // Same opt-in gate as the build pipeline's auto-regen: never
    // first-create the file in a repo that never configured the Xcode
    // adapter, and git-ignore it when a first-create does happen (setup's
    // .git/info/exclude step never ran there).
    let ws_dir = workspace.parent().unwrap_or(&project);
    let build_server = ws_dir.join("buildServer.json");
    if build_server_opted_in(&project, &build_server) {
        // Expand $ZED_WORKTREE_ROOT and anchor relative values to `project`
        // (not the cwd — select-scheme can run from a subdirectory), the same
        // way `refresh` handles this field. Passing it raw would make
        // resolve_build_root record a bogus build_root in buildServer.json.
        let derived_data = debug_json_str(&project, "derivedData")
            .map(|d| refresh::expand_worktree_root(&d, &project));
        let dd = derived_data.as_deref();
        if let Regen::Written(outcome) = regenerate(ws_dir, &workspace, &chosen, None, dd).await {
            if outcome.first_create() {
                git_exclude_build_server(ws_dir);
            }
            // A scheme-only change needs no restart: bsp reloads
            // buildServer.json on the mtime change and pushes
            // buildTarget/didChange, so sourcekit-lsp re-queries without one
            // (outcome.restart_hint is false then). A first-create still hints.
            if outcome.restart_hint {
                println!(
                    "  In Zed: command palette → `editor: restart language server` to pick it up."
                );
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// select-device
// ---------------------------------------------------------------------------

#[derive(clap::Args, Debug)]
pub struct SelectDeviceArgs {
    /// Set non-interactively: device name, UDID, or "booted"
    #[arg(long)]
    pub set: Option<String>,
    /// Print the device list (one per line) and exit
    #[arg(long)]
    pub list: bool,
}

pub async fn run_select_device(args: SelectDeviceArgs) -> Result<()> {
    let project = project_dir()?;
    let devices = list_devices().await?;
    if devices.is_empty() {
        bail!(
            "no available iPhone/iPad simulators found — install a simulator \
             runtime in Xcode (Settings ▸ Components) and try again"
        );
    }

    if args.list {
        for d in &devices {
            println!(
                "{} — iOS {} — {}{}",
                d.name,
                d.os,
                d.udid,
                if d.booted { " (booted)" } else { "" }
            );
        }
        return Ok(());
    }

    let chosen: SimDevice = if let Some(query) = args.set {
        match find_device(&devices, &query) {
            Some(d) => d.clone(),
            None => bail!(
                "no simulator matching \"{query}\" — run \
                 `xcode-dap select-device --list` to see what is available \
                 (names, UDIDs, or \"booted\" work)"
            ),
        }
    } else {
        let current = current_destination(&project);
        let items: Vec<Item> = devices
            .iter()
            .map(|d| Item {
                label: format!("{} — iOS {}", d.name, d.os),
                marked: d.booted,
                current: current
                    .as_ref()
                    .is_some_and(|(dev, os)| device_matches(d, dev, os.as_deref())),
            })
            .collect();
        println!("{} simulators (● = booted):", items.len());
        match pick_interactive(&items, "Destination")? {
            Some(i) => devices[i].clone(),
            None => {
                println!("No destination selected — selection unchanged.");
                return Ok(());
            }
        }
    };

    // Persist name + os (human-readable, survives device re-creation); fall
    // back to the UDID only when the (name, os) pair is ambiguous.
    let ambiguous = devices
        .iter()
        .filter(|d| d.name == chosen.name && d.os == chosen.os)
        .count()
        > 1;
    let mut sel = selection::load(&project);
    sel.device = Some(if ambiguous {
        chosen.udid.clone()
    } else {
        chosen.name.clone()
    });
    sel.os = Some(chosen.os.clone());
    let path = selection::save(&project, &sel)?;
    println!("✓ Destination: {} (iOS {})", chosen.name, chosen.os);
    println!(
        "  Saved to {} — applies to the next build/run (cmd-b / cmd-r).",
        path.display()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// project / current-selection helpers
// ---------------------------------------------------------------------------

fn project_dir() -> Result<PathBuf> {
    selection::find_project_dir_from_cwd().with_context(|| {
        format!(
            "this doesn't look like a Zed project: no .zed/ directory found \
             in {} or any parent — run this from your project (or run \
             `xcode-dap setup --project .` there first)",
            std::env::current_dir()
                .map(|d| d.display().to_string())
                .unwrap_or_else(|_| "the current directory".into())
        )
    })
}

/// The single top-level `*.xcworkspace` (preferred) or `*.xcodeproj`.
fn find_workspace(project: &Path) -> Result<PathBuf> {
    let mut workspaces = vec![];
    let mut projects = vec![];
    for entry in
        std::fs::read_dir(project).with_context(|| format!("cannot read {}", project.display()))?
    {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if name.ends_with(".xcworkspace") {
            workspaces.push(name);
        } else if name.ends_with(".xcodeproj") {
            projects.push(name);
        }
    }
    workspaces.sort();
    projects.sort();
    let found = match (workspaces.len(), projects.len()) {
        (1, _) => workspaces.remove(0),
        (0, 1) => projects.remove(0),
        (0, 0) => bail!(
            "no .xcworkspace/.xcodeproj found in {} — generate the project \
             first (e.g. `make project CI=true`) or pass --workspace",
            project.display()
        ),
        _ => {
            // Ambiguous: several workspaces, or no workspace but several
            // projects — report whichever set the user has to pick from.
            let (kind, names) = if workspaces.is_empty() {
                (".xcodeproj", &projects)
            } else {
                (".xcworkspace", &workspaces)
            };
            bail!(
                "multiple {kind} files found in {} ({}) — pass --workspace",
                project.display(),
                names.join(", ")
            )
        }
    };
    Ok(project.join(found))
}

/// Current scheme for the "(current)" marker: the overlay's scheme, else the
/// `.zed/debug.json` scenario's (best-effort). Also the effective scheme
/// `doctor` compares buildServer.json against.
pub(crate) fn current_scheme(project: &Path) -> Option<String> {
    selection::load(project)
        .scheme
        .or_else(|| debug_json_str(project, "scheme"))
}

/// Current destination (`device`, `os`) for the "(current)" marker.
fn current_destination(project: &Path) -> Option<(String, Option<String>)> {
    let sel = selection::load(project);
    if let Some(device) = sel.device {
        return Some((device, sel.os));
    }
    debug_json_str(project, "device").map(|d| (d, debug_json_str(project, "os")))
}

/// Best-effort string field from the first `"Xcode"` scenario in
/// `.zed/debug.json` (other adapters' scenarios are ignored, matching
/// `refresh`'s scenario lookup).
fn debug_json_str(project: &Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(project.join(".zed").join("debug.json")).ok()?;
    let v = jsonc::parse_jsonc(&text).ok()?;
    v.as_array()?
        .iter()
        .filter(|s| s.get("adapter").and_then(Value::as_str) == Some("Xcode"))
        .find_map(|s| s.get(key)?.as_str().map(str::to_owned))
}

// ---------------------------------------------------------------------------
// scheme listing (xcodebuild -list -json, mtime-keyed cache)
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize)]
struct SchemesCache {
    workspace_mtime: u64,
    schemes: Vec<String>,
}

/// Schemes of `workspace`, cached in
/// `~/.zedxcode/cache/schemes-<hash>.json` keyed on the workspace mtime
/// (`xcodebuild -list` takes ~10s+ on large workspaces — they can have
/// hundreds of schemes — and the list only changes on project
/// regeneration). Deleting `~/.zedxcode/cache` clears the cache.
async fn list_schemes(workspace: &Path) -> Result<Vec<String>> {
    let ws = std::path::absolute(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let mtime = workspace_mtime(&ws).with_context(|| {
        format!(
            "workspace {} does not exist — generate the project first \
             (e.g. `make project CI=true`) or pass --workspace",
            ws.display()
        )
    })?;

    let cache_dir = zedxcode_home()?.join("cache");
    std::fs::create_dir_all(&cache_dir)
        .with_context(|| format!("creating {}", cache_dir.display()))?;
    let cache_file = cache_dir.join(format!(
        "schemes-{:016x}.json",
        fnv1a64(ws.as_os_str().as_encoded_bytes())
    ));
    if let Some(cache) = std::fs::read(&cache_file)
        .ok()
        .and_then(|b| serde_json::from_slice::<SchemesCache>(&b).ok())
    {
        if cache.workspace_mtime == mtime && !cache.schemes.is_empty() {
            return Ok(cache.schemes);
        }
    }

    eprintln!("Listing schemes via `xcodebuild -list` (slow the first time, then cached)...");
    let out = Command::new("xcodebuild")
        .args(["-list", "-json", container_flag(&ws)])
        .arg(&ws)
        .kill_on_drop(true)
        .output()
        .await
        .context("failed to run `xcodebuild -list` — is Xcode installed?")?;
    if !out.status.success() {
        bail!(
            "`xcodebuild -list` failed for {}:\n{}",
            ws.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let schemes = parse_schemes(&out.stdout)
        .with_context(|| format!("unexpected `xcodebuild -list` output for {}", ws.display()))?;

    let _ = serde_json::to_vec(&SchemesCache {
        workspace_mtime: mtime,
        schemes: schemes.clone(),
    })
    .map(|bytes| std::fs::write(&cache_file, bytes)); // best-effort cache
    Ok(schemes)
}

/// `xcodebuild -list -json` stdout -> scheme names.
fn parse_schemes(bytes: &[u8]) -> Result<Vec<String>> {
    let v: Value = serde_json::from_slice(bytes).context("output is not JSON")?;
    let schemes: Vec<String> = v
        .get("workspace")
        .or_else(|| v.get("project"))
        .and_then(|c| c.get("schemes"))
        .and_then(Value::as_array)
        .context("no `schemes` array in output")?
        .iter()
        .filter_map(|s| s.as_str().map(str::to_owned))
        .collect();
    if schemes.is_empty() {
        bail!("the scheme list is empty");
    }
    Ok(schemes)
}

fn path_mtime(p: &Path) -> Result<u64> {
    let mtime = std::fs::metadata(p)?.modified()?;
    Ok(mtime
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs())
}

/// Cache key: the newest of the container dir's own mtime and its
/// `contents.xcworkspacedata` mtime. Tuist regenerates that file in place
/// without bumping the directory mtime, so the dir mtime alone would keep
/// serving a stale scheme list after regeneration (same freshness logic as
/// `doctor`'s buildServer.json check). Errors when the container is missing.
fn workspace_mtime(ws: &Path) -> Result<u64> {
    let own = path_mtime(ws)?;
    let contents = path_mtime(&ws.join("contents.xcworkspacedata")).unwrap_or(0);
    Ok(own.max(contents))
}

// ---------------------------------------------------------------------------
// device listing (simctl list devices --json)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct SimDevice {
    name: String,
    udid: String,
    /// "26.3"
    os: String,
    version: (u32, u32),
    booted: bool,
}

async fn list_devices() -> Result<Vec<SimDevice>> {
    let out = Command::new("xcrun")
        .args(["simctl", "list", "devices", "--json"])
        .kill_on_drop(true)
        .output()
        .await
        .context("failed to run `xcrun simctl list devices` — is Xcode installed?")?;
    if !out.status.success() {
        bail!(
            "`xcrun simctl list devices` failed:\n{}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let json: Value =
        serde_json::from_slice(&out.stdout).context("simctl device list is not JSON")?;
    Ok(parse_devices(&json))
}

/// Available iPhones + iPads from the parsed `simctl list devices --json`,
/// sorted booted-first, then newest OS, then iPhones before iPads, then name.
fn parse_devices(json: &Value) -> Vec<SimDevice> {
    let Some(devices) = json.get("devices").and_then(Value::as_object) else {
        return vec![];
    };
    let mut out: Vec<SimDevice> = Vec::new();
    for (runtime, devs) in devices {
        // iOS runtimes only; "...SimRuntime.iOS-26-3" -> "26.3".
        let Some(os) = runtime
            .rsplit('.')
            .next()
            .and_then(|r| r.strip_prefix("iOS-"))
            .map(|r| r.replace('-', "."))
        else {
            continue;
        };
        let mut parts = os.split('.').filter_map(|p| p.parse::<u32>().ok());
        let version = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
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
            if !(name.starts_with("iPhone") || name.starts_with("iPad")) {
                continue;
            }
            out.push(SimDevice {
                name: name.to_string(),
                udid: udid.to_string(),
                os: os.clone(),
                version,
                booted: state == "Booted",
            });
        }
    }
    out.sort_by(|a, b| {
        b.booted
            .cmp(&a.booted)
            .then(b.version.cmp(&a.version))
            .then(a.name.starts_with("iPad").cmp(&b.name.starts_with("iPad")))
            .then(a.name.cmp(&b.name))
            .then(a.udid.cmp(&b.udid))
    });
    out
}

/// `--set` resolution: UDID, exact name (first in sorted order wins, i.e.
/// booted/newest), or the special query "booted".
fn find_device<'a>(devices: &'a [SimDevice], query: &str) -> Option<&'a SimDevice> {
    devices
        .iter()
        .find(|d| d.udid.eq_ignore_ascii_case(query))
        .or_else(|| {
            devices
                .iter()
                .find(|d| query.eq_ignore_ascii_case("booted") && d.booted)
        })
        .or_else(|| devices.iter().find(|d| d.name.eq_ignore_ascii_case(query)))
}

/// Does this device match a persisted/current `device` (+ optional `os`)?
fn device_matches(d: &SimDevice, device: &str, os: Option<&str>) -> bool {
    (device.eq_ignore_ascii_case(&d.name) || device.eq_ignore_ascii_case(&d.udid))
        && os.is_none_or(|o| o == d.os)
}

// ---------------------------------------------------------------------------
// the interactive picker
// ---------------------------------------------------------------------------

struct Item {
    /// Display label; also the filter target.
    label: String,
    /// Prefix with "● " (booted simulator).
    marked: bool,
    /// Suffix with "  (current)".
    current: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum Input {
    /// 1-based row in the currently shown (filtered) list.
    Select(usize),
    Filter(String),
    Quit,
}

/// One stdin line -> picker action. A number within the shown range
/// selects; `q`/`quit` quits; anything else (including out-of-range
/// numbers and the empty line) is a filter over the full list.
fn parse_input(line: &str, shown: usize) -> Input {
    let t = line.trim();
    if t.eq_ignore_ascii_case("q") || t.eq_ignore_ascii_case("quit") {
        return Input::Quit;
    }
    if let Ok(n) = t.parse::<usize>() {
        if (1..=shown).contains(&n) {
            return Input::Select(n);
        }
    }
    Input::Filter(t.to_string())
}

/// Case-insensitive substring filter; returns indices into `items`.
/// The empty query matches everything.
fn filter_indices(items: &[Item], query: &str) -> Vec<usize> {
    let q = query.to_lowercase();
    items
        .iter()
        .enumerate()
        .filter(|(_, it)| it.label.to_lowercase().contains(&q))
        .map(|(i, _)| i)
        .collect()
}

fn print_items(items: &[Item], view: &[usize], any_marked: bool) {
    let width = view.len().to_string().len();
    for (row, &i) in view.iter().enumerate() {
        let it = &items[i];
        let mark = if !any_marked {
            ""
        } else if it.marked {
            "● "
        } else {
            "  "
        };
        let current = if it.current { "  (current)" } else { "" };
        println!("{:>width$}. {mark}{}{current}", row + 1, it.label);
    }
}

/// The interactive loop. Returns the chosen index into `items`, or `None`
/// on quit / stdin EOF. Reads stdin lines, so it works both on a tty and
/// piped (`printf "pro max\n1\n" | xcode-dap select-device`).
fn pick_interactive(items: &[Item], what: &str) -> Result<Option<usize>> {
    let any_marked = items.iter().any(|i| i.marked);
    let mut view: Vec<usize> = (0..items.len()).collect();
    print_items(items, &view, any_marked);

    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    loop {
        print!("{what} — type to filter, number to select, q to quit: ");
        std::io::stdout().flush().ok();
        let Some(line) = lines.next() else {
            return Ok(None); // stdin EOF
        };
        match parse_input(&line.context("reading stdin")?, view.len()) {
            Input::Quit => return Ok(None),
            Input::Select(row) => return Ok(Some(view[row - 1])),
            Input::Filter(q) => {
                view = filter_indices(items, &q);
                if view.is_empty() {
                    println!("(no matches for \"{q}\" — type another filter)");
                } else {
                    print_items(items, &view, any_marked);
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn sandbox() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zedxcode-select-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn items(labels: &[&str]) -> Vec<Item> {
        labels
            .iter()
            .map(|l| Item {
                label: l.to_string(),
                marked: false,
                current: false,
            })
            .collect()
    }

    #[test]
    fn filter_is_case_insensitive_substring() {
        let it = items(&[
            "MyApp (staging)",
            "MyApp (production)",
            "NotificationService",
        ]);
        assert_eq!(filter_indices(&it, "myapp"), vec![0, 1]);
        assert_eq!(filter_indices(&it, "STAG"), vec![0]);
        assert_eq!(filter_indices(&it, "service"), vec![2]);
        assert_eq!(filter_indices(&it, "nope"), Vec::<usize>::new());
        // Empty query shows everything.
        assert_eq!(filter_indices(&it, ""), vec![0, 1, 2]);
    }

    #[test]
    fn input_parsing() {
        assert_eq!(parse_input("2", 3), Input::Select(2));
        assert_eq!(parse_input("  3 ", 3), Input::Select(3));
        assert_eq!(parse_input("q", 3), Input::Quit);
        assert_eq!(parse_input("Quit", 3), Input::Quit);
        // Out-of-range numbers are filters, not selections.
        assert_eq!(parse_input("4", 3), Input::Filter("4".into()));
        assert_eq!(parse_input("0", 3), Input::Filter("0".into()));
        assert_eq!(parse_input("pro max", 3), Input::Filter("pro max".into()));
        assert_eq!(parse_input("", 3), Input::Filter(String::new()));
    }

    #[test]
    fn schemes_parse_workspace_and_project_shapes() {
        let ws = json!({ "workspace": { "name": "myapp", "schemes": ["A", "B"] } });
        assert_eq!(
            parse_schemes(serde_json::to_vec(&ws).unwrap().as_slice()).unwrap(),
            vec!["A", "B"]
        );
        let proj = json!({ "project": { "name": "app", "schemes": ["Only"] } });
        assert_eq!(
            parse_schemes(serde_json::to_vec(&proj).unwrap().as_slice()).unwrap(),
            vec!["Only"]
        );
        assert!(parse_schemes(b"not json").is_err());
        let empty = json!({ "workspace": { "schemes": [] } });
        assert!(parse_schemes(serde_json::to_vec(&empty).unwrap().as_slice()).is_err());
    }

    fn device_fixture() -> Value {
        json!({
            "devices": {
                "com.apple.CoreSimulator.SimRuntime.iOS-18-2": [
                    { "udid": "AAAA", "name": "iPhone 15 Pro Max",
                      "state": "Shutdown", "isAvailable": true },
                    { "udid": "BBBB", "name": "iPhone SE (3rd generation)",
                      "state": "Booted", "isAvailable": true }
                ],
                "com.apple.CoreSimulator.SimRuntime.iOS-26-3": [
                    { "udid": "CCCC", "name": "iPhone 16 Pro",
                      "state": "Shutdown", "isAvailable": true },
                    { "udid": "DDDD", "name": "iPad Pro 13-inch (M4)",
                      "state": "Shutdown", "isAvailable": true },
                    { "udid": "EEEE", "name": "iPhone 17", // unavailable
                      "state": "Shutdown", "isAvailable": false }
                ],
                "com.apple.CoreSimulator.SimRuntime.watchOS-11-0": [
                    { "udid": "WWWW", "name": "Apple Watch Ultra 2 (49mm)",
                      "state": "Shutdown", "isAvailable": true }
                ]
            }
        })
    }

    #[test]
    fn devices_filtered_and_sorted_booted_then_newest_then_iphone_first() {
        let devs = parse_devices(&device_fixture());
        let order: Vec<(&str, &str, bool)> = devs
            .iter()
            .map(|d| (d.name.as_str(), d.os.as_str(), d.booted))
            .collect();
        assert_eq!(
            order,
            vec![
                ("iPhone SE (3rd generation)", "18.2", true), // booted first
                ("iPhone 16 Pro", "26.3", false),             // newest OS
                ("iPad Pro 13-inch (M4)", "26.3", false),     // iPads after iPhones
                ("iPhone 15 Pro Max", "18.2", false),
            ]
        );
        // Unavailable device and watchOS runtime are excluded.
        assert!(!devs.iter().any(|d| d.udid == "EEEE" || d.udid == "WWWW"));
    }

    #[test]
    fn find_device_by_udid_name_and_booted() {
        let devs = parse_devices(&device_fixture());
        assert_eq!(find_device(&devs, "cccc").unwrap().name, "iPhone 16 Pro");
        assert_eq!(
            find_device(&devs, "iphone 15 pro max").unwrap().udid,
            "AAAA"
        );
        assert_eq!(
            find_device(&devs, "booted").unwrap().name,
            "iPhone SE (3rd generation)"
        );
        assert!(find_device(&devs, "iPhone 99").is_none());
    }

    #[test]
    fn device_matching_for_current_marker() {
        let devs = parse_devices(&device_fixture());
        let pro16 = devs.iter().find(|d| d.udid == "CCCC").unwrap();
        assert!(device_matches(pro16, "iPhone 16 Pro", Some("26.3")));
        assert!(device_matches(pro16, "iPhone 16 Pro", None));
        assert!(device_matches(pro16, "CCCC", Some("26.3")));
        assert!(!device_matches(pro16, "iPhone 16 Pro", Some("18.2")));
        assert!(!device_matches(pro16, "iPhone 15 Pro Max", None));
    }

    #[test]
    fn find_workspace_reports_ambiguous_projects_when_no_workspace() {
        let dir = sandbox();
        fs::create_dir(dir.join("a.xcodeproj")).unwrap();
        fs::create_dir(dir.join("b.xcodeproj")).unwrap();
        // No workspace + several projects: the error must list the actual
        // ambiguous .xcodeproj candidates, not an empty workspace list.
        let err = find_workspace(&dir).unwrap_err().to_string();
        assert!(err.contains(".xcodeproj"), "{err}");
        assert!(err.contains("a.xcodeproj, b.xcodeproj"), "{err}");
        // A single workspace still wins over any number of projects.
        fs::create_dir(dir.join("c.xcworkspace")).unwrap();
        assert_eq!(find_workspace(&dir).unwrap(), dir.join("c.xcworkspace"));
    }

    #[test]
    fn workspace_mtime_tracks_in_place_contents_rewrite() {
        let ws = sandbox().join("myapp.xcworkspace");
        fs::create_dir_all(&ws).unwrap();
        let dir_only = workspace_mtime(&ws).unwrap();
        // Tuist-style regeneration rewrites contents.xcworkspacedata in
        // place; a newer contents mtime must bump the cache key even when
        // the directory mtime is unchanged.
        let contents = ws.join("contents.xcworkspacedata");
        fs::write(&contents, "<Workspace/>").unwrap();
        let future = std::time::SystemTime::now() + std::time::Duration::from_secs(120);
        fs::File::options()
            .write(true)
            .open(&contents)
            .unwrap()
            .set_modified(future)
            .unwrap();
        assert!(workspace_mtime(&ws).unwrap() > dir_only);
        // A missing container is an error (drives the "generate the
        // project first" hint in list_schemes).
        assert!(workspace_mtime(&sandbox().join("missing.xcworkspace")).is_err());
    }
}
