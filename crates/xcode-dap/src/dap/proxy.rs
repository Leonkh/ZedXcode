//! DAP proxy state machine: routing, seq namespace, launch interception,
//! attach/response rewrite, teardown. See `docs/design/dap-proxy.md` §3.3.
//!
//! Message flow:
//! - lldb-dap is spawned at `initialize`; everything except `launch` flows
//!   byte-verbatim in both directions.
//! - `launch` is intercepted: the engine pipeline runs as a spawned task
//!   (build phases stream as `output` events) racing client `disconnect`
//!   in the main `select!` loop; on success the proxy sends
//!   `evaluate(repl) platform select ios-simulator` + `attach {"pid": N}`
//!   to lldb-dap, then rewrites the attach response onto the client's
//!   launch seq.
//! - `configurationDone` passes through verbatim; lldb-dap itself resumes
//!   the attached process afterwards (plain pid attach, no stopOnEntry),
//!   which yields the auto-continue.
//! - `disconnect` / `terminate` end the session and are answered last:
//!   Zed waits for that answer with no timeout and kills the adapter as soon
//!   as it arrives, so a bounded critical teardown (OSLog group, final
//!   console drain, app terminate, pidfile, lldb-dap) runs first.
//! - The hidden `--mock-pipeline` mode skips xcodebuild/simctl entirely and
//!   attaches lldb-dap to a locally compiled dummy process, exercising the
//!   whole DAP flow without Xcode in seconds.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use xcode_dap_config::LaunchConfig;

use crate::dap::framing::{self, DapReader};
use crate::dap::lldb::LldbDap;
use crate::dap::peek::{self, ChildMsg, ClientMsg};
use crate::engine::consoles::{self, Tailers};
use crate::engine::pipeline::{self, LaunchedApp, OutputSink};
use crate::engine::{config, project, selection, simctl};
use crate::util::logging;
use crate::util::pidfile;

/// How long we wait for the client's `initialize` before deciding this was
/// an accidental CLI invocation.
const INIT_GUARD: Duration = Duration::from_secs(2);

/// How long teardown waits for children / writer flushes.
const TEARDOWN_GRACE: Duration = Duration::from_secs(2);

/// How long a forwarded (live-debuggee) `disconnect` / `terminate` waits for
/// lldb-dap's answer before the proxy owns the shutdown itself. Guards the
/// wedge class where lldb-dap kills its simulator debuggee on disconnect but
/// then never answers or exits — leaving the routing loop hung until Zed
/// force-kills the adapter. A healthy disconnect is answered well under this.
const DISCONNECT_WEDGE_GRACE: Duration = Duration::from_secs(3);

/// How long lldb-dap may take to answer the attach request. The app waits
/// suspended under `--wait-for-debugger` until then; past this the launch
/// fails and teardown terminates the app.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(30);

/// Upper bound on the critical teardown that runs before a `disconnect` /
/// `terminate` is answered (OSLog group stop, final console drain, app
/// terminate, pidfile release, lldb-dap reap). Zed waits for that answer
/// with no timeout, so this bounds how long a Stop can hang.
const CRITICAL_TEARDOWN_BUDGET: Duration = Duration::from_secs(2);

/// How long a critical-teardown step waits for a child it has just SIGKILLed
/// to be reaped (the OSLog pump's bounded stop waits the same). These waits
/// sit outside the graceful waits' shared budget; there are at most
/// [`FORCED_REAPS`] of them (OSLog group, mock app, lldb-dap), which keeps
/// the sum within [`CRITICAL_TEARDOWN_BUDGET`].
const FORCED_REAP_GRACE: Duration = consoles::FORCED_STOP_REAP;
const FORCED_REAPS: u32 = 3;

/// Step caps inside the critical budget, so one slow step cannot take the
/// time of the steps after it: the OSLog group is SIGKILLed after its cap,
/// and the final console drain (one read per capture file) is cut short.
const OSLOG_STOP_GRACE: Duration = Duration::from_millis(500);
const CONSOLE_DRAIN_GRACE: Duration = Duration::from_millis(300);

/// How long teardown waits for a cancelled pipeline to wind down. Must
/// cover xcodebuild's SIGTERM -> 3 s -> SIGKILL escalation (xcodebuild.rs);
/// exiting earlier would orphan the build (kill_on_drop dies with us).
const PIPELINE_DRAIN_GRACE: Duration = Duration::from_secs(10);

/// How long a stop request that cancelled the pipeline waits for it to wind
/// down before teardown runs anyway, counted from the request's arrival.
/// Covers the longest cleanup a cancelled step runs (xcodebuild's or the
/// preflight's SIGTERM -> 3 s -> SIGKILL, a cancelled launch's bounded
/// terminate). A step that does not wind down in time (one the token does
/// not reach, a child that hangs on in its kernel call after SIGKILL) must
/// not hold the answer Zed waits for, so the answer to a mid-pipeline Stop
/// goes out within this plus [`CRITICAL_TEARDOWN_BUDGET`].
const PIPELINE_STOP_GRACE: Duration = Duration::from_secs(4);

/// Everything written to Zed (or to the lldb-dap child stdin) goes through
/// one unbounded mpsc channel -> one writer task, so DAP frames never
/// interleave and `OutputSink::line` (sync) can emit events directly.
pub enum Out {
    /// Verbatim passthrough frame body (gets re-framed on write).
    Raw(Vec<u8>),
    /// Proxy-built message (gets serialized + framed on write).
    Msg(serde_json::Value),
}

/// Result of one pipeline run handed back to the routing loop.
struct PipelineDone {
    app: LaunchedApp,
    /// `None` for a mock launch whose arguments are not a valid scenario
    /// (the mock ignores the build keys; it honors `oslog`).
    config: Option<LaunchConfig>,
    /// The dummy app child in `--mock-pipeline` mode (killed on teardown).
    mock_child: Option<Child>,
}

/// `OutputSink` that emits DAP `output` events (category `console` for
/// pipeline phases, `stdout`/`stderr` for the app tailers) through the
/// single-writer client channel.
struct DapSink {
    to_client: mpsc::UnboundedSender<Out>,
}

impl OutputSink for DapSink {
    fn line(&self, category: &str, text: &str) {
        // Tee into the log file: pipeline phase lines at INFO; raw
        // xcodebuild/oslog/preflight stream lines and app output at DEBUG
        // only (the full build log is already captured in build-latest.log).
        match category {
            "console" => log::info!(target: "pipeline", "{text}"),
            _ => {
                if log::log_enabled!(target: "pipeline", log::Level::Debug) {
                    log::debug!(
                        target: "pipeline",
                        "{category}: {}",
                        logging::truncate(text, 2048)
                    );
                }
            }
        }
        // "build" / "oslog" / "preflight" are internal sub-categories of
        // console output, split off above so they don't flood the log at
        // INFO.
        let dap_category = match category {
            "build" | "oslog" | "preflight" => "console",
            other => other,
        };
        let _ = self.to_client.send(Out::Msg(peek::output_event(
            dap_category,
            &format!("{text}\n"),
        )));
    }
}

/// DEBUG tee of one DAP frame (summary only); full body at TRACE,
/// truncated to 2 KB. The summary is only built when DEBUG is enabled.
fn log_frame(direction: &str, raw: &[u8]) {
    if !log::log_enabled!(target: "dap", log::Level::Debug) {
        return;
    }
    let text = String::from_utf8_lossy(raw);
    log::debug!(target: "dap", "{direction} {}", peek::summarize(&text));
    if log::log_enabled!(target: "dap", log::Level::Trace) {
        log::trace!(target: "dap", "{direction} body: {}", logging::truncate(&text, 2048));
    }
}

/// What the routing loop should do after handling one message.
enum LoopAction {
    Continue,
    Exit(i32),
}

