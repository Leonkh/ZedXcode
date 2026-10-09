//! The project's Scheme, Destination and Configuration: the selection store
//! `<root>/.zed/.zedx/selection.json` and [`resolve`], the one answer to
//! which values a build, run or clean uses and where each came from.
//!
//! The store is machine-written by the pickers (version 2):
//!
//! ```json
//! { "version": 2,
//!   "scheme": "MyApp",
//!   "destination": { "kind": "simulator", "udid": "…", "name": "iPhone 17", "os": "26.0" },
//!   "configuration": "Release",
//!   "recent": { "schemes": ["MyApp", "MyApp Dev"], "destinations": [ … ] } }
//! ```
//!
//! An absent key means nothing is chosen there. `recent` keeps five entries
//! each. `kind: "device"` is reserved for physical iPhones; any kind but
//! `simulator` counts as not chosen, with a warning. A version 1 file
//! (`{scheme, device, os}`) reads as version 2 and is rewritten only by the
//! next pick. A write takes an advisory lock on `selection.json.lock`,
//! re-reads the file under it and changes only the keys it sets, so writers
//! running at once keep each other's choices and every key they do not know,
//! down to the fields and entries of `recent` this binary cannot read.
//!
//! Resolution, per value, first match wins:
//! 1. the flags of this invocation (never saved);
//! 2. the store;
//! 3. for a linked git worktree, the main checkout's store (read through,
//!    nothing is copied). This is per value too: a value the worktree's own
//!    store leaves unset still comes from the main checkout's, so one pick
//!    in a worktree does not drop the other two inherited values;
//! 4. the deprecated scenario keys `scheme`, `device` + `os` and
//!    `configuration` (⌘R: the launched scenario; the CLI: the project's
//!    first Xcode scenario);
//! 5. automatic: the container's only scheme, the booted iPhone else the
//!    newest one, and no configuration (xcodebuild then uses the scheme's).
//!
//! A destination that gives only an OS narrows the device the next layers
//! name, as 0.1's overlay narrowed the scenario's device; with none, the
//! automatic rule runs within that OS.
//!
//! Layers 1 to 4 need only files ([`pick`]); automatic values and the
//! validation of all three need the container and the simulators, after any
//! generating preflight ([`settle_scheme`], [`settle_configuration`],
//! [`settle_destination`]).
//!
//! One exception keeps 0.1 projects working as they did: when the command
//! line is one of the tasks 0.1's setup wrote into `.zed/tasks.json`, its
//! baked flags rank below the stores, as 0.1's overlay ranked them below the
//! store ([`FlagRank::BelowStore`]), and one line says how to migrate.

use std::fs::{self, File};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::engine::config::{invalid_config_reason, BuildOutput, LaunchConfig};
use crate::engine::destinations::{self, Device, Inventory, Query};
use crate::engine::project::{self, Project};
use crate::engine::schemes::SchemeList;
use crate::setup::jsonc;
use crate::util::paths::expand_worktree_root;

/// The store format this binary writes.
pub const VERSION: u64 = 2;

/// How many recent schemes and destinations the store keeps.
pub const RECENT_LIMIT: usize = 5;

/// The destination kind this binary runs.
pub const KIND_SIMULATOR: &str = "simulator";

/// Reserved for physical iPhones.
const KIND_DEVICE: &str = "device";

/// The store's path, relative to the project root, for messages.
const STORE_LABEL: &str = ".zed/.zedx/selection.json";

/// `<root>/.zed/.zedx/selection.json`.
pub fn store_path(root: &Path) -> PathBuf {
    root.join(".zed").join(".zedx").join("selection.json")
}

// ---------------------------------------------------------------------------
// the store
// ---------------------------------------------------------------------------

/// A destination as the store keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredDestination {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub udid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    /// Fields a later writer added (a physical device's, say), kept as they
    /// are when the entry is written back.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl StoredDestination {
    /// A picked simulator: UDID, name and OS, so it survives being deleted
    /// and made again under the same name.
    pub fn simulator(device: &Device) -> StoredDestination {
        StoredDestination {
            kind: KIND_SIMULATOR.to_owned(),
            udid: Some(device.udid.clone()),
            name: Some(device.name.clone()),
            os: Some(device.os_version()),
            extra: Map::new(),
        }
    }

    /// The same simulator: equal UDIDs, or without UDIDs, equal name and OS.
    fn same_as(&self, other: &StoredDestination) -> bool {
        match (&self.udid, &other.udid) {
            (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
            _ => self.kind == other.kind && self.name == other.name && self.os == other.os,
        }
    }

    /// The query this destination resolves with, or why it counts as not
    /// chosen.
    fn query(&self, from: &str) -> std::result::Result<Option<Query>, String> {
        if self.kind == KIND_DEVICE {
            return Err(format!(
                "Ignoring the destination in {from}: physical devices (\"kind\": \"device\") \
                 are not supported by this xcode-dap."
            ));
        }
        if self.kind != KIND_SIMULATOR {
            return Err(format!(
                "Ignoring the destination in {from}: the kind \"{}\" is unknown to this \
                 xcode-dap.",
                self.kind
            ));
        }
        if self.udid.is_none() && self.name.is_none() && self.os.is_none() {
            return Ok(None);
        }
        Ok(Some(Query {
            udid: self.udid.clone(),
            name: self.name.clone(),
            os: self.os.clone(),
            legacy: false,
        }))
    }
}

/// What the store holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Store {
    pub scheme: Option<String>,
    pub destination: Option<StoredDestination>,
    pub configuration: Option<String>,
    /// Most recent first.
    pub recent_schemes: Vec<String>,
    /// Most recent first.
    pub recent_destinations: Vec<StoredDestination>,
}

impl Store {
    /// Choose `scheme` and put it first among the recent schemes.
    pub fn choose_scheme(&mut self, scheme: &str) {
        self.scheme = Some(scheme.to_owned());
        self.recent_schemes.retain(|s| s != scheme);
        self.recent_schemes.insert(0, scheme.to_owned());
        self.recent_schemes.truncate(RECENT_LIMIT);
    }

    /// Choose `destination` and put it first among the recent destinations.
    pub fn choose_destination(&mut self, destination: StoredDestination) {
        self.recent_destinations
            .retain(|d| !d.same_as(&destination));
        self.recent_destinations.insert(0, destination.clone());
        self.recent_destinations.truncate(RECENT_LIMIT);
        self.destination = Some(destination);
    }
}

/// One read of a store file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Loaded {
    pub store: Store,
    /// What was wrong with the file (it is never an error: a broken store
    /// must not break a build).
    pub warnings: Vec<String>,
}

/// Read the store of the project at `root`; a missing file is an empty store.
pub fn load(root: &Path) -> Loaded {
    load_file(&store_path(root), STORE_LABEL)
}

fn load_file(path: &Path, label: &str) -> Loaded {
    match read_raw(path, label) {
        Ok(Some(raw)) => {
            let (store, warnings) = parse(&raw, label);
            Loaded { store, warnings }
        }
        Ok(None) => Loaded::default(),
        Err(warning) => Loaded {
            store: Store::default(),
            warnings: vec![warning],
        },
    }
}

/// The file's top-level object; `Ok(None)` when there is no file, `Err`
/// (the warning) when it is not a JSON object.
fn read_raw(path: &Path, label: &str) -> std::result::Result<Option<Map<String, Value>>, String> {
    let Ok(text) = fs::read_to_string(path) else {
        return Ok(None);
    };
    match jsonc::parse_jsonc(&text) {
        Ok(Value::Object(raw)) => Ok(Some(raw)),
        Ok(_) => Err(format!(
            "Ignoring {label}: it is not a JSON object. The next pick rewrites it."
        )),
        Err(e) => Err(format!(
            "Ignoring {label} ({e:#}). The next pick rewrites it."
        )),
    }
}

