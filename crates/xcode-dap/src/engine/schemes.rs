//! The schemes and build configurations of an Xcode container, from
//! `xcodebuild -list -json`, cached per container in
//! `~/.zedxcode/cache/scheme-list-<hash>.json`.
//!
//! `xcodebuild -list` takes ten seconds and more on a large workspace (some
//! have hundreds of schemes), and its answer changes only when the project
//! does. The cache key covers the container and its contents file, a
//! project's `project.pbxproj`, and every `xcschemes/` directory: the
//! container's own and those of the projects a workspace references, shared
//! and per user. A scheme added in Xcode therefore shows up without a
//! regenerated container. Nothing else in `xcshareddata/` or `xcuserdata/`
//! counts: Xcode saves its window state there all the time. Deleting
//! `~/.zedxcode/cache` clears the cache.
//!
//! A workspace's `-list` names only schemes; its configurations stay empty
//! here and are not validated (they are read from the scheme's project
//! later).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::process::Command;

use crate::engine::pipeline::OutputSink;
use crate::setup::project::shell_quote;
use crate::util::hash::fnv1a64;
use crate::util::paths::{container_flag, zedxcode_home};

/// Deadline for one `xcodebuild -list`: it answers in seconds, unless Swift
/// package resolution (which it may run first) is slow or stuck.
const LIST_DEADLINE: Duration = Duration::from_secs(120);

/// What `xcodebuild -list -json` reports for a container.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemeList {
    pub schemes: Vec<String>,
    /// A project's build configurations; empty for a workspace.
    pub configurations: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct Cache {
    key: u64,
    list: SchemeList,
}

