//! JSON-RPC dispatch for the bsp Build Server: classify each inbound frame,
//! serve the handful of methods sourcekit-lsp's modern pull dialect uses, and
//! reply through the single-writer channel. Every reply body is built with
//! `serde_json::json!` — the method surface is small and fixed.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use super::AppState;
use crate::engine::compile_store::CompileStore;
use crate::util::hash::fnv1a64;
use crate::util::paths::{mtime, zedxcode_home};

/// How long `sourceKitOptions` waits for the cold-start bootstrap before
/// serving whatever the store already holds (rather than answering `null`).
const BOOTSTRAP_WAIT: Duration = Duration::from_secs(60);

/// The `buildServer.json` fields we care about (our own private extensions,
/// written by the pipeline's regenerator).
pub(super) struct BsConfig {
    pub(super) build_root: Option<String>,
    pub(super) scheme: Option<String>,
    pub(super) workspace: Option<String>,
}

/// Classify one inbound frame and route it. A request has `method` + a
/// non-null `id`; a notification has `method` and no id; anything with an id
/// but no method is a response to one of our (reply-free) server notifications
/// and is ignored.
pub(super) async fn handle_message(state: &Arc<AppState>, raw: &[u8]) {
    let msg: Value = match serde_json::from_slice(raw) {
        Ok(v) => v,
        Err(e) => {
            log::warn!("bsp: dropping invalid JSON-RPC frame: {e}");
            return;
        }
    };
    let method = msg.get("method").and_then(|m| m.as_str());
    let id = msg.get("id").filter(|v| !v.is_null()).cloned();
    match (method, id) {
        (Some(method), Some(id)) => handle_request(state, method, id, &msg).await,
        (Some(method), None) => handle_notification(state, method).await,
        (None, _) => {} // a response to a server->client notification: none expect one
    }
}

async fn handle_request(state: &Arc<AppState>, method: &str, id: Value, msg: &Value) {
    match method {
        "build/initialize" => {
            let result = on_initialize(state, msg).await;
            send_result(state, id, result);
        }
        "workspace/buildTargets" => send_result(state, id, build_targets()),
        "buildTarget/sources" => {
            let result = build_target_sources(state).await;
            send_result(state, id, result);
        }
        "textDocument/sourceKitOptions" => spawn_source_kit_options(state, id, msg),
        "workspace/waitForBuildSystemUpdates" => send_result(state, id, json!({})),
        "build/shutdown" => send_result(state, id, Value::Null),
        other => {
            log::info!("bsp: unhandled request method '{other}'");
            send_error(state, id, -32601, &format!("method not found: {other}"));
        }
    }
}

async fn handle_notification(state: &Arc<AppState>, method: &str) {
    match method {
        "build/initialized" => start_ingest(state),
        "build/exit" => {
            log::info!("bsp: build/exit, exiting");
            std::process::exit(0);
        }
        "workspace/didChangeWatchedFiles" => {} // sources are discovered from logs, not watchers
        "$/cancelRequest" => {}                 // all our work is cheap / fire-and-forget
        other => log::info!("bsp: unhandled notification method '{other}'"),
    }
}

// ---------------------------------------------------------------------------
// build/initialize
// ---------------------------------------------------------------------------

