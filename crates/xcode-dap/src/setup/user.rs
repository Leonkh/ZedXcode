//! User-level setup: marker blocks in `~/.config/zed/keymap.json` /
//! `settings.json` (cmd-r -> debugger::Rerun, cmd-b / cmd-shift-k tasks,
//! cmd-shift-o -> project_symbols::Toggle). See `docs/design/dap-proxy.md` §6.1.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::setup::jsonc::{self, MergeOutcome};

pub const KEYMAP_MARKER_ID: &str = "keymap";
pub const SETTINGS_MARKER_ID: &str = "settings";

/// Keymap entries merged into `~/.config/zed/keymap.json`.
///
/// - `cmd-r` -> `debugger::Rerun` (first-ever press opens the New Session
///   modal; every later press replays the picked scenario).
/// - `cmd-b` / `cmd-shift-k` -> spawn the "Xcode: Build" / "Xcode: Clean"
///   tasks (written by `setup --project`) in the terminal dock.
/// - `cmd-shift-o` -> `project_symbols::Toggle`.
/// - The `Editor && mode == full` block shadows the default editor bindings
///   for `cmd-shift-k` / `cmd-shift-o` so the shortcuts also work with
///   editor focus. `cmd-k` is deliberately untouched (chords must survive).
pub const KEYMAP_BLOCK: &str = r#"  {
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

/// Settings merged into `~/.config/zed/settings.json`: auto-install the
/// Swift extension (sourcekit-lsp; needed for cmd+click go-to-definition).
pub const SETTINGS_BLOCK: &str = r#"  "auto_install_extensions": {
    "swift": true
  },"#;

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

/// Apply (or re-apply, idempotently) the user-level Zed config blocks
/// into `dir` (normally `~/.config/zed`).
pub fn setup_user_in(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;

    let keymap = dir.join("keymap.json");
    ensure_file(&keymap, "[]\n")?;
    // Pre-validate: merging into an already-broken file would otherwise fail
    // post-merge validation with a message blaming the merge itself.
    jsonc::parse_jsonc(&fs::read_to_string(&keymap)?)
        .context("keymap.json does not parse as JSONC — fix it, then re-run setup")?;
    let outcome = jsonc::merge_marker_block(&keymap, KEYMAP_MARKER_ID, KEYMAP_BLOCK)?;
    report(&keymap, outcome);

    let settings = dir.join("settings.json");
    ensure_file(&settings, "{}\n")?;
    let text = fs::read_to_string(&settings)?;
    // Pre-validate unconditionally (mirrors the keymap path above): when our
    // markers are already present the auto_install branch is skipped, so
    // without this a file broken *outside* the markers would only surface at
    // post-merge validation, with an error blaming the merge itself.
    let parsed = jsonc::parse_jsonc(&text)
        .context("settings.json does not parse as JSONC — fix it, then re-run setup")?;
    let has_our_markers = text.contains(&jsonc::start_marker(SETTINGS_MARKER_ID));
    if !has_our_markers && parsed.get("auto_install_extensions").is_some() {
        println!(
            "! {}: `auto_install_extensions` already exists — not editing it.",
            settings.display()
        );
        println!("  Please add manually inside it:  \"swift\": true");
    } else {
        let outcome = jsonc::merge_marker_block(&settings, SETTINGS_MARKER_ID, SETTINGS_BLOCK)?;
        report(&settings, outcome);
    }
    Ok(())
}

/// Remove the marker blocks installed by [`setup_user_in`] (`--remove`).
pub fn remove_user_in(dir: &Path) -> Result<()> {
    for (file, marker_id) in [
        ("keymap.json", KEYMAP_MARKER_ID),
        ("settings.json", SETTINGS_MARKER_ID),
    ] {
        let path = dir.join(file);
        if !path.exists() {
            println!("– {}: not present, nothing to remove", path.display());
            continue;
        }
        if jsonc::remove_marker_block(&path, marker_id)? {
            println!("✓ {}: zedxcode block removed", path.display());
        } else {
            println!("– {}: no zedxcode block found", path.display());
        }
    }
    Ok(())
}

fn ensure_file(path: &Path, skeleton: &str) -> Result<()> {
    if !path.exists() {
        fs::write(path, skeleton).with_context(|| format!("cannot create {}", path.display()))?;
        println!("– created {} (was missing)", path.display());
    }
    Ok(())
}

