//! `xcode-dap setup [--project <dir>] [--user] [--yes] [--remove]` — user
//! keymap/settings marker blocks and per-project config. See design §6.1.

use std::io::Write;
use std::path::PathBuf;

use anyhow::bail;

use crate::setup::{project, user};

#[derive(clap::Args, Debug)]
pub struct SetupArgs {
    /// Set up a project directory (.zed/debug.json, .zed/tasks.json, buildServer.json)
    #[arg(long, value_name = "DIR")]
    pub project: Option<PathBuf>,
    /// Set up user-level Zed keymap/settings marker blocks (~/.config/zed)
    #[arg(long)]
    pub user: bool,
    /// Non-interactive: assume yes for all prompts
    #[arg(long)]
    pub yes: bool,
    /// Remove the user-level marker blocks installed by --user
    #[arg(long)]
    pub remove: bool,
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
        if args.remove {
            if confirm(
                &format!("Remove the zedxcode blocks from {}?", dir.display()),
                args.yes,
            )? {
                user::remove_user_in(&dir)?;
            }
        } else if confirm(
            &format!(
                "Install the Xcode keymap/settings blocks into {}?",
                dir.display()
            ),
            args.yes,
        )? {
            user::setup_user_in(&dir)?;
        }
    }

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
    Ok(())
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
