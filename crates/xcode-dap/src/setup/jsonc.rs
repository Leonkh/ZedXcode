//! Marker-block surgical merge for JSONC files (keymap.json, settings.json).
//! Text surgery, never re-serialization — user comments/formatting/trailing
//! commas survive. See `docs/design/dap-proxy.md` §6.1.
//!
//! A block sits between `// >>> zedxcode:<id> v2 h=<hash> >>>` and
//! `// <<< zedxcode:<id> <<<`, where `<hash>` is the FNV-1a 64 hash
//! (16 hex digits) of the text between the two marker lines. The block is
//! ZedXcode's only while that text still hashes to `<hash>`, or, under 0.1's
//! unversioned start marker `// >>> zedxcode:<id> >>>`, while it equals the
//! block 0.1 wrote byte for byte. A block that holds exactly our entries, in
//! order and without comments, is ours as well: only its formatting changed
//! (a formatter rewrapped it, or its line breaks became CRLF). A block edited
//! by hand is never rewritten silently: the caller keeps it, moves the
//! entries ZedXcode did not write out of it (verbatim), or overwrites it.
//!
//! Changes are planned on text first ([`plan_merge`], [`plan_remove`]) and
//! written by [`write_change`], so a dry run shows exactly what a real run
//! writes.

// Also runs in DAP and BSP mode (through the selection and compile stores),
// whose stdout carries only protocol messages: no `print!` / `println!` here.
#![deny(clippy::print_stdout)]

use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::util::hash::{fnv1a64, fnv1a64_hex};

/// One marker block ZedXcode manages in a JSONC file.
#[derive(Debug, Clone, Copy)]
pub struct BlockSpec<'a> {
    /// Marker id: the `<id>` in `zedxcode:<id>`.
    pub id: &'a str,
    /// The entries setup writes now, the last one followed by a comma (the
    /// block goes in front of the file's own entries). Empty for a retired
    /// block, which is only ever removed.
    pub block: &'a str,
    /// The exact block 0.1 wrote under its unversioned start marker; a
    /// region that still holds exactly this is adopted as ZedXcode's.
    pub legacy: Option<&'a str>,
}

/// What to do with a block that was edited by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnEdited {
    /// Leave it unchanged.
    #[default]
    Keep,
    /// Move the entries ZedXcode did not write out of it, verbatim: on a
    /// merge to just below the end marker (they still come after our
    /// entries, and in a keymap the later binding wins), on a removal to
    /// where the block was. Then rewrite (or delete) the block.
    Relocate,
    /// Overwrite (or delete) the block as a whole; the backup keeps the old
    /// text.
    Replace,
}

/// An entry between our markers that ZedXcode did not write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignEntry {
    /// One line for messages: a settings key, a keymap context with its
    /// keys, or the first line of a comment.
    pub label: String,
    /// The entry as it stands in the file, its comments included.
    pub text: String,
    /// Where in `text` the separating comma goes when the moved entry is
    /// followed by another value; `None` when it has one, or holds no value.
    comma_at: Option<usize>,
    /// Whether the entry holds a value (not only comments).
    has_value: bool,
}

/// Who a marker region belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockState {
    /// No region with this id.
    Absent,
    /// ZedXcode's: the hash matches, or (`legacy`) it is 0.1's block as 0.1
    /// wrote it, or it holds exactly our entries with other formatting.
    Owned { legacy: bool },
    /// Edited by hand; `foreign` lists what ZedXcode did not write.
    Edited { foreign: Vec<ForeignEntry> },
}

/// What a planned change does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeOutcome {
    /// New block, placed right after the opening `[` / `{`.
    Inserted,
    /// Our block, rewritten in place.
    Updated,
    /// Our block already holds the current text.
    Unchanged,
    /// Edited block: the foreign entries moved below it verbatim, then the
    /// block rewritten (merge); or the block deleted and the foreign entries
    /// left in its place (remove).
    Relocated,
    /// Edited block overwritten (merge), or deleted with its foreign entries
    /// (remove).
    Replaced,
    /// Our block deleted.
    Removed,
    /// Edited block left unchanged.
    Kept,
    /// No block to remove.
    NotPresent,
}

/// A planned change to one marker block; nothing is written until
/// [`write_change`].
#[derive(Debug, Clone)]
pub struct BlockChange {
    pub state: BlockState,
    pub outcome: MergeOutcome,
    /// The region as it stands, markers included; `None` when absent.
    pub before: Option<String>,
    /// What takes the region's place (moved entries included): empty when
    /// it is deleted, `None` when nothing changes.
    pub after: Option<String>,
    /// The whole new file text; `None` when nothing is written.
    new_text: Option<String>,
}

impl BlockChange {
    /// The entries ZedXcode did not write (empty unless edited by hand).
    pub fn foreign(&self) -> &[ForeignEntry] {
        match &self.state {
            BlockState::Edited { foreign } => foreign,
            _ => &[],
        }
    }
}

/// Plan merging `spec.block` into `text`: a block that is ours is rewritten
/// in place, a missing one is inserted right after the opening `[` / `{` of
/// the top-level value (Zed's own writers append at the end, so entries they
/// add never land between our markers), and a block edited by hand is
/// handled as `on_edited` says.
pub fn plan_merge(text: &str, spec: &BlockSpec, on_edited: OnEdited) -> Result<BlockChange> {
    if last_significant_byte(spec.block) != Some(b',') {
        bail!(
            "internal error: the zedxcode:{} block must end with a comma",
            spec.id
        );
    }
    let rendered = render_region(spec.id, spec.block);
    let Some(region) = find_marker_region(text, spec.id)? else {
        let new_text = insert_after_opener(text, &rendered)?;
        return Ok(BlockChange {
            state: BlockState::Absent,
            outcome: MergeOutcome::Inserted,
            before: None,
            after: Some(rendered),
            new_text: Some(new_text),
        });
    };
    let before = text[region.start..region.end].to_string();
    let state = block_state(text, &region, spec)?;
    // Where the untouched rest of the file starts.
    let mut end = region.end;
    let (outcome, after) = match (&state, on_edited) {
        (BlockState::Edited { .. }, OnEdited::Keep) => (MergeOutcome::Kept, None),
        (BlockState::Edited { foreign }, OnEdited::Relocate) => {
            // Below the end marker: the moved entries keep coming after ours,
            // so a binding of the user's still wins over ours for its key.
            let moved = moved_text(foreign, value_follows(text, region.end));
            let after = if moved.is_empty() {
                rendered
            } else {
                // The end-marker line's line break goes before the moved
                // entries, which end with their own.
                end += line_break_len(&text[region.end..]);
                format!("{rendered}\n{moved}")
            };
            (MergeOutcome::Relocated, Some(after))
        }
        (BlockState::Edited { .. }, OnEdited::Replace) => (MergeOutcome::Replaced, Some(rendered)),
        _ if before == rendered => (MergeOutcome::Unchanged, None),
        _ => (MergeOutcome::Updated, Some(rendered)),
    };
    // Whatever replaces the region holds entries: the value before it needs
    // its separating comma.
    let new_text = after.as_ref().map(|after| {
        format!(
            "{}{after}{}",
            with_separator(&text[..region.start]),
            &text[end..]
        )
    });
    Ok(BlockChange {
        state,
        outcome,
        before: Some(before),
        after,
        new_text,
    })
}

