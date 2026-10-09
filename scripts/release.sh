#!/bin/bash
# release.sh X.Y.Z [--dry-run] — prepare the release commit for version X.Y.Z.
#
# Sets the five version declarations that check-versions.sh reads to X.Y.Z:
#
#   crates/xcode-dap/Cargo.toml          [package] version
#   crates/xcode-dap-config/Cargo.toml   [package] version
#   extension/Cargo.toml                 [package] version
#   extension/extension.toml             version
#   extension/src/lib.rs                 PROXY_TAG = "xcode-dap-vX.Y.Z"
#
# then refreshes Cargo.lock and extension/Cargo.lock offline (only the entries
# of the repository's own packages may change), runs scripts/gate.sh, stages
# exactly these seven files with `git add`, and prints the commit title and
# the tag commands. It never commits, tags or pushes.
#
# Every change is made in a temp copy first, checked there (check-versions.sh
# with the new version, and the lockfile rule above) and shown as a diff; the
# checkout is written only after all of that passed. With --dry-run the script
# stops after the diffs: it writes nothing and does not run the gate.
#
# `git add` stages whole files, so the script refuses to start when one of
# the seven files has unstaged edits besides its version. It compares with the
# index: an edit staged on purpose can go into the release commit, and the
# script says so. Re-running it with the same version after a failed gate is
# fine: the files already hold the new version, so it only runs the gate and
# stages them.
#
# Works with macOS's /bin/bash 3.2 and the BSD command line tools.
set -euo pipefail

usage() {
  echo "usage: scripts/release.sh X.Y.Z [--dry-run]" >&2
  exit 2
}

step() {
  printf '\n== %s\n' "$*"
}

version=""
dry_run=0
if [[ $# -eq 0 ]]; then
  usage
fi
for arg in "$@"; do
  case "$arg" in
    --dry-run) dry_run=1 ;;
    -*) usage ;;
    *)
      if [[ -n "$version" ]]; then
        usage
      fi
      version="$arg"
      ;;
  esac
done
if [[ -z "$version" ]]; then
  usage
fi

# Strict X.Y.Z, the only form the Zed registry accepts: no leading zeros, no
# pre-release or build suffix.
version_re='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'
if ! [[ "$version" =~ $version_re ]]; then
  echo "release: \"$version\" is not a version of the form X.Y.Z (three numbers without leading zeros; no \"v\" or tag prefix, no suffix)" >&2
  exit 2
fi
tag="xcode-dap-v$version"
title="Release $version"

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"
# The script always works on its own checkout. Git exports some of these
# variables to hooks; inherited, they would point every git command below at
# another repository or index.
unset GIT_DIR GIT_WORK_TREE GIT_INDEX_FILE GIT_OBJECT_DIRECTORY \
  GIT_ALTERNATE_OBJECT_DIRECTORIES GIT_COMMON_DIR GIT_IMPLICIT_WORK_TREE GIT_PREFIX

if ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  echo "release: $repo is not a git checkout" >&2
  exit 1
fi
if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
  echo "release: the tag $tag already exists" >&2
  exit 1
fi

files=(
  crates/xcode-dap/Cargo.toml
  crates/xcode-dap-config/Cargo.toml
  extension/Cargo.toml
  extension/extension.toml
  extension/src/lib.rs
  Cargo.lock
  extension/Cargo.lock
)
for f in "${files[@]}"; do
  if [[ ! -f "$f" ]]; then
    echo "release: $f is missing" >&2
    exit 1
  fi
done

