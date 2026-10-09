//! Full-screen terminal picker for a Zed task terminal: raw mode on the
//! alternate screen, a key decoder for the byte stream a terminal sends, and
//! a picker model (tabs, sections, rows) that renders into plain lines.
//!
//! Callers check [`is_tty`] first and keep the numbered stdin loop of
//! `select.rs` when it is false (piped input, no terminal). [`run`] blocks
//! until the user picks a row (↵) or cancels (⎋). Both are a normal end: the
//! caller exits 0 either way, so a task with `hide: on_success` closes.
//! `run` drives any [`View`]; the [`Picker`] is one, and a screen with keys
//! of its own (the status hub, a line that waits for a key) can be another.
//!
//! Terminal handling:
//! - raw mode through termios, the alternate screen (`ESC[?1049h` / `l`) and
//!   a hidden cursor. All three are restored when `run` returns, when the
//!   picker's thread panics (a panic hook restores them before the message
//!   prints) and on SIGINT, SIGTERM and SIGHUP (the handler restores them,
//!   then re-raises the signal with its default action);
//! - the window size is read again on every wakeup. SIGWINCH interrupts the
//!   wait, and an idle tick catches a missed signal. Zed's terminal starts at
//!   a 100x6 placeholder and is resized to the pane afterwards, so the first
//!   frame is often drawn at that size.
//!
//! Keys: ↑↓ (`ESC[A/B`, or `ESC O A/B` in application cursor mode), ↵ (CR),
//! ⎋ (a lone ESC once a short timeout passes), ⇥ / ⇧⇥ (`\t`, `ESC[Z`) and
//! ←/→ switch tabs, ⌃R asks the caller to reload, `?` shows the keys, ⌫
//! erases, ⌃C cancels, and any other typed character extends a fuzzy filter
//! (its letters in order, case-insensitive).

// Nothing calls this module yet: the keyboard path moves select-scheme,
// select-destination (select-device), select-configuration and the status
// hub onto it, with select.rs's numbered loop as the fallback when
// `is_tty()` is false.
#![allow(dead_code)]

use std::cell::{Cell, UnsafeCell};
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Once;
use std::time::Duration;

use libc::c_int;

/// How long a lone ESC waits for the rest of an escape sequence before it
/// counts as the ⎋ key. A local terminal writes a whole sequence at once; the
/// wait matters only when one is split across reads.
const ESC_TIMEOUT: Duration = Duration::from_millis(50);
/// Idle wakeup: the window size is checked again and the caller may apply a
/// background refresh (`Request::Tick`).
const TICK: Duration = Duration::from_millis(250);
/// Rows a Recent section keeps.
pub const MAX_RECENT: usize = 5;

/// True when stdin and stdout are both a terminal that can draw the picker.
/// When false, callers keep the numbered stdin loop.
pub fn is_tty() -> bool {
    use std::io::IsTerminal;
    tty_usable(
        io::stdin().is_terminal(),
        io::stdout().is_terminal(),
        std::env::var_os("TERM").as_deref(),
    )
}

/// `TERM=dumb` cannot address the cursor or switch screens; an unset TERM is
/// given the benefit of the doubt.
fn tty_usable(stdin_tty: bool, stdout_tty: bool, term: Option<&OsStr>) -> bool {
    stdin_tty && stdout_tty && term.is_none_or(|t| t != "dumb")
}

// ---------------------------------------------------------------------------
// keys
// ---------------------------------------------------------------------------

/// A key the picker understands, decoded from the terminal's byte stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    /// ← and → switch tabs too, for terminals that keep ⇥ / ⇧⇥ to themselves.
    Left,
    Right,
    Enter,
    Escape,
    Tab,
    /// ⇧⇥ (`ESC [ Z`).
    BackTab,
    Backspace,
    /// ⌃R.
    Reload,
    /// ⌃C. Raw mode delivers it as a byte instead of SIGINT.
    Interrupt,
    Char(char),
}

const ESC: u8 = 0x1b;
/// A CSI sequence longer than this is garbage, not an unfinished key.
const MAX_CSI: usize = 32;

/// Turns raw terminal bytes into keys. Holds an unfinished escape sequence
/// or UTF-8 character until the next read completes it, or until the caller
/// reports a timeout with [`Decoder::flush`].
#[derive(Debug, Default)]
pub struct Decoder {
    pending: Vec<u8>,
}

enum Parse {
    /// A key and the number of bytes it used.
    Key(Key, usize),
    /// Bytes that mean nothing to the picker.
    Skip(usize),
    /// A prefix that the next read may complete.
    Incomplete,
}

impl Decoder {
    /// Decodes what `bytes` completes; an unfinished tail stays pending.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Key> {
        self.pending.extend_from_slice(bytes);
        let mut keys = Vec::new();
        self.decode(&mut keys);
        keys
    }

    /// True while bytes wait for the rest of a sequence; the caller then
    /// waits at most `ESC_TIMEOUT` before calling [`Decoder::flush`].
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// The wait ran out: a pending lone ESC is the ⎋ key, the bytes after it
    /// are decoded on their own, and an unfinished UTF-8 character is dropped.
    pub fn flush(&mut self) -> Vec<Key> {
        let mut keys = Vec::new();
        loop {
            self.decode(&mut keys);
            if self.pending.is_empty() {
                return keys;
            }
            if self.pending.remove(0) == ESC {
                keys.push(Key::Escape);
            }
        }
    }

    fn decode(&mut self, keys: &mut Vec<Key>) {
        let mut at = 0;
        while at < self.pending.len() {
            match parse(&self.pending[at..]) {
                Parse::Key(key, used) => {
                    keys.push(key);
                    at += used;
                }
                Parse::Skip(used) => at += used,
                Parse::Incomplete => break,
            }
        }
        self.pending.drain(..at);
    }
}

fn parse(b: &[u8]) -> Parse {
    match b[0] {
        ESC => parse_escape(b),
        b'\r' | b'\n' => Parse::Key(Key::Enter, 1),
        b'\t' => Parse::Key(Key::Tab, 1),
        0x7f | 0x08 => Parse::Key(Key::Backspace, 1),
        0x12 => Parse::Key(Key::Reload, 1),
        0x03 => Parse::Key(Key::Interrupt, 1),
        0x20..=0x7e => Parse::Key(Key::Char(b[0] as char), 1),
        0x00..=0x1f => Parse::Skip(1),
        _ => parse_utf8(b),
    }
}

fn parse_escape(b: &[u8]) -> Parse {
    match b.get(1) {
        None => Parse::Incomplete,
        Some(b'[') => parse_csi(b),
        // SS3: the arrows in application cursor mode (DECCKM), keypad ↵.
        Some(b'O') => match b.get(2) {
            None => Parse::Incomplete,
            Some(b'A') => Parse::Key(Key::Up, 3),
            Some(b'B') => Parse::Key(Key::Down, 3),
            Some(b'C') => Parse::Key(Key::Right, 3),
            Some(b'D') => Parse::Key(Key::Left, 3),
            Some(b'M') => Parse::Key(Key::Enter, 3),
            Some(&ESC) => Parse::Skip(2),
            Some(_) => Parse::Skip(3),
        },
        // Not a sequence introducer: the ESC was ⎋ on its own, and the byte
        // after it is decoded normally.
        Some(_) => Parse::Key(Key::Escape, 1),
    }
}

/// `ESC [`, parameter and intermediate bytes, one final byte (ECMA-48).
/// Modifier parameters are ignored: `ESC[1;2A` is still ↑.
fn parse_csi(b: &[u8]) -> Parse {
    for (i, &c) in b.iter().enumerate().skip(2) {
        match c {
            0x20..=0x3f => {}
            0x40..=0x7e => {
                let key = match c {
                    b'A' => Key::Up,
                    b'B' => Key::Down,
                    b'C' => Key::Right,
                    b'D' => Key::Left,
                    b'Z' => Key::BackTab,
                    _ => return Parse::Skip(i + 1),
                };
                return Parse::Key(key, i + 1);
            }
            // Not a CSI byte: drop the broken prefix and decode from here.
            _ => return Parse::Skip(i),
        }
    }
    if b.len() > MAX_CSI {
        Parse::Skip(b.len())
    } else {
        Parse::Incomplete
    }
}

fn parse_utf8(b: &[u8]) -> Parse {
    let len = match b[0] {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => return Parse::Skip(1),
    };
    if b.len() < len {
        // Wait only when the bytes so far can still complete a character.
        return if b[1..].iter().all(|c| c & 0xc0 == 0x80) {
            Parse::Incomplete
        } else {
            Parse::Skip(1)
        };
    }
    match std::str::from_utf8(&b[..len])
        .ok()
        .and_then(|s| s.chars().next())
    {
        Some(c) if !c.is_control() => Parse::Key(Key::Char(c), len),
        Some(_) => Parse::Skip(len),
        None => Parse::Skip(1),
    }
}

// ---------------------------------------------------------------------------
// picker model
// ---------------------------------------------------------------------------

/// Health of one header value: ✓, ! or ✗.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    Ok,
    Warning,
    Error,
}

impl Health {
    fn glyph(self) -> &'static str {
        match self {
            Health::Ok => "✓",
            Health::Warning => "!",
            Health::Error => "✗",
        }
    }

    /// SGR color: green, yellow, red.
    fn sgr(self) -> &'static str {
        match self {
            Health::Ok => "32",
            Health::Warning => "33",
            Health::Error => "31",
        }
    }
}

/// One value of the header line (`MyApp ▸ iPhone 17 · iOS 26.1 ▸ Debug`),
/// with an optional health glyph in front of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderPart {
    pub text: String,
    pub health: Option<Health>,
}

impl HeaderPart {
    pub fn new(text: impl Into<String>, health: Option<Health>) -> HeaderPart {
        HeaderPart {
            text: text.into(),
            health,
        }
    }
}

/// A choosable row. `label` is shown and filtered; `value` is what ↵ returns
/// (a scheme name, a simulator UDID, ...); `detail` is shown dimmed after the
/// label; `keywords` are filtered after the label but never shown; `current`
/// draws the ● marker and places the starting cursor.
///
/// The filter reads only `label` and `keywords`. Matching the detail or the
/// section title as well would let short queries hit nearly every row, so a
/// caller that wants more text to be typeable (the OS version of a device
/// listed under its runtime's section) puts it in `keywords`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub label: String,
    pub detail: String,
    pub keywords: String,
    pub value: String,
    pub current: bool,
}

impl Row {
    pub fn new(label: impl Into<String>, value: impl Into<String>) -> Row {
        Row {
            label: label.into(),
            detail: String::new(),
            keywords: String::new(),
            value: value.into(),
            current: false,
        }
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Row {
        self.detail = detail.into();
        self
    }

    pub fn keywords(mut self, keywords: impl Into<String>) -> Row {
        self.keywords = keywords.into();
        self
    }

    /// Whether the filter `query` keeps this row.
    fn matches(&self, query: &str) -> bool {
        let gap = if self.keywords.is_empty() { "" } else { " " };
        fuzzy_match_chars(
            query,
            self.label
                .chars()
                .chain(gap.chars())
                .chain(self.keywords.chars()),
        )
    }

    pub fn current(mut self, current: bool) -> Row {
        self.current = current;
        self
    }
}

/// Rows under a title line; an empty title draws no title line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    pub title: String,
    pub rows: Vec<Row>,
}

impl Section {
    pub fn new(title: impl Into<String>, rows: Vec<Row>) -> Section {
        Section {
            title: title.into(),
            rows,
        }
    }

