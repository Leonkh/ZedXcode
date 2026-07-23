//! `xcode-dap bsp` — a stdio JSON-RPC Build Server (BSP) that sourcekit-lsp
//! spawns via `buildServer.json` to obtain per-file compiler arguments at
//! runtime.
//!
//! It speaks the modern "pull" dialect: it advertises
//! `dataKind: "sourceKit"` + `sourceKitOptionsProvider: true` at
//! `build/initialize`, then answers `textDocument/sourceKitOptions` on demand
//! from a [`CompileStore`](crate::engine::compile_store) reconstructed out of
//! Xcode's `.xcactivitylog` build logs ([`crate::engine::xcactivitylog`]).
//!
//! Wire format mirrors DAP: `Content-Length`-framed JSON (reusing
//! [`crate::dap::framing`]). As in DAP mode, **stdout carries only framed
//! JSON-RPC** — every diagnostic goes to the log file / stderr, never stdout.
//!
//! Two async halves share one [`AppState`]:
//! - [`server`] dispatches inbound requests/notifications and writes replies;
//! - [`ingest`] runs the cold-start bootstrap + poll loop that keeps the
//!   store fresh and pushes `buildTarget/didChange` when it changes.

mod ingest;
mod server;

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::SystemTime;

use anyhow::Result;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, watch, RwLock};

use crate::dap::framing::{self, DapReader};
use crate::engine::compile_store::CompileStore;

/// Entry point for `xcode-dap bsp` (spawned by sourcekit-lsp). Speaks
/// JSON-RPC over stdio until the client disconnects, then exits 0.
pub async fn run() -> Result<()> {
    let stdin = tokio::io::stdin();
    let to_client = spawn_writer(tokio::io::stdout());
    let state = AppState::new(to_client);

    let mut reader = DapReader::new(stdin);
    loop {
        match reader.next_message().await {
            Ok(Some(raw)) => server::handle_message(&state, &raw).await,
            // stdin EOF: sourcekit-lsp is gone. Exit explicitly so the poll
            // task (which never returns) does not orphan the process — cf.
            // the DAP proxy's `Ok(None) => break` at dap/proxy.rs:296 and its
            // shutdown-stall note :348-350.
            Ok(None) => {
                log::info!("bsp: stdin EOF, exiting");
                break;
            }
            Err(e) => {
                log::error!("bsp: error reading from client: {e:#}");
                eprintln!("xcode-dap: bsp: error reading from client: {e:#}");
                break;
            }
        }
    }
    std::process::exit(0);
}

/// Everything a request handler or the ingest loop needs. The mutable
/// `session` sits behind an async `RwLock`; the bootstrap gate is a `watch`
/// so `sourceKitOptions` arriving before the store is ready can wait for it.
pub(super) struct AppState {
    session: RwLock<Session>,
    /// `false` until the cold-start bootstrap has finished.
    bootstrap_tx: watch::Sender<bool>,
    bootstrap_rx: watch::Receiver<bool>,
    /// Guards against `build/initialized` firing the ingest task twice.
    ingest_started: AtomicBool,
    to_client: mpsc::UnboundedSender<serde_json::Value>,
}

/// Mutable per-session state. `root`/`build_server_path` are fixed at
/// `build/initialize`; the config triplet and `store` can change when the
/// poll loop notices `buildServer.json` was rewritten (scheme switch).
struct Session {
    /// Project root (decoded from the initialize `rootUri`).
    root: PathBuf,
    /// The raw `rootUri` string, echoed verbatim where the wire needs a URI.
    root_uri: Option<String>,
    build_server_path: PathBuf,
    build_root: Option<PathBuf>,
    scheme: Option<String>,
    #[allow(dead_code)] // read from buildServer.json for completeness; unused today
    workspace: Option<String>,
    store: Option<CompileStore>,
    /// Newest build-log filename ingested this session (the poll watermark).
    watermark: Option<String>,
    /// Last-seen `buildServer.json` mtime (poll-loop change detection).
    bs_mtime: Option<SystemTime>,
    /// Store-file mtime as bsp last wrote/loaded it. The build pipeline writes
    /// the same `(build_root, scheme)` store when it ingests a CLI build; a
    /// different mtime means an external write to adopt (poll-loop reload).
    store_mtime: Option<SystemTime>,
}

