//! Reader for Xcode `.xcscheme` files: what a scheme's Run action (its
//! `<LaunchAction>`) asks for — environment, arguments, LLDB init file, Main
//! Thread Checker — and whether the scheme runs a built product.
//!
//! A small hand-written scanner instead of an XML crate: scheme and
//! workspace files are machine-written XML that keeps everything in
//! attributes, so tags, attributes, comments and entities are all it needs.

// The LaunchAction phase wires this module into launch composition and the
// automatic scheme pick; until then only the tests call it.
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context};

/// The parts of a scheme's `<LaunchAction>` a launch uses. Every other
/// attribute and element of the file is ignored. Values are unexpanded: the
/// caller runs [`expand`] with the runnable target's build settings.
///
/// Its `Debug` output shows environment and option keys and the number of
/// arguments, never their values: schemes can hold secrets, and those must
/// not reach a log even through a `{:?}`.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct LaunchAction {
    /// `buildConfiguration`: the configuration Run builds, e.g. `Debug`.
    pub build_configuration: Option<String>,
    /// `customLLDBInitFile`, usually `$(SRCROOT)/…`.
    pub custom_lldb_init_file: Option<String>,
    /// `disableMainThreadChecker = "YES"`.
    pub disable_main_thread_checker: bool,
    /// The `BuildableProductRunnable`'s reference; `None` when Run launches
    /// no built product (a framework or test-only scheme).
    pub runnable: Option<BuildableReference>,
    /// Enabled `CommandLineArgument`s in file order, each as written: one
    /// entry can hold several words. Launch composition expands an entry
    /// with [`expand`] and then splits it with [`split_arguments`], the order
    /// Xcode uses (inferred: in Xcode, too, a `$(SRCROOT)` path with a space
    /// needs quotes around it to stay one argument). [`Self::arguments`]
    /// splits without expanding.
    pub argument_entries: Vec<String>,
    /// Enabled `EnvironmentVariable`s as `(key, value)`, in file order.
    pub environment: Vec<(String, String)>,
    /// Enabled `AdditionalOption`s as `(key, value)`, in file order: where
    /// Xcode keeps Zombie Objects and the malloc diagnostics.
    pub additional_options: Vec<(String, String)>,
}

impl LaunchAction {
    /// Whether Run launches a built product (the action has a
    /// `BuildableProductRunnable`).
    pub fn is_runnable(&self) -> bool {
        self.runnable.is_some()
    }

    /// The enabled arguments as argv words, unexpanded: each entry split
    /// with [`split_arguments`].
    pub fn arguments(&self) -> Vec<String> {
        self.argument_entries
            .iter()
            .flat_map(|entry| split_arguments(entry))
            .collect()
    }
}

impl std::fmt::Debug for LaunchAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fn keys(pairs: &[(String, String)]) -> Vec<&str> {
            pairs.iter().map(|(key, _)| key.as_str()).collect()
        }
        f.debug_struct("LaunchAction")
            .field("build_configuration", &self.build_configuration)
            .field("custom_lldb_init_file", &self.custom_lldb_init_file)
            .field(
                "disable_main_thread_checker",
                &self.disable_main_thread_checker,
            )
            .field("runnable", &self.runnable)
            .field("argument_entries", &self.argument_entries.len())
            .field("environment_keys", &keys(&self.environment))
            .field("additional_option_keys", &keys(&self.additional_options))
            .finish()
    }
}

/// The `BuildableReference` inside a LaunchAction's
/// `BuildableProductRunnable`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildableReference {
    /// The product's file name, e.g. `MyApp.app`.
    pub buildable_name: Option<String>,
    /// The target's name, e.g. `MyApp`.
    pub blueprint_name: Option<String>,
    /// The project that owns the target, as a location such as
    /// `container:MyApp.xcodeproj` (see [`Self::container_path`]).
    pub referenced_container: Option<String>,
}

impl BuildableReference {
    /// The project `ReferencedContainer` names, resolved against the
    /// container that holds `scheme_file`. `None` when either is not in a
    /// shape Xcode writes.
    pub fn container_path(&self, scheme_file: &Path) -> Option<PathBuf> {
        let owner = owner_container(scheme_file)?;
        let base = container_dir(owner);
        resolve_location(
            self.referenced_container.as_deref()?,
            base,
            base,
            enclosing_project(owner),
        )
    }
}

/// What [`load`] found for a scheme.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemeLaunch {
    /// The scheme's file; `launch_action` is `None` when the file has no
    /// `<LaunchAction>`.
    Found {
        path: PathBuf,
        launch_action: Option<LaunchAction>,
    },
    /// No file: Xcode keeps auto-created schemes in memory only. `message`
    /// is the line Status shows ([`missing_message`]).
    Missing { message: String },
}

impl SchemeLaunch {
    /// Whether the scheme runs a built product; `None` for a scheme without
    /// a file, whose Run action is unknown.
    pub fn runnable(&self) -> Option<bool> {
        match self {
            SchemeLaunch::Found { launch_action, .. } => Some(
                launch_action
                    .as_ref()
                    .is_some_and(LaunchAction::is_runnable),
            ),
            SchemeLaunch::Missing { .. } => None,
        }
    }
}

/// Find and read the `.xcscheme` of `scheme` for `container` (a workspace
/// or a project), with the current user's `xcuserdata` searched last.
///
/// An `Err` means a scheme file exists but cannot be read or is not a
/// well-formed scheme (Xcode would not open it either). A launch should then
/// go ahead as for [`SchemeLaunch::Missing`], with the error as a warning,
/// rather than fail.
pub fn load(container: &Path, scheme: &str) -> anyhow::Result<SchemeLaunch> {
    load_for_user(container, scheme, current_user().as_deref())
}

/// [`load`] for an explicit `xcuserdata` owner (`None` skips `xcuserdata`).
pub fn load_for_user(
    container: &Path,
    scheme: &str,
    user: Option<&str>,
) -> anyhow::Result<SchemeLaunch> {
    match locate(container, scheme, user) {
        Some(path) => {
            let launch_action = read(&path)?;
            Ok(SchemeLaunch::Found {
                path,
                launch_action,
            })
        }
        None => Ok(SchemeLaunch::Missing {
            message: missing_message(scheme),
        }),
    }
}

/// The Status line for a scheme without a `.xcscheme` file.
pub fn missing_message(scheme: &str) -> String {
    format!(
        "Scheme \"{scheme}\" has no .xcscheme file, so no launch environment or arguments \
         are applied."
    )
}

/// The account name Xcode uses for `xcuserdata/<name>.xcuserdatad`.
pub fn current_user() -> Option<String> {
    ["USER", "LOGNAME"]
        .into_iter()
        .find_map(|key| std::env::var(key).ok().filter(|v| !v.is_empty()))
}

