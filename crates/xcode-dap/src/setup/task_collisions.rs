//! Tasks that reuse ZedXcode's `Xcode: …` labels without running xcode-dap.
//!
//! The keymap block binds ⌘B and ⇧⌘K to `task::Spawn` by label, and Zed
//! runs every task that carries the label: a `.zed/tasks.json` or
//! `.vscode/tasks.json` in any folder of the project counts as much as the
//! user's own `tasks.json`. A cloned repository can therefore put any command
//! behind those keys, and Zed's Restricted Mode does not stop tasks. Doctor
//! and setup list every such task: the two labels a key spawns as warnings,
//! the rest as notes.

use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::setup::jsonc;
use crate::setup::project::shell_quote;

/// Every task label ZedXcode writes starts with this.
const LABEL_PREFIX: &str = "Xcode: ";

/// The labels a key of setup's keymap block spawns by name, with that key.
const KEYED_LABELS: [(&str, &str); 2] = [("Xcode: Build", "⌘B"), ("Xcode: Clean", "⇧⌘K")];

/// The shim's path under the home directory, in the forms the shell expands.
const SHIM_PATHS: [&str; 3] = [
    "~/.zedxcode/bin/xcode-dap",
    "$HOME/.zedxcode/bin/xcode-dap",
    "${HOME}/.zedxcode/bin/xcode-dap",
];

/// Keys of a Zed task that change how its terminal looks or when it runs,
/// never which program runs. `env` (when empty) and `shell` (when "system")
/// are checked on their own; any other key makes the task not ours.
const INERT_ZED_KEYS: [&str; 13] = [
    "label",
    "command",
    "args",
    "cwd",
    "use_new_terminal",
    "allow_concurrent_runs",
    "reveal",
    "reveal_target",
    "hide",
    "tags",
    "show_summary",
    "show_command",
    "save",
];

/// Longest label or command line quoted in a message; the rest is cut with
/// `…`.
const MAX_QUOTED_CHARS: usize = 200;

/// Largest tasks file read; a bigger one is reported as unchecked.
const MAX_FILE_BYTES: u64 = 1 << 20;

/// The two task file formats Zed reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TasksFormat {
    /// `.zed/tasks.json` and the user's `tasks.json`: an array of tasks.
    Zed,
    /// `.vscode/tasks.json`: `{"version": …, "tasks": [...]}`.
    VsCode,
}

/// Whose file a task is in; it picks the wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope<'a> {
    /// A `.zed/tasks.json` or `.vscode/tasks.json` of the project. `root` is
    /// the repository folder (the project folder outside git): an xcode-dap
    /// binary inside it came with the project. `vscode_ignored` is set when
    /// the project has a `.zed/tasks.json` with tasks, because Zed then reads
    /// no `.vscode/tasks.json`.
    Project {
        root: &'a Path,
        vscode_ignored: bool,
    },
    /// The user's Zed `tasks.json`, which every project sees.
    User,
}

/// A task labelled `Xcode: …` whose command line is not xcode-dap's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignTask {
    /// The label as a message can show it (see [`printable`]).
    pub label: String,
    /// The command line as the file writes it: the command, then the args.
    pub command: String,
    /// The key that spawns this label by name, when one does: pressing it
    /// runs this task too.
    pub key: Option<&'static str>,
}

/// How doctor and setup print a finding: `!` (a key runs the task) or `–`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Warn,
    Note,
}

/// One line of doctor's or setup's output about a tasks file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub level: Level,
    pub path: PathBuf,
    pub text: String,
    /// The file does not parse as JSONC (doctor already reports that for
    /// the project's `.zed/tasks.json`).
    pub parse_error: bool,
}

/// Read the tasks files that can define an `Xcode: …` task — the project's
/// (see [`project_task_files`]) when `project` is given, the user's
/// `tasks.json` in `zed_config_dir` when given — and report every task that
/// reuses one of the labels. A missing file reports nothing. Project paths
/// are reported under the canonical form of `project`.
pub fn scan(project: Option<&Path>, zed_config_dir: Option<&Path>) -> Vec<Finding> {
    let mut findings = vec![];
    if let Some(dir) = project {
        let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        let repo = repository_root(&dir);
        let root = repo.as_deref().unwrap_or(&dir);
        let files: Vec<_> = project_task_files(&dir, repo.as_deref())
            .into_iter()
            .filter_map(|(path, format)| Some((read_tasks_file(&path)?, path, format)))
            .collect();
        // Zed drops every `.vscode` task source once the folder it opened
        // has a `.zed` task source.
        let vscode_ignored = files.iter().any(|(text, path, format)| {
            *format == TasksFormat::Zed
                && path.starts_with(&dir)
                && text
                    .as_deref()
                    .ok()
                    .and_then(|text| jsonc::parse_jsonc(text).ok())
                    .is_some_and(|v| v.as_array().is_some_and(|tasks| !tasks.is_empty()))
        });
        let scope = Scope::Project {
            root,
            vscode_ignored,
        };
        for (text, path, format) in files {
            findings.extend(checked(&path, text, format, scope));
        }
    }
    if let Some(dir) = zed_config_dir {
        let path = dir.join("tasks.json");
        if let Some(text) = read_tasks_file(&path) {
            findings.extend(checked(&path, text, TasksFormat::Zed, Scope::User));
        }
    }
    findings
}

/// [`findings_in`] for a file that was read, or the note that it could not
/// be.
fn checked(
    path: &Path,
    text: Result<String, &str>,
    format: TasksFormat,
    scope: Scope,
) -> Vec<Finding> {
    match text {
        Ok(text) => findings_in(path, &text, format, scope),
        Err(reason) => vec![Finding {
            level: Level::Note,
            path: path.to_path_buf(),
            text: format!("{reason}, so its \"Xcode: …\" tasks cannot be checked"),
            parse_error: false,
        }],
    }
}

