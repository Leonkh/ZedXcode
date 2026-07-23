#!/bin/bash
# install.sh — put the locally built xcode-dap on PATH via a symlink.
#
# Prefers target/release/xcode-dap, falls back to target/debug/xcode-dap.
# Idempotent: re-running just refreshes the symlink. Note that the generated
# .zed/tasks.json embeds the absolute binary path, so Zed tasks work even
# without this link — the link is for terminal use (doctor, console, setup).
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
release="$repo/target/release/xcode-dap"
debug="$repo/target/debug/xcode-dap"

if [[ -x "$release" ]]; then
  src="$release"
elif [[ -x "$debug" ]]; then
  src="$debug"
  echo "note: linking the DEBUG build ($debug)"
  echo "      run 'cargo build --release' and re-run install.sh for the optimized binary"
else
  echo "error: no built xcode-dap binary found under $repo/target/" >&2
  echo "       run 'cargo build --release' first, then re-run install.sh" >&2
  exit 1
fi

# Pick a destination directory that is already on PATH. This is only a
# convenience for terminal use — the toolkit itself needs no package manager.
# If Homebrew happens to be installed we reuse its bin dir (prefix differs by
# arch: /opt/homebrew on Apple Silicon, /usr/local on Intel); otherwise we
# probe those standard locations directly.
brew_prefix="$(brew --prefix 2>/dev/null || true)"
if [[ -n "$brew_prefix" && -d "$brew_prefix/bin" ]]; then
  dest_dir="$brew_prefix/bin"
elif [[ -d /opt/homebrew/bin ]]; then
  dest_dir="/opt/homebrew/bin"
elif [[ -d /usr/local/bin ]]; then
  dest_dir="/usr/local/bin"
else
  echo "error: no Homebrew bin dir found (/opt/homebrew/bin or /usr/local/bin)" >&2
  echo "       link manually into any PATH dir: ln -s \"$src\" ~/bin/xcode-dap" >&2
  exit 1
fi
dest="$dest_dir/xcode-dap"
# Short-circuit an already-correct link BEFORE gating on writability, so an
# idempotent re-run stays exit-0 even when $dest_dir is root-owned (e.g. a
# non-Homebrew /usr/local/bin). The writability gate only matters when we
# actually need to create or update the symlink.
current="$(readlink "$dest" 2>/dev/null || true)"
if [[ "$current" == "$src" ]]; then
  echo "ok: $dest already -> $src"
elif [[ ! -w "$dest_dir" ]]; then
  echo "error: $dest_dir is not writable — re-run as: sudo $0" >&2
  exit 1
else
  ln -sfn "$src" "$dest"
  if [[ -n "$current" ]]; then
    echo "ok: $dest -> $src (was: $current)"
  else
    echo "ok: $dest -> $src"
  fi
fi

echo "ok: $("$dest" --version)"
resolved="$(command -v xcode-dap || true)"
if [[ -n "$resolved" ]]; then
  echo "ok: 'xcode-dap' resolves to $resolved"
else
  echo "warning: 'xcode-dap' still not on PATH — is $dest_dir in your PATH?" >&2
fi
