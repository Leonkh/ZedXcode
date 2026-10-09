//! User-level setup: the marker block in `~/.config/zed/keymap.json`
//! (cmd-r -> debugger::Rerun, cmd-b / cmd-shift-k tasks, cmd-shift-o ->
//! project_symbols::Toggle), and retiring the `settings.json` block 0.1
//! wrote. See `docs/design/dap-proxy.md` §6.1.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::setup::jsonc::{self, BlockChange, BlockSpec, BlockState, MergeOutcome, OnEdited};

pub const KEYMAP_MARKER_ID: &str = "keymap";
pub const SETTINGS_MARKER_ID: &str = "settings";

/// The keymap block 0.1 wrote, byte for byte. A `zedxcode:keymap` region
/// under 0.1's unversioned marker that still holds exactly this is adopted
/// as ours (rewritten in place with the hashed marker).
pub const LEGACY_KEYMAP_BLOCK: &str = r#"  {
    "context": "Workspace",
    "bindings": {
      "cmd-r": "debugger::Rerun",
      "cmd-b": ["task::Spawn", { "task_name": "Xcode: Build", "reveal_target": "dock" }],
      "cmd-shift-k": ["task::Spawn", { "task_name": "Xcode: Clean", "reveal_target": "dock" }],
      "cmd-shift-o": "project_symbols::Toggle"
    }
  },
  {
    "context": "Editor && mode == full",
    "bindings": {
      "cmd-shift-k": ["task::Spawn", { "task_name": "Xcode: Clean", "reveal_target": "dock" }],
      "cmd-shift-o": "project_symbols::Toggle"
    }
  },"#;

/// Keymap entries merged into `~/.config/zed/keymap.json` (the bindings are
/// unchanged since 0.1).
///
/// - `cmd-r` -> `debugger::Rerun` (first-ever press opens the New Session
///   modal; every later press replays the picked scenario).
/// - `cmd-b` / `cmd-shift-k` -> spawn the "Xcode: Build" / "Xcode: Clean"
///   tasks (written by `setup --project`) in the terminal dock.
/// - `cmd-shift-o` -> `project_symbols::Toggle`.
/// - The `Editor && mode == full` block shadows the default editor bindings
///   for `cmd-shift-k` / `cmd-shift-o` so the shortcuts also work with
///   editor focus. `cmd-k` is deliberately untouched (chords must survive).
pub const KEYMAP_BLOCK: &str = LEGACY_KEYMAP_BLOCK;

/// The settings block 0.1 wrote into `~/.config/zed/settings.json`
/// (auto-install the Swift extension). Setup no longer writes it — Zed
/// suggests the Swift extension itself — and retires any copy it finds.
pub const LEGACY_SETTINGS_BLOCK: &str = r#"  "auto_install_extensions": {
    "swift": true
  },"#;

const KEYMAP: BlockSpec<'static> = BlockSpec {
    id: KEYMAP_MARKER_ID,
    block: KEYMAP_BLOCK,
    legacy: Some(LEGACY_KEYMAP_BLOCK),
};

/// Retired: only ever removed.
const SETTINGS: BlockSpec<'static> = BlockSpec {
    id: SETTINGS_MARKER_ID,
    block: "",
    legacy: Some(LEGACY_SETTINGS_BLOCK),
};

/// Answers a y/N question: the terminal prompt, `--yes`, or a test stub.
pub type Ask<'a> = &'a mut dyn FnMut(&str) -> Result<bool>;

/// How `setup --user` treats the files.
#[derive(Debug, Clone, Copy, Default)]
pub struct UserOptions {
    /// What to do with a block edited by hand (`--relocate`, `--replace`).
    pub on_edited: OnEdited,
    /// Print each block's before/after and the foreign entries; write
    /// nothing (`--dry-run`).
    pub dry_run: bool,
}

/// What a run does to a block; it picks the wording of the output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    /// Install or update the keymap block.
    Merge,
    /// `--remove`: delete the keymap block.
    Remove,
    /// Delete the retired settings block (every run).
    Retire,
}

/// Zed user config dir: `~/.config/zed` (macOS only).
/// `ZEDXCODE_ZED_CONFIG_DIR` overrides it (dev/test sandboxing).
pub fn zed_config_dir() -> Result<PathBuf> {
    if !cfg!(target_os = "macos") {
        bail!("xcode-dap setup --user is macOS-only");
    }
    if let Ok(dir) = std::env::var("ZEDXCODE_ZED_CONFIG_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config").join("zed"))
}

