# The built-in Build Server (`xcode-dap bsp`)

> **Shipped feature.** This document describes the Build Server that powers
> ⌘click navigation. Where it drifts from the code, the sources are
> authoritative: `crates/xcode-dap/src/bsp/` (`mod.rs`, `server.rs`,
> `ingest.rs`), `crates/xcode-dap/src/engine/compile_store.rs`,
> `crates/xcode-dap/src/engine/xcactivitylog.rs`, and
> `crates/xcode-dap/src/setup/build_server.rs` (the `buildServer.json`
> generator).

## What it is and why

sourcekit-lsp gives Zed cross-module go-to-definition, hover and references —
but only if something tells it *how each file is compiled* (the exact
`swiftc` arguments and where the index lives). That "something" is a **Build
Server**: a process sourcekit-lsp discovers through a `buildServer.json` at the
project root and speaks to over [BSP](https://build-server-protocol.github.io/).

`xcode-dap` ships its own Build Server as the hidden `bsp` subcommand. There is
no separate tool to install: `xcode-dap setup --project` writes a
`buildServer.json` whose `argv` points back at the same `xcode-dap` binary with
the `bsp` argument, and sourcekit-lsp spawns one instance per workspace. The
server reconstructs per-file compile arguments from Xcode's own build artifacts
and answers sourcekit-lsp's queries from that. Everything it needs —
`xcodebuild`, DerivedData, the index store, the `.xcactivitylog` build logs —
is produced by Xcode itself.

### `buildServer.json`

Written deterministically by `setup/build_server.rs::render`:

```json
{
  "name": "xcode-dap",
  "version": "<crate version>",
  "bspVersion": "2.2.0",
  "languages": ["c", "cpp", "objective-c", "objective-cpp", "swift"],
  "argv": ["/usr/local/bin/xcode-dap", "bsp"],
  "workspace": "/Users/x/MyApp/MyApp.xcworkspace",
  "build_root": "/Users/x/Library/Developer/Xcode/DerivedData/MyApp-abcdefghijklmnopqrstuvwxyzab",
  "scheme": "MyApp",
  "kind": "xcode"
}
```

`name` / `version` / `bspVersion` / `languages` / `argv` are the fields
sourcekit-lsp's decoder requires. `workspace` / `build_root` / `scheme` /
`kind` are our private extensions, read back by the server and by `doctor`.
`argv[0]` is the canonicalised running binary, so the file keeps working
regardless of PATH.

## Protocol dialect

The server speaks the modern **"pull"** BSP dialect that sourcekit-lsp
prefers: it advertises itself as a source-kit options provider at initialize
and then answers per-file option requests on demand, instead of pushing a
static settings table. Wire framing mirrors DAP mode — `Content-Length`-framed
JSON-RPC over stdio, reusing `dap::framing` — and, as in DAP mode, **stdout
carries only framed JSON-RPC**; every diagnostic goes to the log file / stderr.

A single-writer task drains one channel to stdout so replies from the read
loop and the concurrent options / ingest tasks never interleave.

### `build/initialize`

Anchors the session to the client's `rootUri` (percent-decoded by hand — paths
carry spaces and non-ASCII and the crate takes no new dependency), loads
`<root>/buildServer.json`, and replies with the pull-dialect capabilities:

```json
{
  "displayName": "xcode-dap",
  "bspVersion": "2.2.0",
  "dataKind": "sourceKit",
  "data": {
    "indexStorePath": "<build_root>/Index.noindex/DataStore",
    "indexDatabasePath": "~/.zedxcode/cache/index-db-<hash>",
    "sourceKitOptionsProvider": true
  }
}
```

`dataKind: "sourceKit"` + `sourceKitOptionsProvider: true` are exactly what
make sourcekit-lsp pull options per file rather than downgrading to the legacy
push protocol.

- **`indexStorePath`** — `<build_root>/Index.noindex/DataStore`, the native
  index store that Xcode's index-while-building fills during a normal build.
  This is what gives cross-module jumps their precision.
- **`indexDatabasePath`** — `~/.zedxcode/cache/index-db-<fnv1a64(indexStorePath)>`,
  created eagerly so sourcekit-lsp's `IndexStoreDB` can open it. Keyed on the
  index-store path so distinct workspaces never collide.

