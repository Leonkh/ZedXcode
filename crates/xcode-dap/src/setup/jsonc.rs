//! Marker-block surgical merge for JSONC files (keymap.json, settings.json).
//! Text surgery, never re-serialization — user comments/formatting/trailing
//! commas survive. See `docs/design/dap-proxy.md` §6.1.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};

/// Outcome of a marker-block merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeOutcome {
    /// No markers existed; block inserted before the final `]` / `}`.
    Inserted,
    /// Both markers existed; inner text replaced.
    Replaced,
    /// Existing region already equals `block`.
    Unchanged,
}

/// Opening marker line (matched with surrounding whitespace trimmed).
pub fn start_marker(marker_id: &str) -> String {
    format!("// >>> zedxcode:{marker_id} >>>")
}

/// Closing marker line (matched with surrounding whitespace trimmed).
pub fn end_marker(marker_id: &str) -> String {
    format!("// <<< zedxcode:{marker_id} <<<")
}

/// Merge `block` into the JSONC file at `path` between
/// `// >>> zedxcode:<marker_id> >>>` and `// <<< zedxcode:<marker_id> <<<`.
///
/// 1. read file; timestamped backup `<file>.zedxcode-backup-<ts>`
/// 2. markers exist -> replace inner text
/// 3. else scan for the final `]` / `}` of the top-level value
///    (string/comment aware) and insert the block (with a leading `,`
///    only when the preceding element lacks a trailing comma)
/// 4. atomic write (tmp + rename); post-merge JSONC validation;
///    restore the original on validation failure
pub fn merge_marker_block(path: &Path, marker_id: &str, block: &str) -> Result<MergeOutcome> {
    let original =
        fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let rendered = render_region(marker_id, block);
    let (new_content, outcome) = match find_marker_region(&original, marker_id)? {
        Some((start, end)) => {
            if original[start..end] == rendered {
                return Ok(MergeOutcome::Unchanged);
            }
            (
                format!("{}{}{}", &original[..start], rendered, &original[end..]),
                MergeOutcome::Replaced,
            )
        }
        None => (
            insert_before_final_closer(&original, &rendered)
                .with_context(|| format!("cannot find insertion point in {}", path.display()))?,
            MergeOutcome::Inserted,
        ),
    };
    write_validated(path, &original, &new_content)?;
    Ok(outcome)
}

/// Remove a previously merged marker block (used by `setup --remove`).
/// Returns `false` when no markers are present.
pub fn remove_marker_block(path: &Path, marker_id: &str) -> Result<bool> {
    let original =
        fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let Some((start, mut end)) = find_marker_region(&original, marker_id)? else {
        return Ok(false);
    };
    // Take the trailing newline of the end-marker line with the region.
    if original[end..].starts_with('\n') {
        end += 1;
    }
    let new_content = format!("{}{}", &original[..start], &original[end..]);
    write_validated(path, &original, &new_content)?;
    Ok(true)
}

/// Tolerant JSONC reader: strip comments + trailing commas (string-aware),
/// then parse with serde_json. Used for post-merge validation and for
/// reading user/project JSONC config files.
pub fn parse_jsonc(text: &str) -> Result<serde_json::Value> {
    let stripped = strip_jsonc(text);
    serde_json::from_str(&stripped).map_err(|e| anyhow::anyhow!("invalid JSON(C): {e}"))
}

// ---------------------------------------------------------------------------
// internals (pub(crate) pieces reused by setup/project.rs)
// ---------------------------------------------------------------------------

/// `<file><suffix>` (suffix appended to the whole file name).
fn path_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut os = path.as_os_str().to_os_string();
    os.push(suffix);
    PathBuf::from(os)
}

/// Write `original` to a fresh timestamped `<file>.zedxcode-backup-<ts>[-n]`.
pub(crate) fn backup_file(path: &Path, original: &str) -> Result<PathBuf> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut n = 0u32;
    let backup = loop {
        let suffix = if n == 0 {
            format!(".zedxcode-backup-{ts}")
        } else {
            format!(".zedxcode-backup-{ts}-{n}")
        };
        let candidate = path_with_suffix(path, &suffix);
        if !candidate.exists() {
            break candidate;
        }
        n += 1;
    };
    fs::write(&backup, original)
        .with_context(|| format!("cannot write backup {}", backup.display()))?;
    Ok(backup)
}