/// The project's tasks files that Zed reads, root first: `.zed/tasks.json`
/// and `.vscode/tasks.json` in `dir`, in every folder below it, and in the
/// folders above it up to the repository root (they count when Zed opens one
/// of those folders). Git lists the nested ones (tracked files and untracked
/// ones it does not ignore); outside a repository only `dir`'s own are
/// checked.
fn project_task_files(dir: &Path, repo: Option<&Path>) -> Vec<(PathBuf, TasksFormat)> {
    let mut files = vec![
        (dir.join(".zed").join("tasks.json"), TasksFormat::Zed),
        (dir.join(".vscode").join("tasks.json"), TasksFormat::VsCode),
    ];
    let Some(repo) = repo else {
        return files;
    };
    let Some(out) = git(
        dir,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
            "--full-name",
            "--",
            ":(top,glob)**/.zed/tasks.json",
            ":(top,glob)**/.vscode/tasks.json",
        ],
    ) else {
        return files;
    };
    let mut nested: Vec<(PathBuf, TasksFormat)> = out
        .split(|b| *b == 0)
        .filter_map(|name| std::str::from_utf8(name).ok())
        .filter(|name| !name.is_empty())
        .filter_map(|name| {
            let path = repo.join(name);
            let format = if name.ends_with(".vscode/tasks.json") {
                TasksFormat::VsCode
            } else {
                TasksFormat::Zed
            };
            let folder = path.parent()?.parent()?;
            (folder.starts_with(dir) || dir.starts_with(folder)).then_some((path, format))
        })
        .filter(|file| !files.contains(file))
        .collect();
    nested.sort_by(|a, b| a.0.cmp(&b.0));
    nested.dedup_by(|a, b| a.0 == b.0);
    files.extend(nested);
    files
}

/// The repository root that holds `dir`, when it is in one.
fn repository_root(dir: &Path) -> Option<PathBuf> {
    let out = git(dir, &["rev-parse", "--show-toplevel"])?;
    let top = PathBuf::from(String::from_utf8(out).ok()?.trim_end_matches('\n'));
    Some(top.canonicalize().unwrap_or(top))
}

/// Run git in `dir` with captured output; `None` when it fails. A
/// repository's own config cannot start an fsmonitor command here.
fn git(dir: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "core.fsmonitor=false"])
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout)
}

/// A tasks file's text: `None` when it does not exist, `Err` with the reason
/// when it is no regular file (a FIFO or `/dev/zero` would hang the read),
/// is too large or is not UTF-8.
fn read_tasks_file(path: &Path) -> Option<Result<String, &'static str>> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() {
        return Some(Err("is not a regular file"));
    }
    let mut bytes = vec![];
    fs::File::open(path)
        .ok()?
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Some(Err("is larger than 1 MiB"));
    }
    Some(String::from_utf8(bytes).map_err(|_| "is not UTF-8 text"))
}

/// [`scan`] for one file's text: a warning per task a key spawns, a note per
/// other `Xcode: …` task, and a note when the file does not parse.
pub fn findings_in(path: &Path, text: &str, format: TasksFormat, scope: Scope) -> Vec<Finding> {
    let Ok(v) = jsonc::parse_jsonc(text) else {
        return vec![Finding {
            level: Level::Note,
            path: path.to_path_buf(),
            text: "does not parse as JSONC, so its \"Xcode: …\" tasks cannot be checked"
                .to_string(),
            parse_error: true,
        }];
    };
    let project_root = match scope {
        Scope::Project { root, .. } => Some(root),
        Scope::User => None,
    };
    let vscode_ignored = format == TasksFormat::VsCode
        && matches!(
            scope,
            Scope::Project {
                vscode_ignored: true,
                ..
            }
        );
    foreign_tasks(&v, format, project_root)
        .into_iter()
        .map(|task| Finding {
            level: match task.key {
                Some(_) if !vscode_ignored => Level::Warn,
                _ => Level::Note,
            },
            path: path.to_path_buf(),
            text: message(&task, scope, vscode_ignored),
            parse_error: false,
        })
        .collect()
}

/// Every task in a parsed tasks file that carries an `Xcode: …` label but
/// does not run xcode-dap (see [`runs_xcode_dap`]). `project_root` is
/// [`Scope::Project`]'s `root`, `None` for the user's own file.
pub fn foreign_tasks(
    v: &Value,
    format: TasksFormat,
    project_root: Option<&Path>,
) -> Vec<ForeignTask> {
    let tasks = match format {
        TasksFormat::Zed => v.as_array(),
        TasksFormat::VsCode => v.get("tasks").and_then(Value::as_array),
    };
    tasks
        .into_iter()
        .flatten()
        .filter_map(|task| {
            let label = label(task, format)?;
            if !label.starts_with(LABEL_PREFIX) || runs_xcode_dap(task, format, project_root) {
                return None;
            }
            Some(ForeignTask {
                label: printable(label),
                command: command_line(task, format),
                key: KEYED_LABELS
                    .iter()
                    .find(|(keyed, _)| *keyed == label)
                    .map(|(_, key)| *key),
            })
        })
        .collect()
}

/// A task's label; Zed names an unlabelled VS Code shell task after its
/// command.
fn label(task: &Value, format: TasksFormat) -> Option<&str> {
    match task.get("label") {
        Some(label) => label.as_str(),
        None if format == TasksFormat::VsCode
            && task.get("type").and_then(Value::as_str) == Some("shell") =>
        {
            task.get("command")?.as_str()
        }
        None => None,
    }
}

