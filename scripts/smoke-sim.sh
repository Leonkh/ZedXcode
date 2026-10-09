#!/bin/bash
# smoke-sim.sh [--binary PATH] [--log-dir DIR] [--local] <fixture-dir>
#
# The simulator smoke test. `xcode-dap run` builds the fixture's MyApp scheme,
# boots a simulator, installs com.example.MyApp and launches it. This script
# waits for the app's first console line ("MyApp launched") in the run's
# ~/.zedxcode/run/<udid>/out.log and for "tick 1" within 3 s of the launch (the
# app's stdout reaches the file as it is written), checks that xcode-dap.log
# records a successful pipeline for that run, then detaches the run, terminates
# the app and checks that the ticks are still in out.log. The logs are copied to
# the log dir (default: ./smoke-logs) whether the test passes or not; the
# simulator-smoke workflow uploads that directory.
#
#   --binary PATH   the xcode-dap to test (default: target/debug/xcode-dap)
#   --log-dir DIR   where the logs go (created if needed)
#   --local         run outside CI (see HOME below)
#
# `xcode-dap run` streams the app console until Ctrl-C, so it runs in the
# background under a deadline (SMOKE_LAUNCH_TIMEOUT seconds for build, boot,
# install and launch, default 1200; SMOKE_LINE_TIMEOUT seconds for the first
# console line after the launch, default 60).
#
# The run targets an iPhone on the selected Xcode's own simulator runtime (its
# iphonesimulator SDK version) when one is available; SMOKE_OS overrides the
# version. See pick_os.
#
# SMOKE_SIMULATOR_WINDOW names the app the run must try first for the
# simulator window: DeviceHub.app (Xcode 27) or Simulator.app (Xcode 26). The
# simulator-smoke workflow sets it per leg; unset, either one passes. See
# check_simulator_window.
#
# HOME is not redirected. xcode-dap keeps its state under $HOME/.zedxcode and
# has no setting that moves only that, while a redirected HOME can also move
# what xcrun and xcodebuild keep under ~/Library (simulator devices,
# DerivedData, caches), so the run would no longer use the simulators and the
# Xcode setup a user's Mac has. CI runners are disposable, so the test uses
# the real HOME there. On a workstation it needs --local, because it appends
# to ~/.zedxcode/logs, replaces the run console files of the simulator it
# picks, and installs and terminates com.example.MyApp on that simulator.
#
# The fixture is copied to a temp dir first, so the build never writes into
# the checkout. Needs macOS with Xcode.
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
bundle_id="com.example.MyApp"
bundle_re='com\.example\.MyApp' # bundle_id as an extended regex
scheme="MyApp"
launch_timeout="${SMOKE_LAUNCH_TIMEOUT:-1200}"
line_timeout="${SMOKE_LINE_TIMEOUT:-60}"
window_app="${SMOKE_SIMULATOR_WINDOW:-}"
# 1: `xcode-dap run` passes NSUnbufferedIO=YES to the app itself, so the tick
# checks run, on the launch's own setting (app_stdout_env removes any inherited
# one). 0 skips them and changes nothing else.
launch_sets_unbuffered_io=1

usage() {
  echo "usage: scripts/smoke-sim.sh [--binary PATH] [--log-dir DIR] [--local] <fixture-dir>" >&2
  exit 2
}

bin="$repo/target/debug/xcode-dap"
log_dir="$PWD/smoke-logs"
local_run=0
fixture=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --binary)
      [[ $# -ge 2 ]] || usage
      bin="$2"
      shift 2
      ;;
    --log-dir)
      [[ $# -ge 2 ]] || usage
      log_dir="$2"
      shift 2
      ;;
    --local)
      local_run=1
      shift
      ;;
    -h | --help) usage ;;
    -*) usage ;;
    *)
      [[ -z "$fixture" ]] || usage
      fixture="$1"
      shift
      ;;
  esac
done
[[ -n "$fixture" ]] || usage

die() {
  echo "smoke-sim: FAIL: $*" >&2
  exit 1
}

