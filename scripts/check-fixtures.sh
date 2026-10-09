#!/bin/bash
# check-fixtures.sh — every Xcode container under tests/fixtures lists the
# scheme the checks expect, as `xcodebuild -list -json` reads it.
#
# The fixtures are copied to a temp dir first (the files git tracks or would
# track, never ignored build output), because xcodebuild may write next to a
# container and nothing may land in the tracked tree. Two layouts must hold no
# container at all: spm-only, and xcodegen-ungenerated until its fake
# generator (generate.sh) has run in the copy.
#
# The checks that need no Xcode run everywhere: the containers of each layout,
# no per-user Xcode state, and the content of every shared MyApp scheme (its
# target, environment, argument and lldbinit file). The content check is
# needed because xcodebuild autocreates a scheme named after the target when
# the shared one is missing or unreadable, so `-list` alone would still pass.
# The xcodebuild checks need Xcode, so on other hosts the script prints one
# line and exits 0 (CI runs it on macOS).
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

tmp="${TMPDIR:-/tmp}" # macOS ends TMPDIR in "/"
work="$(mktemp -d "${tmp%/}/zedx-fixtures.XXXXXX")"
trap 'rm -rf "$work"' EXIT

# Copy tests/fixtures into $work, keeping modes (generate.sh must stay
# executable). COPYFILE_DISABLE keeps macOS tar from adding ._ metadata files.
if git -C "$repo" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  (
    cd "$repo"
    git ls-files -z --cached --others --exclude-standard -- tests/fixtures |
      while IFS= read -r -d '' f; do
        # Skip files deleted from the working tree but still in the index.
        if [[ -e "$f" || -L "$f" ]]; then printf '%s\0' "$f"; fi
      done |
      COPYFILE_DISABLE=1 tar -c -f - --null -T -
  ) | tar -x -f - -C "$work"
else
  # Not a git checkout: copy everything, then drop what the ignore rules cover.
  mkdir -p "$work/tests"
  cp -R "$repo/tests/fixtures" "$work/tests/"
  rm -rf "$work/tests/fixtures/layouts/xcodegen-ungenerated/MyApp.xcodeproj"
  find "$work/tests/fixtures" \( -name xcuserdata -o -name '*.xcuserstate' -o -name DerivedData \
    -o -name build -o -name .build -o -name .swiftpm \) -prune -exec rm -rf {} +
fi
fixtures="$work/tests/fixtures"

failures=0
fail() {
  echo "check-fixtures: FAIL: $*" >&2
  failures=$((failures + 1))
}

# Containers below a fixture directory, one relative path per line, sorted;
# the inner project.xcworkspace of an .xcodeproj is not a container of its own.
containers() {
  (
    cd "$1"
    find . \( -name '*.xcodeproj' -o -name '*.xcworkspace' \) -prune -print |
      sed 's|^\./||' | LC_ALL=C sort
  )
}

# expect_containers <fixture> <expected containers, space-separated>
expect_containers() {
  local fixture="$1" want="$2" got
  if [[ ! -d "$fixtures/$fixture" ]]; then
    fail "$fixture: missing"
    return
  fi
  got="$(containers "$fixtures/$fixture" | tr '\n' ' ' | sed 's/ $//')"
  if [[ "$got" != "$want" ]]; then
    fail "$fixture: containers are '${got:-none}', expected '${want:-none}'"
  fi
}

expect_containers myapp "MyApp.xcodeproj"
expect_containers layouts/xcworkspace "MyApp.xcodeproj MyApp.xcworkspace"
expect_containers layouts/nested-ios "ios/MyApp.xcodeproj ios/MyApp.xcworkspace"
expect_containers layouts/xcodegen-ungenerated ""
expect_containers layouts/tuist-shaped "MyApp.xcodeproj MyApp.xcworkspace"
expect_containers layouts/local-package "MyApp.xcodeproj MyApp.xcworkspace"
expect_containers layouts/spm-only ""

