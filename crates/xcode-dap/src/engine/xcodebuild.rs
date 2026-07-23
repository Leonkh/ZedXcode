//! xcodebuild build/clean/showBuildSettings, output filter/throttle.
//!
//! Default DerivedData unless `derivedData` sets an explicit
//! `-derivedDataPath`; never `CODE_SIGNING_ALLOWED=NO`.
//! See `docs/design/dap-proxy.md` §4-§5.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::engine::config::{BuildOutput, LaunchConfig};
use crate::engine::pipeline::OutputSink;
use crate::util::logging;
use crate::util::paths::{container_flag, zedxcode_home};
use crate::util::procgroup;

/// xcodebuild exited non-zero; carries the exit code so CLI commands can
/// propagate it verbatim (`exit code = xcodebuild's`), plus the last
/// `error:` lines and the full-log path so the message is actionable on
/// its own (CLI stderr and the Zed error toast both show `Display`).
#[derive(Debug, Default)]
pub struct BuildFailed {
    pub code: i32,
    /// Last few `error:` lines seen in the xcodebuild output (may be empty,
    /// e.g. for `clean` failures).
    pub errors: Vec<String>,
    /// Path of the full on-disk build log, when one was written.
    pub log_path: Option<PathBuf>,
}

impl std::fmt::Display for BuildFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "xcodebuild failed with exit code {}", self.code)?;
        for line in &self.errors {
            write!(f, "\n  {line}")?;
        }
        if let Some(log) = &self.log_path {
            write!(f, "\n  full build log: {}", log.display())?;
        }
        Ok(())
    }
}

impl std::error::Error for BuildFailed {}

/// How many trailing `error:` lines [`BuildFailed`] keeps.
const MAX_ERROR_LINES: usize = 5;

/// `xcodebuild <container-flag> <workspace> -scheme <scheme> [-configuration
/// <c>] [-derivedDataPath <dd>]` — the shared prefix for build / clean /
/// showBuildSettings. Split out from [`base_cmd`] so [`resolve_build_root`]
/// (which has no full [`LaunchConfig`]) reuses the exact same argument logic.
fn settings_cmd(
    workspace: &Path,
    scheme: &str,
    configuration: Option<&str>,
    derived_data: Option<&Path>,
) -> Command {
    let mut cmd = Command::new("xcodebuild");
    cmd.arg(container_flag(workspace))
        .arg(workspace)
        .arg("-scheme")
        .arg(scheme);
    if let Some(c) = configuration {
        cmd.arg("-configuration").arg(c);
    }
    if let Some(dd) = derived_data {
        cmd.arg("-derivedDataPath").arg(dd);
    }
    cmd
}

fn base_cmd(cfg: &LaunchConfig) -> Command {
    settings_cmd(
        &cfg.workspace,
        &cfg.scheme,
        cfg.configuration.as_deref(),
        cfg.derived_data.as_deref(),
    )
}

/// Path of the full on-disk build log (`~/.zedxcode/logs/build-latest.log`).
///
/// One shared file, truncated at the start of every build. Concurrent
/// sessions (the per-UDID pidfiles allow one per simulator) interleave
/// their output here, and a [`BuildFailed`] message may then point at the
/// other session's log — the single well-known path is kept because docs
/// and users rely on it, and parallel builds are rare.
pub fn build_log_path() -> anyhow::Result<PathBuf> {
    let dir = zedxcode_home()?.join("logs");
    std::fs::create_dir_all(&dir).context("creating ~/.zedxcode/logs")?;
    Ok(dir.join("build-latest.log"))
}