### `workspace/buildTargets` + `buildTarget/sources`

sourcekit-lsp needs *a* target to hang documents off, but the real
per-file arguments come from the options request, so the model is deliberately
trivial: one opaque dummy target (`dummy://dummy`), and `buildTarget/sources`
reports the project root as a single directory source (`kind: 2`). Every
document that descends from the root therefore maps to the dummy target, and
the per-file answer does the actual work.

### `textDocument/sourceKitOptions`

The heart of the server. For a requested file it returns:

```json
{ "compilerArguments": ["-module-name", "…", "…"], "workingDirectory": "…" }
```

resolved from the compile store (below). Each request is answered off the read
loop on its own task, so a request that has to wait for the cold-start
bootstrap never stalls other messages; replies are matched by JSON-RPC `id`, so
out-of-order completion is fine. A request that arrives before the bootstrap
finishes waits up to 60 s, then serves whatever the store already holds rather
than answering `null`.

### `buildTarget/didChange` (server → client)

Whenever the store changes — a new build folded in, a scheme switch, an
externally-written store adopted — the server pushes
`buildTarget/didChange` with `changes: null` ("everything changed").
sourcekit-lsp re-queries `sourceKitOptions` for open files. **Compiler-argument
changes propagate live; no editor or language-server restart is required for a
rebuild.**

### Lifecycle notifications

`build/initialized` starts the ingest task (guarded so it can only start once);
`build/exit` and stdin EOF exit the process cleanly (the poll loop never
returns on its own, so the exit is explicit); `workspace/didChangeWatchedFiles`
and `$/cancelRequest` are no-ops (sources come from build logs, not file
watchers, and all work is cheap).

## Where the compile arguments come from: dual-source ingestion

The store is fed from two independent sources so that builds made **either**
from the CLI/Zed **or** in Xcode.app both refresh navigation.

### 1. stdout ingest (the reliable path for CLI builds)

After every successful `xcode-dap build` / ⌘R / ⌘B, the build pipeline parses
its own just-captured `xcodebuild` output (from
`~/.zedxcode/logs/build-latest.log`) for the per-module `swiftc` invocations
and merges them into the store for `(build_root, scheme)`
(`engine/pipeline.rs::ingest_build_log`). This runs under the same opt-in gate
as the `buildServer.json` regen, so it never creates a store for a project that
never opted in.

Why parse stdout at all, when Xcode writes `.xcactivitylog` logs?
**Finding (Xcode 26.3):** command-line `xcodebuild` builds write an
`.xcactivitylog` into an existing DerivedData only unreliably — often not at
all — so the log-polling path alone would miss most CLI builds. The captured
stdout is always present, so stdout ingest is the dependable path for builds
`xcode-dap` drives itself.

### 2. `.xcactivitylog` polling (covers Xcode.app builds)

A ~1 s poll loop (`bsp/ingest.rs`) watches DerivedData's build-log store
(`<build_root>/Logs/Build/`, indexed by `LogStoreManifest.plist`) and folds in
the newest `.xcactivitylog` for the scheme whenever a new one appears — so a
build you ran **inside Xcode.app** refreshes Zed's navigation too. The manifest
gate skips in-flight / cancelled builds (which leave an unregistered 0-byte
log), so a partial log is never ingested.

The `.xcactivitylog` reader (`engine/xcactivitylog.rs`) decompresses the
gzip-wrapped SLF0 token stream and tokenizes it properly (string tokens are
length-prefixed and may themselves contain delimiter bytes or newlines, so a
naive substring scan would desync), extracting each module's `-module-name`,
working directory, `-index-store-path`, inline `.swift` files, and
`@…SwiftFileList` response files.

### Cold-start replay

On first launch the store is empty, so the bootstrap replays **every** retained
`.xcactivitylog` for the scheme, oldest → newest, and merges them. Because
`merge` is per-module last-wins, replaying the full history reconstructs the
module coverage a single (newest) log would miss — the server is as complete
from its first request as a long-lived one. A warm store (a prior session's
on-disk cache) skips the replay and lets the poll loop catch up from its
persisted watermark.