/// The store a file's object holds, and the warnings about values it could
/// not read (those count as not chosen).
fn parse(raw: &Map<String, Value>, label: &str) -> (Store, Vec<String>) {
    let mut warnings = Vec::new();
    let mut string = |key: &str| -> Option<String> {
        match raw.get(key) {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => {
                warnings.push(format!(
                    "Ignoring \"{key}\" in {label}: it is not a string."
                ));
                None
            }
        }
    };
    let scheme = string("scheme");
    let configuration = string("configuration");
    let destination = if raw.contains_key("version") {
        match raw.get("destination") {
            None | Some(Value::Null) => None,
            Some(v) => match serde_json::from_value::<StoredDestination>(v.clone()) {
                Ok(d) => Some(d),
                Err(e) => {
                    warnings.push(format!("Ignoring \"destination\" in {label}: {e}."));
                    None
                }
            },
        }
    } else {
        // Version 1: `device` (a name, or a UDID when the name was
        // ambiguous) and `os`.
        let device = string("device");
        let os = string("os");
        (device.is_some() || os.is_some()).then(|| {
            let (udid, name) = match device {
                Some(d) if looks_like_udid(&d) => (Some(d), None),
                d => (None, d),
            };
            StoredDestination {
                kind: KIND_SIMULATOR.to_owned(),
                udid,
                name,
                os,
                extra: Map::new(),
            }
        })
    };
    let recent = raw.get("recent").and_then(Value::as_object);
    let list = |key: &str| {
        recent
            .and_then(|r| r.get(key))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let recent_schemes = list("schemes")
        .into_iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    let recent_destinations = list("destinations")
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect();
    let store = Store {
        scheme,
        destination,
        configuration,
        recent_schemes,
        recent_destinations,
    };
    (store, warnings)
}

/// `AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE`: 8-4-4-4-12 hex digits.
fn looks_like_udid(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(g, len)| g.len() == len && g.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Change the store of the project at `root` and return the file's path.
///
/// Under the lock the file is read again, `change` runs on what it holds
/// now, and only the values `change` changed are written back; every other
/// key, the ones this binary does not know included, stays as it is. A
/// version 1 file is rewritten as version 2 here, its values kept. The write
/// is a temporary file renamed over the store.
pub fn update(root: &Path, change: impl FnOnce(&mut Store)) -> Result<PathBuf> {
    let path = store_path(root);
    let dir = path.parent().expect("the store path has a parent");
    fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    // Held until the function returns; `None` proceeds unlocked.
    let _lock = lock(&path);
    let mut raw = read_raw(&path, STORE_LABEL)
        .ok()
        .flatten()
        .unwrap_or_default();
    let (before, _) = parse(&raw, STORE_LABEL);
    let mut after = before.clone();
    change(&mut after);
    merge(&mut raw, &before, &after);
    let mut text =
        serde_json::to_string_pretty(&Value::Object(raw)).expect("a JSON object always serializes");
    text.push('\n');
    jsonc::atomic_write(&path, &text)?;
    Ok(path)
}

/// Write into `raw` the values that differ between `before` and `after`.
fn merge(raw: &mut Map<String, Value>, before: &Store, after: &Store) {
    if !raw.contains_key("version") {
        // Version 1 keeps its destination in `device` and `os`.
        raw.remove("device");
        raw.remove("os");
        if let Some(destination) = &before.destination {
            raw.insert("destination".into(), to_json(destination));
        }
    }
    if raw
        .get("version")
        .and_then(Value::as_u64)
        .is_none_or(|v| v < VERSION)
    {
        raw.insert("version".into(), VERSION.into());
    }
    let mut set = |key: &str, value: Option<Value>| match value {
        Some(v) => {
            raw.insert(key.into(), v);
        }
        None => {
            raw.remove(key);
        }
    };
    if after.scheme != before.scheme {
        set("scheme", after.scheme.clone().map(Value::from));
    }
    if after.destination != before.destination {
        set("destination", after.destination.as_ref().map(to_json));
    }
    if after.configuration != before.configuration {
        set(
            "configuration",
            after.configuration.clone().map(Value::from),
        );
    }
    let schemes_changed = after.recent_schemes != before.recent_schemes;
    let destinations_changed = after.recent_destinations != before.recent_destinations;
    if schemes_changed || destinations_changed {
        let mut recent = raw
            .get("recent")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        // Entries this binary cannot read (a later writer's) stay, after
        // its own.
        let unreadable = |key: &str, readable: fn(&Value) -> bool| -> Vec<Value> {
            recent
                .get(key)
                .and_then(Value::as_array)
                .map(|a| a.iter().filter(|v| !readable(v)).cloned().collect())
                .unwrap_or_default()
        };
        let other_schemes = unreadable("schemes", Value::is_string);
        let other_destinations = unreadable("destinations", |v| {
            StoredDestination::deserialize(v).is_ok()
        });
        if schemes_changed {
            let mut list: Vec<Value> = after.recent_schemes.iter().map(to_json).collect();
            list.extend(other_schemes);
            recent.insert("schemes".into(), Value::Array(list));
        }
        if destinations_changed {
            let mut list: Vec<Value> = after.recent_destinations.iter().map(to_json).collect();
            list.extend(other_destinations);
            recent.insert("destinations".into(), Value::Array(list));
        }
        raw.insert("recent".into(), Value::Object(recent));
    }
}

fn to_json(value: &impl Serialize) -> Value {
    serde_json::to_value(value).expect("store values always serialize")
}

/// `flock(LOCK_EX)` on `<store>.lock`, released when the returned file
/// closes. `None` on any failure, and the caller proceeds unlocked, as the
/// compile store and the logger do.
fn lock(path: &Path) -> Option<File> {
    use std::os::fd::AsRawFd;
    let mut lock_path = path.as_os_str().to_os_string();
    lock_path.push(".lock");
    let file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(PathBuf::from(lock_path))
        .ok()?;
    // SAFETY: `file` is a valid open descriptor for the duration of the call.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return None;
    }
    Some(file)
}

// ---------------------------------------------------------------------------
// layers
// ---------------------------------------------------------------------------

/// Where a resolved value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// A flag of this invocation.
    Flag,
    /// The project's store.
    Store,
    /// The main checkout's store, for a linked worktree.
    MainCheckout,
    /// A deprecated scenario key.
    Scenario,
    /// No layer chose it.
    Automatic,
}

/// The three values a layer can set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Scheme,
    Destination,
    Configuration,
}

impl Source {
    /// Where the value came from, for "(from …)" in messages.
    pub fn label(self, field: Field) -> &'static str {
        match (self, field) {
            (Source::Flag, Field::Scheme) => "--scheme",
            // 0.1's tasks pass `--device`, the other spelling of the flag.
            (Source::Flag, Field::Destination) => "--destination/--device",
            (Source::Flag, Field::Configuration) => "--configuration",
            (Source::Store, _) => STORE_LABEL,
            (Source::MainCheckout, _) => "the main checkout's .zed/.zedx/selection.json",
            (Source::Scenario, _) => "the Xcode scenario",
            (Source::Automatic, _) => "automatic",
        }
    }

    /// The short form for the one-line summary.
    fn short(self) -> &'static str {
        match self {
            Source::Flag => "flag",
            Source::Store => "selection.json",
            Source::MainCheckout => "main checkout's selection.json",
            Source::Scenario => "scenario",
            Source::Automatic => "automatic",
        }
    }
}

/// A value and its source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sourced<T> {
    pub value: T,
    pub source: Source,
}

/// The values one layer sets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Layer {
    pub scheme: Option<String>,
    pub destination: Option<Query>,
    pub configuration: Option<String>,
}

impl Layer {
    /// The 0.1 shape: flags or scenario keys, with `device` + `os` for the
    /// destination.
    pub fn legacy(
        scheme: Option<&str>,
        device: Option<&str>,
        os: Option<&str>,
        configuration: Option<&str>,
    ) -> Layer {
        Layer {
            scheme: scheme.map(str::to_owned),
            destination: Query::legacy(device, os),
            configuration: configuration.map(str::to_owned),
        }
    }

    /// The deprecated keys of a scenario.
    pub fn from_scenario(cfg: &LaunchConfig) -> Layer {
        Layer::legacy(
            cfg.scheme.as_deref(),
            cfg.device.as_deref(),
            cfg.os.as_deref(),
            cfg.configuration.as_deref(),
        )
    }

