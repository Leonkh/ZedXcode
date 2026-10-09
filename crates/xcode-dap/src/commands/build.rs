//! `xcode-dap build` — pipeline phases 1-4 only; exit code = xcodebuild's.
//! This is what `.zed/tasks.json` "Xcode: Build" (CMD+B) calls.

use std::path::{Path, PathBuf};

use anyhow::Context;
use tokio_util::sync::CancellationToken;

use crate::engine::pipeline::{self, OutputSink};
use crate::engine::project;
use crate::engine::selection::{CliFlags, Request};
use crate::engine::xcodebuild::BuildFailed;
use crate::setup::project::legacy_task_invocation;

/// Shared build/run argument set. Every flag is optional: the workspace is
/// found in the project, and Scheme, Destination and Configuration come
/// from the project's choices (the pickers). A flag overrides them for this
/// invocation only and is never saved.
#[derive(clap::Args, Debug)]
pub struct BuildArgs {
    /// Path to .xcworkspace / .xcodeproj (default: the Xcode scenario's
    /// "workspace", else the one found within two levels of the project root)
    #[arg(long, short = 'w')]
    pub workspace: Option<PathBuf>,
    /// Scheme for this invocation only, e.g. "MyApp (staging)" (default: the
    /// chosen scheme, else the container's only one)
    #[arg(long, short = 's')]
    pub scheme: Option<String>,
    /// Simulator for this invocation only: a UDID or a device name (default:
    /// the chosen destination, else the booted iPhone, else the newest
    /// iPhone). `--device` is the 0.1 spelling
    #[arg(long, alias = "device", value_name = "UDID|NAME")]
    pub destination: Option<String>,
    /// Simulator iOS version, e.g. "18.6": narrows --destination, or the
    /// chosen or automatic simulator
    #[arg(long)]
    pub os: Option<String>,
    /// Build configuration for this invocation only, e.g. Release (default:
    /// the chosen configuration, else the scheme's Run configuration)
    #[arg(long)]
    pub configuration: Option<String>,
    /// DerivedData directory (xcodebuild -derivedDataPath); default: xcodebuild's per-workspace location
    #[arg(long)]
    pub derived_data: Option<PathBuf>,
    /// Disable build-log filtering (stream full xcodebuild output)
    #[arg(long)]
    pub full_output: bool,
    /// Hidden (testing): pump OSLog (`log stream`) into the console
    /// (`run` only; the supported path is `"oslog": true` in .zed/debug.json)
    #[arg(long, hide = true)]
    pub oslog: bool,
    /// Hidden (testing): custom NSPredicate for the OSLog pump (`run` only;
    /// the supported path is `"oslogPredicate"` in .zed/debug.json)
    #[arg(long, hide = true)]
    pub oslog_predicate: Option<String>,
}

impl BuildArgs {
    /// The request for this invocation, at the project root found from the
    /// current directory.
    pub(crate) fn to_request(&self) -> anyhow::Result<Request> {
        let cwd = std::env::current_dir().context("reading the current directory")?;
        Ok(self.request_at(cli_root()?, &cwd, &invocation_args()))
    }

    /// The request for this invocation, at `root`; `args` is the command
    /// line after the program name (a 0.1 task's keeps 0.1's precedence).
    pub(crate) fn request_at(&self, root: PathBuf, cwd: &Path, args: &[String]) -> Request {
        cli_request(root, self.cli_flags(cwd), args)
    }

    /// The flags; relative paths are taken from `cwd`.
    fn cli_flags(&self, cwd: &Path) -> CliFlags {
        CliFlags {
            workspace: self.workspace.as_deref().map(|w| absolute(cwd, w)),
            scheme: self.scheme.clone(),
            destination: self.destination.clone(),
            os: self.os.clone(),
            configuration: self.configuration.clone(),
            derived_data: self.derived_data.clone(),
            full_output: self.full_output,
            oslog: self.oslog,
            oslog_predicate: self.oslog_predicate.clone(),
            legacy_task: None,
        }
    }
}