/// Plan removing the `spec.id` block from `text`: a block that is ours is
/// deleted; one edited by hand is handled as `on_edited` says (`Relocate`
/// keeps the foreign entries where the block was, verbatim).
pub fn plan_remove(text: &str, spec: &BlockSpec, on_edited: OnEdited) -> Result<BlockChange> {
    let Some(region) = find_marker_region(text, spec.id)? else {
        return Ok(BlockChange {
            state: BlockState::Absent,
            outcome: MergeOutcome::NotPresent,
            before: None,
            after: None,
            new_text: None,
        });
    };
    let before = text[region.start..region.end].to_string();
    let state = block_state(text, &region, spec)?;
    let (outcome, after) = match (&state, on_edited) {
        (BlockState::Edited { .. }, OnEdited::Keep) => (MergeOutcome::Kept, None),
        (BlockState::Edited { foreign }, OnEdited::Relocate) => (
            MergeOutcome::Relocated,
            Some(moved_text(foreign, value_follows(text, region.end))),
        ),
        (BlockState::Edited { .. }, OnEdited::Replace) => {
            (MergeOutcome::Replaced, Some(String::new()))
        }
        _ => (MergeOutcome::Removed, Some(String::new())),
    };
    let keeps_values = outcome == MergeOutcome::Relocated
        && match &state {
            BlockState::Edited { foreign } => foreign.iter().any(|f| f.has_value),
            _ => false,
        };
    // The end-marker line's line break goes with the region.
    let end = region.end + line_break_len(&text[region.end..]);
    let new_text = after.as_ref().map(|after| {
        let prefix = &text[..region.start];
        let prefix = if keeps_values {
            with_separator(prefix)
        } else {
            prefix.to_string()
        };
        format!("{prefix}{after}{}", &text[end..])
    });
    Ok(BlockChange {
        state,
        outcome,
        before: Some(before),
        after,
        new_text,
    })
}

/// Write a planned change to `path`, whose content was `original` when the
/// change was planned: timestamped backup, atomic write, post-write JSONC
/// validation (the original is restored when it fails). Returns the backup's
/// path, or `None` when the change writes nothing.
pub fn write_change(path: &Path, original: &str, change: &BlockChange) -> Result<Option<PathBuf>> {
    match &change.new_text {
        Some(new_text) => write_validated(path, original, new_text).map(Some),
        None => Ok(None),
    }
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
/// original content (and keeps the backup) when validation fails. Returns
/// the backup's path. Refuses when the file no longer holds `original`
/// (written while a prompt waited, e.g. by Zed): writing would lose that.
fn write_validated(path: &Path, original: &str, new_content: &str) -> Result<PathBuf> {
    let current =
        fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    if current != original {
        bail!(
            "{} changed on disk while setup was running; nothing was written — re-run setup",
            path.display()
        );
    }
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
    Ok(backup)
}

/// v2 start-marker line for a block whose inner text is `body`.
fn start_marker(marker_id: &str, body: &str) -> String {
    format!(
        "// >>> zedxcode:{marker_id} v2 h={} >>>",
        fnv1a64_hex(body.as_bytes())
    )
}

/// End-marker line (the same in 0.1 and v2).
fn end_marker(marker_id: &str) -> String {
    format!("// <<< zedxcode:{marker_id} <<<")
}

/// Marker block region rendered at the file's 2-space indent level; the
/// hash covers exactly the lines between the markers.
fn render_region(marker_id: &str, block: &str) -> String {
    let body = block.trim_end_matches('\n');
    format!(
        "  {}\n{body}\n  {}",
        start_marker(marker_id, body),
        end_marker(marker_id)
    )
}

/// Which start marker a region has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Marker {
    /// 0.1's `// >>> zedxcode:<id> >>>`.
    Legacy,
    /// `// >>> zedxcode:<id> v2 h=<hash> >>>`; `None` when the hash does not
    /// read as 16 hex digits (then only exactly our entries make it ours).
    V2(Option<u64>),
}

/// A marker region in a file's text.
#[derive(Debug)]
struct Region {
    /// Start of the start-marker line.
    start: usize,
    /// End of the end-marker line, before its line break.
    end: usize,
    /// The lines between the markers, the last line break included.
    inner: Range<usize>,
    marker: Marker,
}

/// Read a (trimmed) line as a start marker for `marker_id`.
fn parse_start_marker(line: &str, marker_id: &str) -> Option<Marker> {
    let rest = line
        .strip_prefix("// >>> zedxcode:")?
        .strip_prefix(marker_id)?;
    if rest == " >>>" {
        return Some(Marker::Legacy);
    }
    let hash = rest.strip_prefix(" v2 ")?.strip_suffix(" >>>")?;
    let hash = hash
        .strip_prefix("h=")
        .filter(|h| h.len() == 16 && h.bytes().all(|c| c.is_ascii_hexdigit()))
        .and_then(|h| u64::from_str_radix(h, 16).ok());
    Some(Marker::V2(hash))
}

/// The first `marker_id` region: from the start of the start-marker line
/// (0.1 or v2) to the end of the end-marker line.
fn find_marker_region(text: &str, marker_id: &str) -> Result<Option<Region>> {
    let end_m = end_marker(marker_id);
    // (line start, end of the line incl. its break, marker)
    let mut start: Option<(usize, usize, Marker)> = None;
    // (line start, end of the line excl. its break)
    let mut end: Option<(usize, usize)> = None;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        let next = offset + line.len();
        if start.is_none() {
            if let Some(marker) = parse_start_marker(trimmed, marker_id) {
                start = Some((offset, next, marker));
                offset = next;
                continue;
            }
        }
        if end.is_none() && trimmed == end_m {
            end = Some((offset, offset + line.trim_end_matches(['\n', '\r']).len()));
        }
        offset = next;
    }
    match (start, end) {
        (None, None) => Ok(None),
        (Some((start, inner_start, marker)), Some((end_line, end))) if end_line >= inner_start => {
            Ok(Some(Region {
                start,
                end,
                inner: inner_start..end_line,
                marker,
            }))
        }
        _ => bail!(
            "corrupt zedxcode:{marker_id} marker block (one marker missing or out of order); \
             fix the file manually"
        ),
    }
}

