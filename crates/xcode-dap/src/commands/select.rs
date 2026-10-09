//! `xcode-dap select-scheme` / `select-device` — Xcode-like scheme and
//! destination pickers for a Zed task terminal. Both write the project's
//! selection store (`.zed/.zedx/selection.json`, see `engine/selection.rs`),
//! which the engine re-reads on every build/run/clean and DAP launch, so a
//! new selection applies to the next cmd-r / cmd-b without touching
//! `.zed/debug.json` or `.zed/tasks.json`.
//!
//! Picker UX: print a numbered list (current selection marked), then read
//! stdin line by line — text filters the list and reprints it, a number
//! selects, `q` quits. Works on a tty and piped (`printf "pro\n2\n" | ...`).
//! Non-interactive paths: `--set <name>` and `--list`.

use std::io::{BufRead, Write};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use crate::commands::build::{cli_root, CliSink};
use crate::engine::destinations::{self, Query};
use crate::engine::schemes;
use crate::engine::selection::{self, StoredDestination};
use crate::setup::build_server::{regenerate, Regen};
use crate::setup::project::{build_server_opted_in, git_exclude_build_server};
use crate::util::paths::expand_worktree_root;

// ---------------------------------------------------------------------------
// select-scheme
// ---------------------------------------------------------------------------

#[derive(clap::Args, Debug)]
pub struct SelectSchemeArgs {
    /// Path to .xcworkspace / .xcodeproj (default: auto-detect within two levels of the project root)
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
    let project = cli_root()?;
    let workspace = selection::cli_container(&project, args.workspace.as_deref())?;
    let schemes = schemes::list(&workspace, &CliSink).await?.schemes;

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

    let path = selection::update(&project, |store| store.choose_scheme(&chosen))?;
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
    // .git/info/exclude step never ran there). The file lives in the project
    // root, where setup and refresh write it and sourcekit-lsp reads it, also
    // when the container sits in a subfolder (`ios/MyApp.xcworkspace`).
    let build_server = project.join("buildServer.json");
    if build_server_opted_in(&project, &build_server) {
        // Expand $ZED_WORKTREE_ROOT and anchor relative values to `project`
        // (not the cwd — select-scheme can run from a subdirectory), the same
        // way `refresh` handles this field. Passing it raw would make
        // resolve_build_root record a bogus build_root in buildServer.json.
        let derived_data = selection::first_xcode_scenario(&project)
            .ok()
            .flatten()
            .and_then(|s| s.derived_data)
            .map(|d| expand_worktree_root(&d.to_string_lossy(), &project));
        let dd = derived_data.as_deref();
        if let Regen::Written(outcome) = regenerate(&project, &workspace, &chosen, None, dd).await {
            if outcome.first_create() {
                git_exclude_build_server(&project);
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
    let project = cli_root()?;
    let inventory = destinations::inventory().await?;
    let devices = inventory.devices();
    if devices.is_empty() {
        bail!(
            "no available iPhone/iPad simulators found — install a simulator \
             runtime in Xcode (Settings ▸ Components) and try again"
        );
    }

    if args.list {
        for d in devices {
            println!(
                "{} — iOS {} — {}{}",
                d.name,
                d.os_version(),
                d.udid,
                if d.booted { " (booted)" } else { "" }
            );
        }
        return Ok(());
    }

    let chosen = if let Some(query) = args.set {
        let query = Query::legacy(Some(&query), None).expect("a device was given");
        match destinations::resolve(Some(&query), &inventory, "--set") {
            Ok(resolved) => resolved.device,
            Err(_) => bail!(
                "no simulator matching \"{}\" — run \
                 `xcode-dap select-device --list` to see what is available \
                 (names, UDIDs, or \"booted\" work)",
                query.label()
            ),
        }
    } else {
        // The simulator a build would use now, marked as current.
        let picks = selection::current(&project);
        let current = selection::settle_destination(picks.destination.as_ref(), &inventory)
            .ok()
            .map(|(device, _)| device.value.udid);
        let items: Vec<Item> = devices
            .iter()
            .map(|d| Item {
                label: format!("{} — iOS {}", d.name, d.os_version()),
                marked: d.booted,
                current: current.as_deref() == Some(d.udid.as_str()),
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

    // UDID, name and OS: the UDID first, and the name and OS when the
    // simulator is deleted and made again.
    let path = selection::update(&project, |store| {
        store.choose_destination(StoredDestination::simulator(&chosen))
    })?;
    println!("✓ Destination: {}", chosen.label());
    println!(
        "  Saved to {} — applies to the next build/run (cmd-b / cmd-r).",
        path.display()
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// current-selection helpers
// ---------------------------------------------------------------------------

/// The scheme a build would use now, for the "(current)" marker: the store,
/// the main checkout's store (a linked worktree), else the first Xcode
/// scenario's. Also the scheme `doctor` compares buildServer.json against.
pub(crate) fn current_scheme(project: &std::path::Path) -> Option<String> {
    selection::current(project).scheme.map(|s| s.value)
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
}
