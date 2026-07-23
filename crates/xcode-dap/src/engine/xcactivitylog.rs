//! Reader for Xcode's `.xcactivitylog` build logs (gzip-wrapped SLF0
//! streams) — extracts the per-module `swiftc` invocations that a Build
//! Server needs to answer sourcekit-lsp compile-args queries.
//!
//! The file is a token stream, not a document: after an ASCII payload the
//! next byte is a *delimiter* that names the token's type. String tokens are
//! length-prefixed (`NNN"` then exactly `NNN` bytes), so a string may itself
//! contain delimiter bytes or newlines. Scanning for markers with a plain
//! substring search desyncs on the first such string; the tokenizer here
//! consumes every token (even the ones it discards) to stay framed.
//!
//! Wired into the `bsp` subcommand's Build Server and [`super::compile_store`].

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// One Swift module's compile invocation, extracted from a build log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedModule {
    /// `-module-name` value; the store keys on this.
    pub module_name: String,
    /// `-working-directory` value, else the section's `cd` directory.
    pub working_dir: String,
    /// `-index-store-path` value, or inferred for index-less (Release) builds.
    pub index_store_path: Option<String>,
    /// `.swift` arguments listed inline on the command (empty for the
    /// modern SwiftDriver form, which lists files in a `.SwiftFileList`).
    pub files: Vec<String>,
    /// `@…SwiftFileList` response-file paths (leading `@` stripped).
    pub file_lists: Vec<String>,
    /// The compiler arguments with `argv[0]` (the `swiftc` path) dropped and
    /// `@…SwiftFileList` args kept verbatim.
    pub args: Vec<String>,
}

/// Read, decompress and parse a `.xcactivitylog`. Returns the compile
/// modules keyed last-wins by name (Xcode emits some modules twice, under
/// both `SwiftDriver` and `SwiftDriver\ Compilation`). Never panics: an
/// empty, truncated, non-gzip or non-SLF file yields an empty vec.
pub fn parse_log(path: &Path) -> Vec<ParsedModule> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    parse_decompressed(&gunzip_partial(&bytes))
}

/// Discover the newest `.xcactivitylog` for `scheme` under a build root
/// (a DerivedData product dir). Reads `Logs/Build/LogStoreManifest.plist`
/// (via `plutil`, so binary plists are handled) and returns the newest log
/// registered for that scheme. In-flight / cancelled builds leave an
/// unregistered 0-byte log, so the manifest gate never returns a partial.
pub fn newest_log(build_root: &Path, scheme: &str) -> Option<PathBuf> {
    let manifest = build_root.join("Logs/Build/LogStoreManifest.plist");
    let json = plutil_to_json(&manifest)?;
    let file_name = newest_log_filename(&json, scheme)?;
    Some(build_root.join("Logs/Build").join(file_name))
}

/// Every registered `.xcactivitylog` for `scheme` under a build root, oldest
/// first (ascending `timeStoppedRecording`). Used by the bsp cold-start
/// bootstrap to replay full build history — merging oldest→newest reconstructs
/// module coverage a single (newest) log would miss. Empty when the manifest
/// is absent or unreadable.
pub fn logs_for_scheme(build_root: &Path, scheme: &str) -> Vec<PathBuf> {
    let manifest = build_root.join("Logs/Build/LogStoreManifest.plist");
    let Some(json) = plutil_to_json(&manifest) else {
        return Vec::new();
    };
    logs_for_scheme_sorted(&json, scheme)
        .into_iter()
        .map(|f| build_root.join("Logs/Build").join(f))
        .collect()
}

// ---------------------------------------------------------------------------
// gzip + SLF0 tokenizer
// ---------------------------------------------------------------------------

/// Decompress a gzip stream, tolerating truncation: whatever inflated before
/// the error is returned, and a non-gzip input yields an empty vec.
/// Uses `MultiGzDecoder` because Xcode writes `.xcactivitylog` as several
/// concatenated gzip members — a plain `GzDecoder` would stop after the first.
fn gunzip_partial(bytes: &[u8]) -> Vec<u8> {
    use std::io::Read;
    let mut out = Vec::new();
    let mut decoder = flate2::read::MultiGzDecoder::new(bytes);
    let _ = decoder.read_to_end(&mut out); // best-effort: keep the inflated prefix
    out
}

fn is_delim(b: u8) -> bool {
    matches!(b, b'"' | b'#' | b'^' | b'(' | b'%' | b'@' | b'-' | b'*')
}