/// `xcodebuild -workspace <ws> (or -project <proj>) -scheme <scheme>
/// -destination platform=iOS Simulator,id=<udid> build`, streamed through
/// the filter/throttle into `sink`. The full log always goes to
/// `~/.zedxcode/logs/build-latest.log`.
pub async fn build(
    cfg: &LaunchConfig,
    udid: &str,
    sink: &dyn OutputSink,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let log_path = build_log_path()?;
    let mut log = std::fs::File::create(&log_path)
        .with_context(|| format!("creating {}", log_path.display()))?;

    let mut cmd = base_cmd(cfg);
    cmd.arg("-destination")
        .arg(format!("platform=iOS Simulator,id={udid}"))
        .arg("build")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    procgroup::spawn_in_new_group(&mut cmd);
    log::info!(target: "xcodebuild", "build: {}", logging::describe_command(&cmd));
    let started = std::time::Instant::now();
    let mut child = cmd.spawn().context("spawning xcodebuild")?;
    // setpgid(0, 0) makes the child's pid its pgid.
    let pgid = child.id().map(|p| p as i32).unwrap_or(0);

    // Merge stdout + stderr into one line stream.
    let (tx, mut rx) = mpsc::channel::<String>(1024);
    let stdout = child.stdout.take().context("xcodebuild stdout not piped")?;
    let stderr = child.stderr.take().context("xcodebuild stderr not piped")?;
    spawn_line_reader(stdout, tx.clone());
    spawn_line_reader(stderr, tx);

    let mut filter = LineFilter::new(cfg.build_output);
    let mut batch = Batcher::new();
    // Trailing `error:` lines for the BuildFailed message.
    let mut error_lines: Vec<String> = Vec::new();
    let mut error_count: usize = 0;
    loop {
        // Copy the deadline out so the sleep future doesn't borrow `batch`.
        let deadline = batch.deadline();
        tokio::select! {
            line = rx.recv() => match line {
                Some(line) => {
                    let _ = writeln!(log, "{line}");
                    if line.contains("error:") {
                        error_count += 1;
                        if error_lines.len() == MAX_ERROR_LINES {
                            error_lines.remove(0);
                        }
                        error_lines.push(line.trim().to_owned());
                    }
                    if filter.passes(&line) {
                        batch.push(line);
                        if batch.full() {
                            batch.flush(sink);
                        }
                    }
                }
                None => break, // both pipes closed: xcodebuild is done
            },
            _ = sleep_until_opt(deadline), if deadline.is_some() => batch.flush(sink),
            _ = cancel.cancelled() => {
                batch.flush(sink);
                sink.line("console", "Build cancelled — stopping xcodebuild");
                procgroup::term_group(pgid);
                if tokio::time::timeout(Duration::from_secs(3), child.wait())
                    .await
                    .is_err()
                {
                    procgroup::kill_group(pgid);
                    let _ = child.wait().await;
                }
                bail!("build cancelled");
            }
        }
    }
    batch.flush(sink);
    let status = child.wait().await.context("waiting for xcodebuild")?;
    log::info!(
        target: "xcodebuild",
        "build exited {} in {} ms ({} error line(s), full log {})",
        status,
        started.elapsed().as_millis(),
        error_count,
        log_path.display()
    );
    if status.success() {
        Ok(())
    } else {
        let code = status.code().unwrap_or(1);
        Err(anyhow::Error::new(BuildFailed {
            code,
            errors: error_lines,
            log_path: Some(log_path),
        }))
    }
}

fn spawn_line_reader(
    stream: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    tx: mpsc::Sender<String>,
) {
    tokio::spawn(async move {
        // Byte-oriented + lossy decode on purpose: a single non-UTF-8 line
        // (e.g. a run-script phase echoing binary bytes, or clang printing a
        // raw source excerpt in a diagnostic) must not kill the reader. With
        // `lines().next_line()` an `Err(InvalidData)` ends the task, drops the
        // pipe, and SIGPIPEs xcodebuild mid-build — a spurious build failure
        // with the real diagnostics lost. `read_until` never errors on invalid
        // UTF-8, so the build always runs to its real exit status.
        let mut reader = BufReader::new(stream);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf).await {
                Ok(0) => break, // EOF: pipe closed
                Ok(_) => {
                    // Strip the trailing newline (and a preceding CR), matching
                    // the previous `lines()` behaviour.
                    if buf.last() == Some(&b'\n') {
                        buf.pop();
                        if buf.last() == Some(&b'\r') {
                            buf.pop();
                        }
                    }
                    let line = String::from_utf8_lossy(&buf).into_owned();
                    if tx.send(line).await.is_err() {
                        break;
                    }
                }
                Err(_) => break, // genuine I/O error
            }
        }
    });
}

