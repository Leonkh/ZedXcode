#!/bin/bash
# check-deps.sh — the direct dependencies of every Cargo manifest must match
# deps.allow exactly (normal, dev and build dependencies alike).
#
# Reads `cargo metadata --no-deps` for the workspace (crates/*) and for the
# standalone extension/ package, prints every difference and exits 1 if there
# is one. A new dependency needs the maintainer's sign-off, recorded as an
# edit to deps.allow in the same pull request.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tmp="${TMPDIR:-/tmp}" # macOS ends TMPDIR in "/"
work="$(mktemp -d "${tmp%/}/zedx-deps.XXXXXX")"
trap 'rm -rf "$work"' EXIT

cargo metadata --no-deps --format-version 1 \
  --manifest-path "$repo/Cargo.toml" >"$work/workspace.json"
cargo metadata --no-deps --format-version 1 \
  --manifest-path "$repo/extension/Cargo.toml" >"$work/extension.json"

python3 - "$repo" "$repo/deps.allow" "$work/workspace.json" "$work/extension.json" <<'PY'
import json
import os
import sys

repo, allow_path, *metadata_paths = sys.argv[1:]
KINDS = ("normal", "dev", "build")

allowed = set()
problems = []
with open(allow_path, encoding="utf-8") as f:
    for number, line in enumerate(f, 1):
        line = line.split("#", 1)[0].strip()
        if not line:
            continue
        fields = line.split()
        if len(fields) != 3 or fields[1] not in KINDS:
            problems.append(f"deps.allow:{number}: expected '<manifest dir> <kind> <name>' "
                            f"with kind one of {', '.join(KINDS)}: {line!r}")
            continue
        allowed.add(tuple(fields))

actual = set()
for path in metadata_paths:
    with open(path, encoding="utf-8") as f:
        metadata = json.load(f)
    for package in metadata["packages"]:
        manifest_dir = os.path.relpath(
            os.path.realpath(os.path.dirname(package["manifest_path"])), os.path.realpath(repo))
        for dep in package["dependencies"]:
            actual.add((manifest_dir, dep.get("kind") or "normal", dep["name"]))

unlisted = sorted(actual - allowed)
stale = sorted(allowed - actual)
for entry in unlisted:
    problems.append("+ %s %s %s   (declared in %s/Cargo.toml, not in deps.allow)"
                    % (entry + (entry[0],)))
for entry in stale:
    problems.append("- %s %s %s   (in deps.allow, not declared in %s/Cargo.toml)"
                    % (entry + (entry[0],)))

if problems:
    print("deps: the manifests and deps.allow differ:", file=sys.stderr)
    for problem in problems:
        print("  " + problem, file=sys.stderr)
    if unlisted:
        print("A new dependency needs the maintainer's sign-off. Remove it, or add the "
              "+ lines to deps.allow in the same pull request and ask for that sign-off.",
              file=sys.stderr)
    if stale:
        print("Delete the - lines from deps.allow.", file=sys.stderr)
    sys.exit(1)

manifests = sorted({entry[0] for entry in actual})
print(f"deps: {len(actual)} direct dependencies in {len(manifests)} manifests "
      f"({', '.join(manifests)}) match deps.allow")
PY
