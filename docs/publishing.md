# Publishing the extension to the Zed registry

Checklist for getting **Xcode Tools** (`xcode-tools`) into
[`zed-industries/extensions`](https://github.com/zed-industries/extensions),
and for shipping updates afterwards. The registry packages only the
`extension/` directory of this monorepo (via the `path` key); the native
`xcode-dap` binary is never built by the registry — it ships through the
GitHub releases produced by [`.github/workflows/release.yml`](../.github/workflows/release.yml).

## Prerequisites (verify before the PR)

- [ ] `LICENSE` (Apache-2.0) exists at the **repo root** — required by the
      registry since 2025-10-01; Apache-2.0 is on the accepted list.
- [ ] `extension/LICENSE` exists (a symlink to the root `LICENSE` is fine —
      git stores it as a symlink blob): the registry validates that a license
      is present at the extension path (`extension`), not only at the repo
      root.
- [ ] `extension/extension.toml` has a unique `id = "xcode-tools"` that
      contains neither `zed` nor `extension` (immutable after publish).
- [ ] `extension/` builds for `wasm32-wasip2` with a rustup toolchain:
      `cargo build --release --target wasm32-wasip2` inside `extension/`.
- [ ] A `xcode-dap-v<version>` GitHub release exists with the Apple silicon
      asset `<tag>-aarch64-apple-darwin.tar.gz` and `SHA256SUMS.txt`,
      matching `PROXY_TAG` in `extension/src/lib.rs` — push the tag and let
      the release workflow produce them. There is no Intel asset: Intel Macs
      are not supported.
- [ ] The git tag of the release equals all five version declarations:
      `crates/xcode-dap/Cargo.toml`, `crates/xcode-dap-config/Cargo.toml`,
      `extension/Cargo.toml`, `extension/extension.toml` `version`, and the
      `PROXY_TAG` constant. `scripts/check-versions.sh <tag>` checks them;
      release CI runs it with the pushed tag and hard-fails on any mismatch,
      so a version/tag mismatch never reaches a published release.
- [ ] The extension has been tested end-to-end as an installed dev extension
      (build, then a real debug run: cmd-R to launch with a breakpoint hit)
      before opening the PR — the Zed team closes untested submissions
      eagerly.

## First publish

1. Fork `zed-industries/extensions` to a **personal** GitHub account
   (org forks break the submodule automation).
2. In the fork, add this repo as a submodule — **HTTPS URL, never SSH**:

   ```sh
   git submodule add https://github.com/Leonkh/ZedXcode.git extensions/xcode-tools
   ```

3. Add the registry entry to `extensions.toml` (the `path` key points at the
   extension directory inside the monorepo):

   ```toml
   [xcode-tools]
   submodule = "extensions/xcode-tools"
   path = "extension"
   version = "0.1.0"          # must equal extension/extension.toml version
   ```

4. Sort the manifest and commit:

   ```sh
   pnpm sort-extensions
   ```

5. Open a PR against `zed-industries/extensions`. CI builds the WASM from
   `extension/`; once merged, "Xcode Tools" appears in `zed: extensions`.

## Shipping an update

Versioning flow — the extension version and the proxy release tag move in
lockstep: every extension version bump re-tags and re-releases the proxy at the
same version, even when only the extension changed:

1. Run `scripts/release.sh 0.2.0` (add `--dry-run` first to see every change
   without writing anything). It sets the five version declarations —
   `version` in `extension/extension.toml`, `PROXY_TAG` in
   `extension/src/lib.rs` (`xcode-dap-v0.2.0`) and the three crate versions
   in `crates/xcode-dap/Cargo.toml`, `crates/xcode-dap-config/Cargo.toml` and
   `extension/Cargo.toml` — and refreshes `Cargo.lock` and
   `extension/Cargo.lock` offline (`cargo update --workspace --offline` at the
   repository root and in `extension/`): every `--locked` build, the release
   gate included, fails on a lockfile that still records the old versions.
   Then it runs `scripts/gate.sh` and stages exactly those seven files. It
   never commits or tags; release CI hard-fails if any declaration and the
   tag disagree (`scripts/check-versions.sh xcode-dap-v0.2.0`).
2. In `CHANGELOG.md`, move the Unreleased notes under `## [0.2.0]` with the
   date and the Xcode and Zed versions the release was tested with, and stage
   it. Commit with the title the script prints (`Release 0.2.0`).
3. Tag and push `xcode-dap-v0.2.0` — the release workflow runs the gate,
   then builds, signs and uploads the Apple silicon asset and
   `SHA256SUMS.txt`. Verify the asset name against the contract in
   `extension/src/lib.rs` before proceeding. To rehearse first, run the
   workflow by hand (`workflow_dispatch`): that dry run builds, signs and
   packages the same files and keeps them as a workflow artifact, without
   creating a release.
4. In the `zed-industries/extensions` fork:

   ```sh
   git submodule update --remote extensions/xcode-tools
   ```

   then bump `version` under `[xcode-tools]` in `extensions.toml`,
   `pnpm sort-extensions`, commit, PR.

## Gotchas

- The registry entry's `version` must exactly equal
  `extension/extension.toml`'s `version`, or registry CI rejects the PR.
- `id` is immutable; renaming means publishing a new extension.
- Keep `extension/` free of `process:exec` usage so the manifest needs no
  `[capabilities]` section.
- `zed_extension_api` is pinned to `0.7.0` — the maximum supported by stable
  Zed 1.6.3. Revisit when 0.8.0 reaches stable Zed.