fn report(path: &Path, outcome: MergeOutcome) {
    match outcome {
        MergeOutcome::Inserted => println!(
            "✓ {}: zedxcode block installed (backup written alongside)",
            path.display()
        ),
        MergeOutcome::Replaced => println!(
            "✓ {}: zedxcode block updated (backup written alongside)",
            path.display()
        ),
        MergeOutcome::Unchanged => println!("✓ {}: already up to date", path.display()),
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Replica of the user's real `~/.config/zed/keymap.json`.
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

    /// Byte-expected keymap.json after `setup --user` on the fixture.
    const KEYMAP_EXPECTED: &str = r#"// Zed keymap
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

    /// Replica of the user's real `~/.config/zed/settings.json`.
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

    /// Byte-expected settings.json after `setup --user` on the fixture.
    const SETTINGS_EXPECTED: &str = r#"// Zed settings
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
  // >>> zedxcode:settings >>>
  "auto_install_extensions": {
    "swift": true
  },
  // <<< zedxcode:settings <<<
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

    #[test]
    fn setup_user_produces_byte_expected_files() {
        let dir = sandbox();
        write_fixtures(&dir);
        setup_user_in(&dir).unwrap();
        let keymap = fs::read_to_string(dir.join("keymap.json")).unwrap();
        let settings = fs::read_to_string(dir.join("settings.json")).unwrap();
        assert_eq!(keymap, KEYMAP_EXPECTED, "keymap.json bytes differ");
        assert_eq!(settings, SETTINGS_EXPECTED, "settings.json bytes differ");
        // both still parse as JSONC
        jsonc::parse_jsonc(&keymap).unwrap();
        jsonc::parse_jsonc(&settings).unwrap();
    }

    #[test]
    fn setup_user_is_idempotent() {
        let dir = sandbox();
        write_fixtures(&dir);
        setup_user_in(&dir).unwrap();
        let keymap1 = fs::read_to_string(dir.join("keymap.json")).unwrap();
        let settings1 = fs::read_to_string(dir.join("settings.json")).unwrap();
        setup_user_in(&dir).unwrap();
        let keymap2 = fs::read_to_string(dir.join("keymap.json")).unwrap();
        let settings2 = fs::read_to_string(dir.join("settings.json")).unwrap();
        assert_eq!(keymap1, keymap2, "double-run keymap not byte-identical");
        assert_eq!(
            settings1, settings2,
            "double-run settings not byte-identical"
        );
        // exactly one backup per file (second run was Unchanged)
        let backups: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| {
                let n = e.unwrap().file_name().to_string_lossy().into_owned();
                n.contains(".zedxcode-backup-").then_some(n)
            })
            .collect();
        assert_eq!(backups.len(), 2, "expected 1 backup per file: {backups:?}");
    }

    #[test]
    fn remove_restores_original_bytes() {
        let dir = sandbox();
        write_fixtures(&dir);
        setup_user_in(&dir).unwrap();
        remove_user_in(&dir).unwrap();
        let keymap = fs::read_to_string(dir.join("keymap.json")).unwrap();
        let settings = fs::read_to_string(dir.join("settings.json")).unwrap();
        assert_eq!(keymap, KEYMAP_FIXTURE);
        assert_eq!(settings, SETTINGS_FIXTURE);
    }

    #[test]
    fn setup_user_creates_missing_files() {
        let dir = sandbox();
        setup_user_in(&dir).unwrap();
        let keymap = fs::read_to_string(dir.join("keymap.json")).unwrap();
        let settings = fs::read_to_string(dir.join("settings.json")).unwrap();
        let v = jsonc::parse_jsonc(&keymap).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 2);
        let v = jsonc::parse_jsonc(&settings).unwrap();
        assert_eq!(v["auto_install_extensions"]["swift"], true);
    }

    #[test]
    fn pre_broken_keymap_fails_before_merging() {
        let dir = sandbox();
        let broken = "[\n  { \"context\": \"Workspace\"\n]\n"; // missing `}`
        fs::write(dir.join("keymap.json"), broken).unwrap();
        fs::write(dir.join("settings.json"), SETTINGS_FIXTURE).unwrap();
        let err = setup_user_in(&dir).unwrap_err().to_string();
        assert!(
            err.contains("keymap.json does not parse as JSONC"),
            "unexpected error: {err}"
        );
        // The broken file is left untouched (no half-applied merge).
        assert_eq!(fs::read_to_string(dir.join("keymap.json")).unwrap(), broken);
    }

    #[test]
    fn pre_broken_settings_fails_before_merging() {
        let dir = sandbox();
        fs::write(dir.join("keymap.json"), KEYMAP_FIXTURE).unwrap();
        // Our markers are present (so the old code skipped validation), but
        // the file is broken *outside* the marker block (no closing `}`).
        let broken = "{\n  // >>> zedxcode:settings >>>\n  \"auto_install_extensions\": {\n    \"swift\": true\n  },\n  // <<< zedxcode:settings <<<\n";
        fs::write(dir.join("settings.json"), broken).unwrap();
        let err = setup_user_in(&dir).unwrap_err().to_string();
        assert!(
            err.contains("settings.json does not parse as JSONC"),
            "unexpected error: {err}"
        );
        // The broken file is left untouched (no half-applied merge).
        assert_eq!(
            fs::read_to_string(dir.join("settings.json")).unwrap(),
            broken
        );
    }

    #[test]
    fn existing_auto_install_extensions_is_left_alone() {
        let dir = sandbox();
        let pre = "{\n  \"auto_install_extensions\": {\n    \"html\": true\n  },\n}\n";
        fs::write(dir.join("keymap.json"), KEYMAP_FIXTURE).unwrap();
        fs::write(dir.join("settings.json"), pre).unwrap();
        setup_user_in(&dir).unwrap();
        let settings = fs::read_to_string(dir.join("settings.json")).unwrap();
        assert_eq!(settings, pre, "settings.json must not be edited");
        assert!(!settings.contains("zedxcode"));
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