/// Apply (or re-apply, idempotently) the user-level Zed config in `dir`
/// (normally `~/.config/zed`): the keymap block, and retiring 0.1's settings
/// block. `ask` answers the y/N for moving entries out of an edited settings
/// block.
pub fn setup_user_in(dir: &Path, opts: UserOptions, ask: Ask) -> Result<()> {
    let keymap = dir.join("keymap.json");
    let text = match read_config(&keymap)? {
        Some(text) => {
            // Merging into an already-broken file would otherwise fail
            // post-merge validation with a message blaming the merge itself.
            validate(&keymap, &text)?;
            text
        }
        None if opts.dry_run => {
            println!("– {}: missing; setup creates it", keymap.display());
            "[]\n".to_string()
        }
        None => {
            fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
            fs::write(&keymap, "[]\n")
                .with_context(|| format!("cannot create {}", keymap.display()))?;
            println!("– created {} (was missing)", keymap.display());
            "[]\n".to_string()
        }
    };
    let change = jsonc::plan_merge(&text, &KEYMAP, opts.on_edited)?;
    apply(&keymap, &text, &change, Action::Merge, opts.dry_run, None)?;

    retire_settings(dir, opts, ask)?;
    if opts.dry_run {
        println!("Dry run: nothing was written.");
    }
    Ok(())
}

/// Remove the keymap block installed by [`setup_user_in`] when it is ours
/// (`--remove`), and retire 0.1's settings block.
pub fn remove_user_in(dir: &Path, opts: UserOptions, ask: Ask) -> Result<()> {
    let keymap = dir.join("keymap.json");
    match read_config(&keymap)? {
        None => println!("– {}: not present, nothing to remove", keymap.display()),
        Some(text) => {
            let change = jsonc::plan_remove(&text, &KEYMAP, opts.on_edited)?;
            // A file without our block is not ours to judge.
            if change.outcome != MergeOutcome::NotPresent {
                validate(&keymap, &text)?;
            }
            apply(&keymap, &text, &change, Action::Remove, opts.dry_run, None)?;
        }
    }
    retire_settings(dir, opts, ask)?;
    if opts.dry_run {
        println!("Dry run: nothing was written.");
    }
    Ok(())
}

/// Setup writes no settings block any more. 0.1's is removed when it is
/// ours; when it was edited by hand, the entries ZedXcode did not write are
/// moved out of it verbatim — after a y/N, or straight away with
/// `--relocate` — and our `auto_install_extensions` member goes with the
/// markers. `--replace` never applies here: those entries are the user's
/// settings.
fn retire_settings(dir: &Path, opts: UserOptions, ask: Ask) -> Result<()> {
    let path = dir.join("settings.json");
    let Some(text) = read_config(&path)? else {
        return Ok(());
    };
    let relocate = if opts.on_edited == OnEdited::Relocate {
        OnEdited::Relocate
    } else {
        OnEdited::Keep
    };
    let mut change = jsonc::plan_remove(&text, &SETTINGS, relocate)?;
    if change.outcome == MergeOutcome::NotPresent {
        if opts.dry_run {
            println!("{}: no zedxcode block, nothing to retire\n", path.display());
        }
        return Ok(());
    }
    validate(&path, &text)?;
    let mut note = None;
    if change.outcome == MergeOutcome::Kept {
        let them = if change.foreign().len() == 1 {
            "it"
        } else {
            "them"
        };
        if opts.dry_run {
            note = Some(format!(
                "setup asks y/N before moving {them}; --relocate skips the question"
            ));
            change = jsonc::plan_remove(&text, &SETTINGS, OnEdited::Relocate)?;
        } else if ask(&format!(
            "{}: setup no longer writes the zedxcode settings block, and it holds {}. \
             Move {them} out of the block, verbatim, and delete the block?",
            file_name(&path),
            holds(&change),
        ))? {
            change = jsonc::plan_remove(&text, &SETTINGS, OnEdited::Relocate)?;
        }
    }
    apply(
        &path,
        &text,
        &change,
        Action::Retire,
        opts.dry_run,
        note.as_deref(),
    )
}

/// The file's text, or `None` when it does not exist.
fn read_config(path: &Path) -> Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
    }
}