/// The schemes and configurations of `container` (absolute), from the cache
/// when the container has not changed, else from `xcodebuild -list` (one
/// line to `sink` first: it can take a while).
pub async fn list(container: &Path, sink: &dyn OutputSink) -> Result<SchemeList> {
    let key = cache_key(container).map_err(|_| {
        anyhow!(
            "{} does not exist; generate the project first",
            container.display()
        )
    })?;
    let cache_file = cache_file(container);
    if let Some(cache) = cache_file
        .as_ref()
        .and_then(|f| fs::read(f).ok())
        .and_then(|bytes| serde_json::from_slice::<Cache>(&bytes).ok())
        .filter(|cache| cache.key == key)
    {
        return Ok(cache.list);
    }

    let name = container
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| container.display().to_string());
    sink.line(
        "console",
        &format!(
            "Reading the schemes of {name} (xcodebuild -list; slow the first time, then cached)..."
        ),
    );
    let flag = container_flag(container);
    let mut cmd = Command::new("xcodebuild");
    cmd.args(["-list", "-json", flag])
        .arg(container)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let out = match tokio::time::timeout(LIST_DEADLINE, cmd.output()).await {
        Ok(out) => out.context("failed to run `xcodebuild -list` — is Xcode installed?")?,
        Err(_) => bail!(
            "xcodebuild -list did not finish in {} s; Swift package resolution may be slow or \
             stuck: run \"xcodebuild -list {flag} {}\" in a terminal to see where it stops, \
             then retry.",
            LIST_DEADLINE.as_secs(),
            shell_quote(&container.to_string_lossy())
        ),
    };
    if !out.status.success() {
        bail!(
            "`xcodebuild -list` failed for {}:\n{}",
            container.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let list = parse(&out.stdout).with_context(|| {
        format!(
            "unexpected `xcodebuild -list` output for {}",
            container.display()
        )
    })?;
    if let Some(file) = cache_file {
        // Best-effort: a cache that cannot be written only costs time.
        let _ = serde_json::to_vec(&Cache {
            key,
            list: list.clone(),
        })
        .map(|bytes| fs::write(file, bytes));
    }
    Ok(list)
}

/// `xcodebuild -list -json` stdout -> the scheme list. A container without
/// schemes is an error: there is nothing to build.
///
/// When xcodebuild resolves Swift packages first, it prints its progress
/// ("Resolve Package Graph", the resolved packages) to stdout before the
/// JSON, so the JSON is read from the first line that starts with `{`.
pub fn parse(bytes: &[u8]) -> Result<SchemeList> {
    let start = bytes
        .iter()
        .enumerate()
        .position(|(i, b)| *b == b'{' && (i == 0 || bytes[i - 1] == b'\n'))
        .context("output is not JSON")?;
    let v: Value = serde_json::Deserializer::from_slice(&bytes[start..])
        .into_iter::<Value>()
        .next()
        .context("output is not JSON")?
        .context("output is not JSON")?;
    let container = v
        .get("workspace")
        .or_else(|| v.get("project"))
        .context("no `workspace` or `project` in output")?;
    let strings = |key: &str| -> Vec<String> {
        container
            .get(key)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    let schemes = strings("schemes");
    if schemes.is_empty() {
        bail!("the scheme list is empty");
    }
    Ok(SchemeList {
        schemes,
        configurations: strings("configurations"),
    })
}

/// `~/.zedxcode/cache/scheme-list-<hash of the container path>.json`;
/// `None` without a home directory. (0.1's picker kept a different format
/// in `schemes-<hash>.json`; a separate name keeps the two from replacing
/// each other's file.)
fn cache_file(container: &Path) -> Option<PathBuf> {
    let dir = zedxcode_home().ok()?.join("cache");
    fs::create_dir_all(&dir).ok()?;
    Some(dir.join(format!(
        "scheme-list-{:016x}.json",
        fnv1a64(container.as_os_str().as_encoded_bytes())
    )))
}

/// A hash over the container path and the mtimes of everything whose change
/// can change the scheme list (see the module docs). Errors when the
/// container is missing.
fn cache_key(container: &Path) -> Result<u64> {
    fs::metadata(container)?;
    let mut text = String::new();
    for path in key_paths(container) {
        let mtime = fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos());
        text.push_str(&format!("{}\0{mtime}\n", path.display()));
    }
    Ok(fnv1a64(text.as_bytes()))
}

/// The files and directories [`cache_key`] stats: for the container and,
/// for a workspace, every project it references: the bundle, its contents
/// file or `project.pbxproj`, `xcshareddata/xcschemes` and each user's
/// `xcuserdata/<user>.xcuserdatad/xcschemes`. Paths that do not exist are
/// listed too, so their creation changes the key (a new user folder adds a
/// path). The folders around them are left out on purpose: Xcode rewrites
/// `UserInterfaceState.xcuserstate` in a user folder while it is open, and
/// every such save would rerun `xcodebuild -list`.
fn key_paths(container: &Path) -> Vec<PathBuf> {
    let mut bundles = vec![container.to_path_buf()];
    let is_project = container_flag(container) == "-project";
    if !is_project {
        bundles.extend(workspace_projects(container));
    }
    let mut paths = Vec::new();
    for bundle in bundles {
        paths.push(bundle.clone());
        paths.push(bundle.join("contents.xcworkspacedata"));
        paths.push(bundle.join("project.pbxproj"));
        paths.push(bundle.join("xcshareddata").join("xcschemes"));
        let mut users: Vec<PathBuf> = fs::read_dir(bundle.join("xcuserdata"))
            .map(|entries| entries.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        users.sort();
        paths.extend(users.into_iter().map(|user| user.join("xcschemes")));
    }
    paths
}

/// The `.xcodeproj` bundles a workspace references
/// (`<FileRef location = "group:MyApp.xcodeproj">`). A `group:` location is
/// relative to the enclosing `<Group>` (a folder in the workspace's
/// navigator), a `container:` location to the workspace's folder, and an
/// `absolute:` one is a full path. Read without an XML parser: only the
/// `Group` and `FileRef` tags and their `location` attributes matter, and a
/// reference that cannot be read is skipped (the key then just misses that
/// project's schemes).
fn workspace_projects(workspace: &Path) -> Vec<PathBuf> {
    let Ok(text) = fs::read_to_string(workspace.join("contents.xcworkspacedata")) else {
        return Vec::new();
    };
    let base = workspace.parent().unwrap_or(Path::new(""));
    let resolve = |location: &str, group: &Path| -> Option<PathBuf> {
        let (kind, path) = location.split_once(':')?;
        match kind {
            "absolute" => Some(PathBuf::from(path)),
            "group" => Some(group.join(path)),
            "container" => Some(base.join(path)),
            _ => None,
        }
    };
    // The folder of each open `<Group>`, innermost last.
    let mut groups = vec![base.to_path_buf()];
    let mut projects = Vec::new();
    for tag in tags(&text) {
        let group = groups.last().cloned().unwrap_or_default();
        match (tag.name, tag.closing) {
            ("Group", true) => {
                if groups.len() > 1 {
                    groups.pop();
                }
            }
            ("Group", false) if !tag.self_closing => {
                // A group without a location of its own is a plain folder
                // of the navigator, at its parent's place.
                let dir = tag
                    .location
                    .as_deref()
                    .and_then(|l| resolve(l, &group))
                    .unwrap_or(group);
                groups.push(dir);
            }
            ("FileRef", false) => {
                if let Some(path) = tag.location.as_deref().and_then(|l| resolve(l, &group)) {
                    if path
                        .extension()
                        .is_some_and(|e| e.eq_ignore_ascii_case("xcodeproj"))
                    {
                        projects.push(path);
                    }
                }
            }
            _ => {}
        }
    }
    projects.sort();
    projects.dedup();
    projects
}

/// One XML tag of a contents file: its name, whether it is `</…>` or
/// `<…/>`, and its `location` attribute, XML-unescaped.
#[derive(Debug, PartialEq, Eq)]
struct Tag<'a> {
    name: &'a str,
    closing: bool,
    self_closing: bool,
    location: Option<String>,
}

/// The tags of `text` in order; the `<?xml …?>` declaration and comments
/// are skipped.
fn tags(text: &str) -> Vec<Tag<'_>> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('<') {
        rest = &rest[open + 1..];
        if let Some(comment) = rest.strip_prefix("!--") {
            rest = comment.find("-->").map_or("", |end| &comment[end + 3..]);
            continue;
        }
        // The tag ends at the first `>` outside a quoted value.
        let mut quote = None;
        let Some(end) = rest.char_indices().find_map(|(i, c)| match (quote, c) {
            (None, '"' | '\'') => {
                quote = Some(c);
                None
            }
            (Some(q), c) if c == q => {
                quote = None;
                None
            }
            (None, '>') => Some(i),
            _ => None,
        }) else {
            break;
        };
        let body = &rest[..end];
        rest = &rest[end + 1..];
        if body.starts_with('?') || body.starts_with('!') {
            continue;
        }
        let (closing, body) = match body.strip_prefix('/') {
            Some(body) => (true, body),
            None => (false, body),
        };
        let self_closing = body.trim_end().ends_with('/');
        let name_end = body
            .find(|c: char| c.is_whitespace() || c == '/')
            .unwrap_or(body.len());
        found.push(Tag {
            name: &body[..name_end],
            closing,
            self_closing,
            location: attribute(&body[name_end..], "location"),
        });
    }
    found
}

