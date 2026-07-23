//! Shared serde structs for the `Xcode` debug-adapter scenario config JSON.
//!
//! Single source of truth consumed by both the WASM extension
//! (`extension/`) and the native proxy (`crates/xcode-dap`), so the
//! schema, extension parsing and proxy parsing never drift.
//! See `docs/design/dap-proxy.md` §4.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Flattened scenario `config` from Zed — the `arguments` of the DAP
/// `launch` request intercepted by the proxy.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchConfig {
    /// Path to `.xcworkspace` (or `.xcodeproj`), e.g. `MyApp.xcworkspace`.
    pub workspace: PathBuf,
    /// Xcode scheme to build and run, e.g. `"MyApp (staging)"`.
    pub scheme: String,
    /// Simulator device name (e.g. `"iPhone 15 Pro Max"`) or UDID.
    /// `None` = prefer the booted iPhone simulator, else the newest
    /// available iPhone (deterministic).
    pub device: Option<String>,
    /// Optional simulator OS version narrowing, e.g. `"26.3"`.
    pub os: Option<String>,
    /// Build configuration (`Debug`/`Release`); `None` = scheme default.
    pub configuration: Option<String>,
    /// Project-generation preflight when the workspace is missing,
    /// e.g. `"make project CI=true"` (written by `xcode-dap setup` when a
    /// Makefile `project:` target is detected).
    pub preflight: Option<String>,
    /// Pump `log stream` (OSLog) output into the Debug Console.
    #[serde(default)]
    pub oslog: bool,
    /// Custom NSPredicate for the OSLog pump (`log stream --predicate`).
    /// `None` = default predicate scoped to the app's own logging
    /// (subsystem == bundle id, or any image inside the .app bundle).
    pub oslog_predicate: Option<String>,
    /// `simctl terminate` the app when the debug session stops.
    #[serde(default = "default_true")]
    pub terminate_on_stop: bool,
    /// Build-log verbosity in the Debug Console.
    #[serde(default)]
    pub build_output: BuildOutput,
    /// Log this session at `trace` verbosity to
    /// `~/.zedxcode/logs/xcode-dap.log` (never lowers a level already
    /// raised via the `XCODE_DAP_LOG` environment variable).
    #[serde(default)]
    pub verbose_logging: bool,
    /// Explicit DerivedData directory, mapped to `xcodebuild
    /// -derivedDataPath`. `None` = xcodebuild's default per-workspace
    /// DerivedData location.
    pub derived_data: Option<PathBuf>,
}

/// Build-log verbosity (`"buildOutput"` in scenario JSON).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BuildOutput {
    /// Phase headers, diagnostics and the final verdict only (default).
    #[default]
    Filtered,
    /// Full xcodebuild output.
    Full,
}

fn default_true() -> bool {
    true
}