fn parse_len(prefix: &[u8]) -> Option<usize> {
    if prefix.is_empty() {
        return None;
    }
    std::str::from_utf8(prefix).ok()?.parse::<usize>().ok()
}

/// Extract every `String` token (delimiter `"`) from a decompressed SLF0
/// stream. `Class` (`%`) and `Json` (`*`) tokens are also length-prefixed —
/// consumed to stay framed but not returned. Integer / double / array /
/// instance / null tokens carry no payload. Stops (never panics) at the
/// first truncated token.
fn slf_strings(data: &[u8]) -> Vec<String> {
    if data.len() < 4 || &data[..4] != b"SLF0" {
        return Vec::new();
    }
    let n = data.len();
    let mut out = Vec::new();
    let mut i = 4;
    while i < n {
        let start = i;
        while i < n && !is_delim(data[i]) {
            i += 1;
        }
        if i >= n {
            break; // trailing bytes with no delimiter — truncated tail
        }
        let delim = data[i];
        let prefix = &data[start..i];
        i += 1; // consume the delimiter byte
        match delim {
            b'"' | b'%' | b'*' => {
                let Some(len) = parse_len(prefix) else {
                    break; // malformed length — stop rather than desync
                };
                let end = match i.checked_add(len) {
                    Some(e) if e <= n => e,
                    _ => break, // payload runs past the buffer — truncated
                };
                if delim == b'"' {
                    out.push(String::from_utf8_lossy(&data[i..end]).into_owned());
                }
                i = end;
            }
            // `#` `^` `(` `@` `-`: value lives in the (already-consumed)
            // prefix, no trailing payload.
            _ => {}
        }
    }
    out
}

// ---------------------------------------------------------------------------
// section grammar
// ---------------------------------------------------------------------------

fn parse_decompressed(data: &[u8]) -> Vec<ParsedModule> {
    collect_modules(
        slf_strings(data)
            .into_iter()
            .filter(|s| is_section_candidate(s)),
    )
}

/// Fold section strings (each a title line + its indented body) into modules,
/// keyed last-wins by `-module-name` (Xcode emits some modules twice, under
/// both the `SwiftDriver` and `SwiftDriver\ Compilation` titles). Shared by
/// the SLF (`.xcactivitylog`) and plain-text (xcodebuild stdout) entry points.
fn collect_modules(sections: impl Iterator<Item = String>) -> Vec<ParsedModule> {
    let mut by_name: BTreeMap<String, ParsedModule> = BTreeMap::new();
    for s in sections {
        if let Some(m) = parse_section(&s) {
            by_name.insert(m.module_name.clone(), m);
        }
    }
    by_name.into_values().collect()
}

/// Parse plain-text `xcodebuild` stdout (as captured in
/// `~/.zedxcode/logs/build-latest.log`) into compile modules, reusing the SLF
/// section grammar: a stdout compile task prints the same title + `cd` +
/// `builtin-…swiftc …` lines the `.xcactivitylog` stores as one SLF string.
/// This is how CLI builds feed the store — Xcode 26.3 `xcodebuild` does not
/// reliably write an `.xcactivitylog` into an existing DerivedData.
pub fn parse_text_lines(text: &str) -> Vec<ParsedModule> {
    collect_modules(group_text_sections(text).into_iter())
}

/// Group stdout into candidate section strings: each begins at a column-0
/// Swift-compile task title ([`is_section_candidate`]) and runs to the next
/// blank line, next section title, or EOF — the shape [`parse_section`] takes.
fn group_text_sections(text: &str) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut sections = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if !is_section_start(lines[i]) {
            i += 1;
            continue;
        }
        let start = i;
        i += 1;
        while i < lines.len() && !lines[i].trim().is_empty() && !is_section_start(lines[i]) {
            i += 1;
        }
        sections.push(lines[start..i].join("\n"));
    }
    sections
}

/// A column-0 (unindented) Swift-compile task title line. Body lines (`cd`,
/// `builtin-…`) are indented, so they never start a new section.
fn is_section_start(line: &str) -> bool {
    !line.starts_with(char::is_whitespace) && is_section_candidate(line)
}

/// Section title prefixes. Both `SwiftDriver ` and `SwiftDriver\ Compilation `
/// are required on modern Xcode — some modules appear only under one form.
/// `CompileSwiftSources ` is the legacy (pre-SwiftDriver) title.
fn is_section_candidate(s: &str) -> bool {
    s.starts_with("SwiftDriver ")
        || s.starts_with("SwiftDriver\\ Compilation ")
        || s.starts_with("CompileSwiftSources ")
}

