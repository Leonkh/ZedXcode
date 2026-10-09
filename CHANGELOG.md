# Changelog

Notable changes to ZedXcode: the `xcode-dap` binary and the Xcode Tools Zed
extension, which share one version number. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). Every release names
the Xcode and Zed versions it was tested with.

## [Unreleased]

Tested with: recorded when this becomes a release.

### Added

- One gate for every change, `scripts/gate.sh`, the same locally and in CI,
  which runs it on every push and pull request: formatting, lints, the unit
  tests, the extension's wasm check, the DAP and Build Server smoke tests, a
  check that the debug adapter never prints anything but protocol messages on
  stdout, and checks that the five version declarations agree, that every
  dependency is on the approved list (`deps.allow`) and that the scenario
  schema matches the launch options the binary reads.
- CI also checks that the extension compiles with Rust 1.90, the toolchain
  Zed's extension registry builds with (now declared as the extension's
  `rust-version`), and builds synthetic sample projects with
  `xcode-dap build`: a neutral `MyApp` and common project layouts (an
  `.xcworkspace`, a nested `ios/` folder, XcodeGen, Tuist, a local Swift
  package).
- A simulator smoke test that builds, installs and launches the sample app on
  Xcode 26 and on Xcode 27, for release tags, on demand, and for pull requests
  that touch the launch path.
- `scripts/release.sh X.Y.Z` prepares a release: it sets all five version
  declarations, refreshes both lockfiles, runs the gate and stages the
  result, without committing or tagging; `--dry-run` shows every change and
  writes nothing. This changelog records each release.
- The unit tests build and run on Linux and other non-macOS hosts.
- One selection store per project, `.zed/.zedx/selection.json` (version 2),
  holds the chosen scheme, destination (UDID, name and iOS version) and
  configuration, and the five most recent schemes and destinations. A 0.1
  file still reads and is rewritten by the next pick. Writes lock the file,
  re-read it and keep every key they do not know.
- A git worktree without its own choice uses the main checkout's: it reads
  the main checkout's store, and nothing is copied into the worktree.
- `xcode-dap select-configuration` chooses the build configuration
  (interactively, or with `--set` and `--list`); for a workspace it lists the
  configurations of the project that holds the chosen scheme, else of the
  first project that lists any. `select-scheme`, `select-device` and
  `select-configuration` take `--reset`, which forgets the project's choice:
  the next build takes that value from the main checkout's choice, the
  scenario or the automatic choice. `select-destination` is another name for
  `select-device`.

### Changed

- `workspace` and `scheme` are optional in the Xcode scenario. Without
  `workspace`, the workspace or project found in the project folder is built;
  without a chosen scheme, the container's only scheme. Each of Scheme,
  Destination and Configuration comes from the first place that sets it: the
  selection store, the main checkout's store (in a worktree), the scenario's
  `scheme` / `device` + `os` / `configuration` keys, then the automatic choice.
  Before anything boots, a scheme the container does not have, a
  configuration an `.xcodeproj` does not list, several schemes with none
  chosen, and a destination that is not available stop with a message that
  names the picker; one console line shows each value and where it came
  from.
- `xcode-dap build`, `run` and `clean` need no flags: the workspace is found
  in the project, and Scheme, Destination and Configuration come from the
  project's choices. `--scheme`, `--destination` (a UDID or a simulator name;
  0.1's `--device` still works), `--os` and `--configuration` override them
  for that one command and are never saved. The tasks 0.1's setup wrote into
  `.zed/tasks.json` keep 0.1's order: the project's choices outrank their
  baked flags (in a worktree the main checkout's too, so ⌘B builds what ⌘R
  builds), and each run prints one line on how to migrate.
- A scenario that does not parse no longer fails before the binary is
  downloaded: Xcode Tools downloads `xcode-dap` first, then names the key at
  fault and says where Scheme, Destination and Configuration are chosen.
- ⌘R, ⌘B, the destination picker, `doctor` and `setup` share one simulator
  list. Renamed simulators are listed (by device type, not by name); the iOS
  version matches loosely (`26`, `26.3` and `26.3.1` all find iOS 26.3, and a
  version that matches several runtimes takes the newest with a warning); a
  chosen destination is found by UDID first, then by name and iOS version.
  The automatic destination is the booted iPhone, else the newest iPhone (by
  model number) on the newest iOS runtime (`setup` no longer takes a booted
  iPad).
- The scheme list is cached until a scheme is added or removed in Xcode, not
  only until the workspace file changes.
- Releases are built on macOS 26 and published only after the gate passes; the
  release workflow can also run as a dry run that publishes nothing.
