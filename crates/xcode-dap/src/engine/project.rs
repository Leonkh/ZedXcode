//! Project discovery: which directory is the project root, which Xcode
//! container (`.xcworkspace` / `.xcodeproj`) in it to build, which generator
//! (Tuist, XcodeGen, a Makefile `project` target) writes that container, and
//! where the git checkout around it lives.
//!
//! The container search covers the root and the two levels below it, so a
//! React Native style `ios/MyApp.xcworkspace` is found from the repository
//! root. It never enters dependency and build folders, hidden folders,
//! bundles (a playground holds a workspace of its own) or another git
//! checkout (a submodule, a linked worktree kept inside this one).
//!
//! The ranking is fixed: a shallower container wins; in one directory a
//! workspace wins over a project; anything else that ties is ambiguous and
//! fails with the list of candidates, never with a silent pick. A Tuist or
//! XcodeGen manifest shallower than every container stands for the project it
//! has not generated yet, so a deeper checked-in container (a vendored
//! library, an example app) is not taken for the project.
//!
//! Everything here is synchronous (directory reads and one `git rev-parse`
//! with captured output); nothing writes to stdout.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Result};

/// Directories never searched: dependency checkouts, build output and tool
/// state, which carry containers of their own (`Pods/Pods.xcodeproj`,
/// `.swiftpm/xcode/package.xcworkspace`, ...). Every other hidden directory
/// is skipped as well.
const SKIPPED_DIRS: [&str; 8] = [
    "Pods",
    "node_modules",
    "build",
    "DerivedData",
    ".build",
    ".git",
    "Carthage",
    ".swiftpm",
];

/// Extensions of bundle directories, which are never entered: a
/// `*.playground` carries a `playground.xcworkspace`, and the others hold
/// resources or build products, never a project.
const BUNDLE_EXTENSIONS: [&str; 11] = [
    "playground",
    "app",
    "appex",
    "framework",
    "xcframework",
    "bundle",
    "xcassets",
    "docc",
    "xcarchive",
    "dSYM",
    "xctest",
];

/// How many directory levels below the root are searched.
const MAX_DEPTH: usize = 2;

/// What [`discover`] found for one project root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// The project root, absolute, in the form the caller gave it (not
    /// canonicalized; [`root_from_zed`] and [`root_from_terminal`] return
    /// canonical paths). Compare it with [`GitInfo`] paths only through
    /// [`Project::main_checkout_root`], which canonicalizes first.
    pub root: PathBuf,
    /// The container to build; `None` when there is none yet (a generated
    /// project before its first generation) or at all (a Swift package).
    pub container: Option<Container>,
    /// The generator that writes the container, when the project has one.
    pub generator: Option<Generator>,
    /// The git checkout around the root.
    pub git: GitInfo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ContainerKind {
    Workspace,
    Project,
}

/// An `.xcworkspace` or `.xcodeproj` bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Container {
    pub kind: ContainerKind,
    /// Absolute path (the root joined with [`Container::relative`]).
    pub path: PathBuf,
    /// Path relative to the project root, e.g. `ios/MyApp.xcworkspace`.
    pub relative: PathBuf,
}

/// The tools that generate an Xcode container from a manifest. The order is
/// the preference among generators in one directory: a Makefile `project`
/// target is the repository's own entry point and usually wraps Tuist or
/// XcodeGen together with the steps they need first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum GeneratorKind {
    Make,
    Tuist,
    XcodeGen,
}

impl GeneratorKind {
    /// The command that generates the container, run in [`Generator::dir`].
    pub fn command(self) -> &'static str {
        match self {
            GeneratorKind::Make => "make project",
            GeneratorKind::Tuist => "tuist generate --no-open",
            GeneratorKind::XcodeGen => "xcodegen generate",
        }
    }
}

/// A generator manifest found by [`discover`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generator {
    pub kind: GeneratorKind,
    /// The directory the command runs in (absolute).
    pub dir: PathBuf,
    /// The manifest, relative to the project root: `Project.swift` or
    /// `Workspace.swift` (Tuist), `project.yml` (XcodeGen) or `Makefile`.
    pub manifest: PathBuf,
}

impl Generator {
    pub fn command(&self) -> &'static str {
        self.kind.command()
    }
}

/// Where the git checkout around a directory lives. All paths are
/// canonical; everything is empty outside a git repository or without git.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitInfo {
    /// `git rev-parse --show-toplevel`.
    pub toplevel: Option<PathBuf>,
    /// The checkout is a linked worktree (`git worktree add`): its
    /// `--git-dir` differs from `--git-common-dir`.
    pub linked_worktree: bool,
    /// The repository's main checkout: the toplevel itself, or for a linked
    /// worktree the parent of `--git-common-dir`. `None` for a linked
    /// worktree of a bare repository (or any common dir not named `.git`),
    /// which has no main checkout.
    pub main_checkout: Option<PathBuf>,
}

/// Several containers tie for first place: [`discover`] refuses to guess.
/// A caller that knows the container (a `"workspace"` set in the scenario,
/// `--workspace`) uses [`with_container`] instead, which never ties and
/// still reports the generator and the git checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ambiguous {
    pub root: PathBuf,
    /// Every tied container, sorted by relative path.
    pub tied: Vec<Container>,
}

impl Ambiguous {
    /// `Found 2 Xcode workspaces: ios/MyApp.xcworkspace, demo/Demo.xcworkspace`
    /// (no trailing period), for callers that add their own way out.
    pub fn summary(&self) -> String {
        let workspaces = self
            .tied
            .iter()
            .filter(|c| c.kind == ContainerKind::Workspace)
            .count();
        let what = if workspaces == self.tied.len() {
            "workspaces"
        } else if workspaces == 0 {
            "projects"
        } else {
            "workspaces and projects"
        };
        let names: Vec<String> = self
            .tied
            .iter()
            .map(|c| c.relative.display().to_string())
            .collect();
        format!(
            "Found {} Xcode {what}: {}",
            self.tied.len(),
            names.join(", ")
        )
    }
}

