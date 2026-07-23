# xcode-dap: Rust DAP-Proxy Engine + v1 Implementation Plan (ZedXcode)

> **Design snapshot.** This document is the pre-implementation design and is
> kept for historical context. Where it drifts from the code, the shipped
> sources are authoritative: `crates/xcode-dap-config/src/lib.rs` and
> `extension/debug_adapter_schemas/Xcode.json` for the config surface,
> `crates/xcode-dap/src/` for behavior.

## 0. New verified findings (deltas to established ground truth)

These were verified during this design pass against live sources:

1. **lldb-dap proxy flow:**
   - lldb-dap is spawned **at `initialize` time**, and the client's initialize message is forwarded **verbatim**; lldb-dap's own initialize response (and thus its real capabilities) flows back to the client untouched. No capability merging anywhere.
   - **No seq renumbering of client traffic.** Proxy-generated requests use a private seq namespace starting at `FirstSeq = 1_000_000`. Responses from lldb-dap with `request_seq >= 1_000_000` are intercepted: the attach response gets its `request_seq` rewritten to the client's original launch seq **and its `command` field rewritten to `"launch"`** (see §3.3 / `peek.rs::rewrite_attach_response`), then is forwarded as the launch response; all other proxy-internal responses are dropped.
   - **Simulator attach = plain `{"pid": N}`**, preceded by a separate `evaluate` request (`context: "repl"`) of `platform select ios-simulator`. `attachCommands` is only used for physical devices. This supersedes the earlier "attachCommands or pid — decide" question: use `pid`.
   - Proxy-generated `output` events use `seq: 0` — proven fine with Zed.
   - We connect to lldb-dap over **stdio**, not a TCP port (`--connection listen://localhost:PORT`). The TCP route exists only to sidestep slow stdio buffering in Swift's `Subprocess`; Rust/tokio pipes have no such problem.
   - **PID discovery**: the primary path parses `"<bundle>: <pid>"` from simctl `--stdout/--stderr` output (verified earlier). The fallback polls `ps aux` filtered on `CoreSimulator/Devices/<udid>/` + `/<App>.app/`, snapshotting the pre-launch PID first. The `--console-pty` mode is avoided: simctl does *not* print the pid line when run without a terminal under `--console-pty`.
   - **Pidfile per sim** (`sim-<udid>.pid`): ownership claimed after a successful pipeline run (the udid is known only post device-resolution), SIGTERMing a stale previous instance then, since a Rerun can race the old session's teardown; removed on teardown. A separate **pre-install SIGTERM** (no ownership taken) supersedes the predecessor before `simctl install` — which otherwise blocks while the old debugged app is alive. See §3.3.
   - *(Unimplemented idea.)* Relaying lldb-dap `progressStart`/`progressEnd` events as plain output lines was considered but never built; today those events pass through byte-verbatim like any other child frame.
2. **`dap` crate is unusable**: `dap` (sztomi/dap-rs) is at `0.1.0-alpha1`, self-described as early-stage with frequent breaking changes → hand-roll (see §3.3).
3. **Zed `dap.<NAME>.binary` settings override is honored for extension adapters**: `crates/debug_adapter_extension/src/extension_dap_adapter.rs::get_binary` passes `user_installed_path` through to the extension's `get_dap_binary`; **`user_args` is ignored** (`_user_args`) for extension adapters. So dev override = `{"dap": {"Xcode": {"binary": "/abs/path/target/debug/xcode-dap"}}}` — binary only, no args. A **dev extension is still required** for in-Zed testing (the adapter name must resolve in the registry); there is no extension-less custom adapter path. Pre-extension testing is done with a scripted DAP harness (§8).
4. **WASM target is `wasm32-wasip2`** (`crates/extension/src/extension_builder.rs`: `const RUST_TARGET: &str = "wasm32-wasip2"`), and Zed's builder auto-installs the target via rustup (`install_rust_wasm_target_if_needed`) — requires a rustup-managed toolchain.
5. **No clear-console action exists in Zed's debugger**: the `console` action namespace contains only `WatchExpression` (`crates/debugger_ui/src/session/running/console.rs`), and the debugger docs mention none. **Decision: CMD+K stays untouched** (chords survive, `terminal::Clear` default remains for terminals); document "Debug Console cannot be cleared in current Zed" as a known limitation in README.

## 1. Repository layout