# Per-account Xcode state must never be committed (it carries account names).
stray="$(cd "$fixtures" && find . \( -name xcuserdata -o -name '*.xcuserstate' \) -print)"
if [[ -n "$stray" ]]; then
  fail "per-user Xcode state in the fixtures: $(echo "$stray" | tr '\n' ' ')"
fi

# The not-yet-generated layout: run its fake generator in the copy; the
# generated project is then checked like the committed ones.
projects=(myapp layouts/xcworkspace layouts/nested-ios/ios layouts/tuist-shaped layouts/local-package)
if "$fixtures/layouts/xcodegen-ungenerated/generate.sh" >"$work/generate.out" 2>&1; then
  expect_containers layouts/xcodegen-ungenerated "MyApp.xcodeproj"
  projects+=(layouts/xcodegen-ungenerated)
else
  fail "layouts/xcodegen-ungenerated: generate.sh failed: $(tail -n 5 "$work/generate.out")"
fi

# The shared MyApp scheme of each project directory (SRCROOT) points at the
# project's MyApp target and carries what the run checks rely on.
scheme_failures=0
python3 -I - "$fixtures" "${projects[@]}" <<'PY' || scheme_failures=$?
import os
import re
import sys
import xml.etree.ElementTree as ET

fixtures = sys.argv[1]
failures = 0


def fail(where, message):
    global failures
    print(f"check-fixtures: FAIL: {where}: {message}", file=sys.stderr)
    failures += 1


def native_target_ids(pbxproj):
    """Ids of the PBXNativeTarget objects named MyApp."""
    text = open(pbxproj, encoding="utf-8").read()
    objects = re.finditer(
        r"^([ \t]*)([0-9A-F]{24})(?: /\*[^*]*\*/)? = \{$(.*?)^\1\};$",
        text,
        re.M | re.S,
    )
    return [
        m.group(2)
        for m in objects
        if re.search(r"^[ \t]*isa = PBXNativeTarget;$", m.group(3), re.M)
        and re.search(r"^[ \t]*name = MyApp;$", m.group(3), re.M)
    ]


def check(project_dir):
    root = os.path.join(fixtures, project_dir)
    where = f"{project_dir}/MyApp.xcodeproj"
    pbxproj = os.path.join(root, "MyApp.xcodeproj", "project.pbxproj")
    scheme = os.path.join(root, "MyApp.xcodeproj", "xcshareddata", "xcschemes", "MyApp.xcscheme")
    for path in (pbxproj, scheme):
        if not os.path.isfile(path):
            fail(where, f"{os.path.relpath(path, root)} is missing")
            return
    ids = native_target_ids(pbxproj)
    if len(ids) != 1:
        fail(where, f"expected one native target MyApp, found {len(ids)}")
        return
    try:
        doc = ET.parse(scheme).getroot()
    except ET.ParseError as err:
        fail(where, f"MyApp.xcscheme does not parse: {err}")
        return
    if doc.tag != "Scheme":
        fail(where, f"MyApp.xcscheme has root <{doc.tag}>, expected <Scheme>")
        return

    want = {
        "BuildableIdentifier": "primary",
        "BlueprintIdentifier": ids[0],
        "BuildableName": "MyApp.app",
        "BlueprintName": "MyApp",
        "ReferencedContainer": "container:MyApp.xcodeproj",
    }
    wrong = set()
    for ref in doc.iter("BuildableReference"):
        for key, value in want.items():
            if ref.get(key) != value:
                wrong.add(f"{key}={ref.get(key)!r} (expected {value!r})")
    for item in sorted(wrong):
        fail(where, f"MyApp.xcscheme has a BuildableReference with {item}")

    launch = doc.find("LaunchAction")
    if launch is None:
        fail(where, "MyApp.xcscheme has no LaunchAction")
        return
    if launch.find("BuildableProductRunnable/BuildableReference") is None:
        fail(where, "LaunchAction runs no BuildableProductRunnable")
    env = [
        (e.get("key"), e.get("value"), e.get("isEnabled"))
        for e in launch.findall("EnvironmentVariables/EnvironmentVariable")
    ]
    if ("MYAPP_FLAG", "1", "YES") not in env:
        fail(where, "LaunchAction does not set MYAPP_FLAG=1 (enabled)")
    args = [
        (a.get("argument"), a.get("isEnabled"))
        for a in launch.findall("CommandLineArguments/CommandLineArgument")
    ]
    if ("-MyAppArg YES", "YES") not in args:
        fail(where, "LaunchAction does not pass '-MyAppArg YES' (enabled)")
    lldbinit = launch.get("customLLDBInitFile", "")
    prefix = "$(SRCROOT)/"
    if not lldbinit.startswith(prefix):
        fail(where, f"customLLDBInitFile is {lldbinit!r}, expected a path under $(SRCROOT)")
    elif not os.path.isfile(os.path.join(root, lldbinit[len(prefix):])):
        fail(where, f"customLLDBInitFile {lldbinit} names no file")