/// The `.xcscheme` file of `scheme`, from the places Xcode keeps them; the
/// first existing file wins:
/// 1. `<container>/xcshareddata/xcschemes/`;
/// 2. the same in each project the workspace's `contents.xcworkspacedata`
///    references, in file order;
/// 3. `xcuserdata/<user>.xcuserdatad/xcschemes/` of the container, then of
///    each referenced project (skipped when `user` is `None`).
pub fn locate(container: &Path, scheme: &str, user: Option<&str>) -> Option<PathBuf> {
    if scheme.is_empty() || scheme.contains(['/', '\0']) {
        return None;
    }
    let file = format!("{scheme}.xcscheme");
    let mut owners = vec![container.to_path_buf()];
    for project in referenced_projects(container) {
        if !owners.contains(&project) {
            owners.push(project);
        }
    }
    let mut candidates: Vec<PathBuf> = owners
        .iter()
        .map(|owner| owner.join("xcshareddata").join("xcschemes").join(&file))
        .collect();
    if let Some(user) = user {
        candidates.extend(owners.iter().map(|owner| {
            owner
                .join("xcuserdata")
                .join(format!("{user}.xcuserdatad"))
                .join("xcschemes")
                .join(&file)
        }));
    }
    candidates.into_iter().find(|path| path.is_file())
}

/// Read and parse one scheme file ([`parse`]).
pub fn read(path: &Path) -> anyhow::Result<Option<LaunchAction>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse(&text).with_context(|| format!("parsing {}", path.display()))
}

/// The `<LaunchAction>` of a scheme file's text (the first one, if a file
/// had several); `None` when it has none. Fails on text that is not a
/// well-formed scheme.
pub fn parse(text: &str) -> anyhow::Result<Option<LaunchAction>> {
    let mut stack: Vec<String> = Vec::new();
    let mut launch: Option<LaunchAction> = None;
    // Set when the first LaunchAction closes: later ones are ignored.
    let mut launch_done = false;
    let mut root_seen = false;
    for (offset, token) in tokenize(text)? {
        let line = || line_of(text, offset);
        match token {
            Token::Start {
                name,
                attrs,
                self_closing,
            } => {
                if stack.is_empty() {
                    if root_seen {
                        bail!("a second root element <{name}> at line {}", line());
                    }
                    if name != "Scheme" {
                        bail!("not an Xcode scheme: the root element is <{name}>, not <Scheme>");
                    }
                    root_seen = true;
                } else if !launch_done {
                    let path: Vec<&str> = stack[1..]
                        .iter()
                        .map(String::as_str)
                        .chain([name.as_str()])
                        .collect();
                    visit(&path, &attrs, &mut launch);
                    if self_closing && path == ["LaunchAction"] {
                        launch_done = true;
                    }
                }
                if !self_closing {
                    stack.push(name);
                }
            }
            Token::End { name } => {
                match stack.pop() {
                    Some(open) if open == name => {}
                    Some(open) => bail!("</{name}> at line {} closes <{open}>", line()),
                    None => bail!("</{name}> at line {} has no start tag", line()),
                }
                if name == "LaunchAction" && stack.len() == 1 {
                    launch_done = true;
                }
            }
        }
    }
    if let Some(open) = stack.last() {
        bail!("the file ends inside <{open}>");
    }
    if !root_seen {
        bail!("not an Xcode scheme: no <Scheme> element");
    }
    Ok(launch)
}

/// Record what one start tag inside `<Scheme>` contributes; `path` runs from
/// the Scheme's child element down to this tag.
fn visit(path: &[&str], attrs: &[(String, String)], launch: &mut Option<LaunchAction>) {
    if path == ["LaunchAction"] {
        if launch.is_none() {
            *launch = Some(LaunchAction {
                build_configuration: non_empty(attr(attrs, "buildConfiguration")),
                custom_lldb_init_file: non_empty(attr(attrs, "customLLDBInitFile")),
                disable_main_thread_checker: is_yes(attr(attrs, "disableMainThreadChecker")),
                ..LaunchAction::default()
            });
        }
        return;
    }
    let Some(launch) = launch.as_mut() else {
        return;
    };
    match path {
        ["LaunchAction", "BuildableProductRunnable"] => {
            launch
                .runnable
                .get_or_insert_with(BuildableReference::default);
        }
        ["LaunchAction", "BuildableProductRunnable", "BuildableReference"] => {
            let runnable = launch
                .runnable
                .get_or_insert_with(BuildableReference::default);
            // The first reference names the product.
            if *runnable == BuildableReference::default() {
                *runnable = BuildableReference {
                    buildable_name: non_empty(attr(attrs, "BuildableName")),
                    blueprint_name: non_empty(attr(attrs, "BlueprintName")),
                    referenced_container: non_empty(attr(attrs, "ReferencedContainer")),
                };
            }
        }
        ["LaunchAction", "CommandLineArguments", "CommandLineArgument"] if is_enabled(attrs) => {
            launch
                .argument_entries
                .push(attr(attrs, "argument").unwrap_or("").to_string());
        }
        ["LaunchAction", "EnvironmentVariables", "EnvironmentVariable"] if is_enabled(attrs) => {
            launch.environment.extend(key_value(attrs));
        }
        ["LaunchAction", "AdditionalOptions", "AdditionalOption"] if is_enabled(attrs) => {
            launch.additional_options.extend(key_value(attrs));
        }
        _ => {}
    }
}

fn attr<'a>(attrs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_str())
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value.filter(|v| !v.is_empty()).map(str::to_string)
}

fn is_yes(value: Option<&str>) -> bool {
    value.is_some_and(|v| v.eq_ignore_ascii_case("YES"))
}

/// Xcode writes `isEnabled = "YES"` or `"NO"` on every entry; an entry
/// without it counts as disabled, the way scheme-editing tools read it.
fn is_enabled(attrs: &[(String, String)]) -> bool {
    is_yes(attr(attrs, "isEnabled"))
}

/// `(key, value)` of an environment entry; entries without a key are
/// skipped.
fn key_value(attrs: &[(String, String)]) -> Option<(String, String)> {
    let key = non_empty(attr(attrs, "key"))?;
    Some((key, attr(attrs, "value").unwrap_or("").to_string()))
}

/// Split one `CommandLineArgument` the way Xcode does: words separated by
/// whitespace; single quotes keep everything literally; double quotes keep
/// whitespace, with `\"` and `\\` as escapes; a backslash outside quotes
/// escapes the next character. `-MyAppArg YES` gives two arguments, and
/// `""` one empty argument. An unclosed quote runs to the end.
pub fn split_arguments(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                for c in chars.by_ref() {
                    if c == '\'' {
                        break;
                    }
                    word.push(c);
                }
            }
            '"' => {
                in_word = true;
                while let Some(c) = chars.next() {
                    match c {
                        '"' => break,
                        '\\' if matches!(chars.peek(), Some('"' | '\\')) => {
                            word.extend(chars.next());
                        }
                        c => word.push(c),
                    }
                }
            }
            '\\' => {
                in_word = true;
                word.push(chars.next().unwrap_or('\\'));
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    words
}