```
ZedXcode/
├── Cargo.toml                      # workspace members = ["crates/xcode-dap", "crates/xcode-dap-config"]
│                                   #  (extension/ is EXCLUDED — Zed's builder compiles it independently for wasm32-wasip2)
├── crates/xcode-dap/
│   ├── Cargo.toml                  # tokio (rt-multi-thread, process, io-util, sync, signal, time, fs, macros),
│   │                               # tokio-util, serde, serde_json, clap (derive), anyhow, flate2, libc, log
│   │                               # (no plist crate — bundle-id is read via a plutil shell-out)
│   └── src/
│       ├── main.rs                 # clap dispatch: no subcommand → dap mode
│       ├── dap/
│       │   ├── framing.rs          # Content-Length codec (read + write)
│       │   ├── peek.rs             # minimal "look but don't re-encode" message inspection
│       │   ├── proxy.rs            # state machine, routing, seq namespace
│       │   └── lldb.rs             # spawn xcrun lldb-dap, stdio plumbing
│       ├── engine/
│       │   ├── config.rs           # LaunchConfig re-export (schema lives in crates/xcode-dap-config)
│       │   ├── pipeline.rs         # preflight→buildServer→build→install→launch→pid→ingest (shared by dap + CLI)
│       │   ├── xcodebuild.rs       # build/clean/showBuildSettings, output filter/throttle
│       │   ├── simctl.rs           # device resolution, boot, install, launch, terminate, pid fallback
│       │   ├── consoles.rs         # stdout/stderr file tailers, optional oslog pump
│       │   ├── compile_store.rs    # persistent per-(build_root,scheme) compile-args store (bsp)
│       │   ├── xcactivitylog.rs    # parse Xcode's .xcactivitylog build logs into compile args
│       │   └── selection.rs        # .zed/.zedx/selection.json scheme/device overlay
│       ├── bsp/
│       │   ├── mod.rs
│       │   ├── server.rs           # sourcekit-lsp Build Server (`xcode-dap bsp`)
│       │   └── ingest.rs           # fold build logs into the compile store, watermark/merge
│       ├── setup/
│       │   ├── jsonc.rs            # marker-block surgical merge (port of verified Python design)
│       │   ├── user.rs             # keymap.json / settings.json blocks
│       │   ├── project.rs          # .zed/debug.json, .zed/tasks.json, .git/info/exclude
│       │   └── build_server.rs     # buildServer.json generator (argv → `xcode-dap bsp`)
│       ├── commands/               # build.rs run.rs clean.rs console.rs select.rs setup.rs refresh.rs doctor.rs
│       └── util/{procgroup.rs, pidfile.rs, logging.rs, hash.rs, paths.rs}
├── crates/xcode-dap-config/        # shared serde LaunchConfig (compiles for both wasm32-wasip2 and the proxy)
├── extension/                      # sibling agent's scope: extension.toml, src/lib.rs (get_dap_binary →
│                                   #  prefer user_installed_path, else download release / find on PATH)
└── tests/dap_smoke.py + harness    # scripted DAP client (see §8)
```

## 2. Runtime decision: tokio (multi-thread), not std threads

The proxy juggles **seven concurrent I/O sources** with cross-cutting cancellation: client stdin, client stdout (single-writer), lldb-dap child stdin/stdout, app stdout+stderr file tailers, xcodebuild output stream, optional oslog pump, SIGTERM/SIGINT, and timers (init guard, PID poll). The hard requirement that tips it to async: **mid-build cancellation** — while `xcodebuild` runs (minutes), the proxy must keep reading client stdin to catch `disconnect` and kill the build. With threads that's a tangle of flags + `kill()` from another thread; with tokio it's one `select!` over `build_task` vs `client_rx`. `tokio::process::Child` with `kill_on_drop(true)` also gives free zombie protection. Cost (binary ~3–4 MB, one dep tree) is acceptable for a dev tool. Use `#[tokio::main]` default multi-thread runtime; all tasks are I/O-bound.

**Single-writer invariant**: every byte to Zed goes through one mpsc channel → one writer task (DAP frames must never interleave):

```rust
enum Out { Raw(Bytes),                       // verbatim passthrough frame from lldb-dap
           Msg(serde_json::Value) }          // proxy-built message (gets framed on write)

async fn stdout_writer(mut rx: mpsc::Receiver<Out>) { /* frame + write + flush */ }
```

Same pattern for the lldb-dap child stdin (`mpsc::Sender<Out>`).

## 3. DAP layer

### 3.1 Framing (`framing.rs`)

Hand-rolled buffer loop:

```rust
pub struct DapReader<R: AsyncRead + Unpin> { inner: BufReader<R>, buf: Vec<u8> }
impl<R> DapReader<R> {
    /// Reads exactly one message body (raw JSON bytes). Handles multiple
    /// messages per read() and split headers. Header: "Content-Length: N\r\n\r\n".
    pub async fn next_message(&mut self) -> anyhow::Result<Option<Bytes>>;
}
pub fn frame(body: &[u8]) -> Vec<u8>   // "Content-Length: {n}\r\n\r\n" + body
```

