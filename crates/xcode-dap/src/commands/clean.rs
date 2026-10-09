//! `xcode-dap clean` — `xcodebuild -workspace|-project ... -scheme ... clean`
//! (the CMD+Shift+K task).

use std::path::PathBuf;

use anyhow::bail;
use tokio_util::sync::CancellationToken;

use crate::commands::build::{absolute, cancel_on_ctrl_c, cli_root, exit_with_build_code, CliSink};
use crate::engine::selection::{CliFlags, Request};
use crate::engine::{pipeline, xcodebuild};

#[derive(clap::Args, Debug)]
pub struct CleanArgs {
    /// Path to .xcworkspace / .xcodeproj
    #[arg(long, short = 'w')]
    pub workspace: PathBuf,
    /// Xcode scheme
    #[arg(long, short = 's')]
    pub scheme: String,
    /// Build configuration (Debug/Release); default: scheme's Run config
    #[arg(long)]
    pub configuration: Option<String>,
    /// DerivedData directory (xcodebuild -derivedDataPath); default: xcodebuild's per-workspace location
    #[arg(long)]
    pub derived_data: Option<PathBuf>,
}

pub async fn run(args: CleanArgs) -> anyhow::Result<()> {
    if !args.workspace.exists() {
        bail!(
            "workspace {} not found — nothing to clean\nhint: check --workspace, \
             or regenerate the project (`xcode-dap refresh`)",
            args.workspace.display()
        );
    }
    let req = Request::for_cli(
        cli_root()?,
        CliFlags {
            workspace: Some(absolute(&args.workspace)),
            scheme: Some(args.scheme),
            configuration: args.configuration,
            derived_data: args.derived_data,
            ..Default::default()
        },
    );
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