impl AppState {
    fn new(to_client: mpsc::UnboundedSender<serde_json::Value>) -> std::sync::Arc<Self> {
        let (bootstrap_tx, bootstrap_rx) = watch::channel(false);
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let session = Session {
            build_server_path: cwd.join("buildServer.json"),
            root: cwd,
            root_uri: None,
            build_root: None,
            scheme: None,
            workspace: None,
            store: None,
            watermark: None,
            bs_mtime: None,
            store_mtime: None,
        };
        std::sync::Arc::new(Self {
            session: RwLock::new(session),
            bootstrap_tx,
            bootstrap_rx,
            ingest_started: AtomicBool::new(false),
            to_client,
        })
    }

    /// Send one framed JSON-RPC message to the client (fire-and-forget; a
    /// closed channel means the client is gone and we are tearing down).
    fn send(&self, msg: serde_json::Value) {
        let _ = self.to_client.send(msg);
    }

    /// Mark the cold-start bootstrap complete and wake any waiters.
    fn signal_bootstrap_done(&self) {
        let _ = self.bootstrap_tx.send(true);
    }

    /// Block until the bootstrap has finished, up to `timeout`; returns
    /// immediately once it is done. On timeout the caller serves whatever the
    /// store already holds rather than answering `null`.
    async fn wait_bootstrap(&self, timeout: std::time::Duration) {
        let mut rx = self.bootstrap_rx.clone();
        if *rx.borrow() {
            return;
        }
        let _ = tokio::time::timeout(timeout, rx.wait_for(|&done| done)).await;
    }
}

// ---------------------------------------------------------------------------
// single-writer stdout task (mirrors dap/proxy.rs::spawn_writer)
// ---------------------------------------------------------------------------

/// One task drains the channel and writes framed JSON to `sink`, so replies
/// from the read loop and the concurrent `sourceKitOptions` / ingest tasks
/// never interleave on the wire.
fn spawn_writer<W>(mut sink: W) -> mpsc::UnboundedSender<serde_json::Value>
where
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (tx, mut rx) = mpsc::unbounded_channel::<serde_json::Value>();
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            let body = match serde_json::to_vec(&msg) {
                Ok(b) => b,
                Err(e) => {
                    log::error!("bsp: failed to serialize message: {e}");
                    eprintln!("xcode-dap: bsp: failed to serialize message: {e}");
                    continue;
                }
            };
            let bytes = framing::frame(&body);
            if sink.write_all(&bytes).await.is_err() {
                break;
            }
            if sink.flush().await.is_err() {
                break;
            }
        }
        let _ = sink.shutdown().await;
    });
    tx
}

// ---------------------------------------------------------------------------
// URI <-> path helpers (children access these via `super::`)
// ---------------------------------------------------------------------------

/// `file://…` URI to a filesystem path. Percent-decodes by hand (paths carry
/// spaces / Cyrillic and we take no new crate) and drops a non-empty host
/// segment (`file://host/p` -> `/p`), the common `file:///p` yielding `/p`.
fn uri_to_path(uri: &str) -> PathBuf {
    let rest = match uri.strip_prefix("file://") {
        Some(r) => match r.find('/') {
            Some(0) | None => r,
            Some(i) => &r[i..], // skip an authority/host component
        },
        None => uri,
    };
    PathBuf::from(percent_decode(rest))
}

/// Percent-decode `%XX` byte-wise, then interpret the bytes as UTF-8 (lossy).
/// A stray or truncated `%` is passed through literally.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Filesystem path to a `file://` URI, percent-encoding everything outside
/// the RFC 3986 unreserved set (keeping `/` as the separator). Only the
/// cwd-fallback path builds a URI this way; the normal path echoes the
/// client's own `rootUri` string instead.
fn path_to_file_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for &b in path.to_string_lossy().as_bytes() {
        match b {
            b'/' | b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_to_path_percent_decodes_and_handles_host() {
        assert_eq!(
            uri_to_path("file:///Users/x/App"),
            PathBuf::from("/Users/x/App")
        );
        // spaces + Cyrillic (U+0420 'Р' = D0 A0)
        assert_eq!(
            uri_to_path("file:///Users/x/My%20%D0%A0/A.swift"),
            PathBuf::from("/Users/x/My Р/A.swift")
        );
        // a host authority is dropped
        assert_eq!(
            uri_to_path("file://localhost/Users/x"),
            PathBuf::from("/Users/x")
        );
        // truncated / stray percent passes through
        assert_eq!(uri_to_path("file:///a%2"), PathBuf::from("/a%2"));
    }

    #[test]
    fn path_to_file_uri_encodes_reserved_bytes() {
        assert_eq!(
            path_to_file_uri(Path::new("/Users/x/App")),
            "file:///Users/x/App"
        );
        assert_eq!(
            path_to_file_uri(Path::new("/Users/x/My App")),
            "file:///Users/x/My%20App"
        );
    }
}