/// Sleep until `deadline`; pend forever when `None` (branch is then
/// disabled by its select! guard anyway).
async fn sleep_until_opt(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// `xcodebuild ... clean` (= Xcode "Clean Build Folder"); stdio inherited.
pub async fn clean(cfg: &LaunchConfig) -> anyhow::Result<()> {
    let status = base_cmd(cfg)
        .arg("clean")
        .kill_on_drop(true)
        .status()
        .await
        .context("running xcodebuild clean")?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow::Error::new(BuildFailed {
            code: status.code().unwrap_or(1),
            ..Default::default()
        }))
    }
}

// ---------------------------------------------------------------------------
// App path via -showBuildSettings (mtime-keyed cache)
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Default)]
struct SettingsCache {
    entries: HashMap<String, CacheEntry>,
}

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    workspace_mtime: u64,
    app_path: PathBuf,
}

fn cache_path() -> anyhow::Result<PathBuf> {
    let dir = zedxcode_home()?.join("cache");
    std::fs::create_dir_all(&dir).context("creating ~/.zedxcode/cache")?;
    Ok(dir.join("build-settings.json"))
}

/// Cache key for the app-path settings cache: `(workspace, scheme, udid,
/// configuration, derivedData)`. Two configs that would resolve different
/// `.app` products (e.g. a different `-derivedDataPath`) must never share a
/// cache entry.
fn settings_cache_key(ws: &Path, cfg: &LaunchConfig, udid: &str) -> String {
    format!(
        "{}|{}|{}|{}|{}",
        ws.display(),
        cfg.scheme,
        udid,
        cfg.configuration.as_deref().unwrap_or(""),
        cfg.derived_data
            .as_deref()
            .map(|p| p.to_string_lossy())
            .unwrap_or_default()
    )
}

fn workspace_mtime(workspace: &Path) -> anyhow::Result<u64> {
    let mtime = std::fs::metadata(workspace)
        .with_context(|| format!("stat {}", workspace.display()))?
        .modified()
        .with_context(|| format!("reading mtime of {}", workspace.display()))?;
    Ok(mtime
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs())
}

