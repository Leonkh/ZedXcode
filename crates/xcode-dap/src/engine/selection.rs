//! Runtime selection overlay: `.zed/.zedx/selection.json`.
//!
//! Written by `xcode-dap select-scheme` / `select-device`, re-read from disk
//! by the engine on every build/run/clean and every DAP `launch`. Because
//! the overlay lives outside `.zed/debug.json`, Zed's `debugger::Rerun`
//! (which reuses its in-memory copy of the last scenario) still picks up a
//! new selection: the scenario JSON stays stale, but this binary overlays
//! the fresh on-disk selection each time it is spawned.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::engine::config::LaunchConfig;
use crate::engine::pipeline::OutputSink;
use crate::setup::jsonc;

/// The persisted selection. Every field is optional; absent fields fall
/// through to the scenario / CLI config.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Selection {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scheme: Option<String>,
    /// Simulator device name or UDID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// Simulator OS version (e.g. `"26.3"`); written alongside `device` by
    /// `select-device` so the pair always identifies one runtime.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
}

impl Selection {
    pub fn is_empty(&self) -> bool {
        self.scheme.is_none() && self.device.is_none() && self.os.is_none()
    }
}

/// Walk up from `start` to the first directory containing `.zed/`.
pub fn find_project_dir(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        if d.join(".zed").is_dir() {
            return Some(d.to_path_buf());
        }
        dir = d.parent();
    }
    None
}

/// [`find_project_dir`] anchored at the process cwd — Zed runs both the DAP
/// adapter and tasks with cwd = the worktree root, so this finds the
/// project's `.zed/` in every Zed-spawned context.
pub fn find_project_dir_from_cwd() -> Option<PathBuf> {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| find_project_dir(&cwd))
}

/// `<project>/.zed/.zedx/selection.json`.
pub fn selection_path(project_dir: &Path) -> PathBuf {
    project_dir
        .join(".zed")
        .join(".zedx")
        .join("selection.json")
}

/// Load the selection for a project. Missing or unparseable files yield the
/// empty selection — a broken overlay must never break a build.
pub fn load(project_dir: &Path) -> Selection {
    let path = selection_path(project_dir);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Selection::default();
    };
    match jsonc::parse_jsonc(&text).and_then(|v| Ok(serde_json::from_value(v)?)) {
        Ok(sel) => sel,
        Err(e) => {
            eprintln!(
                "xcode-dap: ignoring malformed {} ({e:#}) — re-run \
                 `xcode-dap select-scheme` / `select-device` to rewrite it",
                path.display()
            );
            Selection::default()
        }
    }
}

/// Atomically write the selection; returns the file path.
pub fn save(project_dir: &Path, sel: &Selection) -> Result<PathBuf> {
    let path = selection_path(project_dir);
    let dir = path.parent().expect("selection path always has a parent");
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let mut text = serde_json::to_string_pretty(sel).expect("Selection always serializes");
    text.push('\n');
    jsonc::atomic_write(&path, &text)?;
    Ok(path)
}

/// Pure merge: fields present in the selection override the config.
///
/// Picking a `device` replaces the whole destination — `os` is taken from
/// the selection too (even when `None`), so a stale config `os` can never
/// contradict the picked device. A selection with only `os` narrows just
/// the OS (hand-edited files).
pub fn apply(cfg: &mut LaunchConfig, sel: &Selection) {
    if let Some(s) = &sel.scheme {
        cfg.scheme = s.clone();
    }
    if sel.device.is_some() {
        cfg.device = sel.device.clone();
        cfg.os = sel.os.clone();
    } else if sel.os.is_some() {
        cfg.os = sel.os.clone();
    }
}

/// Effective config for one run: re-read the on-disk selection (walking up
/// from cwd to find `.zed/`) and overlay it onto `cfg`. Always emits one
/// `Scheme: … | Destination: …` line so the user sees what runs.
pub fn overlaid(cfg: &LaunchConfig, sink: &dyn OutputSink) -> LaunchConfig {
    let sel = find_project_dir_from_cwd()
        .map(|dir| load(&dir))
        .unwrap_or_default();
    let mut eff = cfg.clone();
    apply(&mut eff, &sel);
    sink.line("console", &describe(&eff, !sel.is_empty()));
    eff
}

