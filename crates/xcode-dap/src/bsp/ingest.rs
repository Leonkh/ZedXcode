//! Store ingestion: a cold-start bootstrap that replays the retained build
//! logs, then a ~1 s poll loop that folds in each new build and notices a
//! `buildServer.json` rewrite (scheme switch). The build pipeline itself is
//! untouched — this side only *reads* Xcode's logs.
//!
//! Parsing happens off every lock (a workspace log is 100+ modules). The
//! read-merge-write is a cross-process advisory-locked operation
//! ([`CompileStore::merge_save_locked`]) run on a blocking thread, off the
//! session lock, so concurrent `sourceKitOptions` readers are never blocked on
//! a parse or on another process's store write.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use super::{server, AppState};
use crate::engine::compile_store::{CompileStore, Watermark};
use crate::engine::xcactivitylog;
use crate::util::paths::mtime;

/// Poll cadence. Cheap: a `buildServer.json` stat plus, when the newest log
/// filename is unchanged, a manifest read.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Bootstrap the store, unblock `sourceKitOptions`, then poll forever (the
/// task dies with the process on stdin EOF / `build/exit`).
pub(super) async fn run(state: Arc<AppState>) {
    bootstrap(&state).await;
    state.signal_bootstrap_done();

    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    loop {
        ticker.tick().await;
        poll_once(&state).await;
    }
}

/// Cold start: when the store is empty, replay every retained log for the
/// scheme oldest→newest so module coverage matches a warm, long-lived build
/// server from the first request. A non-empty store (a prior session's cache)
/// keeps its persisted watermark and lets the poll loop catch up.
async fn bootstrap(state: &Arc<AppState>) {
    let (build_root, scheme, empty) = {
        let s = state.session.read().await;
        (
            s.build_root.clone(),
            s.scheme.clone(),
            s.store.as_ref().map(|st| st.is_empty()).unwrap_or(true),
        )
    };
    let (Some(build_root), Some(scheme)) = (build_root, scheme) else {
        log::info!("bsp: bootstrap skipped (no build_root/scheme in buildServer.json)");
        return;
    };
    if !empty {
        let mut s = state.session.write().await;
        s.store_mtime = store_file_mtime(&build_root, &scheme);
        let modules = s.store.as_ref().map(|st| st.module_count()).unwrap_or(0);
        log::info!("bsp: warm start, store has {modules} modules; poll loop will catch up");
        return;
    }

    let logs = xcactivitylog::logs_for_scheme(&build_root, &scheme);
    log::info!(
        "bsp: cold bootstrap replaying {} log(s) for scheme '{scheme}'",
        logs.len()
    );

    // Parse everything off-lock, remembering the newest (last) filename.
    let parsed: Vec<Vec<_>> = logs.iter().map(|l| xcactivitylog::parse_log(l)).collect();
    let newest = logs.last().and_then(|l| file_name(l));

    ingest_parsed(state, &build_root, &scheme, parsed, newest).await;
    server::push_did_change(state);
}

/// One poll tick: pick up a `buildServer.json` rewrite, an externally-written
/// store (a CLI build the pipeline ingested), then a newer Xcode.app build log.
async fn poll_once(state: &Arc<AppState>) {
    reload_config_if_changed(state).await;
    reload_store_if_changed(state).await;

    let (build_root, scheme, watermark) = {
        let s = state.session.read().await;
        (s.build_root.clone(), s.scheme.clone(), s.watermark.clone())
    };
    let (Some(build_root), Some(scheme)) = (build_root, scheme) else {
        return;
    };

    // Cheap common case (no new build since last tick): the newest registered
    // log is still the watermark, so a single manifest read early-returns.
    let Some(newest_log) = xcactivitylog::newest_log(&build_root, &scheme) else {
        return;
    };
    if file_name(&newest_log) == watermark {
        return; // already ingested up to the newest build
    }

    // A new build appeared: catch up on *every* log after the watermark, not
    // just the newest. Two Xcode.app builds between polls (or while Zed was
    // closed) each leave a log; skipping the intermediate ones serves stale
    // args for the modules only that build recompiled.
    let logs = xcactivitylog::logs_for_scheme(&build_root, &scheme);
    let pending = logs_after_watermark(&logs, watermark.as_deref());
    if pending.is_empty() {
        return; // manifest raced away under us — nothing to ingest
    }

    // Parse every pending log off-lock (a workspace log is 100+ modules),
    // remembering the newest (last) filename.
    let parsed: Vec<Vec<_>> = pending
        .iter()
        .map(|l| xcactivitylog::parse_log(l))
        .collect();
    let newest = pending.last().and_then(|l| file_name(l));

    // `ingest_parsed` re-checks the live config after merging off-lock and only
    // adopts the result when it still matches.
    let changed = ingest_parsed(state, &build_root, &scheme, parsed, newest).await;
    if changed {
        server::push_did_change(state);
    }
}

