//! `xcode-dap select-scheme` / `select-device` (also `select-destination`) /
//! `select-configuration` — Xcode-like pickers for a Zed task terminal. Each
//! writes one value of the project's selection store (version 2,
//! `.zed/.zedx/selection.json`, see `engine/selection.rs`), which the engine
//! re-reads on every build/run/clean and DAP launch, so a new selection
//! applies to the next cmd-r / cmd-b without touching `.zed/debug.json` or
//! `.zed/tasks.json`.
//!
//! Picker UX: print a numbered list (current selection marked), then read
//! stdin line by line — text filters the list and reprints it, a number
//! selects, `q` quits. Works on a tty and piped (`printf "pro\n2\n" | ...`).
//! Non-interactive paths: `--set <name>`, `--list`, and `--reset`, which
//! forgets the project's choice so the next build takes the value from the
//! next place that sets it (the main checkout's choice in a git worktree,
//! the Xcode scenario, else the automatic value).

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::commands::build::{cli_root, CliSink};
use crate::engine::destinations::{self, Query};
use crate::engine::schemes;
use crate::engine::selection::{self, Field, StoredDestination};
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
    /// Forget the chosen scheme: the next build takes the main checkout's
    /// choice (in a git worktree), the Xcode scenario's "scheme", else the
    /// container's only scheme
    #[arg(long, conflicts_with_all = ["set", "list"])]
    pub reset: bool,
}