    /// The Recent section: newest first, so the current value leads it and
    /// ↓↵ picks the previous one. Keeps at most `MAX_RECENT` rows.
    pub fn recent(mut rows: Vec<Row>) -> Section {
        rows.truncate(MAX_RECENT);
        Section::new("Recent", rows)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tab {
    pub title: String,
    pub sections: Vec<Section>,
}

impl Tab {
    pub fn new(title: impl Into<String>, sections: Vec<Section>) -> Tab {
        Tab {
            title: title.into(),
            sections,
        }
    }
}

/// How a picker session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// ↵ on a row: the tab it is on and the row's value.
    Picked { tab: usize, value: String },
    /// ⎋, ⌃C, or the terminal went away. The caller exits 0 all the same.
    Cancelled,
}

/// What a key did to a view; `T` is what the view hands back when it closes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step<T> {
    /// Keep going; the screen is redrawn if it changed.
    Continue,
    /// ⌃R: the caller fetches the lists again (`Request::Reload`).
    Reload,
    Done(T),
}

/// What the callback of [`run`] is asked to do. The frame is repainted in
/// full after either, which also covers anything another thread printed to
/// stderr meanwhile (it would otherwise sit on the alternate screen).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    /// ⌃R: fetch the lists again and hand them over with
    /// [`Picker::replace_sections`].
    Reload,
    /// No key for a while: a background refresh that has finished can be
    /// applied now; the list refreshes in place.
    Tick,
}

/// A full-screen view that [`run`] drives: the [`Picker`], or another screen
/// with keys of its own. All views share the raw mode and its restore paths,
/// the key decoder, resize handling and the redraw.
pub trait View {
    /// What the view hands back when it closes.
    type Outcome;
    /// Applies one key.
    fn handle(&mut self, key: Key) -> Step<Self::Outcome>;
    /// The frame for a `width` x `height` screen: one SGR-styled string per
    /// line, at most `height` of them, none wider than `width` columns.
    /// Missing lines are drawn blank.
    fn styled_lines(&self, width: usize, height: usize) -> Vec<String>;
    /// The outcome when the terminal closes under the view.
    fn closed(&self) -> Self::Outcome;
}

/// A row's place: (section, row) inside one tab.
type RowRef = (usize, usize);

/// The picker state machine. Feed it keys with [`View::handle`]; draw it
/// with [`Picker::render`] (plain text) or the styled lines `run` uses.
#[derive(Debug)]
pub struct Picker {
    header: Vec<HeaderPart>,
    tabs: Vec<Tab>,
    active: usize,
    /// One cursor per tab, kept while another tab is shown.
    cursors: Vec<Option<RowRef>>,
    /// Applies to the shown tab; switching tabs clears it.
    filter: String,
    help: bool,
    /// First body line on screen; rendering moves it to keep the cursor in
    /// view, so it lives in a Cell.
    top: Cell<usize>,
}

impl Picker {
    /// Opens on tab `active`. Each tab's cursor starts on its first current
    /// row (the Recent one when there is one), else on its first row.
    pub fn new(header: Vec<HeaderPart>, tabs: Vec<Tab>, active: usize) -> Picker {
        let cursors = tabs.iter().map(initial_cursor).collect();
        Picker {
            header,
            active: active.min(tabs.len().saturating_sub(1)),
            tabs,
            cursors,
            filter: String::new(),
            help: false,
            top: Cell::new(0),
        }
    }

    pub fn active_tab(&self) -> usize {
        self.active
    }

    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// The row under the cursor, if it is shown (the filter may hide it).
    pub fn selected(&self) -> Option<&Row> {
        let at = self.cursor_shown()?;
        Some(&self.tabs[self.active].sections[at.0].rows[at.1])
    }

    pub fn set_header(&mut self, header: Vec<HeaderPart>) {
        self.header = header;
    }

    /// New rows for `tab` (a reload or a background refresh). The cursor
    /// stays on the same value when it is still listed: in the section it
    /// was in (found by title, or by place when untitled) first, since a
    /// value can be listed twice (Recent and its runtime's section), else
    /// wherever it is. Otherwise it goes to the current row, else the first.
    pub fn replace_sections(&mut self, tab: usize, sections: Vec<Section>) {
        let Some(slot) = self.tabs.get_mut(tab) else {
            return;
        };
        let previous = self.cursors[tab].map(|(s, r)| {
            let section = &slot.sections[s];
            (s, section.title.clone(), section.rows[r].value.clone())
        });
        slot.sections = sections;
        let slot = &self.tabs[tab];
        let same = previous.and_then(|(place, title, value)| {
            let in_section = |s: usize| {
                let rows = &slot.sections[s].rows;
                rows.iter()
                    .position(|row| row.value == value)
                    .map(|r| (s, r))
            };
            (0..slot.sections.len())
                .filter(|&s| {
                    let section = &slot.sections[s];
                    section.title == title && (!title.is_empty() || s == place)
                })
                .find_map(in_section)
                .or_else(|| (0..slot.sections.len()).find_map(in_section))
        });
        self.cursors[tab] = same.or_else(|| initial_cursor(slot));
        if tab == self.active && !self.filter.is_empty() && self.cursor_shown().is_none() {
            self.cursors[tab] = self.shown().first().copied();
        }
    }

    /// The frame as plain text, `height` lines joined by `\n`, none wider
    /// than `width` columns and without trailing spaces.
    pub fn render(&self, width: usize, height: usize) -> String {
        self.frame(width, height)
            .iter()
            .map(Line::plain)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl View for Picker {
    type Outcome = Outcome;

    fn handle(&mut self, key: Key) -> Step<Outcome> {
        if self.help {
            // Any key closes the key list; it does nothing else, so ⎋ there
            // does not cancel the picker.
            if key == Key::Interrupt {
                return Step::Done(Outcome::Cancelled);
            }
            self.help = false;
            return Step::Continue;
        }
        match key {
            Key::Up => self.move_cursor(-1),
            Key::Down => self.move_cursor(1),
            Key::Tab | Key::Right => self.switch_tab(1),
            Key::BackTab | Key::Left => self.switch_tab(-1),
            Key::Enter => {
                if let Some(row) = self.selected() {
                    return Step::Done(Outcome::Picked {
                        tab: self.active,
                        value: row.value.clone(),
                    });
                }
            }
            Key::Escape | Key::Interrupt => return Step::Done(Outcome::Cancelled),
            Key::Reload => return Step::Reload,
            Key::Backspace => {
                if self.filter.pop().is_some() {
                    self.refilter();
                }
            }
            Key::Char('?') => self.help = true,
            Key::Char(c) => {
                self.filter.push(c);
                self.refilter();
            }
        }
        Step::Continue
    }

    fn styled_lines(&self, width: usize, height: usize) -> Vec<String> {
        self.frame(width, height).iter().map(Line::styled).collect()
    }

    fn closed(&self) -> Outcome {
        Outcome::Cancelled
    }
}

impl Picker {
    /// The rows of the shown tab that pass the filter, in list order.
    fn shown(&self) -> Vec<RowRef> {
        let Some(tab) = self.tabs.get(self.active) else {
            return Vec::new();
        };
        row_refs(tab)
            .filter(|&(s, r)| tab.sections[s].rows[r].matches(&self.filter))
            .collect()
    }

    fn cursor_shown(&self) -> Option<RowRef> {
        let at = (*self.cursors.get(self.active)?)?;
        self.shown().contains(&at).then_some(at)
    }

    fn move_cursor(&mut self, delta: isize) {
        let shown = self.shown();
        let Some(last) = shown.len().checked_sub(1) else {
            return;
        };
        let next = match self.cursor_shown() {
            Some(at) => {
                let pos = shown.iter().position(|&r| r == at).unwrap_or(0);
                pos.saturating_add_signed(delta).min(last)
            }
            None => 0,
        };
        self.cursors[self.active] = Some(shown[next]);
    }

    fn switch_tab(&mut self, delta: isize) {
        let n = self.tabs.len();
        if n < 2 {
            return;
        }
        self.active = (self.active + n).saturating_add_signed(delta) % n;
        self.filter.clear();
        self.top.set(0);
    }

    /// A changed filter puts the cursor on the first match, so typing and ↵
    /// picks the top hit. An emptied filter leaves the cursor where it is.
    fn refilter(&mut self) {
        self.top.set(0);
        if self.filter.is_empty() {
            return;
        }
        if let Some(&first) = self.shown().first() {
            self.cursors[self.active] = Some(first);
        }
    }
}

fn row_refs(tab: &Tab) -> impl Iterator<Item = RowRef> + '_ {
    tab.sections
        .iter()
        .enumerate()
        .flat_map(|(s, section)| (0..section.rows.len()).map(move |r| (s, r)))
}

fn initial_cursor(tab: &Tab) -> Option<RowRef> {
    row_refs(tab)
        .find(|&(s, r)| tab.sections[s].rows[r].current)
        .or_else(|| row_refs(tab).next())
}

/// Fuzzy subsequence match: every query character appears in `text`, in
/// order, ignoring case. The empty query matches everything.
pub fn fuzzy_match(query: &str, text: &str) -> bool {
    fuzzy_match_chars(query, text.chars())
}

fn fuzzy_match_chars(query: &str, text: impl Iterator<Item = char>) -> bool {
    let mut hay = text.flat_map(char::to_lowercase);
    query
        .chars()
        .flat_map(char::to_lowercase)
        .all(|q| hay.any(|h| h == q))
}

// ---------------------------------------------------------------------------
// rendering
// ---------------------------------------------------------------------------

/// The key list `?` shows, packed into as few lines as the width allows.
const HELP: &[(&str, &str)] = &[
    ("↑ ↓", "move"),
    ("↵", "choose"),
    ("⎋", "cancel, nothing changes"),
    ("⇥ ⇧⇥ ← →", "switch tab"),
    ("⌃R", "reload the lists"),
    ("letters", "filter, in order"),
    ("⌫", "erase the filter"),
    ("●", "the current value"),
];

/// A run of text with one SGR parameter list (`"1"` bold, `"2"` dim, `"32"`
/// green, ...; empty for plain text).
#[derive(Debug, Clone)]
struct Span {
    sgr: &'static str,
    text: String,
}

/// Columns `c` takes on a terminal: 2 for East Asian wide and fullwidth
/// characters and most emoji, 0 for combining marks and zero-width
/// characters, 1 otherwise. A short table rather than the full Unicode one:
/// it covers what a scheme or device name plausibly holds, and every glyph
/// the picker draws itself takes one column.
fn char_width(c: char) -> usize {
    match c as u32 {
        0x0300..=0x036f
        | 0x1ab0..=0x1aff
        | 0x1dc0..=0x1dff
        | 0x200b..=0x200f
        | 0x20d0..=0x20ff
        | 0xfe00..=0xfe0f
        | 0xfe20..=0xfe2f => 0,
        0x1100..=0x115f
        | 0x2e80..=0xa4cf
        | 0xac00..=0xd7a3
        | 0xf900..=0xfaff
        | 0xfe30..=0xfe4f
        | 0xff00..=0xff60
        | 0xffe0..=0xffe6
        | 0x1f300..=0x1f64f
        | 0x1f680..=0x1f6ff
        | 0x1f900..=0x1f9ff
        | 0x1fa70..=0x1faff
        | 0x20000..=0x3fffd => 2,
        _ => 1,
    }
}

fn text_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// One screen line; widths are terminal columns ([`char_width`]).
#[derive(Debug, Clone, Default)]
struct Line {
    spans: Vec<Span>,
    /// The cursor row, drawn in reverse video.
    reverse: bool,
}

impl Line {
    /// Control characters in `text` (from a scheme or device name) become
    /// spaces, so they cannot move the terminal's cursor.
    fn span(mut self, sgr: &'static str, text: impl Into<String>) -> Line {
        let mut text = text.into();
        if text.chars().any(char::is_control) {
            text = text
                .chars()
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect();
        }
        self.spans.push(Span { sgr, text });
        self
    }