/// Replace each `$(NAME)` and `${NAME}` in `text` with the build setting
/// `NAME` from `settings`. A macro that names no setting, uses an operator
/// such as `:lower` or nests another macro stays as written and adds one
/// warning to `warnings`, which is never repeated for the same macro across
/// calls that share the list. Replacement values are not expanded again.
pub fn expand(
    text: &str,
    settings: &HashMap<String, String>,
    warnings: &mut Vec<String>,
) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(dollar) = rest.find('$') {
        out.push_str(&rest[..dollar]);
        let after = &rest[dollar + 1..];
        let (open, close) = match after.chars().next() {
            Some('(') => ('(', ')'),
            Some('{') => ('{', '}'),
            _ => {
                out.push('$');
                rest = after;
                continue;
            }
        };
        let inner = &after[1..];
        let Some(end) = matching_close(inner, open, close) else {
            // An unclosed macro is plain text.
            out.push_str(&rest[dollar..]);
            return out;
        };
        let name = &inner[..end];
        match settings.get(name) {
            Some(value) => out.push_str(value),
            None => {
                out.push_str(&rest[dollar..dollar + 2 + end + 1]);
                let plain_name =
                    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
                let warning = if plain_name {
                    format!(
                        "The scheme uses $({name}), which is not a build setting of its \
                         target, so it is left as written."
                    )
                } else {
                    format!(
                        "The scheme uses $({name}); macro operators and nested macros are \
                         not expanded, so it is left as written."
                    )
                };
                if !warnings.contains(&warning) {
                    warnings.push(warning);
                }
            }
        }
        rest = &inner[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Byte index in `s` of the `close` that matches an `open` just before `s`.
fn matching_close(s: &str, open: char, close: char) -> Option<usize> {
    let mut depth = 0usize;
    for (i, c) in s.char_indices() {
        if c == open {
            depth += 1;
        } else if c == close {
            if depth == 0 {
                return Some(i);
            }
            depth -= 1;
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Workspace references
// ---------------------------------------------------------------------------

/// Projects a workspace references, in file order: each `FileRef` of its
/// `contents.xcworkspacedata` (nested `Group`s included) that resolves to a
/// `.xcodeproj`. Packages and other files are skipped; a project container
/// or an unreadable file gives none.
fn referenced_projects(container: &Path) -> Vec<PathBuf> {
    let contents = container.join("contents.xcworkspacedata");
    let Ok(text) = std::fs::read_to_string(&contents) else {
        return Vec::new();
    };
    projects_in_workspace(&text, container).unwrap_or_else(|e| {
        log::warn!(target: "xcscheme", "{}: {e:#}", contents.display());
        Vec::new()
    })
}

fn projects_in_workspace(text: &str, workspace: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let base = container_dir(workspace);
    let self_project = enclosing_project(workspace);
    // Directories of the open groups; `group:` locations resolve against
    // the innermost one.
    let mut groups: Vec<PathBuf> = vec![base.to_path_buf()];
    let mut projects = Vec::new();
    for (_, token) in tokenize(text)? {
        let current = groups.last().cloned().unwrap_or_default();
        match token {
            Token::Start {
                name,
                attrs,
                self_closing,
            } if name == "Group" => {
                if !self_closing {
                    let dir = attr(&attrs, "location")
                        .and_then(|l| resolve_location(l, &current, base, self_project))
                        .unwrap_or(current);
                    groups.push(dir);
                }
            }
            Token::Start { name, attrs, .. } if name == "FileRef" => {
                let path = attr(&attrs, "location")
                    .and_then(|l| resolve_location(l, &current, base, self_project));
                if let Some(path) = path.filter(|p| has_extension(p, "xcodeproj")) {
                    projects.push(path);
                }
            }
            Token::End { name } if name == "Group" && groups.len() > 1 => {
                groups.pop();
            }
            _ => {}
        }
    }
    Ok(projects)
}

/// A workspace location: `group:` is relative to the enclosing group's
/// directory, `container:` to the directory that holds the workspace,
/// `absolute:` is a full path, and `self:` is the project an embedded
/// `project.xcworkspace` sits in. Other kinds (e.g. `developer:`) give
/// `None`.
fn resolve_location(
    location: &str,
    group_dir: &Path,
    container_dir: &Path,
    self_project: Option<&Path>,
) -> Option<PathBuf> {
    let (kind, path) = location.split_once(':')?;
    match kind {
        "group" => Some(group_dir.join(path)),
        "container" => Some(container_dir.join(path)),
        "absolute" => Some(PathBuf::from(path)),
        "self" => self_project.map(Path::to_path_buf),
        _ => None,
    }
}

/// The project a `project.xcworkspace` is embedded in. The name matches in
/// any case, as on the default case-insensitive macOS file system.
fn enclosing_project(workspace: &Path) -> Option<&Path> {
    if !workspace
        .file_name()?
        .to_str()?
        .eq_ignore_ascii_case("project.xcworkspace")
    {
        return None;
    }
    workspace
        .parent()
        .filter(|parent| has_extension(parent, "xcodeproj"))
}

/// The directory `container:` locations of `container` resolve against: the
/// one that holds it (for an embedded workspace, the one that holds its
/// project).
fn container_dir(container: &Path) -> &Path {
    enclosing_project(container)
        .unwrap_or(container)
        .parent()
        .unwrap_or(Path::new(""))
}

/// The workspace or project a scheme file belongs to:
/// `<owner>/xcshareddata/xcschemes/S.xcscheme` or
/// `<owner>/xcuserdata/<user>.xcuserdatad/xcschemes/S.xcscheme`.
fn owner_container(scheme_file: &Path) -> Option<&Path> {
    let data = scheme_file.parent()?.parent()?;
    if data.file_name()? == "xcshareddata" {
        data.parent()
    } else if has_extension(data, "xcuserdatad") {
        data.parent()?.parent()
    } else {
        None
    }
}

fn has_extension(path: &Path, ext: &str) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case(ext))
}

// ---------------------------------------------------------------------------
// Tokenizer
// ---------------------------------------------------------------------------

/// One markup token. Text, comments, processing instructions, CDATA and
/// declarations are skipped: scheme and workspace files keep everything in
/// attributes.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Start {
        name: String,
        /// `(name, value)` in file order, values decoded.
        attrs: Vec<(String, String)>,
        self_closing: bool,
    },
    End {
        name: String,
    },
}

/// The tokens of `text`, each with the byte offset of its `<`.
fn tokenize(text: &str) -> anyhow::Result<Vec<(usize, Token)>> {
    let bom = if text.starts_with('\u{feff}') {
        '\u{feff}'.len_utf8()
    } else {
        0
    };
    let mut scanner = Scanner {
        src: text,
        pos: bom,
    };
    let mut tokens = Vec::new();
    while scanner.pos < text.len() {
        let start = scanner.pos;
        if !scanner.eat("<") {
            // Text between tags carries nothing a scheme uses.
            let rest = scanner.rest();
            scanner.pos += rest.find('<').unwrap_or(rest.len());
        } else if scanner.eat("!--") {
            scanner.skip_past("-->", start, "comment")?;
        } else if scanner.eat("?") {
            scanner.skip_past("?>", start, "processing instruction")?;
        } else if scanner.eat("![CDATA[") {
            scanner.skip_past("]]>", start, "CDATA section")?;
        } else if scanner.eat("!") {
            scanner.skip_past(">", start, "declaration")?;
        } else if scanner.eat("/") {
            let name = scanner.name();
            scanner.skip_whitespace();
            if name.is_empty() || !scanner.eat(">") {
                return Err(scanner.error_at(start, format!("malformed end tag </{name}")));
            }
            tokens.push((
                start,
                Token::End {
                    name: name.to_string(),
                },
            ));
        } else {
            tokens.push((start, scanner.start_tag(start)?));
        }
    }
    Ok(tokens)
}

struct Scanner<'a> {
    src: &'a str,
    pos: usize,
}