/// Atomic write: tmp file in the same directory + rename. The tmp name is
/// per-process (pid-scoped) so two concurrent writers of the same target
/// (e.g. a Build task racing a ⌘R launch both regenerating buildServer.json)
/// don't clobber each other's tmp and rename ENOENT.
pub(crate) fn atomic_write(path: &Path, content: &str) -> Result<()> {
    let tmp = path_with_suffix(path, &format!(".zedxcode-tmp-{}", std::process::id()));
    fs::write(&tmp, content).with_context(|| format!("cannot write {}", tmp.display()))?;
    fs::rename(&tmp, path)
        .with_context(|| format!("cannot rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Backup + atomic write + post-write JSONC validation; restores the
/// original content (and keeps the backup) when validation fails.
fn write_validated(path: &Path, original: &str, new_content: &str) -> Result<()> {
    let backup = backup_file(path, original)?;
    atomic_write(path, new_content)?;
    let reread = fs::read_to_string(path)?;
    if let Err(err) = parse_jsonc(&reread) {
        fs::write(path, original)
            .with_context(|| format!("cannot restore {} after failed merge", path.display()))?;
        bail!(
            "post-merge validation of {} failed ({err}); original restored, backup kept at {}",
            path.display(),
            backup.display()
        );
    }
    Ok(())
}

/// Marker block region rendered at the file's 2-space indent level.
fn render_region(marker_id: &str, block: &str) -> String {
    format!(
        "  {}\n{}\n  {}",
        start_marker(marker_id),
        block.trim_end_matches('\n'),
        end_marker(marker_id)
    )
}

/// Byte range `[start, end)` from the start of the start-marker line to the
/// end of the end-marker line (excluding its trailing newline).
fn find_marker_region(text: &str, marker_id: &str) -> Result<Option<(usize, usize)>> {
    let start_m = start_marker(marker_id);
    let end_m = end_marker(marker_id);
    let mut start: Option<usize> = None;
    let mut end: Option<usize> = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed == start_m && start.is_none() {
            start = Some(offset);
        } else if trimmed == end_m && end.is_none() {
            end = Some(offset + line.trim_end_matches(['\n', '\r']).len());
        }
        offset += line.len();
    }
    match (start, end) {
        (None, None) => Ok(None),
        (Some(s), Some(e)) if e > s => Ok(Some((s, e))),
        _ => bail!(
            "corrupt zedxcode:{marker_id} marker block (one marker missing or out of order); \
             fix the file manually"
        ),
    }
}

struct Scan {
    /// Byte index of the last structural `]` / `}` outside strings/comments.
    last_closer: Option<usize>,
    /// Last significant (non-whitespace, non-comment) byte before that
    /// closer: `(index, byte)`.
    sig_before_closer: Option<(usize, u8)>,
}

/// Forward string/comment-aware lexer locating the final closing bracket of
/// the top-level JSONC value and the significant character preceding it
/// (for comma handling in trailing-comma files).
fn scan_structure(text: &str) -> Scan {
    enum Mode {
        Normal,
        Str,
        Line,
        Block,
    }
    let b = text.as_bytes();
    let mut mode = Mode::Normal;
    let mut last_closer = None;
    let mut sig_before_closer = None;
    let mut last_sig: Option<(usize, u8)> = None;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match mode {
            Mode::Normal => match c {
                b'"' => {
                    mode = Mode::Str;
                    last_sig = Some((i, c));
                }
                b'/' if i + 1 < b.len() && b[i + 1] == b'/' => {
                    mode = Mode::Line;
                    i += 1;
                }
                b'/' if i + 1 < b.len() && b[i + 1] == b'*' => {
                    mode = Mode::Block;
                    i += 1;
                }
                b']' | b'}' => {
                    last_closer = Some(i);
                    sig_before_closer = last_sig;
                    last_sig = Some((i, c));
                }
                c if c.is_ascii_whitespace() => {}
                _ => {
                    last_sig = Some((i, c));
                }
            },
            Mode::Str => {
                last_sig = Some((i, c));
                match c {
                    b'\\' => {
                        i += 1; // skip the escaped byte
                    }
                    b'"' => mode = Mode::Normal,
                    _ => {}
                }
            }
            Mode::Line => {
                if c == b'\n' {
                    mode = Mode::Normal;
                }
            }
            Mode::Block => {
                if c == b'*' && i + 1 < b.len() && b[i + 1] == b'/' {
                    mode = Mode::Normal;
                    i += 1;
                }
            }
        }
        i += 1;
    }
    Scan {
        last_closer,
        sig_before_closer,
    }
}

/// Insert `rendered` (the marker region) before the final `]` / `}` of the
/// top-level value, adding a `,` after the previous element only when needed.
fn insert_before_final_closer(text: &str, rendered: &str) -> Result<String> {
    let scan = scan_structure(text);
    let Some(closer_idx) = scan.last_closer else {
        bail!("no top-level ']' or '}}' found — is this a JSON(C) file?");
    };
    let need_comma = match scan.sig_before_closer {
        None => false,
        Some((_, b',')) | Some((_, b'[')) | Some((_, b'{')) => false,
        Some(_) => true,
    };
    let line_start = text[..closer_idx].rfind('\n').map(|p| p + 1).unwrap_or(0);
    let only_ws_before = text[line_start..closer_idx].trim().is_empty();
    let mut insert_at = if only_ws_before {
        line_start
    } else {
        closer_idx
    };
    let block_text = if only_ws_before {
        format!("{rendered}\n")
    } else {
        format!("\n{rendered}\n")
    };
    let mut out = text.to_string();
    if need_comma {
        let (i, _) = scan
            .sig_before_closer
            .expect("need_comma implies a preceding significant char");
        out.insert(i + 1, ',');
        insert_at += 1;
    }
    out.insert_str(insert_at, &block_text);
    Ok(out)
}

/// Strip `//` and `/* */` comments and trailing commas, string-aware, so the
/// result parses with serde_json. Operates on bytes (structural chars are
/// ASCII; non-ASCII bytes are copied verbatim).
fn strip_jsonc(text: &str) -> String {
    let b = text.as_bytes();
    // pass 1: comments -> removed (string-aware)
    let mut no_comments: Vec<u8> = Vec::with_capacity(b.len());
    let mut in_str = false;
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if in_str {
            no_comments.push(c);
            if c == b'\\' && i + 1 < b.len() {
                no_comments.push(b[i + 1]);
                i += 2;
                continue;
            }
            if c == b'"' {
                in_str = false;
            }
            i += 1;
        } else if c == b'"' {
            in_str = true;
            no_comments.push(c);
            i += 1;
        } else if c == b'/' && i + 1 < b.len() && b[i + 1] == b'/' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if c == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(b.len());
        } else {
            no_comments.push(c);
            i += 1;
        }
    }
    // pass 2: drop commas directly preceding a closing bracket (string-aware)
    let mut out: Vec<u8> = Vec::with_capacity(no_comments.len());
    let mut in_str = false;
    let mut i = 0;
    while i < no_comments.len() {
        let c = no_comments[i];
        if in_str {
            out.push(c);
            if c == b'\\' && i + 1 < no_comments.len() {
                out.push(no_comments[i + 1]);
                i += 2;
                continue;
            }
            if c == b'"' {
                in_str = false;
            }
            i += 1;
        } else if c == b'"' {
            in_str = true;
            out.push(c);
            i += 1;
        } else if c == b',' {
            let mut j = i + 1;
            while j < no_comments.len() && no_comments[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < no_comments.len() && (no_comments[j] == b']' || no_comments[j] == b'}') {
                // trailing comma: drop it
            } else {
                out.push(c);
            }
            i += 1;
        } else {
            out.push(c);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Replica of the user's real `~/.config/zed/keymap.json` (header
    /// comments, array, two context blocks, trailing commas everywhere).
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

    /// Replica of the user's real `~/.config/zed/settings.json` (header
    /// comments, nested objects, trailing commas).
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

    const BLOCK: &str = r#"  {
    "context": "Workspace",
    "bindings": {
      "cmd-r": "debugger::Rerun"
    }
  },"#;

    const OBJ_BLOCK: &str = r#"  "auto_install_extensions": {
    "swift": true
  },"#;

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn tmpfile(name: &str, content: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zedxcode-jsonc-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, content).unwrap();
        path
    }

    fn backups_for(path: &Path) -> Vec<PathBuf> {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let mut found = vec![];
        for entry in fs::read_dir(path.parent().unwrap()).unwrap() {
            let p = entry.unwrap().path();
            let n = p.file_name().unwrap().to_string_lossy().into_owned();
            if n.starts_with(&format!("{name}.zedxcode-backup-")) {
                found.push(p);
            }
        }
        found
    }

    #[test]
    fn insert_into_real_keymap_fixture() {
        let path = tmpfile("keymap.json", KEYMAP_FIXTURE);
        let outcome = merge_marker_block(&path, "keymap", BLOCK).unwrap();
        assert_eq!(outcome, MergeOutcome::Inserted);
        let text = fs::read_to_string(&path).unwrap();
        // fixture has a trailing comma after the last element -> no comma added
        assert!(!text.contains(",,"), "double comma introduced:\n{text}");
        // markers present, block before the final ]
        let m_start = text.find("// >>> zedxcode:keymap >>>").unwrap();
        let m_end = text.find("// <<< zedxcode:keymap <<<").unwrap();
        assert!(m_start < m_end);
        assert!(m_end < text.rfind(']').unwrap());
        // still valid JSONC, now with 3 elements
        let v = parse_jsonc(&text).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 3);
        // user content untouched
        assert!(text.starts_with("// Zed keymap\n"));
        assert!(text.contains(r#"// "j k": "vim::NormalBefore""#));
        // backup created
        assert_eq!(backups_for(&path).len(), 1);
    }

    #[test]
    fn insert_into_real_settings_fixture() {
        let path = tmpfile("settings.json", SETTINGS_FIXTURE);
        let outcome = merge_marker_block(&path, "settings", OBJ_BLOCK).unwrap();
        assert_eq!(outcome, MergeOutcome::Inserted);
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains(",,"));
        let v = parse_jsonc(&text).unwrap();
        assert_eq!(v["auto_install_extensions"]["swift"], true);
        assert_eq!(v["theme"]["dark"], "Xcode High Contrast Dark");
        assert!(text.starts_with("// Zed settings\n"));
    }

    #[test]
    fn double_run_is_byte_identical_and_makes_no_new_backup() {
        let path = tmpfile("keymap.json", KEYMAP_FIXTURE);
        merge_marker_block(&path, "keymap", BLOCK).unwrap();
        let first = fs::read_to_string(&path).unwrap();
        let outcome = merge_marker_block(&path, "keymap", BLOCK).unwrap();
        assert_eq!(outcome, MergeOutcome::Unchanged);
        let second = fs::read_to_string(&path).unwrap();
        assert_eq!(first, second, "double-run not byte-identical");
        assert_eq!(
            backups_for(&path).len(),
            1,
            "Unchanged run must not back up"
        );
    }

    #[test]
    fn replace_updates_existing_block() {
        let path = tmpfile("keymap.json", KEYMAP_FIXTURE);
        merge_marker_block(&path, "keymap", BLOCK).unwrap();
        let new_block = BLOCK.replace("cmd-r", "cmd-e");
        let outcome = merge_marker_block(&path, "keymap", &new_block).unwrap();
        assert_eq!(outcome, MergeOutcome::Replaced);
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("cmd-e"));
        assert!(!text.contains("cmd-r"));
        assert_eq!(text.matches("// >>> zedxcode:keymap >>>").count(), 1);
        parse_jsonc(&text).unwrap();
    }

    #[test]
    fn remove_restores_trailing_comma_fixture_byte_identical() {
        let path = tmpfile("keymap.json", KEYMAP_FIXTURE);
        merge_marker_block(&path, "keymap", BLOCK).unwrap();
        let removed = remove_marker_block(&path, "keymap").unwrap();
        assert!(removed);
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(
            text, KEYMAP_FIXTURE,
            "remove must restore the original bytes"
        );
        parse_jsonc(&text).unwrap();
        // second removal is a no-op
        assert!(!remove_marker_block(&path, "keymap").unwrap());
    }

    #[test]
    fn remove_keeps_file_parseable_when_comma_was_added() {
        let path = tmpfile("keymap.json", "[\n  {\"a\": 1}\n]\n");
        merge_marker_block(&path, "keymap", BLOCK).unwrap();
        let with_block = fs::read_to_string(&path).unwrap();
        assert!(
            with_block.contains("{\"a\": 1},"),
            "comma must be added:\n{with_block}"
        );
        parse_jsonc(&with_block).unwrap();
        assert!(remove_marker_block(&path, "keymap").unwrap());
        let text = fs::read_to_string(&path).unwrap();
        // leftover trailing comma is valid JSONC
        parse_jsonc(&text).unwrap();
        assert!(!text.contains("zedxcode"));
    }

    #[test]
    fn comma_added_only_when_needed() {
        // no trailing comma -> comma added
        let path = tmpfile("a.json", "[\n  {\"a\": 1}\n]\n");
        merge_marker_block(&path, "x", BLOCK).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("{\"a\": 1},"));
        parse_jsonc(&text).unwrap();

        // trailing comma -> no extra comma
        let path = tmpfile("b.json", "[\n  {\"a\": 1},\n]\n");
        merge_marker_block(&path, "x", BLOCK).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains(",,"));
        parse_jsonc(&text).unwrap();
    }

    #[test]
    fn insert_into_empty_array_and_object() {
        let path = tmpfile("empty.json", "[]\n");
        merge_marker_block(&path, "x", BLOCK).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        let v = parse_jsonc(&text).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 1);

        let path = tmpfile("empty-obj.json", "{}\n");
        merge_marker_block(&path, "x", OBJ_BLOCK).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        let v = parse_jsonc(&text).unwrap();
        assert_eq!(v["auto_install_extensions"]["swift"], true);
    }

    #[test]
    fn closer_inside_string_or_comment_is_ignored() {
        let src = "[\n  {\"note\": \"contains ] and } in string\"}\n]\n// trailing comment with ] bracket\n";
        let path = tmpfile("tricky.json", src);
        merge_marker_block(&path, "x", BLOCK).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        let v = parse_jsonc(&text).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 2);
        // block must land before the real closer, not inside the comment
        assert!(
            text.find("// <<< zedxcode:x <<<").unwrap() < text.find("// trailing comment").unwrap()
        );
    }

    #[test]
    fn validation_failure_restores_original_and_keeps_backup() {
        let path = tmpfile("keymap.json", KEYMAP_FIXTURE);
        let err = merge_marker_block(&path, "keymap", "  {{{ not json").unwrap_err();
        assert!(
            err.to_string().contains("validation"),
            "unexpected error: {err}"
        );
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text, KEYMAP_FIXTURE, "original must be restored");
        assert_eq!(backups_for(&path).len(), 1, "backup must be kept");
    }

    #[test]
    fn corrupt_single_marker_errors() {
        let src = "[\n  // >>> zedxcode:x >>>\n  {\"a\": 1}\n]\n";
        let path = tmpfile("corrupt.json", src);
        let err = merge_marker_block(&path, "x", BLOCK).unwrap_err();
        assert!(
            err.to_string().contains("corrupt"),
            "unexpected error: {err}"
        );
        // file untouched
        assert_eq!(fs::read_to_string(&path).unwrap(), src);
    }

    #[test]
    fn strip_jsonc_is_string_aware() {
        // // inside a string is not a comment
        let v = parse_jsonc(r#"{"url": "https://zed.dev", "x": 1,}"#).unwrap();
        assert_eq!(v["url"], "https://zed.dev");
        assert_eq!(v["x"], 1);
        // block comments and line comments stripped
        let v = parse_jsonc("/* head */\n{\n  // c\n  \"a\": [1, 2,],\n}\n").unwrap();
        assert_eq!(v["a"].as_array().unwrap().len(), 2);
        // escaped quote inside string
        let v = parse_jsonc(r#"{"s": "a\"//b"}"#).unwrap();
        assert_eq!(v["s"], "a\"//b");
        // invalid stays invalid
        assert!(parse_jsonc(r#"{"a"}"#).is_err());
        assert!(parse_jsonc("[1 2]").is_err());
    }

    #[test]
    fn missing_file_errors() {
        let path = std::env::temp_dir().join("zedxcode-definitely-missing.json");
        assert!(merge_marker_block(&path, "x", BLOCK).is_err());
    }

    #[test]
    fn atomic_write_installs_content_and_leaves_no_tmp() {
        let path = tmpfile("aw.json", "{}\n");
        atomic_write(&path, "{\"a\": 1}\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\"a\": 1}\n");
        // The tmp is pid-scoped and consumed by the rename — no stray tmp
        // file survives (a fixed name would race concurrent writers).
        let strays: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| {
                let n = e.unwrap().file_name().to_string_lossy().into_owned();
                n.contains(".zedxcode-tmp").then_some(n)
            })
            .collect();
        assert!(strays.is_empty(), "leftover tmp files: {strays:?}");
    }
}
