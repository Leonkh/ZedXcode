//! Persistent per-`(build_root, scheme)` store of Swift compile arguments,
//! parsed out of `.xcactivitylog` build logs by [`super::xcactivitylog`] and
//! served to sourcekit-lsp (a Build Server answering "how do I compile this
//! file?"). Merges incrementally so a rebuild of one module doesn't drop the
//! others, expands `@…SwiftFileList` response files at serve time (so files
//! added since the last build are picked up without a re-parse), and infers
//! args for a brand-new file from a sibling in the same module.
//!
//! Wired into the `bsp` subcommand's Build Server ([`crate::bsp`]).

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::engine::xcactivitylog::{shell_split, ParsedModule};
use crate::setup::jsonc;
use crate::util::hash::fnv1a64;
use crate::util::paths::{mtime, zedxcode_home};

/// On-disk schema version. Bump on any incompatible change; a stored file
/// with a different version is ignored (treated as empty).
pub const STORE_VERSION: u32 = 1;

/// One module's stored compile arguments. `args` keeps `@…SwiftFileList`
/// entries verbatim (expanded at serve time) and has `argv[0]` (the `swiftc`
/// path) already dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleEntry {
    pub args: Vec<String>,
    pub working_dir: String,
    pub files: Vec<String>,
    pub file_lists: Vec<String>,
    pub index_store_path: Option<String>,
}

/// How a locked merge should treat the persisted poll watermark
/// (`last_ingested_log`), which records the newest `.xcactivitylog` bsp has
/// folded in.
pub enum Watermark {
    /// The bsp poll loop owns the watermark: advance it to this log filename
    /// (only when the merge actually changed the store).
    Advance(Option<String>),
    /// The build pipeline ingests a different source (xcodebuild stdout) and
    /// does not own the watermark — leave whatever is already on disk.
    Keep,
}

/// Result of [`CompileStore::merge_save_locked`].
pub struct LockedMerge {
    /// The store as it now sits on disk: the freshly-read on-disk copy when
    /// `changed` is false, else the merged, just-persisted store. An in-memory
    /// caller adopts this so its view equals disk.
    pub store: CompileStore,
    /// The store file's mtime, read under the lock right after the save. A bsp
    /// caller records it as its own last-write mtime so the store-watch does
    /// not treat this process's own write as an external change.
    pub mtime: Option<SystemTime>,
    /// Whether a non-empty merge actually changed and persisted the store.
    pub changed: bool,
}

/// The persisted portion of a store.
#[derive(Debug, Serialize, Deserialize)]
struct Persisted {
    version: u32,
    build_root: String,
    scheme: String,
    modules: BTreeMap<String, ModuleEntry>,
    /// Newest build-log filename already ingested (the bsp poll watermark).
    /// `#[serde(default)]` keeps older store files (written before this field
    /// existed) loading as `None` without a version bump.
    #[serde(default)]
    last_log: Option<String>,
}

/// A loaded compile-args store: the persisted module map plus in-memory
/// indexes (file → module, dir → module) derived at load time.
pub struct CompileStore {
    version: u32,
    build_root: String,
    scheme: String,
    modules: BTreeMap<String, ModuleEntry>,
    last_log: Option<String>,
    // Derived, never persisted. Rebuilt on load and after every merge.
    file_index: HashMap<String, String>,
    dir_index: HashMap<String, String>,
}

impl CompileStore {
    /// An empty store for `(build_root, scheme)`. `build_root` is normalised
    /// to an absolute path so the store file and lookups are stable.
    pub fn new(build_root: &Path, scheme: &str) -> Self {
        Self {
            version: STORE_VERSION,
            build_root: abs_string(build_root),
            scheme: scheme.to_string(),
            modules: BTreeMap::new(),
            last_log: None,
            file_index: HashMap::new(),
            dir_index: HashMap::new(),
        }
    }

    /// The cache file for `(build_root, scheme)`:
    /// `~/.zedxcode/cache/compile-store-<hash(build_root)>-<hash(scheme)>-<scheme>.json`.
    /// The build-root path is hashed (no raw path text on disk); the scheme is
    /// both hashed (for a collision-free filename) and sanitised (for
    /// readability).
    pub fn store_path(build_root: &Path, scheme: &str) -> Result<PathBuf> {
        Ok(zedxcode_home()?
            .join("cache")
            .join(store_file_name(build_root, scheme)))
    }