    fn from_store(store: &Store, from: &str, warnings: &mut Vec<String>) -> Layer {
        let destination = match store.destination.as_ref().map(|d| d.query(from)) {
            Some(Ok(query)) => query,
            Some(Err(warning)) => {
                warnings.push(warning);
                None
            }
            None => None,
        };
        Layer {
            scheme: store.scheme.clone(),
            destination,
            configuration: store.configuration.clone(),
        }
    }
}

/// Where the flags rank.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FlagRank {
    /// Above everything: a one-off override.
    #[default]
    First,
    /// For a 0.1 task's baked flags: below the project's store, as the 0.1
    /// overlay ranked them, and below the main checkout's store too, so that
    /// in a linked worktree ⌘B with 0.1's baked flags builds what ⌘R builds
    /// from the inherited store.
    BelowStore,
}

/// Layers 1 to 4, per value; `None` means automatic.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Picks {
    pub scheme: Option<Sourced<String>>,
    pub destination: Option<Sourced<Query>>,
    pub configuration: Option<Sourced<String>>,
    /// What the user should read about the stores and the scenario.
    pub warnings: Vec<String>,
}

/// Take each value from the first layer that sets it: the flags (first, or
/// after both stores with [`FlagRank::BelowStore`]), the store at `root`,
/// the store at `main_checkout` (a linked worktree's main checkout; skipped
/// when it is `root` itself), the scenario.
pub fn pick(
    root: &Path,
    main_checkout: Option<&Path>,
    flags: &Layer,
    rank: FlagRank,
    scenario: &Layer,
) -> Picks {
    let mut warnings = Vec::new();
    let own = load(root);
    warnings.extend(own.warnings);
    let own = Layer::from_store(&own.store, STORE_LABEL, &mut warnings);
    let main = match main_checkout.filter(|m| *m != root) {
        Some(main) => {
            let label = Source::MainCheckout.label(Field::Scheme);
            let loaded = load_file(&store_path(main), label);
            warnings.extend(loaded.warnings);
            Layer::from_store(&loaded.store, label, &mut warnings)
        }
        None => Layer::default(),
    };
    let mut layers = vec![(Source::Store, &own), (Source::MainCheckout, &main)];
    let flags_at = match rank {
        FlagRank::First => 0,
        FlagRank::BelowStore => 2,
    };
    layers.insert(flags_at, (Source::Flag, flags));
    layers.push((Source::Scenario, scenario));

    fn first<T: Clone>(
        layers: &[(Source, &Layer)],
        get: impl Fn(&Layer) -> Option<&T>,
    ) -> Option<Sourced<T>> {
        layers.iter().find_map(|(source, layer)| {
            get(layer).map(|value| Sourced {
                value: value.clone(),
                source: *source,
            })
        })
    }
    Picks {
        scheme: first(&layers, |l| l.scheme.as_ref()),
        destination: first_destination(&layers),
        configuration: first(&layers, |l| l.configuration.as_ref()),
        warnings,
    }
}

/// The first layer's destination. One that gives only an OS takes the
/// device of the next layer that names one (by name when it has one: the
/// UDID of a simulator on another OS would not match), keeping its own OS
/// and source; with none, it stays an OS alone (the automatic rule within
/// that OS).
fn first_destination(layers: &[(Source, &Layer)]) -> Option<Sourced<Query>> {
    let mut set = layers
        .iter()
        .filter_map(|(source, layer)| layer.destination.as_ref().map(|d| (*source, d)));
    let (source, query) = set.next()?;
    let mut value = query.clone();
    if value.udid.is_none() && value.name.is_none() {
        if let Some((_, named)) = set.find(|(_, d)| d.udid.is_some() || d.name.is_some()) {
            value.udid = named.name.is_none().then(|| named.udid.clone()).flatten();
            value.name = named.name.clone();
            value.legacy = named.legacy;
        }
    }
    Some(Sourced { value, source })
}

// ---------------------------------------------------------------------------
// requests
// ---------------------------------------------------------------------------

/// The build options, which are not part of the selection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// Run when the container is missing (only a launched scenario carries
    /// one: ⌘B and the CLI never run a preflight).
    pub preflight: Option<String>,
    pub derived_data: Option<PathBuf>,
    pub build_output: BuildOutput,
    pub oslog: bool,
    pub oslog_predicate: Option<String>,
}

/// What one build, run or clean is asked for, before resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// The project root: where the store is and the container search starts.
    pub root: PathBuf,
    /// A container named by the scenario or `--workspace`, absolute or
    /// relative to `root`; `None` = discover it.
    pub workspace: Option<PathBuf>,
    /// Layer 1.
    pub flags: Layer,
    pub flag_rank: FlagRank,
    /// Layer 4.
    pub scenario: Layer,
    pub options: Options,
    /// What the user should read about the request itself.
    pub warnings: Vec<String>,
}

/// The command-line flags a build, run or clean takes.
#[derive(Debug, Clone, Default)]
pub struct CliFlags {
    /// Absolute.
    pub workspace: Option<PathBuf>,
    pub scheme: Option<String>,
    /// `--destination` (0.1's `--device`): a UDID, a simulator name, or
    /// `booted`.
    pub destination: Option<String>,
    pub os: Option<String>,
    pub configuration: Option<String>,
    pub derived_data: Option<PathBuf>,
    pub full_output: bool,
    pub oslog: bool,
    pub oslog_predicate: Option<String>,
    /// The label of the 0.1 task this command line is (see
    /// [`crate::setup::project::legacy_task_invocation`]): its flags then
    /// rank below the stores.
    pub legacy_task: Option<&'static str>,
}

/// The one line a 0.1 task prints: why the stores outrank its flags, and
/// how to leave the 0.1 setup behind.
pub fn legacy_task_line(label: &str) -> String {
    format!(
        "\"{label}\" is a task of the 0.1 setup (.zed/tasks.json): the scheme, destination \
         and configuration chosen for this project outrank its flags. Run Xcode: Set Up \
         Project (or \"xcode-dap setup\") once to migrate."
    )
}

impl Request {
    /// ⌘R: the launched scenario's config, at the adapter's root.
    pub fn for_launch(cfg: &LaunchConfig, root: PathBuf) -> Request {
        Request {
            root,
            workspace: cfg.workspace.clone(),
            flags: Layer::default(),
            flag_rank: FlagRank::First,
            scenario: Layer::from_scenario(cfg),
            options: Options {
                preflight: cfg.preflight.clone(),
                derived_data: cfg.derived_data.clone(),
                build_output: cfg.build_output,
                oslog: cfg.oslog,
                oslog_predicate: cfg.oslog_predicate.clone(),
            },
            warnings: Vec::new(),
        }
    }

    /// A command typed in a terminal or run by a task: its flags (a one-off
    /// override, or for a 0.1 task below the stores), then the project's
    /// first Xcode scenario for the deprecated keys and for the options the
    /// flags leave open, then the defaults.
    pub fn for_cli(root: PathBuf, flags: CliFlags) -> Request {
        let mut warnings = Vec::new();
        let flag_rank = match flags.legacy_task {
            Some(label) => {
                warnings.push(legacy_task_line(label));
                FlagRank::BelowStore
            }
            None => FlagRank::First,
        };
        // The global `Xcode: Run` scenario, once setup writes one, is the
        // next place to look when the project has none.
        let scenario = first_xcode_scenario(&root).unwrap_or_else(|warning| {
            warnings.push(warning);
            None
        });
        let scenario = scenario.as_ref();
        let expand = |p: &Path| expand_worktree_root(&p.to_string_lossy(), &root);
        let build_output = if flags.full_output {
            BuildOutput::Full
        } else {
            scenario.map_or(BuildOutput::Filtered, |s| s.build_output)
        };
        Request {
            workspace: flags
                .workspace
                .or_else(|| scenario.and_then(|s| s.workspace.as_deref()).map(expand)),
            flags: Layer::legacy(
                flags.scheme.as_deref(),
                flags.destination.as_deref(),
                flags.os.as_deref(),
                flags.configuration.as_deref(),
            ),
            flag_rank,
            scenario: scenario.map(Layer::from_scenario).unwrap_or_default(),
            options: Options {
                preflight: None,
                derived_data: flags
                    .derived_data
                    .or_else(|| scenario.and_then(|s| s.derived_data.as_deref()).map(expand)),
                build_output,
                oslog: flags.oslog || scenario.is_some_and(|s| s.oslog),
                oslog_predicate: flags
                    .oslog_predicate
                    .or_else(|| scenario.and_then(|s| s.oslog_predicate.clone())),
            },
            root,
            warnings,
        }
    }
}