    fn width(&self) -> usize {
        self.spans.iter().map(|s| text_width(&s.text)).sum()
    }

    /// Cuts the line to `width` columns, ending in … when anything was cut.
    fn fit(mut self, width: usize) -> Line {
        if self.width() <= width {
            return self;
        }
        let mut room = width.saturating_sub(1);
        let mut spans = Vec::new();
        let mut cut_sgr = "";
        for span in self.spans {
            if room == 0 {
                break;
            }
            let mut text = String::new();
            for c in span.text.chars() {
                let w = char_width(c);
                if w > room {
                    // A wide character that does not fit ends the line.
                    room = 0;
                    break;
                }
                room -= w;
                text.push(c);
            }
            cut_sgr = span.sgr;
            spans.push(Span {
                sgr: span.sgr,
                text,
            });
        }
        // No space in front of the ellipsis.
        while let Some(last) = spans.last_mut() {
            last.text.truncate(last.text.trim_end().len());
            if !last.text.is_empty() {
                break;
            }
            spans.pop();
        }
        if width > 0 {
            spans.push(Span {
                sgr: cut_sgr,
                text: "…".into(),
            });
        }
        self.spans = spans;
        self
    }

    /// Fills the line with spaces up to `width`, so the reverse-video cursor
    /// row spans the screen.
    fn pad(self, width: usize) -> Line {
        let missing = width.saturating_sub(self.width());
        if missing == 0 {
            self
        } else {
            self.span("", " ".repeat(missing))
        }
    }

    fn plain(&self) -> String {
        let text: String = self.spans.iter().map(|s| s.text.as_str()).collect();
        text.trim_end().to_string()
    }

    fn styled(&self) -> String {
        let mut out = String::new();
        for span in &self.spans {
            let params: Vec<&str> = [if self.reverse { "7" } else { "" }, span.sgr]
                .into_iter()
                .filter(|p| !p.is_empty())
                .collect();
            if params.is_empty() {
                out.push_str("\x1b[0m");
            } else {
                let _ = write!(out, "\x1b[0;{}m", params.join(";"));
            }
            out.push_str(&span.text);
        }
        out.push_str("\x1b[0m");
        out
    }
}

/// What a body line shows.
enum BodyLine {
    Title(usize),
    Row(RowRef),
    Note(String),
}

impl Picker {
    /// Header, tabs, body, footer. Short screens drop the footer first,
    /// then the tabs, then the header; the body keeps at least one line.
    fn frame(&self, width: usize, height: usize) -> Vec<Line> {
        if width == 0 || height == 0 {
            return Vec::new();
        }
        let (header, tabs, footer) = (height >= 2, height >= 3, height >= 4);
        let body_height = height - [header, tabs, footer].iter().filter(|&&b| b).count();
        let mut lines = Vec::with_capacity(height);
        if header {
            lines.push(self.header_line());
        }
        if tabs {
            lines.push(self.tabs_line(width));
        }
        lines.extend(self.body(width, body_height));
        if footer {
            lines.push(self.footer_line());
        }
        lines.into_iter().map(|l| l.fit(width)).collect()
    }

    fn header_line(&self) -> Line {
        let mut line = Line::default();
        for (i, part) in self.header.iter().enumerate() {
            if i > 0 {
                line = line.span("", " ▸ ");
            }
            if let Some(health) = part.health {
                line = line.span(health.sgr(), health.glyph()).span("", " ");
            }
            line = line.span("", part.text.clone());
        }
        line
    }

    /// ` Scheme  [Destination]  Configuration`, the shown tab in brackets
    /// (the titles keep their columns), then the cursor's position in the
    /// list at the right edge when it fits.
    fn tabs_line(&self, width: usize) -> Line {
        let mut line = Line::default();
        for (i, tab) in self.tabs.iter().enumerate() {
            if i > 0 {
                line = line.span("", " ");
            }
            line = if i == self.active {
                line.span("1", format!("[{}]", tab.title))
            } else {
                line.span("", format!(" {} ", tab.title))
            };
        }
        let shown = self.shown();
        if let Some(pos) = self
            .cursor_shown()
            .and_then(|at| shown.iter().position(|&r| r == at))
        {
            let count = format!("{}/{}", pos + 1, shown.len());
            let used = line.width() + text_width(&count);
            if used + 2 <= width {
                line = line.span("", " ".repeat(width - used)).span("2", count);
            }
        }
        line
    }

    fn footer_line(&self) -> Line {
        let text = if self.help {
            "any key closes this list".to_string()
        } else if !self.filter.is_empty() {
            format!("filter: {} · ⌫ erase · ↵ choose · ⎋ cancel", self.filter)
        } else {
            let tab = if self.tabs.len() > 1 {
                "⇥ tab · "
            } else {
                ""
            };
            format!("↑↓ move · ↵ choose · ⎋ cancel · {tab}⌃R reload · ? keys · type to filter")
        };
        Line::default().span("2", text)
    }

    /// Exactly `height` lines: the visible slice of the list, padded.
    fn body(&self, width: usize, height: usize) -> Vec<Line> {
        let mut out = if self.help {
            help_lines(width)
        } else {
            let lines = self.body_lines();
            let cursor = self.cursor_shown();
            let cursor_line = cursor.and_then(|at| {
                lines
                    .iter()
                    .position(|l| matches!(l, BodyLine::Row(r) if *r == at))
            });
            let top = self.scroll(&lines, cursor_line, height);
            lines
                .iter()
                .skip(top)
                .take(height)
                .map(|l| self.body_line(l, cursor, width))
                .collect()
        };
        out.truncate(height);
        out.resize_with(height, Line::default);
        out
    }

    fn body_lines(&self) -> Vec<BodyLine> {
        let shown = self.shown();
        let mut lines = Vec::new();
        if let Some(tab) = self.tabs.get(self.active) {
            for (s, section) in tab.sections.iter().enumerate() {
                let rows: Vec<RowRef> = shown.iter().copied().filter(|r| r.0 == s).collect();
                if rows.is_empty() {
                    continue;
                }
                if !section.title.is_empty() {
                    lines.push(BodyLine::Title(s));
                }
                lines.extend(rows.into_iter().map(BodyLine::Row));
            }
        }
        if lines.is_empty() {
            lines.push(BodyLine::Note(if self.filter.is_empty() {
                "nothing to choose from — ⌃R reloads".to_string()
            } else {
                format!("no match for \"{}\" — ⌫ erases", self.filter)
            }));
        }
        lines
    }

    /// Moves the first shown line just enough to keep the cursor in view,
    /// with its section title when the cursor is on a section's first row.
    fn scroll(&self, lines: &[BodyLine], cursor_line: Option<usize>, height: usize) -> usize {
        let mut top = self.top.get().min(lines.len().saturating_sub(height));
        if let Some(c) = cursor_line {
            let titled = c > 0 && height >= 2 && matches!(lines[c - 1], BodyLine::Title(_));
            let want = if titled { c - 1 } else { c };
            if want < top {
                top = want;
            }
            if c >= top + height {
                top = c + 1 - height;
            }
        }
        self.top.set(top);
        top
    }

    /// Titles and rows exist only on a shown tab; a note needs none (a
    /// picker without tabs draws just the note).
    fn body_line(&self, line: &BodyLine, cursor: Option<RowRef>, width: usize) -> Line {
        match line {
            BodyLine::Title(s) => {
                let title = &self.tabs[self.active].sections[*s].title;
                Line::default().span("1", title.clone())
            }
            BodyLine::Note(text) => Line::default().span("2", format!("  {text}")),
            BodyLine::Row(at) => {
                let row = &self.tabs[self.active].sections[at.0].rows[at.1];
                let is_cursor = cursor == Some(*at);
                let mut line = Line::default()
                    .span("", if is_cursor { "› " } else { "  " })
                    .span("", if row.current { "● " } else { "  " })
                    .span("", row.label.clone());
                if !row.detail.is_empty() {
                    line = line.span("2", format!("  {}", row.detail));
                }
                if is_cursor {
                    line.reverse = true;
                    line = line.fit(width).pad(width);
                }
                line
            }
        }
    }
}

fn help_lines(width: usize) -> Vec<Line> {
    const GAP: &str = "   ";
    let mut lines = Vec::new();
    let mut line = Line::default();
    for (key, what) in HELP {
        let entry = text_width(key) + 2 + text_width(what);
        if line.width() > 0 && line.width() + GAP.len() + entry > width {
            lines.push(std::mem::take(&mut line));
        }
        if line.width() > 0 {
            line = line.span("", GAP);
        }
        line = line.span("1", *key).span("", format!("  {what}"));
    }
    lines.push(line);
    lines
}

// ---------------------------------------------------------------------------
// event loop
// ---------------------------------------------------------------------------

/// What one wait for input brought.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Wake {
    Data(Vec<u8>),
    /// The whole timeout passed without input.
    Timeout,
    /// A signal (SIGWINCH) cut the wait short.
    Interrupted,
    /// The terminal closed.
    Eof,
}

trait Input {
    fn wait(&mut self, timeout: Duration) -> io::Result<Wake>;
}

/// The last frame written, so an unchanged frame is not written again.
#[derive(Default)]
struct Screen {
    last: String,
    size: (usize, usize),
}

impl Screen {
    /// Draws every screen line in place (cursor address, erase line, text),
    /// blank where the view has no line; a new window size clears the
    /// screen first, since the terminal may have reflowed the old frame.
    fn draw(
        &mut self,
        out: &mut impl Write,
        view: &impl View,
        size: (usize, usize),
    ) -> io::Result<()> {
        let mut lines = view.styled_lines(size.0, size.1);
        lines.resize(size.1, String::new());
        let mut frame = String::new();
        for (i, line) in lines.iter().enumerate() {
            let _ = write!(frame, "\x1b[{};1H\x1b[2K{line}", i + 1);
        }
        let resized = size != self.size;
        if !resized && frame == self.last {
            return Ok(());
        }
        if resized {
            out.write_all(b"\x1b[H\x1b[2J")?;
        }
        out.write_all(frame.as_bytes())?;
        out.flush()?;
        self.last = frame;
        self.size = size;
        Ok(())
    }

    /// The next draw writes every line even if the frame is unchanged:
    /// output from elsewhere (a warning on stderr) may have damaged the
    /// screen or scrolled it.
    fn invalidate(&mut self) {
        self.last.clear();
    }
}

/// Runs `view` (usually a [`Picker`]) full screen on the process's terminal
/// (stdin and stdout) until it is done. Blocks the calling thread. `on`
/// serves [`Request`]s: ⌃R and idle ticks. Callers check [`is_tty`] first.
pub fn run<V: View>(view: &mut V, on: impl FnMut(&mut V, Request)) -> io::Result<V::Outcome> {
    // Whatever was printed before the view opens belongs on the normal
    // screen, not the alternate one.
    io::stdout().flush()?;
    run_on(libc::STDIN_FILENO, libc::STDOUT_FILENO, view, on)
}

fn run_on<V: View>(
    in_fd: c_int,
    out_fd: c_int,
    view: &mut V,
    on: impl FnMut(&mut V, Request),
) -> io::Result<V::Outcome> {
    let _terminal = Terminal::enter(in_fd, out_fd)?;
    event_loop(
        view,
        &mut FdInput(in_fd),
        &mut FdWriter(out_fd),
        || window_size(out_fd),
        on,
    )
}