impl fmt::Display for Ambiguous {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}. Set \"workspace\" in this project's Xcode scenario (.zed/debug.json), \
             or run \"xcode-dap setup\" here to choose one.",
            self.summary()
        )
    }
}

impl std::error::Error for Ambiguous {}

impl Project {
    /// The container, or an error that says why there is none: no Xcode
    /// project at all, or one that its generator has not written yet.
    pub fn require_container(&self) -> Result<&Container> {
        if let Some(container) = &self.container {
            return Ok(container);
        }
        let root = self.root.display();
        match &self.generator {
            None => bail!(
                "No Xcode project in {root}: no .xcworkspace, .xcodeproj, project.yml or \
                 Project.swift within two levels. ⌘R, ⌘B and the Xcode tasks work only in \
                 Xcode projects."
            ),
            Some(generator) => bail!(
                "No Xcode project in {root} yet: {} generates it. Run \"{}\" in {}, then try \
                 again.",
                generator.manifest.display(),
                generator.command(),
                generator.dir.display()
            ),
        }
    }

    /// For a linked worktree, the directory of the main checkout that
    /// matches the root (the root's place below the worktree's toplevel,
    /// taken in the main checkout); `None` for any other checkout. The root
    /// is canonicalized first, so a root given through a symlink (or as
    /// `/var/...` for `/private/var/...`) still maps.
    // The read-through to the main checkout's selection; it has no caller
    // until the selection resolver lands.
    #[allow(dead_code)]
    pub fn main_checkout_root(&self) -> Option<PathBuf> {
        if !self.git.linked_worktree {
            return None;
        }
        let toplevel = self.git.toplevel.as_deref()?;
        let main = self.git.main_checkout.as_deref()?;
        let below = canonical(&self.root)
            .strip_prefix(toplevel)
            .ok()?
            .to_owned();
        // `join("")` would add a trailing slash.
        Some(if below.as_os_str().is_empty() {
            main.to_path_buf()
        } else {
            main.join(below)
        })
    }
}

/// The project root for a process Zed starts: the debug adapter runs with
/// the worktree root as its cwd, and so does every task (`cwd` is
/// `$ZED_WORKTREE_ROOT`), so the root is that cwd (canonical, like the
/// [`GitInfo`] paths).
// The debug adapter's entry point; it has no caller until the selection
// resolver moves the adapter onto it.
#[allow(dead_code)]
pub fn root_from_zed(cwd: &Path) -> PathBuf {
    canonical(cwd)
}

/// The project root for a command typed in a terminal, which may run in any
/// folder of the project: the nearest directory, `cwd` itself included, that
/// holds `.zed/` or `buildServer.json`; else the git toplevel; else `cwd`.
/// Inside a git checkout the search stops at its toplevel, so a linked
/// worktree kept inside the main checkout is its own root, not the main
/// checkout's. The home directory is never the answer unless it is `cwd`
/// (a stray `~/.zed` or a home-directory repository is not a project).
/// The result is canonical, like the [`GitInfo`] paths.
// The CLI's entry point; it has no caller until the selection resolver moves
// the commands onto it.
#[allow(dead_code)]
pub fn root_from_terminal(cwd: &Path) -> PathBuf {
    terminal_root(cwd, home_dir().as_deref())
}

fn terminal_root(cwd: &Path, home: Option<&Path>) -> PathBuf {
    let cwd = canonical(cwd);
    let toplevel = git_info(&cwd)
        .toplevel
        .filter(|toplevel| Some(toplevel.as_path()) != home);
    for dir in cwd.ancestors() {
        if Some(dir) == home || dir.parent().is_none() {
            break;
        }
        if dir.join(".zed").is_dir() || dir.join("buildServer.json").is_file() {
            return dir.to_path_buf();
        }
        if Some(dir) == toplevel.as_deref() {
            break;
        }
    }
    toplevel.unwrap_or(cwd)
}

/// Find the container and the generator under `root` (see the module docs
/// for the search and the ranking) and the git checkout around it.
///
/// A home directory or the filesystem root is searched at its top level
/// only: neither is a project folder, and listing `~/Documents` or
/// `~/Library` makes macOS ask for folder access.
pub fn discover(root: &Path) -> std::result::Result<Project, Ambiguous> {
    let (root, found) = search(root);
    let Found {
        containers,
        generators,
    } = found;
    let best = containers.iter().map(|(depth, _)| *depth).min();
    // A Tuist or XcodeGen manifest shallower than every container generates
    // the project that belongs here (both write it next to the manifest), so
    // the deeper containers are not it. A Makefile target is not trusted for
    // this: it may write a workspace next to a deeper checked-in project
    // (CocoaPods in `ios/`).
    let ungenerated = best.is_none_or(|best| {
        generators
            .iter()
            .any(|(depth, g)| *depth < best && g.kind != GeneratorKind::Make)
    });
    let container = if ungenerated {
        None
    } else {
        rank_containers(&root, containers)?
    };
    let generator = pick_generator(generators, container.as_ref());
    let git = git_info(&root);
    Ok(Project {
        root,
        container,
        generator,
        git,
    })
}

