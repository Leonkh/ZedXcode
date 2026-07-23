# ZedXcode — WASM Extension Side: Authoritative Design

> **Design snapshot.** This document is the pre-implementation design and is
> kept for historical context. Where it drifts from the code (e.g. the §2
> schema sketch below predates the shipped config surface), the shipped
> `extension/debug_adapter_schemas/Xcode.json` and
> `crates/xcode-dap-config/src/lib.rs` are authoritative.

All claims below verified 2026-06-11 against: the **Zed v1.6.3 tag** of `zed-industries/zed` (matches the locally installed Zed 1.6.3 at /Applications/Zed.app), `zed_extension_api` 0.7.0 on docs.rs, the live `zed-extensions/swift` and `zed-extensions/php` repos, the live `zed-industries/extensions` registry, and the local machine (Zed.app Info.plist, `~/Library/Application Support/Zed/`).

---

## 0. Hard facts that gate everything (verified)

| Fact | Value | Source |
|---|---|---|
| Extension API max version on **stable** Zed 1.6.3 | **0.7.0** (`since_v0_6_0::MAX_VERSION = Version::new(0, 7, 0)`; 0.8.0 WIT exists on main but is Dev/Nightly-gated) | `crates/extension_host/src/wasm_host/wit.rs` lines 64–67 + `wit/since_v0_6_0.rs` lines 11–12 at tag `v1.6.3` |
| WASM target | **`wasm32-wasip2`** (NOT wasip1) | `crates/extension_builder.rs` line 28 (`const RUST_TARGET: &str = "wasm32-wasip2"`); string `wasm32-wasip2` present in the local `/Applications/Zed.app/Contents/MacOS/zed` binary; `crates/extension_api/README.md` |
| Rust must come from **rustup** (homebrew Rust breaks dev-extension install); Zed auto-runs `rustup target add wasm32-wasip2` if missing | — | `docs/src/extensions/developing-extensions.md` at v1.6.3; `extension_builder.rs` `install_rust_wasm_target_if_needed()` lines 447–467 |
| Manifest section | `[debug_adapters.<Name>]`, single optional field `schema_path`, defaulting to `debug_adapter_schemas/<Name>.json`; schema file **mandatory** (adapter registration fails without it) | `crates/extension/src/extension_manifest.rs` lines 116, 349–351, 190–200; `ExtensionDapAdapter::new` in `crates/debug_adapter_extension/src/extension_dap_adapter.rs`; https://zed.dev/docs/extensions/debugger-extensions |
| The schema's only consumer is JSON validation/completions for `.zed/debug.json` (via `DapRegistry::adapters_schema()` → `json_schema_store`). The New Session modal does **not** render schema-driven fields. | — | `crates/dap/src/registry.rs:71-79`, `crates/json_schema_store/src/json_schema_store.rs:409` |
| `dap.<Adapter>` settings: only **`binary`** reaches an extension adapter (as `user_provided_debug_adapter_path`, resolved relative to the worktree); `args`/`env` are **ignored** for extension adapters (explicit `// TODO support user args in the extension API`) | — | `crates/project/src/debugger/dap_store.rs:258-287`; `extension_dap_adapter.rs` `get_binary` |
| Scenario `config` has Zed task variables substituted and paths relativized **before** the extension sees it | — | `crates/debugger_ui/src/session/running.rs:996-1031` (`substitute_variables_in_config`); WIT doc comment on `start-debugging-request-arguments` |
| The DAP `launch` request body Zed sends = exactly `request_args.configuration` returned from `get_dap_binary` | — | `crates/project/src/debugger/session.rs:425-435` (`Launch { raw: raw.configuration }`) |
| `DebugAdapterBinary.command` is passed through **verbatim** (no work-dir resolution, unlike LSP) → must be an **absolute path**; `env::current_dir()` inside the WASM returns the extension work dir | — | `wit/since_v0_8_0.rs` `TryFrom<DebugAdapterBinary>` lines 244–256; preopens `ctx.preopened_dir(&path, ".", …)` + `.env("PWD", &path)` in `wasm_host.rs:743-748`; PHP ext uses `env::current_dir().unwrap().join(...)` in production |
| Extension work dir | `~/Library/Application Support/Zed/extensions/work/<extension-id>/` (cwd of the WASM; `download_file` paths are relative to it) | `wasm_host.rs` (`work_dir.join(manifest.id)`), confirmed locally (`work/html`, `work/swift` exist) |
| `download_file` needs **no** `[capabilities]` in extension.toml (manifest only gates `process:exec`); host default grants are wildcard: `{ "kind": "download_file", "host": "*", "path": ["**"] }` | — | `crates/extension_host/src/capability_granter.rs` (`grant_download_file` checks granted only), `assets/settings/default.json:2068-2072` at v1.6.3 |
| Monorepo publishing supported via `path` key in registry `extensions.toml` | — | live `zed-industries/extensions/extensions.toml` (e.g. `[al-business-central] submodule=… path = "crates/extension"`); 1268 extensions currently |
| Extension id must not contain `zed`/`Zed`/`extension`; license file (Apache-2.0 OK) required at submodule repo root since 2025-10-01 | — | `docs/src/extensions/developing-extensions.md` at v1.6.3 |