### Store-mtime watch (adopting the pipeline's writes)

The build pipeline and the running `bsp` server write the *same*
`(build_root, scheme)` store file. The poll loop compares the store file's
mtime against what `bsp` itself last wrote; a difference means another process
(a CLI build's stdout ingest) updated it, so `bsp` reloads the store and pushes
`didChange`. This is what makes a `xcode-dap build` in a terminal refresh a
`bsp` server already running under Zed.

### `buildServer.json`-mtime watch (scheme / build-root switch)

The same poll loop stats `buildServer.json`; when `select-scheme` / `refresh`
rewrites it with a different `scheme` or `build_root`, `bsp` swaps in the store
for the new pair (a different `(build_root, scheme)` is a different cache file)
and pushes `didChange`. A scheme-only switch needs no language-server restart;
only an `argv` or `build_root` change does, which is why the generator's
`restart_hint` is `false` for scheme-only rewrites.

## The compile store

`engine/compile_store.rs` — a persistent, per-`(build_root, scheme)` map from
module name to its compile invocation.

- **Location:** `~/.zedxcode/cache/compile-store-<fnv1a64(build_root)>-<fnv1a64(scheme)>-<sanitized-scheme>.json`.
  The build-root path is hashed, so no raw path text lands on disk. The scheme is
  hashed too — so schemes that sanitize alike (e.g. `App/Dev` and `App:Dev` both
  → `App-Dev`) get distinct files — followed by a sanitized-scheme segment (every
  char outside `[A-Za-z0-9._-]` → `-`) kept only as a human-readable aid; the
  stored `scheme`/`build_root` fields are a further mismatch safety net on load.
- **Schema** (`STORE_VERSION = 1`; a stored file with a different version is
  ignored and treated as empty): `{ version, build_root, scheme, modules,
  last_log }`, where each module entry is `{ args, working_dir, files,
  file_lists, index_store_path }`. `args` has `argv[0]` (the `swiftc` path)
  dropped and keeps `@…SwiftFileList` response files verbatim. `last_log` is
  the newest ingested build-log filename — the poll watermark.
- **Fail-soft:** a missing, unreadable, corrupt, or version-mismatched file
  loads as an empty store, never an error — a broken cache must not break
  serving.
- **Incremental merge:** merging replaces the entries for the modules present
  in a parse and keeps every other module, so rebuilding one module never drops
  the rest. A zero-module parse (e.g. a clean rebuild whose log yielded nothing)
  is a strict no-op.
- **Serve-time expansion:** `@…SwiftFileList` response files are expanded when
  the options are served, so files **added since the last build** are picked up
  without a re-parse; a handful of sourcekit-hostile arguments are dropped.

### New-file sibling fallback

A `.swift` file the store has never seen — created since the last build — has
no module of its own. Rather than answer `null` (which would leave the new file
with no semantics at all), the store borrows a sibling module's arguments: the
module owning the same directory, else the nearest ancestor directory up to the
project root (or a `.git` boundary), with the new file's own path appended.
This holds until the next build indexes the file for real.

## Limitations

- **Swift arguments, v1.** The store parses `swiftc` invocations; the server
  advertises the C-family languages sourcekit-lsp expects, but per-file options
  for C / Objective-C / C++ files are out of scope for this version.
- **Locally-uncompiled modules have no data.** A module the project generator
  substitutes with a **prebuilt / binary-cached** artifact is never compiled
  in this build root, so it has neither index units in the DataStore nor a
  `swiftc` invocation in the logs. Cross-module definition *into* such a module
  returns `null` (hover still works from the interface). This is an index-store
  gap, unchanged from before the built-in server existed; the fix lives on the
  consuming project's side (build those modules from source, e.g. a source /
  non-binary cache profile).
- **One build is still required.** Cross-module navigation needs an index and
  arguments, so the first full build after setup (or after a DerivedData wipe)
  is what turns navigation on — see the quick-start.

## Related

- [`dap-proxy.md`](dap-proxy.md) — the DAP proxy, pipeline and the build phase
  that produces the index store and logs this server reads.
- [`extension-api.md`](extension-api.md) — the Zed extension side and binary
  delivery.
