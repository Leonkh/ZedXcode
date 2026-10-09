//! DAP layer: Content-Length framing, byte-transparent peek/passthrough,
//! the proxy state machine and the lldb-dap child.

// DAP mode's stdout carries only framed messages: no `print!` / `println!`.
// The lint cannot see a child process that inherits stdout, so every spawn
// uses `.output()` or an explicit `Stdio` for stdout.
#![deny(clippy::print_stdout)]

pub mod framing;
pub mod lldb;
pub mod peek;
pub mod proxy;