Tolerate extra headers before the blank line (spec allows them) even though lldb-dap only sends Content-Length.

### 3.2 Peek layer (`peek.rs`) — why no DAP crate

Passthrough must be **byte-transparent**: a typed decode→re-encode roundtrip risks dropping fields lldb-dap or Zed adds outside the spec. So: parse to `serde_json::Value` only to *inspect*, forward the **original bytes**. The `dap` crate (0.1.0-alpha1, breaking-changes warning) and `dap-types` would push toward typed roundtrips — rejected. Total hand-rolled surface is ~6 constructors + 1 classifier:

```rust
pub enum ClientMsg<'a> {
    Initialize { raw: &'a [u8] },
    Launch     { seq: i64, args: serde_json::Value },   // OUR scenario config
    Disconnect { raw: &'a [u8] },
    Other      { raw: &'a [u8] },
}
pub enum ChildMsg<'a> {
    InternalResponse { request_seq: i64, raw: &'a [u8] },  // request_seq >= SEQ_BASE
    Other            { raw: &'a [u8] },
}
pub const SEQ_BASE: i64 = 1_000_000;

// builders (all return serde_json::Value):
pub fn output_event(category: &str, text: &str) -> Value           // seq: 0
pub fn evaluate_repl(expr: &str, seq: i64) -> Value                 // {"command":"evaluate","arguments":{"expression":expr,"context":"repl"},...}
pub fn attach_pid(pid: i64, seq: i64) -> Value
pub fn error_response(request_seq: i64, command: &str, msg: &str) -> Value
pub fn terminated_event() -> Value
```

Interception trigger is simply `command == "launch"`: we own the adapter, so every launch is ours — no sentinel `program` marker needed.

### 3.3 State machine (`proxy.rs`)

```rust
struct Proxy {
    to_client: mpsc::Sender<Out>,
    lldb: Option<LldbDap>,            // spawned at initialize
    launch_seq: Option<i64>,          // client's launch request seq
    attach_seq: Option<i64>,          // our internal attach seq
    next_seq: i64,                    // starts at SEQ_BASE
    session: Option<SessionState>,    // udid, bundle_id, pids, tailer handles, config
    disconnecting: bool,
}
```

Message flow:

```
Zed ── initialize ──────────────▶ spawn `xcrun lldb-dap` (stdio); forward initialize verbatim
Zed ◀───────────── init response ── passthrough (REAL lldb-dap capabilities; no merging)
Zed ── launch {workspace,scheme,…}─▶ INTERCEPT. launch_seq = seq. Run pipeline (§4):
        each phase streams output events (category "console") to Zed.
        FAILURE → error_response(launch_seq,"launch",msg) + output(stderr) + terminated_event → exit(1)
        SUCCESS → pid known; then to lldb-dap:
            evaluate_repl("platform select ios-simulator", take_seq())
            attach_pid(pid, attach_seq = take_seq())
lldb-dap ── initialized event ──▶ passthrough          # fires only AFTER target exists
Zed ── setBreakpoints*, configurationDone ─▶ passthrough  # auto-continue on configurationDone
lldb-dap ── response{request_seq==attach_seq} ─▶ rewrite request_seq→launch_seq
                                                  (also rewrite "command":"launch" — not strictly
                                                   required, but it costs nothing and is spec-correct);
                                                  forward; emit output "Debugger attached"/"Debugger attach failed"
lldb-dap ── response{request_seq>=SEQ_BASE, other} ─▶ DROP (forwarding would confuse the client's seq accounting)
everything else, both directions ─▶ byte-verbatim passthrough, no renumbering
app stdout/stderr tailers ─▶ output events (category "stdout"/"stderr"), interleaved
```

**Breakpoint timing — no queue needed.** Zed only sends `setBreakpoints` in reaction to the adapter's `initialized` event, and lldb-dap emits `initialized` only after the attach creates a target. During the (long) build there is nothing to queue — the DAP handshake itself gates the client. This is why no breakpoint queue is needed, and it works under Zed.