fn validate(path: &Path, text: &str) -> Result<()> {
    jsonc::parse_jsonc(text).with_context(|| {
        format!(
            "{} does not parse as JSONC — fix it, then re-run setup",
            file_name(path)
        )
    })?;
    Ok(())
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// Write the change and report it, or (dry run) show it.
fn apply(
    path: &Path,
    text: &str,
    change: &BlockChange,
    action: Action,
    dry_run: bool,
    note: Option<&str>,
) -> Result<()> {
    if dry_run {
        print_dry_run(path, change, action, note);
        return Ok(());
    }
    let backup = jsonc::write_change(path, text, change)?;
    report(path, change, action, backup.as_deref());
    Ok(())
}

/// "2 entries ZedXcode did not write (a; b)".
fn holds(change: &BlockChange) -> String {
    let foreign = change.foreign();
    let labels: Vec<&str> = foreign.iter().map(|f| f.label.as_str()).collect();
    match foreign.len() {
        0 => "no entries ZedXcode did not write".to_string(),
        1 => format!("1 entry ZedXcode did not write ({})", labels[0]),
        n => format!("{n} entries ZedXcode did not write ({})", labels.join("; ")),
    }
}

/// The message for a block edited by hand that was left unchanged.
fn edited_message(path: &Path, change: &BlockChange, action: Action) -> String {
    let file = file_name(path);
    let holds = holds(change);
    let edited = format!(
        "{file}: the zedxcode block was edited by hand and holds {holds}. Left unchanged. \
         To update it, run \"xcode-dap setup --user --relocate\" (keeps your entries, \
         verbatim) or \"--replace\" (overwrites; a backup is kept)."
    );
    match action {
        Action::Merge => edited,
        Action::Remove => format!("{edited} To remove it instead, add --remove to either."),
        Action::Retire => format!(
            "{file}: the retired zedxcode block holds {holds}. Left unchanged. To retire it, \
             run \"xcode-dap setup --user --relocate\" (keeps your entries, verbatim)."
        ),
    }
}

/// One line per applied change.
fn report(path: &Path, change: &BlockChange, action: Action, backup: Option<&Path>) {
    let p = path.display();
    let backup = backup
        .map(|b| format!(" (backup: {})", b.display()))
        .unwrap_or_default();
    let moved = change.foreign().len();
    let entries = if moved == 1 { "entry" } else { "entries" };
    match (action, change.outcome) {
        (_, MergeOutcome::Inserted) => println!("✓ {p}: zedxcode block installed{backup}"),
        (_, MergeOutcome::Updated) => println!("✓ {p}: zedxcode block updated{backup}"),
        (_, MergeOutcome::Unchanged) => println!("✓ {p}: already up to date"),
        (Action::Merge, MergeOutcome::Relocated) => println!(
            "✓ {p}: moved {moved} {entries} ZedXcode did not write below the zedxcode block, \
             verbatim, and updated the block{backup}"
        ),
        (_, MergeOutcome::Relocated) => println!(
            "✓ {p}: zedxcode block removed; the {moved} {entries} ZedXcode did not write stay \
             where it was, verbatim{backup}"
        ),
        (Action::Merge, MergeOutcome::Replaced) => {
            println!("✓ {p}: zedxcode block overwritten{backup}")
        }
        (_, MergeOutcome::Replaced) => println!(
            "✓ {p}: zedxcode block removed with the {moved} {entries} ZedXcode did not \
             write{backup}"
        ),
        (Action::Retire, MergeOutcome::Removed) => println!(
            "✓ {p}: retired zedxcode settings block removed (Zed suggests the Swift extension \
             itself){backup}"
        ),
        (_, MergeOutcome::Removed) => println!("✓ {p}: zedxcode block removed{backup}"),
        (_, MergeOutcome::NotPresent) => println!("– {p}: no zedxcode block found"),
        (_, MergeOutcome::Kept) => println!("{}", edited_message(path, change, action)),
    }
}

/// `--dry-run`: the block's before/after and the foreign entries.
fn print_dry_run(path: &Path, change: &BlockChange, action: Action, note: Option<&str>) {
    if change.outcome == MergeOutcome::NotPresent {
        println!("{}: no zedxcode block, nothing to remove\n", path.display());
        return;
    }
    println!("{}: {}", path.display(), summary(change, action));
    println!("--- before");
    println!("{}", change.before.as_deref().unwrap_or("(none)"));
    println!("+++ after");
    match change.after.as_deref() {
        None => println!("(unchanged)"),
        Some("") => println!("(removed)"),
        Some(after) => println!("{}", after.trim_end_matches('\n')),
    }
    let foreign = change.foreign();
    if !foreign.is_empty() {
        println!("entries ZedXcode did not write ({}):", foreign.len());
        for entry in foreign {
            println!("{}", entry.text.trim_end_matches('\n'));
        }
    }
    if let Some(note) = note {
        println!("({note})");
    }
    if change.outcome == MergeOutcome::Kept {
        println!("{}", edited_message(path, change, action));
    }
    println!();
}

fn summary(change: &BlockChange, action: Action) -> &'static str {
    match (action, change.outcome, &change.state) {
        (_, MergeOutcome::Inserted, _) => {
            "new zedxcode block, placed right after the opening bracket"
        }
        (_, MergeOutcome::Updated, BlockState::Owned { legacy: true }) => {
            "the 0.1 zedxcode block, adopted and updated in place"
        }
        (_, MergeOutcome::Updated, _) => "zedxcode block, updated in place",
        (_, MergeOutcome::Unchanged, _) => "zedxcode block, already up to date",
        (Action::Merge, MergeOutcome::Relocated, _) => {
            "edited by hand: the entries ZedXcode did not write move below the block, \
             verbatim, then the block is updated"
        }
        (_, MergeOutcome::Relocated, _) => {
            "edited by hand: the entries ZedXcode did not write stay where the block was, \
             verbatim, and the block is deleted"
        }
        (Action::Merge, MergeOutcome::Replaced, _) => {
            "edited by hand: overwritten (a backup is kept)"
        }
        (_, MergeOutcome::Replaced, _) => {
            "edited by hand: deleted with the entries ZedXcode did not write (a backup is kept)"
        }
        (Action::Retire, MergeOutcome::Removed, _) => "retired zedxcode block, removed",
        (_, MergeOutcome::Removed, _) => "zedxcode block, removed",
        (_, MergeOutcome::Kept, _) => "edited by hand: left unchanged",
        (_, MergeOutcome::NotPresent, _) => "no zedxcode block",
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Replica of a typical `~/.config/zed/keymap.json`.
    const KEYMAP_FIXTURE: &str = r#"// Zed keymap
//
// For information on binding keys, see the Zed
// documentation: https://zed.dev/docs/key-bindings
//
// To see the default key bindings run `zed: open default keymap`
// from the command palette.
[
  {
    "context": "Workspace",
    "bindings": {
      // "shift shift": "file_finder::Toggle"
    },
  },
  {
    "context": "Editor && vim_mode == insert",
    "bindings": {
      // "j k": "vim::NormalBefore"
    },
  },
]
"#;

    /// Byte-expected keymap.json after `setup --user` on the fixture: the
    /// hashed block opens the array.
    const KEYMAP_EXPECTED: &str = r#"// Zed keymap
//
// For information on binding keys, see the Zed
// documentation: https://zed.dev/docs/key-bindings
//
// To see the default key bindings run `zed: open default keymap`
// from the command palette.
[
  // >>> zedxcode:keymap v2 h=0f254d61a03ab39c >>>
  {
    "context": "Workspace",
    "bindings": {
      "cmd-r": "debugger::Rerun",
      "cmd-b": ["task::Spawn", { "task_name": "Xcode: Build", "reveal_target": "dock" }],
      "cmd-shift-k": ["task::Spawn", { "task_name": "Xcode: Clean", "reveal_target": "dock" }],
      "cmd-shift-o": "project_symbols::Toggle"
    }
  },
  {
    "context": "Editor && mode == full",
    "bindings": {
      "cmd-shift-k": ["task::Spawn", { "task_name": "Xcode: Clean", "reveal_target": "dock" }],
      "cmd-shift-o": "project_symbols::Toggle"
    }
  },
  // <<< zedxcode:keymap <<<
  {
    "context": "Workspace",
    "bindings": {
      // "shift shift": "file_finder::Toggle"
    },
  },
  {
    "context": "Editor && vim_mode == insert",
    "bindings": {
      // "j k": "vim::NormalBefore"
    },
  },
]
"#;

    /// keymap.json as 0.1's `setup --user` left the fixture: the
    /// unversioned block before the final `]`.
    const KEYMAP_0_1: &str = r#"// Zed keymap
//
// For information on binding keys, see the Zed
// documentation: https://zed.dev/docs/key-bindings
//
// To see the default key bindings run `zed: open default keymap`
// from the command palette.
[
  {
    "context": "Workspace",
    "bindings": {
      // "shift shift": "file_finder::Toggle"
    },
  },
  {
    "context": "Editor && vim_mode == insert",
    "bindings": {
      // "j k": "vim::NormalBefore"
    },
  },
  // >>> zedxcode:keymap >>>
  {
    "context": "Workspace",
    "bindings": {
      "cmd-r": "debugger::Rerun",
      "cmd-b": ["task::Spawn", { "task_name": "Xcode: Build", "reveal_target": "dock" }],
      "cmd-shift-k": ["task::Spawn", { "task_name": "Xcode: Clean", "reveal_target": "dock" }],
      "cmd-shift-o": "project_symbols::Toggle"
    }
  },
  {
    "context": "Editor && mode == full",
    "bindings": {
      "cmd-shift-k": ["task::Spawn", { "task_name": "Xcode: Clean", "reveal_target": "dock" }],
      "cmd-shift-o": "project_symbols::Toggle"
    }
  },
  // <<< zedxcode:keymap <<<
]
"#;

    /// A binding of the user's (as Zed's keymap editor writes one).
    const USER_BINDING: &str = r#"  {
    "context": "Editor",
    "bindings": {
      "alt-j": "editor::JoinLines"
    }
  }"#;

    /// Replica of a typical `~/.config/zed/settings.json`.
    const SETTINGS_FIXTURE: &str = r#"// Zed settings
//
// For information on how to configure Zed, see the Zed
// documentation: https://zed.dev/docs/configuring-zed
//
// To see all of Zed's default settings without changing your
// custom settings, run `zed: open default settings` from the
// command palette (cmd-shift-p / ctrl-shift-p)
{
  "autosave": {
    "after_delay": {
      "milliseconds": 0
    }
  },
  "format_on_save": "off",
  "terminal": {
    "shell": "system",
  },
  "agent_servers": {

  },
  "session": {
    "trust_all_worktrees": true,
  },
  "icon_theme": "Zed (Default)",
  "ui_font_size": 16,
  "buffer_font_size": 15,
  "theme": {
    "mode": "dark",
    "light": "One Light",
    "dark": "Xcode High Contrast Dark",
  },
}
"#;

    /// The block 0.1's `setup --user` added to the fixture.
    const SETTINGS_0_1_REGION: &str = r#"  // >>> zedxcode:settings >>>
  "auto_install_extensions": {
    "swift": true
  },
  // <<< zedxcode:settings <<<
"#;

    /// 0.1's settings block after Zed's own settings writers appended to the
    /// end of the object, inside our markers (the last member has no comma).
    const SETTINGS_HAZARD_REGION: &str = r#"  // >>> zedxcode:settings >>>
  "auto_install_extensions": {
    "swift": true
  },
  "dap": {
    "Xcode": {
      "binary": "/Users/x/bin/xcode-dap"
    }
  },
  "context_servers": {
    "example": {}
  },
  "agent": {
    "tool_permissions": {}
  }
  // <<< zedxcode:settings <<<
"#;

    /// What retiring the hazard block leaves: the foreign members byte for
    /// byte, no `auto_install_extensions`, no markers.
    const SETTINGS_HAZARD_RETIRED_TAIL: &str = r#"  "dap": {
    "Xcode": {
      "binary": "/Users/x/bin/xcode-dap"
    }
  },
  "context_servers": {
    "example": {}
  },
  "agent": {
    "tool_permissions": {}
  }