if [[ "$(uname -s)" != Darwin ]]; then
  die "needs macOS with Xcode (simulators)"
fi
if [[ -z "${CI:-}" && $local_run -eq 0 ]]; then
  die "outside CI this writes into ~/.zedxcode and drives a simulator; pass --local to run it anyway"
fi
case "$window_app" in
  "" | DeviceHub.app | Simulator.app) ;;
  *) die "SMOKE_SIMULATOR_WINDOW must be DeviceHub.app or Simulator.app (got \"$window_app\")" ;;
esac
[[ -x "$bin" ]] || die "no xcode-dap at $bin (run cargo build first, or pass --binary)"
[[ -d "$fixture/MyApp.xcodeproj" ]] || die "$fixture has no MyApp.xcodeproj"
bin="$(cd "$(dirname "$bin")" && pwd)/$(basename "$bin")"
mkdir -p "$log_dir"
log_dir="$(cd "$log_dir" && pwd)"

zedxcode="$HOME/.zedxcode"
xcode_dap_log="$zedxcode/logs/xcode-dap.log"

tmp="${TMPDIR:-/tmp}" # macOS ends TMPDIR in "/"
work="$(mktemp -d "${tmp%/}/zedx-smoke.XXXXXX")"
# Copy the fixture without any build output or per-user Xcode state.
mkdir -p "$work/fixture"
cp -R "$fixture/." "$work/fixture/"
find "$work/fixture" \( -name xcuserdata -o -name '*.xcuserstate' -o -name DerivedData \
  -o -name build -o -name .build -o -name .swiftpm \) -prune -exec rm -rf {} +
project="$work/fixture/MyApp.xcodeproj"

run_stdout="$log_dir/run.stdout.log"
run_stderr="$log_dir/run.stderr.log"
: >"$run_stdout"
: >"$run_stderr"
# Run dirs written after this marker belong to this test.
touch "$work/started"

run_pid=""
udid=""
run_dir=""
launched_at="" # $SECONDS when the run reported the launch
first_line_at="" # $SECONDS when "MyApp launched" was first seen in out.log
app_terminated=0
terminate_status="" # the exit status of `xcrun simctl terminate`, once it ran

# --- helpers ------------------------------------------------------------------

# The simulator the run picked, from its "Simulator: <udid>" console line.
udid_from_run() {
  sed -n 's/^Simulator: \([0-9A-Fa-f-]\{36\}\)$/\1/p' "$run_stderr" | tail -n 1
}