**Other states:**
- *Init guard*: if no `initialize` arrives within 2 s of entering dap mode, print "Running in DAP mode but initialize not received — did you mean a subcommand? Try --help" and exit 1 (great accidental-invocation UX).
- *Disconnect/Stop*: forward `disconnect` to lldb-dap; lldb-dap's `terminate`/`disconnect(terminateDebuggee)` handling kills or detaches per Zed's request. The forward is **bounded** (`DISCONNECT_WEDGE_GRACE`, a few seconds): against the simulator debugserver lldb-dap can kill its debuggee on disconnect and then **wedge without exiting** — especially while a concurrent Rerun's `simctl install` contends on the same bundle — which would hang the routing loop until Zed force-kills the adapter (the perceived crash). If lldb-dap has not exited by the deadline, the proxy answers the disconnect itself and shuts down. After lldb-dap exits (or on our own teardown), belt-and-braces: `xcrun simctl terminate <udid> <bundleId>` (ignore failure) — mirrors Xcode's Stop semantics; config flag `"terminateOnStop": true` (default).
- *Disconnect after lldb-dap ended the session*: if an `exited`/`terminated` event was seen **before** the disconnect — e.g. a second run's `simctl install` replaced the running app's bundle, killing it, and lldb-dap's `terminated` is what made Zed disconnect — the proxy does **not** forward-and-wait for lldb-dap to exit. Against the simulator debugserver lldb-dap can wedge on a disconnect once its debuggee is gone, so the routing loop would wait forever for an exit that never arrives and Zed force-kills the adapter (perceived as a crash, with no teardown logged). Instead the proxy answers the disconnect itself and exits 0 — teardown's bounded lldb-dap wait-then-kill reaps a wedged child. The belt-and-braces `simctl terminate` is skipped **only** when an `exited` event proved the process is gone (so we never step on a successor's freshly launched app). A plain `terminated` is a *detach*, not a kill — lldb-dap detaches an attach-by-pid session on disconnect (ignoring `terminateDebuggee`), leaving the app alive — so that path (and every ordinary Stop) still terminates the app per `terminateOnStop`.
- *Stop mid-build*: `select!` in pipeline races build vs client messages; on `disconnect` → kill xcodebuild **process group** (spawn with `setpgid`, kill `-pgid` so `swift-frontend` children die too), respond success to disconnect, `terminated_event()`, exit 0.
- *lldb-dap exits* → exit (Zed usually kills us first).
- *stdin EOF* → cleanup, exit 0. *SIGTERM/SIGINT* (tokio::signal) → **emit a `terminated` event first when a session is active** (`announce_superseded`), so the adapter's exit reads as a clean session end rather than a crash, then the same cleanup path — which terminates our still-owned app, unblocking a superseding run's install. Cleanup = kill children (`kill_on_drop` + explicit pgid kills), drop tailers, remove pidfile.
- *Pidfile / supersede*: ownership is claimed **after a successful pipeline run** (post device-resolution — the udid is not known at launch-intercept): read `~/.zedxcode/run/sim-<udid>.pid`, SIGTERM a stale previous instance, write own pid; removed on teardown. Additionally, in DAP mode a **pre-install SIGTERM** goes to the predecessor right after a successful build and **before `simctl install`** (via `pidfile::kill_old`, which signals **without** taking ownership): on the simulator `install` *blocks* while a previous session's app is still running under lldb (it does not replace or kill it — empirically it stalls until that app dies), so a Rerun would otherwise hang for as long as the old app lives. Because the early signal does not write our pid, the predecessor still owns the pidfile when it tears down (`superseded() == false`) and therefore terminates **its own** app, unblocking our install; the post-launch ownership still guards a launched successor's app from a late predecessor teardown (the 2026-07-16 audit invariant).

## 4. Pipeline (`engine/pipeline.rs`) — shared by dap mode and CLI

```rust
pub struct LaunchConfig {              // = flattened scenario `config` from Zed
    pub workspace: PathBuf,            // YourApp.xcworkspace (or project)
    pub scheme: String,                // "YourApp"
    pub device: Option<String>,        // "iPhone 15 Pro Max" | udid; None = booted iPhone, else newest available
    pub os: Option<String>,            // "26.3" — optional narrowing
    pub configuration: Option<String>,
    pub preflight: Option<String>,     // e.g. "make project CI=true" (written by setup for generated projects)
    pub oslog: bool, pub terminate_on_stop: bool,
    pub build_output: BuildOutput,     // Filtered (default) | Full
}

pub async fn run_pipeline(cfg: &LaunchConfig, sink: &dyn OutputSink, cancel: CancellationToken)
    -> anyhow::Result<LaunchedApp>     // { pid, udid, bundle_id, app_path, stdout_file, stderr_file }
```

`OutputSink` abstracts "where lines go": dap mode → output events; CLI mode → plain stderr/stdout. Phases (each line below is an existing verified mechanic, now codified):

1. **Preflight**: if workspace missing and `preflight` set → run it (for a Tuist-generated project this is the project-generation command, e.g. `make project CI=true`; setup writes it; the binary itself never invents one).
2. **Resolve simulator** (`simctl.rs`): `xcrun simctl list devices --json`; match by udid-or-name (+ optional OS runtime match); prefer `Booted`; ambiguity → deterministic sort. Not booted → `xcrun simctl boot <udid>` (tolerant of "already booted/booting", retrying a "Shutting Down" race) → `open -a Simulator` (visible window) → `xcrun simctl bootstatus <udid>` (blocks until ready; **no `-b`** — booting via Simulator.app first plus a `bootstatus -b` inner boot fails with SimError 405).
2a. **Ensure build server** (`ensure_build_server`, opt-in-gated): when `buildServer.json` under the workspace parent is stale/missing, regenerate it (may print the `editor: restart language server` hint). See [`bsp-server.md`](bsp-server.md).
3. **Build** (`xcodebuild.rs`): `xcodebuild -workspace <ws> -scheme <scheme> -destination platform=iOS Simulator,id=<udid> build` — DerivedData defaults to xcodebuild's standard per-workspace location (override with the `derivedData` config field / `--derived-data`). That directory is where index-while-building writes the native index store and the `.xcactivitylog` build logs the built-in Build Server reads for navigation (see [`bsp-server.md`](bsp-server.md)), so it must not be relocated per-invocation. **Never** `CODE_SIGNING_ALLOWED=NO`. Stream stdout/stderr merged through the throttle (§5.2). Non-zero exit → extract `error:` lines + tail, fail pipeline.
4. **App path**: `xcodebuild ... -showBuildSettings -json` → `TARGET_BUILD_DIR` + `WRAPPER_NAME` (cache per (workspace,scheme,udid) keyed on mtime to skip the ~2s call on rebuilds). **Bundle id**: `plutil -extract CFBundleIdentifier raw <app>/Info.plist` (shell-out; avoids a plist crate dep).
4a. **Ingest build log** (`ingest_build_log`): fold the just-captured build log into the per-`(build_root, scheme)` compile store so the built-in `bsp` server can answer sourcekit-lsp's per-file compile-args queries. See [`bsp-server.md`](bsp-server.md).
5. **Install**: `xcrun simctl install <udid> <app>`.
6. **Launch**: `xcrun simctl launch --wait-for-debugger --terminate-running-process --stdout=$RUN/out.log --stderr=$RUN/err.log <udid> <bundle>` where `$RUN = ~/.zedxcode/run/<udid>/` (absolute paths; pre-truncate files). Never pass `--console-pty` (incompatible with --stdout/--stderr).
7. **PID**: parse `"<bundle>: <pid>"` from simctl stdout. Fallback: snapshot newest matching pid from `ps aux | grep CoreSimulator/Devices/<udid>/.../<App>.app/` **before** launch, poll 5×1 s for a changed pid after.
8. Start **tailers** on out.log/err.log; optionally the oslog pump.

## 5. Console design

### 5.1 App output → Debug Console (the GUI-first win)

`consoles.rs`: per file, a tokio task does poll-reads (open file, loop: `read_to_end` of new bytes every 75 ms — kqueue is unreliable for files appended by another process; polling is what works). Buffer to whole lines; emit `output_event("stdout"|"stderr", line)`. Stops on session teardown; final drain before exit.

### 5.2 Build output: filter + throttle (flood control)

A full xcodebuild log is 10⁵–10⁶ lines; dumping it as one-event-per-line will flood Zed's console editor. Two-stage control in `xcodebuild.rs`:
- **Filter** (default `BuildOutput::Filtered`): pass through phase headers (`=== BUILD`, `Compiling`, `Linking`, target banners), all `warning:`/`error:`/`note:` diagnostics with their context lines, and the final `BUILD SUCCEEDED/FAILED`. Everything always goes to `~/.zedxcode/logs/build-latest.log`; on failure emit a pointer line. `"buildOutput": "full"` disables filtering.
- **Throttle**: batch lines into a single output event flushed every 50 ms or at 8 KB, whichever first (one event per batch, newline-joined). This caps event rate at ~20/s regardless of log volume.

### 5.3 OSLog (flag, off by default)

`"oslog": true` → after PID is known, spawn `xcrun simctl spawn <udid> log stream --style compact --color none --level debug --predicate <predicate>`, pump lines as `output_event("console", ...)`. The predicate scopes the stream to the app by default (`subsystem == <bundle id>` OR an image inside the `.app` bundle — `consoles.rs::default_oslog_predicate`) and is overridable via the `oslogPredicate` config field. Known noise concern → off by default; when oslog is on it can duplicate lldb-dap output events — defer that dedup to v1.1, document the overlap instead.

### 5.4 CMD+K

Verified: **no clear-console action exists** in Zed's debugger (console namespace = `WatchExpression` only). Decision: do not rebind anything; `cmd-k` chord prefix and `cmd-k`-in-terminal behavior stay default. README documents the limitation and links the upstream feature gap.

### 5.5 Diagnostic log file (`util/logging.rs`)

`util::logging::init(mode)` installs a `log`-facade file logger writing to `~/.zedxcode/logs/xcode-dap.log`; line format `<UTC ISO ts> LEVEL [pid <pid> <mode>] message`, where `mode` is `dap` or the subcommand name. ERROR records additionally tee to stderr (Zed's debug-adapter log view). The level comes from the `XCODE_DAP_LOG` env var (`error|warn|info|debug|trace`, default `info`); the `verboseLogging` scenario key raises a session to `trace` (never lowers). Rotation is one generation: a file over 5 MB is renamed to `xcode-dap.log.old` at init. Init failures silently disable logging — diagnostics must never break a DAP session or touch stdout. Content levels: INFO = session header (version, git hash, build timestamp, argv, binary-resolution source), pipeline milestones and engine shell-outs with exit status + duration; DEBUG = DAP frame summaries both directions and raw build/oslog/app stream lines; TRACE = full DAP frame bodies truncated to 2 KB. Hygiene: never log env vars wholesale.

## 6. CLI subcommands (clap; same engine)

| Command | Behavior |
|---|---|
| *(none)* | DAP proxy mode (with 2 s initialize guard) |
| `build --workspace --scheme [--device] [--full-output]` | Pipeline phases 1–4 only; exit code = xcodebuild's. This is what `.zed/tasks.json` "Xcode: Build" (CMD+B) calls |
| `run` | Phases 1–8 without debugger: launch *without* `--wait-for-debugger`, stream console to terminal |
| `clean` | `xcodebuild -workspace … -scheme … clean` (CMD+Shift+K task) |
| `console [-f/--follow]` | Print (or tail) the current run's app console logs from `~/.zedxcode/run/<udid>/{out,err}.log` |
| `select-scheme` / `select-device` | Interactive pickers (or `--set`/`--list`) writing the `.zed/.zedx/selection.json` overlay used by the next run; `select-scheme` also regenerates `buildServer.json` for the new scheme |
| `setup [--project <dir>] [--user] [--yes]` | §6.1 |
| `refresh` | Re-run preflight (Tuist project regeneration) + touch buildServer.json + print "restart LSP" hint (`editor: restart language server`) |
| `doctor` | Checks: Xcode + `xcrun -f lldb-dap`, simctl works, requested sim exists/booted, sourcekit-lsp, `buildServer.json` present + fresh + `argv` launching the built-in `bsp` server + recorded `build_root`/scheme still valid, compile-store health, rustup (dev), pidfile staleness, binary version vs extension expectation |
| `bsp` (hidden) | Built-in sourcekit-lsp Build Server on stdio, spawned via `buildServer.json`; answers per-file compile-args queries from the compile store. Not invoked by hand. See [`bsp-server.md`](bsp-server.md) |

### 6.1 `setup` — port the verified JSONC marker-merge to Rust (decision: Rust, not Python)

One binary, zero runtime deps (host python3 is 3.9 and another dep to doctor) — and the merge logic is mechanical text surgery, not parsing-heavy. Port the existing verified design 1:1:

```rust
// setup/jsonc.rs
pub fn merge_marker_block(path: &Path, marker_id: &str, block: &str) -> Result<MergeOutcome>
// 1. read file; timestamped backup "<file>.zedxcode-backup-<ts>"
// 2. if "// >>> zedxcode:<id> >>>" .. "// <<< zedxcode:<id> <<<" exists → replace inner text
// 3. else find insertion point: scan from EOF backwards for the final ']' / '}' of the
//    top-level value using a tolerant scanner (tracks "strings", // and /* */ comments);
//    insert ",\n" + block before it (handles JSONC trailing commas/comments without a parser)
// 4. atomic write (tmp + rename)
```

User-level (`--user`): keymap.json gets the marker block binding `cmd-r → debugger::Rerun`, `cmd-b`/`cmd-shift-k` → `task::Spawn {"task_name": "Xcode: Build"/"Xcode: Clean"}`, `cmd-shift-o → project_symbols::Toggle` — exactly the bindings already verified to coexist with `cmd-k` chords. Project-level: `.zed/debug.json` (adapter "Xcode" scenario with the project's workspace/scheme/device + the detected `"preflight"` command for generated projects), `.zed/tasks.json` (Build/Clean/Refresh/Console + Choose Scheme/Choose Destination pickers, all invoking `xcode-dap` by absolute binary path — Zed task shells don't have a dev install on PATH), `buildServer.json` written by the pure-Rust generator (`setup/build_server.rs`), whose `argv` points back at `xcode-dap bsp` — the toolkit's own built-in Build Server for sourcekit-lsp (see [`bsp-server.md`](bsp-server.md)) — append generated files to `.git/info/exclude`. Idempotent re-runs (marker replace), `--yes` for non-interactive. `--oslog` writes `"oslog": true` into the generated debug.json; without the flag a re-run preserves the value already in the existing file (enabled oslog is never silently reset).

## 7. Extension interface contract (for the sibling agent)

- Adapter name `Xcode`; extension's `get_dap_binary` must **prefer `user_provided_debug_adapter_path`** (this is how the `dap.Xcode.binary` dev override arrives — verified; note args override is ignored by Zed for extension adapters), else find `xcode-dap` on PATH, else download the GitHub release.
- Proxy speaks DAP on **stdio**, no args needed for dap mode (`tcp_connection` unused).
- Launch request `arguments` = flattened scenario `config` = `LaunchConfig` (§4); `dap_request_kind` → Launch. Ship `debug_adapter_schemas/Xcode.json` (capitalized — Zed derives the default schema path from the adapter name `Xcode`, so a lowercase file would fail adapter registration) mirroring `LaunchConfig`.
- Version handshake: extension passes `--zed-extension-version <v>` env or arg is *not* needed in v1; `doctor` compares `xcode-dap --version` against a minimum the extension prints in errors.

## 8. Testing: DAP-level smoke harness (no Zed needed)

`tests/dap_smoke.py` (host python3 is fine for tests; the *product* stays pure Rust): a minimal DAP client that spawns `xcode-dap`, frames JSON with Content-Length, and asserts a scripted session:

```
initialize → expect response success:true with lldb-dap capabilities (proves spawn+forward)
launch {workspace: YourApp.xcworkspace, scheme: "YourApp", device: "iPhone 15 Pro Max"}
  → collect output events (build phases) → expect initialized event (proves attach)
setBreakpoints (a file in the app) → expect verified breakpoints
configurationDone → expect process/stopped-or-running; expect app stdout/stderr output events
disconnect → expect response, process exit 0, app terminated on sim, no zombie children (ps check)
```

Plus pure-Rust unit tests: framing codec (split headers, multi-message reads), peek classifier, seq rewrite, jsonc merge (fixtures with comments/trailing commas/existing markers), build-log filter. A `--mock-pipeline` hidden flag (skip xcodebuild, attach to a locally spawned dummy process via lldb-dap) makes the full DAP loop CI-testable without Xcode in <5 s.

## 9. Implementation order — verifiable gates

| # | Work | Gate (verifiable) |
|---|---|---|
| 0 | `rustup` install (stable); `cargo new` workspace; commit scaffold + Apache-2.0 license | `cargo build` green; `xcode-dap --help` prints |
| 1 | framing.rs + peek.rs + transparent proxy (spawn lldb-dap, pure passthrough, no interception) + init guard | Python harness: initialize/disconnect roundtrip against real `xcrun lldb-dap`; unit tests green |
| 2 | engine: simctl.rs, xcodebuild.rs, pipeline.rs; CLI `build`/`run`/`clean` | `xcode-dap build -w YourApp.xcworkspace -s "YourApp"` succeeds on a large Tuist-generated project (incl. the missing-workspace preflight); `run` shows the app on a booted iPhone sim with console in terminal |
| 3 | dap mode launch interception + attach + tailers + teardown + pidfile + cancellation | Full §8 smoke script passes incl. breakpoint hit + Stop-mid-build (send disconnect 5 s into build → xcodebuild pgid dead) |
| 4 | minimal dev extension (wasip2, registers "Xcode", schema) — coordinate with sibling agent | `zed: install dev extension` + `dap.Xcode.binary` override → New Session shows "Xcode"; first run via modal, then **CMD+R = debugger::Rerun** rebuilds+relaunches+reattaches with console in Debug Console (manual keybinding for now) |
| 5 | `setup` (jsonc merge port, user + project), `doctor`, `refresh`; tasks for CMD+B / CMD+Shift+K | Fresh-machine dry run on a real iOS project: setup → CMD+R/CMD+B/CMD+Shift+K/CMD+Shift+O all work; cmd-k chords still work; backups created; re-run idempotent |
| 6 | oslog flag, build-log filter polish | oslog stream interleaves in the Debug Console; filtered build output stays readable, full log on disk |
| 7 | Release: GitHub Actions (macOS arm64+x86_64, `codesign -s -` ad-hoc, tar.gz + sha256), extension download path, README, publish PR to zed-industries/extensions | Clean machine: install from registry → setup → CMD+R works without local cargo |

## 10. Risks (proxy-specific)

| Risk | Likelihood/Impact | Mitigation |
|---|---|---|
| lldb-dap protocol drift across Xcode versions (e.g. old `-p` vs `--connection`; capability changes) | Med/Med | We use stdio (no port-syntax dependence); forward real capabilities (no hardcoding); doctor prints lldb-dap version; CI smoke against installed Xcode |
| Seq collisions / renumbering bugs | Low/High | SEQ_BASE=1M namespace (verified under Zed), zero client renumbering; unit test: client seq 999_999 boundary; drop-list only for `request_seq>=SEQ_BASE` |
| Zed rejects synthesized launch response (`command:"attach"` mismatch) | Low/Med | Rewrite `command` to `"launch"` too (not strictly required, we do it anyway); smoke test asserts Zed-equivalent client accepts |
| Breakpoint timing regression (Zed sends setBreakpoints before `initialized`) | Low/High | Passthrough means lldb-dap would answer with error; smoke test covers; if observed, add a pre-attach queue for `setBreakpoints`/`setExceptionBreakpoints` (isolated change in proxy.rs) |
| Stop mid-build leaves xcodebuild/swift-frontend running | Med/Med | setpgid + kill(-pgid); `kill_on_drop`; SIGTERM handler shares the same teardown; smoke gate 3 asserts |
| Zombie lldb-dap / stale proxy on Rerun race | Med/Low | pidfile SIGTERM; `kill_on_drop`; doctor reports stale pidfiles |
| Build log floods Debug Console | High/Med | Filter mode default + 50 ms/8 KB batching (§5.2); full log on disk |
| PID parse fails (simctl output change / non-tty behavior under --console-pty) | Low/High | We use --stdout/--stderr mode (pid line verified); ps-poll fallback with pre-launch snapshot |
| `--wait-for-debugger` hang if attach fails (app stuck stopped) | Med/Med | **Not implemented as a dedicated attach timeout** (known gap): the proxy waits for lldb-dap's attach response with no per-attach deadline (the only timeouts are `INIT_GUARD` 2 s, `TEARDOWN_GRACE` 2 s, `PIPELINE_DRAIN_GRACE` 10 s). An attach failure surfaces when lldb-dap answers; the suspended app is cleaned up at teardown via `terminateOnStop` |
| Gatekeeper/quarantine or missing signature on downloaded release binary | Med/Med | Zed's HTTP client doesn't set com.apple.quarantine; arm64 needs *some* signature → ad-hoc `codesign -s -` in CI; doctor checks `spctl`/runnability; fallback doc: `cargo install` from source |
| oslog pump noise/duplication with stdout events | Med/Low | Off by default; documented; dedup deferred to v1.1 |
| wasip2 assumption breaks on user's Zed version | Low/Low | Zed's builder picks the target itself and auto-installs it; we only require rustup-managed toolchain |

## 11. Citations

- Zed `dap.<NAME>` binary override + extension flow: [zed.dev/docs/debugger](https://zed.dev/docs/debugger), [debugger-extensions](https://zed.dev/docs/extensions/debugger-extensions), `crates/debug_adapter_extension/src/extension_dap_adapter.rs` (user_installed_path passed, `_user_args` ignored), `crates/dap/src/adapters.rs::get_binary`.
- wasm target: `crates/extension/src/extension_builder.rs` (`RUST_TARGET = "wasm32-wasip2"`).
- No console-clear action: `crates/debugger_ui/src/session/running/console.rs` (`actions!(console, [WatchExpression])`); default-macos.json debugger bindings.
- DAP crate maturity: [crates.io/crates/dap 0.1.0-alpha1](https://crates.io/crates/dap/0.1.0-alpha1), [sztomi/dap-rs](https://github.com/sztomi/dap-rs) ("fairly early stage with frequent breakages"); [dap-types](https://crates.io/crates/dap-types) — rejected in favor of serde_json peek + byte passthrough.

### Critical Files for Implementation
- `crates/xcode-dap/src/dap/proxy.rs` — DAP state machine: interception, seq namespace, attach/response rewrite, teardown
- `crates/xcode-dap/src/engine/pipeline.rs` — shared preflight→build→install→launch→PID engine (dap mode + CLI)
- `crates/xcode-dap/src/dap/framing.rs` — Content-Length codec, byte-transparent passthrough foundation
- `crates/xcode-dap/src/setup/jsonc.rs` — JSONC marker-block surgical merge (Rust port of the verified design)
- `extension/src/lib.rs` — WASM extension `get_dap_binary` (must prefer `user_provided_debug_adapter_path`; interface contract in §7)