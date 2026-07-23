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
//! - The hidden `--mock-pipeline` mode skips xcodebuild/simctl entirely and
//!   attaches lldb-dap to a locally compiled dummy process, exercising the
//!   whole DAP flow without Xcode in seconds.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use xcode_dap_config::LaunchConfig;

use crate::dap::framing::{self, DapReader};
use crate::dap::lldb::LldbDap;
use crate::dap::peek::{self, ChildMsg, ClientMsg};
use crate::engine::consoles::{self, Tailers};
use crate::engine::pipeline::{self, LaunchedApp, OutputSink};
use crate::engine::simctl;
use crate::util::logging;
use crate::util::pidfile;

/// How long we wait for the client's `initialize` before deciding this was
/// an accidental CLI invocation.
const INIT_GUARD: Duration = Duration::from_secs(2);

/// How long teardown waits for children / writer flushes.
const TEARDOWN_GRACE: Duration = Duration::from_secs(2);

/// How long teardown waits for a cancelled pipeline to wind down. Must
/// cover xcodebuild's SIGTERM -> 3 s -> SIGKILL escalation (xcodebuild.rs);
/// exiting earlier would orphan the build (kill_on_drop dies with us).
const PIPELINE_DRAIN_GRACE: Duration = Duration::from_secs(10);

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
    /// `None` in mock mode (the mock ignores the scenario config).
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
    /// Disconnect seq received while the pipeline ran; answered ourselves
    /// once the cancelled pipeline winds down.
    pending_disconnect: Option<i64>,
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
        pending_disconnect: None,
        session: None,
        tailers: None,
        oslog: None,
        pidfile_udid: None,
        attached: false,
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
                    match proxy.on_pipeline_result(res).await {
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
            _ = sigterm.recv() => {
                log::info!("SIGTERM received");
                break;
            }
            _ = sigint.recv() => {
                log::info!("SIGINT received");
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
            ClientMsg::Disconnect { seq, raw } => {
                if self.pipeline_running {
                    // Mid-build Stop: cancel the pipeline (kills the
                    // xcodebuild process group); the pipe_rx arm answers the
                    // disconnect once the pipeline has wound down.
                    log::info!("disconnect received (seq {seq}) mid-pipeline — cancelling");
                    self.pending_disconnect = Some(seq);
                    if let Some(cancel) = &self.pipeline_cancel {
                        cancel.cancel();
                    }
                    Ok(LoopAction::Continue)
                } else {
                    log::info!("disconnect received (seq {seq}) — forwarding to lldb-dap");
                    // lldb-dap handles terminate/detach semantics per the
                    // request; its response passes back through. Teardown
                    // adds the belt-and-braces `simctl terminate`.
                    self.send_to_child_raw(raw)?;
                    Ok(LoopAction::Continue)
                }
            }
            ClientMsg::Other { raw } => {
                self.send_to_child_raw(raw)?;
                Ok(LoopAction::Continue)
            }
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
            tokio::spawn(async move {
                let res = mock_pipeline(&sink, cancel)
                    .await
                    .map(|(app, child)| PipelineDone {
                        app,
                        config: None,
                        mock_child: Some(child),
                    });
                let _ = pipe_tx.send(res).await;
            });
        } else {
            let cfg: LaunchConfig = match serde_json::from_value(args) {
                Ok(cfg) => cfg,
                Err(e) => return self.fail_launch(&format!("invalid launch configuration: {e}")),
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
                "launch intercepted (seq {seq}): workspace {}, scheme {:?}, device {:?}, \
                 os {:?}, configuration {:?}, preflight {}, oslog {}, buildOutput {:?}, \
                 terminateOnStop {}",
                cfg.workspace.display(),
                cfg.scheme,
                cfg.device,
                cfg.os,
                cfg.configuration,
                if cfg.preflight.is_some() { "yes" } else { "no" },
                cfg.oslog,
                cfg.build_output,
                cfg.terminate_on_stop,
            );
            tokio::spawn(async move {
                let res = pipeline::run_pipeline(&cfg, true, &sink, cancel)
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
    async fn on_pipeline_result(&mut self, res: Result<PipelineDone>) -> Result<LoopAction> {
        self.pipeline_running = false;
        self.pipeline_cancel = None;

        // A mid-pipeline disconnect cancelled us: answer the disconnect,
        // emit `terminated`, clean up whatever was launched, exit 0.
        if let Some(disc_seq) = self.pending_disconnect.take() {
            if let Ok(mut done) = res {
                // The pipeline won the race anyway — undo the launch. The app
                // was launched `--wait-for-debugger` and never attached, so it
                // is suspended and would hang forever if left: terminate it
                // unconditionally (terminateOnStop only applies to an app that
                // actually ran). Bounded so a wedged simctl can't stall exit.
                if let Some(mut child) = done.mock_child.take() {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                } else {
                    let _ = tokio::time::timeout(
                        TEARDOWN_GRACE,
                        simctl::terminate(&done.app.udid, &done.app.bundle_id),
                    )
                    .await;
                }
            }
            log::info!("pipeline wound down after mid-build disconnect — exiting 0");
            self.send_to_client(Out::Msg(peek::success_response(disc_seq, "disconnect")));
            self.send_to_client(Out::Msg(peek::terminated_event()));
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

        self.session = Some(done);
        Ok(LoopAction::Continue)
    }

    /// Pipeline failure: error response + stderr output + `terminated`,
    /// then graceful exit 1 (teardown still flushes the writer).
    fn fail_launch(&mut self, msg: &str) -> Result<LoopAction> {
        log::error!("launch failed: {msg}");
        let seq = self.launch_seq.unwrap_or(0);
        self.send_to_client(Out::Msg(peek::error_response(seq, "launch", msg)));
        self.send_to_client(Out::Msg(peek::output_event("stderr", &format!("{msg}\n"))));
        self.send_to_client(Out::Msg(peek::terminated_event()));
        Ok(LoopAction::Exit(1))
    }

    /// Route one frame arriving from lldb-dap.
    fn on_child_message(&mut self, raw: &[u8]) -> Result<LoopAction> {
        log_frame("lldb->proxy", raw);
        match peek::classify_child(raw)? {
            ChildMsg::InternalResponse { request_seq, raw } => {
                if self.attach_seq == Some(request_seq) {
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
        // OSLog pump (§5.3): config is None in mock mode, so never here.
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

    /// Clean teardown: wind down a still-running pipeline, stop + drain the
    /// tailers, kill the mock dummy / `simctl terminate` the app, remove
    /// the pidfile, close the child's stdin, kill lldb-dap if still alive,
    /// then drain + flush the client writer so queued frames (e.g. the
    /// disconnect response) reach Zed before exit.
    async fn teardown(
        mut self,
        pipe_rx: &mut mpsc::Receiver<Result<PipelineDone>>,
        child_reader: JoinHandle<()>,
        client_writer: JoinHandle<()>,
        child_writer: JoinHandle<()>,
    ) {
        log::info!("teardown: begin");
        // A pipeline cancelled here (EOF / SIGTERM paths) must finish its
        // own cleanup (xcodebuild pgid kill) before we exit — kill_on_drop
        // does not survive process exit.
        if self.pipeline_running {
            log::info!("teardown: waiting for the cancelled pipeline to wind down");
            if let Some(cancel) = &self.pipeline_cancel {
                cancel.cancel();
            }
            if let Ok(Some(Ok(done))) =
                tokio::time::timeout(PIPELINE_DRAIN_GRACE, pipe_rx.recv()).await
            {
                self.session = Some(done); // launched after all — clean it up below
            }
        }

        // Final drain of app output while the client writer still runs.
        if let Some(oslog) = self.oslog.take() {
            oslog.stop().await;
            log::info!("teardown: oslog pump stopped");
        }
        if let Some(tailers) = self.tailers.take() {
            tailers.stop().await;
            log::info!("teardown: tailers stopped");
        }

        // Xcode Stop semantics: the app dies with the session — but only if
        // it is still *our* app. A newer instance may have superseded us
        // (Rerun / second session): it re-claimed the pidfile and SIGTERM'd
        // us after launching its own app over ours, so a bundle-id terminate
        // here would kill the successor's freshly launched app. Skip when
        // superseded. Otherwise terminate when terminateOnStop is set, or
        // whenever we never attached (a suspended app must not be left
        // frozen). Bounded so a wedged simctl can't stall exit.
        if let Some(mut done) = self.session.take() {
            if let Some(mut child) = done.mock_child.take() {
                let _ = child.start_kill();
                let _ = child.wait().await;
                log::info!("teardown: mock app killed");
            } else if !self.superseded()
                && (!self.attached || done.config.as_ref().is_some_and(|c| c.terminate_on_stop))
            {
                log::info!(
                    "teardown: terminating {} on {}",
                    done.app.bundle_id,
                    done.app.udid
                );
                let _ = tokio::time::timeout(
                    TEARDOWN_GRACE,
                    simctl::terminate(&done.app.udid, &done.app.bundle_id),
                )
                .await;
            }
        }

        if let Some(udid) = self.pidfile_udid.take() {
            let _ = pidfile::remove(&udid);
            log::info!("teardown: pidfile released (udid {udid})");
        }

        // Closing the channel ends the writer task, dropping ChildStdin
        // (lldb-dap sees stdin EOF and exits on its own in the normal path).
        drop(self.to_child.take());
        let _ = tokio::time::timeout(TEARDOWN_GRACE, child_writer).await;
        log::info!("teardown: lldb-dap stdin closed");

        if let Some(mut lldb) = self.lldb.take() {
            match tokio::time::timeout(TEARDOWN_GRACE, lldb.child.wait()).await {
                Ok(_) => log::info!("teardown: lldb-dap exited"),
                Err(_) => {
                    // Still alive after grace period — kill (kill_on_drop
                    // also covers panics/early returns).
                    log::warn!("teardown: lldb-dap still alive after grace period — killing");
                    let _ = lldb.child.start_kill();
                    let _ = lldb.child.wait().await;
                }
            }
        }
        let _ = tokio::time::timeout(TEARDOWN_GRACE, child_reader).await;

        // Drain whatever is still queued for Zed, then flush.
        drop(self.to_client);
        let _ = tokio::time::timeout(TEARDOWN_GRACE, client_writer).await;
        log::info!("teardown: done");
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
    cc.arg("-o").arg(&exe).arg(&src).kill_on_drop(true);
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