# package_field FILE KEY: the quoted value of the first KEY line in [package].
package_field() {
  awk -v key="$2" '
    /^[[:space:]]*\[/ { in_package = ($0 ~ /^[[:space:]]*\[package\][[:space:]]*$/); next }
    in_package && $0 ~ ("^[[:space:]]*" key "[[:space:]]*=") {
      value = $0; sub(/^[^"]*"/, "", value); sub(/".*$/, "", value); print value; exit
    }
  ' "$1"
}

# version_lt A B: whether version A is lower than version B (both X.Y.Z).
version_lt() {
  local a1 a2 a3 b1 b2 b3
  IFS=. read -r a1 a2 a3 <<<"$1"
  IFS=. read -r b1 b2 b3 <<<"$2"
  ((a1 < b1 || (a1 == b1 && (a2 < b2 || (a2 == b2 && a3 < b3)))))
}

current="$(package_field crates/xcode-dap/Cargo.toml version)"
if [[ "$current" =~ $version_re ]] && version_lt "$version" "$current"; then
  echo "release: $version is lower than the current version $current" >&2
  exit 2
fi

work="$(mktemp -d "${TMPDIR:-/tmp}/zedx-release.XXXXXX")"
trap 'rm -rf "$work"' EXIT
old="$work/old" # the seven files as they are now
new="$work/new" # a copy of what cargo reads, where the changes are made
mkdir -p "$old" "$new"

# Manifests, sources (cargo needs them to find each package's targets), cargo
# and toolchain config, and check-versions.sh: the files git tracks or would
# track, never ignored build output. COPYFILE_DISABLE keeps macOS tar from
# adding ._ metadata files.
git ls-files -z --cached --others --exclude-standard -- \
  Cargo.toml Cargo.lock crates extension .cargo rust-toolchain rust-toolchain.toml \
  scripts/check-versions.sh |
  while IFS= read -r -d '' f; do
    # Skip files deleted from the working tree but still in the index.
    if [[ -e "$f" || -L "$f" ]]; then printf '%s\0' "$f"; fi
  done |
  COPYFILE_DISABLE=1 tar -c -f - --null -T - | tar -x -f - -C "$new"
for f in "${files[@]}"; do
  mkdir -p "$old/$(dirname "$f")"
  cp "$f" "$old/$f"
done

# The awk programs below rewrite one declaration each (the new version is in
# `new`) and exit 1 when they find none.
#
# The first `version = "..."` line inside [package].
# shellcheck disable=SC2016 # awk programs: $0 is awk's, not the shell's
set_package_version='
  /^[[:space:]]*\[/ { in_package = ($0 ~ /^[[:space:]]*\[package\][[:space:]]*$/) }
  in_package && !done && /^[[:space:]]*version[[:space:]]*=/ { done = sub(/"[^"]*"/, "\"" new "\"") }
  { print }
  END { exit !done }
'
# The first top-level `version = "..."` line, before any table.
set_toplevel_version='
  /^[[:space:]]*\[/ { in_table = 1 }
  !in_table && !done && /^[[:space:]]*version[[:space:]]*=/ { done = sub(/"[^"]*"/, "\"" new "\"") }
  { print }
  END { exit !done }
'
# const PROXY_TAG: &str = "xcode-dap-v...";
set_proxy_tag='
  !done && /^[[:space:]]*(pub[[:space:]]+)?const PROXY_TAG: &str = "xcode-dap-v[^"]*";/ {
    done = sub(/"xcode-dap-v[^"]*"/, "\"xcode-dap-v" new "\"")
  }
  { print }
  END { exit !done }
'

# rewrite FILE PROGRAM: apply one of the programs above to FILE in the copy.
rewrite() {
  local file="$1" program="$2"
  if ! awk -v new="$version" "$program" "$new/$file" >"$new/$file.tmp"; then
    echo "release: no version declaration found in $file; nothing was written" >&2
    exit 1
  fi
  mv "$new/$file.tmp" "$new/$file"
}

# lock_masked FILE NAME...: FILE with the version line of each named package
# replaced by a placeholder, so lockfiles that differ only there compare equal.
lock_masked() {
  local file="$1"
  shift
  awk -v names=" $* " '
    /^\[/ { own = 0 }
    /^name = "/ {
      name = $0; sub(/^name = "/, "", name); sub(/".*$/, "", name)
      own = (index(names, " " name " ") > 0)
    }
    own && /^version = "/ { print "version = (a package of this repository)"; next }
    { print }
  ' "$file"
}