/// The one-line `Scheme: … | Destination: …` summary.
fn describe(cfg: &LaunchConfig, from_selection: bool) -> String {
    let destination = match (cfg.device.as_deref(), cfg.os.as_deref()) {
        (Some(d), Some(o)) => format!("{d} (iOS {o})"),
        (Some(d), None) => d.to_string(),
        (None, Some(o)) => format!("auto (iOS {o})"),
        (None, None) => "auto (booted iPhone, else newest)".to_string(),
    };
    format!(
        "Scheme: {} | Destination: {}{}",
        cfg.scheme,
        destination,
        if from_selection {
            " (from selection)"
        } else {
            ""
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn sandbox() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "zedxcode-selection-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(dir.join(".zed")).unwrap();
        dir
    }

    fn base_cfg() -> LaunchConfig {
        LaunchConfig {
            workspace: "MyApp.xcworkspace".into(),
            scheme: "MyApp (staging)".into(),
            device: Some("iPhone 15 Pro Max".into()),
            os: Some("26.3".into()),
            configuration: None,
            preflight: None,
            oslog: false,
            oslog_predicate: None,
            terminate_on_stop: true,
            build_output: Default::default(),
            verbose_logging: false,
            derived_data: None,
        }
    }

    #[test]
    fn empty_selection_is_a_noop() {
        let mut cfg = base_cfg();
        apply(&mut cfg, &Selection::default());
        assert_eq!(cfg.scheme, "MyApp (staging)");
        assert_eq!(cfg.device.as_deref(), Some("iPhone 15 Pro Max"));
        assert_eq!(cfg.os.as_deref(), Some("26.3"));
    }

    #[test]
    fn scheme_only_overrides_scheme_and_keeps_destination() {
        let mut cfg = base_cfg();
        apply(
            &mut cfg,
            &Selection {
                scheme: Some("MyApp (production)".into()),
                ..Default::default()
            },
        );
        assert_eq!(cfg.scheme, "MyApp (production)");
        assert_eq!(cfg.device.as_deref(), Some("iPhone 15 Pro Max"));
        assert_eq!(cfg.os.as_deref(), Some("26.3"));
    }

    #[test]
    fn device_with_os_overrides_both() {
        let mut cfg = base_cfg();
        apply(
            &mut cfg,
            &Selection {
                device: Some("iPhone 16 Pro".into()),
                os: Some("18.2".into()),
                ..Default::default()
            },
        );
        assert_eq!(cfg.scheme, "MyApp (staging)");
        assert_eq!(cfg.device.as_deref(), Some("iPhone 16 Pro"));
        assert_eq!(cfg.os.as_deref(), Some("18.2"));
    }

    #[test]
    fn device_without_os_clears_stale_config_os() {
        // A picked device must not be constrained by the old config's os.
        let mut cfg = base_cfg();
        apply(
            &mut cfg,
            &Selection {
                device: Some("iPhone 16 Pro".into()),
                ..Default::default()
            },
        );
        assert_eq!(cfg.device.as_deref(), Some("iPhone 16 Pro"));
        assert_eq!(cfg.os, None);
    }

    #[test]
    fn os_only_narrows_os() {
        let mut cfg = base_cfg();
        apply(
            &mut cfg,
            &Selection {
                os: Some("18.2".into()),
                ..Default::default()
            },
        );
        assert_eq!(cfg.device.as_deref(), Some("iPhone 15 Pro Max"));
        assert_eq!(cfg.os.as_deref(), Some("18.2"));
    }

    #[test]
    fn save_load_roundtrip_and_partial_files() {
        let dir = sandbox();
        // Missing file -> default.
        assert_eq!(load(&dir), Selection::default());

        let sel = Selection {
            scheme: Some("MyApp (staging)".into()),
            device: Some("iPhone 16 Pro".into()),
            os: Some("26.3".into()),
        };
        let path = save(&dir, &sel).unwrap();
        assert_eq!(path, dir.join(".zed/.zedx/selection.json"));
        assert_eq!(load(&dir), sel);

        // Partial hand-written file (scheme only) parses fine.
        fs::write(&path, "{\"scheme\": \"Other\"}\n").unwrap();
        assert_eq!(
            load(&dir),
            Selection {
                scheme: Some("Other".into()),
                ..Default::default()
            }
        );

        // Corrupt file -> default, never an error.
        fs::write(&path, "{nonsense").unwrap();
        assert_eq!(load(&dir), Selection::default());
    }

    #[test]
    fn saved_file_omits_absent_fields() {
        let dir = sandbox();
        let path = save(
            &dir,
            &Selection {
                scheme: Some("X".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let text = fs::read_to_string(path).unwrap();
        assert!(text.contains("\"scheme\""));
        assert!(!text.contains("\"device\""));
        assert!(!text.contains("\"os\""));
    }

    #[test]
    fn find_project_dir_walks_up() {
        let dir = sandbox();
        let nested = dir.join("a/b/c");
        fs::create_dir_all(&nested).unwrap();
        assert_eq!(find_project_dir(&nested), Some(dir.clone()));
        assert_eq!(find_project_dir(&dir), Some(dir.clone()));
        // No .zed anywhere up from temp roots without one.
        let bare =
            std::env::temp_dir().join(format!("zedxcode-selection-bare-{}", std::process::id()));
        fs::create_dir_all(&bare).unwrap();
        // (The system temp dir itself could theoretically contain .zed/ —
        // accept either None or a dir above; just assert it is not `bare`.)
        assert_ne!(find_project_dir(&bare), Some(bare));
    }

    #[test]
    fn describe_lines() {
        let mut cfg = base_cfg();
        assert_eq!(
            describe(&cfg, false),
            "Scheme: MyApp (staging) | Destination: iPhone 15 Pro Max (iOS 26.3)"
        );
        assert_eq!(
            describe(&cfg, true),
            "Scheme: MyApp (staging) | Destination: iPhone 15 Pro Max (iOS 26.3) (from selection)"
        );
        cfg.os = None;
        assert_eq!(
            describe(&cfg, false),
            "Scheme: MyApp (staging) | Destination: iPhone 15 Pro Max"
        );
        cfg.device = None;
        assert_eq!(
            describe(&cfg, false),
            "Scheme: MyApp (staging) | Destination: auto (booted iPhone, else newest)"
        );
    }
}