fn event_loop<V: View>(
    view: &mut V,
    input: &mut impl Input,
    out: &mut impl Write,
    mut size: impl FnMut() -> (usize, usize),
    mut on: impl FnMut(&mut V, Request),
) -> io::Result<V::Outcome> {
    let mut decoder = Decoder::default();
    let mut screen = Screen::default();
    screen.draw(out, view, size())?;
    loop {
        let timeout = if decoder.has_pending() {
            ESC_TIMEOUT
        } else {
            TICK
        };
        let keys = match input.wait(timeout)? {
            Wake::Data(bytes) => decoder.feed(&bytes),
            Wake::Timeout if decoder.has_pending() => decoder.flush(),
            Wake::Timeout => {
                on(view, Request::Tick);
                screen.invalidate();
                Vec::new()
            }
            Wake::Interrupted => Vec::new(),
            Wake::Eof => return Ok(view.closed()),
        };
        for key in keys {
            match view.handle(key) {
                Step::Continue => {}
                Step::Reload => {
                    on(view, Request::Reload);
                    screen.invalidate();
                }
                Step::Done(outcome) => return Ok(outcome),
            }
        }
        screen.draw(out, view, size())?;
    }
}

// ---------------------------------------------------------------------------
// terminal
// ---------------------------------------------------------------------------

/// Alternate screen, hidden cursor, cleared screen.
const ENTER_SCREEN: &[u8] = b"\x1b[?1049h\x1b[?25l\x1b[H\x1b[2J";
/// Plain colors, visible cursor, normal screen.
const LEAVE_SCREEN: &[u8] = b"\x1b[0m\x1b[?25h\x1b[?1049l";

const EXIT_SIGNALS: [c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

/// The termios settings from before raw mode, for the restore paths that
/// cannot reach the `Terminal` value (signal handler, panic hook).
struct SavedTermios(UnsafeCell<MaybeUninit<libc::termios>>);

// SAFETY: written only by the `Terminal::enter` call that holds CLAIMED,
// while ACTIVE is false; read only by `restore_now` after it has switched
// ACTIVE from true to false.
unsafe impl Sync for SavedTermios {}

static SAVED: SavedTermios = SavedTermios(UnsafeCell::new(MaybeUninit::uninit()));
/// Held by the one `Terminal` value that may exist at a time.
static CLAIMED: AtomicBool = AtomicBool::new(false);
/// True while the terminal is in raw mode on the alternate screen.
static ACTIVE: AtomicBool = AtomicBool::new(false);
static IN_FD: AtomicI32 = AtomicI32::new(libc::STDIN_FILENO);
static OUT_FD: AtomicI32 = AtomicI32::new(libc::STDOUT_FILENO);
static PANIC_HOOK: Once = Once::new();

thread_local! {
    /// True on the thread that holds the `Terminal`. A panic elsewhere (a
    /// background refresh) leaves the picker running. Its message still
    /// prints onto the alternate screen, where the next idle repaint covers
    /// it, so it is not visible once the picker closes: a refresh thread
    /// should report its own failure, through the log or its result.
    static OWNER: Cell<bool> = const { Cell::new(false) };
}

/// Raw mode on the alternate screen while it lives; dropping it restores the
/// terminal and the previous signal handlers. One at a time per process.
struct Terminal {
    previous: Vec<(c_int, libc::sigaction)>,
}

impl Terminal {
    fn enter(in_fd: c_int, out_fd: c_int) -> io::Result<Terminal> {
        install_panic_hook();
        if CLAIMED.swap(true, Ordering::SeqCst) {
            return Err(io::Error::other("a terminal picker is already open"));
        }
        // From here on, an early return drops `terminal`, which restores
        // whatever was changed so far and releases CLAIMED.
        let mut terminal = Terminal {
            previous: Vec::new(),
        };
        let mut original = MaybeUninit::<libc::termios>::uninit();
        // SAFETY: tcgetattr fills the struct when it returns 0.
        if unsafe { libc::tcgetattr(in_fd, original.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: initialized by the successful tcgetattr above.
        let original = unsafe { original.assume_init() };
        // SAFETY: this call holds CLAIMED and ACTIVE is false, so nothing
        // else writes or reads SAVED now.
        unsafe { (*SAVED.0.get()).write(original) };
        IN_FD.store(in_fd, Ordering::SeqCst);
        OUT_FD.store(out_fd, Ordering::SeqCst);
        for signal in EXIT_SIGNALS {
            let previous = set_handler(signal, on_exit_signal)?;
            terminal.previous.push((signal, previous));
        }
        let previous = set_handler(libc::SIGWINCH, on_resize)?;
        terminal.previous.push((libc::SIGWINCH, previous));
        ACTIVE.store(true, Ordering::SeqCst);
        OWNER.with(|owner| owner.set(true));

        // Raw input (no echo, no line buffering, ⌃C as a byte) but cooked
        // output, so a stray newline still returns the carriage.
        let mut raw = original;
        // SAFETY: plain struct manipulation on a valid termios.
        unsafe { libc::cfmakeraw(&mut raw) };
        raw.c_oflag |= libc::OPOST;
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        // SAFETY: `raw` is a valid termios.
        if unsafe { libc::tcsetattr(in_fd, libc::TCSANOW, &raw) } != 0 {
            return Err(io::Error::last_os_error());
        }
        FdWriter(out_fd).write_all(ENTER_SCREEN)?;
        Ok(terminal)
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        restore_now();
        OWNER.with(|owner| owner.set(false));
        for (signal, action) in self.previous.drain(..).rev() {
            // SAFETY: reinstalls the action sigaction reported earlier.
            unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) };
        }
        CLAIMED.store(false, Ordering::SeqCst);
    }
}

/// Leaves the alternate screen and restores the saved termios, once: the
/// first caller (drop, signal handler or panic hook) does it. Uses only
/// async-signal-safe calls (write, tcsetattr).
fn restore_now() {
    if !ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }
    let out_fd = OUT_FD.load(Ordering::SeqCst);
    let in_fd = IN_FD.load(Ordering::SeqCst);
    // SAFETY: SAVED was written before ACTIVE became true; the writes are
    // best effort (the terminal may be gone).
    unsafe {
        libc::write(out_fd, LEAVE_SCREEN.as_ptr().cast(), LEAVE_SCREEN.len());
        libc::tcsetattr(in_fd, libc::TCSANOW, (*SAVED.0.get()).as_ptr());
    }
}

/// On a panic in the picker's thread, restores the terminal before the
/// message prints, so the message lands on the normal screen instead of
/// vanishing with the alternate one.
fn install_panic_hook() {
    PANIC_HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if OWNER.try_with(Cell::get).unwrap_or(false) {
                restore_now();
            }
            previous(info);
        }));
    });
}

/// Installs `handler` for `signal` without SA_RESTART (the wait for input
/// must see EINTR) and returns the action it replaced.
fn set_handler(signal: c_int, handler: extern "C" fn(c_int)) -> io::Result<libc::sigaction> {
    // SAFETY: zeroed sigaction structs are valid; sigaction reads `action`
    // and fills `previous`.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = handler as libc::sighandler_t;
        action.sa_flags = 0;
        libc::sigemptyset(&mut action.sa_mask);
        let mut previous: libc::sigaction = std::mem::zeroed();
        if libc::sigaction(signal, &action, &mut previous) != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(previous)
    }
}

/// SIGINT / SIGTERM / SIGHUP: restore the terminal, then die of the signal
/// as if no handler had been installed.
extern "C" fn on_exit_signal(signal: c_int) {
    restore_now();
    // SAFETY: sigemptyset, sigaction and raise are async-signal-safe. The
    // raised signal is blocked until this handler returns, then the default
    // action ends the process.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = libc::SIG_DFL;
        libc::sigemptyset(&mut action.sa_mask);
        libc::sigaction(signal, &action, std::ptr::null_mut());
        libc::raise(signal);
    }
}

/// SIGWINCH: nothing to do here. The handler exists so the signal cuts the
/// wait for input short (EINTR); the loop then reads the new size.
extern "C" fn on_resize(_: c_int) {}

/// Columns and rows of the terminal behind `fd`; 80x24 when it reports none.
fn window_size(fd: c_int) -> (usize, usize) {
    let mut size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: TIOCGWINSZ fills a winsize.
    let ok = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) } == 0;
    if ok && size.ws_col > 0 && size.ws_row > 0 {
        (size.ws_col as usize, size.ws_row as usize)
    } else {
        (80, 24)
    }
}

struct FdInput(c_int);

impl Input for FdInput {
    fn wait(&mut self, timeout: Duration) -> io::Result<Wake> {
        // select(), not poll(): macOS's poll(2) does not support devices,
        // terminals included (see its BUGS section).
        // SAFETY: fd_set and timeval are plain structs; the fd is below
        // FD_SETSIZE (it is stdin, or a test's pty).
        let ready = unsafe {
            let mut fds: libc::fd_set = std::mem::zeroed();
            libc::FD_ZERO(&mut fds);
            libc::FD_SET(self.0, &mut fds);
            let mut tv = libc::timeval {
                tv_sec: timeout.as_secs() as libc::time_t,
                tv_usec: timeout.subsec_micros() as libc::suseconds_t,
            };
            libc::select(
                self.0 + 1,
                &mut fds,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut tv,
            )
        };
        if ready < 0 {
            let err = io::Error::last_os_error();
            return match err.kind() {
                io::ErrorKind::Interrupted => Ok(Wake::Interrupted),
                _ => Err(err),
            };
        }
        if ready == 0 {
            return Ok(Wake::Timeout);
        }
        let mut buf = [0u8; 512];
        // SAFETY: reads at most buf.len() bytes into buf.
        let n = unsafe { libc::read(self.0, buf.as_mut_ptr().cast(), buf.len()) };
        if n > 0 {
            return Ok(Wake::Data(buf[..n as usize].to_vec()));
        }
        if n == 0 {
            return Ok(Wake::Eof);
        }
        let err = io::Error::last_os_error();
        match err.kind() {
            io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock => Ok(Wake::Interrupted),
            // A pty whose other end closed reports EIO.
            _ if err.raw_os_error() == Some(libc::EIO) => Ok(Wake::Eof),
            _ => Err(err),
        }
    }
}

/// Unbuffered writes to a raw fd (the same fd the restore path writes to).
struct FdWriter(c_int);

