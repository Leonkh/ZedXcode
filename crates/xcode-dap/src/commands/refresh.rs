//! `xcode-dap refresh` — re-run the preflight from `.zed/debug.json`
//! (project regen, e.g. Tuist), regenerate buildServer.json, print the
//! "restart LSP" hint.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::engine::selection;
use crate::setup::build_server::{regenerate, Regen};
use crate::setup::jsonc;

/// Expand a `$ZED_WORKTREE_ROOT`-templated path from `.zed/debug.json`
/// against the project dir. Setup writes the value verbatim; a hand-editor
/// may prefix it with `$ZED_WORKTREE_ROOT/` (or embed the token). Strips a
/// leading `$ZED_WORKTREE_ROOT/` or substitutes the token, then anchors any
/// still-relative result to `project` — never the cwd, which `select-scheme`
/// can be run from a subdirectory of.
pub(crate) fn expand_worktree_root(value: &str, project: &Path) -> PathBuf {
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

pub async fn run() -> Result<()> {
    let dir = std::env::current_dir()?;
    let debug_json = dir.join(".zed").join("debug.json");
    if !debug_json.exists() {
        bail!(
            "no .zed/debug.json in {} — run `xcode-dap setup --project .` first",
            dir.display()
        );
    }
    let text = std::fs::read_to_string(&debug_json)
        .with_context(|| format!("reading {}", debug_json.display()))?;
    let v = jsonc::parse_jsonc(&text).context(".zed/debug.json is not valid JSONC")?;
    let scenario = v
        .as_array()
        .and_then(|a| {
            a.iter()
                .find(|s| s.get("adapter").and_then(|x| x.as_str()) == Some("Xcode"))
        })
        .context("no \"Xcode\" scenario found in .zed/debug.json")?;

    let dir_str = dir.to_string_lossy();
    let workspace = scenario.get("workspace").and_then(|w| w.as_str()).map(|w| {
        w.strip_prefix("$ZED_WORKTREE_ROOT/")
            .map(str::to_owned)
            .unwrap_or_else(|| w.replace("$ZED_WORKTREE_ROOT", &dir_str))
    });
    // The runtime selection overlay (select-scheme) wins over debug.json —
    // regenerate buildServer.json for the scheme actually being run.
    let scheme = selection::load(&dir).scheme.or_else(|| {
        scenario
            .get("scheme")
            .and_then(|s| s.as_str())
            .map(str::to_owned)
    });
    // DerivedData from the scenario threads into the regenerated build_root
    // (setup writes it verbatim; a hand-editor may add $ZED_WORKTREE_ROOT).
    let derived_data = scenario
        .get("derivedData")
        .and_then(|d| d.as_str())
        .map(|d| expand_worktree_root(d, &dir));

    // 1. preflight (project regen, e.g. `make project CI=true` for Tuist projects)
    if let Some(preflight) = scenario.get("preflight").and_then(|p| p.as_str()) {
        println!("→ preflight: {preflight}");
        let status = tokio::process::Command::new("/bin/sh")
            .args(["-c", preflight])
            .current_dir(&dir)
            .status()
            .await
            .context("failed to spawn the preflight command")?;
        if !status.success() {
            bail!("preflight `{preflight}` failed ({status})");
        }
    } else {
        println!("– no preflight configured; skipping project regeneration");
    }

    // 2. regenerate buildServer.json
    match (workspace.as_deref(), scheme.as_deref()) {
        (Some(ws), Some(scheme)) => {
            let dd = derived_data.as_deref();
            match regenerate(&dir, Path::new(ws), scheme, None, dd).await {
                Regen::Written(_) => println!("✓ buildServer.json refreshed"),
                Regen::MissingWorkspace => println!(
                    "! workspace {ws} does not exist yet — run `xcode-dap refresh` \
                     after the project is generated"
                ),
                Regen::Failed(e) => println!("✗ buildServer.json refresh failed: {e}"),
            }
        }
        _ => println!("! scenario has no workspace/scheme — skipping buildServer.json refresh"),
    }

    // 3. LSP hint
    println!("\nIn Zed: command palette → `editor: restart language server`");
    println!("(reloads go-to-definition after the project regeneration).");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_worktree_root_anchors_to_project_not_cwd() {
        let project = Path::new("/proj");
        // Leading token is stripped and the remainder anchored to project.
        assert_eq!(
            expand_worktree_root("$ZED_WORKTREE_ROOT/dd", project),
            PathBuf::from("/proj/dd")
        );
        // Embedded token is substituted.
        assert_eq!(
            expand_worktree_root("$ZED_WORKTREE_ROOT/build/dd", project),
            PathBuf::from("/proj/build/dd")
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
}