impl<'a> Scanner<'a> {
    fn rest(&self) -> &'a str {
        &self.src[self.pos..]
    }

    fn eat(&mut self, s: &str) -> bool {
        let found = self.rest().starts_with(s);
        if found {
            self.pos += s.len();
        }
        found
    }

    fn skip_whitespace(&mut self) {
        let rest = self.rest();
        self.pos += rest.len() - rest.trim_start_matches(is_xml_space).len();
    }

    /// Move past the next `end`; a missing one fails, naming the construct
    /// that starts at `start`.
    fn skip_past(&mut self, end: &str, start: usize, what: &str) -> anyhow::Result<()> {
        match self.rest().find(end) {
            Some(n) => {
                self.pos += n + end.len();
                Ok(())
            }
            None => Err(self.error_at(start, format!("unterminated {what}"))),
        }
    }

    /// A tag or attribute name: everything up to whitespace or markup.
    fn name(&mut self) -> &'a str {
        let rest = self.rest();
        let len = rest
            .find(|c: char| is_xml_space(c) || matches!(c, '/' | '>' | '<' | '=' | '"' | '\''))
            .unwrap_or(rest.len());
        self.pos += len;
        &rest[..len]
    }

    /// The rest of a start tag whose `<` is at `start`.
    fn start_tag(&mut self, start: usize) -> anyhow::Result<Token> {
        let name = self.name();
        if name.is_empty() {
            return Err(self.error_at(start, "expected a tag name after '<'"));
        }
        let mut attrs = Vec::new();
        loop {
            self.skip_whitespace();
            let self_closing = self.eat("/>");
            if self_closing || self.eat(">") {
                return Ok(Token::Start {
                    name: name.to_string(),
                    attrs,
                    self_closing,
                });
            }
            if self.rest().is_empty() {
                return Err(self.error_at(start, format!("unterminated tag <{name}")));
            }
            let at = self.pos;
            let key = self.name();
            if key.is_empty() {
                return Err(self.error_at(at, format!("unexpected character in <{name}>")));
            }
            self.skip_whitespace();
            if !self.eat("=") {
                return Err(self.error_at(at, format!("attribute {key} of <{name}> has no value")));
            }
            self.skip_whitespace();
            let quote = match self.rest().chars().next() {
                Some(q @ ('"' | '\'')) => q,
                _ => {
                    return Err(
                        self.error_at(at, format!("attribute {key} of <{name}> is not quoted"))
                    )
                }
            };
            self.pos += 1;
            let Some(len) = self.rest().find(quote) else {
                return Err(self.error_at(
                    at,
                    format!("unterminated value of attribute {key} of <{name}>"),
                ));
            };
            let raw = &self.rest()[..len];
            self.pos += len + 1;
            attrs.push((key.to_string(), decode(raw)));
        }
    }

    fn error_at(&self, offset: usize, message: impl std::fmt::Display) -> anyhow::Error {
        anyhow!("{message} at line {}", line_of(self.src, offset))
    }
}

fn is_xml_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\r')
}

/// 1-based line of byte `offset` in `text`.
fn line_of(text: &str, offset: usize) -> usize {
    text[..offset.min(text.len())].matches('\n').count() + 1
}

/// An attribute value with entities and character references decoded, and
/// literal tabs and line breaks turned into spaces (XML's attribute-value
/// normalization; a `&#10;` reference stays a line break). An `&` that
/// starts no known entity is kept as written.
fn decode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(c) = rest.chars().next() {
        let mut len = c.len_utf8();
        match c {
            '&' => match entity(rest) {
                Some((decoded, entity_len)) => {
                    out.push(decoded);
                    len = entity_len;
                }
                None => out.push('&'),
            },
            // A CR LF pair is one line break.
            '\r' if rest[1..].starts_with('\n') => {}
            '\t' | '\n' | '\r' => out.push(' '),
            c => out.push(c),
        }
        rest = &rest[len..];
    }
    out
}

