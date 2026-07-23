//! Pure-Rust `buildServer.json` generator. sourcekit-lsp reads this file to
//! spawn our own `xcode-dap bsp` Build Server: `argv` points at the running
//! `xcode-dap` binary + the `bsp` subcommand, and our private
//! `workspace`/`build_root`/`scheme`/`kind` fields are read back by
//! [`crate::bsp`] and `doctor`.
//!
//! `name`/`version`/`bspVersion`/`languages`/`argv` are all non-optional in
//! sourcekit-lsp's `buildServer.json` decoder — every one must be present.
//!
//! The write is atomic ([`jsonc::atomic_write`]) and the [`Outcome`] reports
//! whether the file was created, changed, or byte-identical so callers can
//! decide about the git-exclude first-create and the `editor: restart
//! language server` hint.

use std::path::Path;

use anyhow::Result;
use serde_json::json;

use crate::engine::xcodebuild;
use crate::setup::jsonc;
use crate::setup::project::current_exe_path;

/// Render the `buildServer.json` text for `(workspace, scheme, build_root)`.
/// `workspace` and `build_root` are canonicalised to absolute paths; `argv[0]`
/// is the canonicalised running binary ([`current_exe_path`]). Serialisation
/// is deterministic (serde_json key order is stable) so the byte-compare in
/// [`write_build_server_json`] is meaningful.
pub fn render(workspace: &Path, scheme: &str, build_root: &Path) -> String {
    let ws = std::path::absolute(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let br = std::path::absolute(build_root).unwrap_or_else(|_| build_root.to_path_buf());
    let value = json!({
        "name": "xcode-dap",
        "version": env!("CARGO_PKG_VERSION"),
        "bspVersion": "2.2.0",
        "languages": ["c", "cpp", "objective-c", "objective-cpp", "swift"],
        "argv": [current_exe_path(), "bsp"],
        "workspace": ws.to_string_lossy().into_owned(),
        "build_root": br.to_string_lossy().into_owned(),
        "scheme": scheme,
        "kind": "xcode",
    });
    let mut text =
        serde_json::to_string_pretty(&value).expect("buildServer.json always serializes");
    text.push('\n');
    text
}

/// Whether the write created, changed, or left the file byte-identical.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// The file did not exist before.
    Created,
    /// The file existed and its bytes changed.
    Changed,
    /// The file existed and the bytes were byte-identical.
    Unchanged,
}

/// Result of [`write_build_server_json`].
pub struct Outcome {
    pub change: Change,
    /// Whether a completed regen warrants an `editor: restart language server`
    /// hint — i.e. the parsed `argv` or `build_root` differ from the previous
    /// file (or it was a first-create / unreadable). A scheme-only change is
    /// `false` (bsp reloads buildServer.json on its mtime change and pushes
    /// `buildTarget/didChange`, so sourcekit-lsp re-queries without a restart).
    pub restart_hint: bool,
}

impl Outcome {
    /// The file did not exist before this write (callers first-create the
    /// `.git/info/exclude` entry only then).
    pub fn first_create(&self) -> bool {
        self.change == Change::Created
    }
}

/// (Re)write `<dir>/buildServer.json` for `(workspace, scheme, build_root)`.
///
/// Always writes (even on byte-identical content) so the file's mtime stays
/// fresh relative to the workspace — that mtime is the staleness gate; an
/// identical rewrite only costs bsp a cheap re-read (`build_root`/`scheme`
/// unchanged ⇒ no store reload). The [`Change`] is computed from the
/// pre-write bytes.
pub fn write_build_server_json(
    dir: &Path,
    workspace: &Path,
    scheme: &str,
    build_root: &Path,
) -> Result<Outcome> {
    let path = dir.join("buildServer.json");
    let content = render(workspace, scheme, build_root);
    let previous = std::fs::read(&path).ok();
    let change = match &previous {
        None => Change::Created,
        Some(p) if p.as_slice() == content.as_bytes() => Change::Unchanged,
        Some(_) => Change::Changed,
    };
    // Provenance guard: a pre-existing buildServer.json written by a *different*
    // build server (e.g. xcode-build-server, with its own argv) is not ours to
    // discard silently — back it up before the overwrite. Ours (or an empty
    // stub) is left to be replaced in place, as before.
    if let Some(bytes) = previous.as_deref() {
        if is_foreign_build_server(bytes) {
            match jsonc::backup_file(&path, &String::from_utf8_lossy(bytes)) {
                Ok(backup) => log::info!(
                    target: "setup",
                    "backed up foreign buildServer.json to {} before regeneration",
                    backup.display()
                ),
                Err(e) => log::warn!(
                    target: "setup",
                    "could not back up foreign buildServer.json before overwrite: {e:#}"
                ),
            }
        }
    }
    jsonc::atomic_write(&path, &content)?;
    let restart_hint = restart_hint_needed(previous.as_deref(), Some(content.as_bytes()));
    Ok(Outcome {
        change,
        restart_hint,
    })
}