- `setup --project` and `select-scheme` find the Xcode workspace or project
  up to two folders below the project root, such as `ios/MyApp.xcworkspace`
  in a React Native app. They skip `Pods`, `node_modules`, build output,
  hidden folders, bundles such as playgrounds, and nested git checkouts
  (submodules, worktrees). A shallower container wins, and a workspace wins
  over a project in the same folder; when several still tie, they list all of
  them instead of picking one. When a `project.yml` or Tuist manifest has not
  generated its project yet, they say so and name the command that generates
  it. `doctor` uses the same search, but takes a container below the folder
  it runs in only when that folder holds `.zed/` or `buildServer.json` or is
  a git repository's top folder.
- `select-scheme` refreshes `buildServer.json` in the project root, where
  `setup` writes it, also when the workspace is in a subfolder.

### Removed

- Intel Mac support: releases no longer carry an x86_64 binary, and on an
  Intel Mac Xcode Tools says that it supports Apple silicon Macs only.

### Fixed

- Pressing ⌘R while the app is still running no longer stalls in the install
  step: the previous session ends and terminates its app as soon as the new
  build succeeds.
- Stop no longer hangs when lldb-dap stops responding during a disconnect, and
  it still terminates the app.
- A session replaced by a rerun now says so in the Debug Console and ends
  cleanly instead of disappearing without a word.
- Stop finishes its cleanup before it answers Zed, which ends the adapter as
  soon as Stop is answered: the app is terminated (per `terminateOnStop`), the
  OSLog stream and lldb-dap are stopped and the simulator's pidfile is
  released first. The cleanup takes at most 2 seconds. Zed's `terminate`
  request is handled the same way as `disconnect`.
- Stop while the simulator is being looked up, or during the install or
  launch step, takes effect at once instead of after the step, and a launch
  stopped half-way no longer leaves the app suspended. A Stop during the
  build is answered within 6 seconds even when a step hangs.
- When the debugger does not attach within 30 seconds, the app, suspended
  while it waited for the debugger, is terminated and the run fails with a
  message.

## [0.1.0] - 2026-07-23

First release.

Tested with: not recorded. The design notes were checked against Xcode 26.3
and Zed 1.6.3; the extension requires Zed 1.6.3 or later.

### Added

- ⌘R in Zed builds the scheme with `xcodebuild`, installs and launches the app
  on an iOS simulator with lldb attached through `lldb-dap`, and streams the
  build log and the app's stdout and stderr into the Debug Console;
  breakpoints, stepping, the stack and variables work in Zed's debugger.
  Rerunning while the app runs relaunches it, and Stop terminates the app
  (`terminateOnStop`).
- The Xcode Tools extension registers the `Xcode` debug adapter and downloads
  the matching `xcode-dap` release binary on first use; the
  `dap.Xcode.binary` setting and an `xcode-dap` on the PATH take precedence.
- `xcode-dap setup --user` adds Xcode key bindings (⌘R, ⌘B, ⌘⇧K, ⌘⇧O) and has
  Zed install its Swift extension, as marker blocks with backups
  (`setup --user --remove` reverts them). `xcode-dap setup --project` writes
  the debug scenario, the tasks (Build, Clean, Refresh, Console, Choose
  Scheme, Choose Destination) and `buildServer.json`, and detects the
  workspace, scheme and device.
- A built-in Build Server, `xcode-dap bsp`, that hands sourcekit-lsp the
  compiler arguments of your builds, for go-to-definition, hover and
  references across modules.
- Scheme and simulator pickers (`select-scheme`, `select-device`); the next
  run uses the choice.
- The commands `build`, `run` (without the debugger), `clean`, `console`,
  `refresh`, and `doctor`, which checks the whole environment.
- Scenario options `device`, `os`, `configuration`, `preflight` (generates the
  project when the workspace is missing), `oslog` and `oslogPredicate`,
  `buildOutput`, `verboseLogging` and `derivedData`.
- A diagnostic log, `~/.zedxcode/logs/xcode-dap.log`, rotated at 5 MB, with
  its level set by `XCODE_DAP_LOG`.
- Release binaries for Apple silicon and Intel Macs.

### Known issues

- On a Mac whose only Xcode is Xcode 27, every ⌘R stops in the boot phase:
  this release needs Simulator.app, which Xcode 27 replaced with Device Hub.
  Keep an Xcode 26 installed alongside.
- The first ⌘R after a Zed restart opens the New Session modal; pick the
  scenario once and the rerun loop resumes.
- The Debug Console cannot be cleared: Zed has no clear action for it.
- The Launch tab's stop-on-entry toggle (`stopOnEntry`) is ignored.

[Unreleased]: https://github.com/Leonkh/ZedXcode/compare/xcode-dap-v0.1.0...HEAD
[0.1.0]: https://github.com/Leonkh/ZedXcode/releases/tag/xcode-dap-v0.1.0
