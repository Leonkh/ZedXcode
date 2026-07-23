//! Zed extension "Xcode Tools" (`xcode-tools`) — registers the `Xcode`
//! debug adapter backed by the native `xcode-dap` proxy binary.
//!
//! Implements `docs/design/extension-api.md` §1 (trait surface) and §3
//! (binary delivery). The extension is a thin shim: it locates `xcode-dap`,
//! hands Zed's scenario config to it verbatim, and lets the proxy own the
//! whole build → install → launch → attach pipeline.

use std::fs;
use std::path::Path;

use zed_extension_api::{
    self as zed, current_platform, download_file, github_release_by_tag_name, make_file_executable,
    Architecture, DebugAdapterBinary, DebugConfig, DebugRequest, DebugScenario,
    DebugTaskDefinition, DownloadedFileType, Os, StartDebuggingRequestArguments,
    StartDebuggingRequestArgumentsRequest, Worktree,
};

/// GitHub repo hosting `xcode-dap-v*` releases (per-arch tar.gz assets).
const PROXY_REPO: &str = "Leonkh/ZedXcode";
/// Pinned release tag; bumped in lockstep with the extension version.
const PROXY_TAG: &str = "xcode-dap-v0.1.0";
/// Proxy binary name (on PATH, inside release assets, and in the cache).
const PROXY_BIN: &str = "xcode-dap";

struct XcodeToolsExtension;

impl XcodeToolsExtension {
    /// Resolution order (design §3):
    /// 1. `dap.Xcode.binary` user setting (always wins),
    /// 2. `xcode-dap` on the worktree's PATH (dev mode: `cargo install`),
    /// 3. pinned GitHub release, cached in the extension work dir.
    ///
    /// Returns the absolute path plus its source (`"user-setting"` |
    /// `"worktree-path"` | `"cached-release"` | `"github-release"`),
    /// surfaced to the proxy log via `XCODE_DAP_RESOLVED_FROM`.
    fn resolve_proxy_command(
        &mut self,
        user_provided_path: Option<String>,
        worktree: &Worktree,
    ) -> Result<(String, &'static str), String> {
        if let Some(path) = user_provided_path {
            // Zed resolves the setting relative to the worktree, but guard
            // anyway: `DebugAdapterBinary.command` must be absolute.
            let absolute = if path.starts_with('/') {
                path
            } else {
                format!("{}/{}", worktree.root_path(), path)
            };
            return Ok((absolute, "user-setting"));
        }

        if let Some(path) = worktree.which(PROXY_BIN) {
            return Ok((path, "worktree-path"));
        }

        self.cached_or_downloaded_proxy()
    }