/// A client `disconnect` / `terminate`: the request that ends the session.
/// It is answered only after the critical teardown: Zed waits for this
/// answer with no timeout and kills the adapter as soon as it arrives, so
/// whatever still ran then (the OSLog group, the app, lldb-dap) would be
/// orphaned and the pidfile left behind.
struct StopRequest {
    seq: i64,
    /// `"disconnect"` or `"terminate"`, echoed in the proxy's own answer.
    command: &'static str,
    /// When the request arrived (and cancelled a running pipeline, if any):
    /// the start of [`PIPELINE_STOP_GRACE`].
    received: Instant,
    /// lldb-dap's answer to the forwarded request, passed on verbatim;
    /// `None` when the proxy answers itself (a bare success).
    response: Option<Vec<u8>>,
    /// Follow the answer with a `terminated` event: set for a Stop that
    /// cancelled the pipeline, where lldb-dap never had a session whose end
    /// it would report.
    terminated_event: bool,
    /// Further stop requests that arrived while this one was pending, each
    /// answered with a bare success after it.
    repeats: Vec<(i64, &'static str)>,
}

impl StopRequest {
    fn new(seq: i64, command: &'static str) -> Self {
        Self {
            seq,
            command,
            received: Instant::now(),
            response: None,
            terminated_event: false,
            repeats: Vec::new(),
        }
    }

    /// The frames that answer the request (and its repeats), in order.
    fn answers(self) -> Vec<Out> {
        let mut out = vec![match self.response {
            Some(raw) => Out::Raw(raw),
            None => Out::Msg(peek::success_response(self.seq, self.command)),
        }];
        out.extend(
            self.repeats
                .into_iter()
                .map(|(seq, command)| Out::Msg(peek::success_response(seq, command))),
        );
        if self.terminated_event {
            out.push(Out::Msg(peek::terminated_event()));
        }
        out
    }
}

/// The critical teardown's clock. While a stop request waits for its answer,
/// every graceful wait takes its slice from one deadline, so however the
/// steps behave, the hold before the answer stays within
/// [`CRITICAL_TEARDOWN_BUDGET`]. When no stop request waits (SIGTERM, stdin
/// EOF, lldb-dap exit, attach timeout), each wait gets the whole
/// [`TEARDOWN_GRACE`] instead: a `simctl terminate` on a busy Mac can take
/// more than the second the shared budget would leave it.
#[derive(Clone, Copy, Debug)]
struct Budget {
    /// `None`: no answer is waiting (see [`Budget::unhurried`]).
    deadline: Option<Instant>,
}

impl Budget {
    /// The graceful waits' budget for a pending stop request, starting at
    /// `now`: the critical budget minus what the forced reaps may need.
    fn critical(now: Instant) -> Self {
        Self {
            deadline: Some(now + graceful_budget()),
        }
    }

    /// No answer is waiting: every wait gets [`TEARDOWN_GRACE`].
    fn unhurried() -> Self {
        Self { deadline: None }
    }

    /// How long a wait starting at `now` may take: what is left of the
    /// budget, at most `cap`. Zero once the budget is spent — the step then
    /// takes only what is already finished and goes on to its forced
    /// fallback (SIGKILL), or is skipped. Without a deadline, the
    /// [`TEARDOWN_GRACE`] whatever the step's critical `cap`.
    fn slice_at(&self, now: Instant, cap: Duration) -> Duration {
        match self.deadline {
            Some(deadline) => cap.min(deadline.saturating_duration_since(now)),
            None => TEARDOWN_GRACE,
        }
    }

    fn slice(&self, cap: Duration) -> Duration {
        self.slice_at(Instant::now(), cap)
    }
}

/// The part of [`CRITICAL_TEARDOWN_BUDGET`] the graceful waits share.
fn graceful_budget() -> Duration {
    CRITICAL_TEARDOWN_BUDGET.saturating_sub(FORCED_REAP_GRACE * FORCED_REAPS)
}

/// How long teardown waits for a pipeline still running at `now`. With a
/// stop request pending (`stop_received`, which cancelled the pipeline when
/// it arrived), only what is left of [`PIPELINE_STOP_GRACE`]: the answer
/// must not wait out a step that ignores the cancel. Otherwise nothing waits
/// on the adapter and the cancelled build gets [`PIPELINE_DRAIN_GRACE`].
fn pipeline_drain_grace(stop_received: Option<Instant>, now: Instant) -> Duration {
    match stop_received {
        Some(at) => (at + PIPELINE_STOP_GRACE).saturating_duration_since(now),
        None => PIPELINE_DRAIN_GRACE,
    }
}

/// The proxy state machine.
pub struct Proxy {
    to_client: mpsc::UnboundedSender<Out>,
    /// Writer to lldb-dap stdin; present once spawned at `initialize`.
    to_child: Option<mpsc::UnboundedSender<Out>>,
    /// Spawned at `initialize`.
    lldb: Option<LldbDap>,
    /// Hidden `--mock-pipeline` mode (skip xcodebuild/simctl, dummy app).
    mock_pipeline: bool,
    /// Client's launch request seq (the attach response is rewritten onto it).
    launch_seq: Option<i64>,
    /// Our internal attach seq.
    attach_seq: Option<i64>,
    /// Proxy-internal seq namespace; starts at `peek::SEQ_BASE`.
    next_seq: i64,
    /// Sender half of the pipeline-result channel (cloned into the task;
    /// kept alive here so the loop's `recv()` arm pends instead of closing).
    pipe_tx: mpsc::Sender<Result<PipelineDone>>,
    pipeline_running: bool,
    pipeline_cancel: Option<CancellationToken>,
    /// The client's `disconnect` / `terminate`, answered by teardown once its
    /// critical part has run (see [`StopRequest`]).
    stop: Option<StopRequest>,
    /// Set while a stop request waits: for lldb-dap's answer to the forwarded
    /// request ([`DISCONNECT_WEDGE_GRACE`]), or for the pipeline it cancelled
    /// to wind down ([`PIPELINE_STOP_GRACE`]). Past it the routing loop ends
    /// anyway (a wedged lldb-dap, a step that ignores the cancel) and
    /// teardown answers the request itself.
    stop_deadline: Option<Instant>,
    /// Set while the attach request waits for lldb-dap's answer
    /// ([`ATTACH_TIMEOUT`]).
    attach_deadline: Option<Instant>,
    /// The attach timeout's failure (launch error, stderr line, `terminated`),
    /// held like a stop request's answer: Zed may end the adapter once it
    /// sees them, so teardown terminates the suspended app first.
    held_failure: Vec<Out>,
    /// Launched app once the pipeline succeeded (teardown cleanup).
    session: Option<PipelineDone>,
    /// out.log / err.log tailers, started on successful attach.
    tailers: Option<Tailers>,
    /// OSLog pump (`"oslog": true`), started alongside the tailers.
    oslog: Option<consoles::OslogPump>,
    /// UDID whose pidfile we claimed (removed on teardown).
    pidfile_udid: Option<String>,
    /// Set once lldb-dap reports a successful attach. Gates the
    /// `terminateOnStop: false` opt-out: leaving the app running on Stop
    /// only makes sense for an app that actually ran under the debugger; a
    /// never-attached app is still suspended (`--wait-for-debugger`) and must
    /// be terminated on teardown regardless.
    attached: bool,
    /// Set once lldb-dap ended the debug session — an `exited` OR a
    /// `terminated` event. Gates who owns a following client `disconnect`:
    /// once the session has ended (which is why Zed disconnects), the proxy
    /// answers the disconnect itself and drives a clean shutdown rather than
    /// delegating to lldb-dap, which can wedge on a disconnect after its
    /// simulator debuggee died (see `on_stop_request`).
    session_ended: bool,
    /// Set only on an `exited` event — the debuggee **process** is gone
    /// (not a mere `terminated`/detach; lldb-dap detaches an attach-by-pid
    /// session on disconnect, leaving the app alive). Gates the teardown
    /// terminate-skip: skipping `simctl terminate` is right only when nothing
    /// of ours is left to kill (and a successor may now own the bundle id) —
    /// a plain detach must still terminate the app per `terminateOnStop`.
    debuggee_exited: bool,
    /// Set when lldb-dap left a forwarded stop request unanswered past
    /// [`DISCONNECT_WEDGE_GRACE`]: teardown then kills it at once instead of
    /// spending the rest of its budget waiting for an exit.
    lldb_wedged: bool,
}

/// Entry point for DAP proxy mode (no subcommand): speak DAP on stdio,
/// with a 2 s initialize guard against accidental invocation.
pub async fn run_dap_mode(mock_pipeline: bool) -> Result<()> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (to_client, client_writer) = spawn_writer(stdout);
    let mut client_reader = DapReader::new(stdin);