# The run dir of this test: the one of the simulator the run named, else the
# newest run dir written since the test started.
find_run_dir() {
  local candidate newest=""
  if [[ -n "$udid" && -f "$zedxcode/run/$udid/out.log" ]]; then
    echo "$zedxcode/run/$udid"
    return
  fi
  [[ -d "$zedxcode/run" ]] || return 0
  for candidate in "$zedxcode"/run/*/out.log; do
    [[ -f "$candidate" && "$candidate" -nt "$work/started" ]] || continue
    if [[ -z "$newest" || "$candidate" -nt "$newest" ]]; then
      newest="$candidate"
    fi
  done
  if [[ -n "$newest" ]]; then
    dirname "$newest"
  fi
}

run_alive() {
  [[ -n "$run_pid" ]] && kill -0 "$run_pid" 2>/dev/null
}

# The lines xcode-dap.log holds for the run under test (every line carries
# "[pid <pid> <mode>]").
run_log_lines() {
  if [[ -f "$xcode_dap_log" ]]; then
    grep -F "[pid $run_pid run]" "$xcode_dap_log" || true
  fi
}

# Stop `xcode-dap run`: SIGINT is its Ctrl-C (detach; the app keeps running),
# then SIGTERM and SIGKILL if it does not exit.
stop_run() {
  local waited signal
  run_alive || return 0
  kill -INT "$run_pid" 2>/dev/null || true
  for signal in TERM KILL; do
    waited=0
    while run_alive && [[ $waited -lt 15 ]]; do
      sleep 1
      waited=$((waited + 1))
    done
    run_alive || break
    echo "smoke-sim: xcode-dap run did not exit; sending SIG$signal" >&2
    kill "-$signal" "$run_pid" 2>/dev/null || true
  done
  wait "$run_pid" 2>/dev/null || true
}

# Terminate the app on the simulator the run used ("booted" only when the run
# launched it without naming the simulator). A no-op when nothing launched.
terminate_app() {
  local target
  [[ $app_terminated -eq 0 ]] || return 0
  [[ -n "$udid" ]] || udid="$(udid_from_run)"
  target="$udid"
  if [[ -z "$target" ]]; then
    stderr_has "Launched $bundle_id" || return 0
    target=booted
  fi
  app_terminated=1
  terminate_status=0
  xcrun simctl terminate "$target" "$bundle_id" >>"$log_dir/terminate.log" 2>&1 || terminate_status=$?
}

collect_logs() {
  [[ -n "$run_dir" ]] || run_dir="$(find_run_dir)"
  if [[ -n "$run_dir" ]]; then
    cp "$run_dir/out.log" "$log_dir/app-out.log" 2>/dev/null || true
    cp "$run_dir/err.log" "$log_dir/app-err.log" 2>/dev/null || true
  fi
  if [[ -f "$xcode_dap_log" ]]; then
    cp "$xcode_dap_log" "$log_dir/xcode-dap.log"
  fi
  if [[ -f "$zedxcode/logs/build-latest.log" ]]; then
    cp "$zedxcode/logs/build-latest.log" "$log_dir/build-latest.log"
  fi
}

on_exit() {
  local status=$?
  stop_run
  terminate_app
  collect_logs
  if [[ $status -ne 0 ]]; then
    echo "--- xcode-dap run stderr (last 40 lines) ---" >&2
    tail -n 40 "$run_stderr" >&2 || true
    if [[ -n "$run_pid" ]]; then
      echo "--- xcode-dap.log, this run (last 40 lines) ---" >&2
      run_log_lines | tail -n 40 >&2
    fi
    echo "smoke-sim: logs in $log_dir" >&2
  fi
  rm -rf "$work"
  exit "$status"
}
trap on_exit EXIT
# A cancelled CI job sends SIGTERM: exit through on_exit, so the run stops,
# the app is terminated and the logs are collected.
trap 'exit 130' INT
trap 'exit 143' TERM

# wait_for <seconds> <description> <command...>: poll the command once a second
# until it succeeds; fail when the run exits first or the deadline passes.
wait_for() {
  local seconds="$1" what="$2"
  local deadline=$((SECONDS + seconds))
  shift 2
  until "$@"; do
    if ! run_alive; then
      die "xcode-dap run exited before $what"
    fi
    if [[ $SECONDS -ge $deadline ]]; then
      die "no $what within $seconds s"
    fi
    sleep 1
  done
}

stderr_has() {
  grep -qF -- "$1" "$run_stderr"
}

out_log_has() {
  run_dir="$(find_run_dir)"
  [[ -n "$run_dir" ]] && grep -qF -- "$1" "$run_dir/out.log"
}

# Like out_log_has, for a whole line ("tick 1" must not match "tick 12").
out_log_has_line() {
  run_dir="$(find_run_dir)"
  [[ -n "$run_dir" ]] && grep -qxF -- "$1" "$run_dir/out.log"
}

# --- checks -------------------------------------------------------------------

# The simulator window xcode-dap chose: Device Hub with Xcode 27, Simulator
# with Xcode 26, read from the run's first "simulator window: opening <path>"
# line in xcode-dap.log. The path must be the app SMOKE_SIMULATOR_WINDOW names
# (either one when unset). When a "simulator window: opened <path>" line
# follows, it must name the same app. A fresh runner may take longer than the
# open deadline to launch the app for the first time; the run then goes on
# headless, which is the behaviour under test, so a missing "opened" line is a
# note, not a failure.
check_window_app() { # <path> <what>
  if [[ -n "$window_app" ]]; then
    [[ "$1" == */"$window_app" ]] || die "the simulator window $2 $1, expected .../$window_app"
  elif [[ "$1" != */DeviceHub.app && "$1" != */Simulator.app ]]; then
    die "the simulator window $2 $1, expected .../DeviceHub.app or .../Simulator.app"
  fi
}