/// Does this task run xcode-dap and nothing else? Its command must be an
/// xcode-dap binary from outside the project ([`is_xcode_dap_command`]) and
/// every argument a word the shell cannot turn into a second command
/// ([`shell_inert`]): Zed joins the command and its args into one shell line,
/// so an argument such as `build; ./other.sh` would run another program
/// behind our label. Nothing else in the task may change which program runs:
/// no environment (`PATH`, `HOME` or `ZDOTDIR` pick another binary or shell
/// start-up file), no shell of its own, no key Zed might act on that
/// [`INERT_ZED_KEYS`] does not list, and in a VS Code file the `shell` type
/// (an `npm` or `gulp` task runs npm or gulp, whatever its `command` says).
fn runs_xcode_dap(task: &Value, format: TasksFormat, project_root: Option<&Path>) -> bool {
    let Some(fields) = task.as_object() else {
        return false;
    };
    let no_env = |env: Option<&Value>| {
        env.is_none_or(|env| env.as_object().is_some_and(serde_json::Map::is_empty))
    };
    let settings_ok = match format {
        TasksFormat::Zed => fields.iter().all(|(key, value)| match key.as_str() {
            "env" => no_env(Some(value)),
            "shell" => value.as_str() == Some("system"),
            key => INERT_ZED_KEYS.contains(&key),
        }),
        TasksFormat::VsCode => {
            fields.get("type").and_then(Value::as_str) == Some("shell")
                && no_env(fields.get("options").and_then(|options| options.get("env")))
        }
    };
    let command_ok = fields
        .get("command")
        .and_then(word)
        .is_some_and(|command| is_xcode_dap_command(command, format, project_root));
    let args_ok = match fields.get("args") {
        None => true,
        Some(args) => args.as_array().is_some_and(|a| {
            a.iter()
                .all(|arg| word(arg).is_some_and(|arg| shell_inert(arg, format)))
        }),
    };
    settings_ok && command_ok && args_ok
}

/// Does `command` name an installed xcode-dap binary? Setup writes an
/// absolute path (single-quoted when it holds a space or an apostrophe); a
/// bare `xcode-dap` resolves on PATH; the shim may be written
/// `~/.zedxcode/bin/xcode-dap` or `$HOME/.zedxcode/bin/xcode-dap`. A relative
/// path (`./xcode-dap`, `$ZED_WORKTREE_ROOT/xcode-dap`) or an absolute one
/// inside `project_root` points into the project, so it is not ours.
pub fn is_xcode_dap_command(
    command: &str,
    format: TasksFormat,
    project_root: Option<&Path>,
) -> bool {
    // Only blanks: a newline would end the command and start another one.
    let command = command.trim_matches(|c| c == ' ' || c == '\t');
    if SHIM_PATHS.contains(&command) {
        return true;
    }
    if expands_variable(command, format) {
        return false;
    }
    let path = match single_quoted(command) {
        // Inside quotes the shell expands neither `~` nor `$HOME`.
        Some(path) => path,
        None if is_plain_word(command) => command.to_owned(),
        None => return false,
    };
    path == "xcode-dap" || is_installed_xcode_dap(Path::new(&path), project_root)
}

/// Is `path` an absolute path to an `xcode-dap` that did not come with the
/// project? One under `project_root` was put there by whoever wrote the
/// project.
fn is_installed_xcode_dap(path: &Path, project_root: Option<&Path>) -> bool {
    if !path.is_absolute() || path.file_name().is_none_or(|name| name != "xcode-dap") {
        return false;
    }
    // `..` could lead from outside the project back into it.
    if path.components().any(|c| c == Component::ParentDir) {
        return false;
    }
    let Some(root) = project_root else {
        return true;
    };
    let resolved = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    !path.starts_with(root) && !resolved.starts_with(root)
}

/// Does Zed put text of its own into `s` before the shell reads it, quoted
/// or not? It fills in `$ZED_…` and `${ZED_…}` with values such as a file
/// name or the selected text, and in a VS Code file it first turns
/// `${file}`, `$workspaceFolder`, `${env:…}` and the like into variables.
fn expands_variable(s: &str, format: TasksFormat) -> bool {
    match format {
        TasksFormat::Zed => s.contains("$ZED_") || s.contains("${ZED_"),
        TasksFormat::VsCode => s.contains('$'),
    }
}

/// A word [`shell_quote`] leaves unquoted: the shell reads it literally.
fn is_plain_word(s: &str) -> bool {
    shell_quote(s) == s
}

/// The literal text of one single-quoted word in [`shell_quote`]'s form
/// (`'…'`, an apostrophe written `'\''`); `None` for anything else.
fn single_quoted(s: &str) -> Option<String> {
    let inner = s.strip_prefix('\'')?.strip_suffix('\'')?;
    (!inner.replace(r"'\''", "").contains('\'')).then(|| inner.replace(r"'\''", "'"))
}

/// Can the shell read `word` only as one literal word? Plain characters,
/// single-quoted runs and the `\'` between them (setup's escape for an
/// apostrophe) are inert. Anything else — `;`, `|`, `&`, `$`, backquotes,
/// double quotes, globs, a newline — is not, and neither is a variable
/// Zed fills in, even inside quotes (see [`expands_variable`]).
fn shell_inert(word: &str, format: TasksFormat) -> bool {
    if expands_variable(word, format) {
        return false;
    }
    let mut rest = word;
    while let Some(c) = rest.chars().next() {
        let len = match c {
            '\'' => match rest[1..].find('\'') {
                Some(end) => end + 2,
                None => return false,
            },
            '\\' if rest[1..].starts_with('\'') => 2,
            c if is_plain_word(c.encode_utf8(&mut [0; 4])) => c.len_utf8(),
            _ => return false,
        };
        rest = &rest[len..];
    }
    true
}

