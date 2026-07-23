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
- [ ] A `xcode-dap-v<version>` GitHub release exists with **both** arch assets
      (`<tag>-aarch64-apple-darwin.tar.gz`, `<tag>-x86_64-apple-darwin.tar.gz`)
      matching `PROXY_TAG` in `extension/src/lib.rs` — push the tag and let
      the release workflow produce them.
- [ ] The version is consistent in all four places: `extension/extension.toml`
      `version`, the `PROXY_TAG` constant, the git tag of the release, and the
      `xcode-dap` crate version (`crates/xcode-dap/Cargo.toml`). Release CI
      hard-fails the tag build unless `xcode-dap --version` equals the tag
      suffix, so a crate/tag mismatch never reaches a published release.
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

1. Bump `version` in `extension/extension.toml` (e.g. `0.2.0`).
2. Bump `PROXY_TAG` in `extension/src/lib.rs` to `xcode-dap-v0.2.0` and the
   `xcode-dap` crate version (`crates/xcode-dap/Cargo.toml`) to match — release
   CI hard-fails if the crate version and the tag disagree.
3. Tag and push `xcode-dap-v0.2.0` — the release workflow builds, signs and
   uploads both arch assets. Verify the asset names against the contract in
   `extension/src/lib.rs` before proceeding.
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
