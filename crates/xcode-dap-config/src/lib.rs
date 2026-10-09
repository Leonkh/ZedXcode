//! Shared serde structs for the `Xcode` debug-adapter scenario config JSON.
//!
//! Single source of truth consumed by both the WASM extension
//! (`extension/`) and the native proxy (`crates/xcode-dap`), so the
//! schema, extension parsing and proxy parsing never drift.
//! See `docs/design/dap-proxy.md` §4.

use serde::de::value::MapDeserializer;
use serde::de::{Error, IntoDeserializer};
use serde::{Deserialize, Serialize};
use std::fmt::Display;
use std::path::PathBuf;

/// Flattened scenario `config` from Zed — the `arguments` of the DAP
/// `launch` request intercepted by the proxy.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchConfig {
    /// Path to `.xcworkspace` (or `.xcodeproj`), e.g. `MyApp.xcworkspace`.
    /// `None` = the container found in the project (two levels deep).
    pub workspace: Option<PathBuf>,
    /// Legacy: the Xcode scheme, e.g. `"MyApp (staging)"`. Read only when
    /// no scheme is chosen in the project's selection store; `None` = the
    /// only scheme of the container.
    pub scheme: Option<String>,
    /// Legacy: simulator device name (e.g. `"iPhone 15 Pro Max"`) or UDID,
    /// read only when no destination is chosen in the selection store.
    /// `None` = the booted iPhone simulator, else the newest iPhone on the
    /// newest iOS runtime.
    pub device: Option<String>,
    /// Legacy: simulator OS version narrowing `device`, e.g. `"26.3"`.
    pub os: Option<String>,
    /// Legacy: build configuration (`Debug`/`Release`), read only when none
    /// is chosen in the selection store; `None` = scheme default.
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

/// The first key of a scenario `config` object whose value alone does not
/// deserialize into [`LaunchConfig`], with that error: the key to name when
/// the whole object fails to parse. `None` when every key parses on its own
/// (unknown keys always do: [`LaunchConfig`] ignores them).
///
/// Every field is optional, so a single key is a valid config by itself, and
/// checking the keys one by one pins the failure on the key that causes it.
/// Generic over the value type so the extension and the binary can both pass
/// `serde_json` values without this crate depending on `serde_json`.
pub fn invalid_key<'de, K, V, E>(entries: impl IntoIterator<Item = (K, V)>) -> Option<(K, E)>
where
    K: IntoDeserializer<'de, E> + Clone,
    V: IntoDeserializer<'de, E>,
    E: Error,
{
    entries.into_iter().find_map(|(key, value)| {
        let one = MapDeserializer::<_, E>::new(std::iter::once((key.clone(), value)));
        LaunchConfig::deserialize(one).err().map(|e| (key, e))
    })
}

/// The extension's error for a scenario `config` that does not parse, with
/// xcode-dap installed at `command`: it names the key at fault when
/// [`invalid_key`] found one (`invalid`), else gives the whole object's
/// `error`, and says where Scheme, Destination and Configuration are chosen.
pub fn invalid_scenario_message<K: Display, E: Display>(
    command: &str,
    invalid: Option<(K, E)>,
    error: &dyn Display,
) -> String {
    let what = match invalid {
        Some((key, key_error)) => {
            format!("the key \"{key}\" in this scenario is invalid: {key_error}")
        }
        None => format!("this scenario is invalid: {error}"),
    };
    format!(
        "Xcode Tools installed xcode-dap at {command}, but {what}. Fix it in .zed/debug.json \
         (hover shows its docs). Scheme, Destination and Configuration are not set here: use \
         the Xcode: Choose Scheme / Choose Destination / Choose Configuration tasks (⇧⌘R) or \
         Xcode: Set Up Project."
    )
}
