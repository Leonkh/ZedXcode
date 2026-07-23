# ZedXcode — Xcode Tools for Zed

Keep your Xcode muscle memory, get Zed's speed, and lose nothing of the GUI
debugger experience. Press **⌘R** in [Zed](https://zed.dev) and your iOS app
builds, installs on the simulator, launches with lldb attached, and streams
its console into Zed's **Debug Console** — breakpoints, stepping, stack and
variables panels included, exactly like Xcode's unified run flow.

Two pieces:

- **`xcode-dap`** — a native Rust DAP-proxy binary that owns the whole
  pipeline: preflight → `xcodebuild` → `simctl install/launch` →
  `lldb-dap` attach.
- **Xcode Tools** (`xcode-tools`) — a Zed extension (WASM) that registers the
  **Xcode** debug adapter and delivers the binary. See
  [`extension/README.md`](extension/README.md).

macOS only.

## Features

| Xcode habit | In Zed | How |
|---|---|---|
| ⌘R — Run | Build + install + launch on simulator with debugger attached; build phases and app output in the Debug Console; rerun while running relaunches | `debugger::Rerun` (bound by `xcode-dap setup --user`) + the Xcode adapter |
| ⌘B — Build | Build the scheme for the simulator (task terminal) | `task::Spawn "Xcode: Build"` → `xcode-dap build` |
| ⌘⇧K — Clean | `xcodebuild clean` | `task::Spawn "Xcode: Clean"` → `xcode-dap clean` |
| ⌘⇧O — Open Quickly | Project-wide symbol search | `project_symbols::Toggle` |
| Scheme / destination picker | Interactive pickers in a task terminal; the choice overrides the scenario on the next run | `task::Spawn "Xcode: Choose Scheme"/"Xcode: Choose Destination"` → `xcode-dap select-scheme` / `select-device` |
| ⌘click — Jump to Definition | Cross-module go-to-definition, hover, references | sourcekit-lsp (Zed Swift extension) + the built-in Build Server (`xcode-dap bsp`) |
| Breakpoints / step / inspect | Zed's debugger GUI: breakpoints, stack frames, variables, watch | `lldb-dap`, proxied by `xcode-dap` |
| Console output | App stdout/stderr (and optionally OSLog) interleaved with build output in the Debug Console | `xcode-dap` output events |
| Stop | Stop ends the session and terminates the app on the simulator | `terminateOnStop` (default `true`) |

## Requirements

- macOS with [Xcode](https://developer.apple.com/xcode/) (incl. iOS simulators) and its command line tools installed
- [Zed](https://zed.dev) ≥ 1.6.3
- Rust via [rustup](https://rustup.rs) — only for the dev install path below

That's the whole list: `xcode-dap` drives Apple's own toolchain
(`xcodebuild`, `xcrun simctl`, `lldb-dap`, sourcekit-lsp) and ships its own
built-in Build Server for code navigation — nothing else to install.

## Install

### From the Zed registry

*New to the Zed registry — if search doesn't find "Xcode Tools" yet, use the dev install below in the meantime.*

1. In Zed: `zed: extensions` → search **"Xcode Tools"** → Install. The
   extension downloads a matching `xcode-dap` release binary automatically on
   the first debug session.
2. Put `xcode-dap` on your PATH for the CLI (`setup`, `doctor`, tasks): either
   symlink the binary the extension cached under
   `~/Library/Application Support/Zed/extensions/work/xcode-tools/xcode-dap/`
   (one `xcode-dap-v<version>/xcode-dap` per pinned release), e.g.

   ```sh
   ln -s "$HOME/Library/Application Support/Zed/extensions/work/xcode-tools/xcode-dap/xcode-dap-v0.1.0/xcode-dap" \
         /usr/local/bin/xcode-dap   # or any directory already on your PATH
   ```

   or fetch a [release](https://github.com/Leonkh/ZedXcode/releases)
   with `curl -L` (browsers quarantine downloads; if Gatekeeper complains, run
   `xattr -d com.apple.quarantine xcode-dap`), or build from source:
   `cargo install --path crates/xcode-dap`.

### Dev install (from source)

```sh
# Rust must be rustup-managed — Homebrew Rust breaks dev-extension installs.
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

git clone https://github.com/Leonkh/ZedXcode.git
cd ZedXcode
cargo build          # produces target/debug/xcode-dap
```

The Quick start below calls `xcode-dap` by name: run `./install.sh` (symlinks
the built release/debug binary into a directory on your PATH), or use the full
path `target/debug/xcode-dap`, or `cargo install --path crates/xcode-dap`.

In Zed: `zed: extensions` → **Install Dev Extension** → pick `ZedXcode/extension/`.
Then point the adapter at your local binary in Zed `settings.json` (this
override always wins):

```json
{
  "dap": { "Xcode": { "binary": "/abs/path/to/ZedXcode/target/debug/xcode-dap" } }
}
```

Details (rebuild loop, logging, binary resolution order):
[`extension/README.md`](extension/README.md).

## Quick start

```sh
# 1. One-time, user level: Xcode keybindings (⌘R/⌘B/⌘⇧K/⌘⇧O) as a marker
#    block in ~/.config/zed/keymap.json, plus an auto_install_extensions block
#    in settings.json that installs the Zed Swift extension (sourcekit-lsp, for
#    ⌘click). Timestamped backups; re-runs idempotent; --user --remove reverts.
xcode-dap setup --user

# 2. Per project: .zed/debug.json scenario, .zed/tasks.json (Build/Clean/
#    Refresh/Console + Choose Scheme/Choose Destination), buildServer.json for
#    the built-in build server. Workspace/
#    scheme/device are auto-detected; override with --workspace/--scheme/--device.
cd /path/to/YourApp
xcode-dap setup --project .

# 3. One build so sourcekit-lsp can index across modules (⌘click etc.):
xcode-dap build -w YourApp.xcworkspace -s "YourApp"
```

Open the project in Zed and press **⌘R**. The very first press opens the New
Session modal — pick your scenario (e.g. *"Run on simulator"*) **once**; every
subsequent ⌘R replays it without the modal: build → install → launch → attach,
console in the Debug Console. `xcode-dap doctor` checks the whole environment.

## Keybindings

Installed by `xcode-dap setup --user` as marker blocks in
`~/.config/zed/keymap.json` (revert with `xcode-dap setup --user --remove`):

| Binding | Action |
|---|---|
| ⌘R | `debugger::Rerun` — full build/debug loop via the Xcode adapter |
| ⌘B | `task::Spawn "Xcode: Build"` |
| ⌘⇧K | `task::Spawn "Xcode: Clean"` |
| ⌘⇧O | `project_symbols::Toggle` |

⌘K is deliberately left untouched: Zed has no clear action for the Debug
Console, and rebinding ⌘K would break both `terminal::Clear` and the
`cmd-k cmd-*` chord prefix.

## How it works

```
┌────────────────────────────── Zed ──────────────────────────────┐
│  debugger GUI: breakpoints · stack · variables · Debug Console  │
│  "Xcode Tools" extension (WASM) — registers the Xcode adapter,  │
│  resolves/downloads the xcode-dap binary                        │
└───────────────┬─────────────────────────────────────────────────┘
                │ DAP over stdio
                ▼
        xcode-dap (Rust DAP proxy) — intercepts `launch`:
        │ 1. preflight          (project generation if workspace missing)
        │ 2. xcodebuild … build (filtered log → Debug Console)
        │ 3. xcrun simctl install + launch --wait-for-debugger
        │ 4. attach by pid; tail app stdout/stderr → Debug Console
        │    everything else: byte-verbatim DAP passthrough
        ▼
        xcrun lldb-dap  ◀──debugs──▶  your app on the iOS Simulator
```

Code navigation is independent of the debugger. The Zed **Swift** extension
provides sourcekit-lsp; `xcode-dap setup --project` writes a `buildServer.json`
whose `argv` points back at **`xcode-dap bsp`** — the toolkit's own built-in
Build Server. sourcekit-lsp spawns it per workspace and asks it, per file,
"how do I compile this?"; the answer comes from a compile-args store `xcode-dap`
reconstructs from your builds (Xcode's own `.xcactivitylog` logs and the output
of `xcode-dap build`/⌘R). One full build populates the native index and the
store; ⌘click then jumps across modules. Nothing outside Xcode and Zed is
involved — details in [`docs/design/bsp-server.md`](docs/design/bsp-server.md).

Design notes: [`docs/design/dap-proxy.md`](docs/design/dap-proxy.md) (proxy
protocol, pipeline, teardown) and
[`docs/design/extension-api.md`](docs/design/extension-api.md) (Zed extension
side, binary delivery).

## Configuration reference

`.zed/debug.json` holds one or more scenarios for the `Xcode` adapter
(`xcode-dap setup --project` writes the first one). Adapter keys sit at the
top level of the scenario; Zed task variables like `$ZED_WORKTREE_ROOT` are
substituted before launch:

```jsonc
[
  {
    "adapter": "Xcode",
    "label": "Run on simulator",
    "workspace": "$ZED_WORKTREE_ROOT/YourApp.xcworkspace",
    "scheme": "YourApp",
    "device": "iPhone 15 Pro Max",
    "os": "26.3",
    "preflight": "make project CI=true"
  }
]
```

| Key | Type | Default | Meaning |
|---|---|---|---|
| `workspace` | string | **required** | Path to the `.xcworkspace` (or `.xcodeproj`) to build |
| `scheme` | string | **required** | Xcode scheme to build and run |
| `device` | string | booted iPhone, else newest available | Simulator device name (e.g. `"iPhone 15 Pro Max"`) or UDID |
| `os` | string | newest runtime for the device | Simulator OS version narrowing, e.g. `"26.3"` |
| `configuration` | string | scheme default | Build configuration (`Debug`/`Release`) |
| `preflight` | string | — | Shell command run when the workspace file is missing (project generation, e.g. Tuist); see the security note below |
| `oslog` | bool | `false` | Also pump OSLog (`log stream`) into the Debug Console, filtered to the app's own logging (subsystem == bundle id, or any image inside the .app bundle) |
| `oslogPredicate` | string | app-scoped predicate | Custom NSPredicate for the OSLog pump (`log stream --predicate`), e.g. `subsystem == \"com.example.app\"`; overrides the default filter — only used when `oslog` is `true` |
| `terminateOnStop` | bool | `true` | Terminate the app on the simulator when the session stops (Xcode Stop semantics) |
| `buildOutput` | `"filtered"` \| `"full"` | `"filtered"` | Debug Console build-log verbosity; the full log is always written to `~/.zedxcode/logs/build-latest.log` |
| `verboseLogging` | bool | `false` | Log this session at `trace` verbosity (DAP frame summaries and bodies, engine commands) to `~/.zedxcode/logs/xcode-dap.log`; never lowers a level already raised via `XCODE_DAP_LOG` |
| `derivedData` | string | xcodebuild default | Explicit DerivedData directory (`xcodebuild -derivedDataPath`). This is also where the built-in build server reads the native index store and build logs, so overriding it moves both build products and navigation data. Honored by `build`/`run`/`clean`/`setup` and the `--derived-data` CLI flag |

> **Security note.** The `preflight` command is run via `/bin/sh -c` as part
> of starting a debug session — there is no confirmation prompt, the same trust
> model as VS Code's `tasks.json`. Because `.zed/debug.json` is project-local
> configuration that travels with the repository, review it — `preflight` in
> particular — before running debug scenarios for a project you don't trust.

The proxy also keeps a diagnostic log at `~/.zedxcode/logs/xcode-dap.log`
(rotated once at 5 MB to `xcode-dap.log.old`). Its level comes from the
`XCODE_DAP_LOG` environment variable (`error`|`warn`|`info`|`debug`|`trace`,
default `info`); `"verboseLogging": true` raises a single session to `trace`.
The raise takes effect at the `launch` request (which is replayed into the
log); frames before launch (e.g. `initialize`) are only captured when
`XCODE_DAP_LOG` itself is set to `debug`/`trace`.

Note: the New Session **Launch** tab's stop-on-entry toggle is saved into the
scenario as `stopOnEntry`, but the engine currently ignores it.

The same schema powers validation and completions inside `.zed/debug.json`
([`extension/debug_adapter_schemas/Xcode.json`](extension/debug_adapter_schemas/Xcode.json)).

## CLI reference

The same binary that proxies DAP also drives the engine from a terminal or
Zed task. Run `xcode-dap <command> --help` for flags.

| Command | What it does |
|---|---|
| *(no subcommand)* | DAP proxy mode on stdio — how Zed spawns it |
| `bsp` | Built-in Build Server mode on stdio — sourcekit-lsp spawns it via `buildServer.json` to answer per-file compile-args queries; not invoked by hand |
| `build` | Build the scheme for the simulator (`-w` workspace, `-s` scheme, `--device`, `--os`, `--configuration`, `--derived-data`, `--full-output`); exit code = xcodebuild's |
| `run` | Build, install and launch on the simulator **without** the debugger; console streams to the terminal |
| `clean` | `xcodebuild clean` for the workspace/scheme (also accepts `--derived-data`) |
| `console` | Print (or `-f` follow) the current run's app console logs from `~/.zedxcode/run/<udid>/` |
| `select-scheme` | Pick the scheme interactively (or `--set`/`--list`); writes the `.zed/.zedx/selection.json` overlay used by the next run and regenerates `buildServer.json` for the new scheme |
| `select-device` | Pick the simulator destination interactively (or `--set`/`--list`); same overlay |
| `setup` | Install user-level keymap/settings marker blocks (`--user`) and per-project config (`--project <dir>`); `--remove` reverts the user blocks |
| `refresh` | Re-run the preflight (project regeneration) and refresh `buildServer.json`; prints the LSP-restart hint |
| `doctor` | Check the environment: Xcode, `lldb-dap`, simulators, sourcekit-lsp, `buildServer.json` (freshness, its recorded `build_root`/scheme, and that `argv` launches the built-in `bsp` server), the compile store, and stale state |

## Generated projects (Tuist & friends)

If your workspace is generated (Tuist, XcodeGen), set `preflight` to the
generation command — `xcode-dap` runs it automatically whenever the workspace
file is missing, and `xcode-dap build`/⌘R proceed from there.

After regenerating the project, run **`xcode-dap refresh`**: it re-runs the
preflight, refreshes `buildServer.json`, and reminds you to restart the
language server in Zed (`editor: restart language server`) so ⌘click
navigation keeps working against the new project.

## Known limitations

- **First ⌘R after a Zed restart opens the New Session modal** — Zed's
  `debugger::Rerun` needs one scenario pick per app session. Pick once, then
  the rerun loop resumes.
- **The Debug Console cannot be cleared** — current Zed has no clear action
  for the debugger console (the `console` action namespace only contains
  `WatchExpression`); this is an upstream gap.
- **⌘K only clears the focused task terminal** (Zed default `terminal::Clear`)
  — setup deliberately leaves ⌘K untouched so `cmd-k cmd-*` chords keep
  working.
- **Breakpoints can briefly show as "unverified"** right after launch — lldb
  verifies them as the app's modules load; they bind moments later and hit
  normally (including breakpoints in app startup code).

## Troubleshooting

- **Start with `xcode-dap doctor`** — it checks Xcode, `lldb-dap`, simulators,
  sourcekit-lsp, `buildServer.json` (presence, freshness, its recorded
  DerivedData `build_root` and scheme, and that `argv` launches the built-in
  `bsp` server), the compile store, and stale state.
- **"Xcode" missing from the New Session adapter list** — the extension isn't
  installed/built; for dev extensions hit *Rebuild* on the extension card.
- **Dev extension install fails** — your Rust is likely Homebrew-installed;
  Zed requires a rustup-managed toolchain to build WASM extensions.
- **Binary download fails (offline / GitHub unreachable)** — the extension
  reuses any previously cached binary; otherwise install one manually
  (`cargo install --path crates/xcode-dap`) or set `dap.Xcode.binary`.
- **Task fails with exit 127 ("command not found")** — Zed task shells don't
  inherit your PATH; that's why `xcode-dap setup --project` writes the
  **absolute** binary path into `.zed/tasks.json`. If you moved or rebuilt the
  binary elsewhere, re-run `xcode-dap setup --project .` (or put `xcode-dap`
  on PATH via `./install.sh`).
- **⌘click doesn't cross module boundaries** — run one full build, ensure
  `buildServer.json` exists (`xcode-dap setup --project`), then restart the
  language server. After Tuist regeneration, run `xcode-dap refresh`.
- **Go-to-definition broke (after a project clean / new worktree / subfolder
  window)** — a clean that deletes `buildServer.json` leaves sourcekit-lsp
  with no Build Server, so it silently falls back to SPM mode on a root
  `Package.swift` — the signature is macOS fallback args, "Could not load
  module" errors, and jumps to wrong definitions. A clean that only wipes
  DerivedData keeps `buildServer.json` but removes the native index store and
  the build logs the compile store was built from, so navigation degrades
  until one build repopulates them. Fix either way: build once — ⌘B/⌘R
  regenerates `buildServer.json` if needed and re-feeds the compile store from
  the build output — or run `Xcode: Refresh`. The built-in build server folds
  each new build in live and re-queries sourcekit-lsp, so **no language-server
  restart is needed for ordinary rebuilds**; restart only when `xcode-dap`
  prints the hint (it does so when the DerivedData path or the binary changed,
  or when `buildServer.json` had to be created from scratch). Also: every git worktree needs its own `xcode-dap setup
  --project .`; always open the repo **root** as the Zed project folder
  (sourcekit-lsp only finds `buildServer.json` at the LSP root); and only files
  compiled by the configured scheme get exact compiler flags — a brand-new
  file borrows a sibling's flags until the next build.
- **OSLog output is noisy** — the default predicate already scopes to the
  app's own logging; tighten it further with `oslogPredicate` in
  `.zed/debug.json`, or set `"oslog": false` to keep stdout/stderr only.
- **App hangs on launch / attach timeout** — the app starts suspended under
  `--wait-for-debugger`; if attach fails the proxy reports an error and
  terminates the app. Stop the session and press ⌘R again.
- **Gatekeeper blocks a manually downloaded binary** — browsers quarantine
  downloads; use `curl -L` or `xattr -d com.apple.quarantine xcode-dap`.
- Extension logs: run `zed --foreground`; Zed log: `zed: open log`. Full
  build log: `~/.zedxcode/logs/build-latest.log`.
- **Proxy diagnostic log**: `~/.zedxcode/logs/xcode-dap.log` records every
  session (pipeline milestones, simctl/xcodebuild commands with exit status
  and duration, attach/teardown steps). Raise verbosity with
  `XCODE_DAP_LOG=debug` (DAP frame summaries) or `trace` / `"verboseLogging":
  true` (frame bodies, truncated) when filing a bug.

## Uninstall

```sh
xcode-dap setup --user --remove   # reverts the user-level keymap/settings blocks
```

Then uninstall **Xcode Tools** from `zed: extensions`, and optionally delete
the generated per-project files (`.zed/debug.json`, `.zed/tasks.json`,
`buildServer.json` — all git-ignored via `.git/info/exclude`) and the state
directory `~/.zedxcode/`.

## Development

```sh
cargo test                                        # unit + integration tests
python3 tests/dap_smoke.py roundtrip              # DAP framing roundtrip vs the real binary
python3 tests/dap_smoke.py session --mock-pipeline  # full DAP session, no Xcode build needed
./install.sh                                      # symlink the built binary onto your PATH
```

`tests/dap_smoke.py` is a scripted DAP client that drives the proxy over
stdio; `--mock-pipeline` skips xcodebuild/simctl and attaches lldb-dap to a
dummy process, so the whole protocol loop runs offline in seconds.
`install.sh` puts `target/release/xcode-dap` (or the debug build) on PATH for
terminal use — Zed itself doesn't need it, the generated tasks embed the
absolute path.

## License

[Apache-2.0](LICENSE).