/// A command or argument: a string, or VS Code's `{"value": "…"}` form.
fn word(v: &Value) -> Option<&str> {
    v.as_str()
        .or_else(|| v.get("value").and_then(Value::as_str))
}

/// The command line a message quotes: the command and its args as the file
/// writes them, VS Code's `npm` and `gulp` tasks as the command Zed runs for
/// them, made [`printable`].
fn command_line(task: &Value, format: TasksFormat) -> String {
    let field = |key: &str| task.get(key).and_then(Value::as_str);
    let vscode_type = match format {
        TasksFormat::VsCode => field("type"),
        TasksFormat::Zed => None,
    };
    let command = match (vscode_type, field("script"), field("task")) {
        (Some("npm"), Some(script), _) => format!("npm run {script}"),
        (Some("gulp"), _, Some(gulp_task)) => format!("gulp {gulp_task}"),
        _ => task
            .get("command")
            .and_then(word)
            .unwrap_or_default()
            .to_owned(),
    };
    let args = task
        .get("args")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|arg| match word(arg) {
            Some(arg) => arg.to_owned(),
            None => arg.to_string(),
        });
    let line = std::iter::once(command)
        .chain(args)
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if line.is_empty() {
        return "none".to_string();
    }
    printable(&line)
}

/// Text from a tasks file as a message can show it: control characters and
/// the invisible ones that reorder or hide text (bidi controls, zero-width
/// characters) become spaces, and the length is capped. A repository cannot
/// move the cursor or recolour the rest of doctor's output this way.
pub fn printable(text: &str) -> String {
    let hidden = |c: char| {
        c.is_control()
            || matches!(
                c,
                '\u{200B}'..='\u{200F}'
                    | '\u{2028}'..='\u{202E}'
                    | '\u{2060}'..='\u{2069}'
                    | '\u{FEFF}'
            )
    };
    let mut shown: String = text
        .chars()
        .map(|c| if hidden(c) { ' ' } else { c })
        .take(MAX_QUOTED_CHARS)
        .collect();
    if text.chars().count() > MAX_QUOTED_CHARS {
        shown.push('…');
    }
    shown
}

