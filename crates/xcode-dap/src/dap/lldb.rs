//! Spawn `xcrun lldb-dap` and plumb its stdio. See `docs/design/dap-proxy.md` §3.

use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

/// Handle to the lldb-dap child process (stdio transport).
pub struct LldbDap {
    pub child: Child,
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
}

impl LldbDap {
    /// Spawn `xcrun lldb-dap` with piped stdin/stdout (`kill_on_drop`).
    /// stderr is inherited: it ends up in Zed's debug-adapter log, never on
    /// the DAP stdout stream.
    pub async fn spawn() -> Result<Self> {
        let mut child = Command::new("xcrun")
            .arg("lldb-dap")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .context("failed to spawn `xcrun lldb-dap` — is Xcode installed?")?;
        let stdin = child.stdin.take().context("lldb-dap stdin not piped")?;
        let stdout = child.stdout.take().context("lldb-dap stdout not piped")?;
        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout: Some(stdout),
        })
    }

    /// Hand the child's stdin to the single-writer task (callable once).
    pub fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.stdin.take()
    }

    /// Hand the child's stdout to the reader task (callable once).
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.stdout.take()
    }
}
