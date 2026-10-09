#!/bin/bash
# gate.sh — the full check a change passes before it lands, the same locally
# and in CI. Exits non-zero at the first failing step.
#
#   fmt, clippy (-D warnings) and tests for the workspace; fmt, the wasm check
#   and clippy for the extension; a debug build that the DAP and BSP smoke
#   tests run; the version check and the release script's test; the
#   dependency and fixture checks; a release build.
#
# Hermetic: HOME and TMPDIR point into a temp dir for the whole run, so the
# tests and smoke runs never read or write ~/.zedxcode or ~/.config/zed
# (CARGO_HOME and RUSTUP_HOME keep their real locations, so the installed
# toolchains and the crate cache still work). Both builds go to a scratch
# target dir inside it: the gate never touches target/release, which is what
# a local deploy (`cargo build --release`) installs from.
#
# Works from any directory. Off macOS, the steps that need Xcode tools skip
# with a message: the DAP smoke tests need `xcrun lldb-dap` (an xcrun shim on
# PATH that maps it to a local lldb-dap is enough), and check-fixtures needs
# xcodebuild. CI runs everything on macOS.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"

work="$(mktemp -d "${TMPDIR:-/tmp}/zedx-gate.XXXXXX")"
trap 'rm -rf "$work"' EXIT
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}" RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
export HOME="$work/home" TMPDIR="$work/tmp"
mkdir -p "$HOME" "$TMPDIR"
# It points setup at a real Zed profile; nothing in the gate may write there.
unset ZEDXCODE_ZED_CONFIG_DIR

step() {
  printf '\n== %s\n' "$*"
}

step "cargo fmt"
cargo fmt --all -- --check

step "cargo clippy"
cargo clippy --workspace --all-targets --locked -- -D warnings

step "cargo test"
cargo test --workspace --locked

step "extension: fmt, wasm check, clippy"
(cd extension && cargo fmt -- --check \
  && cargo check --target wasm32-wasip2 --locked \
  && cargo clippy --target wasm32-wasip2 --locked -- -D warnings)

step "debug build (scratch target dir)"
CARGO_TARGET_DIR="$work/target" cargo build --locked # the binary the smoke tests run
bin="$work/target/debug/xcode-dap"

step "DAP smoke: roundtrip, mock session, Stop killed after its answer"
if xcrun --find lldb-dap >/dev/null 2>&1; then
  python3 tests/dap_smoke.py --binary "$bin" roundtrip
  python3 tests/dap_smoke.py --binary "$bin" session --mock-pipeline
  python3 tests/dap_smoke.py --binary "$bin" session --mock-pipeline --kill-after-response
elif [[ "$(uname -s)" != Darwin ]]; then
  echo "skipped: needs lldb-dap through xcrun (macOS; elsewhere an xcrun shim on PATH)"
else
  echo "gate: xcrun cannot find lldb-dap; install Xcode or select it with xcode-select" >&2
  exit 1
fi

step "DAP smoke: stdout purity"
python3 tests/dap_smoke.py --binary "$bin" purity

step "BSP smoke"
python3 tests/bsp_smoke.py --binary "$bin"

# One command per line: under `set -e`, a failure in front of a `&&` does not
# stop the script.
step "versions"
scripts/check-versions.sh
step "release script (in a throwaway copy)"
python3 tests/release_smoke.py
step "dependencies"
scripts/check-deps.sh
step "fixtures"
scripts/check-fixtures.sh

step "release build (scratch target dir)"
CARGO_TARGET_DIR="$work/target" cargo build --release --locked # never target/release

printf '\ngate: ok\n'
