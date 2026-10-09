//! Well-known filesystem locations shared by the CLI and the DAP proxy.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Context;

/// `~/.zedxcode` — the tool's home directory (logs, caches, run state).
/// Created lazily by callers as needed.
pub fn zedxcode_home() -> anyhow::Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".zedxcode"))
}

/// The xcodebuild container flag for a workspace/project path: `-project`
/// for a bare `.xcodeproj`, else `-workspace`. xcodebuild rejects a bare
/// `.xcodeproj` passed via `-workspace` ("... is not a workspace file.",
/// exit 66). Extension-based dispatch keeps a path ending in
/// `.xcodeproj/project.xcworkspace` on `-workspace`.
pub fn container_flag(path: &Path) -> &'static str {
    // Case-insensitive: on the default case-insensitive macOS filesystem a
    // hand-written `.XcodeProj`/`.XCODEPROJ` path is a valid reference to the
    // bundle, so a byte-exact match would send `-workspace` and fail exit 66.
    if path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("xcodeproj"))
    {
        "-project"
    } else {
        "-workspace"
    }
}

/// Expand a `$ZED_WORKTREE_ROOT`-templated path from `.zed/debug.json`
/// against the project dir. Setup writes the value verbatim; a hand-editor
/// may prefix it with `$ZED_WORKTREE_ROOT/` (or embed the token). Strips a
/// leading `$ZED_WORKTREE_ROOT/` or substitutes the token, then anchors any
/// still-relative result to `project` — never the cwd, which a command can
/// be run from a subdirectory of.
pub fn expand_worktree_root(value: &str, project: &Path) -> PathBuf {
    let expanded = value
        .strip_prefix("$ZED_WORKTREE_ROOT/")
        .map(str::to_owned)
        .unwrap_or_else(|| value.replace("$ZED_WORKTREE_ROOT", &project.to_string_lossy()));
    let p = Path::new(&expanded);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        project.join(p)
    }
}

/// mtime of `path` (`None` when missing/unreadable).
pub fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Newest of the container dir's mtime and its contents file mtime
/// (Tuist rewrites contents.xcworkspacedata in place).
pub fn workspace_mtime(ws: &Path) -> Option<SystemTime> {
    let own = mtime(ws);
    let contents = mtime(&ws.join("contents.xcworkspacedata"));
    match (own, contents) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// Freshness decision for `buildServer.json`, shared by `doctor` and the
/// build pipeline: regenerate when the file is missing or older than the
/// workspace ([`workspace_mtime`]). An unknown workspace mtime never forces
/// a regen — only a missing buildServer.json does.
pub fn buildserver_stale(build_server: Option<SystemTime>, workspace: Option<SystemTime>) -> bool {
    match (build_server, workspace) {
        (None, _) => true,
        (Some(bs), Some(ws)) => bs < ws,
        (Some(_), None) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn expand_worktree_root_anchors_to_project_not_cwd() {
        let project = Path::new("/proj");
        // Leading token is stripped and the remainder anchored to project.
        assert_eq!(
            expand_worktree_root("$ZED_WORKTREE_ROOT/dd", project),
            PathBuf::from("/proj/dd")
        );
        // A token anywhere else is substituted.
        assert_eq!(
            expand_worktree_root("/cache$ZED_WORKTREE_ROOT/dd", project),
            PathBuf::from("/cache/proj/dd")
        );
        assert_eq!(
            expand_worktree_root("$ZED_WORKTREE_ROOT", project),
            PathBuf::from("/proj")
        );
        // A plain relative value anchors to project (not the cwd).
        assert_eq!(
            expand_worktree_root("dd", project),
            PathBuf::from("/proj/dd")
        );
        // An absolute value is used verbatim.
        assert_eq!(
            expand_worktree_root("/abs/dd", project),
            PathBuf::from("/abs/dd")
        );
    }

    #[test]
    fn container_flag_dispatches_on_extension() {
        assert_eq!(container_flag(Path::new("MyApp.xcworkspace")), "-workspace");
        assert_eq!(container_flag(Path::new("MyApp.xcodeproj")), "-project");
        assert_eq!(
            container_flag(Path::new("/abs/My App.xcodeproj")),
            "-project"
        );
        // The inner workspace of a project container is still a workspace.
        assert_eq!(
            container_flag(Path::new("MyApp.xcodeproj/project.xcworkspace")),
            "-workspace"
        );
    }

    #[test]
    fn container_flag_extension_is_case_insensitive() {
        // Non-canonical extension case still resolves to the real bundle on a
        // case-insensitive volume, so it must map to -project, not exit 66.
        assert_eq!(container_flag(Path::new("MyApp.XcodeProj")), "-project");
        assert_eq!(container_flag(Path::new("MyApp.XCODEPROJ")), "-project");
        assert_eq!(container_flag(Path::new("MyApp.XcWorkspace")), "-workspace");
    }

    #[test]
    fn container_flag_ignores_trailing_slash() {
        // Shell tab-completion appends a slash (.xcodeproj is a directory);
        // Path::extension() ignores it.
        assert_eq!(container_flag(Path::new("MyApp.xcodeproj/")), "-project");
    }

    #[test]
    fn buildserver_stale_decision() {
        let now = SystemTime::now();
        let older = now - Duration::from_secs(60);
        // Missing buildServer.json is always stale.
        assert!(buildserver_stale(None, Some(now)));
        assert!(buildserver_stale(None, None));
        // Older than the workspace -> stale; same age or newer -> fresh.
        assert!(buildserver_stale(Some(older), Some(now)));
        assert!(!buildserver_stale(Some(now), Some(now)));
        assert!(!buildserver_stale(Some(now), Some(older)));
        // Unknown workspace mtime never forces a regen.
        assert!(!buildserver_stale(Some(now), None));
    }

    #[test]
    fn workspace_mtime_tracks_in_place_contents_rewrite() {
        let ws = std::env::temp_dir().join(format!(
            "zedxcode-paths-test-{}.xcworkspace",
            std::process::id()
        ));
        std::fs::create_dir_all(&ws).unwrap();
        let dir_only = workspace_mtime(&ws).unwrap();
        // Tuist regenerates contents.xcworkspacedata in place without
        // bumping the directory mtime — the newer contents mtime must win.
        let contents = ws.join("contents.xcworkspacedata");
        std::fs::write(&contents, "<Workspace/>").unwrap();
        let future = SystemTime::now() + Duration::from_secs(120);
        std::fs::File::options()
            .write(true)
            .open(&contents)
            .unwrap()
            .set_modified(future)
            .unwrap();
        assert!(workspace_mtime(&ws).unwrap() > dir_only);
        // Missing container -> None.
        assert_eq!(workspace_mtime(Path::new("/nonexistent.xcworkspace")), None);
    }
}