/// Length of the line break `s` starts with (0, 1 or 2 bytes).
fn line_break_len(s: &str) -> usize {
    if s.starts_with("\r\n") {
        2
    } else if s.starts_with('\n') {
        1
    } else {
        0
    }
}

/// Owned or edited, with the foreign entries of an edited block.
fn block_state(text: &str, region: &Region, spec: &BlockSpec) -> Result<BlockState> {
    let inner = &text[region.inner.clone()];
    // The hash covers the lines between the markers without the line break
    // that ends the last one (what `render_region` hashes).
    let body = inner.strip_suffix('\n').unwrap_or(inner);
    let owned = match region.marker {
        Marker::V2(hash) => hash == Some(fnv1a64(body.as_bytes())),
        Marker::Legacy => spec
            .legacy
            .is_some_and(|legacy| body == legacy.trim_end_matches('\n')),
    };
    if owned {
        return Ok(BlockState::Owned {
            legacy: region.marker == Marker::Legacy,
        });
    }
    let kind = enclosing_container(text, region.start).with_context(|| {
        format!(
            "the zedxcode:{} block is not inside the file's top-level array or object",
            spec.id
        )
    })?;
    let blocks = [Some(spec.block), spec.legacy].into_iter().flatten();
    // Only the formatting differs from one of our blocks (a formatter
    // rewrapped long lines, CRLF line breaks): nothing of the user's is in
    // it, so it stays ours. A deleted, added or commented entry does not.
    if blocks.clone().any(|block| same_entries(inner, block, kind)) {
        return Ok(BlockState::Owned {
            legacy: region.marker == Marker::Legacy,
        });
    }
    let ours: Vec<Value> = blocks.flat_map(|block| entry_values(block, kind)).collect();
    Ok(BlockState::Edited {
        foreign: foreign_entries(inner, kind, &ours),
    })
}

/// Whether `inner` holds exactly the entries of `block`, in the same order
/// and without comments.
fn same_entries(inner: &str, block: &str, kind: Container) -> bool {
    if classify(inner).contains(&Class::Comment) {
        return false;
    }
    let values = |text: &str| -> Option<Vec<Value>> {
        split_entries(text)?
            .into_iter()
            .map(|entry| parse_entry(&text[entry.value?], kind))
            .collect()
    };
    matches!((values(inner), values(block)), (Some(a), Some(b)) if a == b)
}

/// The kind of container entries live in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Container {
    /// Entries are elements (keymap.json).
    Array,
    /// Entries are `"key": value` members (settings.json).
    Object,
}

/// The innermost array or object that is open at byte `at`.
fn enclosing_container(text: &str, at: usize) -> Option<Container> {
    let prefix = &text[..at];
    let cls = classify(prefix);
    let mut stack = Vec::new();
    for (&c, &class) in prefix.as_bytes().iter().zip(&cls) {
        if class != Class::Code {
            continue;
        }
        match c {
            b'[' => stack.push(Container::Array),
            b'{' => stack.push(Container::Object),
            b']' | b'}' => {
                stack.pop();
            }
            _ => {}
        }
    }
    stack.last().copied()
}

/// Parse one entry: an element, or a `"key": value` member (as a one-key
/// object).
fn parse_entry(text: &str, kind: Container) -> Option<Value> {
    match kind {
        Container::Array => parse_jsonc(text).ok(),
        Container::Object => parse_jsonc(&format!("{{{text}}}")).ok(),
    }
}

/// The parsed entries of one of our blocks.
fn entry_values(block: &str, kind: Container) -> Vec<Value> {
    split_entries(block)
        .unwrap_or_default()
        .iter()
        .filter_map(|e| e.value.clone())
        .filter_map(|v| parse_entry(&block[v], kind))
        .collect()
}

/// The entries between the markers that ZedXcode did not write: every entry
/// that carries a comment or whose value is not one of `ours`, and comments
/// after the last entry.
fn foreign_entries(inner: &str, kind: Container, ours: &[Value]) -> Vec<ForeignEntry> {
    let Some(entries) = split_entries(inner) else {
        // Not a plain list of entries (an unbalanced bracket): the whole
        // text counts as one entry, moved as it is.
        return vec![ForeignEntry {
            label: first_line(inner),
            text: inner.to_string(),
            comma_at: None,
            has_value: true,
        }];
    };
    let cls = classify(inner);
    entries
        .into_iter()
        .filter_map(|entry| {
            let text = &inner[entry.seg.clone()];
            let Some(value) = entry.value else {
                return Some(ForeignEntry {
                    label: first_line(text),
                    text: text.to_string(),
                    comma_at: None,
                    has_value: false,
                });
            };
            let parsed = parse_entry(&inner[value.clone()], kind);
            let has_comment = cls[entry.seg.clone()].contains(&Class::Comment);
            if !has_comment && parsed.as_ref().is_some_and(|p| ours.contains(p)) {
                return None;
            }
            Some(ForeignEntry {
                label: entry_label(kind, parsed.as_ref(), &inner[value.clone()]),
                text: text.to_string(),
                comma_at: (!entry.has_comma).then(|| value.end - entry.seg.start),
                has_value: true,
            })
        })
        .collect()
}

/// The foreign entries' text as it moves, in file order, each on its own
/// lines. A value gains the separating comma it lacks when another value
/// follows it; the last one only when `value_follows` (nothing is added in
/// front of a closing `]` / `}`, so a moved entry stays byte for byte).
fn moved_text(foreign: &[ForeignEntry], value_follows: bool) -> String {
    let last_value = foreign.iter().rposition(|f| f.has_value);
    foreign
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let mut text = f.text.clone();
            if let Some(at) = f.comma_at {
                if value_follows || Some(i) != last_value {
                    text.insert(at, ',');
                }
            }
            with_final_newline(text)
        })
        .collect()
}

/// Whether another value follows byte `at` in its container: the next
/// significant byte neither closes the container nor is a comma.
fn value_follows(text: &str, at: usize) -> bool {
    let b = text.as_bytes();
    let cls = classify(text);
    next_significant(b, &cls, at)
        .is_some_and(|i| cls[i] == Class::Str || !matches!(b[i], b']' | b'}' | b','))
}