/// The character an entity at the start of `s` (`&…;`) stands for, and the
/// entity's length in bytes.
fn entity(s: &str) -> Option<(char, usize)> {
    let end = s.find(';')?;
    let body = &s[1..end];
    let decoded = match body {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        _ => {
            let number = body.strip_prefix('#')?;
            let (digits, radix) = match number.strip_prefix('x') {
                Some(hex) => (hex, 16),
                None => (number, 10),
            };
            if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
                return None;
            }
            char::from_u32(u32::from_str_radix(digits, radix).ok()?).filter(|&c| c != '\0')?
        }
    };
    Some((decoded, end + 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn sandbox() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zedxcode-xcscheme-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        // A failed run of an earlier process with the same id may have left
        // it behind.
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// A scheme file whose `<Scheme>` holds `body`.
    fn scheme(body: &str) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Scheme\n   LastUpgradeVersion = \
             \"1600\"\n   version = \"1.7\">\n{body}\n</Scheme>\n"
        )
    }

    /// A scheme whose only LaunchAction sets `MARK=<mark>`, to tell files
    /// apart in the lookup tests.
    fn marked_scheme(mark: &str) -> String {
        scheme(&format!(
            r#"<LaunchAction buildConfiguration = "Debug">
                 <EnvironmentVariables>
                   <EnvironmentVariable key = "MARK" value = "{mark}" isEnabled = "YES"/>
                 </EnvironmentVariables>
               </LaunchAction>"#
        ))
    }

    fn mark_of(path: &Path) -> String {
        let launch = read(path).unwrap().unwrap();
        launch.environment[0].1.clone()
    }

    fn shared(owner: &Path, scheme: &str) -> PathBuf {
        owner
            .join("xcshareddata/xcschemes")
            .join(format!("{scheme}.xcscheme"))
    }

    fn personal(owner: &Path, user: &str, scheme: &str) -> PathBuf {
        owner
            .join("xcuserdata")
            .join(format!("{user}.xcuserdatad"))
            .join("xcschemes")
            .join(format!("{scheme}.xcscheme"))
    }

    fn workspace_data(refs: &str) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Workspace\n   version = \
             \"1.0\">\n{refs}\n</Workspace>\n"
        )
    }

    fn settings(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn fixture_project() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/myapp/MyApp.xcodeproj")
    }

    // -- tokenizer ----------------------------------------------------------

    fn tokens(text: &str) -> Vec<Token> {
        tokenize(text)
            .unwrap()
            .into_iter()
            .map(|(_, t)| t)
            .collect()
    }

    fn start(name: &str, attrs: &[(&str, &str)], self_closing: bool) -> Token {
        Token::Start {
            name: name.to_string(),
            attrs: attrs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            self_closing,
        }
    }

    fn end(name: &str) -> Token {
        Token::End {
            name: name.to_string(),
        }
    }

    #[test]
    fn tokenizes_start_end_and_self_closing_tags_with_attributes() {
        let text = "\u{feff}<?xml version=\"1.0\"?>\n<!DOCTYPE x>\n<A one = \"1\"\n   \
                    two='2'><B/><C three=\"a > b\" /></A >";
        assert_eq!(
            tokens(text),
            vec![
                start("A", &[("one", "1"), ("two", "2")], false),
                start("B", &[], true),
                start("C", &[("three", "a > b")], true),
                end("A"),
            ]
        );
    }

    #[test]
    fn tokenizer_skips_comments_cdata_and_text() {
        let text = "<A>text <!-- <B key=\"x\"/> -- still a comment -->\
                    <![CDATA[<C/>]]>more</A>";
        assert_eq!(tokens(text), vec![start("A", &[], false), end("A")]);
    }

    #[test]
    fn decodes_entities_and_character_references() {
        let text = "<A v=\"&amp; &lt; &gt; &quot; &apos; &#65; &#x42; &#x1F600;\"/>";
        assert_eq!(
            tokens(text),
            vec![start("A", &[("v", "& < > \" ' A B \u{1F600}")], true)]
        );
    }

    #[test]
    fn keeps_unknown_entities_and_a_bare_ampersand_as_written() {
        let text = "<A v=\"a & b &nbsp; &#xZZ; &#0; &#; &amp\"/>";
        assert_eq!(
            tokens(text),
            vec![start(
                "A",
                &[("v", "a & b &nbsp; &#xZZ; &#0; &#; &amp")],
                true
            )]
        );
    }

    #[test]
    fn normalizes_literal_whitespace_in_values_but_not_references() {
        let text = "<A v=\"one\ttwo\r\nthree\nfour&#10;five\"/>";
        assert_eq!(
            tokens(text),
            vec![start("A", &[("v", "one two three four\nfive")], true)]
        );
    }

    #[test]
    fn malformed_markup_fails_with_a_line_number() {
        for (text, needle) in [
            ("<A>\n<!-- open", "unterminated comment at line 2"),
            ("<A\n  key=\"v\"", "unterminated tag <A at line 1"),
            ("<A key=\"v>", "unterminated value of attribute key"),
            ("<A key>", "attribute key of <A> has no value"),
            ("<A key=v>", "attribute key of <A> is not quoted"),
            ("<A>\n\n</A", "malformed end tag </A at line 3"),
            ("< A>", "expected a tag name"),
        ] {
            let err = tokenize(text).unwrap_err().to_string();
            assert!(err.contains(needle), "{text:?}: {err}");
        }
    }

    // -- scheme parsing -----------------------------------------------------

    #[test]
    fn parses_the_committed_myapp_fixture() {
        let path = fixture_project().join("xcshareddata/xcschemes/MyApp.xcscheme");
        let launch = read(&path)
            .unwrap()
            .expect("the fixture has a LaunchAction");
        assert_eq!(
            launch,
            LaunchAction {
                build_configuration: Some("Debug".into()),
                custom_lldb_init_file: Some("$(SRCROOT)/MyApp/run.lldbinit".into()),
                disable_main_thread_checker: false,
                runnable: Some(BuildableReference {
                    buildable_name: Some("MyApp.app".into()),
                    blueprint_name: Some("MyApp".into()),
                    referenced_container: Some("container:MyApp.xcodeproj".into()),
                }),
                argument_entries: vec!["-MyAppArg YES".into()],
                environment: vec![("MYAPP_FLAG".into(), "1".into())],
                additional_options: vec![],
            }
        );
        assert_eq!(launch.arguments(), ["-MyAppArg", "YES"]);
        assert!(launch.is_runnable());
    }

    #[test]
    fn locates_and_loads_the_fixture_through_its_project() {
        let project = fixture_project();
        let found = load_for_user(&project, "MyApp", None).unwrap();
        let SchemeLaunch::Found {
            path,
            launch_action,
        } = &found
        else {
            panic!("expected the fixture's shared scheme, got {found:?}");
        };
        assert_eq!(path, &shared(&project, "MyApp"));
        assert_eq!(found.runnable(), Some(true));
        let reference = launch_action.as_ref().unwrap().runnable.as_ref().unwrap();
        assert_eq!(reference.container_path(path), Some(project.clone()));
    }

    #[test]
    fn reads_only_enabled_entries_of_the_launch_action() {
        let text = scheme(
            r#"
   <TestAction buildConfiguration = "Debug">
      <CommandLineArguments>
         <CommandLineArgument argument = "-FromTests" isEnabled = "YES"/>
      </CommandLineArguments>
      <EnvironmentVariables>
         <EnvironmentVariable key = "TEST_ONLY" value = "1" isEnabled = "YES"/>
      </EnvironmentVariables>
   </TestAction>
   <LaunchAction
      buildConfiguration = "Beta"
      customLLDBInitFile = "${SRCROOT}/tools/app.lldbinit"
      disableMainThreadChecker = "YES"
      enableAddressSanitizer = "YES">
      <BuildableProductRunnable runnableDebuggingMode = "0">
         <BuildableReference
            BuildableIdentifier = "primary"
            BuildableName = "MyApp Beta.app"
            BlueprintName = "MyApp Beta"
            ReferencedContainer = "container:Apps/MyApp.xcodeproj">
         </BuildableReference>
      </BuildableProductRunnable>
      <CommandLineArguments>
         <CommandLineArgument argument = "-OnArg 1" isEnabled = "YES"/>
         <CommandLineArgument argument = "-OffArg" isEnabled = "NO"/>
         <CommandLineArgument argument = "-NoFlagArg"/>
         <CommandLineArgument argument = "&quot;two words&quot; -Last" isEnabled = "YES"/>
      </CommandLineArguments>
      <EnvironmentVariables>
         <EnvironmentVariable key = "ON" value = "a&amp;b" isEnabled = "YES"/>
         <EnvironmentVariable key = "OFF" value = "x" isEnabled = "NO"/>
         <EnvironmentVariable key = "" value = "no key" isEnabled = "YES"/>
         <EnvironmentVariable key = "EMPTY" isEnabled = "YES"/>
      </EnvironmentVariables>
      <AdditionalOptions>
         <AdditionalOption key = "NSZombieEnabled" value = "YES" isEnabled = "YES"/>
         <AdditionalOption key = "MallocScribble" value = "" isEnabled = "YES"/>
         <AdditionalOption key = "MallocGuardEdges" value = "" isEnabled = "NO"/>
      </AdditionalOptions>
      <LocationScenarioReference identifier = "London, England" referenceType = "1"/>
   </LaunchAction>
   <ProfileAction buildConfiguration = "Release">
      <BuildableProductRunnable runnableDebuggingMode = "0">
         <BuildableReference BuildableName = "Profiled.app" BlueprintName = "Profiled"/>
      </BuildableProductRunnable>
   </ProfileAction>"#,
        );
        let launch = parse(&text).unwrap().unwrap();
        assert_eq!(launch.build_configuration.as_deref(), Some("Beta"));
        assert_eq!(
            launch.custom_lldb_init_file.as_deref(),
            Some("${SRCROOT}/tools/app.lldbinit")
        );
        assert!(launch.disable_main_thread_checker);
        assert_eq!(
            launch.runnable,
            Some(BuildableReference {
                buildable_name: Some("MyApp Beta.app".into()),
                blueprint_name: Some("MyApp Beta".into()),
                referenced_container: Some("container:Apps/MyApp.xcodeproj".into()),
            })
        );
        assert_eq!(launch.argument_entries, ["-OnArg 1", "\"two words\" -Last"]);
        assert_eq!(launch.arguments(), ["-OnArg", "1", "two words", "-Last"]);
        assert_eq!(
            launch.environment,
            [
                ("ON".to_string(), "a&b".to_string()),
                ("EMPTY".to_string(), String::new()),
            ]
        );
        assert_eq!(
            launch.additional_options,
            [
                ("NSZombieEnabled".to_string(), "YES".to_string()),
                ("MallocScribble".to_string(), String::new()),
            ]
        );
    }

    #[test]
    fn debug_output_shows_keys_but_no_values_or_arguments() {
        let text = scheme(
            r#"<LaunchAction buildConfiguration = "Debug">
      <CommandLineArguments>
         <CommandLineArgument argument = "-Token canary-arg-7f3a" isEnabled = "YES"/>
      </CommandLineArguments>
      <EnvironmentVariables>
         <EnvironmentVariable key = "API_TOKEN" value = "canary-env-7f3a" isEnabled = "YES"/>
      </EnvironmentVariables>
      <AdditionalOptions>
         <AdditionalOption key = "MallocStackLogging" value = "canary-opt-7f3a" isEnabled = "YES"/>
      </AdditionalOptions>
   </LaunchAction>"#,
        );
        let launch = parse(&text).unwrap().unwrap();
        let found = SchemeLaunch::Found {
            path: PathBuf::from("/Users/x/MyApp.xcscheme"),
            launch_action: Some(launch.clone()),
        };
        for shown in [format!("{launch:?}"), format!("{found:#?}")] {
            assert!(!shown.contains("canary"), "{shown}");
            assert!(shown.contains("API_TOKEN"), "{shown}");
            assert!(shown.contains("MallocStackLogging"), "{shown}");
        }
    }

    #[test]
    fn commented_out_entries_are_ignored() {
        let text = scheme(
            r#"<LaunchAction buildConfiguration = "Debug">
      <!-- <BuildableProductRunnable><BuildableReference BuildableName = "Old.app"/>
           </BuildableProductRunnable> -->
      <EnvironmentVariables>
         <!--
         <EnvironmentVariable key = "COMMENTED" value = "1" isEnabled = "YES"/>
         -->
         <EnvironmentVariable key = "KEPT" value = "1" isEnabled = "YES"/>
      </EnvironmentVariables>
   </LaunchAction>"#,
        );
        let launch = parse(&text).unwrap().unwrap();
        assert_eq!(launch.environment, [("KEPT".to_string(), "1".to_string())]);
        assert!(!launch.is_runnable());
    }

    #[test]
    fn a_scheme_without_a_launch_action_has_none() {
        let text = scheme(
            r#"<BuildAction parallelizeBuildables = "YES"></BuildAction>
   <TestAction buildConfiguration = "Debug"></TestAction>"#,
        );
        assert_eq!(parse(&text).unwrap(), None);
    }

    #[test]
    fn a_launch_action_without_a_runnable_product_is_not_runnable() {
        // A framework scheme: the Run action only names a target for macro
        // expansion.
        let text = scheme(
            r#"<LaunchAction buildConfiguration = "Debug">
      <MacroExpansion>
         <BuildableReference BuildableName = "Core.framework" BlueprintName = "Core"/>
      </MacroExpansion>
   </LaunchAction>"#,
        );
        let launch = parse(&text).unwrap().unwrap();
        assert!(!launch.is_runnable());
        assert_eq!(launch.build_configuration.as_deref(), Some("Debug"));

        let empty = parse(&scheme(r#"<LaunchAction buildConfiguration = "Debug"/>"#)).unwrap();
        assert_eq!(empty.map(|l| l.is_runnable()), Some(false));
    }

    #[test]
    fn only_the_first_launch_action_counts() {
        let text = scheme(
            r#"<LaunchAction buildConfiguration = "Debug"/>
   <LaunchAction buildConfiguration = "Release">
      <EnvironmentVariables>
         <EnvironmentVariable key = "LATE" value = "1" isEnabled = "YES"/>
      </EnvironmentVariables>
   </LaunchAction>"#,
        );
        let launch = parse(&text).unwrap().unwrap();
        assert_eq!(launch.build_configuration.as_deref(), Some("Debug"));
        assert!(launch.environment.is_empty());
    }

    #[test]
    fn rejects_files_that_are_not_well_formed_schemes() {
        for (text, needle) in [
            (
                "<Workspace version = \"1.0\"></Workspace>",
                "root element is <Workspace>",
            ),
            ("<?xml version=\"1.0\"?>\n", "no <Scheme> element"),
            (
                "<Scheme>\n<LaunchAction>\n</Scheme>",
                "</Scheme> at line 3 closes <LaunchAction>",
            ),
            ("<Scheme>\n<LaunchAction>", "ends inside <LaunchAction>"),
            (
                "<Scheme></Scheme>\n</Extra>",
                "</Extra> at line 2 has no start tag",
            ),
            (
                "<Scheme></Scheme><Scheme></Scheme>",
                "a second root element",
            ),
        ] {
            let err = parse(text).unwrap_err().to_string();
            assert!(err.contains(needle), "{text:?}: {err}");
        }
    }

    #[test]
    fn read_names_the_file_on_failure() {
        let dir = sandbox();
        let path = dir.join("Broken.xcscheme");
        write(&path, "<Scheme><LaunchAction>");
        let err = format!("{:#}", read(&path).unwrap_err());
        assert!(err.contains("Broken.xcscheme"), "{err}");
        assert!(err.contains("ends inside <LaunchAction>"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    // -- argument splitting -------------------------------------------------

    #[test]
    fn splits_arguments_like_xcode() {
        let cases: &[(&str, &[&str])] = &[
            ("-MyAppArg YES", &["-MyAppArg", "YES"]),
            ("  -a\t-b \n -c  ", &["-a", "-b", "-c"]),
            (
                "\"two words\" 'single quoted'",
                &["two words", "single quoted"],
            ),
            ("--name=\"My App\"", &["--name=My App"]),
            (
                "\"say \\\"hi\\\"\" 'no \\ escape'",
                &["say \"hi\"", "no \\ escape"],
            ),
            ("a\\ b c", &["a b", "c"]),
            ("\"\" x ''", &["", "x", ""]),
            ("\"unclosed quote", &["unclosed quote"]),
            ("trailing\\", &["trailing\\"]),
            ("", &[]),
            ("   ", &[]),
        ];
        for (input, want) in cases {
            assert_eq!(split_arguments(input), *want, "{input:?}");
        }
    }

    // -- macro expansion ----------------------------------------------------

    #[test]
    fn expands_both_macro_forms_from_build_settings() {
        let settings = settings(&[("SRCROOT", "/Users/Jane/My App"), ("TARGET_NAME", "MyApp")]);
        let mut warnings = Vec::new();
        assert_eq!(
            expand("$(SRCROOT)/MyApp/run.lldbinit", &settings, &mut warnings),
            "/Users/Jane/My App/MyApp/run.lldbinit"
        );
        assert_eq!(
            expand("${TARGET_NAME}-$(TARGET_NAME)", &settings, &mut warnings),
            "MyApp-MyApp"
        );
        assert_eq!(
            expand("no macros, $5 and $ alone", &settings, &mut warnings),
            "no macros, $5 and $ alone"
        );
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn replacement_values_are_not_expanded_again() {
        let settings = settings(&[("A", "$(B)"), ("B", "b")]);
        let mut warnings = Vec::new();
        assert_eq!(expand("$(A)", &settings, &mut warnings), "$(B)");
        assert!(warnings.is_empty());
    }

    #[test]
    fn unknown_macros_stay_literal_with_one_warning_each() {
        let settings = settings(&[("SRCROOT", "/Users/x/MyApp")]);
        let mut warnings = Vec::new();
        assert_eq!(
            expand("$(NOPE)/$(SRCROOT)/${NOPE}", &settings, &mut warnings),
            "$(NOPE)//Users/x/MyApp/${NOPE}"
        );
        // A second value that shares the list adds nothing for NOPE.
        assert_eq!(
            expand("$(NOPE) $(OTHER)", &settings, &mut warnings),
            "$(NOPE) $(OTHER)"
        );
        assert_eq!(
            warnings,
            [
                "The scheme uses $(NOPE), which is not a build setting of its target, so it \
                 is left as written.",
                "The scheme uses $(OTHER), which is not a build setting of its target, so it \
                 is left as written.",
            ]
        );
    }

    #[test]
    fn operators_nested_and_unclosed_macros_stay_literal() {
        let settings = settings(&[("PRODUCT_NAME", "MyApp"), ("B", "x")]);
        let mut warnings = Vec::new();
        assert_eq!(
            expand("$(PRODUCT_NAME:lower)", &settings, &mut warnings),
            "$(PRODUCT_NAME:lower)"
        );
        assert_eq!(
            expand("$(A_$(B)) tail", &settings, &mut warnings),
            "$(A_$(B)) tail"
        );
        assert_eq!(
            expand("$(PRODUCT_NAME", &settings, &mut warnings),
            "$(PRODUCT_NAME"
        );
        assert_eq!(expand("${B", &settings, &mut warnings), "${B");
        // PRODUCT_NAME is a setting: the warning blames the form, not the
        // name.
        assert_eq!(
            warnings,
            [
                "The scheme uses $(PRODUCT_NAME:lower); macro operators and nested macros \
                 are not expanded, so it is left as written.",
                "The scheme uses $(A_$(B)); macro operators and nested macros are not \
                 expanded, so it is left as written.",
            ]
        );
    }

    // -- locating the file --------------------------------------------------

    #[test]
    fn a_workspace_scheme_wins_over_its_projects() {
        let root = sandbox();
        let ws = root.join("MyApp.xcworkspace");
        let project = root.join("MyApp.xcodeproj");
        write(
            &ws.join("contents.xcworkspacedata"),
            &workspace_data(r#"<FileRef location = "group:MyApp.xcodeproj"></FileRef>"#),
        );
        write(&shared(&project, "MyApp"), &marked_scheme("project"));
        assert_eq!(locate(&ws, "MyApp", None), Some(shared(&project, "MyApp")));
        write(&shared(&ws, "MyApp"), &marked_scheme("workspace"));
        let found = locate(&ws, "MyApp", None).unwrap();
        assert_eq!(mark_of(&found), "workspace");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn finds_schemes_in_every_kind_of_workspace_reference() {
        let root = sandbox();
        let elsewhere = sandbox();
        let ws = root.join("MyApp.xcworkspace");
        let refs = format!(
            r#"<FileRef location = "group:MyApp.xcodeproj"></FileRef>
   <FileRef location = "group:Packages/Feature"></FileRef>
   <FileRef location = "container:Libs/Core.xcodeproj"></FileRef>
   <Group location = "group:Modules" name = "Modules">
      <FileRef location = "group:Payments/Payments.xcodeproj"></FileRef>
      <Group location = "container:Shared" name = "Shared">
         <FileRef location = "group:Design.xcodeproj"/>
      </Group>
   </Group>
   <!-- <FileRef location = "group:Retired.xcodeproj"></FileRef> -->
   <FileRef location = "absolute:{}"></FileRef>
   <FileRef location = "developer:Tools/Unknown.xcodeproj"></FileRef>"#,
            elsewhere.join("Tools.xcodeproj").display()
        );
        write(&ws.join("contents.xcworkspacedata"), &workspace_data(&refs));

        assert_eq!(
            referenced_projects(&ws),
            vec![
                root.join("MyApp.xcodeproj"),
                root.join("Libs/Core.xcodeproj"),
                root.join("Modules/Payments/Payments.xcodeproj"),
                root.join("Shared/Design.xcodeproj"),
                elsewhere.join("Tools.xcodeproj"),
            ]
        );
        for (scheme, project) in [
            ("Core", root.join("Libs/Core.xcodeproj")),
            ("Payments", root.join("Modules/Payments/Payments.xcodeproj")),
            ("Design", root.join("Shared/Design.xcodeproj")),
            ("Tools", elsewhere.join("Tools.xcodeproj")),
        ] {
            write(&shared(&project, scheme), &marked_scheme(scheme));
            assert_eq!(
                locate(&ws, scheme, None),
                Some(shared(&project, scheme)),
                "{scheme}"
            );
        }
        // A package next to the projects is not searched.
        write(
            &shared(&root.join("Packages/Feature"), "Feature"),
            &marked_scheme("package"),
        );
        assert_eq!(locate(&ws, "Feature", None), None);
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&elsewhere);
    }

    #[test]
    fn earlier_workspace_references_win() {
        let root = sandbox();
        let ws = root.join("MyApp.xcworkspace");
        write(
            &ws.join("contents.xcworkspacedata"),
            &workspace_data(
                r#"<FileRef location = "group:First.xcodeproj"></FileRef>
   <FileRef location = "group:Second.xcodeproj"></FileRef>"#,
            ),
        );
        write(
            &shared(&root.join("Second.xcodeproj"), "MyApp"),
            &marked_scheme("second"),
        );
        write(
            &shared(&root.join("First.xcodeproj"), "MyApp"),
            &marked_scheme("first"),
        );
        assert_eq!(mark_of(&locate(&ws, "MyApp", None).unwrap()), "first");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn xcuserdata_is_searched_after_every_shared_scheme() {
        let root = sandbox();
        let ws = root.join("MyApp.xcworkspace");
        let project = root.join("MyApp.xcodeproj");
        write(
            &ws.join("contents.xcworkspacedata"),
            &workspace_data(r#"<FileRef location = "group:MyApp.xcodeproj"></FileRef>"#),
        );
        // The per-user directories are created here, at run time: the
        // repository never holds an xcuserdata directory.
        write(
            &personal(&project, "jane", "MyApp"),
            &marked_scheme("project user"),
        );
        assert_eq!(
            mark_of(&locate(&ws, "MyApp", Some("jane")).unwrap()),
            "project user"
        );
        // Another account's scheme is not ours, and no user skips the step.
        assert_eq!(locate(&ws, "MyApp", Some("alex")), None);
        assert_eq!(locate(&ws, "MyApp", None), None);

        write(
            &personal(&ws, "jane", "MyApp"),
            &marked_scheme("workspace user"),
        );
        assert_eq!(
            mark_of(&locate(&ws, "MyApp", Some("jane")).unwrap()),
            "workspace user"
        );

        // Any shared scheme, even the project's, outranks both.
        write(&shared(&project, "MyApp"), &marked_scheme("project shared"));
        assert_eq!(
            mark_of(&locate(&ws, "MyApp", Some("jane")).unwrap()),
            "project shared"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_project_container_and_its_embedded_workspace_find_the_project_scheme() {
        let root = sandbox();
        let project = root.join("MyApp.xcodeproj");
        let embedded = project.join("project.xcworkspace");
        write(
            &embedded.join("contents.xcworkspacedata"),
            &workspace_data(r#"<FileRef location = "self:"></FileRef>"#),
        );
        write(&personal(&project, "jane", "MyApp"), &marked_scheme("user"));
        assert_eq!(
            locate(&project, "MyApp", Some("jane")),
            Some(personal(&project, "jane", "MyApp"))
        );
        assert_eq!(
            locate(&embedded, "MyApp", Some("jane")),
            Some(personal(&project, "jane", "MyApp"))
        );
        write(&shared(&project, "MyApp"), &marked_scheme("shared"));
        assert_eq!(
            locate(&embedded, "MyApp", Some("jane")),
            Some(shared(&project, "MyApp"))
        );

        // A hand-typed `Project.xcworkspace` names the same directory on a
        // case-insensitive volume, so it is embedded too.
        let other = root.join("Other.xcodeproj");
        let typed = other.join("Project.xcworkspace");
        write(
            &typed.join("contents.xcworkspacedata"),
            &workspace_data(r#"<FileRef location = "self:"></FileRef>"#),
        );
        write(&shared(&other, "Other"), &marked_scheme("other"));
        assert_eq!(enclosing_project(&typed), Some(other.as_path()));
        assert_eq!(container_dir(&typed), root.as_path());
        assert_eq!(locate(&typed, "Other", None), Some(shared(&other, "Other")));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_scheme_file_gives_the_status_message() {
        let root = sandbox();
        let project = root.join("MyApp.xcodeproj");
        fs::create_dir_all(&project).unwrap();
        let found = load_for_user(&project, "MyApp (staging)", Some("jane")).unwrap();
        assert_eq!(
            found,
            SchemeLaunch::Missing {
                message: "Scheme \"MyApp (staging)\" has no .xcscheme file, so no launch \
                          environment or arguments are applied."
                    .into()
            }
        );
        assert_eq!(found.runnable(), None);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_empty_or_path_shaped_scheme_name_finds_nothing() {
        let root = sandbox();
        let project = root.join("MyApp.xcodeproj");
        // The files an unchecked name would reach: `.xcscheme` for "", and
        // one directory up for "../MyApp".
        write(&shared(&project, ""), &marked_scheme("empty name"));
        write(
            &project.join("xcshareddata/MyApp.xcscheme"),
            &marked_scheme("escaped"),
        );
        assert_eq!(locate(&project, "", None), None);
        assert_eq!(locate(&project, "../MyApp", None), None);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_found_scheme_without_a_launch_action_is_not_runnable() {
        let root = sandbox();
        let project = root.join("MyApp.xcodeproj");
        write(
            &shared(&project, "Lint"),
            &scheme(r#"<BuildAction parallelizeBuildables = "YES"></BuildAction>"#),
        );
        let found = load_for_user(&project, "Lint", None).unwrap();
        assert_eq!(
            found,
            SchemeLaunch::Found {
                path: shared(&project, "Lint"),
                launch_action: None,
            }
        );
        assert_eq!(found.runnable(), Some(false));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unreadable_workspace_file_means_no_referenced_projects() {
        let root = sandbox();
        let ws = root.join("MyApp.xcworkspace");
        write(
            &ws.join("contents.xcworkspacedata"),
            "<Workspace><FileRef location=",
        );
        assert!(referenced_projects(&ws).is_empty());
        assert!(referenced_projects(&root.join("Absent.xcworkspace")).is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_referenced_container_resolves_against_the_scheme_owner() {
        let reference = BuildableReference {
            referenced_container: Some("container:Apps/MyApp.xcodeproj".into()),
            ..BuildableReference::default()
        };
        let root = Path::new("/Users/x/Code");
        let ws_scheme = shared(&root.join("MyApp.xcworkspace"), "MyApp");
        assert_eq!(
            reference.container_path(&ws_scheme),
            Some(root.join("Apps/MyApp.xcodeproj"))
        );
        let user_scheme = personal(&root.join("Tools.xcodeproj"), "jane", "MyApp");
        assert_eq!(
            reference.container_path(&user_scheme),
            Some(root.join("Apps/MyApp.xcodeproj"))
        );
        assert_eq!(
            reference.container_path(Path::new("/Users/x/Loose.xcscheme")),
            None
        );
        assert_eq!(
            BuildableReference::default().container_path(&ws_scheme),
            None
        );
    }
}