    // --- 2 s initialize guard ---------------------------------------------
    // NOTE: guard failures use process::exit, not `bail!` — a pending
    // tokio::io::stdin() read runs on the blocking thread pool and keeps the
    // runtime from shutting down while the parent holds our stdin open.
    let guard_msg = format!(
        "xcode-dap: running in DAP mode but no `initialize` request arrived \
         within {}s. This binary speaks DAP on stdio when started without a \
         subcommand (that is how Zed runs it). Did you mean a subcommand? \
         Try `xcode-dap --help`.",
        INIT_GUARD.as_secs()
    );
    let first = match tokio::time::timeout(INIT_GUARD, client_reader.next_message()).await {
        Err(_) | Ok(Ok(None)) => {
            // Timeout, or stdin closed without a frame (e.g. `xcode-dap </dev/null`).
            log::error!("init guard tripped: no `initialize` within {INIT_GUARD:?}");
            eprintln!("{guard_msg}");
            std::process::exit(1);
        }
        Ok(Ok(Some(raw))) => raw,
        Ok(Err(e)) => {
            log::error!("error reading first DAP frame from client: {e:#}");
            eprintln!("xcode-dap: error reading first DAP frame from client: {e:#}");
            std::process::exit(1);
        }
    };
    log_frame("zed->proxy", &first);
    if !matches!(peek::classify_client(&first)?, ClientMsg::Initialize { .. }) {
        log::error!("first DAP message was not `initialize` — protocol error");
        eprintln!(
            "xcode-dap: first DAP message was not `initialize` — protocol error. \
             Try `xcode-dap --help` if you meant to run a subcommand."
        );
        std::process::exit(1);
    }
    let client_id = serde_json::from_slice::<Value>(&first)
        .ok()
        .and_then(|v| {
            v.get("arguments")?
                .get("clientID")?
                .as_str()
                .map(str::to_string)
        })
        .unwrap_or_else(|| "unknown".to_string());
    log::info!("initialize received (clientID {client_id})");

    // --- spawn lldb-dap, forward initialize verbatim ----------------------
    let mut lldb = LldbDap::spawn().await?;
    log::info!(
        "lldb-dap spawned (child pid {})",
        lldb.child.id().unwrap_or(0)
    );
    let child_stdin = lldb.take_stdin().context("lldb-dap stdin already taken")?;
    let child_stdout = lldb
        .take_stdout()
        .context("lldb-dap stdout already taken")?;
    let (to_child, child_writer) = spawn_writer(child_stdin);
    let (from_child_tx, mut from_child) = mpsc::channel::<Vec<u8>>(256);
    let child_reader: JoinHandle<()> = tokio::spawn(async move {
        let mut reader = DapReader::new(child_stdout);
        loop {
            match reader.next_message().await {
                Ok(Some(body)) => {
                    if from_child_tx.send(body).await.is_err() {
                        break; // proxy is gone
                    }
                }
                Ok(None) => break, // lldb-dap closed stdout (exited)
                Err(e) => {
                    log::error!("error reading from lldb-dap: {e:#}");
                    eprintln!("xcode-dap: error reading from lldb-dap: {e:#}");
                    break;
                }
            }
        }
    });

    to_child.send(Out::Raw(first)).map_err(|_| {
        anyhow::anyhow!("failed to forward initialize to lldb-dap (writer task exited)")
    })?;

    let (pipe_tx, mut pipe_rx) = mpsc::channel::<Result<PipelineDone>>(1);
    let mut proxy = Proxy {
        to_client,
        to_child: Some(to_child),
        lldb: Some(lldb),
        mock_pipeline,
        launch_seq: None,
        attach_seq: None,
        next_seq: peek::SEQ_BASE,
        pipe_tx,
        pipeline_running: false,
        pipeline_cancel: None,
        stop: None,
        stop_deadline: None,
        attach_deadline: None,
        held_failure: Vec::new(),
        session: None,
        tailers: None,
        oslog: None,
        pidfile_udid: None,
        attached: false,
        session_ended: false,
        debuggee_exited: false,
        lldb_wedged: false,
    };

    // --- main routing loop -------------------------------------------------
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("installing SIGTERM handler")?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .context("installing SIGINT handler")?;

    // Routing errors must `break`, never propagate (`?`) out of the loop:
    // teardown below still has to wind down a running pipeline, kill the
    // children, remove the pidfile, and flush queued client frames.
    let mut exit_code = 0;
    loop {
        // Copy the deadlines out before the select so the timer arms do not
        // borrow `proxy` (the message arms need `&mut proxy`).
        let stop_deadline = proxy.stop_deadline;
        let attach_deadline = proxy.attach_deadline;
        tokio::select! {
            msg = client_reader.next_message() => match msg {
                Ok(Some(raw)) => match proxy.on_client_message(&raw) {
                    Ok(LoopAction::Continue) => {}
                    Ok(LoopAction::Exit(code)) => { exit_code = code; break; }
                    Err(e) => {
                        log::error!("error handling client message: {e:#}");
                        eprintln!("xcode-dap: error handling client message: {e:#}");
                        exit_code = 1;
                        break;
                    }
                },
                Ok(None) => break, // stdin EOF: Zed is gone
                Err(e) => {
                    log::error!("error reading from client: {e:#}");
                    eprintln!("xcode-dap: error reading from client: {e:#}");
                    break;
                }
            },
            msg = from_child.recv() => match msg {
                Some(raw) => match proxy.on_child_message(&raw) {
                    Ok(LoopAction::Continue) => {}
                    Ok(LoopAction::Exit(code)) => { exit_code = code; break; }
                    Err(e) => {
                        log::error!("error handling lldb-dap message: {e:#}");
                        eprintln!("xcode-dap: error handling lldb-dap message: {e:#}");
                        exit_code = 1;
                        break;
                    }
                },
                None => break, // lldb-dap exited
            },
            // Pipeline completion (the launch interception's other half).
            // `pipe_tx` lives in `proxy`, so `recv()` pends when idle.
            res = pipe_rx.recv(), if proxy.pipeline_running => {
                if let Some(res) = res {
                    match proxy.on_pipeline_result(res) {
                        Ok(LoopAction::Continue) => {}
                        Ok(LoopAction::Exit(code)) => { exit_code = code; break; }
                        Err(e) => {
                            log::error!("error handling pipeline result: {e:#}");
                            eprintln!("xcode-dap: error handling pipeline result: {e:#}");
                            exit_code = 1;
                            break;
                        }
                    }
                }
            },
            // Bounded fallback for a pending stop request: if lldb-dap wedged
            // (killed the debuggee but never answered), or the cancelled
            // pipeline has not wound down, shut down instead of hanging;
            // teardown answers Zed itself.
            () = wait_opt_deadline(stop_deadline) => {
                proxy.stop_deadline = None;
                proxy.on_stop_deadline();
                break;
            }
            // The app waits suspended for the debugger: give up on an attach
            // lldb-dap never answers instead of leaving it frozen.
            () = wait_opt_deadline(attach_deadline) => {
                proxy.attach_deadline = None;
                match proxy.on_attach_timeout() {
                    Ok(LoopAction::Continue) => {}
                    Ok(LoopAction::Exit(code)) => { exit_code = code; break; }
                    Err(e) => {
                        log::error!("error handling the attach timeout: {e:#}");
                        exit_code = 1;
                        break;
                    }
                }
            }
            _ = sigterm.recv() => {
                log::info!("SIGTERM received");
                // A newer run claimed our simulator (or an external stop): tell
                // Zed the session is ending so the adapter's exit reads as a
                // clean stop, not a crash. Teardown then terminates our
                // still-owned app, unblocking the successor's install.
                proxy.announce_superseded();
                break;
            }
            _ = sigint.recv() => {
                log::info!("SIGINT received");
                proxy.announce_superseded();
                break;
            }
        }
    }

