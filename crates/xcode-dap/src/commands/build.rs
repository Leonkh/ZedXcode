//! `xcode-dap build` — pipeline phases 1-4 only; exit code = xcodebuild's.
//! This is what `.zed/tasks.json` "Xcode: Build" (CMD+B) calls.

use std::path::PathBuf;

use tokio_util::sync::CancellationToken;

use crate::engine::config::{BuildOutput, LaunchConfig};
use crate::engine::pipeline::{self, OutputSink};
use crate::engine::xcodebuild::BuildFailed;

/// Shared build/run argument set
/// (`build --workspace --scheme [--device] [--full-output]`).
#[derive(clap::Args, Debug)]
pub struct BuildArgs {
    /// Path to .xcworkspace / .xcodeproj
    #[arg(long, short = 'w')]
    pub workspace: PathBuf,
    /// Xcode scheme, e.g. "MyApp (staging)"
    #[arg(long, short = 's')]
    pub scheme: String,
    /// Simulator device name or UDID (default: the booted iPhone, else the newest available iPhone)
    #[arg(long)]
    pub device: Option<String>,
    /// Simulator OS version, e.g. "26.3"
    #[arg(long)]
    pub os: Option<String>,
    /// Build configuration (Debug/Release); default: scheme's Run config
    #[arg(long)]
    pub configuration: Option<String>,
    /// DerivedData directory (xcodebuild -derivedDataPath); default: xcodebuild's per-workspace location
    #[arg(long)]
    pub derived_data: Option<PathBuf>,
    /// Disable build-log filtering (stream full xcodebuild output)
    #[arg(long)]
    pub full_output: bool,
    /// Hidden (testing): pump OSLog (`log stream`) into the console
    /// (`run` only; the supported path is `"oslog": true` in .zed/debug.json)
    #[arg(long, hide = true)]
    pub oslog: bool,
    /// Hidden (testing): custom NSPredicate for the OSLog pump (`run` only;
    /// the supported path is `"oslogPredicate"` in .zed/debug.json)
    #[arg(long, hide = true)]
    pub oslog_predicate: Option<String>,
}

impl BuildArgs {
    pub(crate) fn to_config(&self) -> LaunchConfig {
        LaunchConfig {
            workspace: self.workspace.clone(),
            scheme: self.scheme.clone(),
            device: self.device.clone(),
            os: self.os.clone(),
            configuration: self.configuration.clone(),
            preflight: None,
            oslog: self.oslog,
            oslog_predicate: self.oslog_predicate.clone(),
            terminate_on_stop: true,
            build_output: if self.full_output {
                BuildOutput::Full
            } else {
                BuildOutput::Filtered
            },
            verbose_logging: false,
            derived_data: self.derived_data.clone(),
        }
    }
}

/// CLI `OutputSink`: app stdout to stdout, everything else to stderr.
pub(crate) struct CliSink;

impl OutputSink for CliSink {
    fn line(&self, category: &str, text: &str) {
        if category == "stdout" {
            println!("{text}");
        } else {
            eprintln!("{text}");
        }
    }
}

/// Map a pipeline error to the process exit: xcodebuild failures exit with
/// xcodebuild's own code; everything else bubbles up as anyhow (exit 1).
pub(crate) fn exit_with_build_code(err: anyhow::Error) -> anyhow::Result<()> {
    if let Some(failed) = err.downcast_ref::<BuildFailed>() {
        eprintln!("{failed}");
        std::process::exit(failed.code);
    }
    Err(err)
}

/// Spawn a task that cancels `token` on Ctrl-C (SIGINT).
pub(crate) fn cancel_on_ctrl_c(token: CancellationToken) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            token.cancel();
        }
    });
}

pub async fn run(args: BuildArgs) -> anyhow::Result<()> {
    let cfg = args.to_config();
    let cancel = CancellationToken::new();
    cancel_on_ctrl_c(cancel.clone());
    match pipeline::run_build(&cfg, &CliSink, cancel).await {
        Ok((_udid, app)) => {
            eprintln!("Build succeeded: {}", app.display());
            Ok(())
        }
        Err(err) => exit_with_build_code(err),
    }
}
