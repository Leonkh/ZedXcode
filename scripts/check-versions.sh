#!/bin/bash
# check-versions.sh [TAG] — the five version declarations must agree.
#
#   crates/xcode-dap/Cargo.toml          [package] version
#   crates/xcode-dap-config/Cargo.toml   [package] version
#   extension/Cargo.toml                 [package] version
#   extension/extension.toml             version
#   extension/src/lib.rs                 PROXY_TAG = "xcode-dap-v<version>"
#
# With TAG (`xcode-dap-vX.Y.Z` or `X.Y.Z`), they must also equal its version.
# Prints each value; exits 1 on any mismatch, 2 on a malformed TAG.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ $# -gt 1 ]]; then
  echo "usage: scripts/check-versions.sh [xcode-dap-vX.Y.Z | X.Y.Z]" >&2
  exit 2
fi

# The quoted value of the first `version = "..."` line inside [package].
package_version() {
  awk '
    /^[[:space:]]*\[/ { in_package = ($0 ~ /^[[:space:]]*\[package\][[:space:]]*$/); next }
    in_package && /^[[:space:]]*version[[:space:]]*=/ {
      value = $0; sub(/^[^"]*"/, "", value); sub(/".*$/, "", value); print value; exit
    }
  ' "$1"
}

# The quoted value of the first top-level `version = "..."` line (before any table).
toplevel_version() {
  awk '
    /^[[:space:]]*\[/ { exit }
    /^[[:space:]]*version[[:space:]]*=/ {
      value = $0; sub(/^[^"]*"/, "", value); sub(/".*$/, "", value); print value; exit
    }
  ' "$1"
}

# The string value of `const PROXY_TAG: &str = "...";`.
proxy_tag() {
  sed -n 's/^[[:space:]]*\(pub[[:space:]]*\)\{0,1\}const PROXY_TAG: &str = "\([^"]*\)";.*$/\2/p' "$1" | head -n 1
}

labels=(
  "crates/xcode-dap/Cargo.toml"
  "crates/xcode-dap-config/Cargo.toml"
  "extension/Cargo.toml"
  "extension/extension.toml"
  "extension/src/lib.rs (PROXY_TAG)"
)
tag_value="$(proxy_tag "$repo/extension/src/lib.rs")"
values=(
  "$(package_version "$repo/crates/xcode-dap/Cargo.toml")"
  "$(package_version "$repo/crates/xcode-dap-config/Cargo.toml")"
  "$(package_version "$repo/extension/Cargo.toml")"
  "$(toplevel_version "$repo/extension/extension.toml")"
  "${tag_value#xcode-dap-v}"
)

status=0
if [[ -n "$tag_value" && "$tag_value" != xcode-dap-v* ]]; then
  echo "versions: PROXY_TAG is \"$tag_value\"; expected the form \"xcode-dap-vX.Y.Z\"" >&2
  status=1
fi

expected=""
if [[ $# -eq 1 ]]; then
  expected="${1#xcode-dap-v}"
  if ! [[ "$expected" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
    echo "versions: \"$1\" is not a release tag (xcode-dap-vX.Y.Z) or a version (X.Y.Z)" >&2
    exit 2
  fi
  printf '  %-36s %s\n' "tag $1" "$expected"
else
  # Compare against the first declaration that was found.
  for value in "${values[@]}"; do
    if [[ -n "$value" ]]; then
      expected="$value"
      break
    fi
  done
fi

for i in "${!labels[@]}"; do
  value="${values[$i]}"
  mark=""
  if [[ -z "$value" ]]; then
    value="<not found>"
    mark="   <- missing"
    status=1
  elif [[ "$value" != "$expected" ]]; then
    mark="   <- expected $expected"
    status=1
  fi
  printf '  %-36s %s%s\n' "${labels[$i]}" "$value" "$mark"
done

if [[ $status -ne 0 ]]; then
  if [[ $# -eq 1 ]]; then
    echo "versions: MISMATCH; every declaration must be $expected, the version of tag $1" >&2
  else
    echo "versions: MISMATCH; give all five declarations the same version" >&2
  fi
  exit 1
fi
echo "versions: all five declarations are $expected"