/// Whether a completed regen warrants an `editor: restart language server`
/// hint: only when the parsed `argv` or `build_root` changed (a first-create
/// or an unreadable previous file counts). A scheme-only change does not —
/// bsp reloads on the mtime change and pushes `buildTarget/didChange`.
pub fn restart_hint_needed(previous: Option<&[u8]>, current: Option<&[u8]>) -> bool {
    match (previous, current) {
        (Some(p), Some(c)) => restart_relevant(p) != restart_relevant(c),
        _ => true,
    }
}

/// `(argv, build_root)` — the subset of fields whose change requires a
/// sourcekit-lsp restart. `scheme` is deliberately excluded.
fn restart_relevant(bytes: &[u8]) -> (Option<Vec<String>>, Option<String>) {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return (None, None);
    };
    let argv = v.get("argv").and_then(|a| a.as_array()).map(|a| {
        a.iter()
            .filter_map(|x| x.as_str().map(str::to_owned))
            .collect()
    });
    let build_root = v
        .get("build_root")
        .and_then(|b| b.as_str())
        .map(str::to_owned);
    (argv, build_root)
}

/// Whether an existing buildServer.json belongs to a *different* build server
/// (so its config must be preserved before we overwrite). Ours is identified
/// by `name: "xcode-dap"` or `kind: "xcode"` (the private fields bsp/doctor
/// read back). A file that doesn't parse, or a bare stub with no `name`, is
/// not treated as foreign — there is nothing another tool authored to lose.
fn is_foreign_build_server(bytes: &[u8]) -> bool {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return false;
    };
    let ours = v.get("name").and_then(|n| n.as_str()) == Some("xcode-dap")
        || v.get("kind").and_then(|k| k.as_str()) == Some("xcode");
    !ours && v.get("name").and_then(|n| n.as_str()).is_some()
}

/// Outcome of the CLI regeneration helper [`regenerate`].
pub enum Regen {
    /// buildServer.json was written; the [`Outcome`] carries the change/hint.
    Written(Outcome),
    /// The workspace/project container does not exist yet.
    MissingWorkspace,
    /// build_root resolution or the write failed (message included).
    Failed(String),
}