    proxy
        .teardown(&mut pipe_rx, child_reader, client_writer, child_writer)
        .await;
    log::info!("exiting {exit_code}");

    // Exit explicitly: a pending blocking stdin read would otherwise stall
    // runtime shutdown for as long as the parent keeps our stdin open
    // (SIGTERM / lldb-dap-exit paths). Teardown already killed the children
    // and flushed the client writer, so nothing relies on destructors here.
    std::process::exit(exit_code);
}

/// Sleep until `deadline` if set, else pend forever — the disabled state of the
/// routing loop's timer arms (stop wedge, attach timeout).
async fn wait_opt_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

/// Whether teardown should run the belt-and-braces `simctl terminate`.
/// Skip it when a successor superseded us (would kill its app) or when our
/// debuggee already exited (nothing of ours is left, and the bundle id may
/// now be the successor's). Otherwise terminate for a `terminateOnStop` app,
/// or for one that never attached (still suspended under `--wait-for-debugger`
/// and must not be left frozen).
fn should_terminate_on_teardown(
    attached: bool,
    terminate_on_stop: bool,
    superseded: bool,
    debuggee_exited: bool,
) -> bool {
    !superseded && !debuggee_exited && (!attached || terminate_on_stop)
}

impl Proxy {
    /// Route one frame arriving from the client (Zed).
    fn on_client_message(&mut self, raw: &[u8]) -> Result<LoopAction> {
        log_frame("zed->proxy", raw);
        match peek::classify_client(raw)? {
            ClientMsg::Initialize { raw } => {
                // Already initialized — forward anyway (lldb-dap will answer).
                self.send_to_child_raw(raw)?;
                Ok(LoopAction::Continue)
            }
            ClientMsg::Launch { seq, args } => self.handle_launch(seq, args, raw),
            ClientMsg::Disconnect { seq, command, raw } => self.on_stop_request(seq, command, raw),
            ClientMsg::Other { raw } => {
                self.send_to_child_raw(raw)?;
                Ok(LoopAction::Continue)
            }
        }
    }

    /// A client `disconnect` / `terminate`. Whatever the state, the request is
    /// answered only after the critical teardown (see [`StopRequest`]); the
    /// state decides how the routing loop gets there.
    fn on_stop_request(
        &mut self,
        seq: i64,
        command: &'static str,
        raw: &[u8],
    ) -> Result<LoopAction> {
        if let Some(stop) = &mut self.stop {
            // A repeat (say `terminate`, then `disconnect`) while the first
            // is pending is answered right after it.
            log::info!("{command} received (seq {seq}) while a stop is pending — answered with it");
            stop.repeats.push((seq, command));
            return Ok(LoopAction::Continue);
        }
        let stop = StopRequest::new(seq, command);
        let received = stop.received;
        self.stop = Some(stop);
        // From here the stop's own path bounds the session's end.
        self.attach_deadline = None;
        if self.pipeline_running {
            // Mid-build Stop: cancel the pipeline (kills the xcodebuild
            // process group, or the install / launch helper); the pipe_rx arm
            // leaves the loop once the pipeline has wound down, the
            // stop_deadline arm after PIPELINE_STOP_GRACE if it has not.
            log::info!("{command} received (seq {seq}) mid-pipeline — cancelling");
            if let Some(cancel) = &self.pipeline_cancel {
                cancel.cancel();
            }
            self.stop_deadline = Some(received + PIPELINE_STOP_GRACE);
            Ok(LoopAction::Continue)
        } else if self.session_ended {
            // lldb-dap already ended the session (an `exited`/`terminated`
            // event) — most often because a second ⌘R's `simctl install`
            // replaced the running app's bundle, killing it, so lldb-dap
            // emitted `terminated` and Zed is now disconnecting. We must NOT
            // forward and wait for lldb-dap: against the simulator debugserver
            // it can wedge on a disconnect once its debuggee is gone, so the
            // routing loop would wait forever and Zed force-kills the adapter
            // (the perceived "crash"). Own the shutdown instead: forward the
            // request so a healthy lldb-dap still detaches, then leave the
            // loop; teardown reaps lldb-dap within its budget (wait, then
            // kill) and answers Zed. Its `simctl terminate` still runs unless
            // the process actually exited (gated by debuggee_exited), so a
            // detach here still terminates the app and we never step on a
            // successor.
            log::info!(
                "{command} received (seq {seq}) after session end — shutting down, \
                 answering after teardown"
            );
            let _ = self.send_to_child_raw(raw);
            Ok(LoopAction::Exit(0))
        } else {
            // Live debuggee (no `exited`/`terminated` seen): forward so
            // lldb-dap terminates/detaches per the request. Its answer is held
            // (see `on_child_message`) and the loop ends there; teardown sends
            // it once the critical work is done. Against the simulator
            // debugserver lldb-dap can KILL the debuggee on disconnect and then
            // wedge without answering (common when a concurrent Rerun's
            // install is contending on the same bundle) — which would hang
            // the loop until Zed force-kills the adapter. So arm a bounded
            // wait: if lldb-dap has not answered by the deadline, we own the
            // shutdown ourselves (the `stop_deadline` arm in the routing loop).
            log::info!("{command} received (seq {seq}) — forwarding to lldb-dap (bounded)");
            self.send_to_child_raw(raw)?;
            self.stop_deadline = Some(Instant::now() + DISCONNECT_WEDGE_GRACE);
            Ok(LoopAction::Continue)
        }
    }

    /// Launch interception: record the seq, spawn the pipeline task. The
    /// main loop races its completion (`pipe_rx`) against client traffic,
    /// which is how a mid-build `disconnect` cancels the build. `raw` is
    /// the launch frame's bytes (re-logged after a `verboseLogging` raise).
    fn handle_launch(&mut self, seq: i64, args: Value, raw: &[u8]) -> Result<LoopAction> {
        if self.pipeline_running || self.session.is_some() {
            self.send_to_client(Out::Msg(peek::error_response(
                seq,
                "launch",
                "a launch is already in progress in this session",
            )));
            return Ok(LoopAction::Continue);
        }
        self.launch_seq = Some(seq);

        let sink = DapSink {
            to_client: self.to_client.clone(),
        };
        let cancel = CancellationToken::new();
        self.pipeline_cancel = Some(cancel.clone());
        let pipe_tx = self.pipe_tx.clone();

        if self.mock_pipeline {
            log::info!("launch intercepted (seq {seq}): mock pipeline");
            // The mock skips the build keys but keeps a scenario that parses,
            // so `"oslog": true` starts the OSLog pump against the mock
            // simulator (the smoke tests answer its `log stream`).
            let config = serde_json::from_value::<LaunchConfig>(args).ok();
            tokio::spawn(async move {
                let res = mock_pipeline(&sink, cancel)
                    .await
                    .map(|(app, child)| PipelineDone {
                        app,
                        config,
                        mock_child: Some(child),
                    });
                let _ = pipe_tx.send(res).await;
            });
        } else {
            let cfg: LaunchConfig = match serde_json::from_value(args.clone()) {
                Ok(cfg) => cfg,
                Err(e) => {
                    return self.fail_launch(&format!(
                        "invalid launch configuration: {}",
                        config::invalid_config_reason(&args, &e)
                    ))
                }
            };
            // Raise (never lower) the log level for this session. Skipped
            // when init installed no logger — raising the level would only
            // enable log_enabled! work on the noop logger.
            if cfg.verbose_logging
                && logging::is_active()
                && log::max_level() < log::LevelFilter::Trace
            {
                log::set_max_level(log::LevelFilter::Trace);
                log::info!("verboseLogging: log level raised to trace");
                // The launch frame itself arrived before the raise, so a
                // verboseLogging-only session would never capture its most
                // diagnostic frame — re-log it at the raised level.
                // (initialize is not retained; frames before launch need
                // XCODE_DAP_LOG.)
                log_frame("zed->proxy [replayed after verboseLogging raise]", raw);
            }
            log::info!(
                "launch intercepted (seq {seq}): workspace {:?}, scheme {:?}, device {:?}, \
                 os {:?}, configuration {:?}, preflight {}, oslog {}, buildOutput {:?}, \
                 terminateOnStop {}",
                cfg.workspace,
                cfg.scheme,
                cfg.device,
                cfg.os,
                cfg.configuration,
                if cfg.preflight.is_some() { "yes" } else { "no" },
                cfg.oslog,
                cfg.build_output,
                cfg.terminate_on_stop,
            );
            // Zed starts the adapter in the worktree root: the project root,
            // whose selection store the pipeline reads on every launch.
            let root = std::env::current_dir()
                .map(|cwd| project::root_from_zed(&cwd))
                .unwrap_or_else(|_| PathBuf::from("."));
            let req = selection::Request::for_launch(&cfg, root);
            tokio::spawn(async move {
                let res = pipeline::run_pipeline(&req, true, &sink, cancel)
                    .await
                    .map(|app| PipelineDone {
                        app,
                        config: Some(cfg),
                        mock_child: None,
                    });
                let _ = pipe_tx.send(res).await;
            });
        }
        self.pipeline_running = true;
        Ok(LoopAction::Continue)
    }