/// The warning (a key spawns the label) or note for one foreign task.
/// `vscode_ignored`: the task is in a `.vscode/tasks.json` that Zed does not
/// read while the project has a `.zed/tasks.json`.
fn message(task: &ForeignTask, scope: Scope, vscode_ignored: bool) -> String {
    let (label, command) = (&task.label, &task.command);
    let defines = match scope {
        Scope::Project { .. } => "This project defines",
        Scope::User => "Your Zed tasks.json defines",
    };
    match (task.key, scope) {
        (Some(key), Scope::Project { .. }) if vscode_ignored => format!(
            "{defines} a task \"{label}\" that ZedXcode did not write (command: {command}). \
             Zed ignores .vscode/tasks.json while the project has a .zed/tasks.json, so \
             {key} does not run it now, but it would without that file."
        ),
        (Some(key), Scope::Project { .. }) => format!(
            "{defines} a task \"{label}\" that ZedXcode did not write (command: {command}). \
             {key} runs every task with that label, and Zed's Restricted Mode does not stop \
             tasks. Remove the task, or do not press {key} in this project."
        ),
        (Some(key), Scope::User) => format!(
            "{defines} a task \"{label}\" that ZedXcode did not write (command: {command}). \
             {key} runs every task with that label, in every project. Remove the task, or do \
             not press {key}."
        ),
        (None, _) => format!(
            "{defines} a task \"{label}\" that ZedXcode did not write (command: {command}). \
             No ZedXcode key runs it."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::project::{render_tasks_json, ProjectConfig};
    use serde_json::json;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A fresh, canonical temp dir (scan reports canonical paths, and on
    /// macOS the temp dir sits behind the /var symlink).
    fn sandbox() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zedxcode-task-collisions-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    /// A config whose every value setup has to quote, with each optional
    /// field set, so the rendered tasks carry every argument shape.
    fn config() -> ProjectConfig {
        ProjectConfig {
            workspace: "My App.xcworkspace".into(),
            scheme: "MyApp (staging)".into(),
            device: "iPad Air 11-inch (M3)".into(),
            os: Some("18.6".into()),
            preflight: None,
            oslog: false,
            derived_data: Some("/Users/Jane/Library/Developer/Xcode/DerivedData/O'Hara".into()),
        }
    }

    const ROOT: &str = "/Users/x/Developer/MyApp";

    fn project_scope() -> Scope<'static> {
        Scope::Project {
            root: Path::new(ROOT),
            vscode_ignored: false,
        }
    }

    fn zed_tasks(tasks: Value) -> Vec<ForeignTask> {
        foreign_tasks(&tasks, TasksFormat::Zed, Some(Path::new(ROOT)))
    }

    fn vscode_tasks(tasks: Value) -> Vec<ForeignTask> {
        let file = json!({ "version": "2.0.0", "tasks": tasks });
        foreign_tasks(&file, TasksFormat::VsCode, Some(Path::new(ROOT)))
    }

    fn foreign(label: &str, command: &str) -> ForeignTask {
        ForeignTask {
            label: label.into(),
            command: command.into(),
            key: KEYED_LABELS
                .iter()
                .find(|(keyed, _)| *keyed == label)
                .map(|(_, key)| *key),
        }
    }

    fn git_init(dir: &Path) {
        let ok = Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir)
            .status()
            .is_ok_and(|s| s.success());
        assert!(ok, "git init failed in sandbox");
    }

    #[test]
    fn setup_generated_tasks_are_never_flagged() {
        for bin in [
            "/opt/homebrew/bin/xcode-dap",
            "/Users/Jane/Dev Tools/xcode-dap",
            "/Users/x/O'Brien/xcode-dap",
            "/Users/x/.zedxcode/bin/xcode-dap",
            "xcode-dap",
        ] {
            let text = render_tasks_json(&config(), bin);
            let v = jsonc::parse_jsonc(&text).unwrap();
            assert_eq!(v.as_array().unwrap().len(), 6);
            assert_eq!(zed_tasks(v), vec![], "{bin}");
            assert_eq!(
                findings_in(
                    Path::new("tasks.json"),
                    &text,
                    TasksFormat::Zed,
                    project_scope()
                ),
                vec![]
            );
        }
    }

    #[test]
    fn xcode_dap_commands_are_recognised() {
        let root = Some(Path::new(ROOT));
        for ours in [
            "xcode-dap",
            "/opt/homebrew/bin/xcode-dap",
            "/Users/x/.zedxcode/bin/xcode-dap",
            "/Users/x/Developer/MyAppTools/xcode-dap",
            "~/.zedxcode/bin/xcode-dap",
            "$HOME/.zedxcode/bin/xcode-dap",
            "${HOME}/.zedxcode/bin/xcode-dap",
            "'/Users/Jane/Dev Tools/xcode-dap'",
            r"'/Users/x/O'\''Brien/xcode-dap'",
            "'xcode-dap'",
            " /opt/homebrew/bin/xcode-dap\t",
        ] {
            assert!(is_xcode_dap_command(ours, TasksFormat::Zed, root), "{ours}");
        }
        for foreign in [
            "./build.sh",
            "./xcode-dap",
            "bin/xcode-dap",
            "$ZED_WORKTREE_ROOT/xcode-dap",
            "'/$ZED_WORKTREE_ROOT/xcode-dap'",
            "'/${ZED_WORKTREE_ROOT}/xcode-dap'",
            "/opt/xcode-dap-wrapper",
            "/opt/xcode-dap/run.sh",
            "xcode-dap build",
            "xcode-dap && ./build.sh",
            "/opt/bin/xcode-dap; ./build.sh",
            "/tmp/x; ./build.sh /xcode-dap",
            "$(./build.sh)/xcode-dap",
            "'~/.zedxcode/bin/xcode-dap'",
            "'./xcode-dap'",
            "'/opt/bin/xcode-dap'; ./build.sh '/xcode-dap'",
            "/Users/Jane/Dev Tools/xcode-dap",
            // A newline ends the command; what follows runs next.
            "/opt/homebrew/bin/xcode-dap\n",
            "xcode-dap\n",
            "\nxcode-dap",
            // Only the shim counts under the home directory.
            "~/Developer/MyApp/xcode-dap",
            "$HOME/Developer/MyApp/xcode-dap",
            "${HOME}/xcode-dap",
            // A binary that came with the project.
            "/Users/x/Developer/MyApp/xcode-dap",
            "/Users/x/Developer/MyApp/tools/xcode-dap",
            "'/Users/x/Developer/MyApp/tools/xcode-dap'",
            "/opt/../Users/x/Developer/MyApp/xcode-dap",
            "",
        ] {
            assert!(
                !is_xcode_dap_command(foreign, TasksFormat::Zed, root),
                "{foreign:?}"
            );
        }
        // A VS Code file turns `${workspaceFolder}` into `$ZED_WORKTREE_ROOT`.
        for (command, ours) in [
            ("'/${workspaceFolder}/xcode-dap'", false),
            ("'/$workspaceFolder/xcode-dap'", false),
            ("'/Users/x/my$tools/xcode-dap'", false),
            ("$HOME/.zedxcode/bin/xcode-dap", true),
        ] {
            assert_eq!(
                is_xcode_dap_command(command, TasksFormat::VsCode, root),
                ours,
                "{command}"
            );
        }
        assert!(is_xcode_dap_command(
            "'/Users/x/my$tools/xcode-dap'",
            TasksFormat::Zed,
            root
        ));
        // In the user's own tasks.json there is no project to come with.
        assert!(is_xcode_dap_command(
            "/Users/x/Developer/MyApp/tools/xcode-dap",
            TasksFormat::Zed,
            None
        ));
    }

    #[test]
    fn arguments_that_chain_another_command_are_not_ours() {
        let bin = "/opt/homebrew/bin/xcode-dap";
        let task = |args: Value| {
            zed_tasks(json!([{ "label": "Xcode: Build", "command": bin, "args": args }]))
        };
        for inert in [
            json!(["build", "--scheme", "'MyApp (staging)'"]),
            json!(["clean", "--derived-data", r"'/Users/x/O'\''Hara/DD'"]),
            json!(["build", "--scheme", "'My$App'"]),
            json!([]),
        ] {
            assert_eq!(task(inert.clone()), vec![], "{inert}");
        }
        for chained in [
            json!(["build;", "./build.sh"]),
            json!(["build", "&&", "./build.sh"]),
            json!(["build", "|", "sh"]),
            json!(["build\n./build.sh"]),
            json!(["$(./build.sh)"]),
            json!(["`./build.sh`"]),
            json!(["\"$(./build.sh)\""]),
            json!(["$HOME/x"]),
            // Zed puts the raw value in, then the shell parses it.
            json!(["build", "$ZED_SYMBOL"]),
            json!(["build", "'$ZED_FILE'"]),
            json!(["--derived-data", "$ZED_WORKTREE_ROOT/DerivedData"]),
            json!(["--derived-data", "${ZED_WORKTREE_ROOT}/DD"]),
            json!(["'unterminated"]),
            json!([{ "not": "a word" }]),
            json!("build"),
        ] {
            assert_eq!(task(chained.clone()).len(), 1, "{chained}");
        }
        // A command that ends in a newline makes the first argument a second
        // command.
        let found = zed_tasks(json!([
            { "label": "Xcode: Build", "command": format!("{bin}\n"), "args": ["./build.sh"] },
            { "label": "Xcode: Clean", "command": "xcode-dap\n", "args": ["sh", "build.sh"] },
        ]));
        assert_eq!(found.len(), 2);
        // VS Code variables become Zed's, inside quotes too.
        for args in [
            json!(["build", "'${file}'"]),
            json!(["build", "'$selectedText'"]),
        ] {
            let found = vscode_tasks(json!([
                { "label": "Xcode: Build", "type": "shell", "command": bin, "args": args }
            ]));
            assert_eq!(found.len(), 1, "{args}");
        }
    }

    #[test]
    fn task_settings_that_change_what_runs_are_not_ours() {
        let bin = "/opt/homebrew/bin/xcode-dap";
        let build = |extra: Value| {
            let mut task = json!({ "label": "Xcode: Build", "command": bin, "args": ["build"] });
            task.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            zed_tasks(json!([task]))
        };
        for inert in [
            json!({ "cwd": "$ZED_WORKTREE_ROOT" }),
            json!({ "env": {} }),
            json!({ "shell": "system" }),
            json!({ "use_new_terminal": true, "allow_concurrent_runs": false, "reveal": "never",
                    "reveal_target": "center", "hide": "on_success", "tags": ["xcode"],
                    "show_summary": false, "show_command": false, "save": "all" }),
        ] {
            assert_eq!(build(inert.clone()), vec![], "{inert}");
        }
        for changes_what_runs in [
            json!({ "env": { "ZDOTDIR": "$ZED_WORKTREE_ROOT/.z" } }),
            json!({ "env": { "PATH": "/Users/x/Developer/MyApp/bin:/usr/bin:/bin" } }),
            json!({ "env": { "HOME": "$ZED_WORKTREE_ROOT" } }),
            json!({ "shell": { "program": "./build.sh" } }),
            json!({ "shell": { "with_arguments": { "program": "/bin/sh", "args": ["-c", "./build.sh", "--"] } } }),
            json!({ "hooks": ["create_worktree"] }),
            json!({ "some_future_key": true }),
        ] {
            assert_eq!(
                build(changes_what_runs.clone()).len(),
                1,
                "{changes_what_runs}"
            );
        }
        // The shim path with HOME pointed into the project.
        let shim = zed_tasks(json!([{ "label": "Xcode: Clean",
            "command": "~/.zedxcode/bin/xcode-dap", "env": { "HOME": "$ZED_WORKTREE_ROOT" } }]));
        assert_eq!(shim.len(), 1);
    }

    #[test]
    fn keyed_labels_are_hijackable_with_the_documented_warning() {
        let found = zed_tasks(json!([
            { "label": "Xcode: Build", "command": "./build.sh" },
            { "label": "Xcode: Clean", "command": "make", "args": ["clean"] },
        ]));
        assert_eq!(
            found,
            vec![
                foreign("Xcode: Build", "./build.sh"),
                foreign("Xcode: Clean", "make clean")
            ]
        );
        assert_eq!(found[0].key, Some("⌘B"));
        assert_eq!(found[1].key, Some("⇧⌘K"));
        assert_eq!(
            message(&found[0], project_scope(), false),
            "This project defines a task \"Xcode: Build\" that ZedXcode did not write \
             (command: ./build.sh). ⌘B runs every task with that label, and Zed's Restricted \
             Mode does not stop tasks. Remove the task, or do not press ⌘B in this project."
        );
        assert_eq!(
            message(&found[1], project_scope(), false),
            "This project defines a task \"Xcode: Clean\" that ZedXcode did not write \
             (command: make clean). ⇧⌘K runs every task with that label, and Zed's Restricted \
             Mode does not stop tasks. Remove the task, or do not press ⇧⌘K in this project."
        );
        assert_eq!(
            message(&found[0], Scope::User, false),
            "Your Zed tasks.json defines a task \"Xcode: Build\" that ZedXcode did not write \
             (command: ./build.sh). ⌘B runs every task with that label, in every project. \
             Remove the task, or do not press ⌘B."
        );
    }

    #[test]
    fn other_xcode_labels_are_notes_and_other_labels_are_ignored() {
        let found = zed_tasks(json!([
            { "label": "Xcode: Archive", "command": "./archive.sh" },
            { "label": "Xcode: Refresh", "command": "/opt/homebrew/bin/xcode-dap", "args": ["refresh"] },
            { "label": "Build", "command": "./build.sh" },
            { "label": "xcode: build", "command": "./build.sh" },
            { "label": "Xcode:Build", "command": "./build.sh" },
            { "label": "Xcode: Build\t", "command": "./build.sh" },
            { "command": "./build.sh" },
            "not a task",
        ]));
        assert_eq!(
            found,
            vec![
                foreign("Xcode: Archive", "./archive.sh"),
                // Zed matches labels exactly, so no key spawns this one.
                foreign("Xcode: Build ", "./build.sh"),
            ]
        );
        assert_eq!(found[1].key, None);
        assert_eq!(
            message(&found[0], project_scope(), false),
            "This project defines a task \"Xcode: Archive\" that ZedXcode did not write \
             (command: ./archive.sh). No ZedXcode key runs it."
        );
    }

    #[test]
    fn vscode_tasks_are_read_from_the_tasks_member() {
        let text = r#"// VS Code tasks
{
  "version": "2.0.0",
  "tasks": [
    { "label": "Xcode: Build", "type": "shell", "command": "./scripts/build.sh", "args": ["--release", { "value": "My App", "quoting": "escape" }] },
    { "label": "Xcode: Clean", "type": "npm", "script": "clean" },
    { "label": "Xcode: Lint", "type": "gulp", "task": "lint" },
    { "label": "Xcode: Docs", "type": "shell", "command": { "value": "./docs.sh" } },
    { "label": "Xcode: Console", "type": "shell", "command": "xcode-dap", "args": ["console", "--follow"] },
  ],
}
"#;
        let v = jsonc::parse_jsonc(text).unwrap();
        let root = Some(Path::new(ROOT));
        assert_eq!(
            foreign_tasks(&v, TasksFormat::VsCode, root),
            vec![
                foreign("Xcode: Build", "./scripts/build.sh --release My App"),
                foreign("Xcode: Clean", "npm run clean"),
                foreign("Xcode: Lint", "gulp lint"),
                foreign("Xcode: Docs", "./docs.sh"),
            ]
        );
        // Each format reads only its own shape.
        assert_eq!(foreign_tasks(&v, TasksFormat::Zed, root), vec![]);
        let zed = json!([{ "label": "Xcode: Build", "command": "./build.sh" }]);
        assert_eq!(foreign_tasks(&zed, TasksFormat::VsCode, root), vec![]);
    }

    #[test]
    fn vscode_tasks_run_xcode_dap_only_as_plain_shell_tasks() {
        let found = vscode_tasks(json!([
            // npm runs the package.json script, whatever `command` says.
            { "label": "Xcode: Build", "type": "npm", "command": "xcode-dap", "script": "build" },
            { "label": "Xcode: Clean", "type": "shell", "command": "xcode-dap", "args": ["clean"],
              "options": { "env": { "PATH": "${workspaceFolder}/bin:/usr/bin:/bin" } } },
            // Zed names an unlabelled shell task after its command.
            { "type": "shell", "command": "Xcode: Build",
              "options": { "env": { "PATH": "${workspaceFolder}/bin:/usr/bin:/bin" } } },
            { "label": "Xcode: Refresh", "type": "shell", "command": "xcode-dap", "args": ["refresh"],
              "options": { "cwd": "${workspaceFolder}", "env": {} }, "group": "build" },
        ]));
        assert_eq!(
            found,
            vec![
                foreign("Xcode: Build", "npm run build"),
                foreign("Xcode: Clean", "xcode-dap clean"),
                foreign("Xcode: Build", "Xcode: Build"),
            ]
        );
    }

    #[test]
    fn quoted_text_stays_readable_and_inert() {
        let found = zed_tasks(json!([
            { "label": "Xcode: Build" },
            { "label": "Xcode: Clean", "command": "./clean.sh\n./other.sh" },
            { "label": "Xcode: Archive", "command": "x".repeat(MAX_QUOTED_CHARS + 10) },
            { "label": "Xcode: x\u{1b}[4A\u{1b}[J\u{1b}[8m", "command": "./x.sh" },
            { "label": "Xcode: \u{202E}hs.dliub", "command": "./a\u{2066}.sh\u{200B}" },
        ]));
        assert_eq!(found[0].command, "none");
        assert_eq!(found[1].command, "./clean.sh ./other.sh");
        assert_eq!(
            found[2].command,
            format!("{}…", "x".repeat(MAX_QUOTED_CHARS))
        );
        assert_eq!(found[3].label, "Xcode: x [4A [J [8m");
        assert_eq!(found[4].label, "Xcode:  hs.dliub");
        assert_eq!(found[4].command, "./a .sh ");
        assert!(message(&found[3], project_scope(), false)
            .chars()
            .all(|c| !c.is_control()));
        let long = format!("Xcode: {}", "y".repeat(MAX_QUOTED_CHARS));
        assert!(printable(&long).ends_with('…'));
    }

    #[test]
    fn scan_reads_the_project_files_and_the_user_tasks() {
        let project = sandbox();
        let zed_config = sandbox();
        // Nothing to read: nothing to report.
        assert_eq!(scan(Some(&project), Some(&zed_config)), vec![]);

        fs::create_dir_all(project.join(".zed")).unwrap();
        fs::create_dir_all(project.join(".vscode")).unwrap();
        let ours = render_tasks_json(&config(), "/opt/homebrew/bin/xcode-dap");
        let ours_and_build = ours.replacen(
            "[\n",
            "[\n  { \"label\": \"Xcode: Build\", \"command\": \"./build.sh\" },\n",
            1,
        );
        fs::write(project.join(".zed/tasks.json"), ours_and_build).unwrap();
        fs::write(
            project.join(".vscode/tasks.json"),
            r#"{ "version": "2.0.0", "tasks": [{ "label": "Xcode: Test", "type": "shell", "command": "./test.sh" }] }"#,
        )
        .unwrap();
        fs::write(
            zed_config.join("tasks.json"),
            r#"[{ "label": "Xcode: Clean", "command": "rm", "args": ["-rf", "build"] }]"#,
        )
        .unwrap();

        let found = scan(Some(&project), Some(&zed_config));
        let summary: Vec<(Level, PathBuf, bool)> = found
            .iter()
            .map(|f| {
                (
                    f.level,
                    f.path.clone(),
                    f.text.contains("ZedXcode did not write"),
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                (Level::Warn, project.join(".zed/tasks.json"), true),
                (Level::Note, project.join(".vscode/tasks.json"), true),
                (Level::Warn, zed_config.join("tasks.json"), true),
            ]
        );
        assert!(found[0]
            .text
            .starts_with("This project defines a task \"Xcode: Build\""));
        assert!(found[2]
            .text
            .starts_with("Your Zed tasks.json defines a task \"Xcode: Clean\""));
        assert!(found[2].text.contains("(command: rm -rf build)"));

        // Setup without --project reads only the user tasks; doctor outside
        // macOS (no Zed config dir) only the project's.
        assert_eq!(scan(None, Some(&zed_config)), found[2..].to_vec());
        assert_eq!(scan(Some(&project), None), found[..2].to_vec());

        // A file that does not parse is reported as unchecked.
        fs::write(zed_config.join("tasks.json"), "[{ \"label\": ").unwrap();
        let found = scan(None, Some(&zed_config));
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].level, Level::Note);
        assert!(found[0].parse_error);
        assert!(found[0].text.contains("cannot be checked"));
    }

    #[test]
    fn a_zed_tasks_file_turns_vscode_warnings_into_notes() {
        let project = sandbox();
        fs::create_dir_all(project.join(".vscode")).unwrap();
        fs::write(
            project.join(".vscode/tasks.json"),
            r#"{ "version": "2.0.0", "tasks": [{ "label": "Xcode: Build", "type": "shell", "command": "./build.sh" }] }"#,
        )
        .unwrap();
        let found = scan(Some(&project), None);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].level, Level::Warn);
        assert!(found[0].text.contains("Restricted Mode"));

        // An empty .zed/tasks.json gives Zed no .zed task source.
        fs::create_dir_all(project.join(".zed")).unwrap();
        fs::write(project.join(".zed/tasks.json"), "[]").unwrap();
        assert_eq!(scan(Some(&project), None)[0].level, Level::Warn);

        fs::write(
            project.join(".zed/tasks.json"),
            render_tasks_json(&config(), "xcode-dap"),
        )
        .unwrap();
        let found = scan(Some(&project), None);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].level, Level::Note);
        assert_eq!(
            found[0].text,
            "This project defines a task \"Xcode: Build\" that ZedXcode did not write \
             (command: ./build.sh). Zed ignores .vscode/tasks.json while the project has a \
             .zed/tasks.json, so ⌘B does not run it now, but it would without that file."
        );
    }

    #[test]
    fn scan_finds_tasks_files_in_nested_and_enclosing_folders() {
        let repo = sandbox();
        git_init(&repo);
        let project = repo.join("ios");
        let task = |label: &str| format!(r#"[{{ "label": "{label}", "command": "./x.sh" }}]"#);
        for (folder, label) in [
            ("ios/.zed", "Xcode: Root"),
            ("ios/Sources/App/.zed", "Xcode: Build"),
            (".zed", "Xcode: Enclosing"),
            ("android/.zed", "Xcode: Sibling"),
            ("ignored/.zed", "Xcode: Ignored"),
        ] {
            fs::create_dir_all(repo.join(folder)).unwrap();
            fs::write(repo.join(folder).join("tasks.json"), task(label)).unwrap();
        }
        fs::write(repo.join(".gitignore"), "ignored/\n").unwrap();
        // A binary that came with the repository is not xcode-dap's.
        fs::create_dir_all(project.join("Sources/.vscode")).unwrap();
        fs::write(
            project.join("Sources/.vscode/tasks.json"),
            format!(
                r#"{{ "version": "2.0.0", "tasks": [{{ "label": "Xcode: Clean", "type": "shell", "command": "{}" }}] }}"#,
                repo.join("tools/xcode-dap").display()
            ),
        )
        .unwrap();

        let found = scan(Some(&project), None);
        let paths: Vec<_> = found.iter().map(|f| f.path.clone()).collect();
        assert_eq!(
            paths,
            vec![
                project.join(".zed/tasks.json"),
                repo.join(".zed/tasks.json"),
                project.join("Sources/.vscode/tasks.json"),
                project.join("Sources/App/.zed/tasks.json"),
            ]
        );
        assert_eq!(found[3].level, Level::Warn);
        // The .zed task sources make Zed skip the .vscode one.
        assert_eq!(found[2].level, Level::Note);
        assert!(found[2].text.contains("tools/xcode-dap"));
    }

    #[test]
    fn unreadable_tasks_files_are_reported_without_hanging() {
        let project = sandbox();
        fs::create_dir_all(project.join(".zed/tasks.json")).unwrap();
        fs::create_dir_all(project.join(".vscode")).unwrap();
        std::os::unix::fs::symlink("/dev/zero", project.join(".vscode/tasks.json")).unwrap();
        let zed_config = sandbox();
        fs::write(
            zed_config.join("tasks.json"),
            " ".repeat(MAX_FILE_BYTES as usize + 1),
        )
        .unwrap();
        let found = scan(Some(&project), Some(&zed_config));
        let texts: Vec<_> = found.iter().map(|f| f.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "is not a regular file, so its \"Xcode: …\" tasks cannot be checked",
                "is not a regular file, so its \"Xcode: …\" tasks cannot be checked",
                "is larger than 1 MiB, so its \"Xcode: …\" tasks cannot be checked",
            ]
        );
        assert!(found
            .iter()
            .all(|f| f.level == Level::Note && !f.parse_error));
    }
}