/// Resolve the build_root without a prior build ([`xcodebuild::resolve_build_root`])
/// and (re)write buildServer.json — the shared path for `setup`, `refresh`,
/// and `select-scheme`. `workspace` may be relative to `dir` or absolute;
/// buildServer.json is written to `dir`. Never fatal — these CLI commands must
/// not die on this step (setup's last step, refresh/select's follow-up).
pub async fn regenerate(
    dir: &Path,
    workspace: &Path,
    scheme: &str,
    configuration: Option<&str>,
    derived_data: Option<&Path>,
) -> Regen {
    let ws_abs = if workspace.is_absolute() {
        workspace.to_path_buf()
    } else {
        dir.join(workspace)
    };
    if !ws_abs.exists() {
        return Regen::MissingWorkspace;
    }
    let build_root =
        match xcodebuild::resolve_build_root(&ws_abs, scheme, configuration, derived_data).await {
            Ok(br) => br,
            Err(e) => return Regen::Failed(format!("{e:#}")),
        };
    match write_build_server_json(dir, &ws_abs, scheme, &build_root) {
        Ok(o) => Regen::Written(o),
        Err(e) => Regen::Failed(format!("{e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn sandbox() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zedxcode-buildserver-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn render_has_required_and_private_fields() {
        let text = render(
            Path::new("/Users/x/MyApp.xcworkspace"),
            "MyApp (staging)",
            Path::new("/Users/x/dd"),
        );
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        // sourcekit-lsp's required fields
        assert_eq!(v["name"], "xcode-dap");
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(v["bspVersion"], "2.2.0");
        assert_eq!(
            v["languages"],
            serde_json::json!(["c", "cpp", "objective-c", "objective-cpp", "swift"])
        );
        // argv = [<canonicalized current exe>, "bsp"]
        let argv: Vec<&str> = v["argv"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap())
            .collect();
        assert_eq!(argv.len(), 2);
        assert_eq!(argv[0], current_exe_path());
        assert_eq!(argv[1], "bsp");
        // our private fields
        assert_eq!(v["workspace"], "/Users/x/MyApp.xcworkspace");
        assert_eq!(v["build_root"], "/Users/x/dd");
        assert_eq!(v["scheme"], "MyApp (staging)");
        assert_eq!(v["kind"], "xcode");
    }

    #[test]
    fn write_reports_created_then_unchanged_then_changed() {
        let dir = sandbox();
        let ws = Path::new("/Users/x/MyApp.xcworkspace");

        // first write -> Created, restart hint (navigation came online)
        let o = write_build_server_json(dir.as_path(), ws, "MyApp", Path::new("/dd")).unwrap();
        assert_eq!(o.change, Change::Created);
        assert!(o.restart_hint);
        assert!(dir.join("buildServer.json").exists());

        // identical -> Unchanged, no hint
        let o = write_build_server_json(dir.as_path(), ws, "MyApp", Path::new("/dd")).unwrap();
        assert_eq!(o.change, Change::Unchanged);
        assert!(!o.restart_hint);

        // scheme-only change -> Changed but NO hint (bsp reloads + didChange)
        let o = write_build_server_json(dir.as_path(), ws, "Other", Path::new("/dd")).unwrap();
        assert_eq!(o.change, Change::Changed);
        assert!(
            !o.restart_hint,
            "scheme-only change must not prompt a restart"
        );

        // build_root change -> Changed AND hint
        let o = write_build_server_json(dir.as_path(), ws, "Other", Path::new("/dd2")).unwrap();
        assert_eq!(o.change, Change::Changed);
        assert!(o.restart_hint, "build_root change must prompt a restart");

        // the file always parses as JSON
        let text = fs::read_to_string(dir.join("buildServer.json")).unwrap();
        serde_json::from_str::<serde_json::Value>(&text).unwrap();
    }

    #[test]
    fn foreign_build_server_is_backed_up_before_overwrite() {
        let dir = sandbox();
        let path = dir.join("buildServer.json");
        // A different build server's config (e.g. xcode-build-server).
        let foreign = r#"{"name":"xcode-build-server","argv":["xcode-build-server"]}"#;
        fs::write(&path, foreign).unwrap();

        let o = write_build_server_json(
            dir.as_path(),
            Path::new("/Users/x/MyApp.xcworkspace"),
            "MyApp",
            Path::new("/dd"),
        )
        .unwrap();
        assert_eq!(o.change, Change::Changed);

        // The foreign config is preserved in a backup alongside the new file.
        let backups: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| {
                let p = e.unwrap().path();
                let n = p.file_name().unwrap().to_string_lossy().into_owned();
                n.starts_with("buildServer.json.zedxcode-backup-")
                    .then_some(p)
            })
            .collect();
        assert_eq!(backups.len(), 1, "foreign file must be backed up");
        assert_eq!(fs::read_to_string(&backups[0]).unwrap(), foreign);
        // The new file is ours.
        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["name"], "xcode-dap");

        // Rewriting over our own file makes no further backup.
        write_build_server_json(
            dir.as_path(),
            Path::new("/Users/x/MyApp.xcworkspace"),
            "Other",
            Path::new("/dd"),
        )
        .unwrap();
        let backups = fs::read_dir(&dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".zedxcode-backup-")
            })
            .count();
        assert_eq!(backups, 1, "our own overwrite must not back up");
    }

    #[test]
    fn is_foreign_build_server_classifies_provenance() {
        assert!(is_foreign_build_server(br#"{"name":"xcode-build-server"}"#));
        // ours by name or by kind -> not foreign
        assert!(!is_foreign_build_server(br#"{"name":"xcode-dap"}"#));
        assert!(!is_foreign_build_server(
            br#"{"name":"whatever","kind":"xcode"}"#
        ));
        // bare stub / unparseable -> nothing to preserve
        assert!(!is_foreign_build_server(b"{}"));
        assert!(!is_foreign_build_server(b"not json"));
    }

    #[test]
    fn restart_hint_only_when_argv_or_build_root_change() {
        let base = render(Path::new("/w.xcworkspace"), "A", Path::new("/dd"));
        let scheme_only = render(Path::new("/w.xcworkspace"), "B", Path::new("/dd"));
        let build_root_changed = render(Path::new("/w.xcworkspace"), "A", Path::new("/dd2"));

        // first-create / unreadable -> hint
        assert!(restart_hint_needed(None, Some(base.as_bytes())));
        assert!(restart_hint_needed(Some(base.as_bytes()), None));
        // byte-identical -> no hint
        assert!(!restart_hint_needed(
            Some(base.as_bytes()),
            Some(base.as_bytes())
        ));
        // scheme-only diff -> no hint
        assert!(!restart_hint_needed(
            Some(base.as_bytes()),
            Some(scheme_only.as_bytes())
        ));
        // build_root diff -> hint
        assert!(restart_hint_needed(
            Some(base.as_bytes()),
            Some(build_root_changed.as_bytes())
        ));
        // argv diff (different binary) -> hint
        let argv_changed = base.replace("\"bsp\"", "\"bsp\",\"--x\"");
        assert!(restart_hint_needed(
            Some(base.as_bytes()),
            Some(argv_changed.as_bytes())
        ));
    }
}
