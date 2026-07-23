# Xcode Tools for Zed

Xcode-like build & debug for iOS Simulator projects, inside the
[Zed](https://zed.dev) editor. This extension registers the **Xcode** debug
adapter: pick a scenario once, then every rerun builds the workspace
(`xcodebuild`), installs and launches the app on a simulator (`simctl`), and
attaches `lldb` — with build phases and app console output streamed into
Zed's **Debug Console**.

## Requirements

- macOS with Xcode installed (`xcodebuild`, `xcrun simctl`, `lldb-dap`).
- Zed with extension API 0.7.0 support.

## Quick start

Create `.zed/debug.json` in your iOS project (or let `xcode-dap setup`
generate it):

```jsonc
[
  {
    "adapter": "Xcode",
    "label": "Run on simulator",
    "workspace": "$ZED_WORKTREE_ROOT/MyApp.xcworkspace",
    "scheme": "MyApp",
    "device": "iPhone 15 Pro Max"
  }
]
```

`workspace` and `scheme` are required; the bundled JSON schema provides
validation and completions for the remaining keys (`device`, `os`,
`configuration`, `preflight`, `oslog`, `oslogPredicate`, `terminateOnStop`,
`buildOutput`, `verboseLogging`, `derivedData`).

Note: the generic **Launch** tab's stop-on-entry toggle is saved into the
scenario as `stopOnEntry`, but the engine currently ignores it.

## How the engine binary is resolved

The pipeline is owned by the native `xcode-dap` binary, which the extension
resolves automatically in this order:

1. the `dap.Xcode.binary` Zed setting (always wins) — point it at a locally
   built proxy during development:

   ```json
   {
     "dap": {
       "Xcode": { "binary": "/path/to/ZedXcode/target/debug/xcode-dap" }
     }
   }
   ```

2. `xcode-dap` found on `PATH`;
3. the pinned GitHub release, downloaded and cached in the extension work dir.

To build the proxy yourself, run `cargo build -p xcode-dap` from the repo
root — not from `extension/`, which only holds this WASM shim.

## Full documentation

Setup, CLI subcommands (`build`, `run`, `setup`, `doctor`, …), and
troubleshooting are covered in the
[main repository README](https://github.com/Leonkh/ZedXcode).

## License

Apache-2.0