---

## 1. Debug-adapter API surface (zed_extension_api 0.7.0)

Exact signatures (verified on docs.rs 0.7.0; identical WIT as 0.6.0 — there is no `since_v0.7.0` WIT dir; the `since_v0.6.0` WIT serves 0.6.0–0.7.0):

```rust
// Required
fn new() -> Self;

// DAP — all provided methods we override:
fn get_dap_binary(
    &mut self,
    adapter_name: String,
    config: DebugTaskDefinition,                      // { label, adapter, config: String(JSON, substituted), tcp_connection: Option<TcpArgumentsTemplate> }
    user_provided_debug_adapter_path: Option<String>, // from settings dap.Xcode.binary
    worktree: &Worktree,                              // .root_path(), .which(), .shell_env(), .read_text_file()
) -> Result<DebugAdapterBinary, String>;

fn dap_request_kind(
    &mut self,
    adapter_name: String,
    config: serde_json::Value,                        // raw scenario config
) -> Result<StartDebuggingRequestArgumentsRequest, String>;  // Launch | Attach

fn dap_config_to_scenario(
    &mut self,
    config: DebugConfig,                              // { label, adapter, request: DebugRequest::Launch{program,cwd,args,envs}|Attach{process_id}, stop_on_entry }
) -> Result<DebugScenario, String>;

// Locator methods — NOT needed for our design (no build task, see §4):
fn dap_locator_create_scenario(&mut self, locator_name: String, build_task: TaskTemplate,
    resolved_label: String, debug_adapter_name: String) -> Option<DebugScenario>;
fn run_dap_locator(&mut self, locator_name: String, build_task: TaskTemplate) -> Result<DebugRequest, String>;
```

Key records (WIT `since_v0.6.0/dap.wit`, verbatim semantics):

```text
debug-scenario        { label, adapter, build: option<build-task-definition>, config: string(JSON), tcp-connection: option<tcp-arguments-template> }
debug-adapter-binary  { command: option<string>, arguments: list<string>, envs, cwd: option<string>,
                        connection: option<tcp-arguments>,   // None => stdio transport
                        request-args: { configuration: string(JSON), request: launch|attach } }
```

### What each method does for adapter "Xcode"

- **`get_dap_binary`** — the heart. Resolve the `xcode-dap` proxy binary (§3), parse `config.config` only enough to sanity-check it (full validation belongs to the proxy), and return:
  ```rust
  DebugAdapterBinary {
      command: Some(absolute_proxy_path),
      arguments: vec![],                       // proxy speaks DAP on stdio, no args needed
      envs: worktree.shell_env(),              // user's shell env → DEVELOPER_DIR, PATH for xcrun/tuist/make
      cwd: Some(worktree.root_path()),         // proxy runs xcodebuild from the project root
      connection: None,                        // stdio, like lldb-dap
      request_args: StartDebuggingRequestArguments {
          configuration: config.config,        // pass scenario config through VERBATIM
          request: StartDebuggingRequestArgumentsRequest::Launch,
      },
  }
  ```
  This mirrors the Swift extension exactly (it also passes `config.config` verbatim into `request_args.configuration`) — the proxy then receives that JSON as the `arguments` of the DAP `launch` request (verified `session.rs:425-435`).