/// The first `"Xcode"` scenario of `<root>/.zed/debug.json`; `Ok(None)` when
/// there is none, `Err` (the warning) when the file or the scenario cannot
/// be read.
pub fn first_xcode_scenario(root: &Path) -> std::result::Result<Option<LaunchConfig>, String> {
    let path = root.join(".zed").join("debug.json");
    let Ok(text) = fs::read_to_string(&path) else {
        return Ok(None);
    };
    let parsed =
        jsonc::parse_jsonc(&text).map_err(|e| format!("Ignoring .zed/debug.json ({e:#})."))?;
    let Some(scenario) = parsed.as_array().and_then(|scenarios| {
        scenarios
            .iter()
            .find(|s| s.get("adapter").and_then(Value::as_str) == Some("Xcode"))
    }) else {
        return Ok(None);
    };
    serde_json::from_value(scenario.clone())
        .map(Some)
        .map_err(|e| {
            format!(
                "Ignoring the Xcode scenario in .zed/debug.json: {}.",
                invalid_config_reason(scenario, &e)
            )
        })
}

/// What [`resolve`] found: the project and layers 1 to 4.
#[derive(Debug, Clone)]
pub struct Resolution {
    pub project: Project,
    pub picks: Picks,
}

/// The project (the named container, else the one discovery finds) and the
/// values layers 1 to 4 choose. A tie between containers is an error that
/// lists them.
pub fn resolve(req: &Request) -> Result<Resolution> {
    let project = match &req.workspace {
        Some(workspace) => project::with_container(&req.root, workspace),
        None => project::discover(&req.root).map_err(|tie| anyhow!(tie))?,
    };
    let main = project.main_checkout_root();
    let mut picks = pick(
        &project.root,
        main.as_deref(),
        &req.flags,
        req.flag_rank,
        &req.scenario,
    );
    picks.warnings.splice(0..0, req.warnings.iter().cloned());
    Ok(Resolution { project, picks })
}

/// What a build at `root` would choose without flags: for the pickers'
/// current marks, `doctor` and `refresh`.
pub fn current(root: &Path) -> Picks {
    let mut warnings = Vec::new();
    let scenario = first_xcode_scenario(root).unwrap_or_else(|warning| {
        warnings.push(warning);
        None
    });
    let main = Project::without_search(root).main_checkout_root();
    let mut picks = pick(
        root,
        main.as_deref(),
        &Layer::default(),
        FlagRank::First,
        &scenario
            .as_ref()
            .map(Layer::from_scenario)
            .unwrap_or_default(),
    );
    picks.warnings.splice(0..0, warnings);
    picks
}

/// The container a CLI command works on: `--workspace` (absolute), else the
/// first Xcode scenario's `"workspace"`, else what discovery finds.
pub fn cli_container(root: &Path, flag: Option<&Path>) -> Result<PathBuf> {
    if let Some(workspace) = flag {
        return Ok(std::path::absolute(workspace).unwrap_or_else(|_| workspace.to_path_buf()));
    }
    if let Ok(Some(workspace)) = first_xcode_scenario(root).map(|s| s.and_then(|s| s.workspace)) {
        return Ok(expand_worktree_root(&workspace.to_string_lossy(), root));
    }
    Ok(project::container_for_cli(root)?.path)
}

// ---------------------------------------------------------------------------
// validation and the automatic values
// ---------------------------------------------------------------------------

fn container_name(container: &Path) -> String {
    container
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| container.display().to_string())
}

/// `A, B, C`, or the first ten and `…`.
fn list_names(names: &[String]) -> String {
    const SHOWN: usize = 10;
    let mut text = names
        .iter()
        .take(SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if names.len() > SHOWN {
        text.push_str(", …");
    }
    text
}

/// The name in `names` that `wanted` means: the exact one, else the only
/// one equal to it ignoring case.
fn find_name<'a>(names: &'a [String], wanted: &str) -> Option<&'a String> {
    names.iter().find(|n| *n == wanted).or_else(|| {
        let mut equal = names.iter().filter(|n| n.eq_ignore_ascii_case(wanted));
        match (equal.next(), equal.next()) {
            (Some(only), None) => Some(only),
            _ => None,
        }
    })
}

/// The scheme to build: the picked one when the container has it (spelled
/// as the container spells it), else the container's only scheme.
pub fn settle_scheme(
    pick: Option<&Sourced<String>>,
    list: &SchemeList,
    container: &Path,
) -> Result<Sourced<String>> {
    let name = container_name(container);
    match pick {
        Some(pick) => match find_name(&list.schemes, &pick.value) {
            Some(scheme) => Ok(Sourced {
                value: scheme.clone(),
                source: pick.source,
            }),
            None => bail!(
                "Scheme \"{}\" (from {}) is not in {name}. Choose another with Xcode: Choose \
                 Scheme; after regenerating the project, choosing again reloads the list.",
                pick.value,
                pick.source.label(Field::Scheme)
            ),
        },
        None => match list.schemes.as_slice() {
            [only] => Ok(Sourced {
                value: only.clone(),
                source: Source::Automatic,
            }),
            schemes => bail!(
                "No scheme chosen: {name} has {} schemes ({}). Run Xcode: Choose Scheme (⇧⌘R) \
                 or \"xcode-dap select-scheme\".",
                schemes.len(),
                list_names(schemes)
            ),
        },
    }
}

/// The configuration to build: the picked one when the container lists it
/// (a workspace lists none, so its pick is taken as it is); none when
/// nothing is picked, and xcodebuild uses the scheme's.
pub fn settle_configuration(
    pick: Option<&Sourced<String>>,
    list: &SchemeList,
    container: &Path,
) -> Result<Option<Sourced<String>>> {
    let Some(pick) = pick else {
        return Ok(None);
    };
    if list.configurations.is_empty() {
        return Ok(Some(pick.clone()));
    }
    match find_name(&list.configurations, &pick.value) {
        Some(configuration) => Ok(Some(Sourced {
            value: configuration.clone(),
            source: pick.source,
        })),
        None => bail!(
            "Configuration \"{}\" (from {}) is not in {} ({}). Choose another with Xcode: Choose \
             Configuration.",
            pick.value,
            pick.source.label(Field::Configuration),
            container_name(container),
            list_names(&list.configurations)
        ),
    }
}

/// The simulator to run on, and the warnings about how it was found.
pub fn settle_destination(
    pick: Option<&Sourced<Query>>,
    inventory: &Inventory,
) -> Result<(Sourced<Device>, Vec<String>)> {
    let from = pick.map_or("automatic", |p| p.source.label(Field::Destination));
    let resolved = destinations::resolve(pick.map(|p| &p.value), inventory, from)?;
    Ok((
        Sourced {
            value: resolved.device,
            source: pick.map_or(Source::Automatic, |p| p.source),
        },
        resolved.warnings,
    ))
}