    /// The pipeline task finished (success, failure, or cancellation).
    fn on_pipeline_result(&mut self, res: Result<PipelineDone>) -> Result<LoopAction> {
        self.pipeline_running = false;
        self.pipeline_cancel = None;

        // A mid-pipeline stop request cancelled us: leave the loop; teardown
        // runs its critical part, then answers the request and emits
        // `terminated`, and the adapter exits 0.
        if let Some(stop) = self.stop.as_mut() {
            stop.terminated_event = true;
            let command = stop.command;
            // The pipeline won the race anyway: hand the app to teardown,
            // which undoes the launch. The app was launched
            // `--wait-for-debugger` and never attached, so it is suspended and
            // would hang forever if left; teardown terminates a never-attached
            // app unconditionally (terminateOnStop only applies to an app that
            // actually ran).
            if let Ok(done) = res {
                self.session = Some(done);
            }
            log::info!("pipeline wound down after a mid-build {command} — tearing down");
            return Ok(LoopAction::Exit(0));
        }

        let done = match res {
            Ok(done) => done,
            Err(err) => return self.fail_launch(&format!("{err:#}")),
        };
        log::info!(
            "pipeline ok (pid {}, udid {}, bundle {})",
            done.app.pid,
            done.app.udid,
            done.app.bundle_id
        );

        // Pidfile claim: the udid is known only post-resolution, so the
        // claim happens here (SIGTERM a stale previous instance so a Rerun
        // can't race the old session's teardown).
        match pidfile::kill_old_and_remember(&done.app.udid) {
            Ok(()) => self.pidfile_udid = Some(done.app.udid.clone()),
            Err(e) => {
                log::error!("pidfile claim failed: {e:#}");
                eprintln!("xcode-dap: pidfile claim failed: {e:#}");
            }
        }

        // Attach: `platform select ios-simulator` (repl evaluate) then a
        // plain `{"pid": N}` attach. The mock dummy is a host process, so
        // no platform select there.
        let pid = done.app.pid;
        if !self.mock_pipeline {
            let seq = self.take_seq();
            log::info!("platform select ios-simulator (seq {seq})");
            self.send_to_child_msg(peek::evaluate_repl("platform select ios-simulator", seq))?;
        }
        let attach_seq = self.take_seq();
        self.attach_seq = Some(attach_seq);
        log::info!("attach requested (pid {pid}, seq {attach_seq})");
        self.send_to_child_msg(peek::attach_pid(pid, attach_seq))?;
        self.attach_deadline = Some(Instant::now() + ATTACH_TIMEOUT);

        self.session = Some(done);
        Ok(LoopAction::Continue)
    }

    /// Pipeline failure: error response + stderr output + `terminated`,
    /// then graceful exit 1 (teardown still flushes the writer).
    fn fail_launch(&mut self, msg: &str) -> Result<LoopAction> {
        for out in self.launch_failure(msg) {
            self.send_to_client(out);
        }
        Ok(LoopAction::Exit(1))
    }

    /// The frames that fail the client's launch: error response, stderr
    /// output, `terminated`.
    fn launch_failure(&self, msg: &str) -> Vec<Out> {
        log::error!("launch failed: {msg}");
        let seq = self.launch_seq.unwrap_or(0);
        vec![
            Out::Msg(peek::error_response(seq, "launch", msg)),
            Out::Msg(peek::output_event("stderr", &format!("{msg}\n"))),
            Out::Msg(peek::terminated_event()),
        ]
    }

    /// lldb-dap did not answer the attach within [`ATTACH_TIMEOUT`]: fail the
    /// launch, exit 1. The failure is held until teardown has terminated the
    /// app, still suspended under `--wait-for-debugger` (never attached, so
    /// regardless of `terminateOnStop`).
    fn on_attach_timeout(&mut self) -> Result<LoopAction> {
        let app = self
            .session
            .as_ref()
            .map(|s| format!("{} (pid {})", s.app.bundle_id, s.app.pid))
            .unwrap_or_else(|| "the app".to_string());
        self.held_failure = self.launch_failure(&format!(
            "the debugger did not attach to {app} within {}s — stopping the app, \
             which was waiting for the debugger. Run again; if this repeats, \
             `xcode-dap doctor` checks lldb-dap",
            ATTACH_TIMEOUT.as_secs()
        ));
        Ok(LoopAction::Exit(1))
    }

    /// The stop request's deadline passed with the routing loop still
    /// running (it ends right after this). Either the pipeline it cancelled
    /// has not wound down, or lldb-dap left the forwarded request
    /// unanswered (teardown then kills it at once).
    fn on_stop_deadline(&mut self) {
        let Some((command, seq)) = self.stop.as_ref().map(|s| (s.command, s.seq)) else {
            return;
        };
        if self.pipeline_running {
            log::warn!(
                "{command} (seq {seq}): the cancelled pipeline did not wind down within \
                 {}s — tearing down without it",
                PIPELINE_STOP_GRACE.as_secs()
            );
        } else {
            self.lldb_wedged = true;
            log::info!(
                "{command} (seq {seq}): lldb-dap did not answer within {}s — owning the \
                 shutdown",
                DISCONNECT_WEDGE_GRACE.as_secs()
            );
        }
    }

    /// Route one frame arriving from lldb-dap.
    fn on_child_message(&mut self, raw: &[u8]) -> Result<LoopAction> {
        log_frame("lldb->proxy", raw);
        match peek::classify_child(raw)? {
            ChildMsg::InternalResponse { request_seq, raw } => {
                if self.attach_seq == Some(request_seq) {
                    self.attach_deadline = None;
                    // Rewrite the attach response into the client's launch
                    // response (request_seq -> launch seq, command ->
                    // "launch") and forward it.
                    let launch_seq = self.launch_seq.unwrap_or(0);
                    let (msg, success) = peek::rewrite_attach_response(raw, launch_seq)?;
                    self.send_to_client(Out::Msg(msg));
                    if success {
                        log::info!("attach response ok — debugger attached");
                        self.attached = true;
                        self.send_to_client(Out::Msg(peek::output_event(
                            "console",
                            "Debugger attached\n",
                        )));
                        self.start_tailers();
                    } else {
                        log::error!("attach response failed (seq {request_seq})");
                        self.send_to_client(Out::Msg(peek::output_event(
                            "stderr",
                            "Debugger attach failed\n",
                        )));
                    }
                }
                // All other internal responses (e.g. the platform-select
                // evaluate) are dropped — forwarding them would confuse the
                // client's seq accounting.
                Ok(LoopAction::Continue)
            }
            ChildMsg::Other { raw } => {
                // lldb-dap's answer to a forwarded stop request is held, not
                // passed on: teardown sends it once the critical work is done.
                // (A mid-pipeline stop is never forwarded.)
                if self.stop_deadline.is_some() && !self.pipeline_running {
                    if let Some(stop) = self.stop.as_mut() {
                        if peek::is_response_to(raw, stop.seq) {
                            log::info!(
                                "lldb-dap answered {} (seq {}) — holding the answer \
                                 until teardown is done",
                                stop.command,
                                stop.seq
                            );
                            stop.response = Some(raw.to_vec());
                            self.stop_deadline = None;
                            return Ok(LoopAction::Exit(0));
                        }
                    }
                }
                // Track lldb-dap's end-of-session events so a following client
                // `disconnect` is owned by us (see `on_stop_request`).
                // `exited` => the process is gone (also gates the teardown
                // terminate-skip); a plain `terminated` is a detach — the app
                // is still alive and must be terminated on Stop. `exited`
                // precedes `terminated`, so the first terminal event decides
                // both flags; the `!session_ended` guard keeps this to a single
                // parse per session.
                if !self.session_ended {
                    if let Some(process_exited) = peek::terminal_event(raw) {
                        self.session_ended = true;
                        self.debuggee_exited = process_exited;
                        log::info!(
                            "lldb-dap ended the session (debuggee_exited={})",
                            self.debuggee_exited
                        );
                    }
                }
                self.send_to_client(Out::Raw(raw.to_vec()));
                Ok(LoopAction::Continue)
            }
        }
    }