    /// Download the pinned release asset into the extension work dir
    /// (the WASM cwd), or reuse the cached copy. Asset naming contract
    /// (release CI must match): `<tag>-<arch>-apple-darwin.tar.gz`, e.g.
    /// `xcode-dap-v0.1.0-aarch64-apple-darwin.tar.gz`, containing a single
    /// `xcode-dap` binary at the archive root.
    fn cached_or_downloaded_proxy(&mut self) -> Result<(String, &'static str), String> {
        let arch = match current_platform() {
            (Os::Mac, Architecture::Aarch64) => "aarch64",
            (Os::Mac, Architecture::X8664) => "x86_64",
            _ => return Err("the Xcode debug adapter is macOS-only".to_string()),
        };

        let version_dir = format!("{PROXY_BIN}/{PROXY_TAG}");
        let bin_rel = format!("{version_dir}/{PROXY_BIN}");

        let mut source = "cached-release";
        if !Path::new(&bin_rel).exists() {
            match github_release_by_tag_name(PROXY_REPO, PROXY_TAG) {
                Ok(release) => {
                    let asset_name = format!("{PROXY_TAG}-{arch}-apple-darwin.tar.gz");
                    let asset = release
                        .assets
                        .into_iter()
                        .find(|asset| asset.name == asset_name)
                        .ok_or_else(|| {
                            format!("release {PROXY_TAG} of {PROXY_REPO} has no asset {asset_name}")
                        })?;
                    // Download the pinned version into its own dir first; a
                    // failed download must not destroy a working cached copy,
                    // so stale versions are purged only once the new one lands.
                    if let Err(err) = download_file(
                        &asset.download_url,
                        &version_dir,
                        DownloadedFileType::GzipTar,
                    ) {
                        // Offline fallback: reuse any previously cached version.
                        return match Self::newest_cached_proxy() {
                            Some(cached) => Ok((Self::absolutize(&cached), "cached-release")),
                            None => Err(format!("failed to download {asset_name}: {err}")),
                        };
                    }
                    // Belt-and-braces; tar should already preserve the mode.
                    make_file_executable(&bin_rel)
                        .map_err(|err| format!("failed to mark {bin_rel} executable: {err}"))?;
                    Self::purge_stale_versions(&version_dir);
                    source = "github-release";
                }
                Err(github_err) => {
                    // Offline fallback: reuse any previously cached version.
                    return match Self::newest_cached_proxy() {
                        Some(cached) => Ok((Self::absolutize(&cached), "cached-release")),
                        None => Err(format!(
                            "failed to fetch GitHub release {PROXY_TAG} from {PROXY_REPO} \
                             ({github_err}) and no cached {PROXY_BIN} found; install it manually \
                             (e.g. `cargo install --path crates/xcode-dap`) or point the \
                             `dap.Xcode.binary` setting at an existing binary"
                        )),
                    };
                }
            }
        }

        Ok((Self::absolutize(&bin_rel), source))
    }

    /// Remove every cached version dir except the freshly-downloaded one,
    /// leaving a single version in the work dir. Best-effort and run only
    /// after a successful download, so a failed fetch can't wipe the cache.
    fn purge_stale_versions(keep: &str) {
        let keep = Path::new(keep);
        let Ok(entries) = fs::read_dir(PROXY_BIN) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path != keep {
                fs::remove_dir_all(&path).ok();
            }
        }
    }

    /// Lexicographically last cached `xcode-dap/<tag>/xcode-dap` under the
    /// work dir, if any. In practice at most one version exists — each
    /// successful download purges the others — so the ordering never has to
    /// disambiguate semantic versions.
    fn newest_cached_proxy() -> Option<String> {
        let mut candidates: Vec<String> = fs::read_dir(PROXY_BIN)
            .ok()?
            .flatten()
            .filter_map(|entry| {
                let bin = entry.path().join(PROXY_BIN);
                bin.is_file().then(|| bin.to_string_lossy().into_owned())
            })
            .collect();
        candidates.sort();
        candidates.pop()
    }

    /// `DebugAdapterBinary.command` is used verbatim by Zed (no work-dir
    /// resolution) — turn a work-dir-relative path into an absolute one.
    fn absolutize(relative: &str) -> String {
        std::env::current_dir()
            .map(|cwd| cwd.join(relative).to_string_lossy().into_owned())
            .unwrap_or_else(|_| relative.to_string())
    }
}

impl zed::Extension for XcodeToolsExtension {
    fn new() -> Self {
        XcodeToolsExtension
    }

