//! Build/install/launch engine shared by DAP mode and the CLI subcommands.

// Shared with DAP mode, whose stdout carries only framed messages: no
// `print!` / `println!` (CLI output belongs in `commands/`). The lint cannot
// see a child process that inherits stdout, so every spawn reachable from DAP
// mode uses `.output()` or an explicit `Stdio` for stdout.
#![deny(clippy::print_stdout)]

pub mod compile_store;
pub mod config;
pub mod consoles;
pub mod pipeline;
pub mod project;
pub mod selection;
pub mod simctl;
pub mod xcactivitylog;
pub mod xcodebuild;