/// Parse a section string (title line + indented body lines) into a module.
/// Returns `None` for the noise sections (`…Compilation Requirements`,
/// `SwiftCompile`, `SwiftEmitModule`, …) whose command fails the checks.
fn parse_section(value: &str) -> Option<ParsedModule> {
    // Xcode separates the title/body with a bare CR (`\r`), so split like
    // Python's `str.splitlines()` (CR, LF, or CRLF), not Rust's `str::lines()`
    // which only breaks on LF.
    let mut lines = split_lines(value).into_iter();
    let _title = lines.next()?;

    // Body = the indented lines, trimmed, up to the first blank line
    // (mirrors the reference's read-until-empty behaviour).
    let mut body: Vec<String> = Vec::new();
    for line in lines {
        let t = line.trim();
        if t.is_empty() {
            break;
        }
        body.push(t.to_string());
    }
    let command = body.last()?; // command = last non-empty body line

    // Modern SwiftDriver-era commands are `builtin-… -- <swiftc …>`; accept
    // only the two real compile wrappers, rejecting other `builtin-… -- `
    // phases (e.g. the `builtin-Swift-Compilation-Requirements` command of a
    // Requirements section). Legacy (pre-SwiftDriver, Xcode <= 13)
    // `CompileSwiftSources` sections log a bare `swiftc` invocation with no
    // wrapper — keep it as-is so those still parse.
    let rest = if command.starts_with("builtin-Swift-Compilation -- ")
        || command.starts_with("builtin-SwiftDriver -- ")
    {
        let idx = command.find(" -- ")?;
        &command[idx + " -- ".len()..]
    } else if command.contains(" -- ") {
        return None;
    } else {
        command.as_str()
    };
    if !rest.contains("bin/swiftc ") {
        return None;
    }

    let all_args = shell_split(rest);
    if all_args.is_empty() {
        return None;
    }

    let module_name = arg_value(&all_args, "-module-name")?;
    if module_name.is_empty() {
        return None;
    }

    let cd_dir = body.iter().find_map(|l| {
        l.strip_prefix("cd ")
            .and_then(|_| shell_split(l).into_iter().nth(1))
    });
    let working_dir = arg_value(&all_args, "-working-directory")
        .or(cd_dir)
        .unwrap_or_default();

    let index_store_path =
        arg_value(&all_args, "-index-store-path").or_else(|| infer_index_store(&all_args));

    let files: Vec<String> = all_args
        .iter()
        .filter(|a| a.ends_with(".swift"))
        .cloned()
        .collect();
    let file_lists: Vec<String> = all_args
        .iter()
        .filter(|a| a.starts_with('@') && a.ends_with(".SwiftFileList"))
        .map(|a| a[1..].to_string())
        .collect();

    // args = the invocation with argv[0] (the swiftc path) dropped.
    let args = all_args[1..].to_vec();

    Some(ParsedModule {
        module_name,
        working_dir,
        index_store_path,
        files,
        file_lists,
        args,
    })
}