    fn get_dap_binary(
        &mut self,
        adapter_name: String,
        config: DebugTaskDefinition,
        user_provided_debug_adapter_path: Option<String>,
        worktree: &Worktree,
    ) -> Result<DebugAdapterBinary, String> {
        if adapter_name != "Xcode" {
            return Err(format!("unexpected debug adapter name: {adapter_name}"));
        }

        // Sanity check only — full validation belongs to the proxy, which
        // parses the very same shared struct (crates/xcode-dap-config).
        serde_json::from_str::<xcode_dap_config::LaunchConfig>(&config.config).map_err(|err| {
            format!(
                "invalid Xcode scenario config: {err}. See the Xcode.json schema \
                 (required: \"workspace\", \"scheme\"; e.g. \
                 {{\"workspace\": \"$ZED_WORKTREE_ROOT/App.xcworkspace\", \"scheme\": \"App\", \
                 \"device\": \"iPhone 15 Pro Max\"}})"
            )
        })?;

        let (command, source) =
            self.resolve_proxy_command(user_provided_debug_adapter_path, worktree)?;
        // Visible under `zed --foreground`.
        println!("xcode-tools: xcode-dap resolved from {source}: {command}");

        // User's shell env → DEVELOPER_DIR, PATH for xcrun/tuist/make.
        let mut envs = worktree.shell_env();
        // Surfaced in the proxy's log session header (xcode-dap.log).
        envs.push((
            "XCODE_DAP_RESOLVED_FROM".to_string(),
            format!("{source}:{command} ext-v{}", env!("CARGO_PKG_VERSION")),
        ));

        Ok(DebugAdapterBinary {
            // Absolute path; Zed passes it through verbatim.
            command: Some(command),
            // The proxy speaks DAP on stdio; no arguments needed.
            arguments: vec![],
            envs,
            // The proxy runs preflight/xcodebuild from the project root.
            cwd: Some(worktree.root_path()),
            // None => stdio transport, like lldb-dap itself.
            connection: None,
            request_args: StartDebuggingRequestArguments {
                // Scenario config passes through verbatim: Zed sends it back
                // as the `arguments` of the DAP `launch` request.
                configuration: config.config,
                request: StartDebuggingRequestArgumentsRequest::Launch,
            },
        })
    }

    fn dap_request_kind(
        &mut self,
        _adapter_name: String,
        config: serde_json::Value,
    ) -> Result<StartDebuggingRequestArgumentsRequest, String> {
        match config.get("request") {
            None => Ok(StartDebuggingRequestArgumentsRequest::Launch),
            Some(serde_json::Value::String(request)) if request == "launch" => {
                Ok(StartDebuggingRequestArgumentsRequest::Launch)
            }
            Some(serde_json::Value::String(request)) if request == "attach" => Err(
                "the Xcode adapter does not support attaching to an existing process yet"
                    .to_string(),
            ),
            Some(other) => Err(format!(
                "unsupported \"request\" value in Xcode scenario config: {other}"
            )),
        }
    }

    fn dap_config_to_scenario(&mut self, config: DebugConfig) -> Result<DebugScenario, String> {
        match config.request {
            // Generic New Session "Launch" tab (adapter-agnostic). Documented
            // compromise: `program` maps to the Xcode scheme. The required
            // `workspace` cannot be derived here — save the scenario to
            // .zed/debug.json and fill it in (the schema flags the omission).
            DebugRequest::Launch(launch) => {
                let mut scenario_config = serde_json::Map::new();
                scenario_config.insert(
                    "scheme".to_string(),
                    serde_json::Value::String(launch.program),
                );
                // `stopOnEntry` round-trips the Launch tab's toggle into the
                // saved scenario, but the proxy currently ignores it:
                // `LaunchConfig` has no such field and parses without
                // `deny_unknown_fields`, so the key is accepted and dropped.
                if let Some(stop_on_entry) = config.stop_on_entry {
                    scenario_config.insert(
                        "stopOnEntry".to_string(),
                        serde_json::Value::Bool(stop_on_entry),
                    );
                }
                Ok(DebugScenario {
                    label: config.label,
                    adapter: config.adapter,
                    build: None,
                    config: serde_json::Value::Object(scenario_config).to_string(),
                    tcp_connection: None,
                })
            }
            DebugRequest::Attach(_) => Err(
                "the Xcode adapter does not support attaching to an existing process; \
                 use a launch scenario from .zed/debug.json"
                    .to_string(),
            ),
        }
    }
}

zed::register_extension!(XcodeToolsExtension);
