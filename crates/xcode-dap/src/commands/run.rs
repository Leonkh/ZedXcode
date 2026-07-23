//! `xcode-dap run` — pipeline phases 1-8 without the debugger: launch
//! *without* `--wait-for-debugger`, stream the app console to the terminal
//! via the file tailers. Ctrl-C detaches (the app keeps running).

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::commands::build::{cancel_on_ctrl_c, exit_with_build_code, BuildArgs, CliSink};
use crate::engine::{consoles, pipeline};

pub async fn run(args: BuildArgs) -> anyhow::Result<()> {
    let cfg = args.to_config();
    let sink: Arc<CliSink> = Arc::new(CliSink);
    let cancel = CancellationToken::new();
    cancel_on_ctrl_c(cancel.clone());

    let launched = match pipeline::run_pipeline(&cfg, false, sink.as_ref(), cancel.clone()).await {
        Ok(launched) => launched,
        Err(err) => return exit_with_build_code(err),
    };

    eprintln!(
        "Streaming console of {} (pid {}) — Ctrl-C to detach (app keeps running)",
        launched.bundle_id, launched.pid
    );
    let tailers =
        consoles::start_tailers(&launched.stdout_file, &launched.stderr_file, sink.clone());
    let oslog = if cfg.oslog {
        let app_name = launched
            .app_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let predicate = cfg
            .oslog_predicate
            .clone()
            .unwrap_or_else(|| consoles::default_oslog_predicate(&launched.bundle_id, app_name));
        Some(consoles::start_oslog_pump(
            &launched.udid,
            &predicate,
            sink.clone(),
        ))
    } else {
        None
    };

    // Stream until Ctrl-C (detach) or the app exits.
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                eprintln!("Detached from {}", launched.bundle_id);
                break;
            }
            _ = tokio::time::sleep(Duration::from_secs(2)) => {
                if !pid_alive(launched.pid) {
                    eprintln!("App {} (pid {}) exited", launched.bundle_id, launched.pid);
                    break;
                }
            }
        }
    }
    if let Some(oslog) = oslog {
        oslog.stop().await;
    }
    tailers.stop().await; // final drain
    Ok(())
}

fn pid_alive(pid: i64) -> bool {
    // SAFETY: signal 0 only checks for existence/permission.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}