    /// Load the store for `(build_root, scheme)`. Fail-soft: a missing,
    /// unreadable, corrupt, or mismatched file yields an empty store, never
    /// an error (a broken cache must never break serving).
    pub fn load(build_root: &Path, scheme: &str) -> Self {
        let store = Self::new(build_root, scheme);
        match Self::store_path(build_root, scheme) {
            Ok(path) => Self::read_json(store, &path),
            Err(_) => {
                let mut store = store;
                store.rebuild_indexes();
                store
            }
        }
    }

    /// Cross-process-safe read-merge-write for `(build_root, scheme)`.
    ///
    /// The build pipeline (in the DAP/run process) and the bsp poll loop (a
    /// separate `xcode-dap bsp` process) both fold freshly parsed modules into
    /// the *same* store file. Without coordination their independent
    /// load→merge→save sequences interleave and lose each other's modules. This
    /// takes an advisory `flock` on the store's sidecar `<file>.lock` and, under
    /// it, re-reads the current on-disk store, merges every non-empty group in
    /// `parsed`, applies `watermark`, and writes atomically — so a concurrent
    /// writer's modules are read in, never clobbered.
    ///
    /// Parsing must already have happened off-lock (a workspace log is 100+
    /// modules); only the fast read+merge+save runs in the critical section.
    /// The lock is best-effort and fail-open, exactly like the logger's
    /// rotation lock ([`crate::util::logging`]): a lock that cannot be created
    /// or acquired simply runs the same sequence unlocked rather than aborting
    /// ingestion.
    ///
    /// An all-empty `parsed` is a no-op (no lock, no write): the returned
    /// [`LockedMerge`] carries the current on-disk store and `changed: false`.
    /// Otherwise the returned store equals what was just persisted, and `mtime`
    /// is that file's mtime read under the lock (record it so the store-watch
    /// does not mistake this process's own write for an external one).
    pub fn merge_save_locked(
        build_root: &Path,
        scheme: &str,
        parsed: Vec<Vec<ParsedModule>>,
        watermark: Watermark,
    ) -> LockedMerge {
        match Self::store_path(build_root, scheme) {
            Ok(path) => Self::merge_save_locked_at(&path, build_root, scheme, parsed, watermark),
            // No resolvable path (e.g. HOME unset): merge in memory so
            // ingestion still degrades gracefully; nothing is persisted.
            Err(_) => {
                let mut store = Self::new(build_root, scheme);
                let changed = merge_all(&mut store, parsed, watermark);
                LockedMerge {
                    store,
                    mtime: None,
                    changed,
                }
            }
        }
    }

    /// [`merge_save_locked`](Self::merge_save_locked) with an explicit
    /// store-file path (the public entry resolves it from `(build_root,
    /// scheme)`; tests pass a temp path).
    fn merge_save_locked_at(
        store_path: &Path,
        build_root: &Path,
        scheme: &str,
        parsed: Vec<Vec<ParsedModule>>,
        watermark: Watermark,
    ) -> LockedMerge {
        if !parsed.iter().any(|m| !m.is_empty()) {
            // Nothing to merge: hand back the current on-disk store untouched
            // (no lock, no write — a no-op save would bump mtime and trigger a
            // pointless store-watch reload).
            let store = Self::read_json(Self::new(build_root, scheme), store_path);
            return LockedMerge {
                store,
                mtime: mtime(store_path),
                changed: false,
            };
        }
        // Held across the re-read, merge, save and post-save stat below; drops
        // (unlocks) when the function returns. `None` -> proceed unlocked.
        let _lock = store_lock(store_path);
        let mut store = Self::read_json(Self::new(build_root, scheme), store_path);
        let changed = merge_all(&mut store, parsed, watermark);
        if let Err(e) = store.write_json(store_path) {
            log::warn!("compile-store locked save failed: {e:#}");
        }
        LockedMerge {
            store,
            mtime: mtime(store_path),
            changed,
        }
    }