check_simulator_window() {
  local tried opened
  tried="$(run_log_lines | grep -F 'simulator window: opening ' | head -n 1 || true)"
  [[ -n "$tried" ]] || die "xcode-dap.log has no \"simulator window: opening\" line for this run"
  tried="${tried#*simulator window: opening }"
  check_window_app "$tried" "tried first"
  opened="$(run_log_lines | grep -F 'simulator window: opened ' | tail -n 1 || true)"
  if [[ -n "$opened" ]]; then
    opened="${opened#*simulator window: opened }"
    check_window_app "$opened" "opened"
    echo "ok: xcode-dap.log: simulator window: opened $opened"
  else
    echo "ok: xcode-dap.log: simulator window: tried $tried first (it did not confirm in time; the run went on headless)"
  fi
}

# The app's stdout reaches out.log unbuffered: "tick 1" within 3 s of the
# launch. MyApp prints it a second or more after its first line; buffered, it
# would sit in a 4 KB stdio buffer for minutes. The clock starts when the script
# first saw the run's "Launched" line ($launched_at, polled once a second) and
# counts whole seconds ($SECONDS); "tick 1" must be seen at 3 s or less by that
# clock, so a line that came within 3 s of the launch always passes. A "tick 1"
# already there when the check starts counts as seen then. Runs only with
# launch_sets_unbuffered_io=1.
check_tick_soon_after_launch() {
  local elapsed late
  [[ $launch_sets_unbuffered_io -eq 1 ]] || return 0
  late="\"MyApp launched\" was seen $((first_line_at - launched_at)) s after the launch: when that is late too, the app started slowly; see app-out.log in $log_dir"
  while :; do
    elapsed=$((SECONDS - launched_at))
    out_log_has_line "tick 1" && break
    [[ $elapsed -le 3 ]] || die "no \"tick 1\" in the run's out.log within 3 s of the launch ($late)"
    sleep 0.2
  done
  [[ $elapsed -le 3 ]] ||
    die "\"tick 1\" was first seen in the run's out.log $elapsed s after the launch, later than 3 s ($late)"
  echo "ok: \"tick 1\" in out.log, $elapsed s after the launch"
}

# Output written before the app was terminated stays in out.log ("tick 1" is
# still there after `simctl terminate`). Runs only with
# launch_sets_unbuffered_io=1.
check_ticks_after_terminate() {
  local ticks
  [[ $launch_sets_unbuffered_io -eq 1 ]] || return 0
  [[ "$terminate_status" == 0 ]] ||
    die "xcrun simctl terminate did not succeed (exit status ${terminate_status:-none}; see $log_dir/terminate.log), so out.log cannot show what survives it"
  out_log_has_line "tick 1" ||
    die "\"tick 1\" is no longer in the run's out.log after xcrun simctl terminate"
  ticks="$(grep -c '^tick [0-9][0-9]*$' "$run_dir/out.log" || true)"
  echo "ok: \"tick 1\" still in out.log after xcrun simctl terminate ($ticks tick lines)"
}

# simctl passes the SIMCTL_CHILD_* variables of its own environment to the app,
# and xcode-dap sets SIMCTL_CHILD_NSUnbufferedIO=YES on its `simctl launch`. A
# value inherited from the caller is removed: it would let the tick checks pass
# without the launch's own.
app_stdout_env() {
  unset SIMCTL_CHILD_NSUnbufferedIO
}