    /// Start the out.log / err.log tailers and, when configured, the OSLog
    /// pump (idempotent; on attach success).
    fn start_tailers(&mut self) {
        if self.tailers.is_some() {
            return;
        }
        let Some(session) = &self.session else { return };
        let sink: Arc<dyn OutputSink> = Arc::new(DapSink {
            to_client: self.to_client.clone(),
        });
        log::info!(
            "tailers started ({}, {})",
            session.app.stdout_file.display(),
            session.app.stderr_file.display()
        );
        self.tailers = Some(consoles::start_tailers(
            &session.app.stdout_file,
            &session.app.stderr_file,
            sink.clone(),
        ));
        // OSLog pump (§5.3). In mock mode the "simulator" is `mock`, which only
        // the smoke tests' stand-in `log stream` answers.
        if let Some(config) = session.config.as_ref().filter(|c| c.oslog) {
            let app_name = session
                .app
                .app_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            if !app_name.is_empty() {
                let predicate = config.oslog_predicate.clone().unwrap_or_else(|| {
                    consoles::default_oslog_predicate(&session.app.bundle_id, app_name)
                });
                log::info!("oslog pump started (predicate {predicate:?})");
                self.oslog = Some(consoles::start_oslog_pump(
                    &session.app.udid,
                    &predicate,
                    sink,
                ));
            }
        }
    }

    /// On a superseding SIGTERM/SIGINT (a newer run claimed our simulator, or
    /// an external stop), emit a `terminated` event so Zed renders the adapter
    /// exit as a clean session end rather than a crash. No-op before there is a
    /// session to end, or if lldb-dap already ended it (Zed already knows).
    fn announce_superseded(&mut self) {
        if self.session_ended || (!self.attached && self.session.is_none()) {
            return;
        }
        self.send_to_client(Out::Msg(peek::output_event(
            "console",
            "Superseded by a new run — stopping this session.\n",
        )));
        self.send_to_client(Out::Msg(peek::terminated_event()));
        self.session_ended = true;
    }

    fn send_to_client(&self, out: Out) {
        // A send failure means the writer is gone (stdout closed) — the
        // loop will end via EOF/child paths; nothing useful to do here.
        let _ = self.to_client.send(out);
    }

    fn send_to_child_raw(&self, raw: &[u8]) -> Result<()> {
        self.send_to_child(Out::Raw(raw.to_vec()))
    }

    fn send_to_child_msg(&self, msg: Value) -> Result<()> {
        self.send_to_child(Out::Msg(msg))
    }

    fn send_to_child(&self, out: Out) -> Result<()> {
        self.to_child
            .as_ref()
            .context("lldb-dap not spawned yet")?
            .send(out)
            .map_err(|_| {
                anyhow::anyhow!("failed to forward frame to lldb-dap (writer task exited — child stdin closed?)")
            })
    }

    /// Allocate the next proxy-internal seq (evaluate/attach requests).
    fn take_seq(&mut self) -> i64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        seq
    }

    /// True iff we recorded a pidfile claim for this session but a newer
    /// proxy instance has since re-claimed it (SIGTERM'ing us). When that
    /// happens the app now running under our bundle id belongs to the
    /// successor, so teardown must not `simctl terminate` it. Mirrors the
    /// ownership check `pidfile::remove` performs; when we never claimed a
    /// pidfile (`None`) we cannot have been superseded.
    fn superseded(&self) -> bool {
        let Some(udid) = &self.pidfile_udid else {
            return false;
        };
        let still_ours = pidfile::pidfile_path(udid)
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| s.trim().parse::<u32>().ok())
            == Some(std::process::id());
        !still_ours
    }

    /// Teardown, in three parts. First a pipeline still running is wound
    /// down: on the EOF / SIGTERM paths nothing waits and it gets
    /// [`PIPELINE_DRAIN_GRACE`]; with a stop request pending only what is
    /// left of [`PIPELINE_STOP_GRACE`]. Then the critical part, bounded by
    /// [`CRITICAL_TEARDOWN_BUDGET`] while a stop request waits: stop the
    /// OSLog group, drain the console tailers one last time, kill the mock
    /// app / `simctl terminate` the app, release the pidfile, reap lldb-dap.
    /// It runs before a pending `disconnect` / `terminate` (or the attach
    /// timeout's failure) is answered, because Zed kills the adapter as soon
    /// as it has that answer and anything left for later would be orphaned.
    /// Last, the answer goes out and the client writer is drained and
    /// flushed so it reaches Zed before exit.
    async fn teardown(
        mut self,
        pipe_rx: &mut mpsc::Receiver<Result<PipelineDone>>,
        child_reader: JoinHandle<()>,
        client_writer: JoinHandle<()>,
        child_writer: JoinHandle<()>,
    ) {
        let started = Instant::now();
        log::info!("teardown: begin");
        // A pipeline still running here must finish its own cleanup
        // (xcodebuild pgid kill) before we exit — kill_on_drop does not
        // survive process exit. With a stop request pending, only within
        // what is left of the stop's grace (see `pipeline_drain_grace`).
        if self.pipeline_running {
            log::info!("teardown: waiting for the cancelled pipeline to wind down");
            if let Some(cancel) = &self.pipeline_cancel {
                cancel.cancel();
            }
            if let Some(stop) = self.stop.as_mut() {
                stop.terminated_event = true; // lldb-dap never had a session
            }
            let grace = pipeline_drain_grace(self.stop.as_ref().map(|s| s.received), started);
            match tokio::time::timeout(grace, pipe_rx.recv()).await {
                Ok(Some(Ok(done))) => self.session = Some(done), // launched after all — clean it up below
                Ok(_) => {}
                Err(_) => log::warn!(
                    "teardown: the cancelled pipeline is still running after {} ms — \
                     going on without it",
                    grace.as_millis()
                ),
            }
        }

        // --- critical part: bounded, before the answer ----------------------
        let budget = if self.stop.is_some() {
            Budget::critical(Instant::now())
        } else {
            Budget::unhurried()
        };

        // Final drain of app output while the client writer still runs.
        if let Some(oslog) = self.oslog.take() {
            if oslog.stop_within(budget.slice(OSLOG_STOP_GRACE)).await {
                log::warn!(
                    "teardown: oslog pump did not stop in time — SIGKILLed its process group"
                );
            } else {
                log::info!("teardown: oslog pump stopped");
            }
        }
        if let Some(tailers) = self.tailers.take() {
            match tokio::time::timeout(budget.slice(CONSOLE_DRAIN_GRACE), tailers.stop()).await {
                Ok(()) => log::info!("teardown: tailers stopped"),
                Err(_) => log::warn!("teardown: final console drain cut short"),
            }
        }

        // Xcode Stop semantics: the app dies with the session — but only if
        // it is still *our* app. Two ways it may not be: a newer instance
        // superseded us (Rerun / second session re-claimed the pidfile and
        // SIGTERM'd us after launching its own app over ours), or our own
        // debuggee already exited (e.g. a successor's `simctl install`
        // replaced the running bundle) — in both cases a bundle-id terminate
        // here would hit the successor's app or nothing. Otherwise terminate
        // when terminateOnStop is set, or whenever we never attached (a
        // suspended `--wait-for-debugger` app must not be left frozen).
        // Bounded so a wedged simctl can't stall the answer.
        if let Some(mut done) = self.session.take() {
            if let Some(mut child) = done.mock_child.take() {
                let _ = child.start_kill();
                let _ = tokio::time::timeout(FORCED_REAP_GRACE, child.wait()).await;
                log::info!("teardown: mock app killed");
            } else {
                let terminate_on_stop = done.config.as_ref().is_some_and(|c| c.terminate_on_stop);
                if should_terminate_on_teardown(
                    self.attached,
                    terminate_on_stop,
                    self.superseded(),
                    self.debuggee_exited,
                ) {
                    log::info!(
                        "teardown: terminating {} on {}",
                        done.app.bundle_id,
                        done.app.udid
                    );
                    let terminate = simctl::terminate(&done.app.udid, &done.app.bundle_id);
                    if tokio::time::timeout(budget.slice(TEARDOWN_GRACE), terminate)
                        .await
                        .is_err()
                    {
                        log::warn!("teardown: simctl terminate did not finish in time");
                    }
                }
            }
        }

        if let Some(udid) = self.pidfile_udid.take() {
            let _ = pidfile::remove(&udid);
            log::info!("teardown: pidfile released (udid {udid})");
        }

        // Closing the channel ends the writer task, dropping ChildStdin
        // (lldb-dap sees stdin EOF and exits on its own in the normal path).
        drop(self.to_child.take());
        let _ = tokio::time::timeout(budget.slice(TEARDOWN_GRACE), child_writer).await;
        log::info!("teardown: lldb-dap stdin closed");

        if let Some(mut lldb) = self.lldb.take() {
            let grace = if self.lldb_wedged {
                Duration::ZERO // it already ignored the stop request
            } else {
                budget.slice(TEARDOWN_GRACE)
            };
            match tokio::time::timeout(grace, lldb.child.wait()).await {
                Ok(_) => log::info!("teardown: lldb-dap exited"),
                Err(_) => {
                    // Still alive at the deadline (or wedged) — kill
                    // (kill_on_drop also covers panics/early returns).
                    if self.lldb_wedged {
                        log::warn!("teardown: lldb-dap left the stop request unanswered — killing");
                    } else {
                        log::warn!("teardown: lldb-dap still alive at the deadline — killing");
                    }
                    let _ = lldb.child.start_kill();
                    let _ = tokio::time::timeout(FORCED_REAP_GRACE, lldb.child.wait()).await;
                }
            }
        }
        // The reader only forwards lldb-dap frames nobody reads any more.
        child_reader.abort();
        log::info!("teardown: done in {} ms", started.elapsed().as_millis());

        // --- the answer, then the flush ----------------------------------------
        if !self.held_failure.is_empty() {
            log::info!(
                "launch (seq {}): failure answered after teardown",
                self.launch_seq.unwrap_or(0)
            );
            for out in std::mem::take(&mut self.held_failure) {
                self.send_to_client(out);
            }
        }
        if let Some(stop) = self.stop.take() {
            log::info!(
                "{} (seq {}): answered after teardown",
                stop.command,
                stop.seq
            );
            for out in stop.answers() {
                self.send_to_client(out);
            }
        }
        // Drain whatever is still queued for Zed, then flush.
        drop(self.to_client);
        let _ = tokio::time::timeout(TEARDOWN_GRACE, client_writer).await;
    }
}