    /// Merge parsed modules in, replacing entries for the modules present and
    /// keeping every other module. A zero-module parse is a strict no-op (a
    /// clean rebuild whose log yielded nothing must not wipe the store).
    pub fn merge(&mut self, parsed: Vec<ParsedModule>) {
        if parsed.is_empty() {
            return;
        }
        for m in parsed {
            if m.module_name.is_empty() {
                continue;
            }
            self.modules.insert(
                m.module_name,
                ModuleEntry {
                    args: m.args,
                    working_dir: m.working_dir,
                    files: m.files,
                    file_lists: m.file_lists,
                    index_store_path: m.index_store_path,
                },
            );
        }
        self.rebuild_indexes();
    }

    /// Compile arguments + working directory for `path`, or `None` for a file
    /// no module can be inferred for. A known file serves its module's args
    /// (`@filelist` expanded, `\=` unescaped, sourcekit-hostile args dropped).
    /// An unknown `.swift` file borrows a sibling module's args (same
    /// directory, else the nearest ancestor up to `project_root` / a `.git`).
    pub fn options_for_file(
        &self,
        path: &Path,
        project_root: Option<&Path>,
    ) -> Option<(Vec<String>, String)> {
        let key = file_key(path);
        if let Some(module) = self.file_index.get(&key) {
            let entry = self.modules.get(module)?;
            return Some((self.serve_args(entry, None), entry.working_dir.clone()));
        }
        if !is_swift(path) {
            return None;
        }
        let module = self.new_file_module(&key, project_root)?;
        let entry = self.modules.get(module)?;
        let extra = path.to_string_lossy().into_owned();
        Some((
            self.serve_args(entry, Some(&extra)),
            entry.working_dir.clone(),
        ))
    }

    pub fn module_count(&self) -> usize {
        self.modules.len()
    }

    pub fn is_empty(&self) -> bool {
        self.modules.is_empty()
    }

    /// Newest build-log filename ingested into this store (the bsp poll
    /// watermark), or `None` if never set. Persisted so a restart resumes
    /// ingestion without re-parsing already-ingested build history.
    pub fn last_ingested_log(&self) -> Option<&str> {
        self.last_log.as_deref()
    }

    pub fn set_last_ingested_log(&mut self, name: Option<String>) {
        self.last_log = name;
    }

    #[cfg(test)]
    pub fn module(&self, name: &str) -> Option<&ModuleEntry> {
        self.modules.get(name)
    }

    // -- serving ------------------------------------------------------------

    /// Turn a module's stored args into what sourcekit-lsp should receive:
    /// drop the sourcekit-hostile flags, expand `@filelist` entries (falling
    /// back to the stored file list when the referenced file is gone), and
    /// unescape Xcode's `\=`. `extra` appends a not-yet-compiled file.
    fn serve_args(&self, entry: &ModuleEntry, extra: Option<&str>) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut it = entry.args.iter();
        while let Some(arg) = it.next() {
            if arg == "-emit-localized-strings-path" {
                it.next(); // drop the path value too
                continue;
            }
            if arg == "-use-frontend-parseable-output" || arg == "-emit-localized-strings" {
                continue;
            }
            if let Some(list) = arg.strip_prefix('@') {
                let path = Path::new(list);
                if path.is_file() {
                    out.extend(read_file_args(path));
                } else {
                    out.extend(entry.files.iter().cloned());
                }
                continue;
            }
            out.push(unescape_eq(arg));
        }
        if let Some(f) = extra {
            out.push(f.to_string());
        }
        out
    }

    // -- new-file inference -------------------------------------------------

    /// Module for a `.swift` file absent from the index: a sibling in the
    /// same directory, else the nearest ancestor directory that owns one,
    /// stopping at `project_root` (or a directory containing `.git`).
    fn new_file_module(&self, key: &str, project_root: Option<&Path>) -> Option<&String> {
        let dir = parent_key(key)?;
        if let Some(module) = self.dir_index.get(&dir) {
            return Some(module);
        }
        let mut current = dir;
        while let Some(parent) = parent_key(&current) {
            if parent == current {
                break; // reached the filesystem root
            }
            if let Some(module) = self.dir_index.get(&parent) {
                return Some(module);
            }
            if is_project_root(&parent, project_root) {
                break;
            }
            current = parent;
        }
        None
    }

    // -- indexes / persistence ---------------------------------------------

    fn rebuild_indexes(&mut self) {
        let mut file_index: HashMap<String, String> = HashMap::new();
        for (name, entry) in &self.modules {
            for key in module_file_keys(entry) {
                file_index.insert(key, name.clone());
            }
        }
        let mut dir_index: HashMap<String, String> = HashMap::new();
        for (key, module) in &file_index {
            if !key.ends_with(".swift") {
                continue;
            }
            if let Some(dir) = parent_key(key) {
                dir_index.entry(dir).or_insert_with(|| module.clone());
            }
        }
        self.file_index = file_index;
        self.dir_index = dir_index;
    }

    fn write_json(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let persisted = Persisted {
            version: self.version,
            build_root: self.build_root.clone(),
            scheme: self.scheme.clone(),
            modules: self.modules.clone(),
            last_log: self.last_log.clone(),
        };
        let mut text =
            serde_json::to_string_pretty(&persisted).expect("CompileStore always serializes");
        text.push('\n');
        jsonc::atomic_write(path, &text)?;
        Ok(())
    }

    /// Populate `store` from `path` if it parses, matches the version, and
    /// was written for the same `(build_root, scheme)`; otherwise leave it
    /// empty. Always rebuilds the derived indexes.
    fn read_json(mut store: Self, path: &Path) -> Self {
        if let Ok(bytes) = std::fs::read(path) {
            if let Ok(p) = serde_json::from_slice::<Persisted>(&bytes) {
                if p.version == STORE_VERSION
                    && p.build_root == store.build_root
                    && p.scheme == store.scheme
                {
                    store.modules = p.modules;
                    store.last_log = p.last_log;
                }
            }
        }
        store.rebuild_indexes();
        store
    }
}