# lock_version FILE NAME: the version FILE records for the package NAME.
lock_version() {
  awk -v want="$2" '
    /^\[/ { own = 0 }
    /^name = "/ {
      name = $0; sub(/^name = "/, "", name); sub(/".*$/, "", name)
      own = (name == want)
    }
    own && /^version = "/ {
      value = $0; sub(/^version = "/, "", value); sub(/".*$/, "", value); print value; exit
    }
  ' "$1"
}

# refresh_lock DIR NAME...: `cargo update --workspace --offline` for the copy
# of DIR (empty for the repository root), then check that its lockfile changed
# in nothing but the version lines of the named packages, and that each of
# them now records the new version.
refresh_lock() {
  local dir="$1" lock name names got
  shift
  lock="${dir:+$dir/}Cargo.lock"
  if ! (cd "$new/$dir" && cargo update --workspace --offline) >"$work/cargo.log" 2>&1; then
    sed 's/^/  /' "$work/cargo.log" >&2
    echo "release: \`cargo update --workspace --offline\` failed for $lock; nothing was written." >&2
    echo "release: if a crate is missing from the local cache, run \`cargo fetch --locked\` in ${dir:-the repository root} with network access, then retry" >&2
    exit 1
  fi
  lock_masked "$old/$lock" "$@" >"$work/before.lock"
  lock_masked "$new/$lock" "$@" >"$work/after.lock"
  if ! cmp -s "$work/before.lock" "$work/after.lock"; then
    diff -u -L "$lock (before, versions masked)" -L "$lock (after)" \
      "$work/before.lock" "$work/after.lock" >&2 || true
    echo "release: refreshing $lock changed more than the versions of $*; nothing was written" >&2
    exit 1
  fi
  for name in "$@"; do
    got="$(lock_version "$new/$lock" "$name")"
    if [[ "$got" != "$version" ]]; then
      echo "release: $lock records $name ${got:-(no entry)}, expected $version; nothing was written" >&2
      exit 1
    fi
  done
  names="$*"
  echo "$lock: only the entries of ${names// /, } changed, to $version"
}

step "Version declarations, set to $version (in a temp copy)"
rewrite crates/xcode-dap/Cargo.toml "$set_package_version"
rewrite crates/xcode-dap-config/Cargo.toml "$set_package_version"
rewrite extension/Cargo.toml "$set_package_version"
rewrite extension/extension.toml "$set_toplevel_version"
rewrite extension/src/lib.rs "$set_proxy_tag"
if ! "$new/scripts/check-versions.sh" "$version"; then
  echo "release: the declarations do not all read $version after the rewrite; nothing was written" >&2
  exit 1
fi

step "Lockfiles, refreshed offline (in the temp copy)"
binary_package="$(package_field crates/xcode-dap/Cargo.toml name)"
config_package="$(package_field crates/xcode-dap-config/Cargo.toml name)"
extension_package="$(package_field extension/Cargo.toml name)"
refresh_lock "" "$binary_package" "$config_package"
refresh_lock extension "$extension_package" "$config_package"

step "Changes"
changed=""
for f in "${files[@]}"; do
  if ! cmp -s "$old/$f" "$new/$f"; then
    changed="$changed $f"
    diff -u -L "a/$f" -L "b/$f" "$old/$f" "$new/$f" || [[ $? -eq 1 ]]
  fi
done
if [[ -z "$changed" ]]; then
  echo "none: all five declarations and both lockfiles already record $version"
fi

# normalized FILE COPY: COPY (a version of the repository file FILE) with its
# version declaration set to the new version or, for a lockfile, with the
# version lines of the repository's own packages masked, so that versions of
# FILE that differ only in the version compare equal.
normalized() {
  case "$1" in
    extension/extension.toml) awk -v new="$version" "$set_toplevel_version" "$2" || true ;;
    extension/src/lib.rs) awk -v new="$version" "$set_proxy_tag" "$2" || true ;;
    Cargo.lock) lock_masked "$2" "$binary_package" "$config_package" ;;
    extension/Cargo.lock) lock_masked "$2" "$extension_package" "$config_package" ;;
    *) awk -v new="$version" "$set_package_version" "$2" || true ;;
  esac
}