/// Merge freshly parsed logs into the `(build_root, scheme)` store, then adopt
/// the result into the session. The read-merge-write itself is a cross-process
/// locked operation ([`CompileStore::merge_save_locked`]): the build pipeline
/// writes the same store when it ingests a CLI build's stdout, so both sides
/// take a shared advisory lock and re-read the current on-disk store before
/// merging — a concurrent write is folded in, never lost. It runs on a blocking
/// thread (`flock(LOCK_EX)` would otherwise block a tokio worker) and *off* the
/// session lock, so `sourceKitOptions` readers are never blocked on it. The
/// in-memory watermark advances to `newest` even on a no-op parse (matching the
/// on-disk watermark, which only advances on a real change). Returns `true`
/// only when a non-empty parse actually changed the store.
async fn ingest_parsed(
    state: &Arc<AppState>,
    build_root: &Path,
    scheme: &str,
    parsed: Vec<Vec<xcactivitylog::ParsedModule>>,
    newest: Option<String>,
) -> bool {
    let br = build_root.to_path_buf();
    let sc = scheme.to_string();
    let watermark = Watermark::Advance(newest.clone());
    let merged = tokio::task::spawn_blocking(move || {
        CompileStore::merge_save_locked(&br, &sc, parsed, watermark)
    })
    .await;
    let merged = match merged {
        Ok(m) => m,
        Err(e) => {
            log::warn!("bsp: store ingest task failed to join: {e}");
            return false;
        }
    };

    let mut s = state.session.write().await;
    // The config may have been swapped under us while we merged off-lock; only
    // adopt the result when it still matches the live config.
    if s.build_root.as_deref() != Some(build_root) || s.scheme.as_deref() != Some(scheme) {
        return false;
    }
    if merged.changed {
        log::info!(
            "bsp: ingested up to {newest:?} ({} modules total)",
            merged.store.module_count()
        );
    }
    // Adopt the just-written store and record its mtime as our own last write,
    // so `reload_store_if_changed` does not re-trigger on it.
    s.store = Some(merged.store);
    s.store_mtime = merged.mtime;
    s.watermark = newest;
    merged.changed
}

/// Logs still to ingest: those strictly after the watermark, given every scheme
/// log oldest→newest. A `None` watermark (never ingested) replays all; a
/// watermark no longer present in the manifest falls back to the newest log
/// alone (self-healing rather than replaying the whole history).
fn logs_after_watermark<'a>(logs: &'a [PathBuf], watermark: Option<&str>) -> &'a [PathBuf] {
    match watermark {
        None => logs,
        Some(w) => match logs.iter().position(|p| file_name(p).as_deref() == Some(w)) {
            Some(i) => &logs[i + 1..],
            None => &logs[logs.len().saturating_sub(1)..],
        },
    }
}