impl Write for FdWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // SAFETY: writes at most buf.len() bytes from buf.
        let n = unsafe { libc::write(self.0, buf.as_ptr().cast(), buf.len()) };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    const IPAD_AIR: &str = "AAAAAAAA-BBBB-CCCC-DDDD-000000000001";
    const IPHONE_17: &str = "AAAAAAAA-BBBB-CCCC-DDDD-000000000002";
    const IPHONE_17_PRO_MAX: &str = "AAAAAAAA-BBBB-CCCC-DDDD-000000000003";
    const IPAD_MINI: &str = "AAAAAAAA-BBBB-CCCC-DDDD-000000000004";
    const IPHONE_SE: &str = "AAAAAAAA-BBBB-CCCC-DDDD-000000000005";

    const SCHEME: usize = 0;
    const DESTINATION: usize = 1;
    const CONFIGURATION: usize = 2;

    fn scheme_tab() -> Tab {
        Tab::new(
            "Scheme",
            vec![
                Section::recent(vec![
                    Row::new("MyApp", "MyApp").current(true),
                    Row::new("Widgets", "Widgets"),
                ]),
                Section::new(
                    "Schemes",
                    vec![
                        Row::new("MyApp", "MyApp").current(true),
                        Row::new("MyApp-Staging", "MyApp-Staging"),
                        Row::new("NotificationService", "NotificationService"),
                        Row::new("Widgets", "Widgets"),
                    ],
                ),
            ],
        )
    }

    fn destination_sections() -> Vec<Section> {
        vec![
            Section::recent(vec![
                Row::new("iPad Air 11-inch (M3)", IPAD_AIR)
                    .detail("iOS 18.6 · booted")
                    .current(true),
                Row::new("iPhone 17", IPHONE_17).detail("iOS 26.1"),
            ]),
            Section::new(
                "",
                vec![Row::new("Automatic", "automatic").detail("booted iPhone, else newest")],
            ),
            Section::new(
                "iOS 26.1",
                vec![
                    Row::new("iPhone 17", IPHONE_17),
                    Row::new("iPhone 17 Pro Max", IPHONE_17_PRO_MAX),
                    Row::new("iPad mini (A17 Pro)", IPAD_MINI),
                ],
            ),
            Section::new(
                "iOS 18.6",
                vec![
                    Row::new("iPad Air 11-inch (M3)", IPAD_AIR)
                        .detail("booted")
                        .current(true),
                    Row::new("iPhone SE (3rd generation)", IPHONE_SE),
                ],
            ),
        ]
    }

    fn configuration_tab() -> Tab {
        Tab::new(
            "Configuration",
            vec![Section::new(
                "",
                vec![
                    Row::new("Scheme default (Debug)", "").current(true),
                    Row::new("Debug", "Debug"),
                    Row::new("Release", "Release"),
                ],
            )],
        )
    }

    fn picker() -> Picker {
        Picker::new(
            vec![
                HeaderPart::new("MyApp", Some(Health::Ok)),
                HeaderPart::new("iPad Air 11-inch (M3) · iOS 18.6", Some(Health::Warning)),
                HeaderPart::new("Debug (scheme default)", Some(Health::Ok)),
            ],
            vec![
                scheme_tab(),
                Tab::new("Destination", destination_sections()),
                configuration_tab(),
            ],
            DESTINATION,
        )
    }

    fn picked(tab: usize, value: &str) -> Outcome {
        Outcome::Picked {
            tab,
            value: value.to_string(),
        }
    }

    /// Scripted input: the given wakeups, then the terminal closes.
    #[derive(Default)]
    struct Script {
        wakes: VecDeque<Wake>,
        timeouts: Vec<Duration>,
    }

    impl Input for Script {
        fn wait(&mut self, timeout: Duration) -> io::Result<Wake> {
            self.timeouts.push(timeout);
            Ok(self.wakes.pop_front().unwrap_or(Wake::Eof))
        }
    }

    fn data(bytes: &[u8]) -> Wake {
        Wake::Data(bytes.to_vec())
    }

    /// Runs the event loop on scripted wakeups at 100x6 (Zed's placeholder
    /// size); returns the outcome and the requests the callback saw.
    fn play<V: View>(view: &mut V, wakes: Vec<Wake>) -> (V::Outcome, Vec<Request>) {
        let mut script = Script {
            wakes: wakes.into(),
            ..Script::default()
        };
        let mut requests = Vec::new();
        let outcome = event_loop(
            view,
            &mut script,
            &mut Vec::new(),
            || (100, 6),
            |_, request| requests.push(request),
        )
        .unwrap();
        (outcome, requests)
    }

    fn keys(bytes: &[u8]) -> Vec<Key> {
        let mut decoder = Decoder::default();
        let mut keys = decoder.feed(bytes);
        keys.extend(decoder.flush());
        keys
    }

    fn strip_sgr(s: &str) -> String {
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    // -- decoder ------------------------------------------------------------

    #[test]
    fn decoder_maps_single_bytes() {
        assert_eq!(
            keys(b"\r\n\t\x7f\x08\x12\x03a?Z "),
            vec![
                Key::Enter,
                Key::Enter,
                Key::Tab,
                Key::Backspace,
                Key::Backspace,
                Key::Reload,
                Key::Interrupt,
                Key::Char('a'),
                Key::Char('?'),
                Key::Char('Z'),
                Key::Char(' '),
            ]
        );
        // Other control bytes mean nothing.
        assert_eq!(keys(b"\x00\x01\x1a\x1f"), vec![]);
    }

    #[test]
    fn decoder_reads_csi_and_ss3_arrows() {
        assert_eq!(
            keys(b"\x1b[A\x1b[B\x1b[C\x1b[D\x1b[Z"),
            vec![Key::Up, Key::Down, Key::Right, Key::Left, Key::BackTab]
        );
        assert_eq!(
            keys(b"\x1bOA\x1bOB\x1bOC\x1bOD\x1bOM"),
            vec![Key::Up, Key::Down, Key::Right, Key::Left, Key::Enter]
        );
        // Modifier parameters do not change the key.
        assert_eq!(keys(b"\x1b[1;2A\x1b[1;5B"), vec![Key::Up, Key::Down]);
        // Unknown sequences are swallowed whole (Delete, Page Up, F5).
        assert_eq!(keys(b"\x1b[3~\x1b[5~\x1b[15~x"), vec![Key::Char('x')]);
        assert_eq!(keys(b"\x1bOPx"), vec![Key::Char('x')]);
    }

    #[test]
    fn decoder_waits_for_a_sequence_split_across_reads() {
        let mut d = Decoder::default();
        assert_eq!(d.feed(b"\x1b"), vec![]);
        assert!(d.has_pending());
        assert_eq!(d.feed(b"["), vec![]);
        assert!(d.has_pending());
        assert_eq!(d.feed(b"B"), vec![Key::Down]);
        assert!(!d.has_pending());

        assert_eq!(d.feed(b"\x1bO"), vec![]);
        assert_eq!(d.feed(b"A"), vec![Key::Up]);

        assert_eq!(d.feed(b"\x1b[1;"), vec![]);
        assert_eq!(d.feed(b"2Z"), vec![Key::BackTab]);
    }

    #[test]
    fn decoder_lone_escape_needs_the_timeout() {
        let mut d = Decoder::default();
        assert_eq!(d.feed(b"\x1b"), vec![]);
        assert_eq!(d.flush(), vec![Key::Escape]);
        assert!(!d.has_pending());
        // An unfinished introducer at the timeout: ⎋, then the byte itself.
        assert_eq!(d.feed(b"\x1b["), vec![]);
        assert_eq!(d.flush(), vec![Key::Escape, Key::Char('[')]);
        // Nothing pending: nothing to flush.
        assert_eq!(d.flush(), vec![]);
    }

    #[test]
    fn decoder_escape_before_other_bytes_is_escape() {
        assert_eq!(keys(b"\x1bx"), vec![Key::Escape, Key::Char('x')]);
        assert_eq!(keys(b"\x1b\x1b"), vec![Key::Escape, Key::Escape]);
        assert_eq!(keys(b"\x1b\x1b[A"), vec![Key::Escape, Key::Up]);
        // A broken CSI is dropped up to the byte that broke it.
        assert_eq!(keys(b"\x1b[1\x1b[B"), vec![Key::Down]);
        // An endless one is dropped once it is too long to be a key.
        let mut long = b"\x1b[".to_vec();
        long.extend(std::iter::repeat_n(b'1', MAX_CSI));
        let mut d = Decoder::default();
        assert_eq!(d.feed(&long), vec![]);
        assert!(!d.has_pending());
    }

    #[test]
    fn decoder_reads_utf8_also_split() {
        assert_eq!(keys("é✓".as_bytes()), vec![Key::Char('é'), Key::Char('✓')]);
        let mut d = Decoder::default();
        let bytes = "é".as_bytes();
        assert_eq!(d.feed(&bytes[..1]), vec![]);
        assert_eq!(d.feed(&bytes[1..]), vec![Key::Char('é')]);
        // Invalid bytes are skipped, the rest still decodes.
        assert_eq!(keys(b"\xff\xc3a"), vec![Key::Char('a')]);
        // An unfinished character at the timeout is dropped.
        let mut d = Decoder::default();
        assert_eq!(d.feed(&"✓".as_bytes()[..2]), vec![]);
        assert_eq!(d.flush(), vec![]);
    }

    // -- state machine on scripted byte streams -----------------------------

    #[test]
    fn cursor_starts_on_current_and_enter_keeps_it() {
        let mut p = picker();
        assert_eq!(p.selected().unwrap().value, IPAD_AIR);
        assert_eq!(
            play(&mut p, vec![data(b"\r")]).0,
            picked(DESTINATION, IPAD_AIR)
        );
        // The other tabs start on their current rows too.
        let p = Picker::new(vec![], vec![configuration_tab()], 0);
        assert_eq!(p.selected().unwrap().label, "Scheme default (Debug)");
        // No current row: the first row.
        let tab = Tab::new("Scheme", vec![Section::new("", vec![Row::new("A", "a")])]);
        assert_eq!(
            Picker::new(vec![], vec![tab], 0).selected().unwrap().value,
            "a"
        );
    }

    #[test]
    fn down_enter_picks_the_previous_value() {
        let mut p = picker();
        let (outcome, _) = play(&mut p, vec![data(b"\x1b[B\r")]);
        assert_eq!(outcome, picked(DESTINATION, IPHONE_17));
        // Application cursor mode arrows work the same.
        let (outcome, _) = play(&mut picker(), vec![data(b"\x1bOB\x1bOB\x1bOA\r")]);
        assert_eq!(outcome, picked(DESTINATION, IPHONE_17));
    }

    #[test]
    fn cursor_stops_at_both_ends() {
        let (outcome, _) = play(&mut picker(), vec![data(b"\x1b[A\x1b[A\r")]);
        assert_eq!(outcome, picked(DESTINATION, IPAD_AIR));
        let mut p = picker();
        let downs = b"\x1b[B".repeat(20);
        let (outcome, _) = play(&mut p, vec![data(&downs), data(b"\r")]);
        assert_eq!(outcome, picked(DESTINATION, IPHONE_SE));
    }

    #[test]
    fn lone_escape_cancels_after_the_timeout() {
        let mut script = Script {
            wakes: vec![data(b"\x1b"), Wake::Timeout].into(),
            ..Script::default()
        };
        let outcome = event_loop(
            &mut picker(),
            &mut script,
            &mut Vec::new(),
            || (100, 6),
            |_, _| {},
        )
        .unwrap();
        assert_eq!(outcome, Outcome::Cancelled);
        // The wait after the ESC is the short one.
        assert_eq!(script.timeouts, vec![TICK, ESC_TIMEOUT]);
    }

    #[test]
    fn escape_split_from_its_sequence_is_still_an_arrow() {
        let wakes = vec![data(b"\x1b"), data(b"[B"), data(b"\r")];
        assert_eq!(play(&mut picker(), wakes).0, picked(DESTINATION, IPHONE_17));
        let wakes = vec![data(b"\x1b["), data(b"B"), data(b"\r")];
        assert_eq!(play(&mut picker(), wakes).0, picked(DESTINATION, IPHONE_17));
    }

    #[test]
    fn interrupt_and_closed_terminal_cancel() {
        assert_eq!(
            play(&mut picker(), vec![data(b"\x03")]).0,
            Outcome::Cancelled
        );
        assert_eq!(play(&mut picker(), vec![]).0, Outcome::Cancelled);
    }

    #[test]
    fn typing_filters_by_subsequence_and_enter_takes_the_top_match() {
        let mut p = picker();
        for key in keys(b"17pm") {
            assert_eq!(p.handle(key), Step::Continue);
        }
        assert_eq!(p.filter(), "17pm");
        let frame = p.render(80, 24);
        let body: Vec<&str> = frame.lines().skip(2).filter(|l| !l.is_empty()).collect();
        assert_eq!(
            body,
            [
                "iOS 26.1",
                "›   iPhone 17 Pro Max",
                "filter: 17pm · ⌫ erase · ↵ choose · ⎋ cancel",
            ]
        );
        assert_eq!(
            p.handle(Key::Enter),
            Step::Done(picked(DESTINATION, IPHONE_17_PRO_MAX))
        );

        // Case-insensitive; matches in Recent come first.
        let (outcome, _) = play(&mut picker(), vec![data(b"IPHONE"), data(b"\r")]);
        assert_eq!(outcome, picked(DESTINATION, IPHONE_17));
    }

    #[test]
    fn backspace_widens_the_filter_again() {
        let mut p = picker();
        for key in keys(b"sex\x7f") {
            p.handle(key);
        }
        assert_eq!(p.filter(), "se");
        assert_eq!(p.selected().unwrap().value, IPHONE_SE);
        // A filter without matches: ↵ does nothing, ⌫ brings rows back.
        let (outcome, _) = play(&mut picker(), vec![data(b"zzz\r\x7f\x7f\x7f\r")]);
        assert_eq!(outcome, picked(DESTINATION, IPAD_AIR));
        let mut p = picker();
        for key in keys(b"zzz") {
            p.handle(key);
        }
        assert!(p.render(80, 24).contains("no match for \"zzz\""));
        assert_eq!(p.handle(Key::Enter), Step::Continue);
    }

    #[test]
    fn tabs_switch_both_ways_and_wrap() {
        let mut p = picker();
        p.handle(Key::Tab);
        assert_eq!(p.active_tab(), CONFIGURATION);
        p.handle(Key::Tab);
        assert_eq!(p.active_tab(), SCHEME);
        p.handle(Key::BackTab);
        assert_eq!(p.active_tab(), CONFIGURATION);
        p.handle(Key::Left);
        assert_eq!(p.active_tab(), DESTINATION);
        p.handle(Key::Right);
        assert_eq!(p.active_tab(), CONFIGURATION);

        // Through the decoder: ⇧⇥ twice from Destination lands on Configuration.
        let (outcome, _) = play(&mut picker(), vec![data(b"\x1b[Z\x1b[Z\x1b[B\r")]);
        assert_eq!(outcome, picked(CONFIGURATION, "Debug"));
        // ⇥ to Configuration, ↵ keeps the scheme default (empty value).
        let (outcome, _) = play(&mut picker(), vec![data(b"\t\r")]);
        assert_eq!(outcome, picked(CONFIGURATION, ""));
        // ← to Scheme.
        let (outcome, _) = play(&mut picker(), vec![data(b"\x1b[D\x1b[B\r")]);
        assert_eq!(outcome, picked(SCHEME, "Widgets"));
    }

    #[test]
    fn each_tab_keeps_its_cursor_and_switching_clears_the_filter() {
        let mut p = picker();
        p.handle(Key::Down);
        p.handle(Key::Char('x'));
        p.handle(Key::Tab);
        assert_eq!(p.filter(), "");
        p.handle(Key::BackTab);
        assert_eq!(p.active_tab(), DESTINATION);
        // The filter "x" had moved the cursor to its first match.
        assert_eq!(p.selected().unwrap().value, IPHONE_17_PRO_MAX);
        // A single tab does not switch.
        let mut single = Picker::new(vec![], vec![configuration_tab()], 0);
        single.handle(Key::Tab);
        assert_eq!(single.active_tab(), 0);
    }

    #[test]
    fn ctrl_r_asks_for_a_reload_and_the_cursor_survives_it() {
        let mut p = picker();
        p.handle(Key::Down); // iPhone 17
        let mut script = Script {
            wakes: vec![data(b"\x12"), data(b"\r")].into(),
            ..Script::default()
        };
        let mut reloads = 0;
        let outcome = event_loop(
            &mut p,
            &mut script,
            &mut Vec::new(),
            || (100, 6),
            |p, request| {
                if request == Request::Reload {
                    reloads += 1;
                    // The refreshed list lost a device and gained one.
                    let mut sections = destination_sections();
                    sections[3].rows.remove(1);
                    sections[2].rows.insert(
                        0,
                        Row::new("iPhone Air", "AAAAAAAA-BBBB-CCCC-DDDD-000000000006"),
                    );
                    p.replace_sections(DESTINATION, sections);
                }
            },
        )
        .unwrap();
        assert_eq!(reloads, 1);
        assert_eq!(outcome, picked(DESTINATION, IPHONE_17));
    }

    #[test]
    fn replace_sections_falls_back_to_current_then_first() {
        let mut p = picker();
        p.handle(Key::Down); // iPhone 17
        let without = vec![Section::new(
            "",
            vec![Row::new("A", "a"), Row::new("B", "b").current(true)],
        )];
        p.replace_sections(DESTINATION, without);
        assert_eq!(p.selected().unwrap().value, "b");
        p.replace_sections(
            DESTINATION,
            vec![Section::new("", vec![Row::new("C", "c")])],
        );
        assert_eq!(p.selected().unwrap().value, "c");
        p.replace_sections(DESTINATION, vec![]);
        assert_eq!(p.selected(), None);
        assert_eq!(p.handle(Key::Enter), Step::Continue);
        assert!(p.render(80, 24).contains("nothing to choose from"));
        // An unknown tab is ignored.
        p.replace_sections(9, vec![]);
    }

    #[test]
    fn replace_sections_keeps_the_cursor_in_its_section() {
        // "iPhone 17" under iOS 26.1; Recent lists the same device.
        let mut p = picker();
        for _ in 0..3 {
            p.handle(Key::Down);
        }
        assert!(p.render(80, 24).contains("›   iPhone 17\n"));
        // An unchanged refresh leaves the cursor where it was, not on the
        // Recent row with the same value, so ↓ goes on down the list.
        p.replace_sections(DESTINATION, destination_sections());
        assert!(p.render(80, 24).contains("›   iPhone 17\n"));
        assert_eq!(p.cursors[DESTINATION], Some((2, 0)));
        p.handle(Key::Down);
        assert_eq!(p.selected().unwrap().value, IPHONE_17_PRO_MAX);
        p.handle(Key::Up);
        // A new runtime above it: the cursor follows its section's title.
        let mut sections = destination_sections();
        sections.insert(
            2,
            Section::new(
                "iOS 26.2",
                vec![Row::new(
                    "iPhone Air",
                    "AAAAAAAA-BBBB-CCCC-DDDD-000000000006",
                )],
            ),
        );
        p.replace_sections(DESTINATION, sections);
        assert_eq!(p.cursors[DESTINATION], Some((3, 0)));
        // On a Recent row, it stays in Recent.
        let mut p = picker();
        p.handle(Key::Down);
        p.replace_sections(DESTINATION, destination_sections());
        assert_eq!(p.cursors[DESTINATION], Some((0, 1)));
        // The section is gone: the value wherever it is listed.
        let mut sections = destination_sections();
        sections.remove(0);
        p.replace_sections(DESTINATION, sections);
        assert_eq!(p.cursors[DESTINATION], Some((1, 0)));
        assert_eq!(p.selected().unwrap().value, IPHONE_17);
    }

    #[test]
    fn keywords_are_filtered_but_not_drawn() {
        let section = |os: &str, udid: &str| {
            Section::new(
                format!("iOS {os}"),
                vec![Row::new("iPhone 17", udid).keywords(format!("iOS {os}"))],
            )
        };
        let tab = Tab::new(
            "Destination",
            vec![
                section("26.1", IPHONE_17),
                section("18.6", IPHONE_SE),
                Section::new("", vec![Row::new("Automatic", "automatic")]),
            ],
        );
        let mut p = Picker::new(vec![], vec![tab], 0);
        for key in keys(b"17 18") {
            p.handle(key);
        }
        assert_eq!(p.selected().unwrap().value, IPHONE_SE);
        let frame = p.render(60, 6);
        let body: Vec<&str> = frame.lines().skip(2).take(3).collect();
        assert_eq!(body, ["iOS 18.6", "›   iPhone 17", ""]);
        // The detail is not searched.
        let tab = Tab::new(
            "Scheme",
            vec![Section::new(
                "",
                vec![Row::new("MyApp", "MyApp").detail("Release")],
            )],
        );
        let mut p = Picker::new(vec![], vec![tab], 0);
        p.handle(Key::Char('r'));
        assert_eq!(p.selected(), None);
    }

    #[test]
    fn idle_ticks_reach_the_callback() {
        let (outcome, requests) = play(
            &mut picker(),
            vec![Wake::Timeout, Wake::Interrupted, Wake::Timeout],
        );
        assert_eq!(outcome, Outcome::Cancelled);
        assert_eq!(requests, vec![Request::Tick, Request::Tick]);
    }

    #[test]
    fn callbacks_repaint_the_whole_frame() {
        // Stray stderr output may have damaged the screen meanwhile, so an
        // unchanged frame is written again after a tick or a reload, but
        // not after a key that changed nothing.
        let mut script = Script {
            wakes: vec![Wake::Timeout, data(b"\x12"), data(b"\x1b[A")].into(),
            ..Script::default()
        };
        let mut out = Vec::new();
        event_loop(&mut picker(), &mut script, &mut out, || (100, 6), |_, _| {}).unwrap();
        let out = String::from_utf8(out).unwrap();
        assert_eq!(
            out.matches("\x1b[1;1H").count(),
            3,
            "first draw, tick, reload"
        );
        assert_eq!(out.matches("\x1b[2J").count(), 1);
    }

    /// A screen with keys of its own, as the status hub will have.
    struct Hub {
        drawn: Cell<usize>,
    }

    impl View for Hub {
        type Outcome = Option<char>;

        fn handle(&mut self, key: Key) -> Step<Option<char>> {
            match key {
                Key::Char(c @ ('s' | 'd' | 'c' | 'q')) => Step::Done(Some(c)),
                Key::Escape | Key::Enter => Step::Done(None),
                Key::Reload => Step::Reload,
                _ => Step::Continue,
            }
        }

        fn styled_lines(&self, _: usize, _: usize) -> Vec<String> {
            self.drawn.set(self.drawn.get() + 1);
            vec!["MyApp".to_string()]
        }

        fn closed(&self) -> Option<char> {
            None
        }
    }

    #[test]
    fn event_loop_drives_any_view() {
        let mut hub = Hub {
            drawn: Cell::new(0),
        };
        let mut script = Script {
            wakes: vec![data(b"x\x12"), data(b"d")].into(),
            ..Script::default()
        };
        let mut out = Vec::new();
        let mut reloads = 0;
        let outcome = event_loop(
            &mut hub,
            &mut script,
            &mut out,
            || (40, 3),
            |_, request| {
                assert_eq!(request, Request::Reload);
                reloads += 1;
            },
        )
        .unwrap();
        assert_eq!((outcome, reloads, hub.drawn.get()), (Some('d'), 1, 2));
        // The lines the view leaves out are drawn blank.
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("\x1b[1;1H\x1b[2KMyApp\x1b[2;1H\x1b[2K\x1b[3;1H\x1b[2K"));
        let mut hub = Hub {
            drawn: Cell::new(0),
        };
        assert_eq!(play(&mut hub, vec![]).0, None, "the terminal closed");
    }

    #[test]
    fn question_mark_shows_the_keys_and_any_key_closes_them() {
        let mut p = picker();
        p.handle(Key::Char('?'));
        assert_eq!(p.filter(), "");
        let frame = p.render(100, 6);
        assert!(frame.contains("switch tab"), "{frame}");
        assert!(frame.contains("any key closes this list"), "{frame}");
        // ⎋ closes the list instead of cancelling the picker.
        assert_eq!(p.handle(Key::Escape), Step::Continue);
        assert!(!p.render(100, 6).contains("any key closes"));
        assert_eq!(p.handle(Key::Escape), Step::Done(Outcome::Cancelled));
    }

    // -- rendering ----------------------------------------------------------

    #[test]
    fn snapshot_100x6_placeholder() {
        let expected = "\
✓ MyApp ▸ ! iPad Air 11-inch (M3) · iOS 18.6 ▸ ✓ Debug (scheme default)
 Scheme  [Destination]  Configuration                                                            1/8
Recent
› ● iPad Air 11-inch (M3)  iOS 18.6 · booted
    iPhone 17  iOS 26.1
↑↓ move · ↵ choose · ⎋ cancel · ⇥ tab · ⌃R reload · ? keys · type to filter";
        let frame = picker().render(100, 6);
        assert_eq!(frame, expected);
        // The position count sits at the right edge.
        assert_eq!(frame.lines().nth(1).unwrap().chars().count(), 100);
    }

    #[test]
    fn snapshot_80x24() {
        let mut p = picker();
        p.handle(Key::Down);
        p.handle(Key::Down);
        let expected = "\
✓ MyApp ▸ ! iPad Air 11-inch (M3) · iOS 18.6 ▸ ✓ Debug (scheme default)
 Scheme  [Destination]  Configuration                                        3/8
Recent
  ● iPad Air 11-inch (M3)  iOS 18.6 · booted
    iPhone 17  iOS 26.1
›   Automatic  booted iPhone, else newest
iOS 26.1
    iPhone 17
    iPhone 17 Pro Max
    iPad mini (A17 Pro)
iOS 18.6
  ● iPad Air 11-inch (M3)  booted
    iPhone SE (3rd generation)










↑↓ move · ↵ choose · ⎋ cancel · ⇥ tab · ⌃R reload · ? keys · type to filter";
        assert_eq!(p.render(80, 24), expected);
    }

    #[test]
    fn snapshot_80x24_filtered() {
        let mut p = picker();
        for key in keys(b"ipad") {
            p.handle(key);
        }
        let frame = p.render(80, 24);
        let lines: Vec<&str> = frame.lines().collect();
        assert_eq!(
            lines[1..7],
            [
                " Scheme  [Destination]  Configuration                                        1/3",
                "Recent",
                "› ● iPad Air 11-inch (M3)  iOS 18.6 · booted",
                "iOS 26.1",
                "    iPad mini (A17 Pro)",
                "iOS 18.6",
            ]
        );
        assert_eq!(lines[7], "  ● iPad Air 11-inch (M3)  booted");
        assert_eq!(lines[23], "filter: ipad · ⌫ erase · ↵ choose · ⎋ cancel");
    }

    #[test]
    fn scrolling_keeps_the_cursor_and_its_section_title_in_view() {
        let mut p = picker();
        let frames: Vec<String> = (0..8)
            .map(|_| {
                let frame = p.render(100, 6);
                p.handle(Key::Down);
                frame
            })
            .collect();
        for frame in &frames {
            assert_eq!(
                frame.lines().filter(|l| l.starts_with('›')).count(),
                1,
                "{frame}"
            );
        }
        let body = |frame: &str| {
            frame
                .lines()
                .skip(2)
                .take(3)
                .map(String::from)
                .collect::<Vec<_>>()
        };
        // Moving down onto the first row of "iOS 26.1" brings its title along.
        assert_eq!(
            body(&frames[3]),
            [
                "    Automatic  booted iPhone, else newest",
                "iOS 26.1",
                "›   iPhone 17",
            ]
        );
        assert_eq!(
            body(&frames[7]),
            [
                "iOS 18.6",
                "  ● iPad Air 11-inch (M3)  booted",
                "›   iPhone SE (3rd generation)",
            ]
        );
        // Moving up scrolls just enough to show the cursor ...
        for _ in 0..3 {
            p.handle(Key::Up);
        }
        assert_eq!(
            body(&p.render(100, 6)),
            [
                "›   iPhone 17 Pro Max",
                "    iPad mini (A17 Pro)",
                "iOS 18.6",
            ]
        );
        // ... and onto a section's first row, its title as well.
        p.handle(Key::Up);
        assert_eq!(
            body(&p.render(100, 6)),
            ["iOS 26.1", "›   iPhone 17", "    iPhone 17 Pro Max"]
        );
        // Back at the top, the frame is the starting one.
        for _ in 0..8 {
            p.handle(Key::Up);
        }
        assert_eq!(p.render(100, 6), picker().render(100, 6));
    }

    #[test]
    fn every_size_fits_the_screen() {
        let mut p = picker();
        p.handle(Key::Down);
        for width in [0, 1, 2, 5, 20, 40, 100, 300] {
            for height in [0, 1, 2, 3, 4, 6, 24] {
                let frame = p.render(width, height);
                if width == 0 || height == 0 {
                    assert_eq!(frame, "");
                    continue;
                }
                let lines: Vec<&str> = frame.split('\n').collect();
                assert_eq!(lines.len(), height, "{width}x{height}:\n{frame}");
                for line in &lines {
                    assert!(line.chars().count() <= width, "{width}x{height}: {line:?}");
                }
                // The cursor row is always on screen.
                assert!(
                    lines.iter().any(|l| l.starts_with('›')) || width < 2,
                    "{width}x{height}:\n{frame}"
                );
                for line in p.styled_lines(width, height) {
                    assert!(strip_sgr(&line).chars().count() <= width);
                }
            }
        }
        // Narrow: cut with an ellipsis; the position count drops out.
        let narrow = p.render(30, 6);
        assert_eq!(
            narrow.lines().next().unwrap(),
            "✓ MyApp ▸ ! iPad Air 11-inch…"
        );
        assert_eq!(
            narrow.lines().nth(1).unwrap(),
            " Scheme  [Destination]  Confi…"
        );
        // One line: the cursor row.
        assert_eq!(p.render(40, 1), "›   iPhone 17  iOS 26.1");
        // A picker without tabs draws its note at any size.
        let empty = Picker::new(vec![], vec![], 0);
        for height in [1, 3, 6] {
            assert!(empty.render(40, height).contains("nothing to choose from"));
        }
    }

    #[test]
    fn wide_characters_take_two_columns() {
        assert_eq!(text_width("MyApp ✓ ● ▸ › … ⎋ ⌫ ⇥ ⌃ ↵"), 25);
        assert_eq!(text_width("日本語のアプリ"), 14);
        assert_eq!(text_width("Ｗ🚀😀"), 6);
        assert_eq!(text_width("e\u{301}\u{200b}"), 1);
        let tab = Tab::new(
            "Scheme",
            vec![Section::new(
                "",
                vec![Row::new("日本語のアプリ", "a"), Row::new("MyApp", "b")],
            )],
        );
        let p = Picker::new(vec![HeaderPart::new("日本語のアプリ", None)], vec![tab], 0);
        for width in 1..30 {
            for line in p.styled_lines(width, 4) {
                assert!(text_width(&strip_sgr(&line)) <= width, "{width}: {line:?}");
            }
        }
        // The cursor row spans exactly the width, cut between characters.
        let lines = p.styled_lines(13, 4);
        assert_eq!(strip_sgr(&lines[2]), "›   日本語の…");
        assert_eq!(text_width(&strip_sgr(&lines[2])), 13);
        assert_eq!(strip_sgr(&p.styled_lines(12, 4)[2]), "›   日本語… ");
    }

    #[test]
    fn control_characters_in_names_cannot_reach_the_terminal() {
        let tab = Tab::new(
            "Scheme",
            vec![Section::new("", vec![Row::new("My\x1b[2JApp\tB", "x")])],
        );
        let p = Picker::new(vec![HeaderPart::new("a\rb", None)], vec![tab], 0);
        let gap = " ".repeat(40 - "[Scheme]1/1".len());
        assert_eq!(
            p.render(40, 3),
            format!("a b\n[Scheme]{gap}1/1\n›   My [2JApp B")
        );
        for line in p.styled_lines(40, 3) {
            assert!(!line.contains("\x1b[2J") && !line.contains('\r') && !line.contains('\t'));
        }
    }

    #[test]
    fn styled_lines_carry_the_same_text_plus_styling() {
        let p = picker();
        let plain: Vec<String> = p.render(100, 6).split('\n').map(String::from).collect();
        let styled = p.styled_lines(100, 6);
        assert_eq!(styled.len(), plain.len());
        for (s, plain) in styled.iter().zip(&plain) {
            assert_eq!(strip_sgr(s).trim_end(), plain);
        }
        // Glyph colors, bold active tab, reverse-video cursor row spanning the width.
        assert!(styled[0].starts_with("\x1b[0;32m✓"));
        assert!(styled[0].contains("\x1b[0;33m!"));
        assert!(styled[1].contains("\x1b[0;1m[Destination]"));
        assert!(styled[3].starts_with("\x1b[0;7m› "));
        assert_eq!(strip_sgr(&styled[3]).chars().count(), 100);
        // ✗ in red.
        let mut p = picker();
        p.set_header(vec![
            HeaderPart::new("MyApp", Some(Health::Error)),
            HeaderPart::new("iPhone 17 · iOS 26.1", None),
        ]);
        assert!(p
            .render(100, 6)
            .starts_with("✗ MyApp ▸ iPhone 17 · iOS 26.1\n"));
        assert!(p.styled_lines(100, 6)[0].starts_with("\x1b[0;31m✗"));
    }

    #[test]
    fn screen_redraws_only_changes_and_clears_on_resize() {
        let p = picker();
        let mut screen = Screen::default();
        let mut out = Vec::new();
        screen.draw(&mut out, &p, (100, 6)).unwrap();
        let first = String::from_utf8(out.clone()).unwrap();
        assert!(first.starts_with("\x1b[H\x1b[2J\x1b[1;1H\x1b[2K"));
        assert!(first.contains("\x1b[6;1H\x1b[2K"));
        assert!(!first.contains("\x1b[7;1H"));
        out.clear();
        screen.draw(&mut out, &p, (100, 6)).unwrap();
        assert!(out.is_empty(), "an unchanged frame is not written again");
        // Zed resizes the placeholder to the pane.
        screen.draw(&mut out, &p, (80, 24)).unwrap();
        let resized = String::from_utf8(out).unwrap();
        assert!(resized.starts_with("\x1b[H\x1b[2J"));
        assert!(resized.contains("\x1b[24;1H\x1b[2K"));
    }

    #[test]
    fn event_loop_redraws_after_a_resize() {
        let mut sizes = vec![(80, 24), (100, 6), (100, 6)];
        let mut script = Script {
            wakes: vec![Wake::Interrupted, Wake::Interrupted].into(),
            ..Script::default()
        };
        let mut out = Vec::new();
        let outcome = event_loop(
            &mut picker(),
            &mut script,
            &mut out,
            || sizes.pop().unwrap_or((80, 24)),
            |_, _| {},
        )
        .unwrap();
        assert_eq!(outcome, Outcome::Cancelled);
        let out = String::from_utf8(out).unwrap();
        // Drawn at 100x6, once more on the SIGWINCH wake with the new size.
        assert_eq!(out.matches("\x1b[2J").count(), 2);
        assert!(out.contains("\x1b[24;1H"));
    }

    #[test]
    fn fuzzy_match_is_an_ordered_case_insensitive_subsequence() {
        assert!(fuzzy_match("", "anything"));
        assert!(fuzzy_match("ip17", "iPhone 17"));
        assert!(fuzzy_match("17 pro", "iPhone 17 Pro Max"));
        assert!(fuzzy_match("MYAPP", "myapp-staging"));
        assert!(!fuzzy_match("pro 17", "iPhone 17 Pro Max"));
        assert!(!fuzzy_match("ipadx", "iPad mini"));
    }

    #[test]
    fn recent_keeps_five_rows() {
        let rows = (0..8)
            .map(|i| Row::new(format!("S{i}"), format!("s{i}")))
            .collect();
        let recent = Section::recent(rows);
        assert_eq!(recent.title, "Recent");
        assert_eq!(recent.rows.len(), MAX_RECENT);
        assert_eq!(recent.rows[0].value, "s0");
    }

    #[test]
    fn no_tty_means_the_numbered_fallback() {
        assert!(tty_usable(true, true, Some(OsStr::new("xterm-256color"))));
        assert!(tty_usable(true, true, None));
        assert!(!tty_usable(false, true, None), "piped stdin");
        assert!(!tty_usable(true, false, None), "redirected stdout");
        assert!(!tty_usable(true, true, Some(OsStr::new("dumb"))));
    }

    // -- a real pty -----------------------------------------------------------

    use std::ffi::CStr;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::time::Instant;

    /// Raw mode and the restore paths are process-wide: one pty test at a time.
    static PTY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct Pty {
        master: OwnedFd,
        slave: OwnedFd,
        /// The slave's device path, for a child process to open.
        path: std::path::PathBuf,
    }

    fn open_pty() -> Pty {
        // SAFETY: standard pty setup; every fd is checked before use and
        // owned afterwards.
        unsafe {
            let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
            assert!(master >= 0, "posix_openpt: {}", io::Error::last_os_error());
            let master = OwnedFd::from_raw_fd(master);
            assert_eq!(libc::grantpt(master.as_raw_fd()), 0);
            assert_eq!(libc::unlockpt(master.as_raw_fd()), 0);
            let name = libc::ptsname(master.as_raw_fd());
            assert!(!name.is_null());
            let name = CStr::from_ptr(name).to_owned();
            let slave = libc::open(name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY);
            assert!(slave >= 0, "open {name:?}: {}", io::Error::last_os_error());
            Pty {
                master,
                slave: OwnedFd::from_raw_fd(slave),
                path: OsStr::from_bytes(name.to_bytes()).into(),
            }
        }
    }

    fn set_size(master: c_int, cols: u16, rows: u16) {
        let size = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: TIOCSWINSZ reads a winsize.
        assert_eq!(unsafe { libc::ioctl(master, libc::TIOCSWINSZ, &size) }, 0);
    }

    type Flags = (
        libc::tcflag_t,
        libc::tcflag_t,
        libc::tcflag_t,
        libc::tcflag_t,
        [libc::cc_t; libc::NCCS],
    );

    fn flags(fd: c_int) -> Flags {
        let mut t = MaybeUninit::<libc::termios>::uninit();
        // SAFETY: tcgetattr fills the struct when it returns 0.
        let t = unsafe {
            assert_eq!(libc::tcgetattr(fd, t.as_mut_ptr()), 0);
            t.assume_init()
        };
        (t.c_iflag, t.c_oflag, t.c_cflag, t.c_lflag, t.c_cc)
    }

    fn handler_of(signal: c_int) -> libc::sighandler_t {
        // SAFETY: a null new action only reads the current one.
        unsafe {
            let mut current: libc::sigaction = std::mem::zeroed();
            assert_eq!(libc::sigaction(signal, std::ptr::null(), &mut current), 0);
            current.sa_sigaction
        }
    }

    /// Everything the master side receives until it stays quiet for `quiet`.
    fn read_output(master: c_int, quiet: Duration) -> Vec<u8> {
        let mut out = Vec::new();
        let mut input = FdInput(master);
        loop {
            match input.wait(quiet).unwrap() {
                Wake::Data(bytes) => out.extend(bytes),
                Wake::Interrupted => {}
                Wake::Timeout | Wake::Eof => return out,
            }
        }
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    fn send(master: c_int, bytes: &[u8]) {
        FdWriter(master).write_all(bytes).unwrap();
    }

    /// Runs the picker on the pty's slave side in a thread, as `run` does on
    /// stdin/stdout, and returns its outcome plus everything it drew.
    fn run_on_pty(script: impl FnOnce(c_int)) -> (Outcome, Vec<u8>) {
        let pty = open_pty();
        let master = pty.master.as_raw_fd();
        set_size(master, 100, 6);
        let slave = pty.slave;
        let worker = std::thread::spawn(move || {
            let fd = slave.as_raw_fd();
            run_on(fd, fd, &mut picker(), |_, _| {})
        });
        let mut drawn = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !contains(&drawn, b"[Destination]") {
            assert!(Instant::now() < deadline, "no frame drawn");
            drawn.extend(read_output(master, Duration::from_millis(20)));
        }
        script(master);
        while !worker.is_finished() {
            assert!(Instant::now() < deadline, "the picker did not finish");
            drawn.extend(read_output(master, Duration::from_millis(20)));
        }
        let outcome = worker.join().unwrap().unwrap();
        drawn.extend(read_output(master, Duration::from_millis(20)));
        (outcome, drawn)
    }

    #[test]
    fn pty_raw_mode_and_screen_are_restored_once() {
        let _lock = PTY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let pty = open_pty();
        let (master, slave) = (pty.master.as_raw_fd(), pty.slave.as_raw_fd());
        let quiet = Duration::from_millis(50);

        set_size(master, 100, 6);
        assert_eq!(window_size(slave), (100, 6));
        set_size(master, 120, 40);
        assert_eq!(window_size(slave), (120, 40));

        let before = flags(slave);
        let handlers_before = (handler_of(libc::SIGTERM), handler_of(libc::SIGWINCH));
        let terminal = Terminal::enter(slave, slave).unwrap();
        let (_, oflag, _, lflag, _) = flags(slave);
        assert_eq!(lflag & (libc::ICANON | libc::ECHO | libc::ISIG), 0);
        assert_ne!(oflag & libc::OPOST, 0);
        assert_ne!(handler_of(libc::SIGTERM), handlers_before.0);
        assert!(
            Terminal::enter(slave, slave).is_err(),
            "one picker at a time"
        );
        assert!(contains(&read_output(master, quiet), ENTER_SCREEN));

        // The path the signal handler and the panic hook take.
        restore_now();
        assert_eq!(flags(slave), before);
        assert!(contains(&read_output(master, quiet), LEAVE_SCREEN));
        // Dropping afterwards does not restore a second time.
        drop(terminal);
        assert!(read_output(master, quiet).is_empty());
        assert_eq!(
            (handler_of(libc::SIGTERM), handler_of(libc::SIGWINCH)),
            handlers_before
        );

        // A plain drop restores as well.
        drop(Terminal::enter(slave, slave).unwrap());
        assert_eq!(flags(slave), before);
        let out = read_output(master, quiet);
        assert!(contains(&out, ENTER_SCREEN) && contains(&out, LEAVE_SCREEN));
    }

    #[test]
    fn pty_panic_on_the_picker_thread_restores_before_unwinding() {
        let _lock = PTY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let pty = open_pty();
        let (master, slave) = (pty.master.as_raw_fd(), pty.slave.as_raw_fd());
        let quiet = Duration::from_millis(50);
        let before = flags(slave);
        let terminal = Terminal::enter(slave, slave).unwrap();
        assert!(contains(&read_output(master, quiet), ENTER_SCREEN));

        // A panic on another thread (a background refresh) leaves raw mode
        // and the alternate screen alone.
        let refresh = std::thread::spawn(|| panic!("refresh failed (expected by this test)"));
        assert!(refresh.join().is_err());
        assert!(ACTIVE.load(Ordering::SeqCst));
        assert_ne!(flags(slave), before);
        assert!(read_output(master, quiet).is_empty());

        // On the thread that holds the terminal, the panic hook restores it
        // before anything unwinds: `terminal` has not been dropped yet.
        let caught = std::panic::catch_unwind(|| panic!("picker failed (expected by this test)"));
        assert!(caught.is_err());
        assert!(!ACTIVE.load(Ordering::SeqCst));
        assert_eq!(flags(slave), before);
        assert!(contains(&read_output(master, quiet), LEAVE_SCREEN));
        drop(terminal);
        assert!(read_output(master, quiet).is_empty());
    }

    /// Names the pty for the child half of the exit-signal test.
    const SIGNAL_CHILD_PTY: &str = "XCODE_DAP_TUI_TEST_SIGNAL_CHILD_PTY";

    /// The child half of `pty_exit_signal_restores_then_ends_the_process`,
    /// run in its own process: raw mode on the named pty, then a wait for
    /// the parent's SIGTERM. Does nothing in a normal test run.
    #[test]
    fn pty_signal_child() {
        use std::os::unix::fs::OpenOptionsExt;
        let Some(path) = std::env::var_os(SIGNAL_CHILD_PTY) else {
            return;
        };
        let tty = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY)
            .open(path)
            .unwrap();
        let _terminal = Terminal::enter(tty.as_raw_fd(), tty.as_raw_fd()).unwrap();
        // The signal ends the process before this sleep does.
        std::thread::sleep(Duration::from_secs(20));
    }

    #[test]
    fn pty_exit_signal_restores_then_ends_the_process() {
        use std::os::unix::process::ExitStatusExt;
        use std::process::{Command, Stdio};
        let _lock = PTY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let pty = open_pty();
        let (master, slave) = (pty.master.as_raw_fd(), pty.slave.as_raw_fd());
        let before = flags(slave);
        // Test names leave out the crate name that module_path! starts with.
        let tests = module_path!().split_once("::").unwrap().1;
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                &format!("{tests}::pty_signal_child"),
                "--exact",
                "--nocapture",
            ])
            .env(SIGNAL_CHILD_PTY, &pty.path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        // The alternate screen is entered after raw mode and the handlers.
        let mut drawn = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !contains(&drawn, ENTER_SCREEN) {
            if Instant::now() >= deadline || child.try_wait().unwrap().is_some() {
                let _ = child.kill();
                panic!("the child never entered raw mode");
            }
            drawn.extend(read_output(master, Duration::from_millis(20)));
        }
        assert_ne!(flags(slave), before);
        // SAFETY: signals the child this test spawned and still owns.
        assert_eq!(
            unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) },
            0
        );
        let status = child.wait().unwrap();
        assert_eq!(status.signal(), Some(libc::SIGTERM), "{status}");
        assert_eq!(flags(slave), before);
        drawn.extend(read_output(master, Duration::from_millis(50)));
        assert!(drawn.ends_with(LEAVE_SCREEN));
    }

    #[test]
    fn pty_keys_pick_a_row() {
        let _lock = PTY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (outcome, drawn) = run_on_pty(|master| {
            send(master, b"\x1b[B");
            send(master, b"\r");
        });
        assert_eq!(outcome, picked(DESTINATION, IPHONE_17));
        assert!(contains(&drawn, ENTER_SCREEN));
        assert!(drawn.ends_with(LEAVE_SCREEN));
    }

    #[test]
    fn pty_lone_escape_cancels() {
        let _lock = PTY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (outcome, drawn) = run_on_pty(|master| send(master, b"\x1b"));
        assert_eq!(outcome, Outcome::Cancelled);
        assert!(drawn.ends_with(LEAVE_SCREEN));
    }
}