# Exits 0 when `simctl list --json devices` (stdin) holds an available iPhone on
# the iOS version in argv[1], with the filters xcode-dap's device choice uses.
has_iphone_py='
import json, sys
suffix = "iOS-" + sys.argv[1].replace(".", "-")
devices = json.load(sys.stdin).get("devices", {})
sys.exit(0 if any(
    "SimRuntime.iOS" in runtime and runtime.endswith(suffix)
    and any(d.get("isAvailable") and str(d.get("name", "")).startswith("iPhone")
            for d in devs)
    for runtime, devs in devices.items()) else 1)
'

# The iOS version for --os: SMOKE_OS when set, else the selected Xcode's
# iphonesimulator SDK version when an available iPhone runs it, else nothing
# (xcode-dap's own choice). Left to itself, xcode-dap takes the newest iPhone
# across every installed runtime, and a runner image may carry the runtimes of
# newer Xcodes (betas among them), which the selected Xcode cannot build for.
pick_os() {
  local sdk
  if [[ -n "${SMOKE_OS:-}" ]]; then
    echo "$SMOKE_OS"
    return 0
  fi
  sdk="$(xcrun --sdk iphonesimulator --show-sdk-version 2>/dev/null | cut -d. -f1-2)" || return 0
  [[ -n "$sdk" ]] || return 0
  if xcrun simctl list --json devices 2>/dev/null | python3 -c "$has_iphone_py" "$sdk" 2>/dev/null; then
    echo "$sdk"
  else
    echo "smoke-sim: no available iPhone on iOS $sdk (the selected Xcode's SDK); xcode-dap picks the simulator" >&2
  fi
}

# --- the test -------------------------------------------------------------------

"$bin" --version
xcodebuild -version
os="$(pick_os)"
os_args=()
[[ -z "$os" ]] || os_args=(--os "$os")
echo "smoke-sim: xcode-dap run -w $project -s $scheme${os:+ --os $os}"
app_stdout_env
# ${os_args[@]+...}: an empty array under `set -u` is an error in bash 3.2.
"$bin" run -w "$project" -s "$scheme" ${os_args[@]+"${os_args[@]}"} </dev/null >"$run_stdout" 2>"$run_stderr" &
run_pid=$!

wait_for "$launch_timeout" "launch of $bundle_id" stderr_has "Launched $bundle_id (pid "
launched_at=$SECONDS
udid="$(udid_from_run)"
echo "ok: xcode-dap launched $bundle_id on ${udid:-<simulator not named>}"
check_simulator_window

wait_for "$line_timeout" "\"MyApp launched\" in the run's out.log" out_log_has "MyApp launched"
first_line_at=$SECONDS
echo "ok: \"MyApp launched\" in $run_dir/out.log, $((first_line_at - launched_at)) s after the launch"
check_tick_soon_after_launch

# `xcode-dap run` exits on its own only when the app does.
run_alive || die "xcode-dap run exited: the app quit after its first line"
stop_run
echo "ok: detached from the run"

# The pipeline as xcode-dap.log records it for this run.
run_lines="$(run_log_lines)"
[[ -n "$run_lines" ]] || die "xcode-dap.log has no line of this run (pid $run_pid)"
expect_log() {
  local what="$1" pattern="$2"
  if grep -qE -- "$pattern" <<<"$run_lines"; then
    echo "ok: xcode-dap.log: $what"
  else
    die "xcode-dap.log has no line for: $what (pattern: $pattern)"
  fi
}
expect_log "session start" 'session start: xcode-dap '
expect_log "build succeeded" 'build exited exit status: 0 '
expect_log "bundle id read" "bundle id: $bundle_re\$"
expect_log "simctl launch succeeded" "simctl launch .* $bundle_re -> exit status: 0 "
# Every line is "<time> <LEVEL> [pid <pid> run] <message>".
errors="$(grep -E -- '^[^ ]+ ERROR ' <<<"$run_lines" || true)"
[[ -z "$errors" ]] || die "xcode-dap.log records an error for this run: $(head -n 3 <<<"$errors")"

terminate_app
echo "ok: terminated $bundle_id"
check_ticks_after_terminate

echo "smoke-sim: PASS"