/// Adopt a store written by another process — the build pipeline writes the
/// same `(build_root, scheme)` store when it ingests a CLI build's stdout.
/// Detected by the store file's mtime differing from bsp's own last write, so
/// bsp reloads it (replacing its in-memory copy) and re-queries sourcekit-lsp.
async fn reload_store_if_changed(state: &Arc<AppState>) {
    let (build_root, scheme, recorded) = {
        let s = state.session.read().await;
        (s.build_root.clone(), s.scheme.clone(), s.store_mtime)
    };
    let (Some(build_root), Some(scheme)) = (build_root, scheme) else {
        return;
    };
    let cur = store_file_mtime(&build_root, &scheme);
    if cur == recorded {
        return; // unchanged since bsp's own last write
    }
    let store = CompileStore::load(&build_root, &scheme);

    let mut s = state.session.write().await;
    // Config may have switched under us; only apply if still current.
    if s.build_root.as_deref() != Some(build_root.as_path())
        || s.scheme.as_deref() != Some(scheme.as_str())
    {
        return;
    }
    log::info!(
        "bsp: external store update ({} modules), reloading",
        store.module_count()
    );
    s.watermark = store.last_ingested_log().map(String::from);
    s.store = Some(store);
    s.store_mtime = cur;
    drop(s);
    server::push_did_change(state);
}

/// Reload the config triplet when `buildServer.json` changed on disk. A scheme
/// or build-root switch swaps the whole store (a different `(build_root,
/// scheme)` is a different cache file) and pushes `didChange`.
async fn reload_config_if_changed(state: &Arc<AppState>) {
    let (bs_path, last) = {
        let s = state.session.read().await;
        (s.build_server_path.clone(), s.bs_mtime)
    };
    let cur = mtime(&bs_path);
    if cur == last {
        return;
    }
    let cfg = server::read_build_server(&bs_path);
    let new_build_root = cfg.build_root.as_deref().map(PathBuf::from);

    let mut s = state.session.write().await;
    s.bs_mtime = cur;
    let switched = new_build_root.as_deref() != s.build_root.as_deref() || cfg.scheme != s.scheme;
    s.build_root = new_build_root.clone();
    s.scheme = cfg.scheme.clone();
    s.workspace = cfg.workspace;
    if !switched {
        return;
    }
    s.store = match (&new_build_root, &cfg.scheme) {
        (Some(br), Some(sc)) => Some(CompileStore::load(br, sc)),
        _ => None,
    };
    s.watermark = s
        .store
        .as_ref()
        .and_then(|st| st.last_ingested_log().map(String::from));
    s.store_mtime = match (&new_build_root, &cfg.scheme) {
        (Some(br), Some(sc)) => store_file_mtime(br, sc),
        _ => None,
    };
    log::info!(
        "bsp: buildServer.json changed, reloaded build_root {:?} scheme {:?}",
        new_build_root,
        cfg.scheme,
    );
    drop(s);
    server::push_did_change(state);
}

fn file_name(path: &Path) -> Option<String> {
    path.file_name().map(|f| f.to_string_lossy().into_owned())
}

/// mtime of the on-disk store file for `(build_root, scheme)`, or `None` when
/// it does not exist yet.
fn store_file_mtime(build_root: &Path, scheme: &str) -> Option<SystemTime> {
    CompileStore::store_path(build_root, scheme)
        .ok()
        .and_then(|p| mtime(&p))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logs(names: &[&str]) -> Vec<PathBuf> {
        names
            .iter()
            .map(|n| PathBuf::from(format!("/logs/{n}")))
            .collect()
    }

    #[test]
    fn logs_after_watermark_returns_only_newer_logs() {
        let all = logs(&["1.xcactivitylog", "2.xcactivitylog", "3.xcactivitylog"]);

        // Watermark mid-history: only strictly-newer logs are pending — the
        // intermediate-log-skip bug (serving stale args) would return nothing.
        assert_eq!(
            logs_after_watermark(&all, Some("2.xcactivitylog")),
            &all[2..]
        );
        // Newest already ingested -> nothing pending.
        assert!(logs_after_watermark(&all, Some("3.xcactivitylog")).is_empty());
        // Never ingested -> replay every log.
        assert_eq!(logs_after_watermark(&all, None), &all[..]);
        // Watermark no longer in the manifest -> newest only (self-heal).
        assert_eq!(
            logs_after_watermark(&all, Some("gone.xcactivitylog")),
            &all[2..]
        );
        // Empty manifest -> nothing, no panic.
        assert!(logs_after_watermark(&[], Some("2.xcactivitylog")).is_empty());
        assert!(logs_after_watermark(&[], None).is_empty());
    }
}