/// `xcodebuild ... -showBuildSettings -json` ->
/// `TARGET_BUILD_DIR` + `WRAPPER_NAME`. Cached per
/// (workspace, scheme, udid, configuration, derivedData) keyed on the
/// workspace mtime, so rebuilds skip the multi-second settings call.
///
/// Known staleness window: the key is the `.xcworkspace` directory mtime,
/// but the referenced `.xcodeproj` files live outside it. An edit made
/// directly in Xcode that changes the product (e.g. `PRODUCT_NAME`) does
/// not bump the workspace mtime, and the old cached `.app` path may still
/// exist from a prior build — delete
/// `~/.zedxcode/cache/build-settings.json` to force re-resolution.
pub async fn app_path(cfg: &LaunchConfig, udid: &str) -> anyhow::Result<PathBuf> {
    let ws = std::path::absolute(&cfg.workspace).unwrap_or_else(|_| cfg.workspace.clone());
    let mtime = workspace_mtime(&ws)?;
    let key = settings_cache_key(&ws, cfg, udid);

    let cache_file = cache_path()?;
    let mut cache: SettingsCache = std::fs::read(&cache_file)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    if let Some(entry) = cache.entries.get(&key) {
        if entry.workspace_mtime == mtime && entry.app_path.exists() {
            log::info!(
                target: "xcodebuild",
                "app path cache hit: {}",
                entry.app_path.display()
            );
            return Ok(entry.app_path.clone());
        }
    }
    log::info!(target: "xcodebuild", "app path cache miss — running -showBuildSettings");

    let started = std::time::Instant::now();
    let out = base_cmd(cfg)
        .arg("-destination")
        .arg(format!("platform=iOS Simulator,id={udid}"))
        .args(["-showBuildSettings", "-json", "build"])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .context("running xcodebuild -showBuildSettings")?;
    log::info!(
        target: "xcodebuild",
        "-showBuildSettings exited {} in {} ms",
        out.status,
        started.elapsed().as_millis()
    );
    if !out.status.success() {
        bail!(
            "xcodebuild -showBuildSettings failed: {}\nhint: check \"scheme\" \
             / \"configuration\" in .zed/debug.json (or the --scheme flag)",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let path = parse_app_path(&out.stdout)?;

    cache.entries.insert(
        key,
        CacheEntry {
            workspace_mtime: mtime,
            app_path: path.clone(),
        },
    );
    // Best-effort cache write.
    if let Ok(bytes) = serde_json::to_vec(&cache) {
        let _ = std::fs::write(&cache_file, bytes);
    }
    Ok(path)
}

/// First settings entry whose `WRAPPER_NAME` ends in `.app` ->
/// `TARGET_BUILD_DIR/WRAPPER_NAME`.
fn parse_app_path(json_bytes: &[u8]) -> anyhow::Result<PathBuf> {
    let entries: Value =
        serde_json::from_slice(json_bytes).context("parsing -showBuildSettings JSON")?;
    let arr = entries
        .as_array()
        .context("-showBuildSettings JSON is not an array")?;
    for entry in arr {
        let Some(settings) = entry.get("buildSettings") else {
            continue;
        };
        let wrapper = settings.get("WRAPPER_NAME").and_then(Value::as_str);
        let dir = settings.get("TARGET_BUILD_DIR").and_then(Value::as_str);
        if let (Some(wrapper), Some(dir)) = (wrapper, dir) {
            if wrapper.ends_with(".app") {
                return Ok(Path::new(dir).join(wrapper));
            }
        }
    }
    bail!("no .app product found in -showBuildSettings output")
}

// ---------------------------------------------------------------------------
// DerivedData build_root resolution (for buildServer.json generation)
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Default)]
struct BuildRootCache {
    entries: HashMap<String, BuildRootEntry>,
}

#[derive(Serialize, Deserialize)]
struct BuildRootEntry {
    workspace_mtime: u64,
    build_root: PathBuf,
}

fn build_root_cache_path() -> anyhow::Result<PathBuf> {
    let dir = zedxcode_home()?.join("cache");
    std::fs::create_dir_all(&dir).context("creating ~/.zedxcode/cache")?;
    Ok(dir.join("build-root.json"))
}

/// Cache key `ws|scheme|derivedData` for the build-root cache. Caching only
/// happens for the default-DerivedData case, but the key keeps the
/// `derivedData` segment so it never collides with a future explicit variant.
fn build_root_cache_key(ws: &Path, scheme: &str, derived_data: Option<&Path>) -> String {
    format!(
        "{}|{}|{}",
        ws.display(),
        scheme,
        derived_data
            .map(|p| p.to_string_lossy())
            .unwrap_or_default()
    )
}

