//! `xcode-dap clean` — `xcodebuild -workspace|-project ... -scheme ... clean`
//! (the CMD+Shift+K task).

use std::path::PathBuf;

use anyhow::bail;

use crate::commands::build::{exit_with_build_code, CliSink};
use crate::engine::config::{BuildOutput, LaunchConfig};
use crate::engine::{selection, xcodebuild};

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
    let cfg = LaunchConfig {
        workspace: args.workspace,
        scheme: args.scheme,
        device: None, // unused by clean
        os: None,
        configuration: args.configuration,
        preflight: None,
        oslog: false,
        oslog_predicate: None,
        terminate_on_stop: true,
        build_output: BuildOutput::Filtered,
        verbose_logging: false,
        derived_data: args.derived_data,
    };
    // select-scheme overlay applies to clean too (cleaning the scheme the
    // user actually runs); device/os in the overlay are unused by clean.
    let cfg = selection::overlaid(&cfg, &CliSink);
    match xcodebuild::clean(&cfg).await {
        Ok(()) => {
            eprintln!("Clean succeeded");
            Ok(())
        }
        Err(err) => exit_with_build_code(err),
    }
}