- **`dap_request_kind`** — return `Launch` unless `config["request"] == "attach"` (future-proofing); error on unknown values, like the Swift impl. Zed calls this (via `DebugAdapter::request_kind`) wherever it must classify a raw config.

- **`dap_config_to_scenario`** — called when the user uses the New Session modal's **Launch/Attach tabs** (generic, adapter-agnostic; verified `new_process_modal.rs`: adapter dropdown enumerates `DapRegistry` including extension adapters, Launch tab builds a `ZedDebugConfig` and calls `config_from_zed_format`). Map minimally: `launch.program` → `scheme`, and `stop_on_entry` → `stopOnEntry` (round-tripped into the saved scenario but currently ignored by the proxy — `LaunchConfig` has no such field and parses without `deny_unknown_fields`, so the key is accepted and dropped). The required `workspace` cannot be derived here — the scenario is saved without it, and the user fills it into `.zed/debug.json` by hand (the schema flags the omission). `Attach` → error `"Xcode adapter: use a .zed/debug.json scenario (attach not yet supported)"`. The primary GUI path is the modal's scenario list fed from `.zed/debug.json`, not these tabs — but this method must not panic, and modal's "Save to debug.json" uses its output.

---

## 2. `extension.toml` + config schema

```toml
# extension/extension.toml
id = "xcode-tools"            # MUST NOT contain "zed" or "extension"; immutable after publish
name = "Xcode Tools"
description = "Xcode-like build & debug for iOS Simulator projects: build, install, launch and attach lldb with app console output in the Debug Console."
version = "0.1.0"
schema_version = 1
authors = ["Leonkh <leon0897le@gmail.com>"]
repository = "https://github.com/Leonkh/ZedXcode"

[debug_adapters.Xcode]
# schema_path defaults to "debug_adapter_schemas/Xcode.json" — declare explicitly for clarity:
schema_path = "debug_adapter_schemas/Xcode.json"
```