/// C source of the mock dummy app: appends a line to its stdout capture
/// file every 500 ms (and one line to the stderr file at start). Compiled
/// locally because lldb cannot attach to Apple-signed binaries like
/// `/bin/sh` under SIP — an ad-hoc-signed local build attaches fine.
const MOCK_APP_C: &str = r#"
#include <stdio.h>
#include <unistd.h>
int main(int argc, char **argv) {
    if (argc < 3) return 2;
    FILE *out = fopen(argv[1], "a");
    FILE *err = fopen(argv[2], "a");
    if (!out || !err) return 2;
    fprintf(err, "mock-app stderr ready\n");
    fflush(err);
    for (int i = 0; i < 2400; i++) {
        fprintf(out, "mock-app stdout line %d\n", i);
        fflush(out);
        usleep(500000);
    }
    return 0;
}
"#;

/// Hidden `--mock-pipeline` pathway: skip xcodebuild/simctl entirely.
/// Compiles a tiny local C program into `~/.zedxcode/run/mock/`, spawns it
/// writing to fake out.log/err.log capture files, and returns a
/// `LaunchedApp` pointing at it. The rest of the DAP flow (attach via real
/// lldb-dap, tailers, teardown) is exercised unchanged.
async fn mock_pipeline(
    sink: &dyn OutputSink,
    cancel: CancellationToken,
) -> Result<(LaunchedApp, Child)> {
    sink.line("console", "Mock pipeline: skipping xcodebuild/simctl");
    let run_dir = pipeline::zedxcode_home()?.join("run").join("mock");
    tokio::fs::create_dir_all(&run_dir)
        .await
        .with_context(|| format!("creating {}", run_dir.display()))?;
    let stdout_file = run_dir.join("out.log");
    let stderr_file = run_dir.join("err.log");
    for f in [&stdout_file, &stderr_file] {
        tokio::fs::File::create(f)
            .await
            .with_context(|| format!("truncating {}", f.display()))?;
    }

    sink.line("console", "Mock pipeline: compiling dummy app...");
    let src = run_dir.join("mock_app.c");
    let exe = run_dir.join("mock_app");
    tokio::fs::write(&src, MOCK_APP_C)
        .await
        .context("writing mock_app.c")?;
    let mut cc = Command::new("cc");
    cc.arg("-o")
        .arg(&exe)
        .arg(&src)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let out = tokio::select! {
        out = cc.output() => out.context("running cc")?,
        _ = cancel.cancelled() => bail!("cancelled"),
    };
    if !out.status.success() {
        bail!(
            "compiling the mock app failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    let child = Command::new(&exe)
        .arg(&stdout_file)
        .arg(&stderr_file)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("spawning the mock app")?;
    let pid = child.id().context("mock app has no pid")? as i64;
    sink.line("console", &format!("Launched mock app (pid {pid})"));

    Ok((
        LaunchedApp {
            pid,
            udid: "mock".into(),
            bundle_id: "dev.zedxcode.mock-app".into(),
            app_path: exe,
            stdout_file,
            stderr_file,
        },
        child,
    ))
}

/// Spawn the single-writer task for one sink. Every frame written to `W`
/// goes through the returned channel, so frames never interleave. The
/// channel is unbounded so sync contexts (`OutputSink::line`) can send.
fn spawn_writer<W>(mut sink: W) -> (mpsc::UnboundedSender<Out>, JoinHandle<()>)
where
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (tx, mut rx) = mpsc::unbounded_channel::<Out>();
    let handle = tokio::spawn(async move {
        while let Some(out) = rx.recv().await {
            let bytes = match out {
                Out::Raw(body) => framing::frame(&body),
                Out::Msg(value) => match serde_json::to_vec(&value) {
                    Ok(body) => framing::frame(&body),
                    Err(e) => {
                        log::error!("failed to serialize proxy message: {e}");
                        eprintln!("xcode-dap: failed to serialize proxy message: {e}");
                        continue;
                    }
                },
            };
            if sink.write_all(&bytes).await.is_err() {
                break;
            }
            if sink.flush().await.is_err() {
                break;
            }
        }
        let _ = sink.shutdown().await;
    });
    (tx, handle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminates_attached_app_with_terminate_on_stop() {
        // Normal Stop of a live, attached app: terminate it.
        assert!(should_terminate_on_teardown(true, true, false, false));
    }

    #[test]
    fn keeps_attached_app_when_opted_out() {
        // terminateOnStop=false leaves an attached app running on Stop.
        assert!(!should_terminate_on_teardown(true, false, false, false));
    }

    #[test]
    fn terminates_never_attached_suspended_app() {
        // Never attached: the app is suspended under --wait-for-debugger and
        // must not be left frozen, regardless of terminateOnStop.
        assert!(should_terminate_on_teardown(false, false, false, false));
    }

    #[test]
    fn skips_terminate_when_superseded() {
        // A successor re-claimed the pidfile: never terminate (would kill its
        // freshly launched app).
        assert!(!should_terminate_on_teardown(true, true, true, false));
        assert!(!should_terminate_on_teardown(false, false, true, false));
    }

    #[test]
    fn skips_terminate_when_debuggee_exited() {
        // Regression: the second-⌘R crash path. Our debuggee already exited
        // (its bundle was replaced by a successor's install), so there is
        // nothing of ours to terminate and the bundle id may now be the
        // successor's — never terminate, in every attach/opt-out combination.
        assert!(!should_terminate_on_teardown(true, true, false, true));
        assert!(!should_terminate_on_teardown(true, false, false, true));
        assert!(!should_terminate_on_teardown(false, false, false, true));
    }

    #[test]
    fn budget_slice_is_capped_by_the_step() {
        let now = Instant::now();
        let budget = Budget::critical(now);
        assert_eq!(budget.slice_at(now, OSLOG_STOP_GRACE), OSLOG_STOP_GRACE);
    }

    #[test]
    fn budget_slice_shrinks_to_what_is_left() {
        let now = Instant::now();
        let budget = Budget::critical(now);
        let later = now + graceful_budget() - Duration::from_millis(200);
        assert_eq!(
            budget.slice_at(later, TEARDOWN_GRACE),
            Duration::from_millis(200)
        );
    }

    #[test]
    fn budget_slice_is_zero_once_spent() {
        let now = Instant::now();
        let budget = Budget::critical(now);
        assert_eq!(
            budget.slice_at(now + graceful_budget(), TEARDOWN_GRACE),
            Duration::ZERO
        );
        assert_eq!(
            budget.slice_at(now + Duration::from_secs(60), TEARDOWN_GRACE),
            Duration::ZERO
        );
    }

    #[test]
    fn worst_case_teardown_stays_within_the_critical_budget() {
        // Every step uses its whole slice, in teardown order: OSLog group,
        // console drain, simctl terminate, lldb-dap stdin flush, lldb-dap
        // exit. The graceful waits never exceed the shared budget, and with
        // every forced reap on top the hold stays within the critical budget.
        let start = Instant::now();
        let budget = Budget::critical(start);
        let mut now = start;
        for cap in [
            OSLOG_STOP_GRACE,
            CONSOLE_DRAIN_GRACE,
            TEARDOWN_GRACE,
            TEARDOWN_GRACE,
            TEARDOWN_GRACE,
        ] {
            now += budget.slice_at(now, cap);
        }
        let graceful = now - start;
        assert!(graceful <= graceful_budget(), "{graceful:?}");
        assert!(graceful + FORCED_REAP_GRACE * FORCED_REAPS <= CRITICAL_TEARDOWN_BUDGET);
    }

    #[test]
    fn critical_budget_leaves_room_after_the_capped_steps() {
        // The capped steps (OSLog stop, console drain) must leave time for
        // the app terminate and the lldb-dap reap after them.
        assert_eq!(
            graceful_budget() + FORCED_REAP_GRACE * FORCED_REAPS,
            CRITICAL_TEARDOWN_BUDGET
        );
        assert!(OSLOG_STOP_GRACE + CONSOLE_DRAIN_GRACE < graceful_budget());
    }

    #[test]
    fn unhurried_budget_gives_every_step_the_full_grace() {
        // No answer waits (SIGTERM, EOF, attach timeout): the app terminate
        // must not get only what the OSLog stop and the drain left over.
        let now = Instant::now();
        let budget = Budget::unhurried();
        assert_eq!(budget.slice_at(now, OSLOG_STOP_GRACE), TEARDOWN_GRACE);
        assert_eq!(
            budget.slice_at(now + Duration::from_secs(60), TEARDOWN_GRACE),
            TEARDOWN_GRACE
        );
    }

    #[test]
    fn pipeline_drain_without_a_stop_covers_the_build_escalation() {
        // EOF / SIGTERM mid-build: nothing waits, so the build gets its full
        // SIGTERM -> 3 s -> SIGKILL escalation.
        let now = Instant::now();
        assert_eq!(pipeline_drain_grace(None, now), PIPELINE_DRAIN_GRACE);
        assert!(PIPELINE_DRAIN_GRACE > Duration::from_secs(3));
    }

    #[test]
    fn pipeline_drain_with_a_stop_takes_only_what_is_left_of_its_grace() {
        let received = Instant::now();
        assert_eq!(
            pipeline_drain_grace(Some(received), received),
            PIPELINE_STOP_GRACE
        );
        assert_eq!(
            pipeline_drain_grace(Some(received), received + Duration::from_secs(1)),
            PIPELINE_STOP_GRACE - Duration::from_secs(1)
        );
        // The routing loop already waited the whole grace (a hung step):
        // teardown does not wait again.
        assert_eq!(
            pipeline_drain_grace(Some(received), received + PIPELINE_STOP_GRACE),
            Duration::ZERO
        );
        assert_eq!(
            pipeline_drain_grace(Some(received), received + Duration::from_secs(60)),
            Duration::ZERO
        );
    }

    #[test]
    fn stop_grace_covers_the_cancel_cleanup_and_stays_short() {
        // Long enough for xcodebuild's (and the preflight's) SIGTERM -> 3 s
        // -> SIGKILL, so a normal cancel never hits it; short enough that a
        // mid-pipeline Stop, critical teardown included, is answered within
        // 6 s however long the cancelled step hangs.
        assert!(PIPELINE_STOP_GRACE > Duration::from_secs(3));
        assert!(PIPELINE_STOP_GRACE + CRITICAL_TEARDOWN_BUDGET <= Duration::from_secs(6));
    }

    fn message(out: &Out) -> Value {
        match out {
            Out::Msg(v) => v.clone(),
            Out::Raw(raw) => serde_json::from_slice(raw).unwrap(),
        }
    }

    #[test]
    fn own_answer_is_a_bare_success_for_the_request() {
        let answers = StopRequest::new(7, "terminate").answers();
        assert_eq!(answers.len(), 1);
        let msg = message(&answers[0]);
        assert_eq!(msg["type"], "response");
        assert_eq!(msg["request_seq"], 7);
        assert_eq!(msg["command"], "terminate");
        assert_eq!(msg["success"], true);
    }

    #[test]
    fn lldb_dap_answer_is_passed_on_verbatim() {
        let raw = br#"{"seq":31,"type":"response","request_seq":7,"command":"disconnect","success":true}"#;
        let mut stop = StopRequest::new(7, "disconnect");
        stop.response = Some(raw.to_vec());
        let answers = stop.answers();
        assert_eq!(answers.len(), 1);
        match &answers[0] {
            Out::Raw(bytes) => assert_eq!(bytes.as_slice(), raw.as_slice()),
            Out::Msg(_) => panic!("expected the held bytes"),
        }
    }

    #[test]
    fn cancelled_pipeline_answer_is_followed_by_terminated() {
        let mut stop = StopRequest::new(5, "disconnect");
        stop.terminated_event = true;
        stop.repeats.push((6, "disconnect"));
        let answers: Vec<Value> = stop.answers().iter().map(message).collect();
        assert_eq!(answers.len(), 3);
        assert_eq!(answers[0]["request_seq"], 5);
        assert_eq!(answers[1]["request_seq"], 6);
        assert_eq!(answers[1]["success"], true);
        assert_eq!(answers[2]["event"], "terminated");
    }
}
