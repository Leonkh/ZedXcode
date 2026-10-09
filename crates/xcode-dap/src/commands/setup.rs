//! `xcode-dap setup [--project <dir>] [--user [--remove] [--dry-run]
//! [--relocate | --replace]] [--yes]` — the user keymap marker block (and
//! retiring 0.1's settings block) and per-project config, then a list of
//! the tasks that reuse the Xcode task labels. See design §6.1.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::bail;

use crate::setup::jsonc::OnEdited;
use crate::setup::task_collisions::{self, Level};
use crate::setup::{project, user};

#[derive(clap::Args, Debug)]
pub struct SetupArgs {
    /// Set up a project directory (.zed/debug.json, .zed/tasks.json, buildServer.json)
    #[arg(long, value_name = "DIR")]
    pub project: Option<PathBuf>,
    /// Set up the user-level Zed keymap marker block (~/.config/zed)
    #[arg(long)]
    pub user: bool,
    /// Non-interactive: assume yes for all prompts
    #[arg(long)]
    pub yes: bool,
    /// Remove the user-level marker block installed by --user (only while it
    /// is unedited, unless --relocate or --replace says otherwise)
    #[arg(long)]
    pub remove: bool,
    /// With --user: show each marker block's before/after and the entries
    /// ZedXcode did not write; write nothing
    #[arg(long)]
    pub dry_run: bool,
    /// With --user: for a marker block edited by hand, move the entries
    /// ZedXcode did not write out of it, verbatim, then update (or remove) it
    #[arg(long, conflicts_with = "replace")]
    pub relocate: bool,
    /// With --user: overwrite (or remove) a marker block edited by hand,
    /// entries ZedXcode did not write included; a backup is kept
    #[arg(long)]
    pub replace: bool,
    /// Workspace/project file (skips auto-detection), e.g. MyApp.xcworkspace
    #[arg(long)]
    pub workspace: Option<PathBuf>,
    /// Xcode scheme (skips auto-detection), e.g. "MyApp (staging)"
    #[arg(long)]
    pub scheme: Option<String>,
    /// Simulator device name or UDID (skips auto-detection)
    #[arg(long)]
    pub device: Option<String>,
    /// Simulator OS version, e.g. "26.3"
    #[arg(long)]
    pub os: Option<String>,
    /// Preflight command for a missing workspace (auto-detected as
    /// "make project CI=true" when the Makefile has a `project:` target)
    #[arg(long)]
    pub preflight: Option<String>,
    /// Pump OSLog (`log stream`) into the Debug Console ("oslog": true in
    /// debug.json). Without the flag, an existing file's value is preserved.
    #[arg(long)]
    pub oslog: bool,
    /// DerivedData directory for build/run (xcodebuild -derivedDataPath),
    /// written as "derivedData" into the generated .zed/debug.json
    #[arg(long)]
    pub derived_data: Option<PathBuf>,
}

pub async fn run(args: SetupArgs) -> anyhow::Result<()> {
    if !args.user && args.project.is_none() {
        bail!("nothing to do: pass --user and/or --project <dir> (see `xcode-dap setup --help`)");
    }
    if args.remove && args.project.is_some() {
        bail!("--remove only applies to --user marker blocks; delete the project's .zed/ files manually");
    }
    if (args.dry_run || args.relocate || args.replace) && !args.user {
        bail!("--dry-run, --relocate and --replace apply only to --user");
    }
    if args.dry_run && args.project.is_some() {
        bail!("--dry-run applies only to --user; run it without --project");
    }
    // Warn (do not error: the flags were always accepted) when project-only
    // flags are passed without --project — they would be silently ignored.
    let project_flags_present = args.workspace.is_some()
        || args.scheme.is_some()
        || args.device.is_some()
        || args.os.is_some()
        || args.preflight.is_some()
        || args.oslog
        || args.derived_data.is_some();
    if args.project.is_none() && project_flags_present {
        eprintln!(
            "warning: --workspace/--scheme/--device/--os/--preflight/--oslog/--derived-data \
             apply only together with --project <dir>; ignoring them"
        );
    }

    if args.user {
        let dir = user::zed_config_dir()?;
        let opts = user::UserOptions {
            on_edited: if args.relocate {
                OnEdited::Relocate
            } else if args.replace {
                OnEdited::Replace
            } else {
                OnEdited::Keep
            },
            dry_run: args.dry_run,
        };
        let yes = args.yes;
        let mut ask = move |prompt: &str| confirm(prompt, yes);
        // A dry run writes nothing, so it needs no confirmation.
        if args.remove {
            if args.dry_run
                || confirm(
                    &format!("Remove the zedxcode blocks from {}?", dir.display()),
                    args.yes,
                )?
            {
                user::remove_user_in(&dir, opts, &mut ask)?;
            }
        } else if args.dry_run
            || confirm(
                &format!("Install the Xcode keymap block into {}?", dir.display()),
                args.yes,
            )?
        {
            user::setup_user_in(&dir, opts, &mut ask)?;
        }
    }

    // The scan only reads, so a dry run and a declined prompt get it too;
    // after --remove no ZedXcode key spawns the labels any more.
    let scan_project = args
        .project
        .as_deref()
        .map(|dir| dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf()));
    let scan_tasks = !args.remove;

    if let Some(dir) = args.project {
        if confirm(
            &format!("Write .zed config into {}?", dir.display()),
            args.yes,
        )? {
            let flags = project::ProjectFlags {
                workspace: args.workspace,
                scheme: args.scheme,
                device: args.device,
                os: args.os,
                preflight: args.preflight,
                oslog: args.oslog,
                derived_data: args.derived_data,
            };
            project::setup_project(&dir, flags).await?;
        }
    }

    if scan_tasks {
        let zed_config_dir = user::zed_config_dir().ok();
        print_task_collisions(scan_project.as_deref(), zed_config_dir.as_deref());
    }
    Ok(())
}

/// Every task that reuses an `Xcode: …` label without running xcode-dap:
/// in the project's `.zed/tasks.json` and `.vscode/tasks.json` files (with
/// `--project`) and the user's Zed `tasks.json`. A label a key spawns by
/// name (⌘B, ⇧⌘K) is a warning, any other `Xcode: …` label a note; nothing
/// is printed when there are none.
fn print_task_collisions(project: Option<&Path>, zed_config_dir: Option<&Path>) {
    let findings = task_collisions::scan(project, zed_config_dir);
    if findings.is_empty() {
        return;
    }
    println!();
    for finding in findings {
        let mark = match finding.level {
            Level::Warn => '!',
            Level::Note => '–',
        };
        let path = task_collisions::printable(&finding.path.display().to_string());
        println!("{mark} {path}: {}", finding.text);
    }
}

fn confirm(prompt: &str, yes: bool) -> anyhow::Result<bool> {
    if yes {
        return Ok(true);
    }
    print!("{prompt} [y/N]: ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let accepted = matches!(line.trim(), "y" | "Y" | "yes" | "YES");
    if !accepted {
        println!("skipped.");
    }
    Ok(accepted)
}