fn with_final_newline(mut text: String) -> String {
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

/// Labels longer than this are cut with "…".
const LABEL_MAX_CHARS: usize = 60;

fn shorten(text: &str) -> String {
    if text.chars().count() <= LABEL_MAX_CHARS {
        return text.to_string();
    }
    let cut: String = text.chars().take(LABEL_MAX_CHARS - 1).collect();
    format!("{cut}…")
}

/// The first non-blank line of `text`, trimmed and shortened.
fn first_line(text: &str) -> String {
    shorten(
        text.lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or(""),
    )
}

/// One line naming an entry: `"key"` for a member, `<context>: <keys>` for a
/// keymap entry, compact JSON otherwise.
fn entry_label(kind: Container, value: Option<&Value>, raw: &str) -> String {
    let Some(value) = value else {
        return first_line(raw);
    };
    match (kind, value) {
        (Container::Object, Value::Object(member)) if member.len() == 1 => member
            .keys()
            .next()
            .map(|key| shorten(&format!("\"{key}\"")))
            .unwrap_or_default(),
        (Container::Array, Value::Object(entry)) => {
            match entry.get("bindings").and_then(Value::as_object) {
                Some(bindings) => {
                    let context = entry
                        .get("context")
                        .and_then(Value::as_str)
                        .unwrap_or("no context");
                    let keys: Vec<&str> = bindings.keys().map(String::as_str).collect();
                    if keys.is_empty() {
                        shorten(&format!("{context}: no bindings"))
                    } else {
                        shorten(&format!("{context}: {}", keys.join(", ")))
                    }
                }
                None => shorten(&value.to_string()),
            }
        }
        _ => shorten(&value.to_string()),
    }
}

/// One entry of a marker region: an array element or an object member, with
/// the comments above it and the rest of its last line.
#[derive(Debug)]
struct Entry {
    /// The whole segment, comments and line breaks included.
    seg: Range<usize>,
    /// The element or `"key": value` member itself; `None` for comments
    /// after the last entry.
    value: Option<Range<usize>>,
    /// Whether a separating comma follows the value.
    has_comma: bool,
}

/// Split the text between two markers into entries (string- and
/// comment-aware). Comments on their own lines belong to the entry below
/// them; a comment after an entry on its line belongs to that entry. `None`
/// when the text is not a plain list of entries (an unbalanced bracket).
fn split_entries(inner: &str) -> Option<Vec<Entry>> {
    let b = inner.as_bytes();
    let cls = classify(inner);
    let mut entries = Vec::new();
    let mut seg_start = 0;
    loop {
        let Some(value_start) = next_significant(b, &cls, seg_start) else {
            if cls[seg_start..].contains(&Class::Comment) {
                entries.push(Entry {
                    seg: seg_start..b.len(),
                    value: None,
                    has_comma: false,
                });
            }
            return Some(entries);
        };
        let mut depth = 0usize;
        let mut last = value_start;
        let mut comma = None;
        for i in value_start..b.len() {
            if !is_significant(b, &cls, i) {
                continue;
            }
            if cls[i] == Class::Code {
                match b[i] {
                    b'[' | b'{' => depth += 1,
                    b']' | b'}' => depth = depth.checked_sub(1)?,
                    b',' if depth == 0 => {
                        comma = Some(i);
                        break;
                    }
                    _ => {}
                }
            }
            last = i;
        }
        if depth != 0 {
            return None;
        }
        let value_end = last + 1;
        let seg_end = rest_of_line(b, &cls, comma.map_or(value_end, |c| c + 1));
        entries.push(Entry {
            seg: seg_start..seg_end,
            value: Some(value_start..value_end),
            has_comma: comma.is_some(),
        });
        seg_start = seg_end;
    }
}

/// From `from`, take blanks and comments up to and including the line break;
/// stay at `from` when another value starts on the same line.
fn rest_of_line(b: &[u8], cls: &[Class], from: usize) -> usize {
    let mut i = from;
    while i < b.len() {
        match (cls[i], b[i]) {
            (Class::Comment, _) | (Class::Code, b' ' | b'\t' | b'\r') => i += 1,
            (Class::Code, b'\n') => return i + 1,
            _ => return from,
        }
    }
    i
}

/// `prefix` with a comma after its last value, unless one follows it
/// already or the last token opens the container.
fn with_separator(prefix: &str) -> String {
    let b = prefix.as_bytes();
    let cls = classify(prefix);
    match last_significant(b, &cls, b.len()) {
        Some(i) if cls[i] == Class::Str || !matches!(b[i], b',' | b'[' | b'{') => {
            format!("{},{}", &prefix[..=i], &prefix[i + 1..])
        }
        _ => prefix.to_string(),
    }
}

/// Insert `rendered` (the marker region) right after the opening `[` / `{`
/// of the top-level value, on its own lines.
fn insert_after_opener(text: &str, rendered: &str) -> Result<String> {
    let b = text.as_bytes();
    let cls = classify(text);
    let opener = match next_significant(b, &cls, 0) {
        Some(i) if cls[i] == Class::Code && matches!(b[i], b'[' | b'{') => i,
        _ => bail!("no top-level '[' or '{{' found — is this a JSON(C) file?"),
    };
    let line_end = b[opener..]
        .iter()
        .position(|&c| c == b'\n')
        .map(|p| opener + p);
    Ok(match line_end {
        // Only blanks or a line comment follow the opener on its line: the
        // block starts on the next line.
        Some(nl)
            if cls[nl] == Class::Code && (opener + 1..nl).all(|i| !is_significant(b, &cls, i)) =>
        {
            format!("{}\n{rendered}{}", &text[..nl], &text[nl..])
        }
        _ => format!("{}\n{rendered}\n{}", &text[..=opener], &text[opener + 1..]),
    })
}

// ---------------------------------------------------------------------------
// the string- and comment-aware scanner
// ---------------------------------------------------------------------------

/// What a byte of JSONC text belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    /// Structure, values and whitespace outside strings and comments.
    Code,
    /// A string literal, its quotes included.
    Str,
    /// A `//` or `/* */` comment, its delimiters included (not the line
    /// break that ends a line comment).
    Comment,
}