/// Anchor to the project `rootUri`, load `<root>/buildServer.json`, and reply
/// with the modern pull-dialect capabilities. `dataKind: "sourceKit"` +
/// `sourceKitOptionsProvider: true` are what make sourcekit-lsp pull options
/// per file instead of downgrading to the legacy push protocol.
async fn on_initialize(state: &Arc<AppState>, msg: &Value) -> Value {
    let params = msg.get("params");
    let root_uri = params
        .and_then(|p| p.get("rootUri"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let root = match &root_uri {
        Some(u) => super::uri_to_path(u),
        None => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
    };
    let bs_path = root.join("buildServer.json");
    // Stat before reading (like `reload_config_if_changed`): a concurrent
    // rewrite landing in the load window below then triggers one redundant,
    // self-healing reload next tick instead of being masked by a newer mtime.
    let bs_mtime = mtime(&bs_path);
    let cfg = read_build_server(&bs_path);

    let build_root = cfg.build_root.as_deref().map(PathBuf::from);
    let scheme = cfg.scheme.clone();
    let store = match (&build_root, &scheme) {
        (Some(br), Some(sc)) => Some(CompileStore::load(br, sc)),
        _ => None,
    };

    let index_store_path = build_root
        .as_ref()
        .map(|br| format!("{}/Index.noindex/DataStore", br.display()))
        .unwrap_or_default();
    let index_database_path = index_database_path(&index_store_path);

    log::info!(
        "bsp: initialize root {} build_root {:?} scheme {:?} store {} modules",
        root.display(),
        build_root,
        scheme,
        store.as_ref().map(|s| s.module_count()).unwrap_or(0),
    );

    {
        let mut s = state.session.write().await;
        s.bs_mtime = bs_mtime;
        s.build_server_path = bs_path;
        s.watermark = store
            .as_ref()
            .and_then(|st| st.last_ingested_log().map(String::from));
        s.store = store;
        s.build_root = build_root;
        s.scheme = scheme;
        s.workspace = cfg.workspace;
        s.root_uri = root_uri.clone();
        s.root = root;
    }

    json!({
        "displayName": "xcode-dap",
        "version": env!("CARGO_PKG_VERSION"),
        "bspVersion": "2.2.0",
        "rootUri": root_uri,
        "capabilities": {
            "languageIds": ["c", "cpp", "objective-c", "objective-cpp", "swift"]
        },
        "dataKind": "sourceKit",
        "data": {
            "indexStorePath": index_store_path,
            "indexDatabasePath": index_database_path,
            "sourceKitOptionsProvider": true
        }
    })
}

/// `~/.zedxcode/cache/index-db-<fnv1a64(indexStorePath):016x>` (absolute).
/// Created eagerly so sourcekit-lsp's IndexStoreDB can open it.
fn index_database_path(index_store_path: &str) -> String {
    let hash = fnv1a64(index_store_path.as_bytes());
    let name = format!("index-db-{hash:016x}");
    match zedxcode_home() {
        Ok(home) => {
            let dir = home.join("cache").join(&name);
            let _ = std::fs::create_dir_all(&dir);
            dir.to_string_lossy().into_owned()
        }
        Err(_) => name,
    }
}

// ---------------------------------------------------------------------------
// workspace/buildTargets + buildTarget/sources
// ---------------------------------------------------------------------------

/// A single opaque dummy target. sourcekit-lsp only needs *a* target to hang
/// documents off; the real per-file compile args come from `sourceKitOptions`.
fn build_targets() -> Value {
    json!({
        "targets": [{
            "id": { "uri": "dummy://dummy" },
            "displayName": "BuildServer",
            "tags": ["test"],
            "capabilities": {},
            "languageIds": ["c", "cpp", "objective-c", "objective-cpp", "swift"],
            "dependencies": []
        }]
    })
}

/// One source item: the project root as a directory (`kind: 2`). sourcekit-lsp
/// maps every document that descends from this directory to the dummy target.
async fn build_target_sources(state: &Arc<AppState>) -> Value {
    let s = state.session.read().await;
    // Prefer the client's own rootUri string (byte-identical host/encoding);
    // fall back to encoding the decoded root when the client omitted it.
    let base = match &s.root_uri {
        Some(u) => u.trim_end_matches('/').to_string(),
        None => super::path_to_file_uri(&s.root)
            .trim_end_matches('/')
            .to_string(),
    };
    let uri = format!("{base}/");
    json!({
        "items": [{
            "target": { "uri": "dummy://dummy" },
            "sources": [{ "uri": uri, "kind": 2, "generated": false }]
        }]
    })
}

// ---------------------------------------------------------------------------
// textDocument/sourceKitOptions
// ---------------------------------------------------------------------------

/// Answer off the read loop so a wait for the cold-start bootstrap never
/// stalls other messages. Replies land on the single-writer channel, so
/// out-of-order completion is fine (JSON-RPC matches by id).
fn spawn_source_kit_options(state: &Arc<AppState>, id: Value, msg: &Value) {
    let uri = msg
        .get("params")
        .and_then(|p| p.get("textDocument"))
        .and_then(|d| d.get("uri"))
        .and_then(|u| u.as_str())
        .map(String::from);
    let state = state.clone();
    tokio::spawn(async move {
        let result = source_kit_options(&state, uri).await;
        send_result(&state, id, result.unwrap_or(Value::Null));
    });
}

async fn source_kit_options(state: &Arc<AppState>, uri: Option<String>) -> Option<Value> {
    let path = super::uri_to_path(&uri?);
    state.wait_bootstrap(BOOTSTRAP_WAIT).await;
    let s = state.session.read().await;
    let store = s.store.as_ref()?;
    let (args, working_dir) = store.options_for_file(&path, Some(&s.root))?;
    Some(json!({ "compilerArguments": args, "workingDirectory": working_dir }))
}

// ---------------------------------------------------------------------------
// ingest handoff + server->client notification
// ---------------------------------------------------------------------------

fn start_ingest(state: &Arc<AppState>) {
    if state.ingest_started.swap(true, Ordering::SeqCst) {
        return; // build/initialized already fired
    }
    let state = state.clone();
    tokio::spawn(async move { super::ingest::run(state).await });
}

/// Tell sourcekit-lsp all targets changed so it re-queries `sourceKitOptions`
/// (no LSP restart). `changes: null` = "everything".
pub(super) fn push_did_change(state: &AppState) {
    state.send(json!({
        "jsonrpc": "2.0",
        "method": "buildTarget/didChange",
        "params": { "changes": null }
    }));
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// Parse the `build_root` / `scheme` / `workspace` fields out of a
/// `buildServer.json`. Missing / unreadable / malformed yields all-`None`
/// (the server still runs; `sourceKitOptions` just answers `null`).
pub(super) fn read_build_server(path: &Path) -> BsConfig {
    let none = BsConfig {
        build_root: None,
        scheme: None,
        workspace: None,
    };
    let Ok(bytes) = std::fs::read(path) else {
        return none;
    };
    let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
        return none;
    };
    let field = |k: &str| v.get(k).and_then(|x| x.as_str()).map(String::from);
    BsConfig {
        build_root: field("build_root"),
        scheme: field("scheme"),
        workspace: field("workspace"),
    }
}

fn send_result(state: &AppState, id: Value, result: Value) {
    state.send(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
}

fn send_error(state: &AppState, id: Value, code: i64, message: &str) {
    state.send(json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    }));
}
