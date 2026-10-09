//! `xcode-dap refresh` — re-run the preflight from `.zed/debug.json`
//! (project regen, e.g. Tuist), regenerate buildServer.json, print the
//! "restart LSP" hint.

use anyhow::{bail, Context, Result};

use crate::commands::build::{cli_root, CliSink};
use crate::engine::{schemes, selection};
use crate::setup::build_server::{regenerate, Regen};
use crate::setup::jsonc;
use crate::util::paths::expand_worktree_root;

pub async fn run() -> Result<()> {
    let dir = cli_root()?;
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

    // The scheme a build would use: the selection store (or a linked
    // worktree's main checkout store) over the scenario's key.
    let picks = selection::current(&dir);
    for warning in &picks.warnings {
        println!("! {warning}");
    }
    let picked_scheme = picks.scheme.map(|s| s.value);
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

    // 2. regenerate buildServer.json, for the container the build uses (the
    // scenario's "workspace", else the one found in the project) and its
    // scheme (the picked one, else the container's only scheme).
    let workspace = match selection::cli_container(&dir, None) {
        Ok(ws) => ws,
        Err(e) => {
            println!("! {e:#} — skipping buildServer.json refresh");
            return finish();
        }
    };
    let scheme = match picked_scheme {
        Some(scheme) => Some(scheme),
        None if workspace.exists() => match schemes::list(&workspace, &CliSink).await {
            Ok(list) => match selection::settle_scheme(None, &list, &workspace) {
                Ok(scheme) => Some(scheme.value),
                Err(e) => {
                    println!("! {e:#}");
                    None
                }
            },
            Err(e) => {
                println!("! {e:#}");
                None
            }
        },
        None => None,
    };
    match scheme {
        Some(scheme) => {
            let dd = derived_data.as_deref();
            match regenerate(&dir, &workspace, &scheme, None, dd).await {
                Regen::Written(_) => println!("✓ buildServer.json refreshed"),
                Regen::MissingWorkspace => println!(
                    "! workspace {} does not exist yet — run `xcode-dap refresh` \
                     after the project is generated",
                    workspace.display()
                ),
                Regen::Failed(e) => println!("✗ buildServer.json refresh failed: {e}"),
            }
        }
        None => println!("! no scheme to build — skipping buildServer.json refresh"),
    }
    finish()
}

/// 3. The language-server hint.
fn finish() -> Result<()> {
    println!("\nIn Zed: command palette → `editor: restart language server`");
    println!("(reloads go-to-definition after the project regeneration).");
    Ok(())
}