/// The project under `root` with the container the caller already knows (a
/// `"workspace"` set in the scenario, `--workspace`; absolute or relative to
/// `root`): no container search, so no tie. The generator is the one on the
/// way to that container, as [`discover`] picks it.
// The resolver's path for an explicit workspace; it has no caller until the
// selection resolver lands.
#[allow(dead_code)]
pub fn with_container(root: &Path, container: &Path) -> Project {
    let (root, found) = search(root);
    let path = root.join(container); // an absolute `container` replaces `root`
    let relative = path
        .strip_prefix(&root)
        .map_or_else(|_| path.clone(), Path::to_path_buf);
    // The same split as `util::paths::container_flag`.
    let kind = if path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("xcodeproj"))
    {
        ContainerKind::Project
    } else {
        ContainerKind::Workspace
    };
    let container = Container {
        kind,
        path,
        relative,
    };
    let generator = pick_generator(found.generators, Some(&container));
    let git = git_info(&root);
    Project {
        root,
        container: Some(container),
        generator,
        git,
    }
}

/// The container under `root` for a command that takes `--workspace`
/// (`setup`, `select-scheme`): what [`discover`] finds, or its error with
/// `--workspace` as the way out.
pub fn container_for_cli(root: &Path) -> Result<Container> {
    let project = match discover(root) {
        Ok(project) => project,
        Err(tie) => bail!("{}. Pass --workspace to choose one.", tie.summary()),
    };
    project.require_container().cloned().map_err(|e| {
        anyhow!("{e} If the Xcode project is deeper or elsewhere, pass --workspace with its path.")
    })
}

/// Whether a Makefile line starts the `project` target: a `project:` rule,
/// not a `project:=` / `project::=` assignment, which defines no target
/// (make would fail with "No rule to make target 'project'").
pub fn is_project_rule(line: &str) -> bool {
    line.strip_prefix("project:")
        .is_some_and(|rest| !rest.starts_with('=') && !rest.starts_with(":="))
}

/// Whether `text` (a Makefile) defines the `project` target.
fn makefile_has_project_target(text: &str) -> bool {
    text.lines().any(is_project_rule)
}

/// Whether `text` (a `project.yml`) is an XcodeGen spec: it has a top-level
/// `targets:` key, or `include:` for a spec split over several files. The
/// `project.yml` files of CI and docs tools have neither.
fn is_xcodegen_spec(text: &str) -> bool {
    text.lines()
        .any(|line| line.starts_with("targets:") || line.starts_with("include:"))
}