# differs_besides_version FILE A B: whether the copies A and B of FILE differ
# in more than the version.
differs_besides_version() {
  normalized "$1" "$2" >"$work/a.normalized"
  normalized "$1" "$3" >"$work/b.normalized"
  ! cmp -s "$work/a.normalized" "$work/b.normalized"
}

# `git add` stages whole files, so any unstaged edit in the seven files besides
# the version would go into the release commit too (dirty). Edits already
# staged in them besides the version go in as well, which is allowed but
# reported (prestaged).
dirty=""
prestaged=""
have_head=0
if git rev-parse -q --verify HEAD >/dev/null; then
  have_head=1
fi
for f in "${files[@]}"; do
  if ! git cat-file --filters ":$f" >"$work/index.copy" 2>/dev/null; then
    dirty="$dirty $f" # not in the index, or unmerged
    continue
  fi
  if differs_besides_version "$f" "$work/index.copy" "$old/$f"; then
    dirty="$dirty $f"
  fi
  if [[ $have_head -eq 1 ]]; then
    if ! git cat-file --filters "HEAD:$f" >"$work/head.copy" 2>/dev/null ||
      differs_besides_version "$f" "$work/head.copy" "$work/index.copy"; then
      prestaged="$prestaged $f"
    fi
  fi
done
prestaged_note="note: these files also have staged edits besides the version, which go into the release commit too:$prestaged"

if [[ $dry_run -eq 1 ]]; then
  step "Dry run: nothing was written"
  echo "A real run writes the changes above into the checkout, runs scripts/gate.sh,"
  echo "stages exactly these files with git add:"
  printf '  %s\n' "${files[@]}"
  echo "and prints the commit title \"$title\" and the commands that tag the commit $tag."
  if [[ -n "$prestaged" ]]; then
    echo
    echo "$prestaged_note"
  fi
  if [[ -n "$dirty" ]]; then
    echo
    echo "Right now a real run would refuse to start, because \`git add\` would also stage"
    echo "the unstaged changes besides the version in:$dirty"
  fi
  exit 0
fi

if [[ -n "$dirty" ]]; then
  echo "release: these files have unstaged changes besides the version, which \`git add\` would put into the release commit:$dirty" >&2
  echo "release: commit or stash them first, or stage them to release them on purpose; nothing was written" >&2
  exit 1
fi

step "Writing the changes"
for f in $changed; do
  cp "$new/$f" "$repo/$f"
  echo "  $f"
done

step "Gate (scripts/gate.sh)"
if ! "$repo/scripts/gate.sh"; then
  echo >&2
  echo "release: the gate failed. The version changes are in the working tree, not staged." >&2
  echo "release: fix the failure and run scripts/release.sh $version again, or undo them with" >&2
  echo "  git checkout -- ${files[*]}" >&2
  exit 1
fi

step "Staging"
git add -- "${files[@]}"
git diff --cached --stat -- "${files[@]}"
others=""
staged="$(git diff --cached --name-only)"
while IFS= read -r path; do
  case " ${files[*]} " in
    *" $path "*) ;;
    *) others="$others $path" ;;
  esac
done <<<"$staged"
if [[ -n "${others# }" ]]; then
  echo "note: also staged, not by this script:$others"
fi
if [[ -n "$prestaged" ]]; then
  echo "$prestaged_note"
fi

step "Next steps"
if [[ -f CHANGELOG.md ]] && ! grep -qF -- "## [$version]" CHANGELOG.md; then
  echo "CHANGELOG.md has no section for $version yet: rename \"## [Unreleased]\" to"
  echo "\"## [$version] - $(date +%Y-%m-%d)\", record the tested Xcode and Zed versions,"
  echo "start a new empty Unreleased section, update the links at the bottom, and"
  echo "stage CHANGELOG.md with the release."
  echo
fi
echo "Review the staged release with \`git diff --cached\`, then commit it:"
echo
echo "  git commit -m \"$title\""
echo
echo "When that commit is on main, tag it and push the tag; the release workflow"
echo "then builds and publishes $tag:"
echo
echo "  git tag -a $tag -m \"$title\""
echo "  git push origin $tag"
