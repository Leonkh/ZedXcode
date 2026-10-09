//! `xcode-dap clean` — `xcodebuild -workspace|-project ... -scheme ... clean`
//! (the CMD+Shift+K task).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context};
use tokio_util::sync::CancellationToken;

use crate::commands::build::{
    absolute, cancel_on_ctrl_c, cli_request, cli_root, exit_with_build_code, invocation_args,
    CliSink,
};
use crate::engine::selection::{CliFlags, Request};
use crate::engine::{pipeline, xcodebuild};

/// Every flag is optional, as for `build`: a flag overrides the project's
/// choices for this invocation only.
#[derive(clap::Args, Debug)]
pub struct CleanArgs {
    /// Path to .xcworkspace / .xcodeproj (default: the Xcode scenario's
    /// "workspace", else the one found within two levels of the project root)
    #[arg(long, short = 'w')]
    pub workspace: Option<PathBuf>,
    /// Scheme for this invocation only (default: the chosen scheme, else the
    /// container's only one)
    #[arg(long, short = 's')]
    pub scheme: Option<String>,
    /// Build configuration for this invocation only (default: the chosen
    /// configuration, else the scheme's Run configuration)
    #[arg(long)]
    pub configuration: Option<String>,
    /// DerivedData directory (xcodebuild -derivedDataPath); default: xcodebuild's per-workspace location
    #[arg(long)]
    pub derived_data: Option<PathBuf>,
}

impl CleanArgs {
    /// The flags; relative paths are taken from `cwd`.
    fn cli_flags(&self, cwd: &Path) -> CliFlags {
        CliFlags {
            workspace: self.workspace.as_deref().map(|w| absolute(cwd, w)),
            scheme: self.scheme.clone(),
            configuration: self.configuration.clone(),
            derived_data: self.derived_data.clone(),
            ..Default::default()
        }
    }

    /// The request for this invocation, at `root`; `args` is the command
    /// line after the program name (a 0.1 task's keeps 0.1's precedence).
    pub(crate) fn request_at(&self, root: PathBuf, cwd: &Path, args: &[String]) -> Request {
        cli_request(root, self.cli_flags(cwd), args)
    }
}

pub async fn run(args: CleanArgs) -> anyhow::Result<()> {
    if let Some(workspace) = args.workspace.as_deref().filter(|w| !w.exists()) {
        bail!(
            "workspace {} not found — nothing to clean\nhint: check --workspace, \
             or regenerate the project (`xcode-dap refresh`)",
            workspace.display()
        );
    }
    let cwd = std::env::current_dir().context("reading the current directory")?;
    let req = args.request_at(cli_root()?, &cwd, &invocation_args());
    // The selection store applies to clean too (cleaning the scheme the user
    // actually runs); no simulator is needed.
    let cancel = CancellationToken::new();
    cancel_on_ctrl_c(cancel.clone());
    let prepared = pipeline::prepare(&req, &CliSink, &cancel, false).await?;
    let cfg = prepared.target;
    match xcodebuild::clean(&cfg).await {
        Ok(()) => {
            eprintln!("Clean succeeded");
            Ok(())
        }
        Err(err) => exit_with_build_code(err),
    }
}