// ---------------------------------------------------------------------------
// free helpers
// ---------------------------------------------------------------------------

fn abs_string(path: &Path) -> String {
    std::path::absolute(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// Merge every non-empty group into `store`, advance the watermark on a real
/// change, and report whether anything was merged. Shared by the locked
/// read-merge-write and its path-less fallback.
fn merge_all(
    store: &mut CompileStore,
    parsed: Vec<Vec<ParsedModule>>,
    watermark: Watermark,
) -> bool {
    let mut changed = false;
    for modules in parsed {
        if !modules.is_empty() {
            store.merge(modules);
            changed = true;
        }
    }
    if let Watermark::Advance(w) = watermark {
        if changed {
            store.set_last_ingested_log(w);
        }
    }
    changed
}

/// `<store-file>.lock` — the advisory-lock sidecar for a store file.
fn store_lock_path(store_path: &Path) -> PathBuf {
    let mut os = store_path.as_os_str().to_os_string();
    os.push(".lock");
    PathBuf::from(os)
}

/// `flock(LOCK_EX)` on the store's sidecar lock file; the lock releases when
/// the returned `File` drops (closing the fd unlocks). Returns `None` on any
/// failure so the caller proceeds unlocked (fail-open). Mirrors
/// [`crate::util::logging`]'s `rotation_lock`.
fn store_lock(store_path: &Path) -> Option<File> {
    use std::os::fd::AsRawFd;
    let lock_path = store_lock_path(store_path);
    if let Some(dir) = lock_path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&lock_path)
        .ok()?;
    // SAFETY: `file` is a valid open fd for the lifetime of the call.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return None;
    }
    Some(file)
}

fn store_file_name(build_root: &Path, scheme: &str) -> String {
    let abs = std::path::absolute(build_root).unwrap_or_else(|_| build_root.to_path_buf());
    let root_hash = fnv1a64(abs.as_os_str().as_encoded_bytes());
    // Hash the raw scheme too: `sanitize_scheme` is a lossy many-to-one map
    // (e.g. `App/Dev` and `App:Dev` both sanitise to `App-Dev`), so two schemes
    // could otherwise share one file and mutually clobber each other's cache.
    let scheme_hash = fnv1a64(scheme.as_bytes());
    format!(
        "compile-store-{root_hash:016x}-{scheme_hash:016x}-{}.json",
        sanitize_scheme(scheme)
    )
}