pub async fn run_select_scheme(args: SelectSchemeArgs) -> Result<()> {
    let project = cli_root()?;
    if args.reset {
        print_lines(&reset_lines(&project, Field::Scheme)?);
        // buildServer.json follows the scheme the next build uses. With none
        // chosen anywhere, that is the container's only scheme, which the
        // reset one then was too (or nothing builds until one is chosen).
        if let Some(scheme) = selection::current(&project).scheme {
            if let Ok(workspace) = selection::cli_container(&project, args.workspace.as_deref()) {
                sync_build_server(&project, &workspace, &scheme.value).await;
            }
        }
        return Ok(());
    }
    let workspace = selection::cli_container(&project, args.workspace.as_deref())?;
    let schemes = schemes::list(&workspace, &CliSink).await?.schemes;

    if args.list {
        print_lines(&schemes);
        return Ok(());
    }

    let chosen = if let Some(query) = args.set {
        choose_by_name(&schemes, &query, &SCHEMES, &workspace)?
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
    print_lines(&saved_lines("Scheme", &chosen, &path));
    sync_build_server(&project, &workspace, &chosen).await;
    Ok(())
}

/// Point buildServer.json at `scheme`: it records the scheme, and a stale
/// one keeps answering sourcekit-lsp with another scheme's products.
async fn sync_build_server(project: &Path, workspace: &Path, scheme: &str) {
    // Same opt-in gate as the build pipeline's auto-regen: never
    // first-create the file in a repo that never configured the Xcode
    // adapter, and git-ignore it when a first-create does happen (setup's
    // .git/info/exclude step never ran there). The file lives in the project
    // root, where setup and refresh write it and sourcekit-lsp reads it, also
    // when the container sits in a subfolder (`ios/MyApp.xcworkspace`).
    let build_server = project.join("buildServer.json");
    if !build_server_opted_in(project, &build_server) {
        return;
    }
    // Expand $ZED_WORKTREE_ROOT and anchor relative values to `project`
    // (not the cwd — select-scheme can run from a subdirectory), the same
    // way `refresh` handles this field. Passing it raw would make
    // resolve_build_root record a bogus build_root in buildServer.json.
    let derived_data = selection::first_xcode_scenario(project)
        .ok()
        .flatten()
        .and_then(|s| s.derived_data)
        .map(|d| expand_worktree_root(&d.to_string_lossy(), project));
    let dd = derived_data.as_deref();
    if let Regen::Written(outcome) = regenerate(project, workspace, scheme, None, dd).await {
        if outcome.first_create() {
            git_exclude_build_server(project);
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

// ---------------------------------------------------------------------------
// select-device (select-destination)
// ---------------------------------------------------------------------------

#[derive(clap::Args, Debug)]
pub struct SelectDeviceArgs {
    /// Set non-interactively: device name, UDID, or "booted"
    #[arg(long)]
    pub set: Option<String>,
    /// Print the device list (one per line) and exit
    #[arg(long)]
    pub list: bool,
    /// Forget the chosen destination: the next build takes the main
    /// checkout's choice (in a git worktree), the Xcode scenario's "device"
    /// and "os", else the booted iPhone or the newest one
    #[arg(long, conflicts_with_all = ["set", "list"])]
    pub reset: bool,
}

pub async fn run_select_device(args: SelectDeviceArgs) -> Result<()> {
    let project = cli_root()?;
    if args.reset {
        print_lines(&reset_lines(&project, Field::Destination)?);
        return Ok(());
    }
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
    print_lines(&saved_lines("Destination", &chosen.label(), &path));
    Ok(())
}

// ---------------------------------------------------------------------------
// select-configuration
// ---------------------------------------------------------------------------

#[derive(clap::Args, Debug)]
pub struct SelectConfigurationArgs {
    /// Path to .xcworkspace / .xcodeproj (default: auto-detect within two levels of the project root)
    #[arg(long, short = 'w')]
    pub workspace: Option<PathBuf>,
    /// Set the build configuration non-interactively (exact name, case-insensitive)
    #[arg(long)]
    pub set: Option<String>,
    /// Print the build configurations (one per line) and exit
    #[arg(long)]
    pub list: bool,
    /// Forget the chosen configuration: the next build takes the main
    /// checkout's choice (in a git worktree), the Xcode scenario's
    /// "configuration", else the scheme's own
    #[arg(long, conflicts_with_all = ["set", "list"])]
    pub reset: bool,
}

pub async fn run_select_configuration(args: SelectConfigurationArgs) -> Result<()> {
    let project = cli_root()?;
    if args.reset {
        print_lines(&reset_lines(&project, Field::Configuration)?);
        return Ok(());
    }
    let container = selection::cli_container(&project, args.workspace.as_deref())?;
    let scheme = selection::current(&project).scheme.map(|s| s.value);
    // `--set` can do without the list: when it cannot be read, the name is
    // saved unchecked (the build takes it as it is), with the reason.
    let (configurations, unread) =
        match schemes::configurations(&container, scheme.as_deref(), &CliSink).await {
            Ok(names) => (names, None),
            Err(e) if args.set.is_some() => (Vec::new(), Some(format!("{e:#}"))),
            Err(e) => return Err(e),
        };
    let lines = configuration_lines(
        &project,
        &container,
        &configurations,
        unread.as_deref(),
        &args,
    )?;
    if let Some(lines) = lines {
        print_lines(&lines);
        return Ok(());
    }

    if configurations.is_empty() {
        bail!(
            "no build configurations are listed for {}; set one by name with \
             `xcode-dap select-configuration --set <name>`",
            container.display()
        );
    }
    let current = selection::current(&project).configuration.map(|c| c.value);
    let items: Vec<Item> = configurations
        .iter()
        .map(|c| Item {
            label: c.clone(),
            marked: false,
            current: current.as_deref() == Some(c.as_str()),
        })
        .collect();
    println!(
        "{} configurations in {}:",
        items.len(),
        container.file_name().unwrap_or_default().to_string_lossy()
    );
    let Some(i) = pick_interactive(&items, "Configuration")? else {
        println!("No configuration selected — selection unchanged.");
        return Ok(());
    };
    let chosen = &configurations[i];
    let path = selection::update(&project, |store| {
        store.configuration = Some(chosen.clone());
    })?;
    print_lines(&saved_lines("Configuration", chosen, &path));
    Ok(())
}

/// `--list` and `--set` once the container's configurations are known: the
/// lines to print (`--set` also writes the store); `None` when neither flag
/// is given and the picker runs. With none to check against — no project of
/// a workspace lists any, or the list could not be read (`unread` says why)
/// — `--set` takes the name as it is and says so.
fn configuration_lines(
    project: &Path,
    container: &Path,
    configurations: &[String],
    unread: Option<&str>,
    args: &SelectConfigurationArgs,
) -> Result<Option<Vec<String>>> {
    if args.list {
        return Ok(Some(configurations.to_vec()));
    }
    let Some(query) = &args.set else {
        return Ok(None);
    };
    let (chosen, checked) = if configurations.is_empty() {
        (query.clone(), false)
    } else {
        (
            choose_by_name(configurations, query, &CONFIGURATIONS, container)?,
            true,
        )
    };
    let path = selection::update(project, |store| {
        store.configuration = Some(chosen.clone());
    })?;
    let mut lines = saved_lines("Configuration", &chosen, &path);
    if !checked {
        lines.push(match unread {
            Some(reason) => format!(
                "  Not checked: the build configurations of {} could not be read: {reason}",
                container.display()
            ),
            None => format!(
                "  Not checked: no build configurations are listed for {}.",
                container.display()
            ),
        });
    }
    Ok(Some(lines))
}

// ---------------------------------------------------------------------------
// shared by the pickers
// ---------------------------------------------------------------------------

/// What a picker chooses among, for its messages.
struct Kind {
    one: &'static str,
    many: &'static str,
    command: &'static str,
}

const SCHEMES: Kind = Kind {
    one: "scheme",
    many: "schemes",
    command: "select-scheme",
};

const CONFIGURATIONS: Kind = Kind {
    one: "configuration",
    many: "configurations",
    command: "select-configuration",
};

/// `--set <query>`: the name in `names` that `query` means (the exact one,
/// else the first equal to it ignoring case), or an error that lists the
/// names containing it.
fn choose_by_name(names: &[String], query: &str, kind: &Kind, container: &Path) -> Result<String> {
    if let Some(name) = names
        .iter()
        .find(|n| *n == query)
        .or_else(|| names.iter().find(|n| n.eq_ignore_ascii_case(query)))
    {
        return Ok(name.clone());
    }
    let near: Vec<&str> = names
        .iter()
        .filter(|n| n.to_lowercase().contains(&query.to_lowercase()))
        .take(10)
        .map(String::as_str)
        .collect();
    let hint = if near.is_empty() {
        format!(
            "run `xcode-dap {} --list` to see all {} {}",
            kind.command,
            names.len(),
            kind.many
        )
    } else {
        format!("did you mean:\n  {}", near.join("\n  "))
    };
    bail!(
        "no {} named \"{query}\" in {} — {hint}",
        kind.one,
        container.display()
    )
}

/// What a picker prints after saving a choice.
fn saved_lines(what: &str, value: &str, path: &Path) -> Vec<String> {
    vec![
        format!("✓ {what}: {value}"),
        format!(
            "  Saved to {} — applies to the next build/run (cmd-b / cmd-r).",
            path.display()
        ),
    ]
}

/// `--reset`: forget the project's choice of `field` (the other values and
/// the recent lists stay) and say what the next build uses instead. A store
/// without that choice is not written.
fn reset_lines(project: &Path, field: Field) -> Result<Vec<String>> {
    let (what, automatic) = match field {
        Field::Scheme => (
            "scheme",
            "the container's only scheme (with several, it asks you to choose one)",
        ),
        Field::Destination => (
            "destination",
            "the booted iPhone, else the newest iPhone on the newest iOS",
        ),
        Field::Configuration => ("configuration", "the scheme's own configuration"),
    };
    let store = selection::load(project).store;
    let chosen = match field {
        Field::Scheme => store.scheme.is_some(),
        Field::Destination => store.destination.is_some(),
        Field::Configuration => store.configuration.is_some(),
    };
    let mut lines = Vec::new();
    if chosen {
        let path = selection::update(project, |store| match field {
            Field::Scheme => store.scheme = None,
            Field::Destination => store.destination = None,
            Field::Configuration => store.configuration = None,
        })?;
        lines.push(format!(
            "✓ The {what} choice is cleared in {}.",
            path.display()
        ));
    } else {
        lines.push(format!(
            "No {what} is chosen in {}; nothing to clear.",
            selection::store_path(project).display()
        ));
    }
    let picks = selection::current(project);
    let next = match field {
        Field::Scheme => picks.scheme.map(|s| (s.value, s.source)),
        Field::Destination => picks.destination.map(|d| (d.value.label(), d.source)),
        Field::Configuration => picks.configuration.map(|c| (c.value, c.source)),
    };
    lines.push(match next {
        Some((value, source)) => format!(
            "  The next build/run uses {value} (from {}).",
            source.label(field)
        ),
        None => format!("  The next build/run uses {automatic}."),
    });
    Ok(lines)
}

fn print_lines(lines: &[String]) {
    for line in lines {
        println!("{line}");
    }
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
    use crate::engine::selection::{Source, StoredDestination};
    use serde_json::{json, Map, Value};
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A project folder with a container, canonical like the CLI's roots.
    fn project() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zedxcode-select-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(dir.join("MyApp.xcodeproj")).unwrap();
        dir.canonicalize().unwrap()
    }

    fn store_json(project: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(selection::store_path(project)).unwrap()).unwrap()
    }

    /// What `xcodebuild -list -json` prints for a project with three
    /// configurations.
    fn configurations() -> Vec<String> {
        schemes::parse(
            br#"{"project": {"name": "MyApp", "schemes": ["MyApp", "MyApp Dev"],
                 "configurations": ["Debug", "Release", "Beta"], "targets": ["MyApp"]}}"#,
        )
        .unwrap()
        .configurations
    }

    fn configuration_args(set: Option<&str>, list: bool) -> SelectConfigurationArgs {
        SelectConfigurationArgs {
            workspace: None,
            set: set.map(str::to_owned),
            list,
            reset: false,
        }
    }

    #[test]
    fn select_configuration_lists_and_sets_from_the_scheme_list() {
        let project = project();
        let container = project.join("MyApp.xcodeproj");
        let list = configurations();
        assert_eq!(
            configuration_lines(
                &project,
                &container,
                &list,
                None,
                &configuration_args(None, true)
            )
            .unwrap(),
            Some(vec!["Debug".to_owned(), "Release".into(), "Beta".into()])
        );
        assert!(
            !selection::store_path(&project).exists(),
            "--list writes nothing"
        );
        // Neither flag: the picker runs.
        assert_eq!(
            configuration_lines(
                &project,
                &container,
                &list,
                None,
                &configuration_args(None, false)
            )
            .unwrap(),
            None
        );

        // --set takes the name as the project spells it.
        let lines = configuration_lines(
            &project,
            &container,
            &list,
            None,
            &configuration_args(Some("release"), false),
        )
        .unwrap()
        .unwrap();
        let path = selection::store_path(&project);
        assert_eq!(
            lines,
            [
                "✓ Configuration: Release".to_owned(),
                format!(
                    "  Saved to {} — applies to the next build/run (cmd-b / cmd-r).",
                    path.display()
                )
            ]
        );
        assert_eq!(
            store_json(&project),
            json!({ "version": 2, "configuration": "Release" })
        );
        assert_eq!(
            selection::current(&project).configuration.unwrap().source,
            Source::Store
        );

        // A name the project does not have is refused, with near names.
        let err = configuration_lines(
            &project,
            &container,
            &list,
            None,
            &configuration_args(Some("Staging"), false),
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "no configuration named \"Staging\" in {} — run `xcode-dap \
                 select-configuration --list` to see all 3 configurations",
                container.display()
            )
        );
        let err = configuration_lines(
            &project,
            &container,
            &list,
            None,
            &configuration_args(Some("de"), false),
        )
        .unwrap_err();
        assert!(err.to_string().ends_with("did you mean:\n  Debug"), "{err}");
        assert_eq!(store_json(&project)["configuration"], "Release");

        // A workspace whose projects list none: the name is taken as it is.
        let workspace = project.join("MyApp.xcworkspace");
        let lines = configuration_lines(
            &project,
            &workspace,
            &[],
            None,
            &configuration_args(Some("Staging"), false),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            lines.last().unwrap(),
            &format!(
                "  Not checked: no build configurations are listed for {}.",
                workspace.display()
            )
        );
        assert_eq!(store_json(&project)["configuration"], "Staging");

        // A list that could not be read: the same, with the reason.
        let lines = configuration_lines(
            &project,
            &workspace,
            &[],
            Some("xcodebuild -list did not finish in 120 s"),
            &configuration_args(Some("Profile"), false),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            lines.last().unwrap(),
            &format!(
                "  Not checked: the build configurations of {} could not be read: xcodebuild \
                 -list did not finish in 120 s",
                workspace.display()
            )
        );
        assert_eq!(store_json(&project)["configuration"], "Profile");
    }

    #[test]
    fn reset_forgets_one_choice_and_names_what_comes_next() {
        let project = project();
        let path = selection::store_path(&project);
        selection::update(&project, |store| {
            store.choose_scheme("MyApp Dev");
            store.choose_destination(StoredDestination {
                kind: selection::KIND_SIMULATOR.into(),
                udid: Some("BBBBBBBB-1111-2222-3333-444444444444".into()),
                name: Some("iPad Air 11-inch (M3)".into()),
                os: Some("18.4".into()),
                extra: Map::new(),
            });
            store.configuration = Some("Release".into());
        })
        .unwrap();
        // The project's scenario still sets a scheme and a device (0.1 keys).
        fs::create_dir_all(project.join(".zed")).unwrap();
        fs::write(
            project.join(".zed/debug.json"),
            r#"[{"adapter": "Xcode", "label": "Run", "scheme": "MyApp", "device": "iPhone 16e"}]"#,
        )
        .unwrap();

        assert_eq!(
            reset_lines(&project, Field::Configuration).unwrap(),
            [
                format!(
                    "✓ The configuration choice is cleared in {}.",
                    path.display()
                ),
                "  The next build/run uses the scheme's own configuration.".to_owned(),
            ]
        );
        assert_eq!(
            reset_lines(&project, Field::Scheme).unwrap(),
            [
                format!("✓ The scheme choice is cleared in {}.", path.display()),
                "  The next build/run uses MyApp (from the Xcode scenario).".to_owned(),
            ]
        );
        // The other values and the recent lists stay.
        let v = store_json(&project);
        assert_eq!(v.get("scheme"), None);
        assert_eq!(v.get("configuration"), None);
        assert_eq!(v["destination"]["name"], "iPad Air 11-inch (M3)");
        assert_eq!(v["recent"]["schemes"], json!(["MyApp Dev"]));

        assert_eq!(
            reset_lines(&project, Field::Destination).unwrap(),
            [
                format!("✓ The destination choice is cleared in {}.", path.display()),
                "  The next build/run uses iPhone 16e (from the Xcode scenario).".to_owned(),
            ]
        );
        assert_eq!(store_json(&project).get("destination"), None);

        // Nothing left to clear: the store is not written.
        let before = fs::read(&path).unwrap();
        fs::remove_file(project.join(".zed/debug.json")).unwrap();
        assert_eq!(
            reset_lines(&project, Field::Destination).unwrap(),
            [
                format!(
                    "No destination is chosen in {}; nothing to clear.",
                    path.display()
                ),
                "  The next build/run uses the booted iPhone, else the newest iPhone on the \
                 newest iOS."
                    .to_owned(),
            ]
        );
        assert_eq!(fs::read(&path).unwrap(), before);
        let fresh = self::project();
        reset_lines(&fresh, Field::Scheme).unwrap();
        assert!(!selection::store_path(&fresh).exists());
    }

    #[test]
    fn set_picks_the_exact_name_else_one_ignoring_case() {
        let container = Path::new("/Users/x/MyApp/MyApp.xcworkspace");
        let schemes: Vec<String> = ["MyApp (staging)", "MyApp (production)", "Widget"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            choose_by_name(&schemes, "widget", &SCHEMES, container).unwrap(),
            "Widget"
        );
        assert_eq!(
            choose_by_name(&schemes, "MyApp", &SCHEMES, container)
                .unwrap_err()
                .to_string(),
            "no scheme named \"MyApp\" in /Users/x/MyApp/MyApp.xcworkspace — did you mean:\n  \
             MyApp (staging)\n  MyApp (production)"
        );
    }

    #[test]
    fn reset_conflicts_with_set_and_list() {
        use clap::Parser;
        #[derive(Parser)]
        struct Cli {
            #[command(flatten)]
            args: SelectConfigurationArgs,
        }
        assert!(Cli::try_parse_from(["select-configuration", "--reset"]).is_ok());
        assert!(
            Cli::try_parse_from(["select-configuration", "--reset", "--set", "Debug"]).is_err()
        );
        assert!(Cli::try_parse_from(["select-configuration", "--reset", "--list"]).is_err());
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
}