/// The value of attribute `name` in a tag's attribute text, XML-unescaped.
fn attribute(attributes: &str, name: &str) -> Option<String> {
    let mut rest = attributes;
    loop {
        let at = rest.find(name)?;
        let before = rest[..at].chars().next_back();
        rest = &rest[at + name.len()..];
        if before.is_some_and(|c| !c.is_whitespace()) {
            continue; // part of a longer attribute name
        }
        let Some(after_eq) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let after_eq = after_eq.trim_start();
        let quote = after_eq
            .chars()
            .next()
            .filter(|c| *c == '"' || *c == '\'')?;
        let value = &after_eq[1..];
        let end = value.find(quote)?;
        return Some(
            value[..end]
                .replace("&quot;", "\"")
                .replace("&apos;", "'")
                .replace("&lt;", "<")
                .replace("&gt;", ">")
                .replace("&amp;", "&"),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::SystemTime;

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn sandbox() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zedxcode-schemes-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn bytes(v: Value) -> Vec<u8> {
        serde_json::to_vec(&v).unwrap()
    }

    /// Move `path`'s mtime into the future, as a later edit would.
    fn touch_later(path: &Path, seconds: u64) {
        let later = SystemTime::now() + Duration::from_secs(seconds);
        fs::File::open(path).unwrap().set_modified(later).unwrap();
    }

    #[test]
    fn parses_workspace_and_project_shapes() {
        let ws = json!({ "workspace": { "name": "MyApp", "schemes": ["MyApp", "MyApp Dev"] } });
        assert_eq!(
            parse(&bytes(ws)).unwrap(),
            SchemeList {
                schemes: vec!["MyApp".into(), "MyApp Dev".into()],
                configurations: vec![],
            }
        );
        let project = json!({ "project": { "name": "MyApp", "schemes": ["MyApp"],
                                           "configurations": ["Debug", "Release"],
                                           "targets": ["MyApp"] } });
        assert_eq!(
            parse(&bytes(project)).unwrap(),
            SchemeList {
                schemes: vec!["MyApp".into()],
                configurations: vec!["Debug".into(), "Release".into()],
            }
        );
        assert!(parse(b"not json").is_err());
        assert!(parse(&bytes(json!({ "workspace": { "schemes": [] } }))).is_err());
        assert!(parse(&bytes(json!({ "other": {} }))).is_err());
    }

    #[test]
    fn package_resolution_output_before_the_json_is_skipped() {
        let out = "Command line invocation:\n    /usr/bin/xcodebuild -list -json -workspace \
                   MyApp.xcworkspace\n\nResolve Package Graph\n\nResolved source packages:\n  \
                   swift-collections: https://example.com/swift-collections.git @ 1.1.4\n\n\
                   {\n  \"workspace\" : {\n    \"name\" : \"MyApp\",\n    \"schemes\" : [\n      \
                   \"MyApp\"\n    ]\n  }\n}\n";
        assert_eq!(
            parse(out.as_bytes()).unwrap(),
            SchemeList {
                schemes: vec!["MyApp".into()],
                configurations: vec![],
            }
        );
        // A brace inside a progress line is not the start of the JSON.
        assert!(parse(b"Resolving {swift-collections}\n").is_err());
    }

    #[test]
    fn the_key_follows_the_contents_file_and_every_scheme_folder() {
        let dir = sandbox();
        let ws = dir.join("MyApp.xcworkspace");
        let project = dir.join("ios/MyApp.xcodeproj");
        fs::create_dir_all(&ws).unwrap();
        fs::create_dir_all(&project).unwrap();
        fs::write(
            ws.join("contents.xcworkspacedata"),
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<Workspace version = \"1.0\">\n   \
             <FileRef\n      location = \"group:ios/MyApp.xcodeproj\">\n   </FileRef>\n   \
             <FileRef location = 'group:Pods/Pods.xcodeproj'></FileRef>\n</Workspace>\n",
        )
        .unwrap();
        assert_eq!(
            workspace_projects(&ws),
            [dir.join("Pods/Pods.xcodeproj"), project.clone()]
        );
        let mut key = cache_key(&ws).unwrap();
        let mut changed = |what: &str| {
            let next = cache_key(&ws).unwrap();
            assert_ne!(next, key, "{what} did not change the key");
            key = next;
        };

        // Tuist rewrites the contents file in place.
        touch_later(&ws.join("contents.xcworkspacedata"), 60);
        changed("the contents file");
        // A shared scheme added to a referenced project.
        fs::create_dir_all(project.join("xcshareddata/xcschemes")).unwrap();
        changed("a new xcschemes folder");
        fs::write(
            project.join("xcshareddata/xcschemes/MyApp Dev.xcscheme"),
            "<Scheme/>",
        )
        .unwrap();
        touch_later(&project.join("xcshareddata/xcschemes"), 120);
        changed("a scheme added to the project");
        // A user scheme in the workspace itself.
        fs::create_dir_all(ws.join("xcuserdata/jane.xcuserdatad/xcschemes")).unwrap();
        changed("a user's xcschemes folder");
        assert_eq!(cache_key(&ws).unwrap(), key, "the key is stable");

        assert!(cache_key(&dir.join("Missing.xcworkspace")).is_err());
    }

    #[test]
    fn xcode_saving_its_window_state_keeps_the_key() {
        let ws = sandbox().join("MyApp.xcworkspace");
        let user = ws.join("xcuserdata/jane.xcuserdatad");
        fs::create_dir_all(user.join("xcschemes")).unwrap();
        fs::create_dir_all(ws.join("xcshareddata/xcschemes")).unwrap();
        let key = cache_key(&ws).unwrap();
        // An atomic save: a new file in the user folder, the folder's mtime
        // moved; the same next to the shared schemes.
        fs::write(user.join("UserInterfaceState.xcuserstate"), "bplist00").unwrap();
        touch_later(&user, 60);
        fs::write(ws.join("xcshareddata/IDEWorkspaceChecks.plist"), "<plist/>").unwrap();
        touch_later(&ws.join("xcshareddata"), 60);
        touch_later(&ws.join("xcuserdata"), 60);
        assert_eq!(cache_key(&ws).unwrap(), key);
        // A second user's folder still counts.
        fs::create_dir_all(ws.join("xcuserdata/sam.xcuserdatad")).unwrap();
        assert_ne!(cache_key(&ws).unwrap(), key);
    }

    #[test]
    fn group_locations_are_relative_to_the_enclosing_group() {
        let dir = sandbox();
        let ws = dir.join("MyApp.xcworkspace");
        fs::create_dir_all(&ws).unwrap();
        fs::write(
            ws.join("contents.xcworkspacedata"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<Workspace
   version = "1.0">
   <!-- <FileRef location = "group:Commented.xcodeproj"></FileRef> -->
   <Group
      location = "container:Modules"
      name = "Modules">
      <FileRef
         location = "group:Billing/Billing.xcodeproj">
      </FileRef>
      <Group
         location = "group:Shared"
         name = "Shared">
         <FileRef location = "group:Kit.xcodeproj"/>
      </Group>
      <Group name = "Unplaced">
         <FileRef location = "group:Loose.xcodeproj"></FileRef>
      </Group>
      <Group location = "group:Empty" name = "Empty"/>
      <FileRef location = "group:Search.xcodeproj"></FileRef>
   </Group>
   <FileRef
      location = "group:App/MyApp.xcodeproj">
   </FileRef>
   <FileRef location = "container:Tools/Tools.xcodeproj"></FileRef>
   <FileRef location = "absolute:/Users/x/Vendor/Vendor.xcodeproj"></FileRef>
   <FileRef location = "group:README.md"></FileRef>
</Workspace>
"#,
        )
        .unwrap();
        let mut expected = vec![
            dir.join("Modules/Billing/Billing.xcodeproj"),
            dir.join("Modules/Shared/Kit.xcodeproj"),
            dir.join("Modules/Loose.xcodeproj"),
            dir.join("Modules/Search.xcodeproj"),
            dir.join("App/MyApp.xcodeproj"),
            dir.join("Tools/Tools.xcodeproj"),
            PathBuf::from("/Users/x/Vendor/Vendor.xcodeproj"),
        ];
        expected.sort();
        assert_eq!(workspace_projects(&ws), expected);
    }

    #[test]
    fn a_project_key_follows_its_project_file() {
        let project = sandbox().join("MyApp.xcodeproj");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("project.pbxproj"), "// !$*UTF8*$!").unwrap();
        let key = cache_key(&project).unwrap();
        touch_later(&project.join("project.pbxproj"), 60);
        assert_ne!(cache_key(&project).unwrap(), key);
        // A project is never read as a workspace.
        assert!(!key_paths(&project)
            .iter()
            .any(|p| p.ends_with("Pods.xcodeproj")));
    }

    #[test]
    fn tags_and_locations_are_read_and_unescaped() {
        assert_eq!(
            tags(
                "<FileRef location = \"group:A &amp; B.xcodeproj\"/><Group sublocation=\"x\" \
                 location='container:' name=\"a>b\"></Group>"
            ),
            [
                Tag {
                    name: "FileRef",
                    closing: false,
                    self_closing: true,
                    location: Some("group:A & B.xcodeproj".into()),
                },
                Tag {
                    name: "Group",
                    closing: false,
                    self_closing: false,
                    location: Some("container:".into()),
                },
                Tag {
                    name: "Group",
                    closing: true,
                    self_closing: false,
                    location: None,
                },
            ]
        );
        assert_eq!(tags("no tags"), Vec::<Tag>::new());
    }
}