/// Split like Python's `str.splitlines()`: on CR, LF, or CRLF, each counting
/// as a single break. CR/LF are ASCII, so byte scanning is UTF-8-safe.
fn split_lines(s: &str) -> Vec<&str> {
    let bytes = s.as_bytes();
    let mut lines = Vec::new();
    let (mut start, mut i) = (0, 0);
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => {
                lines.push(&s[start..i]);
                i += 1;
            }
            b'\r' => {
                lines.push(&s[start..i]);
                i += if i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                    2
                } else {
                    1
                };
            }
            _ => {
                i += 1;
                continue;
            }
        }
        start = i;
    }
    if start < bytes.len() {
        lines.push(&s[start..]);
    }
    lines
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// Release builds omit `-index-store-path`; Xcode still writes a debug index
/// alongside the intermediates. Infer the store from any absolute arg that
/// passes through `…/Build/Intermediates.noindex`: the DerivedData root is
/// the prefix before `/Build`, and the store is `<root>/Index.noindex/DataStore`.
fn infer_index_store(args: &[String]) -> Option<String> {
    for v in args {
        if !v.starts_with('/') {
            continue;
        }
        if let Some(i) = v.find("/Build/Intermediates.noindex") {
            if i > 0 {
                return Some(format!("{}/Index.noindex/DataStore", &v[..i]));
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// shell-word splitting (shared with compile_store)
// ---------------------------------------------------------------------------

/// Split a command / response-file string into arguments the way the build
/// log encodes them: whitespace-separated, single/double quotes, and
/// backslash-escaped spaces. A backslash before a space becomes a literal
/// space (keeping a path together); every other backslash sequence — notably
/// Xcode's `\=` — is kept verbatim and unescaped later at serve time.
pub(crate) fn shell_split(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let n = chars.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i < n {
        while i < n && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= n {
            break;
        }
        let mut cur = String::new();
        while i < n && !chars[i].is_whitespace() {
            match chars[i] {
                '\\' => {
                    if i + 1 < n {
                        let next = chars[i + 1];
                        if next == ' ' {
                            cur.push(' '); // escaped space — real space, not a boundary
                        } else {
                            cur.push('\\');
                            cur.push(next); // e.g. `\=` — kept verbatim
                        }
                        i += 2;
                    } else {
                        cur.push('\\');
                        i += 1;
                    }
                }
                '\'' => {
                    i += 1;
                    while i < n && chars[i] != '\'' {
                        cur.push(chars[i]);
                        i += 1;
                    }
                    if i < n {
                        i += 1; // closing quote
                    }
                }
                '"' => {
                    i += 1;
                    while i < n {
                        if chars[i] == '\\' && i + 1 < n && chars[i + 1] == '"' {
                            cur.push('\\');
                            cur.push('"'); // escaped quote stays content
                            i += 2;
                        } else if chars[i] == '"' {
                            i += 1; // closing quote
                            break;
                        } else {
                            cur.push(chars[i]);
                            i += 1;
                        }
                    }
                }
                c => {
                    cur.push(c);
                    i += 1;
                }
            }
        }
        out.push(cur);
    }
    out
}

// ---------------------------------------------------------------------------
// LogStoreManifest.plist
// ---------------------------------------------------------------------------

fn plutil_to_json(path: &Path) -> Option<serde_json::Value> {
    if !path.exists() {
        return None;
    }
    let out = std::process::Command::new("/usr/bin/plutil")
        .args(["-convert", "json", "-o", "-"])
        .arg(path)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

/// Pure interpretation of a `LogStoreManifest.plist` (already JSON): the
/// `fileName` of the newest log whose `schemeIdentifier-schemeName` matches
/// `scheme`, by `timeStoppedRecording`. Entries missing any field are skipped.
fn newest_log_filename(manifest: &serde_json::Value, scheme: &str) -> Option<String> {
    let logs = manifest.get("logs")?.as_object()?;
    let mut best: Option<(f64, String)> = None;
    for entry in logs.values() {
        if entry
            .get("schemeIdentifier-schemeName")
            .and_then(|v| v.as_str())
            != Some(scheme)
        {
            continue;
        }
        let (Some(time), Some(file)) = (
            entry.get("timeStoppedRecording").and_then(|v| v.as_f64()),
            entry.get("fileName").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        if best.as_ref().is_none_or(|(bt, _)| time > *bt) {
            best = Some((time, file.to_string()));
        }
    }
    best.map(|(_, f)| f)
}

/// Pure interpretation of a `LogStoreManifest.plist` (already JSON): every
/// log `fileName` whose `schemeIdentifier-schemeName` matches `scheme`,
/// sorted ascending by `timeStoppedRecording`. Entries missing any field are
/// skipped. The bootstrap replays these oldest→newest.
fn logs_for_scheme_sorted(manifest: &serde_json::Value, scheme: &str) -> Vec<String> {
    let Some(logs) = manifest.get("logs").and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    let mut entries: Vec<(f64, String)> = Vec::new();
    for entry in logs.values() {
        if entry
            .get("schemeIdentifier-schemeName")
            .and_then(|v| v.as_str())
            != Some(scheme)
        {
            continue;
        }
        let (Some(time), Some(file)) = (
            entry.get("timeStoppedRecording").and_then(|v| v.as_f64()),
            entry.get("fileName").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        entries.push((time, file.to_string()));
    }
    entries.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    entries.into_iter().map(|(_, f)| f).collect()
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// SLF0 token, for building synthetic streams (never captured bytes).
    enum Tok {
        Str(String),
        Int(i64),
        Double(f64),
        Array(usize),
        Instance(usize),
        Null,
        Class(String),
        Json(String),
    }

    fn encode_stream(toks: &[Tok]) -> Vec<u8> {
        let mut out = b"SLF0".to_vec();
        for t in toks {
            match t {
                Tok::Str(s) => {
                    out.extend(s.len().to_string().bytes());
                    out.push(b'"');
                    out.extend(s.bytes());
                }
                Tok::Class(s) => {
                    out.extend(s.len().to_string().bytes());
                    out.push(b'%');
                    out.extend(s.bytes());
                }
                Tok::Json(s) => {
                    out.extend(s.len().to_string().bytes());
                    out.push(b'*');
                    out.extend(s.bytes());
                }
                Tok::Int(v) => {
                    out.extend(v.to_string().bytes());
                    out.push(b'#');
                }
                Tok::Array(v) => {
                    out.extend(v.to_string().bytes());
                    out.push(b'(');
                }
                Tok::Instance(v) => {
                    out.extend(v.to_string().bytes());
                    out.push(b'@');
                }
                Tok::Double(v) => {
                    let hex: String = v.to_le_bytes().iter().map(|b| format!("{b:02x}")).collect();
                    out.extend(hex.bytes());
                    out.push(b'^');
                }
                Tok::Null => out.push(b'-'),
            }
        }
        out
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    fn tmpdir() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "zedxcode-xcactivitylog-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // A realistic SwiftDriver section (synthetic paths only).
    fn driver_section(
        title: &str,
        module: &str,
        wrapper: &str,
        with_index_store: bool,
        filelist: &str,
    ) -> String {
        let dd = "/Users/x/DD";
        let index = if with_index_store {
            format!(" -index-store-path {dd}/Index.noindex/DataStore")
        } else {
            String::new()
        };
        format!(
            "{title} {module} normal arm64 com.apple.xcode.tools.swift.compiler (in target '{module}' from project '{module}')\n    \
             cd /Users/x/MyApp/Modules/{module}\n    \
             {wrapper} -- /Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/bin/swiftc \
             -module-name {module} -Onone -enforce-exclusivity\\=checked \
             @{filelist} -DDEBUG{index} \
             -target arm64-apple-ios16.0-simulator \
             {dd}/Build/Intermediates.noindex/MyApp.build/x/{module}.build/Objects-normal/arm64/{module}.o \
             -working-directory /Users/x/MyApp/Modules/{module}"
        )
    }

    #[test]
    fn tokenizer_stays_framed_across_lookalikes_and_newlines() {
        // A string whose content contains a `123"`-style token lookalike and
        // a newline must not desync the scanner.
        let tricky = "prefix 123\" and more\nsecond line".to_string();
        let toks = vec![
            Tok::Int(42),
            Tok::Str("first".into()),
            Tok::Class("SomeClass".into()),
            Tok::Str(tricky.clone()),
            Tok::Double(3.5),
            Tok::Array(2),
            Tok::Null,
            Tok::Instance(7),
            Tok::Json("{\"k\":1}".into()),
            Tok::Str("last".into()),
        ];
        let stream = encode_stream(&toks);
        let strings = slf_strings(&stream);
        // Class/Json/int/double/array/null are consumed but not emitted.
        assert_eq!(
            strings,
            vec!["first".to_string(), tricky, "last".to_string()]
        );
    }

    #[test]
    fn parses_two_modules_with_both_section_forms() {
        let dd = "/Users/x/DD";
        let s1 = driver_section(
            "SwiftDriver",
            "AlphaKit",
            "builtin-SwiftDriver",
            true,
            &format!("{dd}/Build/Intermediates.noindex/MyApp.build/x/AlphaKit.build/AlphaKit.SwiftFileList"),
        );
        let s2 = driver_section(
            "SwiftDriver\\ Compilation",
            "BetaKit",
            "builtin-Swift-Compilation",
            false, // no -index-store-path -> must be inferred
            &format!("{dd}/Build/Intermediates.noindex/MyApp.build/x/BetaKit.build/BetaKit.SwiftFileList"),
        );
        let stream = encode_stream(&[Tok::Str(s1), Tok::Str(s2)]);
        let mods = parse_decompressed(&stream);
        assert_eq!(mods.len(), 2);

        let alpha = mods.iter().find(|m| m.module_name == "AlphaKit").unwrap();
        assert_eq!(alpha.working_dir, "/Users/x/MyApp/Modules/AlphaKit");
        assert_eq!(
            alpha.index_store_path.as_deref(),
            Some("/Users/x/DD/Index.noindex/DataStore")
        );
        assert_eq!(alpha.file_lists.len(), 1);
        assert!(alpha.file_lists[0].ends_with("AlphaKit.SwiftFileList"));
        assert!(alpha.files.is_empty()); // SwiftDriver form lists no inline files
                                         // argv[0] (swiftc) dropped, @filelist kept verbatim
        assert!(alpha.args.iter().any(|a| a.starts_with('@')));
        assert!(!alpha.args.iter().any(|a| a.ends_with("bin/swiftc")));
        assert!(alpha.args.contains(&"-module-name".to_string()));

        let beta = mods.iter().find(|m| m.module_name == "BetaKit").unwrap();
        // inferred from the Intermediates.noindex object path
        assert_eq!(
            beta.index_store_path.as_deref(),
            Some("/Users/x/DD/Index.noindex/DataStore")
        );
    }

    #[test]
    fn parses_legacy_compileswiftsources_bare_swiftc() {
        // Pre-SwiftDriver (Xcode <= 13) form: a `CompileSwiftSources` title
        // whose command is a bare `swiftc` invocation with no `builtin-… -- `
        // wrapper. Files are listed inline (no `.SwiftFileList`).
        let section = "CompileSwiftSources normal arm64 com.apple.xcode.tools.swift.compiler (in target 'LegacyKit' from project 'LegacyKit')\n    \
             cd /Users/x/MyApp/Modules/LegacyKit\n    \
             /Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/bin/swiftc -incremental -module-name LegacyKit -Onone /Users/x/MyApp/Modules/LegacyKit/A.swift -index-store-path /Users/x/DD/Index.noindex/DataStore -working-directory /Users/x/MyApp/Modules/LegacyKit";
        let stream = encode_stream(&[Tok::Str(section.into())]);
        let mods = parse_decompressed(&stream);
        assert_eq!(mods.len(), 1, "legacy CompileSwiftSources must parse");
        let m = &mods[0];
        assert_eq!(m.module_name, "LegacyKit");
        assert_eq!(m.working_dir, "/Users/x/MyApp/Modules/LegacyKit");
        assert_eq!(
            m.index_store_path.as_deref(),
            Some("/Users/x/DD/Index.noindex/DataStore")
        );
        // legacy form lists inline .swift files, no .SwiftFileList
        assert!(m.files.iter().any(|f| f.ends_with("A.swift")));
        assert!(m.file_lists.is_empty());
    }

    #[test]
    fn ignores_requirements_and_noise_sections() {
        // A Compilation Requirements section: title matches, but the command
        // is `builtin-Swift-Compilation-Requirements` — must be rejected.
        let requirements = "SwiftDriver\\ Compilation\\ Requirements GammaKit normal arm64 (in target 'GammaKit')\n    \
             cd /Users/x/MyApp/Modules/GammaKit\n    \
             builtin-Swift-Compilation-Requirements -- /Applications/Xcode.app/usr/bin/swiftc -module-name GammaKit";
        // A SwiftCompile per-file section: title is not a candidate at all.
        let swiftcompile = "SwiftCompile normal arm64 /Users/x/MyApp/Modules/GammaKit/Foo.swift (in target 'GammaKit')\n    \
             cd /Users/x/MyApp/Modules/GammaKit\n    \
             builtin-Swift-Compilation -- /Applications/Xcode.app/usr/bin/swiftc -module-name GammaKit";
        let stream = encode_stream(&[Tok::Str(requirements.into()), Tok::Str(swiftcompile.into())]);
        assert!(parse_decompressed(&stream).is_empty());
    }

    #[test]
    fn empty_truncated_and_non_slf_never_panic() {
        // empty gzip
        let dir = tmpdir();
        let empty = dir.join("empty.xcactivitylog");
        std::fs::write(&empty, gzip(b"")).unwrap();
        assert!(parse_log(&empty).is_empty());

        // valid gzip, but not an SLF0 stream
        let non_slf = dir.join("nonslf.xcactivitylog");
        std::fs::write(&non_slf, gzip(b"not a slf file at all")).unwrap();
        assert!(parse_log(&non_slf).is_empty());

        // truncated gzip (chop the tail)
        let full = gzip(&encode_stream(&[Tok::Str("SwiftDriver Foo\n".into())]));
        let truncated = dir.join("trunc.xcactivitylog");
        std::fs::write(&truncated, &full[..full.len() / 2]).unwrap();
        assert!(parse_log(&truncated).is_empty()); // no panic

        // not gzip at all
        let raw = dir.join("raw.xcactivitylog");
        std::fs::write(&raw, b"plain bytes").unwrap();
        assert!(parse_log(&raw).is_empty());

        // missing file
        assert!(parse_log(&dir.join("does-not-exist.xcactivitylog")).is_empty());
    }

    #[test]
    fn parse_log_round_trips_through_gzip() {
        let dir = tmpdir();
        let s = driver_section(
            "SwiftDriver",
            "AlphaKit",
            "builtin-SwiftDriver",
            true,
            "/Users/x/DD/Build/Intermediates.noindex/MyApp.build/x/AlphaKit.build/AlphaKit.SwiftFileList",
        );
        let path = dir.join("log.xcactivitylog");
        std::fs::write(&path, gzip(&encode_stream(&[Tok::Str(s)]))).unwrap();
        let mods = parse_log(&path);
        assert_eq!(mods.len(), 1);
        assert_eq!(mods[0].module_name, "AlphaKit");
    }

    #[test]
    fn parse_text_lines_parses_stdout_sections() {
        // Captured xcodebuild stdout: two real Swift modules (both title
        // forms) interleaved with CompileC/Ld noise and a Requirements
        // section that must be rejected. `\` for the escaped title space.
        let dd = "/Users/x/DD";
        let stdout = format!(
            "note: Building targets in dependency order\n\
             SwiftDriver AlphaKit normal arm64 com.apple.xcode.tools.swift.compiler (in target 'AlphaKit' from project 'AlphaKit')\n    \
             cd /Users/x/MyApp/Modules/AlphaKit\n    \
             builtin-SwiftDriver -- /Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/bin/swiftc -module-name AlphaKit -Onone @{dd}/Build/Intermediates.noindex/AlphaKit.SwiftFileList -index-store-path {dd}/Index.noindex/DataStore\n\
             \n\
             CompileC {dd}/Build/foo.o /Users/x/MyApp/foo.m normal arm64\n    \
             cd /Users/x/MyApp\n    \
             /Applications/Xcode.app/usr/bin/clang -x objective-c foo.m\n\
             \n\
             SwiftDriver\\ Compilation\\ Requirements AlphaKit normal arm64 (in target 'AlphaKit' from project 'AlphaKit')\n    \
             cd /Users/x/MyApp/Modules/AlphaKit\n    \
             builtin-Swift-Compilation-Requirements -- /Applications/Xcode.app/usr/bin/swiftc -module-name AlphaKit\n\
             \n\
             SwiftDriver\\ Compilation BetaKit normal arm64 com.apple.xcode.tools.swift.compiler (in target 'BetaKit' from project 'BetaKit')\n    \
             cd /Users/x/MyApp/Modules/BetaKit\n    \
             builtin-Swift-Compilation -- /Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/bin/swiftc -module-name BetaKit @{dd}/Build/Intermediates.noindex/BetaKit.SwiftFileList\n\
             \n\
             Ld {dd}/Build/Products/BetaKit.framework/BetaKit normal\n    \
             cd /Users/x/MyApp\n"
        );

        let mods = parse_text_lines(&stdout);
        assert_eq!(
            mods.len(),
            2,
            "AlphaKit + BetaKit; CompileC/Ld noise + Requirements excluded"
        );

        let alpha = mods.iter().find(|m| m.module_name == "AlphaKit").unwrap();
        assert_eq!(alpha.working_dir, "/Users/x/MyApp/Modules/AlphaKit");
        assert_eq!(
            alpha.index_store_path.as_deref(),
            Some("/Users/x/DD/Index.noindex/DataStore")
        );
        assert_eq!(alpha.file_lists.len(), 1);
        assert!(alpha.file_lists[0].ends_with("AlphaKit.SwiftFileList"));
        // argv[0] (swiftc) dropped, @filelist kept verbatim
        assert!(alpha.args.iter().any(|a| a.starts_with('@')));
        assert!(!alpha.args.iter().any(|a| a.ends_with("bin/swiftc")));
        assert!(alpha.args.contains(&"-module-name".to_string()));

        assert!(mods.iter().any(|m| m.module_name == "BetaKit"));
    }

    #[test]
    fn parse_text_lines_empty_and_noise_only_yield_nothing() {
        assert!(parse_text_lines("").is_empty());
        assert!(parse_text_lines("note: foo\n** BUILD SUCCEEDED **\n").is_empty());
    }

    #[test]
    fn shell_split_edge_cases() {
        assert_eq!(shell_split("'a b'"), vec!["a b"]);
        assert_eq!(shell_split("\"a b\""), vec!["a b"]);
        assert_eq!(shell_split(r"a\ b"), vec!["a b"]); // escaped space -> real space
                                                       // `\=` is kept verbatim (unescaped later at serve time)
        assert_eq!(
            shell_split(r"-enforce-exclusivity\=checked"),
            vec![r"-enforce-exclusivity\=checked"]
        );
        assert_eq!(
            shell_split("-module-name  Foo   -Onone"),
            vec!["-module-name", "Foo", "-Onone"]
        );
        assert_eq!(shell_split(""), Vec::<String>::new());
        // a filelist path with an escaped space stays one token
        assert_eq!(
            shell_split(r"/Users/x/My\ App/A.swift"),
            vec!["/Users/x/My App/A.swift"]
        );
    }

    #[test]
    fn newest_log_filename_picks_max_time_for_scheme() {
        let manifest = serde_json::json!({
            "logs": {
                "AAAAAAAA-0000": {
                    "fileName": "old.xcactivitylog",
                    "schemeIdentifier-schemeName": "MyApp",
                    "timeStoppedRecording": 100.0
                },
                "BBBBBBBB-1111": {
                    "fileName": "new.xcactivitylog",
                    "schemeIdentifier-schemeName": "MyApp",
                    "timeStoppedRecording": 200.5
                },
                "CCCCCCCC-2222": {
                    "fileName": "other-scheme.xcactivitylog",
                    "schemeIdentifier-schemeName": "OtherApp",
                    "timeStoppedRecording": 999.0
                },
                "DDDDDDDD-3333": {
                    "schemeIdentifier-schemeName": "MyApp"
                    // missing timeStoppedRecording + fileName -> skipped
                }
            }
        });
        assert_eq!(
            newest_log_filename(&manifest, "MyApp").as_deref(),
            Some("new.xcactivitylog")
        );
        assert_eq!(newest_log_filename(&manifest, "Nonexistent"), None);
        // no `logs` key -> None, no panic
        assert_eq!(newest_log_filename(&serde_json::json!({}), "MyApp"), None);
    }

    #[test]
    fn logs_for_scheme_sorted_is_ascending_and_scheme_scoped() {
        let manifest = serde_json::json!({
            "logs": {
                "BBBB": {
                    "fileName": "new.xcactivitylog",
                    "schemeIdentifier-schemeName": "MyApp",
                    "timeStoppedRecording": 200.5
                },
                "AAAA": {
                    "fileName": "old.xcactivitylog",
                    "schemeIdentifier-schemeName": "MyApp",
                    "timeStoppedRecording": 100.0
                },
                "CCCC": {
                    "fileName": "mid.xcactivitylog",
                    "schemeIdentifier-schemeName": "MyApp",
                    "timeStoppedRecording": 150.0
                },
                "DDDD": {
                    "fileName": "other.xcactivitylog",
                    "schemeIdentifier-schemeName": "OtherApp",
                    "timeStoppedRecording": 999.0
                },
                "EEEE": {
                    "schemeIdentifier-schemeName": "MyApp"
                    // missing fields -> skipped
                }
            }
        });
        // Ascending by timeStoppedRecording, only this scheme, missing skipped.
        assert_eq!(
            logs_for_scheme_sorted(&manifest, "MyApp"),
            vec![
                "old.xcactivitylog".to_string(),
                "mid.xcactivitylog".to_string(),
                "new.xcactivitylog".to_string()
            ]
        );
        assert!(logs_for_scheme_sorted(&manifest, "Nonexistent").is_empty());
        assert!(logs_for_scheme_sorted(&serde_json::json!({}), "MyApp").is_empty());
    }

    /// End-to-end against a real DerivedData build root. Opt-in via env
    /// (`XCODE_DAP_TEST_BUILD_ROOT` + `XCODE_DAP_TEST_SCHEME`); skipped
    /// silently when unset so it never fails in CI or on other machines.
    #[test]
    fn integration_real_build_root() {
        let (Ok(build_root), Ok(scheme)) = (
            std::env::var("XCODE_DAP_TEST_BUILD_ROOT"),
            std::env::var("XCODE_DAP_TEST_SCHEME"),
        ) else {
            return; // not configured — skip
        };

        let log = newest_log(Path::new(&build_root), &scheme)
            .expect("a registered .xcactivitylog for the scheme");
        let modules = parse_log(&log);
        assert!(
            modules.len() >= 100,
            "expected >= 100 modules from a real workspace log, got {}",
            modules.len()
        );
        assert!(
            modules.iter().any(|m| m
                .index_store_path
                .as_deref()
                .is_some_and(|p| p.ends_with("Index.noindex/DataStore"))),
            "no module carries an Index.noindex/DataStore index-store-path"
        );
    }
}