No `[capabilities]` section needed (verified §0). No `[debug_locators]` (we don't use the locator flow).

`extension/debug_adapter_schemas/Xcode.json` (mandatory; powers validation + completions inside `.zed/debug.json` only):

```json
{
  "type": "object",
  "required": ["scheme"],
  "properties": {
    "request":          { "type": "string", "enum": ["launch"], "default": "launch" },
    "workspace":        { "type": "string", "description": "Path to .xcworkspace/.xcodeproj. Default: auto-discover in $ZED_WORKTREE_ROOT.", "default": "${ZED_WORKTREE_ROOT}" },
    "scheme":           { "type": "string", "description": "Xcode scheme to build and run, e.g. \"YourApp\"" },
    "device":           { "type": "string", "description": "Simulator device name, e.g. \"iPhone 15 Pro Max\". Default: currently booted simulator." },
    "os":               { "type": "string", "description": "Simulator OS version, e.g. \"26.3\". Default: latest available for device." },
    "configuration":    { "type": "string", "description": "Build configuration (Debug/Release).", "default": "Debug" },
    "generate_command": { "type": "string", "description": "Project-generation preflight when workspace is missing, e.g. \"make project CI=true\"" },
    "skip_build":       { "type": "boolean", "default": false },
    "stop_on_entry":    { "type": "boolean", "default": false }
  }
}
```

### What the New Session modal shows (verified `crates/debugger_ui/src/new_process_modal.rs` at v1.6.3)

Four tabs: a scenario/task list (entries from `.zed/debug.json` and locators), Debug, **Attach**, **Launch**. "Xcode" appears in the adapter dropdown automatically once the extension is installed (`DapRegistry::enumerate_adapters()`). There is no schema-driven form — the GUI-first flow is: setup flow writes the scenario into `.zed/debug.json` → it appears in the modal list → first CMD+R opens the modal, user picks "Xcode: Run YourApp" → every subsequent `debugger::Rerun` (CMD+R) replays it without the modal.

---

## 3. Adapter binary delivery

**Reference implementation: PHP extension's Xdebug adapter** (`zed-extensions/php/src/xdebug.rs` — the only first-party-org debugger extension that *downloads* its adapter; read verbatim during this research). Pattern adopted, with version pinning instead of `latest`:

Resolution order in `get_dap_binary`:
1. `user_provided_debug_adapter_path` (settings `"dap": { "Xcode": { "binary": "/abs/path/xcode-dap" } }"`) — always wins. Note: `dap.Xcode.args`/`env` never reach extensions (verified §0) — all knobs live in scenario config.
2. `worktree.which("xcode-dap")` — dev mode: a `cargo install --path crates/xcode-dap` binary on PATH.
3. Pinned GitHub release, cached in the work dir:

```rust
const PROXY_REPO: &str = "Leonkh/ZedXcode";
const PROXY_TAG:  &str = "xcode-dap-v0.1.0";   // bumped in lockstep with extension version

fn cached_or_downloaded_proxy(&mut self) -> Result<String, String> {
    let arch = match zed::current_platform() {
        (zed::Os::Mac, zed::Architecture::Aarch64) => "aarch64",
        (zed::Os::Mac, zed::Architecture::X8664)   => "x86_64",
        _ => return Err("the Xcode adapter is macOS-only".into()),
    };
    let version_dir = format!("xcode-dap/{PROXY_TAG}");            // relative to work dir
    let bin_rel = format!("{version_dir}/xcode-dap");
    if !std::path::Path::new(&bin_rel).exists() {
        let release = zed::github_release_by_tag_name(PROXY_REPO, PROXY_TAG)?;   // pinned, deterministic
        let asset_name = format!("{PROXY_TAG}-{arch}-apple-darwin.tar.gz");   // e.g. xcode-dap-v0.1.0-aarch64-apple-darwin.tar.gz
        let asset = release.assets.into_iter().find(|a| a.name == asset_name)
            .ok_or_else(|| format!("no release asset {asset_name}"))?;
        std::fs::remove_dir_all("xcode-dap").ok();                 // purge stale versions (PHP pattern)
        zed::download_file(&asset.download_url, &version_dir, zed::DownloadedFileType::GzipTar)?;
        zed::make_file_executable(&bin_rel)?;                      // belt-and-braces; tar should preserve mode
    }
    // command must be ABSOLUTE (verified: no work-dir resolution for DAP commands)
    Ok(std::env::current_dir().unwrap().join(&bin_rel).to_string_lossy().into_owned())
}
```

Signatures verified from WIT/docs.rs: `latest_github_release(repo, GithubReleaseOptions { require_assets, pre_release })`, `github_release_by_tag_name(repo, tag)`, `download_file(url, path, DownloadedFileType::{Gzip, GzipTar, Zip, Uncompressed})`, `make_file_executable(path)`. Offline fallback (PHP pattern): if the GitHub call *or* the download fails, reuse the newest previously cached version instead of erroring.

**Resolution provenance.** `get_dap_binary` tags every resolution with its source (`user-setting` | `worktree-path` | `cached-release` | `github-release`), prints it to Zed's foreground log, and exports `XCODE_DAP_RESOLVED_FROM=<source>:<path> ext-v<version>` into the proxy's env — the proxy echoes it in the `xcode-dap.log` session header, so `doctor` and bug reports show exactly which binary ran and where it came from.

### Gatekeeper / quarantine / signing (verified + cited)

- **Zed's `download_file` produces non-quarantined files.** Zed downloads via its own HTTP client (`wasm_host` `download_file` → `self.host.http_client.get(...)`, raw stream-to-disk); `com.apple.quarantine` is only attached by apps that opt in via `LSFileQuarantineEnabled` — Zed.app's Info.plist does **not** set it (checked locally with `defaults read`), and empirically this machine's `~/Library/Application Support/Zed/debug_adapters/` carries only `com.apple.provenance`, no quarantine xattr. Gatekeeper only assesses quarantined files, so **no notarization or Developer ID is required** for this delivery path. ([HackTricks on quarantine/LSFileQuarantineEnabled](https://hacktricks.wiki/en/macos-hardening/macos-security-and-privilege-escalation/macos-security-protections/macos-gatekeeper.html), [Red Canary on Gatekeeper](https://redcanary.com/blog/threat-detection/gatekeeper/))
- **arm64 macOS still requires *some* code signature to execute.** Rust release binaries built on/for Apple Silicon are automatically ad-hoc "linker-signed"; add an explicit `codesign --force -s - target/.../xcode-dap` step in release CI to make it deterministic for both arches. Ship two per-arch tar.gz assets (simpler than universal lipo).
- **Caveat to document:** if a user downloads the binary with a browser (quarantining app), ad-hoc + quarantine = Gatekeeper block ([pnpm hit exactly this with ad-hoc-signed `.node` files](https://github.com/pnpm/pnpm/issues/11056)). The README's manual-install path must use `curl -L` (curl does not quarantine) or `xattr -d com.apple.quarantine`.

---

## 4. Scenario flow

`.zed/debug.json` written by the setup flow (adapter-specific keys are **flattened** at top level — `DebugScenario.config` is `#[serde(flatten)]` per prior verified deep-dive of `crates/task/src/debug_format.rs`; matches the official docs example):

```jsonc
[
  {
    "adapter": "Xcode",
    "label": "Run YourApp",
    "request": "launch",
    "workspace": "$ZED_WORKTREE_ROOT/YourApp.xcworkspace",
    "scheme": "YourApp",
    "device": "iPhone 15 Pro Max",
    "os": "26.3",
    "preflight": "make project CI=true"
  }
]
```

Pipeline (every step verified in v1.6.3 source):
1. User picks the scenario (modal) or hits `debugger::Rerun` → `resolve_scenario()` substitutes `$ZED_WORKTREE_ROOT` etc. in `config` and produces `DebugTaskDefinition { label, adapter: "Xcode", config: "<substituted JSON string>", tcp_connection: None }` (`running.rs:996-1031`).
2. `dap_store` reads `dap.Xcode.binary` setting → calls our `get_dap_binary(…, user_installed_path, …)` (`dap_store.rs:258-287`).
3. We return the proxy binary with `request_args.configuration = config` verbatim.
4. Zed spawns the proxy on stdio, runs DAP `initialize`, then sends `launch` whose `arguments` == our scenario JSON, exactly (`session.rs:425-435`). The proxy now owns: preflight (workspace missing → `make project CI=true`), xcodebuild, simctl install/launch `--wait-for-debugger`, lldb-dap proxying, attach-by-PID, console injection as `output` events. Breakpoints set in Zed arrive between `initialize` and `configurationDone`, before the proxy lets the process continue — Xcode-like startup breakpoints work.

### `build` field: **omit it** (recommendation)

Keep the scenario build-free and run xcodebuild inside the proxy. Rationale:
- **GUI-first console**: a Zed `build` task runs in the terminal panel; the proxy instead streams xcodebuild output as DAP `output` events → build progress appears in the **Debug Console**, exactly like Xcode's unified run log. Zed's locator machinery (the reason `build` exists) solves "find the artifact after building" — our proxy already knows the artifact via `-showBuildSettings`.
- **Rerun semantics for free**: `debugger::Rerun` replays the scenario; since build lives in the proxy, every CMD+R is build+install+launch+attach with zero task config.
- **No tasks.json coupling**: `build` as `ByName` requires a task label defined elsewhere; inline templates can't express our conditional preflight (Tuist regen only when workspace missing).
- Trade-off accepted: no terminal scrollback/colors for builds; mitigate by piping through `xcbeautify` when present.

---

## 5. Dev workflow

```sh
# one-time (no Rust installed yet on this machine)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # rustup REQUIRED; brew Rust breaks dev extensions
rustup target add wasm32-wasip2     # Zed would auto-add it, but explicit is faster to debug
```

Iteration loop:
1. `cargo build --release --target wasm32-wasip2 -p zed-xcode-ext` only as a compile check — Zed builds the WASM itself.
2. In Zed: `zed: extensions` → **Install Dev Extension** → select `ZedXcode/extension/`. Re-run (or use the Rebuild control on the dev extension's card) after each change; a published `xcode-tools` would show "Overridden by dev extension".
3. Logs: relaunch with `zed --foreground` (extension `println!` is forwarded); `zed: open log` for Zed.log.
4. Proxy dev loop is independent: `cargo install --path crates/xcode-dap` puts `xcode-dap` on PATH → resolution step 2 picks it up without touching the cached release.

Publishing checklist (per v1.6.3 docs + live registry):
1. Apache-2.0 `LICENSE` at ZedXcode repo root (required since 2025-10-01; Apache-2.0 is on the accepted list).
2. `id = "xcode-tools"` — contains no `zed`/`extension`; unique in the registry; immutable.
3. Fork `zed-industries/extensions` to a **personal account**, then:
   ```sh
   git submodule add https://github.com/Leonkh/ZedXcode.git extensions/xcode-tools   # HTTPS only, never SSH
   ```
   ```toml
   # extensions.toml
   [xcode-tools]
   submodule = "extensions/xcode-tools"
   path = "extension"          # monorepo: extension.toml lives in ZedXcode/extension/
   version = "0.1.0"           # must equal extension.toml version
   ```
   `pnpm sort-extensions`, PR.
4. Updates: bump `extension/extension.toml` version + tag a matching `xcode-dap-v*` release with both arch assets, then `git submodule update --remote extensions/xcode-tools` + bump `version` in `extensions.toml`, PR.

`Cargo.toml` for the extension crate:

```toml
[package]
name = "zed-xcode-ext"        # crate name is free-form; the registry id is what matters
version = "0.1.0"
edition = "2021"
publish = false

[lib]
crate-type = ["cdylib"]

[dependencies]
zed_extension_api = "0.7.0"   # max supported by stable Zed 1.6.3 (verified); Swift ext still on 0.6.0
serde_json = "1"              # also re-exported as zed_extension_api::serde_json
xcode-dap-config = { path = "../crates/xcode-dap-config" }   # owns the serde derives; a direct serde dep is unneeded
```

---

## 6. Repo layout (monorepo)

```
ZedXcode/
├── Cargo.toml                      # [workspace] members = ["crates/xcode-dap", "crates/xcode-dap-config"];
│                                   #  extension/ is EXCLUDED (Zed's builder compiles it independently for wasm32-wasip2)
├── LICENSE                         # Apache-2.0 — registry checks the submodule repo root
├── README.md
├── extension/                      # ← the registry `path`; ONLY this dir is built by registry CI
│   ├── extension.toml
│   ├── Cargo.toml
│   ├── debug_adapter_schemas/Xcode.json
│   └── src/lib.rs                  # SwiftExtension/PhpExtension-shaped impl (§1, §3)
├── crates/
│   ├── xcode-dap/                  # native Rust DAP proxy (sibling agent's design); releases tagged xcode-dap-v*
│   └── xcode-dap-config/           # shared serde structs for the scenario config JSON
│                                   #   (pure serde/no-IO → compiles for both wasm32-wasip2 and aarch64-apple-darwin;
│                                   #    single source of truth so extension schema, extension parsing and proxy parsing never drift)
└── .github/workflows/release.yml   # cargo build --release (both arches) → codesign -s - → tar.gz assets on xcode-dap-v* tags
```

Registry interaction: the **whole repo** becomes the submodule; `path = "extension"` points the registry's packaging at the extension dir, whose `cargo build --target wasm32-wasip2` resolves the `xcode-dap-config` path-dependency inside the submodule. The native proxy is never compiled or shipped by the registry — it arrives via the pinned GitHub release (§3). Keep `extension/` free of any `process:exec` usage so the manifest needs no capabilities.

Risks / open items for implementation:
- `zed_extension_api` 0.8.0 will reach stable eventually (WIT already on main; changes only `TcpArguments.host` u32→ip-address, irrelevant to us — we use stdio). Pin 0.7.0; revisit on Zed updates.
- The `crates/extension_api` README compatibility table is stale (stops at Zed 0.192/0.6.0) — trust the source check (stable cap = 0.7.0 at v1.6.3), not the table.
- `dap_config_to_scenario` mapping for the generic Launch tab (program→scheme) is a UX compromise; document it in the extension README.

### Critical Files for Implementation
- `extension/extension.toml` — manifest with `[debug_adapters.Xcode]` (§2)
- `extension/src/lib.rs` — `Extension` trait impl: `get_dap_binary` / `dap_request_kind` / `dap_config_to_scenario` (§1, §3)
- `extension/debug_adapter_schemas/Xcode.json` — mandatory adapter config schema (§2)
- `extension/Cargo.toml` — `zed_extension_api = "0.7.0"`, `cdylib`, path-dep on shared config crate (§5)
- `crates/xcode-dap-config/src/lib.rs` — shared scenario-config structs consumed by both the WASM extension and the proxy (§6)