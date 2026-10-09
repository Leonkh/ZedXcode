//! `xcode-dap build` — pipeline phases 1-4 only; exit code = xcodebuild's.
//! This is what `.zed/tasks.json` "Xcode: Build" (CMD+B) calls.

use std::path::PathBuf;

use anyhow::Context;
use tokio_util::sync::CancellationToken;

use crate::engine::pipeline::{self, OutputSink};
use crate::engine::project;
use crate::engine::selection::{CliFlags, Request};
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
    /// The request for this invocation, at the project root found from the
    /// current directory. The project's selection store outranks these
    /// flags, as 0.1's overlay did.
    pub(crate) fn to_request(&self) -> anyhow::Result<Request> {
        Ok(Request::for_cli(
            cli_root()?,
            CliFlags {
                workspace: Some(absolute(&self.workspace)),
                scheme: Some(self.scheme.clone()),
                device: self.device.clone(),
                os: self.os.clone(),
                configuration: self.configuration.clone(),
                derived_data: self.derived_data.clone(),
                full_output: self.full_output,
                oslog: self.oslog,
                oslog_predicate: self.oslog_predicate.clone(),
            },
        ))
    }
}

/// The project root for a command run from a terminal or a task.
pub(crate) fn cli_root() -> anyhow::Result<PathBuf> {
    let cwd = std::env::current_dir().context("reading the current directory")?;
    let zed_worktree_root = std::env::var_os("ZED_WORKTREE_ROOT").map(PathBuf::from);
    Ok(project::root_for_cli(&cwd, zed_worktree_root.as_deref()))
}

/// `path` made absolute against the current directory (flags are relative
/// to it, not to the project root).
pub(crate) fn absolute(path: &std::path::Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
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
    let req = args.to_request()?;
    let cancel = CancellationToken::new();
    cancel_on_ctrl_c(cancel.clone());
    match pipeline::run_build(&req, &CliSink, cancel).await {
        Ok((_udid, app)) => {
            eprintln!("Build succeeded: {}", app.display());
            Ok(())
        }
        Err(err) => exit_with_build_code(err),
    }
}
