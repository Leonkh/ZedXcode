//! xcode-dap — DAP proxy + CLI for Xcode-like build & debug in Zed.
//!
//! With no subcommand the binary runs in DAP proxy mode (stdio), as spawned
//! by the Zed `xcode-tools` extension. Subcommands expose the same engine
//! for tasks / terminal use. See `docs/design/dap-proxy.md` §6.

mod bsp;
mod commands;
mod dap;
mod engine;
mod setup;
mod util;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "xcode-dap",
    version,
    about = "Xcode-like build & debug engine for Zed (DAP proxy + CLI)",
    long_about = "Runs as a DAP proxy on stdio when invoked without a subcommand \
                  (this is how Zed spawns it). Subcommands drive the same \
                  build/run/clean engine from tasks or a terminal."
)]
struct Cli {
    /// Hidden (testing): in DAP mode, skip xcodebuild/simctl and attach
    /// lldb-dap to a locally spawned dummy process instead.
    #[arg(long, hide = true)]
    mock_pipeline: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Build the chosen scheme for the simulator (pipeline phases 1-4 only); flags override the choices for this build only; exit code = xcodebuild's
    Build(commands::build::BuildArgs),
    /// Build, install and launch on the simulator without the debugger; console streams to the terminal; flags as for build
    Run(commands::build::BuildArgs),
    /// xcodebuild clean for the chosen scheme; flags override the choices for this clean only
    Clean(commands::clean::CleanArgs),
    /// Print/tail the current run's app console logs (~/.zedxcode/run/<udid>/{out,err}.log)
    Console(commands::console::ConsoleArgs),
    /// Choose the scheme to build/run (interactive, or --set/--list/--reset; saved in .zed/.zedx/selection.json)
    SelectScheme(commands::select::SelectSchemeArgs),
    /// Choose the simulator destination (interactive, or --set/--list/--reset; saved in .zed/.zedx/selection.json)
    #[command(visible_alias = "select-destination")]
    SelectDevice(commands::select::SelectDeviceArgs),
    /// Choose the build configuration (interactive, or --set/--list/--reset; saved in .zed/.zedx/selection.json)
    SelectConfiguration(commands::select::SelectConfigurationArgs),
    /// Install the Zed user keymap block and per-project config
    Setup(commands::setup::SetupArgs),
    /// Re-run preflight (project regen) and refresh buildServer.json; prints LSP-restart hint
    Refresh,
    /// Check the environment (Xcode, lldb-dap, simulators, buildServer.json, ...)
    Doctor,
    /// Run as a sourcekit-lsp Build Server on stdio (spawned via buildServer.json)
    Bsp,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let mode_label = match &cli.command {
        None => "dap",
        Some(Command::Build(_)) => "build",
        Some(Command::Run(_)) => "run",
        Some(Command::Clean(_)) => "clean",
        Some(Command::Console(_)) => "console",
        Some(Command::SelectScheme(_)) => "select-scheme",
        Some(Command::SelectDevice(_)) => "select-device",
        Some(Command::SelectConfiguration(_)) => "select-configuration",
        Some(Command::Setup(_)) => "setup",
        Some(Command::Refresh) => "refresh",
        Some(Command::Doctor) => "doctor",
        Some(Command::Bsp) => "bsp",
    };
    util::logging::init(mode_label);
    let result = match cli.command {
        None => dap::proxy::run_dap_mode(cli.mock_pipeline).await,
        Some(Command::Build(args)) => commands::build::run(args).await,
        Some(Command::Run(args)) => commands::run::run(args).await,
        Some(Command::Clean(args)) => commands::clean::run(args).await,
        Some(Command::Console(args)) => commands::console::run(args).await,
        Some(Command::SelectScheme(args)) => commands::select::run_select_scheme(args).await,
        Some(Command::SelectDevice(args)) => commands::select::run_select_device(args).await,
        Some(Command::SelectConfiguration(args)) => {
            commands::select::run_select_configuration(args).await
        }
        Some(Command::Setup(args)) => commands::setup::run(args).await,
        Some(Command::Refresh) => commands::refresh::run().await,
        Some(Command::Doctor) => commands::doctor::run().await,
        Some(Command::Bsp) => bsp::run().await,
    };
    if let Err(err) = result {
        log::error!("exiting 1: {err:#}");
        // Human-readable error chain instead of anyhow's Debug dump
        // ("context: cause: ...", hints included by the failure sites).
        eprintln!("xcode-dap: {err:#}");
        std::process::exit(1);
    }
}