/// Keep a scheme readable in the filename while staying filesystem-safe. This
/// sanitised segment is a readability aid only; filename uniqueness across
/// schemes is guaranteed by the `<hash(scheme)>` segment (see
/// [`store_file_name`]), with the stored `scheme` field a further mismatch
/// safety net on load.
fn sanitize_scheme(scheme: &str) -> String {
    let s: String = scheme
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if s.is_empty() {
        "scheme".to_string()
    } else {
        s
    }
}

/// Xcode escapes `=` as `\=` in the logged command; sourcekit wants it plain.
fn unescape_eq(arg: &str) -> String {
    arg.replace(r"\=", "=")
}

fn is_swift(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("swift"))
}

/// Case-insensitive absolute path key (realpath when the file exists, else a
/// lexical absolute path). Matches the reference's lowercased realpath keys.
fn file_key(path: &Path) -> String {
    std::fs::canonicalize(path)
        .or_else(|_| std::path::absolute(path))
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_lowercase()
}

fn parent_key(key: &str) -> Option<String> {
    Path::new(key)
        .parent()
        .map(|p| p.to_string_lossy().to_lowercase())
}

fn is_project_root(key: &str, project_root: Option<&Path>) -> bool {
    if let Some(root) = project_root {
        if file_key(root) == key {
            return true;
        }
    }
    Path::new(key).join(".git").exists()
}

/// File keys owned by a module: the module's `.SwiftFileList` contents (read
/// at index time), falling back to the stored inline `files` for any list
/// that no longer exists (DerivedData wiped) or when there is no list.
fn module_file_keys(entry: &ModuleEntry) -> Vec<String> {
    let mut keys = Vec::new();
    if entry.file_lists.is_empty() {
        for f in &entry.files {
            keys.push(file_key(Path::new(f)));
        }
        return keys;
    }
    let mut any_missing = false;
    for list in &entry.file_lists {
        let path = Path::new(list);
        if path.is_file() {
            for arg in read_file_args(path) {
                keys.push(file_key(Path::new(&arg)));
            }
        } else {
            any_missing = true;
        }
    }
    if any_missing {
        for f in &entry.files {
            keys.push(file_key(Path::new(f)));
        }
    }
    keys
}