/// Classify every byte of `text`. Structural characters are ASCII, so
/// non-ASCII bytes simply take the class of their surroundings.
fn classify(text: &str) -> Vec<Class> {
    let b = text.as_bytes();
    let mut out = vec![Class::Code; b.len()];
    let mut i = 0;
    while i < b.len() {
        let start = i;
        match b[i] {
            b'"' => {
                i += 1;
                while i < b.len() {
                    match b[i] {
                        b'\\' => i += 2, // skip the escaped byte
                        b'"' => {
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
                i = i.min(b.len());
                out[start..i].fill(Class::Str);
            }
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                out[start..i].fill(Class::Comment);
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < b.len() && !(b[i] == b'*' && b.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i = (i + 2).min(b.len());
                out[start..i].fill(Class::Comment);
            }
            _ => i += 1,
        }
    }
    out
}

/// A byte that is part of a value or of the structure (not whitespace, not
/// a comment).
fn is_significant(b: &[u8], cls: &[Class], i: usize) -> bool {
    match cls[i] {
        Class::Str => true,
        Class::Code => !b[i].is_ascii_whitespace(),
        Class::Comment => false,
    }
}

fn next_significant(b: &[u8], cls: &[Class], from: usize) -> Option<usize> {
    (from..b.len()).find(|&i| is_significant(b, cls, i))
}

fn last_significant(b: &[u8], cls: &[Class], before: usize) -> Option<usize> {
    (0..before).rev().find(|&i| is_significant(b, cls, i))
}

/// The last significant byte of `text` (outside strings: a string ends with
/// its quote).
fn last_significant_byte(text: &str) -> Option<u8> {
    let b = text.as_bytes();
    last_significant(b, &classify(text), b.len()).map(|i| b[i])
}

/// Strip `//` and `/* */` comments and trailing commas, string-aware, so the
/// result parses with serde_json.
fn strip_jsonc(text: &str) -> String {
    let b = text.as_bytes();
    let cls = classify(text);
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    for (i, (&c, &class)) in b.iter().zip(&cls).enumerate() {
        match class {
            Class::Comment => {}
            // A trailing comma: the next value-or-structure byte closes.
            Class::Code
                if c == b','
                    && next_significant(b, &cls, i + 1)
                        .is_some_and(|j| cls[j] == Class::Code && matches!(b[j], b']' | b'}')) => {}
            _ => out.push(c),
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

    /// Replica of a typical `~/.config/zed/keymap.json` (header comments,
    /// array, two context blocks, trailing commas everywhere).
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

    /// Replica of a typical `~/.config/zed/settings.json` (header comments,
    /// nested objects, trailing commas).
    const SETTINGS_FIXTURE: &str = r#"// Zed settings
//
// For information on how to configure Zed, see the Zed
// documentation: https://zed.dev/docs/configuring-zed
{
  "autosave": {
    "after_delay": {
      "milliseconds": 0
    }
  },
  "format_on_save": "off",
  "theme": {
    "mode": "dark",
    "light": "One Light",
    "dark": "One Dark",
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

    const SPEC: BlockSpec<'static> = BlockSpec {
        id: "keymap",
        block: BLOCK,
        legacy: Some(BLOCK),
    };

    const OBJ_SPEC: BlockSpec<'static> = BlockSpec {
        id: "settings",
        block: OBJ_BLOCK,
        legacy: Some(OBJ_BLOCK),
    };

    /// A user entry that Zed's keymap editor appended inside our markers.
    const APPENDED: &str = r#"  {
    "context": "Editor",
    "bindings": {
      "alt-j": "editor::JoinLines"
    }
  }"#;

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

    /// Plan a merge and return the new text (panics when nothing changes).
    fn merged(text: &str, spec: &BlockSpec, on_edited: OnEdited) -> String {
        let change = plan_merge(text, spec, on_edited).unwrap();
        change.new_text.expect("the merge writes")
    }

    /// `text` as 0.1 left it: the unversioned markers before the final `]`.
    fn legacy_keymap(text: &str, inner: &str) -> String {
        let closer = text.rfind(']').unwrap();
        format!(
            "{}  // >>> zedxcode:keymap >>>\n{inner}\n  // <<< zedxcode:keymap <<<\n{}",
            &text[..closer],
            &text[closer..]
        )
    }

    /// The v2 region of `text` with `extra` added just before the end marker
    /// (where Zed's writers used to append).
    fn with_appended(text: &str, extra: &str) -> String {
        let end = text.find("  // <<< zedxcode:keymap <<<").unwrap();
        format!("{}{extra}\n{}", &text[..end], &text[end..])
    }

    #[test]
    fn new_block_goes_right_after_the_opening_bracket() {
        let change = plan_merge(KEYMAP_FIXTURE, &SPEC, OnEdited::Keep).unwrap();
        assert_eq!(change.outcome, MergeOutcome::Inserted);
        assert_eq!(change.state, BlockState::Absent);
        let text = change.new_text.unwrap();
        // The header is untouched; the block opens the array.
        let (head, rest) = text.split_at(text.find("[\n").unwrap() + 2);
        assert_eq!(head, &KEYMAP_FIXTURE[..head.len()]);
        assert!(
            rest.starts_with("  // >>> zedxcode:keymap v2 h="),
            "block is not first:\n{text}"
        );
        // The user's entries follow the block, byte for byte.
        let after_block = &rest[rest.find("// <<< zedxcode:keymap <<<\n").unwrap() + 27..];
        assert_eq!(
            after_block,
            &KEYMAP_FIXTURE[KEYMAP_FIXTURE.find("[\n").unwrap() + 2..]
        );
        let v = parse_jsonc(&text).unwrap();
        let entries = v.as_array().unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0]["bindings"]["cmd-r"], "debugger::Rerun");
    }

    #[test]
    fn new_object_block_goes_right_after_the_opening_brace() {
        let text = merged(SETTINGS_FIXTURE, &OBJ_SPEC, OnEdited::Keep);
        let open = text.find("{\n").unwrap();
        assert!(text[open + 2..].starts_with("  // >>> zedxcode:settings v2 h="));
        assert!(
            text.find("// <<< zedxcode:settings <<<").unwrap() < text.find("\"autosave\"").unwrap()
        );
        let v = parse_jsonc(&text).unwrap();
        assert_eq!(v["auto_install_extensions"]["swift"], true);
        assert_eq!(v["theme"]["dark"], "One Dark");
    }

    #[test]
    fn insert_into_empty_and_single_line_values() {
        for (src, spec, len) in [
            ("[]\n", &SPEC, 1),
            ("[{\"a\": 1}]\n", &SPEC, 2),
            ("{}\n", &OBJ_SPEC, 1),
            ("{ \"a\": 1 }", &OBJ_SPEC, 2),
            ("[ // my bindings\n]\n", &SPEC, 1),
        ] {
            let text = merged(src, spec, OnEdited::Keep);
            let v = parse_jsonc(&text).unwrap_or_else(|e| panic!("{e}:\n{text}"));
            let n = v
                .as_array()
                .map(Vec::len)
                .or(v.as_object().map(|o| o.len()));
            assert_eq!(n, Some(len), "{text}");
        }
    }

    #[test]
    fn opener_inside_a_comment_or_string_is_skipped() {
        let src = "// header with [ and {\n/* [ */\n[\n  {\"note\": \"[ ] {\"}\n]\n";
        let text = merged(src, &SPEC, OnEdited::Keep);
        let v = parse_jsonc(&text).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 2);
        assert!(text.starts_with("// header with [ and {\n/* [ */\n[\n  // >>> zedxcode:keymap v2"));
    }

    #[test]
    fn v2_marker_carries_the_hash_of_the_inner_text() {
        let text = merged("[]\n", &SPEC, OnEdited::Keep);
        let start = text
            .lines()
            .find(|l| l.contains(">>> zedxcode:keymap"))
            .unwrap();
        let hash = start
            .trim()
            .strip_prefix("// >>> zedxcode:keymap v2 h=")
            .and_then(|r| r.strip_suffix(" >>>"))
            .unwrap();
        assert_eq!(hash.len(), 16);
        assert!(hash.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')));
        assert_eq!(hash, fnv1a64_hex(BLOCK.as_bytes()));
        // The end marker is 0.1's.
        assert!(text.contains("\n  // <<< zedxcode:keymap <<<\n"));
    }

    #[test]
    fn double_run_is_byte_identical_and_makes_no_new_backup() {
        let path = tmpfile("keymap.json", KEYMAP_FIXTURE);
        let change = plan_merge(KEYMAP_FIXTURE, &SPEC, OnEdited::Keep).unwrap();
        assert!(write_change(&path, KEYMAP_FIXTURE, &change)
            .unwrap()
            .is_some());
        let first = fs::read_to_string(&path).unwrap();
        let change = plan_merge(&first, &SPEC, OnEdited::Keep).unwrap();
        assert_eq!(change.outcome, MergeOutcome::Unchanged);
        assert_eq!(change.state, BlockState::Owned { legacy: false });
        assert!(write_change(&path, &first, &change).unwrap().is_none());
        assert_eq!(fs::read_to_string(&path).unwrap(), first);
        assert_eq!(
            backups_for(&path).len(),
            1,
            "Unchanged run must not back up"
        );
    }

    #[test]
    fn owned_block_is_updated_in_place() {
        let first = merged(KEYMAP_FIXTURE, &SPEC, OnEdited::Keep);
        let new_block = BLOCK.replace("cmd-r", "cmd-e");
        let spec = BlockSpec {
            block: &new_block,
            ..SPEC
        };
        let change = plan_merge(&first, &spec, OnEdited::Keep).unwrap();
        assert_eq!(change.outcome, MergeOutcome::Updated);
        let text = change.new_text.unwrap();
        assert!(text.contains("cmd-e") && !text.contains("cmd-r"));
        assert_eq!(text.matches(">>> zedxcode:keymap").count(), 1);
        // Same place: right after the opening bracket.
        assert_eq!(text.find("// >>>"), first.find("// >>>"));
        parse_jsonc(&text).unwrap();
        // The new hash makes it ours again.
        assert_eq!(
            plan_merge(&text, &spec, OnEdited::Keep).unwrap().outcome,
            MergeOutcome::Unchanged
        );
    }

    #[test]
    fn legacy_block_is_adopted_and_updated_in_place() {
        let old = legacy_keymap(KEYMAP_FIXTURE, BLOCK);
        parse_jsonc(&old).unwrap();
        let change = plan_merge(&old, &SPEC, OnEdited::Keep).unwrap();
        assert_eq!(change.state, BlockState::Owned { legacy: true });
        assert_eq!(change.outcome, MergeOutcome::Updated);
        let text = change.new_text.unwrap();
        // In place: still where 0.1 put it, now with the v2 marker.
        assert_eq!(
            text.find("  // >>> zedxcode:keymap"),
            old.find("  // >>> zedxcode:keymap")
        );
        assert_eq!(
            text,
            old.replace(
                "// >>> zedxcode:keymap >>>",
                &format!(
                    "// >>> zedxcode:keymap v2 h={} >>>",
                    fnv1a64_hex(BLOCK.as_bytes())
                )
            )
        );
        assert_eq!(
            plan_merge(&text, &SPEC, OnEdited::Keep).unwrap().outcome,
            MergeOutcome::Unchanged
        );
    }

    #[test]
    fn legacy_block_that_differs_from_the_0_1_text_is_edited() {
        let old = legacy_keymap(KEYMAP_FIXTURE, &BLOCK.replace("cmd-r", "cmd-y"));
        let change = plan_merge(&old, &SPEC, OnEdited::Keep).unwrap();
        assert_eq!(change.outcome, MergeOutcome::Kept);
        assert_eq!(change.foreign().len(), 1);
        assert_eq!(change.foreign()[0].label, "Workspace: cmd-y");
        assert!(change.new_text.is_none());
    }

    #[test]
    fn edited_block_is_kept_and_its_foreign_entries_listed() {
        let ours = merged(KEYMAP_FIXTURE, &SPEC, OnEdited::Keep);
        let edited = with_appended(&ours, &format!("{APPENDED},"));
        parse_jsonc(&edited).unwrap();
        let change = plan_merge(&edited, &SPEC, OnEdited::Keep).unwrap();
        assert_eq!(change.outcome, MergeOutcome::Kept);
        assert!(change.new_text.is_none() && change.after.is_none());
        let foreign = change.foreign();
        assert_eq!(foreign.len(), 1);
        assert_eq!(foreign[0].label, "Editor: alt-j");
        assert_eq!(foreign[0].text, format!("{APPENDED},\n"));
        // Owned-only remove: an edited block is not deleted either.
        let removal = plan_remove(&edited, &SPEC, OnEdited::Keep).unwrap();
        assert_eq!(removal.outcome, MergeOutcome::Kept);
        assert!(removal.new_text.is_none());
    }

    #[test]
    fn formatting_only_changes_keep_a_block_ours() {
        let ours = merged("[]\n", &SPEC, OnEdited::Keep);
        // A formatter rewrapped a line; another editor made the file CRLF.
        for edited in [
            ours.replace("\"cmd-r\": ", "\"cmd-r\":\n        "),
            ours.replace('\n', "\r\n"),
        ] {
            assert_ne!(edited, ours);
            let change = plan_merge(&edited, &SPEC, OnEdited::Keep).unwrap();
            assert_eq!(change.state, BlockState::Owned { legacy: false });
            assert_eq!(change.outcome, MergeOutcome::Updated);
            parse_jsonc(&change.new_text.unwrap()).unwrap();
            let removal = plan_remove(&edited, &SPEC, OnEdited::Keep).unwrap();
            assert_eq!(removal.outcome, MergeOutcome::Removed);
            assert!(!removal.new_text.unwrap().contains("zedxcode"));
        }
        // The same for 0.1's block under its unversioned marker.
        let old = legacy_keymap(
            KEYMAP_FIXTURE,
            &BLOCK.replace("\"cmd-r\": ", "\"cmd-r\":\n        "),
        );
        let change = plan_merge(&old, &SPEC, OnEdited::Keep).unwrap();
        assert_eq!(change.state, BlockState::Owned { legacy: true });
        assert_eq!(change.outcome, MergeOutcome::Updated);
    }

    #[test]
    fn deleted_entry_makes_a_block_edited_with_no_foreign_entries() {
        let two = format!("{BLOCK}\n{},", APPENDED.replace("alt-j", "alt-q"));
        let spec = BlockSpec {
            block: &two,
            legacy: None,
            ..SPEC
        };
        let ours = merged("[]\n", &spec, OnEdited::Keep);
        // The user deleted our second entry: nothing foreign, but not ours.
        let start = ours.find("  {\n    \"context\": \"Editor\"").unwrap();
        let end = ours.find("  // <<< zedxcode:keymap <<<").unwrap();
        let edited = format!("{}{}", &ours[..start], &ours[end..]);
        parse_jsonc(&edited).unwrap();
        let change = plan_merge(&edited, &spec, OnEdited::Keep).unwrap();
        assert_eq!(change.outcome, MergeOutcome::Kept);
        assert!(change.foreign().is_empty());
        assert_eq!(
            plan_remove(&edited, &spec, OnEdited::Keep).unwrap().outcome,
            MergeOutcome::Kept
        );
        // Relocating moves nothing and rewrites ours.
        assert_eq!(merged(&edited, &spec, OnEdited::Relocate), ours);
    }

    #[test]
    fn relocate_moves_foreign_entries_verbatim_below_the_end_marker() {
        // 0.1's block at the end of the array, where Zed's keymap editor
        // appended an entry (no trailing comma) inside the markers; above
        // ours, a commented entry added by hand.
        let commented = "  // my kill binding\n  {\"context\": \"Editor\", \"bindings\": {\"alt-k\": \"editor::Kill\"}}, // keep\n";
        let edited = legacy_keymap(KEYMAP_FIXTURE, &format!("{commented}{BLOCK}\n{APPENDED}"));
        parse_jsonc(&edited).unwrap();
        let change = plan_merge(&edited, &SPEC, OnEdited::Relocate).unwrap();
        assert_eq!(change.outcome, MergeOutcome::Relocated);
        let labels: Vec<_> = change.foreign().iter().map(|f| f.label.as_str()).collect();
        assert_eq!(labels, ["Editor: alt-k", "Editor: alt-j"]);
        let text = change.new_text.unwrap();
        // Ours is rewritten in place; the foreign entries follow it byte for
        // byte (comments travel with their entry; the last one needs no
        // comma in front of the `]`), so their bindings still win over ours.
        let closer = KEYMAP_FIXTURE.rfind(']').unwrap();
        let expected = format!(
            "{}{}\n{commented}{APPENDED}\n{}",
            &KEYMAP_FIXTURE[..closer],
            render_region("keymap", BLOCK),
            &KEYMAP_FIXTURE[closer..]
        );
        assert_eq!(text, expected);
        let v = parse_jsonc(&text).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 5);

        // Idempotent: the block is ours again; a second relocate writes nothing.
        let again = plan_merge(&text, &SPEC, OnEdited::Relocate).unwrap();
        assert_eq!(again.outcome, MergeOutcome::Unchanged);
        assert!(again.new_text.is_none());
    }

    #[test]
    fn relocated_entry_followed_by_a_value_keeps_its_comma() {
        // Our block opens the array; a user entry was added inside it.
        let ours = merged(KEYMAP_FIXTURE, &SPEC, OnEdited::Keep);
        let edited = with_appended(&ours, &format!("{APPENDED},"));
        let text = merged(&edited, &SPEC, OnEdited::Relocate);
        assert_eq!(
            text,
            ours.replacen(
                "// <<< zedxcode:keymap <<<\n",
                &format!("// <<< zedxcode:keymap <<<\n{APPENDED},\n"),
                1
            )
        );
        assert_eq!(parse_jsonc(&text).unwrap().as_array().unwrap().len(), 4);
    }

    #[test]
    fn relocate_adds_the_comma_the_value_before_the_block_lacks() {
        // The block holds only a comment; the entry before it has no comma.
        let src = "[\n  {\"a\": 1}\n  // >>> zedxcode:keymap v2 h=0000000000000000 >>>\n  // {\"old\": true}\n  // <<< zedxcode:keymap <<<\n]\n";
        parse_jsonc(src).unwrap();
        let change = plan_merge(src, &SPEC, OnEdited::Relocate).unwrap();
        assert_eq!(change.foreign().len(), 1);
        assert_eq!(change.foreign()[0].label, "// {\"old\": true}");
        let text = change.new_text.unwrap();
        assert_eq!(
            text,
            format!(
                "[\n  {{\"a\": 1}},\n{}\n  // {{\"old\": true}}\n]\n",
                render_region("keymap", BLOCK)
            )
        );
        assert_eq!(parse_jsonc(&text).unwrap().as_array().unwrap().len(), 2);
    }

    #[test]
    fn our_entry_with_a_comment_out_line_is_foreign() {
        let ours = merged("[]\n", &SPEC, OnEdited::Keep);
        let edited = ours.replace(
            "      \"cmd-r\": \"debugger::Rerun\"\n",
            "      // \"cmd-r\": \"debugger::Rerun\"\n",
        );
        let change = plan_merge(&edited, &SPEC, OnEdited::Keep).unwrap();
        assert_eq!(change.foreign().len(), 1);
        assert_eq!(change.foreign()[0].label, "Workspace: no bindings");
    }

    #[test]
    fn replace_overwrites_an_edited_block_and_keeps_a_backup() {
        let ours = merged(KEYMAP_FIXTURE, &SPEC, OnEdited::Keep);
        let edited = with_appended(&ours, &format!("{APPENDED},"));
        let path = tmpfile("keymap.json", &edited);
        let change = plan_merge(&edited, &SPEC, OnEdited::Replace).unwrap();
        assert_eq!(change.outcome, MergeOutcome::Replaced);
        let backup = write_change(&path, &edited, &change).unwrap().unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), ours);
        assert_eq!(fs::read_to_string(backup).unwrap(), edited);
    }

    #[test]
    fn remove_deletes_owned_blocks_only() {
        // Ours (v2): removed, restoring the original bytes.
        let ours = merged(KEYMAP_FIXTURE, &SPEC, OnEdited::Keep);
        let change = plan_remove(&ours, &SPEC, OnEdited::Keep).unwrap();
        assert_eq!(change.outcome, MergeOutcome::Removed);
        assert_eq!(change.new_text.unwrap(), KEYMAP_FIXTURE);
        // Ours (0.1, adopted): removed.
        let old = legacy_keymap(KEYMAP_FIXTURE, BLOCK);
        let change = plan_remove(&old, &SPEC, OnEdited::Keep).unwrap();
        assert_eq!(change.outcome, MergeOutcome::Removed);
        assert_eq!(change.new_text.unwrap(), KEYMAP_FIXTURE);
        // Edited: kept by default.
        let edited = with_appended(&ours, &format!("{APPENDED},"));
        assert_eq!(
            plan_remove(&edited, &SPEC, OnEdited::Keep).unwrap().outcome,
            MergeOutcome::Kept
        );
        // --relocate: the foreign entry stays, verbatim; ours and the markers go.
        let change = plan_remove(&edited, &SPEC, OnEdited::Relocate).unwrap();
        assert_eq!(change.outcome, MergeOutcome::Relocated);
        let text = change.new_text.unwrap();
        assert_eq!(
            text,
            KEYMAP_FIXTURE.replacen("[\n", &format!("[\n{APPENDED},\n"), 1)
        );
        assert!(!text.contains("zedxcode"));
        parse_jsonc(&text).unwrap();
        // --replace: everything between the markers goes.
        let change = plan_remove(&edited, &SPEC, OnEdited::Replace).unwrap();
        assert_eq!(change.outcome, MergeOutcome::Replaced);
        assert_eq!(change.new_text.unwrap(), KEYMAP_FIXTURE);
        // Nothing to remove.
        assert_eq!(
            plan_remove(KEYMAP_FIXTURE, &SPEC, OnEdited::Keep)
                .unwrap()
                .outcome,
            MergeOutcome::NotPresent
        );
    }

    #[test]
    fn retired_object_block_relocates_members_and_drops_ours() {
        let retired = BlockSpec {
            id: "settings",
            block: "",
            legacy: Some(OBJ_BLOCK),
        };
        let src = "{\n  \"a\": 1,\n  // >>> zedxcode:settings >>>\n  \"auto_install_extensions\": {\n    \"swift\": true\n  },\n  \"b\": [1, \"x,]\"],\n  \"c\": {\"d\": {}}\n  // <<< zedxcode:settings <<<\n}\n";
        parse_jsonc(src).unwrap();
        let change = plan_remove(src, &retired, OnEdited::Relocate).unwrap();
        let labels: Vec<_> = change.foreign().iter().map(|f| f.label.as_str()).collect();
        assert_eq!(labels, ["\"b\"", "\"c\""]);
        assert_eq!(
            change.new_text.unwrap(),
            "{\n  \"a\": 1,\n  \"b\": [1, \"x,]\"],\n  \"c\": {\"d\": {}}\n}\n"
        );
    }

    #[test]
    fn block_without_a_final_comma_is_rejected() {
        let spec = BlockSpec {
            block: "  {\"a\": 1}",
            ..SPEC
        };
        let err = plan_merge("[]\n", &spec, OnEdited::Keep).unwrap_err();
        assert!(err.to_string().contains("must end with a comma"), "{err}");
    }

    #[test]
    fn validation_failure_restores_original_and_keeps_backup() {
        let path = tmpfile("keymap.json", KEYMAP_FIXTURE);
        let spec = BlockSpec {
            block: "  {{{ not json,",
            ..SPEC
        };
        let change = plan_merge(KEYMAP_FIXTURE, &spec, OnEdited::Keep).unwrap();
        let err = write_change(&path, KEYMAP_FIXTURE, &change).unwrap_err();
        assert!(
            err.to_string().contains("validation"),
            "unexpected error: {err}"
        );
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text, KEYMAP_FIXTURE, "original must be restored");
        assert_eq!(backups_for(&path).len(), 1, "backup must be kept");
    }

    #[test]
    fn write_refuses_a_file_that_changed_since_planning() {
        let path = tmpfile("keymap.json", KEYMAP_FIXTURE);
        let change = plan_merge(KEYMAP_FIXTURE, &SPEC, OnEdited::Keep).unwrap();
        // Zed saved the file in the meantime.
        let saved = KEYMAP_FIXTURE.replace("shift shift", "shift space");
        fs::write(&path, &saved).unwrap();
        let err = write_change(&path, KEYMAP_FIXTURE, &change).unwrap_err();
        assert!(err.to_string().contains("changed on disk"), "{err}");
        assert_eq!(fs::read_to_string(&path).unwrap(), saved);
        assert!(backups_for(&path).is_empty());
    }

    #[test]
    fn corrupt_single_marker_errors() {
        for src in [
            "[\n  // >>> zedxcode:keymap >>>\n  {\"a\": 1}\n]\n",
            "[\n  // >>> zedxcode:keymap v2 h=0123456789abcdef >>>\n  {\"a\": 1}\n]\n",
            "[\n  // <<< zedxcode:keymap <<<\n  // >>> zedxcode:keymap >>>\n]\n",
        ] {
            let err = plan_merge(src, &SPEC, OnEdited::Keep).unwrap_err();
            assert!(
                err.to_string().contains("corrupt"),
                "unexpected error: {err}"
            );
        }
    }

    #[test]
    fn unreadable_hash_with_other_entries_is_edited() {
        let src = "[\n  // >>> zedxcode:keymap v2 h=xyz >>>\n  {\"a\": 1},\n  // <<< zedxcode:keymap <<<\n]\n";
        let change = plan_merge(src, &SPEC, OnEdited::Keep).unwrap();
        assert_eq!(change.outcome, MergeOutcome::Kept);
        assert_eq!(change.foreign().len(), 1);
    }

    #[test]
    fn entries_split_on_top_level_commas_only() {
        let inner = "  // lead\n  {\"k\": \"a,b\", \"n\": [1, 2]}, // tail\n  [3, {\"x\": \"}\"}]\n  /* end */\n";
        let entries = split_entries(inner).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(
            &inner[entries[0].seg.clone()],
            "  // lead\n  {\"k\": \"a,b\", \"n\": [1, 2]}, // tail\n"
        );
        assert!(entries[0].has_comma);
        assert_eq!(
            &inner[entries[1].value.clone().unwrap()],
            "[3, {\"x\": \"}\"}]"
        );
        assert!(!entries[1].has_comma);
        assert!(entries[2].value.is_none());
        assert_eq!(&inner[entries[2].seg.clone()], "  /* end */\n");
        // Unbalanced text is not a list of entries.
        assert!(split_entries("  {\"a\": 1}},\n").is_none());
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
        // a trailing comma before a comment and the closer
        let v = parse_jsonc("[1, /* c */ ]").unwrap();
        assert_eq!(v.as_array().unwrap().len(), 1);
        // escaped quote inside string
        let v = parse_jsonc(r#"{"s": "a\"//b"}"#).unwrap();
        assert_eq!(v["s"], "a\"//b");
        // a comma inside a string before a bracket is kept
        let v = parse_jsonc(r#"{"s": ",]"}"#).unwrap();
        assert_eq!(v["s"], ",]");
        // invalid stays invalid
        assert!(parse_jsonc(r#"{"a"}"#).is_err());
        assert!(parse_jsonc("[1 2]").is_err());
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