/// [`GitInfo`] for `dir`, from one `git rev-parse` with captured output.
/// The `GIT_DIR`-style variables are cleared so the answer is about `dir`,
/// not about a repository the caller's environment points at.
pub fn git_info(dir: &Path) -> GitInfo {
    let output = Command::new("git")
        .args([
            "rev-parse",
            "--show-toplevel",
            "--git-dir",
            "--git-common-dir",
        ])
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_INDEX_FILE")
        .stdin(Stdio::null())
        .output();
    let Ok(output) = output else {
        return GitInfo::default(); // git is not installed
    };
    if !output.status.success() {
        return GitInfo::default(); // not a repository (or a bare one)
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let (Some(toplevel), Some(git_dir), Some(common_dir)) =
        (lines.next(), lines.next(), lines.next())
    else {
        return GitInfo::default();
    };
    // --git-dir and --git-common-dir may be relative to `dir`.
    let resolve = |p: &str| {
        let p = dir.join(p);
        p.canonicalize().unwrap_or(p)
    };
    let toplevel = resolve(toplevel);
    let common_dir = resolve(common_dir);
    let linked_worktree = resolve(git_dir) != common_dir;
    let main_checkout = if !linked_worktree {
        Some(toplevel.clone())
    } else if common_dir.file_name().is_some_and(|n| n == ".git") {
        common_dir.parent().map(Path::to_path_buf)
    } else {
        None
    };
    GitInfo {
        toplevel: Some(toplevel),
        linked_worktree,
        main_checkout,
    }
}

// ---------------------------------------------------------------------------
// search
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Found {
    containers: Vec<(usize, Container)>,
    generators: Vec<(usize, Generator)>,
}

/// `path` canonicalized, else made absolute, else as it is.
fn canonical(path: &Path) -> PathBuf {
    path.canonicalize()
        .or_else(|_| std::path::absolute(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

/// `$HOME`, canonical; `None` when unset or empty.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(|h| canonical(Path::new(&h)))
}

/// `root` made absolute and everything [`scan`] finds under it.
fn search(root: &Path) -> (PathBuf, Found) {
    let root = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
    let max_depth = if searches_below(&root) { MAX_DEPTH } else { 0 };
    let mut found = Found::default();
    scan(&root, Path::new(""), 0, max_depth, &mut found);
    (root, found)
}

fn searches_below(root: &Path) -> bool {
    let root = canonical(root);
    root.parent().is_some() && home_dir().as_deref() != Some(root.as_path())
}

/// Collect containers and generator manifests in `root/relative` (at
/// `depth`), descending into ordinary subdirectories while `depth` is below
/// `max_depth`. Container bundles are never entered, so the
/// `project.xcworkspace` inside every `.xcodeproj` is not a candidate, and
/// neither are the directories [`enters`] rules out. Symlinked directories
/// are not entered either; unreadable ones are skipped.
fn scan(root: &Path, relative: &Path, depth: usize, max_depth: usize, found: &mut Found) {
    // `root.join("")` would add a trailing slash to every printed path.
    let here = if relative.as_os_str().is_empty() {
        root.to_path_buf()
    } else {
        root.join(relative)
    };
    let Ok(entries) = fs::read_dir(&here) else {
        return;
    };
    // Only reachable when the root itself is an `.xcodeproj`.
    let inside_project = here
        .file_name()
        .is_some_and(|n| container_kind(&n.to_string_lossy()) == Some(ContainerKind::Project));
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if inside_project && name == "project.xcworkspace" {
            continue;
        }
        let rel = relative.join(entry.file_name());
        let path = here.join(entry.file_name());
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        // A symlink counts as what it points to, for containers and manifests.
        let is_dir = file_type.is_dir() || (file_type.is_symlink() && path.is_dir());
        if is_dir {
            if let Some(kind) = container_kind(&name) {
                found.containers.push((
                    depth,
                    Container {
                        kind,
                        path,
                        relative: rel,
                    },
                ));
            } else if depth < max_depth && file_type.is_dir() && enters(&name, &path) {
                scan(root, &rel, depth + 1, max_depth, found);
            }
        } else if let Some(kind) = generator_kind(&name, &path) {
            found.generators.push((
                depth,
                Generator {
                    kind,
                    dir: here.clone(),
                    manifest: rel,
                },
            ));
        }
    }
}

/// Whether the search descends into the directory `name` at `path`: not a
/// skipped or hidden directory (`.github`, a `.worktrees` folder), not a
/// bundle, and not another git checkout (a submodule, a nested repository or
/// a linked worktree, which hold a `.git` of their own).
fn enters(name: &str, path: &Path) -> bool {
    let bundle = name.rsplit_once('.').is_some_and(|(_, extension)| {
        BUNDLE_EXTENSIONS
            .iter()
            .any(|b| b.eq_ignore_ascii_case(extension))
    });
    !name.starts_with('.')
        && !SKIPPED_DIRS.contains(&name)
        && !bundle
        && fs::symlink_metadata(path.join(".git")).is_err()
}

fn container_kind(name: &str) -> Option<ContainerKind> {
    let (stem, extension) = name.rsplit_once('.')?;
    if stem.is_empty() {
        return None;
    }
    // Case-insensitive like the default macOS filesystem.
    if extension.eq_ignore_ascii_case("xcworkspace") {
        Some(ContainerKind::Workspace)
    } else if extension.eq_ignore_ascii_case("xcodeproj") {
        Some(ContainerKind::Project)
    } else {
        None
    }
}

/// The generator a file marks, if any. A `Project.swift` or `Workspace.swift`
/// counts only as a Tuist manifest (it uses `ProjectDescription`), so an
/// app's own `Project.swift` source file is not mistaken for one; a
/// `project.yml` only as an XcodeGen spec (see [`is_xcodegen_spec`]).
fn generator_kind(name: &str, path: &Path) -> Option<GeneratorKind> {
    match name {
        "project.yml" => fs::read_to_string(path)
            .is_ok_and(|text| is_xcodegen_spec(&text))
            .then_some(GeneratorKind::XcodeGen),
        "Project.swift" | "Workspace.swift" => fs::read_to_string(path)
            .is_ok_and(|text| text.contains("ProjectDescription"))
            .then_some(GeneratorKind::Tuist),
        "Makefile" => fs::read_to_string(path)
            .is_ok_and(|text| makefile_has_project_target(&text))
            .then_some(GeneratorKind::Make),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// ranking
// ---------------------------------------------------------------------------

/// The shallowest containers compete; in each directory its workspaces stand
/// for it when it has any, else its projects. One survivor is the answer,
/// several are [`Ambiguous`].
fn rank_containers(
    root: &Path,
    containers: Vec<(usize, Container)>,
) -> std::result::Result<Option<Container>, Ambiguous> {
    let Some(best) = containers.iter().map(|(depth, _)| *depth).min() else {
        return Ok(None);
    };
    let shallowest: Vec<Container> = containers
        .into_iter()
        .filter(|(depth, _)| *depth == best)
        .map(|(_, c)| c)
        .collect();
    let has_workspace = |dir: Option<&Path>| {
        shallowest
            .iter()
            .any(|c| c.kind == ContainerKind::Workspace && c.relative.parent() == dir)
    };
    let mut tied: Vec<Container> = shallowest
        .iter()
        .filter(|c| c.kind == ContainerKind::Workspace || !has_workspace(c.relative.parent()))
        .cloned()
        .collect();
    tied.sort_by(|a, b| a.relative.cmp(&b.relative));
    if tied.len() > 1 {
        return Err(Ambiguous {
            root: root.to_path_buf(),
            tied,
        });
    }
    Ok(tied.pop())
}

/// With a container, its generator is on the way from the root to the
/// container's directory (a manifest elsewhere generates something else);
/// without one, any manifest qualifies. The shallowest wins, then the
/// [`GeneratorKind`] order, then the path, so the choice is stable.
fn pick_generator(
    generators: Vec<(usize, Generator)>,
    container: Option<&Container>,
) -> Option<Generator> {
    let container_dir = container.and_then(|c| c.path.parent());
    generators
        .into_iter()
        .filter(|(_, g)| container_dir.is_none_or(|dir| dir.starts_with(&g.dir)))
        .min_by(|(da, a), (db, b)| (da, a.kind, &a.manifest).cmp(&(db, b.kind, &b.manifest)))
        .map(|(_, g)| g)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    /// A fresh directory, canonical (on macOS the temp dir is reached
    /// through the `/var` symlink), so it compares with the canonical paths
    /// of the root functions and [`GitInfo`].
    fn sandbox() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zedxcode-discover-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    /// The smallest `project.yml` that counts as an XcodeGen spec.
    const XCODEGEN_SPEC: &str = "name: MyApp\ntargets:\n  MyApp:\n    type: application\n";

    fn mkdirs(root: &Path, dirs: &[&str]) {
        for d in dirs {
            fs::create_dir_all(root.join(d)).unwrap();
        }
    }

    /// Copy `tests/fixtures/layouts/<name>` into a fresh sandbox, leaving out
    /// what git ignores there: per-user Xcode state and the project that the
    /// ungenerated layout's generate.sh writes (a local run may have left it).
    fn layout(name: &str) -> PathBuf {
        fn copy(from: &Path, to: &Path) {
            fs::create_dir_all(to).unwrap();
            for entry in fs::read_dir(from).unwrap() {
                let entry = entry.unwrap();
                let name = entry.file_name();
                if name == "xcuserdata" || name.to_string_lossy().ends_with(".xcuserstate") {
                    continue;
                }
                if entry.file_type().unwrap().is_dir() {
                    copy(&entry.path(), &to.join(&name));
                } else {
                    fs::copy(entry.path(), to.join(&name)).unwrap();
                }
            }
        }
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/layouts")
            .join(name);
        let dir = sandbox().join(name);
        copy(&source, &dir);
        if name == "xcodegen-ungenerated" {
            let _ = fs::remove_dir_all(dir.join("MyApp.xcodeproj"));
        }
        dir
    }

    fn container_of(root: &Path) -> Option<String> {
        discover(root)
            .unwrap()
            .container
            .map(|c| c.relative.display().to_string())
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

    #[test]
    fn xcworkspace_layout_finds_the_workspace() {
        let root = layout("xcworkspace");
        let project = discover(&root).unwrap();
        let container = project.container.unwrap();
        assert_eq!(container.kind, ContainerKind::Workspace);
        assert_eq!(container.relative, Path::new("MyApp.xcworkspace"));
        assert_eq!(container.path, root.join("MyApp.xcworkspace"));
        assert_eq!(project.generator, None);
    }

    #[test]
    fn nested_ios_layout_finds_the_workspace_below_the_root() {
        let root = layout("nested-ios");
        assert_eq!(
            container_of(&root).as_deref(),
            Some("ios/MyApp.xcworkspace")
        );
    }

    #[test]
    fn xcodegen_layout_has_a_generator_and_no_container() {
        let root = layout("xcodegen-ungenerated");
        let project = discover(&root).unwrap();
        assert_eq!(project.container, None);
        let generator = project.generator.clone().unwrap();
        assert_eq!(generator.kind, GeneratorKind::XcodeGen);
        assert_eq!(generator.command(), "xcodegen generate");
        assert_eq!(generator.dir, root);
        assert_eq!(generator.manifest, Path::new("project.yml"));
        let err = project.require_container().unwrap_err().to_string();
        assert_eq!(
            err,
            format!(
                "No Xcode project in {0} yet: project.yml generates it. Run \"xcodegen \
                 generate\" in {0}, then try again.",
                root.display()
            )
        );

        // After generation, the generated project is the container.
        mkdirs(&root, &["MyApp.xcodeproj/project.xcworkspace"]);
        let project = discover(&root).unwrap();
        assert_eq!(
            project.container.unwrap().relative,
            Path::new("MyApp.xcodeproj")
        );
        assert_eq!(project.generator.unwrap().kind, GeneratorKind::XcodeGen);
    }

    #[test]
    fn tuist_layout_finds_the_workspace_and_tuist() {
        let root = layout("tuist-shaped");
        let project = discover(&root).unwrap();
        assert_eq!(
            project.container.unwrap().relative,
            Path::new("MyApp.xcworkspace")
        );
        let generator = project.generator.unwrap();
        assert_eq!(generator.kind, GeneratorKind::Tuist);
        assert_eq!(generator.command(), "tuist generate --no-open");
        assert_eq!(generator.manifest, Path::new("Project.swift"));
    }

    #[test]
    fn spm_only_layout_has_nothing() {
        let root = layout("spm-only");
        let project = discover(&root).unwrap();
        assert_eq!(project.container, None);
        assert_eq!(project.generator, None);
        let err = project.require_container().unwrap_err().to_string();
        assert_eq!(
            err,
            format!(
                "No Xcode project in {}: no .xcworkspace, .xcodeproj, project.yml or \
                 Project.swift within two levels. ⌘R, ⌘B and the Xcode tasks work only in \
                 Xcode projects.",
                root.display()
            )
        );
    }

    #[test]
    fn local_package_layout_finds_the_workspace() {
        let root = layout("local-package");
        let project = discover(&root).unwrap();
        assert_eq!(
            project.container.unwrap().relative,
            Path::new("MyApp.xcworkspace")
        );
        assert_eq!(project.generator, None);
    }

    #[test]
    fn two_containers_at_one_rank_are_ambiguous() {
        let root = sandbox();
        mkdirs(
            &root,
            &[
                "ios/MyApp.xcworkspace",
                "ios/MyApp.xcodeproj",
                "demo/Demo.xcworkspace",
            ],
        );
        let err = discover(&root).unwrap_err();
        assert_eq!(err.root, root);
        assert_eq!(
            err.to_string(),
            "Found 2 Xcode workspaces: demo/Demo.xcworkspace, ios/MyApp.xcworkspace. Set \
             \"workspace\" in this project's Xcode scenario (.zed/debug.json), or run \
             \"xcode-dap setup\" here to choose one."
        );

        // Every tied container is listed, of either kind.
        mkdirs(&root, &["macos/MyMacApp.xcodeproj"]);
        let err = discover(&root).unwrap_err();
        assert_eq!(
            err.summary(),
            "Found 3 Xcode workspaces and projects: demo/Demo.xcworkspace, \
             ios/MyApp.xcworkspace, macos/MyMacApp.xcodeproj"
        );

        // Two of one kind in one directory tie as well.
        let root = sandbox();
        mkdirs(&root, &["MyApp.xcodeproj", "MyAppTests.xcodeproj"]);
        assert_eq!(
            discover(&root).unwrap_err().summary(),
            "Found 2 Xcode projects: MyApp.xcodeproj, MyAppTests.xcodeproj"
        );
    }

    #[test]
    fn shallower_wins_then_workspace_over_project() {
        let root = sandbox();
        mkdirs(&root, &["Example/Example.xcworkspace", "MyApp.xcodeproj"]);
        assert_eq!(container_of(&root).as_deref(), Some("MyApp.xcodeproj"));
        mkdirs(&root, &["MyApp.xcworkspace"]);
        assert_eq!(container_of(&root).as_deref(), Some("MyApp.xcworkspace"));
    }

    #[test]
    fn skipped_and_deep_directories_are_not_searched() {
        let root = sandbox();
        mkdirs(
            &root,
            &[
                "Pods/Pods.xcodeproj",
                "node_modules/some-lib/ios/SomeLib.xcodeproj",
                "build/Out.xcodeproj",
                "DerivedData/Out.xcworkspace",
                ".build/Out.xcodeproj",
                ".git/Out.xcodeproj",
                "Carthage/Checkouts/Lib.xcodeproj",
                ".swiftpm/xcode/package.xcworkspace",
                "a/b/c/TooDeep.xcodeproj",
            ],
        );
        assert_eq!(container_of(&root), None);
        // Not even when the root is an .xcodeproj itself.
        let bundle = sandbox().join("MyApp.xcodeproj");
        mkdirs(&bundle, &["project.xcworkspace"]);
        assert_eq!(container_of(&bundle), None);
        // Two levels below the root still count.
        mkdirs(&root, &["apps/ios/MyApp.xcodeproj/project.xcworkspace"]);
        assert_eq!(
            container_of(&root).as_deref(),
            Some("apps/ios/MyApp.xcodeproj")
        );
    }

    #[test]
    fn playgrounds_and_other_bundles_are_not_searched() {
        // A Swift package with playgrounds is still not an Xcode project.
        let root = layout("spm-only");
        mkdirs(
            &root,
            &[
                "MyKit.playground/playground.xcworkspace",
                "Playgrounds/Tour.playground/playground.xcworkspace",
                "Docs/MyKit.docc",
                "Frameworks/Lib.xcframework/ios-arm64/Lib.framework",
            ],
        );
        fs::write(root.join("Docs/MyKit.docc/project.yml"), XCODEGEN_SPEC).unwrap();
        let project = discover(&root).unwrap();
        assert_eq!(project.container, None);
        assert_eq!(project.generator, None);

        // A playground next to a nested container makes no tie.
        let root = layout("nested-ios");
        mkdirs(&root, &["Docs.playground/playground.xcworkspace"]);
        assert_eq!(
            container_of(&root).as_deref(),
            Some("ios/MyApp.xcworkspace")
        );
    }

    #[test]
    fn hidden_folders_and_other_checkouts_are_not_searched() {
        let root = layout("spm-only");
        // A GitHub workflow named project.yml is no XcodeGen spec, and
        // hidden folders are not searched anyway.
        mkdirs(
            &root,
            &[".github/workflows", ".worktrees/feature/MyApp.xcodeproj"],
        );
        fs::write(root.join(".github/workflows/project.yml"), XCODEGEN_SPEC).unwrap();
        // A submodule or a linked worktree holds a .git file of its own.
        mkdirs(
            &root,
            &["Vendor/Lib/Lib.xcodeproj", "wt/feature/MyApp.xcodeproj"],
        );
        fs::write(
            root.join("Vendor/Lib/.git"),
            "gitdir: ../../.git/modules/Lib\n",
        )
        .unwrap();
        fs::write(
            root.join("wt/feature/.git"),
            "gitdir: ../../.git/worktrees/feature\n",
        )
        .unwrap();
        let project = discover(&root).unwrap();
        assert_eq!(project.container, None);
        assert_eq!(project.generator, None);
    }

    #[test]
    fn project_yml_counts_only_as_an_xcodegen_spec() {
        assert!(is_xcodegen_spec(XCODEGEN_SPEC));
        assert!(is_xcodegen_spec("name: MyApp\ninclude:\n  - base.yml\n"));
        assert!(!is_xcodegen_spec("name: Docs\nnav:\n  - targets: x\n"));
        assert!(!is_xcodegen_spec(
            "name: Add to project\non: issues\njobs: {}\n"
        ));
        let root = layout("spm-only");
        mkdirs(&root, &["docs"]);
        fs::write(root.join("docs/project.yml"), "name: Docs\nnav: []\n").unwrap();
        assert_eq!(discover(&root).unwrap().generator, None);
    }

    #[test]
    fn a_shallower_manifest_outranks_a_deeper_checked_in_container() {
        // An ungenerated XcodeGen project with a vendored library.
        let root = sandbox();
        mkdirs(&root, &["Vendor/Lib/Lib.xcodeproj"]);
        fs::write(root.join("project.yml"), XCODEGEN_SPEC).unwrap();
        let project = discover(&root).unwrap();
        assert_eq!(project.container, None);
        assert_eq!(project.generator.unwrap().kind, GeneratorKind::XcodeGen);
        // Also when the deeper containers tie.
        mkdirs(&root, &["Vendor/Other/Other.xcodeproj"]);
        assert_eq!(discover(&root).unwrap().container, None);
        // Generated, the root project wins.
        mkdirs(&root, &["MyApp.xcodeproj"]);
        assert_eq!(container_of(&root).as_deref(), Some("MyApp.xcodeproj"));

        // An ungenerated Tuist workspace with an example app.
        let root = sandbox();
        mkdirs(&root, &["Example/Example.xcodeproj"]);
        fs::write(root.join("Workspace.swift"), "import ProjectDescription\n").unwrap();
        let project = discover(&root).unwrap();
        assert_eq!(project.container, None);
        assert_eq!(
            project.generator.clone().unwrap().kind,
            GeneratorKind::Tuist
        );
        // A Makefile `project` target beside it is the entry point to run.
        fs::write(root.join("Makefile"), "project:\n\ttuist generate\n").unwrap();
        let project = discover(&root).unwrap();
        assert_eq!(project.container, None);
        assert_eq!(project.generator.unwrap().kind, GeneratorKind::Make);

        // A Makefile alone never outranks a container: its target may write
        // a workspace next to a deeper checked-in project.
        let root = sandbox();
        mkdirs(&root, &["ios/MyApp.xcodeproj"]);
        fs::write(root.join("Makefile"), "project:\n\tcd ios && pod install\n").unwrap();
        let project = discover(&root).unwrap();
        assert_eq!(
            project.container.unwrap().relative,
            Path::new("ios/MyApp.xcodeproj")
        );
        assert_eq!(project.generator.unwrap().kind, GeneratorKind::Make);
    }

    #[test]
    fn with_container_skips_the_search_and_never_ties() {
        let root = sandbox();
        mkdirs(&root, &["ios/MyApp.xcworkspace", "demo/Demo.xcodeproj"]);
        fs::write(root.join("ios/project.yml"), XCODEGEN_SPEC).unwrap();
        fs::write(root.join("demo/project.yml"), XCODEGEN_SPEC).unwrap();
        assert!(discover(&root).is_err());

        let project = with_container(&root, Path::new("demo/Demo.xcodeproj"));
        let container = project.container.clone().unwrap();
        assert_eq!(container.kind, ContainerKind::Project);
        assert_eq!(container.path, root.join("demo/Demo.xcodeproj"));
        assert_eq!(container.relative, Path::new("demo/Demo.xcodeproj"));
        let generator = project.generator.unwrap();
        assert_eq!(generator.manifest, Path::new("demo/project.yml"));
        assert_eq!(project.git, GitInfo::default());

        // An absolute path works the same; a workspace is a workspace.
        let project = with_container(&root, &root.join("ios/MyApp.xcworkspace"));
        let container = project.container.unwrap();
        assert_eq!(container.kind, ContainerKind::Workspace);
        assert_eq!(container.relative, Path::new("ios/MyApp.xcworkspace"));
        assert_eq!(
            project.generator.unwrap().manifest,
            Path::new("ios/project.yml")
        );
    }

    #[test]
    fn container_for_cli_points_at_the_workspace_flag() {
        let root = layout("spm-only");
        assert_eq!(
            container_for_cli(&root).unwrap_err().to_string(),
            format!(
                "No Xcode project in {}: no .xcworkspace, .xcodeproj, project.yml or \
                 Project.swift within two levels. ⌘R, ⌘B and the Xcode tasks work only in \
                 Xcode projects. If the Xcode project is deeper or elsewhere, pass \
                 --workspace with its path.",
                root.display()
            )
        );
        mkdirs(&root, &["ios/MyApp.xcworkspace", "demo/Demo.xcworkspace"]);
        assert_eq!(
            container_for_cli(&root).unwrap_err().to_string(),
            "Found 2 Xcode workspaces: demo/Demo.xcworkspace, ios/MyApp.xcworkspace. Pass \
             --workspace to choose one."
        );
        fs::remove_dir_all(root.join("demo")).unwrap();
        assert_eq!(
            container_for_cli(&root).unwrap().path,
            root.join("ios/MyApp.xcworkspace")
        );
    }

    #[test]
    fn generators_rank_by_depth_and_follow_the_container() {
        let root = sandbox();
        mkdirs(&root, &["ios", "tools"]);
        fs::write(root.join("ios/project.yml"), XCODEGEN_SPEC).unwrap();
        // An app's own Project.swift source is not a Tuist manifest.
        fs::write(root.join("tools/Project.swift"), "struct Project {}\n").unwrap();
        // A Makefile without a `project` target is no generator.
        fs::write(root.join("Makefile"), "project:=MyApp\nbuild:\n\techo hi\n").unwrap();
        let generator = discover(&root).unwrap().generator.unwrap();
        assert_eq!(generator.kind, GeneratorKind::XcodeGen);
        assert_eq!(generator.dir, root.join("ios"));
        assert_eq!(generator.manifest, Path::new("ios/project.yml"));

        // A root Makefile `project` target is shallower and wins.
        fs::write(root.join("Makefile"), "project:\n\tcd ios && xcodegen\n").unwrap();
        let generator = discover(&root).unwrap().generator.unwrap();
        assert_eq!(generator.kind, GeneratorKind::Make);
        assert_eq!(generator.command(), "make project");
        assert_eq!(generator.dir, root);

        // With a container, only manifests on its path count.
        let root = sandbox();
        mkdirs(&root, &["ios/MyApp.xcodeproj", "macos"]);
        fs::write(
            root.join("macos/Project.swift"),
            "import ProjectDescription\n",
        )
        .unwrap();
        assert_eq!(discover(&root).unwrap().generator, None);
        fs::write(root.join("ios/project.yml"), XCODEGEN_SPEC).unwrap();
        assert_eq!(
            discover(&root).unwrap().generator.unwrap().kind,
            GeneratorKind::XcodeGen
        );
    }

    #[test]
    fn makefile_project_target_detection() {
        assert!(!makefile_has_project_target("build:\n\techo hi\n"));
        assert!(makefile_has_project_target(
            "project:\n\ttuist generate\n\nbuild:\n\techo hi\n"
        ));
        assert!(makefile_has_project_target(
            "project: deps\n\ttuist generate\n"
        ));
        // `project:=` / `project::=` are variable assignments, not targets.
        assert!(!makefile_has_project_target("project:=MyApp\n"));
        assert!(!makefile_has_project_target("project::=MyApp\n"));
    }

    #[test]
    fn terminal_root_prefers_markers_then_git_then_cwd() {
        // No marker and no repository: the cwd itself.
        let plain = sandbox();
        mkdirs(&plain, &["Sources/App"]);
        let cwd = plain.join("Sources/App");
        assert_eq!(root_from_terminal(&cwd), cwd);

        // The git toplevel.
        let repo = sandbox();
        mkdirs(&repo, &["MyApp/Views"]);
        git(&repo, &["init", "-q"]);
        let toplevel = repo.canonicalize().unwrap();
        assert_eq!(root_from_terminal(&repo.join("MyApp/Views")), toplevel);

        // The nearest .zed/ or buildServer.json, before the toplevel.
        mkdirs(&repo, &["MyApp/.zed"]);
        assert_eq!(
            root_from_terminal(&repo.join("MyApp/Views")),
            repo.join("MyApp")
        );
        fs::write(repo.join("MyApp/Views/buildServer.json"), "{}").unwrap();
        assert_eq!(
            root_from_terminal(&repo.join("MyApp/Views")),
            repo.join("MyApp/Views")
        );

        // A marker above the toplevel is another project's.
        let outer = sandbox();
        mkdirs(&outer, &[".zed", "MyApp/Sources"]);
        git(&outer.join("MyApp"), &["init", "-q"]);
        assert_eq!(
            root_from_terminal(&outer.join("MyApp/Sources")),
            outer.join("MyApp")
        );

        // From Zed: the cwd, canonical.
        assert_eq!(root_from_zed(&repo.join("MyApp")), repo.join("MyApp"));
        let link = sandbox().join("link");
        std::os::unix::fs::symlink(repo.join("MyApp"), &link).unwrap();
        assert_eq!(root_from_zed(&link), repo.join("MyApp"));
    }

    #[test]
    fn terminal_root_is_never_the_home_directory() {
        // A stray ~/.zed is not a project.
        let home = sandbox();
        mkdirs(
            &home,
            &[".zed", "scratch/notes", "MyApp/.zed", "MyApp/Sources"],
        );
        let cwd = home.join("scratch/notes");
        assert_eq!(terminal_root(&cwd, Some(&home)), cwd);
        // A project below it still is.
        assert_eq!(
            terminal_root(&home.join("MyApp/Sources"), Some(&home)),
            home.join("MyApp")
        );
        // Nor is a home directory that is a git repository (dotfiles).
        let home = sandbox();
        mkdirs(&home, &["scratch/notes"]);
        git(&home, &["init", "-q"]);
        let cwd = home.join("scratch/notes");
        assert_eq!(terminal_root(&cwd, Some(&home)), cwd);
        assert_eq!(terminal_root(&home, Some(&home)), home);
    }

    #[test]
    fn terminal_root_of_a_worktree_inside_the_main_checkout_is_the_worktree() {
        let main = sandbox().join("MyApp");
        mkdirs(&main, &[".zed"]);
        git(&main, &["init", "-q"]);
        git(&main, &["commit", "-q", "--allow-empty", "-m", "init"]);
        git(
            &main,
            &["worktree", "add", "-q", "--detach", ".worktrees/feature"],
        );
        let worktree = main.join(".worktrees/feature");
        mkdirs(&worktree, &["Sources"]);
        assert_eq!(root_from_terminal(&worktree.join("Sources")), worktree);
        assert_eq!(root_from_terminal(&main), main);
    }

    #[test]
    fn linked_worktree_knows_its_main_checkout() {
        let base = sandbox();
        let main = base.join("MyApp");
        mkdirs(&main, &["ios"]);
        git(&main, &["init", "-q"]);
        git(&main, &["commit", "-q", "--allow-empty", "-m", "init"]);
        git(
            &main,
            &["worktree", "add", "-q", "--detach", "../MyApp-feature"],
        );
        let main_canonical = main.canonicalize().unwrap();
        let worktree = base.join("MyApp-feature").canonicalize().unwrap();

        let info = git_info(&main.join("ios"));
        assert_eq!(info.toplevel.as_deref(), Some(main_canonical.as_path()));
        assert!(!info.linked_worktree);
        assert_eq!(
            info.main_checkout.as_deref(),
            Some(main_canonical.as_path())
        );

        let project = discover(&worktree).unwrap();
        assert_eq!(project.git.toplevel.as_deref(), Some(worktree.as_path()));
        assert!(project.git.linked_worktree);
        assert_eq!(
            project.git.main_checkout.as_deref(),
            Some(main_canonical.as_path())
        );

        // The worktree's root maps onto the main checkout, also through a
        // symlink (the form a macOS temp dir is often given in).
        assert_eq!(project.main_checkout_root(), Some(main_canonical.clone()));
        let link = base.join("link");
        std::os::unix::fs::symlink(&worktree, &link).unwrap();
        mkdirs(&worktree, &["ios"]);
        let project = discover(&link.join("ios")).unwrap();
        assert_eq!(project.root, link.join("ios"));
        assert_eq!(
            project.main_checkout_root(),
            Some(main_canonical.join("ios"))
        );
        // The main checkout itself reads through to nothing.
        assert_eq!(discover(&main).unwrap().main_checkout_root(), None);

        // Outside a repository there is nothing to report.
        assert_eq!(git_info(&sandbox()), GitInfo::default());
    }

    #[test]
    fn a_worktree_inside_the_checkout_is_not_searched() {
        // The main checkout is not generated yet; a worktree kept inside it
        // has been. Its project is not the main checkout's.
        let main = sandbox().join("MyApp");
        mkdirs(&main, &["MyApp"]);
        fs::write(main.join("Makefile"), "project:\n\t./generate.sh\n").unwrap();
        git(&main, &["init", "-q"]);
        git(&main, &["add", "Makefile"]);
        git(&main, &["commit", "-q", "-m", "init"]);
        git(&main, &["worktree", "add", "-q", "--detach", "wt/feature"]);
        mkdirs(&main, &["wt/feature/MyApp.xcodeproj"]);
        let project = discover(&main).unwrap();
        assert_eq!(project.container, None);
        assert_eq!(project.generator.unwrap().kind, GeneratorKind::Make);
        // In the worktree itself, its project is found.
        assert_eq!(
            container_of(&main.join("wt/feature")).as_deref(),
            Some("MyApp.xcodeproj")
        );
    }
}