/// `path` made absolute against `cwd` (flags are relative to the current
/// directory, not to the project root).
pub(crate) fn absolute(cwd: &Path, path: &Path) -> PathBuf {
    let joined = cwd.join(path);
    std::path::absolute(&joined).unwrap_or(joined)
}

/// This process's command line after the program name.
pub(crate) fn invocation_args() -> Vec<String> {
    std::env::args_os()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect()
}

/// The request for a build, run or clean at `root`. Its flags override the
/// project's choices for this invocation only, unless `args` (the command
/// line after the program name) is one of the tasks 0.1's setup wrote into
/// `<root>/.zed/tasks.json`: those flags rank below the project's choices,
/// as in 0.1, and one line names the migration.
pub(crate) fn cli_request(root: PathBuf, mut flags: CliFlags, args: &[String]) -> Request {
    flags.legacy_task = legacy_task_invocation(&root, args);
    Request::for_cli(root, flags)
}

/// The project root for a command run from a terminal or a task.
pub(crate) fn cli_root() -> anyhow::Result<PathBuf> {
    let cwd = std::env::current_dir().context("reading the current directory")?;
    let zed_worktree_root = std::env::var_os("ZED_WORKTREE_ROOT").map(PathBuf::from);
    Ok(project::root_for_cli(&cwd, zed_worktree_root.as_deref()))
}

/// CLI `OutputSink`: app stdout to stdout, everything else to stderr.
pub(crate) struct CliSink;

impl OutputSink for CliSink {
    fn line(&self, category: &str, text: &str) {
        if category == "stdout" {
            println!("{text}");
        } else {
            eprintln!("{text}");
        }
    }
}

/// Map a pipeline error to the process exit: xcodebuild failures exit with
/// xcodebuild's own code; everything else bubbles up as anyhow (exit 1).
pub(crate) fn exit_with_build_code(err: anyhow::Error) -> anyhow::Result<()> {
    if let Some(failed) = err.downcast_ref::<BuildFailed>() {
        eprintln!("{failed}");
        std::process::exit(failed.code);
    }
    Err(err)
}

/// Spawn a task that cancels `token` on Ctrl-C (SIGINT).
pub(crate) fn cancel_on_ctrl_c(token: CancellationToken) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            token.cancel();
        }
    });
}