"#;

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn sandbox() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zedxcode-user-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_fixtures(dir: &Path) {
        fs::write(dir.join("keymap.json"), KEYMAP_FIXTURE).unwrap();
        fs::write(dir.join("settings.json"), SETTINGS_FIXTURE).unwrap();
    }

    /// The settings fixture with `region` before its final `}`.
    fn settings_with(region: &str) -> String {
        let closer = SETTINGS_FIXTURE.rfind('}').unwrap();
        format!(
            "{}{region}{}",
            &SETTINGS_FIXTURE[..closer],
            &SETTINGS_FIXTURE[closer..]
        )
    }

    /// `text` with `extra` on its own line just before our end marker.
    fn appended_inside(text: &str, extra: &str) -> String {
        let end = text.find("  // <<< zedxcode:keymap <<<").unwrap();
        format!("{}{extra}\n{}", &text[..end], &text[end..])
    }

    fn read(dir: &Path, name: &str) -> String {
        fs::read_to_string(dir.join(name)).unwrap()
    }

    fn backups(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| {
                let n = e.unwrap().file_name().to_string_lossy().into_owned();
                n.contains(".zedxcode-backup-").then_some(n)
            })
            .collect()
    }

    fn no_question(prompt: &str) -> Result<bool> {
        panic!("setup asked: {prompt}")
    }

    fn setup(dir: &Path, on_edited: OnEdited) {
        let opts = UserOptions {
            on_edited,
            dry_run: false,
        };
        setup_user_in(dir, opts, &mut no_question).unwrap();
    }

    fn remove(dir: &Path, on_edited: OnEdited) {
        let opts = UserOptions {
            on_edited,
            dry_run: false,
        };
        remove_user_in(dir, opts, &mut no_question).unwrap();
    }

    #[test]
    fn setup_user_produces_byte_expected_files() {
        let dir = sandbox();
        write_fixtures(&dir);
        setup(&dir, OnEdited::Keep);
        let keymap = read(&dir, "keymap.json");
        assert_eq!(keymap, KEYMAP_EXPECTED, "keymap.json bytes differ");
        jsonc::parse_jsonc(&keymap).unwrap();
        // settings.json gets nothing any more.
        assert_eq!(read(&dir, "settings.json"), SETTINGS_FIXTURE);
        assert_eq!(backups(&dir).len(), 1, "only keymap.json is backed up");
    }

    #[test]
    fn setup_user_is_idempotent() {
        let dir = sandbox();
        write_fixtures(&dir);
        setup(&dir, OnEdited::Keep);
        let keymap1 = read(&dir, "keymap.json");
        setup(&dir, OnEdited::Keep);
        assert_eq!(
            keymap1,
            read(&dir, "keymap.json"),
            "double-run not byte-identical"
        );
        // exactly one backup (second run was Unchanged)
        assert_eq!(backups(&dir).len(), 1, "{:?}", backups(&dir));
    }

    #[test]
    fn remove_restores_original_bytes() {
        let dir = sandbox();
        write_fixtures(&dir);
        setup(&dir, OnEdited::Keep);
        remove(&dir, OnEdited::Keep);
        assert_eq!(read(&dir, "keymap.json"), KEYMAP_FIXTURE);
        assert_eq!(read(&dir, "settings.json"), SETTINGS_FIXTURE);
    }

    #[test]
    fn setup_user_creates_only_a_missing_keymap() {
        let dir = sandbox().join("zed");
        setup(&dir, OnEdited::Keep);
        let v = jsonc::parse_jsonc(&read(&dir, "keymap.json")).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 2);
        assert!(!dir.join("settings.json").exists());
    }

    #[test]
    fn pre_broken_keymap_fails_before_merging() {
        let dir = sandbox();
        let broken = "[\n  { \"context\": \"Workspace\"\n]\n"; // missing `}`
        fs::write(dir.join("keymap.json"), broken).unwrap();
        fs::write(dir.join("settings.json"), SETTINGS_FIXTURE).unwrap();
        let opts = UserOptions::default();
        let err = setup_user_in(&dir, opts, &mut no_question)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("keymap.json does not parse as JSONC"),
            "unexpected error: {err}"
        );
        // The broken file is left untouched (no half-applied merge).
        assert_eq!(read(&dir, "keymap.json"), broken);
    }

    #[test]
    fn pre_broken_settings_with_our_block_fails_before_retiring() {
        let dir = sandbox();
        fs::write(dir.join("keymap.json"), KEYMAP_FIXTURE).unwrap();
        // Our markers are present, but the file is broken *outside* the
        // marker block (no closing `}`).
        let broken = format!("{{\n{SETTINGS_0_1_REGION}");
        fs::write(dir.join("settings.json"), &broken).unwrap();
        let opts = UserOptions::default();
        let err = setup_user_in(&dir, opts, &mut no_question)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("settings.json does not parse as JSONC"),
            "unexpected error: {err}"
        );
        assert_eq!(read(&dir, "settings.json"), broken);
    }

    #[test]
    fn settings_without_our_block_are_never_touched() {
        let dir = sandbox();
        fs::write(dir.join("keymap.json"), KEYMAP_FIXTURE).unwrap();
        let pre = "{\n  \"auto_install_extensions\": {\n    \"html\": true\n  },\n}\n";
        fs::write(dir.join("settings.json"), pre).unwrap();
        setup(&dir, OnEdited::Keep);
        assert_eq!(read(&dir, "settings.json"), pre);
        // Even a broken settings.json is none of setup's business without
        // our block.
        let broken = "{\n  \"theme\": \n";
        fs::write(dir.join("settings.json"), broken).unwrap();
        setup(&dir, OnEdited::Keep);
        remove(&dir, OnEdited::Keep);
        assert_eq!(read(&dir, "settings.json"), broken);
    }

    #[test]
    fn remove_ignores_a_broken_keymap_without_our_block() {
        let dir = sandbox();
        let broken = "[\n  { \"context\": \"Workspace\"\n]\n";
        fs::write(dir.join("keymap.json"), broken).unwrap();
        remove(&dir, OnEdited::Keep);
        assert_eq!(read(&dir, "keymap.json"), broken);
    }

    #[test]
    fn retired_settings_block_is_removed_when_ours() {
        let dir = sandbox();
        fs::write(dir.join("keymap.json"), KEYMAP_FIXTURE).unwrap();
        fs::write(
            dir.join("settings.json"),
            settings_with(SETTINGS_0_1_REGION),
        )
        .unwrap();
        setup(&dir, OnEdited::Keep);
        assert_eq!(read(&dir, "settings.json"), SETTINGS_FIXTURE);
    }

    #[test]
    fn edited_settings_block_keeps_foreign_members_verbatim() {
        let hazard = settings_with(SETTINGS_HAZARD_REGION);
        jsonc::parse_jsonc(&hazard).unwrap();
        let retired = settings_with(SETTINGS_HAZARD_RETIRED_TAIL);
        let v = jsonc::parse_jsonc(&retired).unwrap();
        assert_eq!(v["dap"]["Xcode"]["binary"], "/Users/x/bin/xcode-dap");
        assert!(v.get("auto_install_extensions").is_none());

        // y/N answered yes: moved, ours dropped, markers deleted.
        let dir = sandbox();
        fs::write(dir.join("keymap.json"), KEYMAP_FIXTURE).unwrap();
        fs::write(dir.join("settings.json"), &hazard).unwrap();
        let mut asked = Vec::new();
        let mut ask = |prompt: &str| -> Result<bool> {
            asked.push(prompt.to_string());
            Ok(true)
        };
        setup_user_in(&dir, UserOptions::default(), &mut ask).unwrap();
        assert_eq!(read(&dir, "settings.json"), retired);
        assert_eq!(asked.len(), 1);
        assert!(
            asked[0].contains(
                "holds 3 entries ZedXcode did not write (\"dap\"; \"context_servers\"; \"agent\")"
            ),
            "{}",
            asked[0]
        );

        // y/N answered no: untouched.
        let dir = sandbox();
        fs::write(dir.join("keymap.json"), KEYMAP_FIXTURE).unwrap();
        fs::write(dir.join("settings.json"), &hazard).unwrap();
        setup_user_in(&dir, UserOptions::default(), &mut |_: &str| Ok(false)).unwrap();
        assert_eq!(read(&dir, "settings.json"), hazard);

        // --relocate: no question; also on --remove.
        for removing in [false, true] {
            let dir = sandbox();
            fs::write(dir.join("keymap.json"), KEYMAP_FIXTURE).unwrap();
            fs::write(dir.join("settings.json"), &hazard).unwrap();
            if removing {
                remove(&dir, OnEdited::Relocate);
            } else {
                setup(&dir, OnEdited::Relocate);
            }
            assert_eq!(read(&dir, "settings.json"), retired);
        }

        // --replace never drops the user's settings: it still asks.
        let dir = sandbox();
        fs::write(dir.join("keymap.json"), KEYMAP_FIXTURE).unwrap();
        fs::write(dir.join("settings.json"), &hazard).unwrap();
        let opts = UserOptions {
            on_edited: OnEdited::Replace,
            dry_run: false,
        };
        setup_user_in(&dir, opts, &mut |_: &str| Ok(false)).unwrap();
        assert_eq!(read(&dir, "settings.json"), hazard);
    }

    #[test]
    fn settings_saved_while_the_question_waits_are_not_overwritten() {
        let hazard = settings_with(SETTINGS_HAZARD_REGION);
        let dir = sandbox();
        fs::write(dir.join("keymap.json"), KEYMAP_FIXTURE).unwrap();
        fs::write(dir.join("settings.json"), &hazard).unwrap();
        // Zed writes settings.json while setup waits for the answer.
        let saved = hazard.replace("\"ui_font_size\": 16", "\"ui_font_size\": 17");
        let path = dir.join("settings.json");
        let mut ask = |_: &str| -> Result<bool> {
            fs::write(&path, &saved).unwrap();
            Ok(true)
        };
        let err = setup_user_in(&dir, UserOptions::default(), &mut ask)
            .unwrap_err()
            .to_string();
        assert!(err.contains("changed on disk"), "{err}");
        assert_eq!(read(&dir, "settings.json"), saved);
        assert!(backups(&dir)
            .iter()
            .all(|b| b.starts_with("keymap.json.zedxcode-backup-")));
    }

    #[test]
    fn settings_question_names_a_single_entry_as_it() {
        let one = SETTINGS_0_1_REGION.replace(
            "  },\n  // <<<",
            "  },\n  \"context_servers\": {\n    \"example\": {}\n  },\n  // <<<",
        );
        let dir = sandbox();
        fs::write(dir.join("keymap.json"), KEYMAP_FIXTURE).unwrap();
        fs::write(dir.join("settings.json"), settings_with(&one)).unwrap();
        let mut asked = String::new();
        let mut ask = |prompt: &str| -> Result<bool> {
            asked = prompt.to_string();
            Ok(false)
        };
        setup_user_in(&dir, UserOptions::default(), &mut ask).unwrap();
        assert!(
            asked.contains(
                "holds 1 entry ZedXcode did not write (\"context_servers\"). Move it out of the block"
            ),
            "{asked}"
        );
    }

    #[test]
    fn legacy_keymap_block_is_adopted_in_place() {
        let dir = sandbox();
        fs::write(dir.join("keymap.json"), KEYMAP_0_1).unwrap();
        setup(&dir, OnEdited::Keep);
        assert_eq!(
            read(&dir, "keymap.json"),
            KEYMAP_0_1.replace(
                "// >>> zedxcode:keymap >>>",
                "// >>> zedxcode:keymap v2 h=0f254d61a03ab39c >>>"
            )
        );
        setup(&dir, OnEdited::Keep);
        assert_eq!(backups(&dir).len(), 1, "the second run changes nothing");
    }

    #[test]
    fn user_binding_after_our_keymap_block_survives() {
        let closer = KEYMAP_0_1.rfind(']').unwrap();
        let with_user = format!(
            "{}{USER_BINDING},\n{}",
            &KEYMAP_0_1[..closer],
            &KEYMAP_0_1[closer..]
        );
        jsonc::parse_jsonc(&with_user).unwrap();
        let dir = sandbox();
        fs::write(dir.join("keymap.json"), &with_user).unwrap();
        setup(&dir, OnEdited::Keep);
        let text = read(&dir, "keymap.json");
        assert!(text.contains("v2 h=0f254d61a03ab39c"), "{text}");
        assert!(
            text.ends_with(&format!(
                "  // <<< zedxcode:keymap <<<\n{USER_BINDING},\n]\n"
            )),
            "{text}"
        );
        remove(&dir, OnEdited::Keep);
        let text = read(&dir, "keymap.json");
        assert!(!text.contains("zedxcode") && !text.contains("cmd-shift-o"));
        assert!(
            text.ends_with(&format!("  }},\n{USER_BINDING},\n]\n")),
            "{text}"
        );
        jsonc::parse_jsonc(&text).unwrap();
    }

    #[test]
    fn binding_appended_inside_our_keymap_block_is_kept_until_relocated() {
        let edited = appended_inside(KEYMAP_0_1, USER_BINDING);
        jsonc::parse_jsonc(&edited).unwrap();
        let dir = sandbox();
        fs::write(dir.join("keymap.json"), &edited).unwrap();
        // Default and --remove: left unchanged, nothing written.
        setup(&dir, OnEdited::Keep);
        remove(&dir, OnEdited::Keep);
        assert_eq!(read(&dir, "keymap.json"), edited);
        assert!(backups(&dir).is_empty());
        // --relocate: ours is rewritten in place; the binding moves below
        // our end marker byte for byte, so it still comes after ours.
        setup(&dir, OnEdited::Relocate);
        let expected = KEYMAP_0_1
            .replace(
                "// >>> zedxcode:keymap >>>",
                "// >>> zedxcode:keymap v2 h=0f254d61a03ab39c >>>",
            )
            .replace(
                "  // <<< zedxcode:keymap <<<\n",
                &format!("  // <<< zedxcode:keymap <<<\n{USER_BINDING}\n"),
            );
        assert_eq!(read(&dir, "keymap.json"), expected);
        // Idempotent, and the block is ours again: --remove takes only it.
        setup(&dir, OnEdited::Relocate);
        assert_eq!(read(&dir, "keymap.json"), expected);
        remove(&dir, OnEdited::Keep);
        let text = read(&dir, "keymap.json");
        assert!(text.contains("editor::JoinLines") && !text.contains("zedxcode"));
        assert_eq!(
            jsonc::parse_jsonc(&text).unwrap().as_array().unwrap().len(),
            3
        );
    }

    #[test]
    fn replace_overwrites_an_edited_keymap_block_and_keeps_a_backup() {
        let edited = appended_inside(KEYMAP_0_1, USER_BINDING);
        let dir = sandbox();
        fs::write(dir.join("keymap.json"), &edited).unwrap();
        setup(&dir, OnEdited::Replace);
        assert_eq!(
            read(&dir, "keymap.json"),
            KEYMAP_0_1.replace(
                "// >>> zedxcode:keymap >>>",
                "// >>> zedxcode:keymap v2 h=0f254d61a03ab39c >>>"
            )
        );
        let backups = backups(&dir);
        assert_eq!(backups.len(), 1);
        assert_eq!(fs::read_to_string(dir.join(&backups[0])).unwrap(), edited);
    }

    #[test]
    fn dry_run_writes_nothing() {
        let cases = [
            (
                Some(KEYMAP_FIXTURE.to_string()),
                Some(SETTINGS_FIXTURE.to_string()),
            ),
            (
                Some(appended_inside(KEYMAP_0_1, USER_BINDING)),
                Some(settings_with(SETTINGS_HAZARD_REGION)),
            ),
            (
                Some(KEYMAP_0_1.to_string()),
                Some(settings_with(SETTINGS_0_1_REGION)),
            ),
            (None, None),
        ];
        for (keymap, settings) in cases {
            for on_edited in [OnEdited::Keep, OnEdited::Relocate, OnEdited::Replace] {
                let dir = sandbox();
                if let Some(k) = &keymap {
                    fs::write(dir.join("keymap.json"), k).unwrap();
                }
                if let Some(s) = &settings {
                    fs::write(dir.join("settings.json"), s).unwrap();
                }
                let opts = UserOptions {
                    on_edited,
                    dry_run: true,
                };
                setup_user_in(&dir, opts, &mut no_question).unwrap();
                remove_user_in(&dir, opts, &mut no_question).unwrap();
                assert_eq!(dir.join("keymap.json").exists(), keymap.is_some());
                if let Some(k) = &keymap {
                    assert_eq!(&read(&dir, "keymap.json"), k);
                }
                if let Some(s) = &settings {
                    assert_eq!(&read(&dir, "settings.json"), s);
                }
                assert_eq!(
                    fs::read_dir(&dir).unwrap().count(),
                    keymap.iter().count() + settings.iter().count()
                );
            }
        }
    }

    #[test]
    fn edited_message_matches_the_documented_wording() {
        let edited = format!(
            "{},\n  {{\"context\": \"Workspace\", \"bindings\": {{\"cmd-e\": \"editor::Foo\"}}}}",
            USER_BINDING
        );
        let text = appended_inside(KEYMAP_0_1, &edited);
        let change = jsonc::plan_merge(&text, &KEYMAP, OnEdited::Keep).unwrap();
        let path = Path::new("/Users/x/.config/zed/keymap.json");
        let documented = "keymap.json: the zedxcode block was edited by hand and holds 2 \
             entries ZedXcode did not write (Editor: alt-j; Workspace: cmd-e). Left unchanged. \
             To update it, run \"xcode-dap setup --user --relocate\" (keeps your entries, \
             verbatim) or \"--replace\" (overwrites; a backup is kept).";
        assert_eq!(edited_message(path, &change, Action::Merge), documented);
        // --remove: the same message, plus how to remove instead.
        let change = jsonc::plan_remove(&text, &KEYMAP, OnEdited::Keep).unwrap();
        assert_eq!(
            edited_message(path, &change, Action::Remove),
            format!("{documented} To remove it instead, add --remove to either.")
        );
    }

    #[test]
    fn keymap_block_is_valid_jsonc_fragment() {
        let wrapped = format!("[\n{KEYMAP_BLOCK}\n]\n");
        let v = jsonc::parse_jsonc(&wrapped).unwrap();
        let entries = v.as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["context"], "Workspace");
        assert_eq!(entries[0]["bindings"]["cmd-r"], "debugger::Rerun");
        assert_eq!(
            entries[0]["bindings"]["cmd-b"][1]["task_name"],
            "Xcode: Build"
        );
        assert_eq!(
            entries[0]["bindings"]["cmd-shift-k"][1]["reveal_target"],
            "dock"
        );
        assert_eq!(entries[1]["context"], "Editor && mode == full");
        assert_eq!(
            entries[1]["bindings"]["cmd-shift-o"],
            "project_symbols::Toggle"
        );
        // cmd-k must never be rebound (chords + terminal::Clear stay default)
        assert!(!KEYMAP_BLOCK.contains("\"cmd-k\""));
    }
}