for project_dir in sys.argv[2:]:
    check(project_dir)
sys.exit(min(failures, 100))
PY
failures=$((failures + scheme_failures))

if ((failures > 0)); then
  exit 1
fi

# The rest needs Xcode. On a Mac with only the Command Line Tools,
# /usr/bin/xcodebuild exists but refuses to run, so ask it for its version.
# A macOS CI host must have Xcode: skipping there would hide a broken runner.
if [[ "$(uname -s)" != Darwin ]] || ! xcodebuild -version >"$work/xcodebuild.version" 2>&1; then
  if [[ "$(uname -s)" == Darwin && -n "${CI:-}" ]]; then
    echo "check-fixtures: FAIL: xcodebuild does not run on this CI host: $(tail -n 3 "$work/xcodebuild.version")" >&2
    exit 1
  fi
  echo "check-fixtures: skipped (needs Xcode; CI runs it)"
  exit 0
fi

# expect_scheme <container, relative to tests/fixtures> <scheme>
expect_scheme() {
  local container="$1" scheme="$2" flag json
  case "$container" in
    *.xcodeproj) flag=-project ;;
    *.xcworkspace) flag=-workspace ;;
    *)
      fail "$container: not an Xcode container"
      return
      ;;
  esac
  if ! json="$(cd "$fixtures" && xcodebuild -list -json "$flag" "$container" 2>"$work/xcodebuild.err")"; then
    fail "$container: xcodebuild -list failed: $(tail -n 5 "$work/xcodebuild.err")"
    return
  fi
  if python3 -c '
import json, sys
text = sys.stdin.read()
try:
    data = json.loads(text[text.index("{"):])
except ValueError:
    sys.exit(1)
info = data.get("workspace") or data.get("project") or {}
sys.exit(0 if sys.argv[1] in info.get("schemes", []) else 1)
' "$scheme" <<<"$json"; then
    echo "ok: $container lists scheme $scheme"
  else
    fail "$container: scheme $scheme not listed: $(tr -d '\n' <<<"$json")"
  fi
}

expect_scheme myapp/MyApp.xcodeproj MyApp
expect_scheme layouts/xcworkspace/MyApp.xcworkspace MyApp
expect_scheme layouts/xcworkspace/MyApp.xcodeproj MyApp
expect_scheme layouts/nested-ios/ios/MyApp.xcworkspace MyApp
expect_scheme layouts/nested-ios/ios/MyApp.xcodeproj MyApp
expect_scheme layouts/tuist-shaped/MyApp.xcworkspace MyApp
expect_scheme layouts/tuist-shaped/MyApp.xcodeproj MyApp
# The app links a product of the workspace's local package, which the bare
# MyApp.xcodeproj cannot resolve on its own, so only the workspace is listed
# here; the project's scheme is covered by the content check above.
expect_scheme layouts/local-package/MyApp.xcworkspace MyApp
expect_scheme layouts/xcodegen-ungenerated/MyApp.xcodeproj MyApp

if ((failures > 0)); then
  echo "check-fixtures: $failures failure(s)" >&2
  exit 1
fi
echo "check-fixtures: ok"