fn read_file_args(path: &Path) -> Vec<String> {
    match std::fs::read_to_string(path) {
        Ok(text) => shell_split(&text),
        Err(_) => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn tmpdir() -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "zedxcode-compile-store-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn module(
        name: &str,
        args: Vec<&str>,
        files: Vec<&str>,
        file_lists: Vec<&str>,
    ) -> ParsedModule {
        ParsedModule {
            module_name: name.to_string(),
            working_dir: format!("/Users/x/MyApp/Modules/{name}"),
            index_store_path: Some("/Users/x/DD/Index.noindex/DataStore".to_string()),
            files: files.into_iter().map(String::from).collect(),
            file_lists: file_lists.into_iter().map(String::from).collect(),
            args: args.into_iter().map(String::from).collect(),
        }
    }

    #[test]
    fn zero_module_merge_is_a_strict_no_op() {
        let mut store = CompileStore::new(Path::new("/Users/x/DD"), "MyApp");
        store.merge(vec![module(
            "AlphaKit",
            vec!["-module-name", "AlphaKit"],
            vec![],
            vec![],
        )]);
        assert_eq!(store.module_count(), 1);
        let before = store.module("AlphaKit").cloned();

        store.merge(vec![]); // zero-module parse
        assert_eq!(store.module_count(), 1, "zero-module merge must not wipe");
        assert_eq!(store.module("AlphaKit").cloned(), before);
    }

    #[test]
    fn merge_is_last_wins_per_module_and_keeps_others() {
        let mut store = CompileStore::new(Path::new("/Users/x/DD"), "MyApp");
        store.merge(vec![
            module(
                "AlphaKit",
                vec!["-module-name", "AlphaKit", "-Onone"],
                vec![],
                vec![],
            ),
            module("GammaKit", vec!["-module-name", "GammaKit"], vec![], vec![]),
        ]);
        // Re-parse of AlphaKit only: replaces AlphaKit, keeps GammaKit.
        store.merge(vec![module(
            "AlphaKit",
            vec!["-module-name", "AlphaKit", "-O"],
            vec![],
            vec![],
        )]);
        assert_eq!(store.module_count(), 2);
        assert!(store
            .module("AlphaKit")
            .unwrap()
            .args
            .contains(&"-O".to_string()));
        assert!(!store
            .module("AlphaKit")
            .unwrap()
            .args
            .contains(&"-Onone".to_string()));
        assert!(store.module("GammaKit").is_some());
    }

    #[test]
    fn serves_expanded_filelist_dropped_args_and_unescaped_eq() {
        let dir = tmpdir();
        let filelist = dir.join("AlphaKit.SwiftFileList");
        std::fs::write(
            &filelist,
            "/Users/x/MyApp/Sources/A.swift\n/Users/x/MyApp/Sources/B.swift\n",
        )
        .unwrap();
        let list_arg = format!("@{}", filelist.display());

        let mut store = CompileStore::new(Path::new("/Users/x/DD"), "MyApp");
        store.merge(vec![module(
            "AlphaKit",
            vec![
                "-module-name",
                "AlphaKit",
                r"-enforce-exclusivity\=checked",
                "-use-frontend-parseable-output",
                "-emit-localized-strings",
                "-emit-localized-strings-path",
                "/Users/x/DD/loc",
                &list_arg,
                "-DDEBUG",
            ],
            vec!["/Users/x/MyApp/Sources/Fallback.swift"],
            vec![filelist.to_str().unwrap()],
        )]);

        let (args, wd) = store
            .options_for_file(Path::new("/Users/x/MyApp/Sources/A.swift"), None)
            .expect("A.swift resolves to AlphaKit");
        assert_eq!(wd, "/Users/x/MyApp/Modules/AlphaKit");
        // filelist expanded inline
        assert!(args.contains(&"/Users/x/MyApp/Sources/A.swift".to_string()));
        assert!(args.contains(&"/Users/x/MyApp/Sources/B.swift".to_string()));
        assert!(!args.iter().any(|a| a.starts_with('@')));
        // \= unescaped
        assert!(args.contains(&"-enforce-exclusivity=checked".to_string()));
        assert!(!args.iter().any(|a| a.contains(r"\=")));
        // sourcekit-hostile flags (and the localized-strings value) dropped
        assert!(!args.contains(&"-use-frontend-parseable-output".to_string()));
        assert!(!args.contains(&"-emit-localized-strings".to_string()));
        assert!(!args.contains(&"-emit-localized-strings-path".to_string()));
        assert!(!args.contains(&"/Users/x/DD/loc".to_string()));
        assert!(args.contains(&"-DDEBUG".to_string()));

        // filelist gone -> serve falls back to the stored files
        std::fs::remove_file(&filelist).unwrap();
        let (args, _) = store
            .options_for_file(Path::new("/Users/x/MyApp/Sources/A.swift"), None)
            .expect("still resolves via the pre-built index");
        assert!(args.contains(&"/Users/x/MyApp/Sources/Fallback.swift".to_string()));
        assert!(!args.contains(&"/Users/x/MyApp/Sources/B.swift".to_string()));
    }

    #[test]
    fn new_file_inference_same_dir_and_parent_walk() {
        let dir = tmpdir();
        let filelist = dir.join("AlphaKit.SwiftFileList");
        std::fs::write(&filelist, "/Users/x/MyApp/Sources/A.swift\n").unwrap();

        let mut store = CompileStore::new(Path::new("/Users/x/DD"), "MyApp");
        store.merge(vec![module(
            "AlphaKit",
            vec![
                "-module-name",
                "AlphaKit",
                &format!("@{}", filelist.display()),
            ],
            vec![],
            vec![filelist.to_str().unwrap()],
        )]);

        // same directory as A.swift
        let (args, _) = store
            .options_for_file(Path::new("/Users/x/MyApp/Sources/New.swift"), None)
            .expect("same-dir sibling");
        assert!(args.contains(&"/Users/x/MyApp/Sources/New.swift".to_string()));

        // nested directory: walk up to Sources, bounded by project_root
        let (args, _) = store
            .options_for_file(
                Path::new("/Users/x/MyApp/Sources/Sub/Deep.swift"),
                Some(Path::new("/Users/x/MyApp")),
            )
            .expect("parent-walk sibling");
        assert!(args.contains(&"/Users/x/MyApp/Sources/Sub/Deep.swift".to_string()));

        // outside any known module -> None
        assert!(store
            .options_for_file(
                Path::new("/Users/x/Elsewhere/Foo.swift"),
                Some(Path::new("/Users/x/MyApp"))
            )
            .is_none());

        // non-.swift -> None
        assert!(store
            .options_for_file(Path::new("/Users/x/MyApp/Sources/README.md"), None)
            .is_none());
    }

    #[test]
    fn save_load_round_trip_and_mismatch_is_ignored() {
        let dir = tmpdir();
        let path = dir.join("store.json");

        let mut store = CompileStore::new(Path::new("/Users/x/DD"), "MyApp");
        store.merge(vec![module(
            "AlphaKit",
            vec!["-module-name", "AlphaKit", "-DDEBUG"],
            vec!["/Users/x/MyApp/Sources/A.swift"],
            vec![],
        )]);
        store.write_json(&path).unwrap();

        let loaded =
            CompileStore::read_json(CompileStore::new(Path::new("/Users/x/DD"), "MyApp"), &path);
        assert_eq!(loaded.module_count(), 1);
        assert_eq!(
            loaded.module("AlphaKit").unwrap().args,
            store.module("AlphaKit").unwrap().args
        );
        // index rebuilt on load: A.swift resolves
        assert!(loaded
            .options_for_file(Path::new("/Users/x/MyApp/Sources/A.swift"), None)
            .is_some());

        // A store written for a different build_root must be ignored.
        let mismatched = CompileStore::read_json(
            CompileStore::new(Path::new("/Users/x/Other"), "MyApp"),
            &path,
        );
        assert!(mismatched.is_empty());
    }

    #[test]
    fn last_ingested_log_watermark_round_trips() {
        let dir = tmpdir();
        let path = dir.join("store.json");

        let mut store = CompileStore::new(Path::new("/Users/x/DD"), "MyApp");
        store.merge(vec![module(
            "AlphaKit",
            vec!["-module-name", "AlphaKit"],
            vec![],
            vec![],
        )]);
        assert_eq!(store.last_ingested_log(), None);
        store.set_last_ingested_log(Some("42-abc.xcactivitylog".to_string()));
        store.write_json(&path).unwrap();

        let loaded =
            CompileStore::read_json(CompileStore::new(Path::new("/Users/x/DD"), "MyApp"), &path);
        assert_eq!(loaded.last_ingested_log(), Some("42-abc.xcactivitylog"));

        // A v1 store file written before this field existed (no `last_log`
        // key) still loads, defaulting the watermark to None.
        let legacy = dir.join("legacy.json");
        std::fs::write(
            &legacy,
            r#"{"version":1,"build_root":"/Users/x/DD","scheme":"MyApp","modules":{}}"#,
        )
        .unwrap();
        let loaded_legacy = CompileStore::read_json(
            CompileStore::new(Path::new("/Users/x/DD"), "MyApp"),
            &legacy,
        );
        assert_eq!(loaded_legacy.last_ingested_log(), None);
    }

    #[test]
    fn store_file_name_is_unique_and_path_free() {
        let a = store_file_name(Path::new("/Users/x/DD-one"), "MyApp");
        let b = store_file_name(Path::new("/Users/x/DD-two"), "MyApp");
        let c = store_file_name(Path::new("/Users/x/DD-one"), "MyApp (staging)");
        assert_ne!(a, b, "different build roots -> different files");
        assert_ne!(a, c, "different schemes -> different files");
        assert!(a.starts_with("compile-store-") && a.ends_with(".json"));
        // scheme sanitised (no spaces or parens leaking into the filename)
        assert!(!c.contains(' ') && !c.contains('(') && !c.contains(')'));
        // no raw path text on disk
        assert!(!a.contains("Users") && !a.contains("DD-one"));
    }

    #[test]
    fn store_file_name_distinguishes_schemes_that_sanitize_alike() {
        // Two schemes that sanitize_scheme maps to the same readable segment
        // must still get distinct files (guaranteed by the scheme-hash segment),
        // so they can never destroy each other's cache.
        let root = Path::new("/Users/x/DD");
        assert_eq!(sanitize_scheme("App/Dev"), sanitize_scheme("App:Dev"));
        assert_ne!(
            store_file_name(root, "App/Dev"),
            store_file_name(root, "App:Dev"),
            "colliding sanitized schemes -> still distinct files"
        );
    }

    #[test]
    fn merge_save_locked_preserves_concurrent_writers_modules() {
        let dir = tmpdir();
        let store_path = dir.join("compile-store.json");
        let br = Path::new("/Users/x/DD");

        // Writer 1 (e.g. the bsp poll loop) ingests AlphaKit and advances the
        // watermark.
        let r1 = CompileStore::merge_save_locked_at(
            &store_path,
            br,
            "MyApp",
            vec![vec![module(
                "AlphaKit",
                vec!["-module-name", "AlphaKit"],
                vec![],
                vec![],
            )]],
            Watermark::Advance(Some("1.xcactivitylog".to_string())),
        );
        assert!(r1.changed);
        assert_eq!(r1.store.module_count(), 1);

        // Writer 2 (e.g. the build pipeline) ingests GammaKit. Even starting
        // from a *stale* empty in-memory snapshot, the locked re-read folds in
        // AlphaKit rather than clobbering it — the lost-update the lock exists
        // to prevent. `Watermark::Keep` leaves writer 1's watermark alone.
        let r2 = CompileStore::merge_save_locked_at(
            &store_path,
            br,
            "MyApp",
            vec![vec![module(
                "GammaKit",
                vec!["-module-name", "GammaKit"],
                vec![],
                vec![],
            )]],
            Watermark::Keep,
        );
        assert!(r2.changed);
        assert_eq!(r2.store.module_count(), 2, "both writers' modules survive");
        assert!(r2.store.module("AlphaKit").is_some());
        assert!(r2.store.module("GammaKit").is_some());
        assert_eq!(
            r2.store.last_ingested_log(),
            Some("1.xcactivitylog"),
            "Watermark::Keep preserved the poll watermark"
        );

        // The same is true on disk (not just in the returned in-memory store).
        let reloaded = CompileStore::read_json(CompileStore::new(br, "MyApp"), &store_path);
        assert_eq!(reloaded.module_count(), 2);
        assert_eq!(reloaded.last_ingested_log(), Some("1.xcactivitylog"));
    }

    #[test]
    fn merge_save_locked_empty_parse_is_no_change() {
        let dir = tmpdir();
        let store_path = dir.join("compile-store.json");
        let br = Path::new("/Users/x/DD");

        CompileStore::merge_save_locked_at(
            &store_path,
            br,
            "MyApp",
            vec![vec![module(
                "AlphaKit",
                vec!["-module-name", "AlphaKit"],
                vec![],
                vec![],
            )]],
            Watermark::Advance(Some("1.xcactivitylog".to_string())),
        );

        // An all-empty parse changes nothing: the store is preserved and the
        // on-disk watermark is not advanced (nothing is written).
        let r = CompileStore::merge_save_locked_at(
            &store_path,
            br,
            "MyApp",
            vec![vec![], vec![]],
            Watermark::Advance(Some("2.xcactivitylog".to_string())),
        );
        assert!(!r.changed);
        assert_eq!(r.store.module_count(), 1);
        assert_eq!(
            r.store.last_ingested_log(),
            Some("1.xcactivitylog"),
            "no-op parse must not advance the persisted watermark"
        );
    }

    #[test]
    fn merge_save_locked_degrades_when_lock_unavailable() {
        let dir = tmpdir();
        let store_path = dir.join("compile-store.json");
        let br = Path::new("/Users/x/DD");

        // Make the sidecar lock un-openable-as-a-file by parking a directory at
        // its path; `store_lock` then returns None and the merge proceeds
        // unlocked (fail-open) rather than panicking or aborting.
        std::fs::create_dir(store_lock_path(&store_path)).unwrap();

        let r = CompileStore::merge_save_locked_at(
            &store_path,
            br,
            "MyApp",
            vec![vec![module(
                "AlphaKit",
                vec!["-module-name", "AlphaKit"],
                vec![],
                vec![],
            )]],
            Watermark::Keep,
        );
        assert!(r.changed);
        assert_eq!(r.store.module_count(), 1);

        // The store was still persisted despite the missing lock.
        let reloaded = CompileStore::read_json(CompileStore::new(br, "MyApp"), &store_path);
        assert_eq!(reloaded.module_count(), 1);
    }
}