pub async fn run(args: BuildArgs) -> anyhow::Result<()> {
    let req = args.to_request()?;
    let cancel = CancellationToken::new();
    cancel_on_ctrl_c(cancel.clone());
    match pipeline::run_build(&req, &CliSink, cancel).await {
        Ok((_udid, app)) => {
            eprintln!("Build succeeded: {}", app.display());
            Ok(())
        }
        Err(err) => exit_with_build_code(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::clean::CleanArgs;
    use crate::engine::config::BuildOutput;
    use crate::engine::destinations::Inventory;
    use crate::engine::schemes::{self, SchemeList};
    use crate::engine::selection::{self, FlagRank, Source, StoredDestination};
    use crate::engine::xcodebuild::Target;
    use clap::Parser;
    use serde_json::{json, Map};
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[derive(Parser)]
    struct Cli {
        #[command(subcommand)]
        command: Sub,
    }

    #[derive(clap::Subcommand)]
    enum Sub {
        Build(BuildArgs),
        Clean(CleanArgs),
    }

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A fresh directory, canonical (macOS reaches the temp dir through a
    /// symlink), like the roots the CLI works with.
    fn sandbox() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zedxcode-build-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    fn copy(from: &Path, to: &Path) {
        fs::create_dir_all(to).unwrap();
        for entry in fs::read_dir(from).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name();
            if name == "xcuserdata" {
                continue;
            }
            if entry.file_type().unwrap().is_dir() {
                copy(&entry.path(), &to.join(&name));
            } else {
                fs::copy(entry.path(), to.join(&name)).unwrap();
            }
        }
    }

    const IPHONE_15_PRO: &str = "AAAAAAAA-1111-2222-3333-000000000015";
    const IPHONE_16E: &str = "CCCCCCCC-1111-2222-3333-000000000016";

    /// The `xcworkspace` layout fixture as 0.1's `setup --project` left it:
    /// the `.zed/debug.json` scenario and the six `.zed/tasks.json` tasks of
    /// `tests/fixtures/setup-0.1`, which bake scheme MyApp Dev and an
    /// iPhone 15 Pro on iOS 17.5 into their flags. Its store chose MyApp on
    /// an iPhone 16e (iOS 18.6).
    fn v0_1_project() -> PathBuf {
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures");
        let root = sandbox().join("MyApp");
        copy(&fixtures.join("layouts/xcworkspace"), &root);
        copy(&fixtures.join("setup-0.1"), &root.join(".zed"));
        selection::update(&root, |store| {
            store.choose_scheme("MyApp");
            store.choose_destination(StoredDestination {
                kind: selection::KIND_SIMULATOR.into(),
                udid: Some(IPHONE_16E.into()),
                name: Some("iPhone 16e".into()),
                os: Some("18.6".into()),
                extra: Map::new(),
            });
        })
        .unwrap();
        root
    }

    /// What `xcodebuild -list -json` prints for the fixture's workspace,
    /// with a second scheme.
    fn scheme_list() -> SchemeList {
        schemes::parse(br#"{"workspace": {"name": "MyApp", "schemes": ["MyApp", "MyApp Dev"]}}"#)
            .unwrap()
    }

    fn inventory() -> Inventory {
        let device = |udid: &str, name: &str, model: &str| {
            json!({ "udid": udid, "name": name, "state": "Shutdown", "isAvailable": true,
                    "deviceTypeIdentifier": format!("com.apple.CoreSimulator.SimDeviceType.{model}") })
        };
        Inventory::parse(&json!({ "devices": {
            "com.apple.CoreSimulator.SimRuntime.iOS-17-5":
                [device(IPHONE_15_PRO, "iPhone 15 Pro", "iPhone-15-Pro")],
            "com.apple.CoreSimulator.SimRuntime.iOS-18-6":
                [device(IPHONE_16E, "iPhone 16e", "iPhone-16e")],
        }}))
        .unwrap()
    }

    /// The request a command line makes at `root`, run from `root` as Zed
    /// runs the tasks (cwd `$ZED_WORKTREE_ROOT`).
    fn request(root: &Path, args: &[&str]) -> Request {
        let argv = std::iter::once("xcode-dap").chain(args.iter().copied());
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        match Cli::try_parse_from(argv).unwrap().command {
            Sub::Build(build) => build.request_at(root.to_path_buf(), root, &args),
            Sub::Clean(clean) => clean.request_at(root.to_path_buf(), root, &args),
        }
    }

    /// What the pipeline would build for `req` and on which simulator,
    /// through the same steps as `pipeline::prepare` with a canned scheme
    /// list and simulator inventory (no xcodebuild, no simctl).
    fn would_build(req: &Request) -> (Target, String) {
        let resolution = selection::resolve(req).unwrap();
        let container = resolution.project.require_container().unwrap().path.clone();
        let settled = pipeline::settle_target(
            &req.options,
            &resolution.picks,
            container,
            Some(&scheme_list()),
        )
        .unwrap();
        let (device, _) =
            selection::settle_destination(resolution.picks.destination.as_ref(), &inventory())
                .unwrap();
        (settled.target, device.value.udid)
    }

    fn target(root: &Path, scheme: &str) -> Target {
        Target {
            workspace: root.join("MyApp.xcworkspace"),
            scheme: scheme.into(),
            configuration: None,
            derived_data: None,
            build_output: BuildOutput::Filtered,
        }
    }

    /// The 0.1 Build task's command line, as Zed and the shell hand it to
    /// xcode-dap (setup quoted "MyApp Dev" and "iPhone 15 Pro" in the file).
    const V0_1_BUILD: [&str; 9] = [
        "build",
        "--workspace",
        "MyApp.xcworkspace",
        "--scheme",
        "MyApp Dev",
        "--device",
        "iPhone 15 Pro",
        "--os",
        "17.5",
    ];

    #[test]
    fn a_0_1_task_builds_what_the_store_chose_over_its_baked_flags() {
        let root = v0_1_project();
        let store = fs::read(selection::store_path(&root)).unwrap();

        let req = request(&root, &V0_1_BUILD);
        assert_eq!(req.flag_rank, FlagRank::BelowStore);
        assert_eq!(req.warnings, [selection::legacy_task_line("Xcode: Build")]);
        assert_eq!(
            req.warnings[0],
            "\"Xcode: Build\" is a task of the 0.1 setup (.zed/tasks.json): the scheme, \
             destination and configuration chosen for this project outrank its flags. Run \
             Xcode: Set Up Project (or \"xcode-dap setup\") once to migrate."
        );
        assert_eq!(
            would_build(&req),
            (target(&root, "MyApp"), IPHONE_16E.to_owned())
        );

        // The 0.1 Clean task too.
        let req = request(
            &root,
            &[
                "clean",
                "--workspace",
                "MyApp.xcworkspace",
                "--scheme",
                "MyApp Dev",
            ],
        );
        assert_eq!(req.warnings, [selection::legacy_task_line("Xcode: Clean")]);
        assert_eq!(would_build(&req).0, target(&root, "MyApp"));

        // Without a choice in the store, the baked flags still beat the
        // scenario's keys, as in 0.1.
        fs::remove_file(selection::store_path(&root)).unwrap();
        let resolution = selection::resolve(&request(&root, &V0_1_BUILD)).unwrap();
        assert_eq!(resolution.picks.scheme.unwrap().source, Source::Flag);
        fs::write(selection::store_path(&root), &store).unwrap();

        // Nothing was written: the flags are never saved.
        assert_eq!(fs::read(selection::store_path(&root)).unwrap(), store);
    }

    #[test]
    fn flags_typed_by_hand_override_the_store_for_one_invocation() {
        let root = v0_1_project();
        let store = fs::read(selection::store_path(&root)).unwrap();

        let req = request(
            &root,
            &[
                "build",
                "-w",
                "MyApp.xcworkspace",
                "-s",
                "MyApp Dev",
                "--destination",
                "iPhone 15 Pro",
                "--configuration",
                "Release",
            ],
        );
        assert_eq!(req.flag_rank, FlagRank::First);
        assert!(req.warnings.is_empty());
        let (built, udid) = would_build(&req);
        assert_eq!(
            built,
            Target {
                configuration: Some("Release".into()),
                ..target(&root, "MyApp Dev")
            }
        );
        assert_eq!(udid, IPHONE_15_PRO);

        // 0.1's `--device` still works, as a one-off.
        let req = request(&root, &["build", "--device", IPHONE_15_PRO]);
        assert_eq!(would_build(&req).1, IPHONE_15_PRO);

        // The store is untouched, so the next plain build uses its choice.
        assert_eq!(fs::read(selection::store_path(&root)).unwrap(), store);
        let req = request(&root, &["build"]);
        assert_eq!(
            would_build(&req),
            (target(&root, "MyApp"), IPHONE_16E.to_owned())
        );
    }

    #[test]
    fn every_build_and_clean_flag_is_optional() {
        let root = v0_1_project();
        assert!(Cli::try_parse_from(["xcode-dap", "build"]).is_ok());
        assert!(Cli::try_parse_from(["xcode-dap", "clean"]).is_ok());
        // One destination flag at a time.
        assert!(Cli::try_parse_from([
            "xcode-dap",
            "build",
            "--device",
            "iPhone 15 Pro",
            "--destination",
            "iPhone 16e"
        ])
        .is_err());
        // Without flags the workspace comes from the Xcode scenario, and
        // without one it is found in the project.
        let req = request(&root, &["clean"]);
        assert_eq!(req.workspace, Some(root.join("MyApp.xcworkspace")));
        fs::remove_file(root.join(".zed/debug.json")).unwrap();
        let req = request(&root, &["clean"]);
        assert_eq!(req.workspace, None);
        assert_eq!(would_build(&req).0, target(&root, "MyApp"));
    }
}