/// The DerivedData build_root for `(workspace, scheme, configuration,
/// derivedData)` **without** a prior build (setup / refresh / select-scheme,
/// and the pre-build pipeline regen). An explicit `derived_data` short-
/// circuits; otherwise `xcodebuild -showBuildSettings -json` yields `BUILD_DIR`
/// (`<build_root>/Build/Products`) → up two levels. Cached per
/// (workspace, scheme, derivedData) keyed on the workspace mtime (like
/// [`app_path`]), so repeated setup / refresh calls skip the multi-second call.
/// Matches [`build_root_from_app`] so bsp reads the same compile store.
pub async fn resolve_build_root(
    workspace: &Path,
    scheme: &str,
    configuration: Option<&str>,
    derived_data: Option<&Path>,
) -> anyhow::Result<PathBuf> {
    if let Some(dd) = derived_data {
        return Ok(std::path::absolute(dd).unwrap_or_else(|_| dd.to_path_buf()));
    }
    let ws = std::path::absolute(workspace).unwrap_or_else(|_| workspace.to_path_buf());
    let mtime = workspace_mtime(&ws)?;
    let key = build_root_cache_key(&ws, scheme, None);

    let cache_file = build_root_cache_path()?;
    let mut cache: BuildRootCache = std::fs::read(&cache_file)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    if let Some(entry) = cache.entries.get(&key) {
        // The build_root path is deterministic per workspace (it need not exist
        // yet — a fresh setup runs before any build), so an mtime match is
        // enough; no existence check.
        if entry.workspace_mtime == mtime {
            log::info!(
                target: "xcodebuild",
                "build_root cache hit: {}",
                entry.build_root.display()
            );
            return Ok(entry.build_root.clone());
        }
    }
    log::info!(target: "xcodebuild", "build_root cache miss — running -showBuildSettings");

    let started = std::time::Instant::now();
    let out = settings_cmd(&ws, scheme, configuration, None)
        // A generic simulator destination resolves settings without a booted
        // device; BUILD_DIR (the per-workspace DerivedData root) is
        // destination-independent anyway.
        .arg("-destination")
        .arg("generic/platform=iOS Simulator")
        .args(["-showBuildSettings", "-json", "build"])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .context("running xcodebuild -showBuildSettings for build_root")?;
    log::info!(
        target: "xcodebuild",
        "-showBuildSettings (build_root) exited {} in {} ms",
        out.status,
        started.elapsed().as_millis()
    );
    if !out.status.success() {
        bail!(
            "xcodebuild -showBuildSettings failed: {}\nhint: check \"scheme\" \
             / \"configuration\" in .zed/debug.json (or the --scheme flag)",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let build_root = parse_build_root(&out.stdout)?;

    cache.entries.insert(
        key,
        BuildRootEntry {
            workspace_mtime: mtime,
            build_root: build_root.clone(),
        },
    );
    if let Ok(bytes) = serde_json::to_vec(&cache) {
        let _ = std::fs::write(&cache_file, bytes); // best-effort cache
    }
    Ok(build_root)
}

/// The DerivedData build_root a completed build wrote into: an explicit
/// `derived_data`, else the resolved `.app` product up four levels
/// (`<build_root>/Build/Products/<cfg>-<sdk>/<App>.app`). Must equal
/// [`resolve_build_root`]'s value so bsp reads the same compile store as the
/// buildServer.json records.
pub fn build_root_from_app(derived_data: Option<&Path>, app: &Path) -> Option<PathBuf> {
    if let Some(dd) = derived_data {
        return Some(std::path::absolute(dd).unwrap_or_else(|_| dd.to_path_buf()));
    }
    app.ancestors().nth(4).map(Path::to_path_buf)
}

/// First `BUILD_DIR` (`<build_root>/Build/Products`) in the
/// `-showBuildSettings -json` output → up two levels = the per-workspace
/// DerivedData root.
fn parse_build_root(json_bytes: &[u8]) -> anyhow::Result<PathBuf> {
    let entries: Value =
        serde_json::from_slice(json_bytes).context("parsing -showBuildSettings JSON")?;
    let arr = entries
        .as_array()
        .context("-showBuildSettings JSON is not an array")?;
    for entry in arr {
        if let Some(build_dir) = entry
            .get("buildSettings")
            .and_then(|s| s.get("BUILD_DIR"))
            .and_then(Value::as_str)
        {
            if let Some(root) = Path::new(build_dir).ancestors().nth(2) {
                return Ok(root.to_path_buf());
            }
        }
    }
    bail!("no BUILD_DIR in -showBuildSettings output")
}

// ---------------------------------------------------------------------------
// Filter + throttle (flood control, design §5.2)
// ---------------------------------------------------------------------------

/// Filtered mode passes phase headers, diagnostics (+2 context lines) and
/// the final `** BUILD SUCCEEDED/FAILED **`. Full mode passes everything.
pub(crate) struct LineFilter {
    mode: BuildOutput,
    context_left: u8,
}

impl LineFilter {
    pub(crate) fn new(mode: BuildOutput) -> Self {
        Self {
            mode,
            context_left: 0,
        }
    }

    pub(crate) fn passes(&mut self, line: &str) -> bool {
        if self.mode == BuildOutput::Full {
            return true;
        }
        if is_diagnostic(line) {
            // Pass the diagnostic plus its excerpt + caret context lines.
            self.context_left = 2;
            return true;
        }
        if self.context_left > 0 {
            self.context_left -= 1;
            return true;
        }
        is_phase_header(line)
    }
}

fn is_diagnostic(line: &str) -> bool {
    line.contains("error:") || line.contains("warning:") || line.contains("note:")
}

fn is_phase_header(line: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "=== ",              // legacy target banners
        "** ",               // ** BUILD SUCCEEDED/FAILED **
        "Build description", // build planning
        "CreateBuildDescription",
        "SwiftDriver",          // one per module: "Compiling module X"
        "CompileSwiftSources ", // legacy batch compile
        "CompileC ",
        "Ld ",
        "Libtool ",
        "CodeSign ",
        "PhaseScriptExecution ",
        "ProcessInfoPlistFile ",
    ];
    PREFIXES.iter().any(|p| line.starts_with(p))
}

/// Batches filtered lines into one newline-joined sink emission, flushed
/// every 50 ms or at 8 KB, whichever comes first (one event per batch).
struct Batcher {
    buf: String,
    deadline: Option<tokio::time::Instant>,
}

const BATCH_INTERVAL: Duration = Duration::from_millis(50);
const BATCH_MAX_BYTES: usize = 8 * 1024;

impl Batcher {
    fn new() -> Self {
        Self {
            buf: String::new(),
            deadline: None,
        }
    }

    fn push(&mut self, line: String) {
        if self.buf.is_empty() {
            self.deadline = Some(tokio::time::Instant::now() + BATCH_INTERVAL);
        } else {
            self.buf.push('\n');
        }
        self.buf.push_str(&line);
    }

    fn full(&self) -> bool {
        self.buf.len() >= BATCH_MAX_BYTES
    }

    /// 50 ms deadline of the oldest buffered line, if any.
    fn deadline(&self) -> Option<tokio::time::Instant> {
        self.deadline
    }

    fn flush(&mut self, sink: &dyn OutputSink) {
        if !self.buf.is_empty() {
            // Internal category "build": DapSink emits it as DAP "console"
            // but only tees it into the log at DEBUG (the full xcodebuild
            // log already lives in build-latest.log).
            sink.line("build", &self.buf);
            self.buf.clear();
        }
        self.deadline = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filtered() -> LineFilter {
        LineFilter::new(BuildOutput::Filtered)
    }

    #[tokio::test]
    async fn line_reader_is_lossy_and_survives_invalid_utf8() {
        // A middle line carries a raw non-UTF-8 byte (0xFF). The reader must
        // decode it lossily and keep going: dying here would drop the pipe and
        // SIGPIPE xcodebuild mid-build (a spurious failure, diagnostics lost).
        let mut input: Vec<u8> = Vec::new();
        input.extend_from_slice(b"first line\n");
        input.extend_from_slice(b"bad \xFF byte\n");
        input.extend_from_slice(b"last line\n");
        let (tx, mut rx) = mpsc::channel::<String>(16);
        spawn_line_reader(std::io::Cursor::new(input), tx);

        let mut lines = Vec::new();
        while let Some(l) = rx.recv().await {
            lines.push(l);
        }
        assert_eq!(lines.len(), 3, "every line survives an invalid-UTF-8 line");
        assert_eq!(lines[0], "first line");
        assert_eq!(lines[2], "last line");
        assert!(lines[1].contains('\u{FFFD}'), "bad byte decoded lossily");
    }

    #[test]
    fn passes_result_banners_and_phase_headers() {
        let mut f = filtered();
        assert!(f.passes("** BUILD SUCCEEDED **"));
        assert!(f.passes("** BUILD FAILED **"));
        assert!(f.passes("=== BUILD TARGET MyApp OF PROJECT MyApp ==="));
        assert!(f.passes("SwiftDriver MyApp normal arm64 (in target 'MyApp' from project 'MyApp')"));
        assert!(f.passes("Ld /path/MyApp.app/MyApp normal arm64"));
        assert!(f.passes("CodeSign /path/MyApp.app"));
        assert!(f.passes("PhaseScriptExecution [CP]\\ Check\\ Pods /path.sh"));
    }

    #[test]
    fn drops_noise() {
        let mut f = filtered();
        assert!(!f.passes("    cd /Users/x/my-app-ios"));
        assert!(!f.passes("export PATH=/usr/bin"));
        assert!(!f.passes("CompileSwift normal arm64 /path/SomeFile.swift (in target 'MyApp')"));
        assert!(!f.passes(""));
    }

    #[test]
    fn passes_diagnostics_with_context() {
        let mut f = filtered();
        assert!(f.passes("/path/File.swift:10:5: error: cannot find 'foo' in scope"));
        // Two context lines (source excerpt + caret) ride along...
        assert!(f.passes("        foo()"));
        assert!(f.passes("        ^~~"));
        // ...then filtering resumes.
        assert!(!f.passes("        bar()"));
        assert!(f.passes("/path/File.swift:12:1: warning: unused variable"));
        assert!(f.passes("note: Using codesigning identity"));
    }

    #[test]
    fn full_mode_passes_everything() {
        let mut f = LineFilter::new(BuildOutput::Full);
        assert!(f.passes("    cd /anywhere"));
        assert!(f.passes(""));
    }

    #[test]
    fn parses_app_path_from_settings_json() {
        let json = serde_json::json!([
            { "action": "build", "target": "SomeLib",
              "buildSettings": { "WRAPPER_NAME": "SomeLib.framework",
                                  "TARGET_BUILD_DIR": "/dd/Build/Products/Debug-iphonesimulator" } },
            { "action": "build", "target": "MyApp",
              "buildSettings": { "WRAPPER_NAME": "MyApp.app",
                                  "TARGET_BUILD_DIR": "/dd/Build/Products/Debug-iphonesimulator" } }
        ]);
        let path = parse_app_path(serde_json::to_vec(&json).unwrap().as_slice()).unwrap();
        assert_eq!(
            path,
            PathBuf::from("/dd/Build/Products/Debug-iphonesimulator/MyApp.app")
        );
    }

    #[test]
    fn build_failed_display_is_human_readable() {
        let plain = BuildFailed {
            code: 65,
            ..Default::default()
        };
        assert_eq!(plain.to_string(), "xcodebuild failed with exit code 65");

        let rich = BuildFailed {
            code: 65,
            errors: vec!["/x/File.swift:10:5: error: cannot find 'foo' in scope".into()],
            log_path: Some(PathBuf::from("/home/.zedxcode/logs/build-latest.log")),
        };
        assert_eq!(
            rich.to_string(),
            "xcodebuild failed with exit code 65\n  \
             /x/File.swift:10:5: error: cannot find 'foo' in scope\n  \
             full build log: /home/.zedxcode/logs/build-latest.log"
        );
    }

    #[test]
    fn base_cmd_picks_container_flag_by_extension() {
        let cfg = |workspace: &str| LaunchConfig {
            workspace: PathBuf::from(workspace),
            scheme: "MyApp".into(),
            device: None,
            os: None,
            configuration: None,
            preflight: None,
            oslog: false,
            oslog_predicate: None,
            terminate_on_stop: true,
            build_output: BuildOutput::Filtered,
            verbose_logging: false,
            derived_data: None,
        };
        let args = |workspace: &str| -> Vec<String> {
            base_cmd(&cfg(workspace))
                .as_std()
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(
            args("MyApp.xcworkspace")[..2],
            ["-workspace", "MyApp.xcworkspace"]
        );
        // A bare .xcodeproj must go through -project (xcodebuild exits 66
        // when it is passed via -workspace).
        assert_eq!(
            args("MyApp.xcodeproj")[..2],
            ["-project", "MyApp.xcodeproj"]
        );
        // The project's inner workspace is still a workspace.
        assert_eq!(
            args("MyApp.xcodeproj/project.xcworkspace")[..2],
            ["-workspace", "MyApp.xcodeproj/project.xcworkspace"]
        );
    }

    #[test]
    fn app_path_missing_is_an_error() {
        let json = serde_json::json!([
            { "buildSettings": { "WRAPPER_NAME": "Lib.framework",
                                  "TARGET_BUILD_DIR": "/dd" } }
        ]);
        assert!(parse_app_path(serde_json::to_vec(&json).unwrap().as_slice()).is_err());
    }

    #[test]
    fn parse_build_root_takes_build_dir_up_two_levels() {
        // BUILD_DIR = <build_root>/Build/Products -> build_root two up.
        let json = serde_json::json!([
            { "buildSettings": { "BUILD_DIR":
                "/Users/x/Library/Developer/Xcode/DerivedData/MyApp-abc/Build/Products" } }
        ]);
        assert_eq!(
            parse_build_root(serde_json::to_vec(&json).unwrap().as_slice()).unwrap(),
            PathBuf::from("/Users/x/Library/Developer/Xcode/DerivedData/MyApp-abc")
        );
        // No BUILD_DIR anywhere -> error.
        let json = serde_json::json!([{ "buildSettings": { "OTHER": "x" } }]);
        assert!(parse_build_root(serde_json::to_vec(&json).unwrap().as_slice()).is_err());
    }

    #[test]
    fn build_root_from_app_matches_showbuildsettings_derivation() {
        // .app up four levels == the -showBuildSettings BUILD_DIR/../.. root.
        let app = Path::new(
            "/Users/x/Library/Developer/Xcode/DerivedData/MyApp-abc/Build/Products/\
             Debug-iphonesimulator/MyApp.app",
        );
        assert_eq!(
            build_root_from_app(None, app).unwrap(),
            PathBuf::from("/Users/x/Library/Developer/Xcode/DerivedData/MyApp-abc")
        );
        // Explicit derivedData short-circuits, ignoring the .app path.
        assert_eq!(
            build_root_from_app(Some(Path::new("/Users/x/dd")), app).unwrap(),
            PathBuf::from("/Users/x/dd")
        );
    }

    fn cfg_with_derived_data(derived_data: Option<&str>) -> LaunchConfig {
        LaunchConfig {
            workspace: PathBuf::from("MyApp.xcworkspace"),
            scheme: "MyApp".into(),
            device: None,
            os: None,
            configuration: None,
            preflight: None,
            oslog: false,
            oslog_predicate: None,
            terminate_on_stop: true,
            build_output: BuildOutput::Filtered,
            verbose_logging: false,
            derived_data: derived_data.map(PathBuf::from),
        }
    }

    #[test]
    fn base_cmd_appends_derived_data_path_when_set() {
        let args = |cfg: &LaunchConfig| -> Vec<String> {
            base_cmd(cfg)
                .as_std()
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect()
        };
        // Absent -> no -derivedDataPath flag at all.
        assert!(!args(&cfg_with_derived_data(None))
            .iter()
            .any(|a| a == "-derivedDataPath"));
        // Present -> the flag followed by the path.
        let with = args(&cfg_with_derived_data(Some("/Users/x/dd")));
        let i = with
            .iter()
            .position(|a| a == "-derivedDataPath")
            .expect("-derivedDataPath present");
        assert_eq!(with[i + 1], "/Users/x/dd");
    }

    #[test]
    fn cache_key_varies_with_derived_data() {
        let ws = Path::new("/Users/x/MyApp.xcworkspace");
        let none = settings_cache_key(ws, &cfg_with_derived_data(None), "UDID");
        let some = settings_cache_key(ws, &cfg_with_derived_data(Some("/Users/x/dd")), "UDID");
        assert_ne!(none, some);
        // Same derivedData -> same key (cache hit).
        assert_eq!(
            some,
            settings_cache_key(ws, &cfg_with_derived_data(Some("/Users/x/dd")), "UDID")
        );
    }
}