/// `Scheme: MyApp (selection.json) | Destination: iPhone 17 · iOS 26.0
/// (automatic) | Configuration: scheme default`.
pub fn summary(
    scheme: &Sourced<String>,
    destination: Option<&Sourced<Device>>,
    configuration: Option<&Sourced<String>>,
) -> String {
    let mut line = format!("Scheme: {} ({})", scheme.value, scheme.source.short());
    if let Some(d) = destination {
        line.push_str(&format!(
            " | Destination: {} · iOS {} ({})",
            d.value.name,
            d.value.os_version(),
            d.source.short()
        ));
    }
    match configuration {
        Some(c) => line.push_str(&format!(
            " | Configuration: {} ({})",
            c.value,
            c.source.short()
        )),
        None => line.push_str(" | Configuration: scheme default"),
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::mpsc;
    use std::time::Duration;

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A fresh directory, canonical (macOS reaches the temp dir through a
    /// symlink), like the roots the resolver works with.
    fn sandbox() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zedxcode-selection-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args([
                "-c",
                "user.name=MyApp Dev",
                "-c",
                "user.email=dev@example.com",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .args(args)
            .current_dir(dir)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .env_remove("GIT_INDEX_FILE")
            .stdin(Stdio::null())
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    /// A main checkout and a linked worktree of it, each holding a project.
    fn checkout_and_worktree() -> (PathBuf, PathBuf) {
        let base = sandbox();
        let main = base.join("MyApp");
        fs::create_dir_all(main.join("MyApp.xcodeproj")).unwrap();
        fs::write(main.join("README.md"), "MyApp\n").unwrap();
        git(&main, &["init", "-q"]);
        git(&main, &["add", "README.md"]);
        git(&main, &["commit", "-q", "-m", "init"]);
        git(
            &main,
            &["worktree", "add", "-q", "--detach", "../MyApp-feature"],
        );
        let worktree = base.join("MyApp-feature");
        fs::create_dir_all(worktree.join("MyApp.xcodeproj")).unwrap();
        (main, worktree)
    }

    fn simulator(name: &str, os: &str) -> StoredDestination {
        StoredDestination {
            kind: KIND_SIMULATOR.into(),
            udid: None,
            name: Some(name.into()),
            os: Some(os.into()),
            extra: Map::new(),
        }
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
    }

    // --- the store -----------------------------------------------------------

    #[test]
    fn round_trip_and_recents() {
        let root = sandbox();
        assert_eq!(load(&root), Loaded::default());
        let path = update(&root, |s| {
            s.choose_scheme("MyApp");
            s.choose_destination(simulator("iPhone 16e", "18.6"));
            s.configuration = Some("Release".into());
        })
        .unwrap();
        assert_eq!(path, root.join(".zed/.zedx/selection.json"));
        assert_eq!(
            read_json(&path),
            json!({
                "version": 2,
                "scheme": "MyApp",
                "destination": { "kind": "simulator", "name": "iPhone 16e", "os": "18.6" },
                "configuration": "Release",
                "recent": {
                    "schemes": ["MyApp"],
                    "destinations": [{ "kind": "simulator", "name": "iPhone 16e", "os": "18.6" }]
                }
            })
        );
        let loaded = load(&root);
        assert!(loaded.warnings.is_empty());
        assert_eq!(loaded.store.scheme.as_deref(), Some("MyApp"));
        assert_eq!(loaded.store.configuration.as_deref(), Some("Release"));

        // Recents: most recent first, no duplicates, five at most.
        for scheme in ["A", "B", "C", "D", "E", "B"] {
            update(&root, |s| s.choose_scheme(scheme)).unwrap();
        }
        assert_eq!(load(&root).store.recent_schemes, ["B", "E", "D", "C", "A"]);
        update(&root, |s| {
            s.choose_destination(simulator("iPhone 17", "26.0"));
            s.choose_destination(simulator("iPhone 16e", "18.6"));
        })
        .unwrap();
        let store = load(&root).store;
        assert_eq!(store.recent_destinations.len(), 2);
        assert_eq!(store.destination, Some(simulator("iPhone 16e", "18.6")));
    }

    #[test]
    fn writers_keep_keys_they_do_not_own() {
        let root = sandbox();
        let path = store_path(&root);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"{"version": 2, "cards": {"dismissed": ["setup"]}, "scheme": "MyApp",
                "recent": {"schemes": ["MyApp"], "configurations": ["Debug"]},
                "destination": {"kind": "device", "udid": "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE"}}"#,
        )
        .unwrap();
        update(&root, |s| s.choose_scheme("MyApp Dev")).unwrap();
        let v = read_json(&path);
        assert_eq!(v["cards"], json!({ "dismissed": ["setup"] }));
        assert_eq!(v["scheme"], "MyApp Dev");
        assert_eq!(v["recent"]["schemes"], json!(["MyApp Dev", "MyApp"]));
        assert_eq!(v["recent"]["configurations"], json!(["Debug"]));
        // The destination this binary cannot run is left as it was.
        assert_eq!(v["destination"]["kind"], "device");
    }

    #[test]
    fn recent_entries_keep_fields_and_entries_this_binary_cannot_read() {
        let root = sandbox();
        let path = store_path(&root);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let phone = json!({ "kind": "device", "udid": "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE",
                            "name": "Jane's iPhone", "os": "26.1", "connection": "usb" });
        let odd = json!({ "name": "no kind" });
        fs::write(
            &path,
            json!({ "version": 2,
                    "recent": { "schemes": ["MyApp", { "pinned": "Widget" }],
                                "destinations": [phone.clone(), odd.clone()] } })
            .to_string(),
        )
        .unwrap();
        update(&root, |s| {
            s.choose_scheme("MyApp Dev");
            s.choose_destination(simulator("iPhone 16e", "18.6"));
        })
        .unwrap();
        let v = read_json(&path);
        assert_eq!(
            v["recent"]["schemes"],
            json!(["MyApp Dev", "MyApp", { "pinned": "Widget" }])
        );
        assert_eq!(
            v["recent"]["destinations"],
            json!([
                { "kind": "simulator", "name": "iPhone 16e", "os": "18.6" },
                phone,
                odd
            ])
        );
    }

    #[test]
    fn unknown_kinds_count_as_not_chosen_with_a_warning() {
        let root = sandbox();
        let path = store_path(&root);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        for (kind, warning) in [
            (
                "device",
                "Ignoring the destination in .zed/.zedx/selection.json: physical devices \
                 (\"kind\": \"device\") are not supported by this xcode-dap.",
            ),
            (
                "watch",
                "Ignoring the destination in .zed/.zedx/selection.json: the kind \"watch\" is \
                 unknown to this xcode-dap.",
            ),
        ] {
            fs::write(
                &path,
                json!({ "version": 2, "destination": { "kind": kind, "name": "Phone" } })
                    .to_string(),
            )
            .unwrap();
            let scenario = Layer::legacy(None, Some("iPhone 17"), None, None);
            let picks = pick(&root, None, &Layer::default(), FlagRank::First, &scenario);
            assert_eq!(picks.warnings, [warning]);
            // The destination falls through to the next layer.
            assert_eq!(picks.destination.unwrap().source, Source::Scenario);
            let picks = pick(
                &root,
                None,
                &Layer::default(),
                FlagRank::First,
                &Layer::default(),
            );
            assert_eq!(picks.destination, None);
        }
    }

    #[test]
    fn a_broken_store_is_ignored_with_a_warning_and_rewritten_by_the_next_pick() {
        let root = sandbox();
        let path = store_path(&root);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{nonsense").unwrap();
        let loaded = load(&root);
        assert_eq!(loaded.store, Store::default());
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].starts_with("Ignoring .zed/.zedx/selection.json ("));
        fs::write(&path, r#"{"version": 2, "scheme": 42}"#).unwrap();
        assert_eq!(
            load(&root).warnings,
            ["Ignoring \"scheme\" in .zed/.zedx/selection.json: it is not a string."]
        );
        update(&root, |s| s.choose_scheme("MyApp")).unwrap();
        assert_eq!(read_json(&path)["scheme"], "MyApp");
    }

    #[test]
    fn a_version_1_file_reads_as_version_2_and_only_a_pick_rewrites_it() {
        let root = sandbox();
        let path = store_path(&root);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let v1 = r#"{"scheme": "MyApp (staging)", "device": "iPhone 15 Pro Max", "os": "18.6"}"#;
        fs::write(&path, v1).unwrap();
        let store = load(&root).store;
        assert_eq!(store.scheme.as_deref(), Some("MyApp (staging)"));
        assert_eq!(
            store.destination,
            Some(simulator("iPhone 15 Pro Max", "18.6"))
        );
        // Resolving reads it and leaves it alone.
        let picks = pick(
            &root,
            None,
            &Layer::default(),
            FlagRank::First,
            &Layer::default(),
        );
        assert_eq!(picks.scheme.unwrap().value, "MyApp (staging)");
        assert_eq!(
            picks.destination.unwrap().value,
            Query {
                udid: None,
                name: Some("iPhone 15 Pro Max".into()),
                os: Some("18.6".into()),
                legacy: false,
            }
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), v1);

        // A version 1 UDID stays a UDID.
        fs::write(
            &path,
            r#"{"device": "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE", "os": "26.0"}"#,
        )
        .unwrap();
        let destination = load(&root).store.destination.unwrap();
        assert_eq!(
            destination.udid.as_deref(),
            Some("AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE")
        );
        assert_eq!(destination.name, None);

        // The next pick rewrites it as version 2, the other values kept.
        fs::write(&path, v1).unwrap();
        update(&root, |s| s.configuration = Some("Debug".into())).unwrap();
        assert_eq!(
            read_json(&path),
            json!({
                "version": 2,
                "scheme": "MyApp (staging)",
                "destination": { "kind": "simulator", "name": "iPhone 15 Pro Max", "os": "18.6" },
                "configuration": "Debug"
            })
        );
    }

    #[test]
    fn a_write_waits_for_the_lock_and_merges_what_changed_meanwhile() {
        let root = sandbox();
        update(&root, |s| s.choose_scheme("MyApp")).unwrap();
        let path = store_path(&root);
        // Another writer holds the lock while it adds a key of its own.
        let held = lock(&path).expect("lock taken");
        let (done_tx, done_rx) = mpsc::channel();
        let writer_root = root.clone();
        let writer = std::thread::spawn(move || {
            update(&writer_root, |s| {
                s.choose_destination(simulator("iPhone 17", "26.0"))
            })
            .unwrap();
            done_tx.send(()).unwrap();
        });
        assert!(
            done_rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "the write did not wait for the lock"
        );
        let mut raw = read_json(&path);
        raw["cards"] = json!({ "dismissed": ["navigation"] });
        raw["scheme"] = json!("MyApp Dev");
        fs::write(&path, raw.to_string()).unwrap();
        drop(held);
        writer.join().unwrap();
        let v = read_json(&path);
        assert_eq!(v["cards"], json!({ "dismissed": ["navigation"] }));
        assert_eq!(
            v["scheme"], "MyApp Dev",
            "the other writer's scheme survives"
        );
        assert_eq!(v["destination"]["name"], "iPhone 17");
    }

    #[test]
    fn udid_shapes() {
        assert!(looks_like_udid("AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE"));
        assert!(looks_like_udid("0a1b2c3d-0000-1111-2222-333344445555"));
        assert!(!looks_like_udid("iPhone 17"));
        assert!(!looks_like_udid("AAAA-18"));
    }

    // --- the precedence table ------------------------------------------------

    /// One value of one layer, distinct per layer so the winner shows.
    fn value(field: Field, layer: usize) -> String {
        match field {
            Field::Scheme => format!("Scheme{layer}"),
            Field::Destination => format!("iPhone {}", 10 + layer),
            Field::Configuration => format!("Config{layer}"),
        }
    }

    fn layer_with(field: Field, layer: usize) -> Layer {
        let v = value(field, layer);
        match field {
            Field::Scheme => Layer::legacy(Some(&v), None, None, None),
            Field::Destination => Layer::legacy(None, Some(&v), Some("26.0"), None),
            Field::Configuration => Layer::legacy(None, None, None, Some(&v)),
        }
    }

    fn store_with(root: &Path, field: Field, layer: usize) {
        let v = value(field, layer);
        update(root, |s| match field {
            Field::Scheme => s.choose_scheme(&v),
            Field::Destination => s.choose_destination(simulator(&v, "26.0")),
            Field::Configuration => s.configuration = Some(v.clone()),
        })
        .unwrap();
    }

    /// The same value in a version 1 file, as 0.1's pickers wrote it (0.1
    /// had no configuration; the key reads the same in either version).
    fn v1_store_with(root: &Path, field: Field, layer: usize) {
        let v = value(field, layer);
        let path = store_path(root);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let file = match field {
            Field::Scheme => json!({ "scheme": v }),
            Field::Destination => json!({ "device": v, "os": "26.0" }),
            Field::Configuration => json!({ "configuration": v }),
        };
        fs::write(path, file.to_string()).unwrap();
    }

    fn picked(picks: &Picks, field: Field) -> Option<(String, Source)> {
        match field {
            Field::Scheme => picks.scheme.clone().map(|p| (p.value, p.source)),
            Field::Destination => picks
                .destination
                .clone()
                .map(|p| (p.value.name.unwrap(), p.source)),
            Field::Configuration => picks.configuration.clone().map(|p| (p.value, p.source)),
        }
    }

    /// What the settled value of `field` is, and its source; layer 5 shows
    /// the automatic values ("MyApp", the newest iPhone, no configuration).
    fn settled(picks: &Picks, field: Field) -> Option<(String, Source)> {
        let container = Path::new("/Users/x/MyApp/MyApp.xcodeproj");
        let schemes = picks
            .scheme
            .as_ref()
            .map_or(vec!["MyApp"], |s| vec![s.value.as_str(), "MyApp"]);
        let list = list(&schemes, &["Config1", "Config2", "Config3", "Config4"]);
        let devices: Vec<Value> = [(11, "iPhone-11"), (12, "iPhone-12"), (13, "iPhone-13")]
            .into_iter()
            .chain([(14, "iPhone-14"), (17, "iPhone-17")])
            .map(|(n, model)| {
                json!({ "udid": format!("55555555-AAAA-BBBB-CCCC-0000000000{n}"),
                        "name": format!("iPhone {n}"), "state": "Shutdown", "isAvailable": true,
                        "deviceTypeIdentifier":
                            format!("com.apple.CoreSimulator.SimDeviceType.{model}") })
            })
            .collect();
        let inventory = Inventory::parse(&json!({ "devices": {
            "com.apple.CoreSimulator.SimRuntime.iOS-26-0": devices } }))
        .unwrap();
        match field {
            Field::Scheme => settle_scheme(picks.scheme.as_ref(), &list, container)
                .ok()
                .map(|s| (s.value, s.source)),
            Field::Destination => settle_destination(picks.destination.as_ref(), &inventory)
                .ok()
                .map(|(d, _)| (d.value.name, d.source)),
            Field::Configuration => {
                settle_configuration(picks.configuration.as_ref(), &list, container)
                    .unwrap()
                    .map(|c| (c.value, c.source))
            }
        }
    }

    /// 5 layers × 3 values, once with version 2 stores and once with
    /// version 1 files: every layer from `winner` down sets the value, the
    /// ones above it do not, and `winner` must be the source. The worktree
    /// has no store of its own until layer 2 is in play, so layer 3 is the
    /// main checkout's store, read through `resolve()`; layer 5 is settled
    /// to the automatic values.
    #[test]
    fn precedence_table() {
        let sources = [
            Source::Flag,
            Source::Store,
            Source::MainCheckout,
            Source::Scenario,
            Source::Automatic,
        ];
        let cases = [Field::Scheme, Field::Destination, Field::Configuration]
            .into_iter()
            .flat_map(|field| [(field, false), (field, true)]);
        for (field, v1) in cases {
            let write: fn(&Path, Field, usize) = if v1 { v1_store_with } else { store_with };
            for winner in 1..=5 {
                let (main, worktree) = checkout_and_worktree();
                let set = |layer: usize| layer >= winner && layer < 5;
                if set(2) {
                    write(&worktree, field, 2);
                }
                if set(3) {
                    write(&main, field, 3);
                }
                let req = Request {
                    root: worktree.clone(),
                    workspace: None,
                    flags: if set(1) {
                        layer_with(field, 1)
                    } else {
                        Layer::default()
                    },
                    flag_rank: FlagRank::First,
                    scenario: if set(4) {
                        layer_with(field, 4)
                    } else {
                        Layer::default()
                    },
                    options: Options::default(),
                    warnings: Vec::new(),
                };
                let resolution = resolve(&req).unwrap();
                assert_eq!(
                    resolution
                        .project
                        .container
                        .as_ref()
                        .map(|c| c.relative.clone()),
                    Some(PathBuf::from("MyApp.xcodeproj"))
                );
                let expected = (winner < 5).then(|| (value(field, winner), sources[winner - 1]));
                assert_eq!(
                    picked(&resolution.picks, field),
                    expected,
                    "{field:?}, layer {winner}, v1 {v1}"
                );
                let automatic = match field {
                    Field::Scheme => Some(("MyApp".to_owned(), Source::Automatic)),
                    Field::Destination => Some(("iPhone 17".to_owned(), Source::Automatic)),
                    Field::Configuration => None,
                };
                assert_eq!(
                    settled(&resolution.picks, field),
                    expected.or(automatic),
                    "{field:?}, layer {winner}, v1 {v1}, settled"
                );
                assert!(resolution.picks.warnings.is_empty());
                // Nothing was copied into the worktree, and reading never
                // rewrites a version 1 file.
                assert_eq!(store_path(&worktree).exists(), set(2));
                if v1 && set(3) {
                    assert!(!read_json(&store_path(&main))
                        .as_object()
                        .unwrap()
                        .contains_key("version"));
                }
            }
        }
    }

    #[test]
    fn values_fall_through_layers_one_by_one() {
        let (main, worktree) = checkout_and_worktree();
        store_with(&worktree, Field::Scheme, 2);
        store_with(&main, Field::Scheme, 3);
        store_with(&main, Field::Destination, 3);
        let picks = pick(
            &worktree,
            Some(&main),
            &Layer::default(),
            FlagRank::First,
            &layer_with(Field::Configuration, 4),
        );
        assert_eq!(
            picked(&picks, Field::Scheme),
            Some(("Scheme2".into(), Source::Store))
        );
        assert_eq!(
            picked(&picks, Field::Destination),
            Some(("iPhone 13".into(), Source::MainCheckout))
        );
        assert_eq!(
            picked(&picks, Field::Configuration),
            Some(("Config4".into(), Source::Scenario))
        );
        // The main checkout itself reads only its own store.
        assert_eq!(current(&main).scheme.unwrap().source, Source::Store);
        assert_eq!(
            current(&worktree).destination.unwrap().source,
            Source::MainCheckout
        );
    }

    #[test]
    fn flags_below_the_store_keep_the_0_1_order() {
        let (main, worktree) = checkout_and_worktree();
        let flags = Layer {
            scheme: Some("Scheme1".into()),
            destination: layer_with(Field::Destination, 1).destination,
            configuration: Some("Config1".into()),
        };
        store_with(&worktree, Field::Scheme, 2);
        store_with(&main, Field::Destination, 3);
        let picks = pick(
            &worktree,
            Some(&main),
            &flags,
            FlagRank::BelowStore,
            &layer_with(Field::Configuration, 4),
        );
        // Both stores beat the flags (in a worktree, ⌘B builds what ⌘R
        // builds); the flags beat the scenario.
        assert_eq!(
            picked(&picks, Field::Scheme),
            Some(("Scheme2".into(), Source::Store))
        );
        assert_eq!(
            picked(&picks, Field::Destination),
            Some(("iPhone 13".into(), Source::MainCheckout))
        );
        assert_eq!(
            picked(&picks, Field::Configuration),
            Some(("Config1".into(), Source::Flag))
        );
    }

    #[test]
    fn an_os_alone_narrows_the_next_layers_device() {
        let root = sandbox();
        let path = store_path(&root);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        // A hand-edited version 1 file with only an OS, as 0.1 allowed.
        fs::write(&path, r#"{"os": "18.6"}"#).unwrap();
        let scenario = Layer::legacy(None, Some("iPhone 16e"), Some("26.1"), None);
        let picks = pick(&root, None, &Layer::default(), FlagRank::First, &scenario);
        assert_eq!(
            picks.destination,
            Some(Sourced {
                value: Query::legacy(Some("iPhone 16e"), Some("18.6")).unwrap(),
                source: Source::Store,
            })
        );
        // A picked simulator below is taken by name: its UDID is on another OS.
        let flags = Layer::legacy(None, None, Some("17.5"), None);
        update(&root, |s| {
            s.choose_destination(StoredDestination {
                udid: Some("AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE".into()),
                ..simulator("iPhone 15", "26.1")
            })
        })
        .unwrap();
        let picks = pick(&root, None, &flags, FlagRank::First, &Layer::default());
        let destination = picks.destination.unwrap();
        assert_eq!(destination.source, Source::Flag);
        assert_eq!(
            destination.value,
            Query {
                udid: None,
                name: Some("iPhone 15".into()),
                os: Some("17.5".into()),
                legacy: false,
            }
        );
        // Nothing below: the OS alone (the automatic rule within it).
        let picks = pick(&sandbox(), None, &flags, FlagRank::First, &Layer::default());
        assert_eq!(picks.destination.unwrap().value, flags.destination.unwrap());
    }

    // --- requests --------------------------------------------------------------

    #[test]
    fn cli_requests_read_the_first_xcode_scenario() {
        let root = sandbox();
        fs::create_dir_all(root.join(".zed")).unwrap();
        fs::write(
            root.join(".zed/debug.json"),
            r#"// a comment
            [
              {"adapter": "CodeLLDB", "label": "Other", "scheme": "Wrong"},
              {"adapter": "Xcode", "label": "Run", "request": "launch",
               "workspace": "$ZED_WORKTREE_ROOT/ios/MyApp.xcworkspace",
               "scheme": "MyApp", "device": "iPhone 17", "os": "26.0",
               "derivedData": "$ZED_WORKTREE_ROOT/.build/dd", "buildOutput": "full",
               "oslog": true, "preflight": "xcodegen generate"},
              {"adapter": "Xcode", "label": "Second", "scheme": "Later"}
            ]"#,
        )
        .unwrap();
        let req = Request::for_cli(root.clone(), CliFlags::default());
        assert_eq!(req.workspace, Some(root.join("ios/MyApp.xcworkspace")));
        assert_eq!(
            req.scenario,
            Layer::legacy(Some("MyApp"), Some("iPhone 17"), Some("26.0"), None)
        );
        assert_eq!(req.flags, Layer::default());
        assert_eq!(req.flag_rank, FlagRank::First);
        assert_eq!(
            req.options,
            Options {
                // ⌘B never runs one.
                preflight: None,
                derived_data: Some(root.join(".build/dd")),
                build_output: BuildOutput::Full,
                oslog: true,
                oslog_predicate: None,
            }
        );
        // Flags win over the scenario's options.
        let req = Request::for_cli(
            root.clone(),
            CliFlags {
                workspace: Some(root.join("MyApp.xcodeproj")),
                derived_data: Some("/Users/x/dd".into()),
                ..Default::default()
            },
        );
        assert_eq!(req.workspace, Some(root.join("MyApp.xcodeproj")));
        assert_eq!(req.options.derived_data, Some(PathBuf::from("/Users/x/dd")));

        // A scenario with a wrong type is ignored, with the key named.
        fs::write(
            root.join(".zed/debug.json"),
            r#"[{"adapter": "Xcode", "scheme": "MyApp", "oslog": "yes"}]"#,
        )
        .unwrap();
        let req = Request::for_cli(root.clone(), CliFlags::default());
        assert_eq!(req.scenario, Layer::default());
        assert_eq!(
            req.warnings,
            [
                "Ignoring the Xcode scenario in .zed/debug.json: the key \"oslog\" is invalid: \
              invalid type: string \"yes\", expected a boolean."
            ]
        );
    }

    #[test]
    fn cli_flags_override_once_and_are_never_saved() {
        let root = sandbox();
        fs::create_dir_all(root.join("MyApp.xcodeproj")).unwrap();
        store_with(&root, Field::Scheme, 2);
        store_with(&root, Field::Destination, 2);
        let path = store_path(&root);
        let before = fs::read(&path).unwrap();
        let flags = CliFlags {
            scheme: Some("Scheme1".into()),
            destination: Some("iPhone 11".into()),
            configuration: Some("Config1".into()),
            ..Default::default()
        };
        let req = Request::for_cli(root.clone(), flags.clone());
        assert_eq!(req.flag_rank, FlagRank::First);
        assert!(req.warnings.is_empty());
        let picks = resolve(&req).unwrap().picks;
        for field in [Field::Scheme, Field::Destination, Field::Configuration] {
            assert_eq!(
                picked(&picks, field),
                Some((value(field, 1), Source::Flag)),
                "{field:?}"
            );
        }
        assert_eq!(
            Source::Flag.label(Field::Destination),
            "--destination/--device",
            "named in messages"
        );
        // Nothing was written, and without the flags the store answers again.
        assert_eq!(fs::read(&path).unwrap(), before);
        let picks = resolve(&Request::for_cli(root.clone(), CliFlags::default()))
            .unwrap()
            .picks;
        assert_eq!(
            picked(&picks, Field::Scheme),
            Some(("Scheme2".into(), Source::Store))
        );

        // A 0.1 task's flags rank below the store, with one line about it.
        let req = Request::for_cli(
            root.clone(),
            CliFlags {
                legacy_task: Some("Xcode: Build"),
                ..flags
            },
        );
        assert_eq!(req.flag_rank, FlagRank::BelowStore);
        assert_eq!(req.warnings, [legacy_task_line("Xcode: Build")]);
        let resolution = resolve(&req).unwrap();
        assert_eq!(
            picked(&resolution.picks, Field::Scheme),
            Some(("Scheme2".into(), Source::Store))
        );
        assert_eq!(
            picked(&resolution.picks, Field::Destination),
            Some(("iPhone 12".into(), Source::Store))
        );
        // The store chose no configuration: the flag's applies.
        assert_eq!(
            picked(&resolution.picks, Field::Configuration),
            Some(("Config1".into(), Source::Flag))
        );
        assert_eq!(resolution.picks.warnings, req.warnings);
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn launch_requests_take_the_launched_config() {
        let cfg: LaunchConfig = serde_json::from_value(json!({
            "scheme": "MyApp", "configuration": "Release", "preflight": "tuist generate --no-open",
            "terminateOnStop": false
        }))
        .unwrap();
        let req = Request::for_launch(&cfg, "/Users/Jane/MyApp".into());
        assert_eq!(req.workspace, None);
        assert_eq!(req.flag_rank, FlagRank::First);
        assert_eq!(
            req.scenario,
            Layer::legacy(Some("MyApp"), None, None, Some("Release"))
        );
        assert_eq!(
            req.options.preflight.as_deref(),
            Some("tuist generate --no-open")
        );
    }

    #[test]
    fn resolve_reports_a_tie_and_takes_a_named_container() {
        let root = sandbox();
        fs::create_dir_all(root.join("ios/MyApp.xcworkspace")).unwrap();
        fs::create_dir_all(root.join("demo/Demo.xcworkspace")).unwrap();
        let empty: LaunchConfig = serde_json::from_value(json!({})).unwrap();
        let mut req = Request::for_launch(&empty, root.clone());
        assert_eq!(
            resolve(&req).unwrap_err().to_string(),
            "Found 2 Xcode workspaces: demo/Demo.xcworkspace, ios/MyApp.xcworkspace. Set \
             \"workspace\" in this project's Xcode scenario (.zed/debug.json), or run \
             \"xcode-dap setup\" here to choose one."
        );
        req.workspace = Some("ios/MyApp.xcworkspace".into());
        let resolution = resolve(&req).unwrap();
        assert_eq!(
            resolution.project.container.unwrap().path,
            root.join("ios/MyApp.xcworkspace")
        );
    }

    // --- validation ------------------------------------------------------------

    fn list(schemes: &[&str], configurations: &[&str]) -> SchemeList {
        SchemeList {
            schemes: schemes.iter().map(|s| s.to_string()).collect(),
            configurations: configurations.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn from_store(value: &str) -> Sourced<String> {
        Sourced {
            value: value.into(),
            source: Source::Store,
        }
    }

    #[test]
    fn schemes_settle_on_the_pick_or_the_only_one() {
        let ws = Path::new("/Users/x/MyApp/MyApp.xcworkspace");
        let three = list(&["MyApp", "MyApp Dev", "Widget"], &[]);
        assert_eq!(
            settle_scheme(Some(&from_store("myapp dev")), &three, ws).unwrap(),
            from_store("MyApp Dev")
        );
        assert_eq!(
            settle_scheme(Some(&from_store("MyApp Beta")), &three, ws)
                .unwrap_err()
                .to_string(),
            "Scheme \"MyApp Beta\" (from .zed/.zedx/selection.json) is not in \
             MyApp.xcworkspace. Choose another with Xcode: Choose Scheme; after regenerating \
             the project, choosing again reloads the list."
        );
        assert_eq!(
            settle_scheme(None, &three, ws).unwrap_err().to_string(),
            "No scheme chosen: MyApp.xcworkspace has 3 schemes (MyApp, MyApp Dev, Widget). Run \
             Xcode: Choose Scheme (⇧⌘R) or \"xcode-dap select-scheme\"."
        );
        assert_eq!(
            settle_scheme(None, &list(&["MyApp"], &[]), ws).unwrap(),
            Sourced {
                value: "MyApp".into(),
                source: Source::Automatic
            }
        );
        let many: Vec<String> = (1..=12).map(|i| format!("S{i}")).collect();
        let many = SchemeList {
            schemes: many,
            configurations: vec![],
        };
        assert!(settle_scheme(None, &many, ws)
            .unwrap_err()
            .to_string()
            .contains("has 12 schemes (S1, S2, S3, S4, S5, S6, S7, S8, S9, S10, …)"));
    }

    #[test]
    fn configurations_settle_when_the_container_lists_them() {
        let project = Path::new("/Users/x/MyApp/MyApp.xcodeproj");
        let listed = list(&["MyApp"], &["Debug", "Release"]);
        assert_eq!(settle_configuration(None, &listed, project).unwrap(), None);
        assert_eq!(
            settle_configuration(Some(&from_store("release")), &listed, project).unwrap(),
            Some(from_store("Release"))
        );
        assert_eq!(
            settle_configuration(Some(&from_store("Beta")), &listed, project)
                .unwrap_err()
                .to_string(),
            "Configuration \"Beta\" (from .zed/.zedx/selection.json) is not in MyApp.xcodeproj \
             (Debug, Release). Choose another with Xcode: Choose Configuration."
        );
        // A workspace lists none: the pick is taken as it is.
        assert_eq!(
            settle_configuration(Some(&from_store("Beta")), &list(&["MyApp"], &[]), project)
                .unwrap(),
            Some(from_store("Beta"))
        );
    }

    #[test]
    fn destinations_settle_with_their_source() {
        let inventory = Inventory::parse(&json!({ "devices": {
            "com.apple.CoreSimulator.SimRuntime.iOS-26-0": [
                { "udid": "33333333-AAAA-BBBB-CCCC-000000000001", "name": "iPhone 17",
                  "state": "Shutdown", "isAvailable": true,
                  "deviceTypeIdentifier": "com.apple.CoreSimulator.SimDeviceType.iPhone-17" },
            ]
        }}))
        .unwrap();
        let (device, warnings) = settle_destination(None, &inventory).unwrap();
        assert_eq!(device.source, Source::Automatic);
        assert!(warnings.is_empty());
        let pick = Sourced {
            value: Query::legacy(Some("iPhone 99"), Some("26.0")).unwrap(),
            source: Source::Scenario,
        };
        assert_eq!(
            settle_destination(Some(&pick), &inventory)
                .unwrap_err()
                .to_string(),
            "Destination \"iPhone 99 · iOS 26.0\" (from the Xcode scenario) is not available. \
             Use Xcode: Choose Destination."
        );
        assert_eq!(
            summary(&from_store("MyApp"), Some(&device), None),
            "Scheme: MyApp (selection.json) | Destination: iPhone 17 · iOS 26.0 (automatic) | \
             Configuration: scheme default"
        );
        assert_eq!(
            summary(
                &from_store("MyApp"),
                None,
                Some(&Sourced {
                    value: "Release".into(),
                    source: Source::Flag
                })
            ),
            "Scheme: MyApp (selection.json) | Configuration: Release (flag)"
        );
    }
}
