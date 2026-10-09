#!/usr/bin/env python3
"""DAP-level smoke harness for xcode-dap.

A minimal scripted DAP client: spawns `xcode-dap`, frames JSON with
Content-Length, and asserts a scripted session against the real binary.
See docs/design/dap-proxy.md section 8.

Subcommands:
  roundtrip   initialize -> expect lldb-dap capabilities response ->
              disconnect -> expect response -> expect clean exit 0.
              (gate 1: proves spawn + verbatim forward + teardown)
  session     full scripted session: initialize -> launch -> output events
              -> initialized -> setBreakpoints -> configurationDone ->
              app stdout output events -> disconnect -> clean exit, and no
              process the adapter started (lldb-dap, xcodebuild, the app...)
              still running. (gate 3)
              With --mock-pipeline --kill-after-response it ends the way Zed
              does: SIGKILL the adapter right after the disconnect response.
              Teardown must be done by then: no lldb-dap, mock app or
              `log stream` (a stand-in that ignores SIGTERM) left, the
              pidfile gone, and "teardown: done" in the log before the
              response arrived. HOME points at a temp dir for the run, so the
              log is the run's own.
  purity      stdout purity: a real (non-mock) launch against a temp project
              with PATH-shimmed fakes of xcrun/simctl, lldb-dap, xcodebuild,
              open, git and plutil that print a canary on stdout; every byte
              the adapter writes to stdout must sit inside a well-formed
              Content-Length frame, and the canary must never appear.

Usage (note: --binary belongs to the top-level parser, before the subcommand):
  python3 tests/dap_smoke.py [--binary target/debug/xcode-dap] roundtrip
  python3 tests/dap_smoke.py [--binary PATH] session --mock-pipeline
  python3 tests/dap_smoke.py [--binary PATH] session --mock-pipeline --kill-after-response
  python3 tests/dap_smoke.py [--binary PATH] session --workspace W --scheme S
          [--device D] [--os V] [--configuration C] [--preflight CMD]
          --bp-file FILE --bp-line N [--timeout SECS]
  python3 tests/dap_smoke.py [--binary PATH] purity [--timeout SECS]
"""

import argparse
import json
import os
import re
import select
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time

DEFAULT_TIMEOUT = 15.0

INITIALIZE_ARGS = {
    "clientID": "dap-smoke",
    "clientName": "dap_smoke.py",
    "adapterID": "xcode",
    "pathFormat": "path",
    "linesStartAt1": True,
    "columnsStartAt1": True,
    "supportsRunInTerminalRequest": False,
}


class DapClient:
    """Talks DAP (Content-Length framing) to a child process over stdio."""

    def __init__(self, argv, env=None, cwd=None, lenient=False):
        self.proc = subprocess.Popen(
            argv,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
            cwd=cwd,
        )
        self._buf = b""
        self._seq = 0
        # Every byte read from the adapter's stdout, for the purity check.
        self.raw = bytearray()
        # Skip stray bytes in front of a header instead of failing, so the
        # purity check can drive the session on and judge `raw` at the end.
        self.lenient = lenient

    # --- framing -----------------------------------------------------------

    def send(self, command: str, arguments=None) -> int:
        self._seq += 1
        msg = {"seq": self._seq, "type": "request", "command": command}
        if arguments is not None:
            msg["arguments"] = arguments
        body = json.dumps(msg).encode()
        frame = b"Content-Length: %d\r\n\r\n%s" % (len(body), body)
        self.proc.stdin.write(frame)
        self.proc.stdin.flush()
        return self._seq

    def _read_some(self, deadline: float) -> None:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("timed out waiting for DAP data")
        fd = self.proc.stdout.fileno()
        ready, _, _ = select.select([fd], [], [], remaining)
        if not ready:
            raise TimeoutError("timed out waiting for DAP data")
        chunk = os.read(fd, 65536)
        if not chunk:
            raise EOFError("xcode-dap closed stdout")
        self._buf += chunk
        self.raw += chunk

    def drain(self, timeout: float = DEFAULT_TIMEOUT) -> None:
        """Read the rest of stdout until EOF (after the adapter exited)."""
        deadline = time.monotonic() + timeout
        while True:
            try:
                self._read_some(deadline)
            except (EOFError, TimeoutError, OSError):
                return

    def read_message(self, timeout: float = DEFAULT_TIMEOUT) -> dict:
        deadline = time.monotonic() + timeout
        while True:
            if self.lenient:
                at = self._buf.find(b"Content-Length:")
                if at > 0:
                    self._buf = self._buf[at:]
                elif at == -1:
                    self._buf = self._buf[-len(b"Content-Length:"):]
            header_end = self._buf.find(b"\r\n\r\n")
            if header_end != -1:
                header = self._buf[:header_end].decode("utf-8", "replace")
                length = None
                for line in header.split("\r\n"):
                    name, _, value = line.partition(":")
                    if name.strip().lower() == "content-length":
                        length = int(value.strip())
                if length is None:
                    raise AssertionError(f"header without Content-Length: {header!r}")
                total = header_end + 4 + length
                if len(self._buf) >= total:
                    body = self._buf[header_end + 4 : total]
                    self._buf = self._buf[total:]
                    return json.loads(body)
            self._read_some(deadline)

    def wait_for_response(self, request_seq: int, timeout: float = DEFAULT_TIMEOUT) -> dict:
        """Read messages (collecting/ignoring events) until the response."""
        deadline = time.monotonic() + timeout
        while True:
            msg = self.read_message(timeout=max(0.1, deadline - time.monotonic()))
            if msg.get("type") == "response" and msg.get("request_seq") == request_seq:
                return msg

    # --- teardown ----------------------------------------------------------

    def close_stdin(self) -> None:
        if self.proc.stdin and not self.proc.stdin.closed:
            try:
                self.proc.stdin.close()
            except BrokenPipeError:
                pass  # the adapter already exited

    def wait_exit(self, timeout: float = DEFAULT_TIMEOUT) -> int:
        return self.proc.wait(timeout=timeout)

    def dump_stderr(self, timeout: float = 5.0) -> str:
        """stderr so far; stops after `timeout` (a leftover grandchild may hold
        the pipe open, so reading to EOF could block forever)."""
        data = b""
        deadline = time.monotonic() + timeout
        try:
            fd = self.proc.stderr.fileno()
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0 or not select.select([fd], [], [], remaining)[0]:
                    break
                chunk = os.read(fd, 65536)
                if not chunk:
                    break
                data += chunk
        except Exception:
            return data.decode("utf-8", "replace") + "<unreadable>"
        return data.decode("utf-8", "replace")

    def kill(self) -> None:
        if self.proc.poll() is None:
            self.proc.kill()
            self.proc.wait()


def check(cond: bool, what: str, client: DapClient) -> None:
    if cond:
        print(f"  ok: {what}")
        return
    print(f"  FAIL: {what}", file=sys.stderr)
    client.kill()
    print("--- xcode-dap stderr ---", file=sys.stderr)
    print(client.dump_stderr(), file=sys.stderr)
    sys.exit(1)


def cmd_roundtrip(args) -> int:
    binary = os.path.abspath(args.binary)
    if not os.path.exists(binary):
        print(f"binary not found: {binary} (run `cargo build` first)", file=sys.stderr)
        return 2

    print(f"roundtrip: {binary}")
    client = DapClient([binary])
    try:
        seq = client.send("initialize", INITIALIZE_ARGS)
        resp = client.wait_for_response(seq)
        check(resp.get("success") is True, "initialize response success", client)
        check(resp.get("command") == "initialize", "initialize response command", client)
        body = resp.get("body") or {}
        # Real lldb-dap capabilities prove spawn + verbatim forward (a fake
        # adapter would not know these).
        check(
            "supportsConfigurationDoneRequest" in body,
            "capabilities contain supportsConfigurationDoneRequest (lldb-dap)",
            client,
        )
        check(
            any(k.startswith("supports") for k in body),
            "capabilities body looks like lldb-dap's",
            client,
        )

        seq = client.send("disconnect", {})
        resp = client.wait_for_response(seq)
        check(resp.get("command") == "disconnect", "disconnect response received", client)
        check(resp.get("success") is True, "disconnect response success", client)

        # Zed closes the adapter's stdin after disconnect; mirror that and
        # expect a clean exit.
        client.close_stdin()
        code = client.wait_exit()
        check(code == 0, f"clean exit 0 (got {code})", client)
    except (TimeoutError, EOFError, subprocess.TimeoutExpired) as e:
        print(f"  FAIL: {e}", file=sys.stderr)
        client.kill()
        print("--- xcode-dap stderr ---", file=sys.stderr)
        print(client.dump_stderr(), file=sys.stderr)
        return 1

    print("roundtrip: PASS")
    return 0


# --- session (gate 3) -------------------------------------------------------


def process_table() -> dict:
    """pid -> (ppid, state, command) of every process (macOS and Linux ps)."""
    out = subprocess.run(
        ["ps", "-A", "-o", "pid=,ppid=,stat=,comm="], capture_output=True, text=True
    ).stdout
    table = {}
    for line in out.splitlines():
        parts = line.split(None, 3)
        if len(parts) < 3:
            continue
        try:
            pid, ppid = int(parts[0]), int(parts[1])
        except ValueError:
            continue
        table[pid] = (ppid, parts[2], parts[3] if len(parts) == 4 else "?")
    return table


def descendants(root_pid: int) -> dict:
    """pid -> command of every running descendant of `root_pid`.

    Snapshot it while the adapter is still alive: once it exits, its leftover
    children are re-parented and no longer reachable from its pid. Only these
    processes count as leftovers, so unrelated xcodebuild or lldb-dap runs on
    the same machine never fail the check.
    """
    table = process_table()
    children = {}
    for pid, (ppid, _, _) in table.items():
        children.setdefault(ppid, []).append(pid)
    found = {}
    stack = [root_pid]
    while stack:
        for child in children.get(stack.pop(), []):
            if child in found:
                continue
            _, state, command = table[child]
            if not state.startswith("Z"):  # exited, waiting to be reaped
                found[child] = os.path.basename(command)
            stack.append(child)
    return found


def still_running(tracked: dict) -> dict:
    """The tracked pid -> command entries that are still alive (zombies are not)."""
    table = process_table()
    return {
        pid: command
        for pid, command in tracked.items()
        if pid in table and not table[pid][1].startswith("Z")
    }


def wait_for_exit_of(tracked: dict, timeout: float = 5.0) -> dict:
    """Poll until every tracked process is gone; return the ones left."""
    deadline = time.monotonic() + timeout
    while True:
        left = still_running(tracked)
        if not left or time.monotonic() >= deadline:
            return left
        time.sleep(0.25)


def describe_pids(procs: dict) -> str:
    return ", ".join(f"{pid} {command}" for pid, command in sorted(procs.items())) or "none"


def pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True


class Recorder:
    """Pumps messages off a DapClient, recording everything seen."""

    def __init__(self, client: DapClient):
        self.client = client
        self.responses = {}  # request_seq -> response
        self.events = []  # all events
        self.outputs = []  # (category, text) of output events

    def _record(self, msg: dict) -> None:
        if msg.get("type") == "response":
            self.responses[msg.get("request_seq")] = msg
        elif msg.get("type") == "event":
            self.events.append(msg)
            if msg.get("event") == "output":
                body = msg.get("body") or {}
                self.outputs.append(
                    (body.get("category", ""), body.get("output", ""))
                )

    def pump_until(self, pred, what: str, timeout: float) -> dict:
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"timed out waiting for {what}")
            msg = self.client.read_message(timeout=remaining)
            self._record(msg)
            if pred(msg):
                return msg

    def response(self, request_seq: int, timeout: float) -> dict:
        if request_seq in self.responses:
            return self.responses[request_seq]
        return self.pump_until(
            lambda m: m.get("type") == "response"
            and m.get("request_seq") == request_seq,
            f"response to seq {request_seq}",
            timeout,
        )

    def output_containing(self, needle: str, timeout: float) -> None:
        if any(needle in text for _, text in self.outputs):
            return
        self.pump_until(
            lambda m: m.get("type") == "event"
            and m.get("event") == "output"
            and needle in (m.get("body") or {}).get("output", ""),
            f"output event containing {needle!r}",
            timeout,
        )

    def stdout_output(self, needle: str, timeout: float) -> None:
        if any(c == "stdout" and needle in t for c, t in self.outputs):
            return
        self.pump_until(
            lambda m: m.get("type") == "event"
            and m.get("event") == "output"
            and (m.get("body") or {}).get("category") == "stdout"
            and needle in (m.get("body") or {}).get("output", ""),
            f"stdout output event containing {needle!r}",
            timeout,
        )

    def app_console_output(self, timeout: float) -> None:
        """Any tailer-fed app output (stdout or stderr category).

        Real iOS apps typically log via NSLog/os_log, which lands on
        stderr — an empty stdout is normal (large real-world apps often
        write 0 bytes to stdout). On the success path stdout/stderr
        categories are emitted only by the app-file tailers, so either
        proves app console output is flowing.
        """
        if any(c in ("stdout", "stderr") for c, _ in self.outputs):
            return
        self.pump_until(
            lambda m: m.get("type") == "event"
            and m.get("event") == "output"
            and (m.get("body") or {}).get("category") in ("stdout", "stderr"),
            "app console output event (stdout/stderr category)",
            timeout,
        )


# --- session --kill-after-response ------------------------------------------
#
# Zed waits for the answer to its disconnect with no timeout and kills the
# adapter as soon as it arrives, so whatever the adapter has not cleaned up by
# then is orphaned: lldb-dap, the app, and the OSLog `log stream`, which runs in
# its own process group. This mode ends the session the same way and checks
# that the adapter finished its teardown before it answered.

# xcrun wrapper on PATH for this mode. The mock pipeline's OSLog pump runs
# `xcrun simctl spawn mock log stream ...`; the stand-in prints one line and
# then ignores SIGTERM, so the adapter's bounded stop has to SIGKILL the group.
# Everything else (lldb-dap) goes to the real xcrun.
KILL_MODE_XCRUN = """#!/bin/sh
# xcrun wrapper for `dap_smoke.py session --kill-after-response`, generated
# into a temp dir.
if [ "$1" = "simctl" ] && [ "$2" = "spawn" ]; then
  echo "mock oslog line"
  trap '' TERM
  exec sleep 300
fi
exec @XCRUN@ "$@"
"""

# Longest the adapter may hold the disconnect answer: lldb-dap's answer (the
# adapter gives it 3 s) plus the 2 s critical teardown, plus slack for a
# loaded machine.
KILL_MODE_HOLD_LIMIT = 6.0


def kill_mode_env(root: str):
    """(env, home) for a kill-after-response run: HOME in `root`, the xcrun
    wrapper first on PATH. None when no real xcrun is on PATH."""
    xcrun = shutil.which("xcrun")
    if not xcrun:
        return None
    home = os.path.join(root, "home")
    bindir = os.path.join(root, "bin")
    os.makedirs(home)
    os.makedirs(bindir)
    wrapper = os.path.join(bindir, "xcrun")
    with open(wrapper, "w") as f:
        f.write(KILL_MODE_XCRUN.replace("@XCRUN@", shlex.quote(os.path.abspath(xcrun))))
    os.chmod(wrapper, 0o755)
    env = dict(os.environ)
    env.update(HOME=home, PATH=bindir + os.pathsep + env.get("PATH", ""))
    # The checks read INFO lines ("teardown: done"); a quieter level set in
    # the caller's environment would fail them although teardown worked.
    if env.get("XCODE_DAP_LOG", "").lower() not in ("info", "debug", "trace"):
        env.pop("XCODE_DAP_LOG", None)
    return env, home


def adapter_log_lines(home: str, pid: int) -> list:
    """This adapter's lines of xcode-dap.log, in order."""
    path = os.path.join(home, ".zedxcode", "logs", "xcode-dap.log")
    try:
        with open(path, errors="replace") as f:
            lines = f.read().splitlines()
    except OSError:
        return []
    tag = f"[pid {pid} dap]"
    return [line for line in lines if tag in line]


def first_index(lines: list, needle: str) -> int:
    return next((i for i, line in enumerate(lines) if needle in line), -1)


def children_of(parent_pid: int) -> dict:
    """pid -> command of the running direct children of `parent_pid`."""
    return {
        pid: os.path.basename(command)
        for pid, (ppid, state, command) in process_table().items()
        if ppid == parent_pid and not state.startswith("Z")
    }


def kill_after_response(client, rec, started: dict, home: str, dummy_pid) -> None:
    """Steps 8-9 of the kill-after-response mode (see the module docstring)."""
    adapter_pid = client.proc.pid

    def verify(cond: bool, what: str) -> None:
        if not cond:  # the log lives in a temp HOME that is removed on exit
            print("--- xcode-dap.log (this adapter) ---", file=sys.stderr)
            print("\n".join(adapter_log_lines(home, adapter_pid)[-40:]), file=sys.stderr)
        check(cond, what, client)

    pidfile = os.path.join(home, ".zedxcode", "run", "sim-mock.pid")
    verify(os.path.exists(pidfile), "the session holds its pidfile")
    commands = list(started.values())
    verify(
        any("lldb" in c for c in commands) and "mock_app" in commands and "sleep" in commands,
        "the adapter runs lldb-dap, the mock app and the log stream stand-in "
        f"(tracked: {describe_pids(started)})",
    )
    # The adapter's own children (lldb-dap, the mock app, the log stream
    # stand-in) are the ones it reaps before answering. Their children
    # (lldb-dap's debugserver / lldb-server) may take a moment longer to go
    # and are left to the check after the SIGKILL. On macOS a process the
    # debugger has attached to shows debugserver as its parent, so the mock
    # app is found by the pid it announced rather than by parentage.
    own = children_of(adapter_pid)
    if dummy_pid is not None and pid_alive(dummy_pid):
        own.setdefault(dummy_pid, started.get(dummy_pid, "mock_app"))
    verify(
        "mock_app" in own.values() and "sleep" in own.values(),
        "the mock app and the log stream stand-in are the adapter's own children "
        f"(children: {describe_pids(own)})",
    )
    asked = time.monotonic()
    disc_seq = client.send("disconnect", {"terminateDebuggee": True})
    resp = rec.response(disc_seq, KILL_MODE_HOLD_LIMIT + DEFAULT_TIMEOUT)
    held = time.monotonic() - asked
    # The moment the answer is in: what the log holds and what still runs.
    log_at_answer = adapter_log_lines(home, adapter_pid)
    running_at_answer = still_running(own)
    pidfile_at_answer = os.path.exists(pidfile)
    os.kill(adapter_pid, signal.SIGKILL)
    client.wait_exit()

    verify(resp.get("command") == "disconnect", "disconnect response received")
    verify(resp.get("success") is True, "disconnect response success")
    done = first_index(log_at_answer, "teardown: done")
    answered = first_index(log_at_answer, f"disconnect (seq {disc_seq}): answered after teardown")
    verify(
        done != -1 and answered != -1 and done < answered,
        "'teardown: done' is in the log before the response arrived "
        f"(line {done}; answer logged at line {answered})",
    )
    verify(
        first_index(log_at_answer, "oslog pump did not stop in time") != -1,
        "the log stream stand-in ignored SIGTERM and the bounded stop SIGKILLed it",
    )
    verify(
        held <= KILL_MODE_HOLD_LIMIT,
        f"the answer was held {held:.2f}s (limit {KILL_MODE_HOLD_LIMIT:.0f}s)",
    )
    verify(
        not running_at_answer,
        "none of the adapter's own children runs when the answer arrives "
        f"(children: {describe_pids(own)}; left: {describe_pids(running_at_answer)})",
    )
    left = wait_for_exit_of(started, timeout=2.0)
    verify(
        not left,
        f"no lldb-dap, mock app or log stream left after the SIGKILL ({len(started)} "
        f"tracked: {describe_pids(started)}; left: {describe_pids(left)})",
    )
    verify(not pidfile_at_answer, "pidfile released before the answer")
    if dummy_pid is not None:
        verify(not pid_alive(dummy_pid), f"dummy app (pid {dummy_pid}) terminated")


def cmd_session(args) -> int:
    binary = os.path.abspath(args.binary)
    if not os.path.exists(binary):
        print(f"binary not found: {binary} (run `cargo build` first)", file=sys.stderr)
        return 2
    mock = args.mock_pipeline
    if not mock and not (args.workspace and args.scheme):
        print("session: --workspace and --scheme are required without "
              "--mock-pipeline", file=sys.stderr)
        return 2
    kill_mode = args.kill_after_response
    if kill_mode and not mock:
        print("session: --kill-after-response needs --mock-pipeline", file=sys.stderr)
        return 2
    if not kill_mode:
        return run_session(args, binary, mock, None, None)
    root = tempfile.mkdtemp(prefix="zedx-kill.")
    try:
        prepared = kill_mode_env(root)
        if prepared is None:
            print("session: --kill-after-response needs xcrun on PATH", file=sys.stderr)
            return 2
        env, home = prepared
        return run_session(args, binary, mock, env, home)
    finally:
        shutil.rmtree(root, ignore_errors=True)


def run_session(args, binary: str, mock: bool, env, home) -> int:
    """The session mode; `home` (with `env`) selects --kill-after-response."""
    kill_mode = home is not None
    label = "session"
    if mock:
        label += " (mock, kill after response)" if kill_mode else " (mock)"
    timeout = args.timeout or (60.0 if mock else 1800.0)
    argv = [binary] + (["--mock-pipeline"] if mock else [])
    print(f"{label}: {' '.join(argv)}")

    client = DapClient(argv, env=env)
    # Processes the adapter started, collected while it runs (see descendants()).
    started = {}
    rec = Recorder(client)
    try:
        # 1. initialize
        seq = client.send("initialize", INITIALIZE_ARGS)
        resp = rec.response(seq, DEFAULT_TIMEOUT)
        check(resp.get("success") is True, "initialize response success", client)

        # 2. launch (config = flattened scenario config; the mock ignores it)
        if mock:
            config = {"workspace": "/nonexistent.xcworkspace", "scheme": "Mock"}
            if kill_mode:
                config["oslog"] = True  # answered by the xcrun wrapper's stand-in
        else:
            config = {"workspace": args.workspace, "scheme": args.scheme}
            for key, value in (
                ("device", args.device),
                ("os", args.os),
                ("configuration", args.configuration),
                ("preflight", args.preflight),
            ):
                if value:
                    config[key] = value
        launch_seq = client.send("launch", config)

        # 3. pipeline output events stream, then lldb-dap's initialized
        #    event (emitted only after the attach created a target).
        rec.pump_until(
            lambda m: m.get("type") == "event" and m.get("event") == "initialized",
            "initialized event",
            timeout,
        )
        print("  ok: initialized event (attach created a target)")
        check(
            len(rec.outputs) >= 1,
            f"output events streamed before initialized ({len(rec.outputs)} seen)",
            client,
        )

        # 4. setBreakpoints
        bp_file = os.path.abspath(args.bp_file or __file__)
        bp_line = args.bp_line or 30
        bp_seq = client.send(
            "setBreakpoints",
            {
                "source": {"path": bp_file},
                "breakpoints": [{"line": bp_line}],
                "lines": [bp_line],
            },
        )
        resp = rec.response(bp_seq, DEFAULT_TIMEOUT)
        check(resp.get("success") is True, "setBreakpoints response success", client)
        bps = (resp.get("body") or {}).get("breakpoints") or []
        check(len(bps) == 1, f"one breakpoint in response (got {len(bps)})", client)
        if mock:
            # The dummy has no symbols for this source — unverified is fine.
            print(f"  ok: breakpoint accepted (verified={bps[0].get('verified')}, "
                  "mock: unverified allowed)")
        else:
            # lldb-dap commonly answers verified=false while the process sits
            # at _dyld_start (debug info not resolved yet) and verifies the
            # breakpoint later via `breakpoint` change events. The hard gate
            # is the actual HIT (stopped reason=breakpoint) asserted below.
            print(f"  ok: breakpoint accepted (verified={bps[0].get('verified')}"
                  f"{', message=' + repr(bps[0].get('message')) if bps[0].get('message') else ''}"
                  "; hard gate = the hit below)")

        # 5. configurationDone (lldb-dap auto-continues the process)
        cd_seq = client.send("configurationDone", {})
        resp = rec.response(cd_seq, DEFAULT_TIMEOUT)
        check(resp.get("success") is True, "configurationDone response success", client)

        # 6. launch response = rewritten attach response
        resp = rec.response(launch_seq, DEFAULT_TIMEOUT)
        check(resp.get("success") is True, "launch response success", client)
        check(
            resp.get("command") == "launch",
            f"launch response command rewritten to 'launch' "
            f"(got {resp.get('command')!r})",
            client,
        )
        rec.output_containing("Debugger attached", DEFAULT_TIMEOUT)
        print("  ok: 'Debugger attached' console output")
        started.update(descendants(client.proc.pid))

        # 6b. real run: the breakpoint set in didFinishLaunching must HIT —
        #     expect a stopped(reason=breakpoint) event, then continue.
        #     (Mock dummy has no symbols for the bp source, so skip there.)
        if not mock:
            def is_bp_stop(m):
                return (
                    m.get("type") == "event"
                    and m.get("event") == "stopped"
                    and (m.get("body") or {}).get("reason") == "breakpoint"
                )

            stopped = next((e for e in rec.events if is_bp_stop(e)), None)
            if stopped is None:
                stopped = rec.pump_until(
                    is_bp_stop, "stopped(reason=breakpoint) event", 120.0
                )
            body = stopped.get("body") or {}
            print(f"  ok: stopped event (reason={body.get('reason')}, "
                  f"threadId={body.get('threadId')}) — breakpoint hit")
            bp_changes = [
                e for e in rec.events
                if e.get("event") == "breakpoint"
                and ((e.get("body") or {}).get("breakpoint") or {}).get("verified")
            ]
            if bp_changes:
                print(f"  ok: breakpoint verified via {len(bp_changes)} "
                      "breakpoint change event(s)")
            cont_seq = client.send(
                "continue", {"threadId": body.get("threadId") or 1}
            )
            resp = rec.response(cont_seq, DEFAULT_TIMEOUT)
            check(resp.get("success") is True, "continue response success", client)

        # 7. app is running: continued/process events may or may not appear;
        #    the authoritative signal is app output flowing via the tailers.
        if mock:
            rec.stdout_output("mock-app stdout", 30.0)
            print("  ok: app stdout output events flowing")
            if kill_mode:
                rec.output_containing("mock oslog line", DEFAULT_TIMEOUT)
                print("  ok: OSLog pump running (stand-in log stream line arrived)")
        else:
            rec.app_console_output(30.0)
            print("  ok: app console output events flowing (stdout/stderr)")
        ran = [e.get("event") for e in rec.events if e.get("event") in
               ("continued", "process")]
        if ran:
            print(f"  ok: saw {'/'.join(sorted(set(ran)))} event(s)")

        # Mock: learn the dummy pid from the pipeline console output.
        dummy_pid = None
        if mock:
            for _, text in rec.outputs:
                if "Launched mock app (pid " in text:
                    dummy_pid = int(text.split("(pid ")[1].split(")")[0])
            check(dummy_pid is not None, "dummy pid announced in console", client)

        # 8. disconnect -> response -> clean exit
        started.update(descendants(client.proc.pid))
        # The leftover check (9) only means something if the process walk
        # works: lldb-dap is always one of the adapter's children here.
        check(
            any("lldb" in command for command in started.values()),
            f"process walk finds the adapter's lldb-dap child "
            f"(tracked: {describe_pids(started)})",
            client,
        )
        if kill_mode:
            kill_after_response(client, rec, started, home, dummy_pid)
            print(f"{label}: PASS")
            return 0
        disc_seq = client.send("disconnect", {"terminateDebuggee": True})
        resp = rec.response(disc_seq, DEFAULT_TIMEOUT)
        check(resp.get("command") == "disconnect", "disconnect response received", client)
        check(resp.get("success") is True, "disconnect response success", client)
        client.close_stdin()
        code = client.wait_exit()
        check(code == 0, f"clean exit 0 (got {code})", client)

        # 9. nothing the adapter started is still running (lldb-dap and its
        #    debug server, xcodebuild, log stream, the mock dummy).
        left = wait_for_exit_of(started)
        check(
            not left,
            f"no leftover process of the adapter ({len(started)} tracked: "
            f"{describe_pids(started)}; left: {describe_pids(left)})",
            client,
        )
        if dummy_pid is not None:
            check(not pid_alive(dummy_pid), f"dummy app (pid {dummy_pid}) terminated",
                  client)
    except (TimeoutError, EOFError, subprocess.TimeoutExpired) as e:
        print(f"  FAIL: {e}", file=sys.stderr)
        client.kill()
        print("--- xcode-dap stderr ---", file=sys.stderr)
        print(client.dump_stderr(), file=sys.stderr)
        return 1

    print(f"{label}: PASS")
    return 0


# --- purity: stdout carries only DAP frames -----------------------------------
#
# A real (non-mock) launch against a temp project, with every tool the adapter
# spawns in DAP mode replaced by a fake on PATH. A fake prints a canary line on
# stdout wherever its caller ignores stdout or skips extra lines, and carries
# it as an extra JSON key where the caller parses JSON; a fake whose whole
# stdout is a value (a path, a bundle id) prints just that, and output the
# adapter forwards on purpose (the preflight, the build log, `log stream`, the
# app's console) gets ordinary lines. A child that inherited the adapter's
# stdout would put bytes outside the Content-Length frames, and usually the
# canary with them.

CANARY = "ZEDX-STDOUT-CANARY"
PURITY_UDID = "AAAAAAAA-BBBB-CCCC-DDDD-EEEEEEEEEEEE"
PURITY_BUNDLE_ID = "com.example.MyApp"
# Every spawn the launch must reach, as (name, pattern matched against the start
# of a fake-log line), so the check keeps covering each spawn site and not just
# each program: a launch that stops early after `simctl list` would otherwise
# still count xcrun as reached. If a pipeline change stops reaching one, make
# the fakes answer what the new code expects rather than shortening this list.
PURITY_SPAWNS = (
    ("xcrun lldb-dap", r"xcrun lldb-dap\b"),
    ("lldb-dap", r"lldb-dap\b"),
    ("simctl list", r"xcrun simctl list\b"),
    ("xcode-select -p", r"xcode-select -p\b"),
    ("open <Simulator.app>", r"open /\S*/Simulator\.app$"),
    ("open -a Simulator", r"open -a Simulator\b"),
    ("xcodebuild -showBuildSettings", r"xcodebuild .*-showBuildSettings\b"),
    ("xcodebuild build", r"xcodebuild (?!.*-showBuildSettings).* build$"),
    ("git check-ignore", r"git check-ignore\b"),
    ("plutil -extract", r"plutil -extract\b"),
    ("simctl install", r"xcrun simctl install\b"),
    ("simctl launch", r"xcrun simctl launch\b"),
    ("simctl spawn ... log stream", r"xcrun simctl spawn \S+ log stream\b"),
    ("simctl terminate", r"xcrun simctl terminate\b"),
)

FAKE_HEADER = """#!/bin/sh
# Fake @NAME@ for `dap_smoke.py purity`, generated into a temp dir.
printf '%s\\n' "@NAME@ $*" >>"$ZEDX_FAKE_LOG"
"""

FAKE_XCRUN = FAKE_HEADER + """if [ "$1" = "lldb-dap" ]; then
  shift
  exec "$(dirname "$0")/lldb-dap" "$@"
fi
if [ "$1" != "simctl" ]; then
  echo "@CANARY@ xcrun $*"
  echo "fake xcrun: unsupported: $*" >&2
  exit 1
fi
shift
case "$1" in
  list)
    cat <<'JSON'
@SIMCTL_LIST@
JSON
    ;;
  launch)
    # The app's console: the adapter tails this file into output events.
    for arg in "$@"; do
      case "$arg" in --stdout=*) echo "MyApp stdout line" >>"${arg#--stdout=}" ;; esac
    done
    for bundle in "$@"; do :; done
    echo "@CANARY@ xcrun simctl launch"
    echo "$bundle: $$"
    ;;
  spawn)
    # `log stream`: forwarded on purpose, so an ordinary line; then block
    # until the adapter stops the stream.
    echo "MyApp oslog line"
    exec sleep 300
    ;;
  *)
    echo "@CANARY@ xcrun simctl $*"
    ;;
esac
"""

FAKE_XCODEBUILD = FAKE_HEADER + """case " $* " in
  *" -showBuildSettings "*)
    cat <<'JSON'
@SETTINGS@
JSON
    ;;
  *" -list "*)
    cat <<'JSON'
@LIST@
JSON
    ;;
  *" build "*)
    # The build log is forwarded on purpose (through the build filter).
    echo "note: building MyApp with a fake xcodebuild"
    echo "** BUILD SUCCEEDED **"
    ;;
  *)
    echo "@CANARY@ xcodebuild $*"
    ;;
esac
"""

# Always fails, so the launch reaches both spawns of the simulator window (the
# developer dir's Simulator.app, then the fallback `open -a Simulator`) and must
# carry on headless: a window that does not open is never fatal.
FAKE_OPEN = FAKE_HEADER + """echo "@CANARY@ open $*"
case "$1" in
  /*.app) echo "fake open: cannot open $1" >&2 ;;
  *) echo "fake open: Unable to find application named 'Simulator'" >&2 ;;
esac
exit 1
"""
# The console line for a simulator window that did not open, carrying the
# preferred app's error rather than the fallback's.
PURITY_WINDOW_LINE = "Could not open the simulator window (fake open: cannot open "

FAKE_GIT = FAKE_HEADER + """case "$1" in
  rev-parse)
    if [ "$2" = "--git-path" ]; then
      echo ".git/$3"  # the caller reads this path, so no canary
    else
      echo "@CANARY@ git $*"
      echo ".git"
    fi
    ;;
  check-ignore | ls-files)
    # Print the path even under -q (a bare check-ignore or ls-files does on a
    # match), then report no match so the caller carries on.
    echo "@CANARY@ git $*"
    for path in "$@"; do :; done
    echo "$path"
    exit 1
    ;;
  *)
    echo "@CANARY@ git $*"
    ;;
esac
"""

# Answers `-p` for the simulator window's developer dir lookup
# (DEVELOPER_DIR is removed from the adapter's environment).
FAKE_XCODE_SELECT = FAKE_HEADER + """if [ "$1" = "-p" ] || [ "$1" = "--print-path" ]; then
  echo "@DEVELOPER_DIR@"  # the caller reads the path, so no canary
else
  echo "@CANARY@ xcode-select $*"
fi
"""

FAKE_PLUTIL = FAKE_HEADER + """if [ "$1" = "-extract" ]; then
  echo "@BUNDLE_ID@"  # the caller reads the value, so no canary
else
  echo "@CANARY@ plutil $*"
fi
"""

FAKE_LLDB_DAP = '''#!@PYTHON@
"""Fake lldb-dap for `dap_smoke.py purity`: answers every request with success.

Each frame it writes carries the canary in an extra header, which the adapter
must drop when it re-frames lldb-dap's messages for the client.
"""
import json
import os
import sys

with open(os.environ["ZEDX_FAKE_LOG"], "a") as log:
    log.write(" ".join(["lldb-dap"] + sys.argv[1:]) + "\\n")

stdin, stdout = sys.stdin.buffer, sys.stdout.buffer
seq = 0


def send(message):
    global seq
    seq += 1
    message["seq"] = seq
    body = json.dumps(message).encode()
    stdout.write(b"X-Canary: @CANARY@ lldb-dap\\r\\n")
    stdout.write(b"Content-Length: %d\\r\\n\\r\\n" % len(body) + body)
    stdout.flush()


def receive():
    length = 0
    while True:
        line = stdin.readline()
        if not line:
            return None
        line = line.strip()
        if not line:
            break
        name, _, value = line.decode().partition(":")
        if name.strip().lower() == "content-length":
            length = int(value)
    return json.loads(stdin.read(length))


while True:
    request = receive()
    if request is None:
        break
    if request.get("type") != "request":
        continue
    command = request.get("command")
    body = {}
    if command == "initialize":
        body = {"supportsConfigurationDoneRequest": True}
    elif command == "evaluate":
        body = {"result": "", "variablesReference": 0}
    send({"type": "response", "request_seq": request.get("seq"), "success": True,
          "command": command, "body": body})
    if command == "attach":
        send({"type": "event", "event": "initialized"})
    if command == "disconnect":
        break
'''


def simctl_devices_json() -> str:
    return json.dumps(
        {
            # Parse-neutral canary: the adapter ignores unknown keys.
            "zedxCanary": f"{CANARY} xcrun simctl list",
            "devices": {
                "com.apple.CoreSimulator.SimRuntime.iOS-18-4": [
                    {
                        "udid": PURITY_UDID,
                        "name": "iPhone 15",
                        "state": "Booted",
                        "isAvailable": True,
                        "deviceTypeIdentifier":
                            "com.apple.CoreSimulator.SimDeviceType.iPhone-15",
                    }
                ],
                "com.apple.CoreSimulator.SimRuntime.iOS-17-5": [
                    {
                        "udid": "AAAAAAAA-BBBB-CCCC-DDDD-FFFFFFFFFFFF",
                        "name": "iPhone SE (3rd generation)",
                        "state": "Shutdown",
                        "isAvailable": True,
                        "deviceTypeIdentifier":
                            "com.apple.CoreSimulator.SimDeviceType.iPhone-SE-3rd-generation",
                    }
                ],
            },
        },
        indent=2,
    )


def build_settings_json(derived_data: str) -> str:
    products = f"{derived_data}/Build/Products"
    return json.dumps(
        [
            {
                "zedxCanary": f"{CANARY} xcodebuild -showBuildSettings",
                "action": "build",
                "target": "MyApp",
                "buildSettings": {
                    "BUILD_DIR": products,
                    "TARGET_BUILD_DIR": f"{products}/Debug-iphonesimulator",
                    "WRAPPER_NAME": "MyApp.app",
                    "PRODUCT_BUNDLE_IDENTIFIER": PURITY_BUNDLE_ID,
                },
            }
        ],
        indent=2,
    )


def xcodebuild_list_json() -> str:
    return json.dumps(
        {
            "zedxCanary": f"{CANARY} xcodebuild -list",
            "project": {
                "configurations": ["Debug", "Release"],
                "name": "MyApp",
                "schemes": ["MyApp"],
                "targets": ["MyApp"],
            },
        },
        indent=2,
    )


def write_fakes(bindir: str, derived_data: str, developer_dir: str) -> list:
    python = sys.executable
    if not python or not os.path.isabs(python) or any(c.isspace() for c in python):
        python = "/usr/bin/env python3"
    scripts = {
        "xcrun": FAKE_XCRUN.replace("@SIMCTL_LIST@", simctl_devices_json()),
        "xcodebuild": FAKE_XCODEBUILD.replace(
            "@SETTINGS@", build_settings_json(derived_data)
        ).replace("@LIST@", xcodebuild_list_json()),
        "open": FAKE_OPEN,
        "git": FAKE_GIT,
        "plutil": FAKE_PLUTIL.replace("@BUNDLE_ID@", PURITY_BUNDLE_ID),
        "xcode-select": FAKE_XCODE_SELECT.replace("@DEVELOPER_DIR@", developer_dir),
        "lldb-dap": FAKE_LLDB_DAP.replace("@PYTHON@", python),
    }
    for name, text in scripts.items():
        path = os.path.join(bindir, name)
        with open(path, "w") as f:
            f.write(text.replace("@NAME@", name).replace("@CANARY@", CANARY))
        os.chmod(path, 0o755)
    return list(scripts)


def check_stdout_purity(raw: bytes):
    """Split `raw` into back-to-back `Content-Length: N\\r\\n\\r\\n` + N-byte JSON
    frames, exactly as the adapter writes them. Returns (messages, problems):
    any byte outside a frame, a malformed frame or a canary is a problem."""
    messages, problems = [], []
    pos = 0
    while pos < len(raw):
        end = raw.find(b"\r\n\r\n", pos)
        match = re.fullmatch(rb"Content-Length: (\d+)", raw[pos:end]) if end != -1 else None
        if not match:
            problems.append(f"bytes outside any frame at offset {pos}: {raw[pos:pos + 160]!r}")
            break
        start = end + 4
        length = int(match.group(1))
        if start + length > len(raw):
            problems.append(
                f"frame at offset {pos} is cut short: {len(raw) - start} of {length} body bytes"
            )
            break
        body = raw[start:start + length]
        try:
            message = json.loads(body)
        except ValueError as e:
            problems.append(f"frame at offset {pos} does not hold JSON ({e}): {body[:160]!r}")
            break
        if not isinstance(message, dict):
            problems.append(f"frame at offset {pos} is not a JSON object: {body[:160]!r}")
            break
        messages.append(message)
        pos = start + length
    token = CANARY.encode()
    at = raw.find(token)
    while at != -1:
        problems.append(f"canary at offset {at}: {raw[max(0, at - 40):at + 80]!r}")
        at = raw.find(token, at + len(token))
    return messages, problems


def cmd_purity(args) -> int:
    binary = os.path.abspath(args.binary)
    if not os.path.exists(binary):
        print(f"binary not found: {binary} (run `cargo build` first)", file=sys.stderr)
        return 2
    root = tempfile.mkdtemp(prefix="zedx-purity.")
    try:
        return run_purity(binary, root, args.timeout or 60.0)
    finally:
        shutil.rmtree(root, ignore_errors=True)


def run_purity(binary: str, root: str, timeout: float) -> int:
    home = os.path.join(root, "home")
    bindir = os.path.join(root, "bin")
    project = os.path.join(root, "MyApp")
    fake_log = os.path.join(root, "fake-invocations.log")
    for d in (home, bindir, os.path.join(project, ".zed")):
        os.makedirs(d)
    developer_dir = os.path.join(root, "Xcode.app", "Contents", "Developer")
    # The Xcode 26 layout, so the simulator window tries the bundle path first.
    os.makedirs(os.path.join(developer_dir, "Applications", "Simulator.app"))
    fakes = write_fakes(bindir, os.path.join(root, "DerivedData", "MyApp"), developer_dir)
    open(fake_log, "w").close()
    # An "Xcode" scenario opts the project in to buildServer.json, whose first
    # write git-ignores it (the git probes).
    with open(os.path.join(project, ".zed", "debug.json"), "w") as f:
        json.dump(
            [{"label": "MyApp", "adapter": "Xcode", "request": "launch",
              "workspace": "$ZED_WORKTREE_ROOT/MyApp.xcodeproj", "scheme": "MyApp"}],
            f,
            indent=2,
        )
    env = dict(os.environ)
    env.pop("DEVELOPER_DIR", None)
    env.update(
        HOME=home,
        PATH=bindir + os.pathsep + env.get("PATH", ""),
        ZEDX_FAKE_LOG=fake_log,
    )
    config = {
        # Missing until the preflight creates it, so the preflight runs too.
        "workspace": os.path.join(project, "MyApp.xcodeproj"),
        "scheme": "MyApp",
        "preflight": "echo 'MyApp.xcodeproj generated' && mkdir MyApp.xcodeproj",
        "oslog": True,
    }

    print(f"purity: {binary} (fakes on PATH: {', '.join(fakes)})")
    client = DapClient([binary], env=env, cwd=project, lenient=True)
    rec = Recorder(client)
    started = {}
    # The fakes are built for a launch that succeeds end to end. A launch that
    # fails or a session that ends early skips the spawns after that point, so
    # both fail the check; the session still runs on to judge stdout as a whole.
    session_failures = []
    try:
        seq = client.send("initialize", INITIALIZE_ARGS)
        rec.response(seq, DEFAULT_TIMEOUT)
        launch_seq = client.send("launch", config)
        resp = rec.response(launch_seq, timeout)
        if resp.get("success") is True:
            print("  ok: launch succeeded against the fakes")
            # Every `open` failed, so the launch only got here because the
            # simulator window is never fatal; the reason must reach the console.
            try:
                rec.output_containing(PURITY_WINDOW_LINE, DEFAULT_TIMEOUT)
                print("  ok: the failed simulator window was one console line, not fatal")
            except TimeoutError:
                session_failures.append(
                    f"no output event with {PURITY_WINDOW_LINE!r} although every `open` failed"
                )
            # One line each from the console tailer and the log stream.
            for needle in ("MyApp stdout line", "MyApp oslog line"):
                try:
                    rec.output_containing(needle, DEFAULT_TIMEOUT)
                    print(f"  ok: forwarded output {needle!r} arrived in an output event")
                except TimeoutError:
                    print(f"  note: no output event with {needle!r}")
        else:
            session_failures.append(
                f"the launch failed against the fakes ({resp.get('message')}), so the "
                "spawns after the failing step went unchecked: make the fakes answer "
                "what the pipeline now expects"
            )
        started.update(descendants(client.proc.pid))
        disc_seq = client.send("disconnect", {"terminateDebuggee": True})
        rec.response(disc_seq, DEFAULT_TIMEOUT)
    except (TimeoutError, EOFError, AssertionError, ValueError, OSError) as e:
        session_failures.append(
            f"the session ended early ({e!r}), so the spawns after that point went unchecked"
        )
    client.close_stdin()
    try:
        code = client.wait_exit(DEFAULT_TIMEOUT)
    except subprocess.TimeoutExpired:
        client.kill()
        code = None
    client.drain(timeout=5.0)
    left = wait_for_exit_of(started)
    for pid in left:
        try:
            os.kill(pid, 9)
        except OSError:
            pass

    messages, problems = check_stdout_purity(bytes(client.raw))
    with open(fake_log) as f:
        invoked = [line.rstrip("\n") for line in f if line.strip()]
    print(f"  adapter exit code: {code}; stdout: {len(client.raw)} bytes in "
          f"{len(messages)} frames; processes it started: {describe_pids(started)}; "
          f"fake invocations: {len(invoked)}")
    for line in invoked:
        print(f"    {line if len(line) <= 110 else line[:107] + '...'}")

    failures = list(problems) + session_failures
    missing = [
        name
        for name, pattern in PURITY_SPAWNS
        if not any(re.match(pattern, line) for line in invoked)
    ]
    if missing:
        failures.append(
            f"the launch never reached these spawns: {', '.join(missing)}: make the "
            "fakes answer what the pipeline now expects, so they stay covered"
        )
    # The fake log stream blocks in `sleep` until teardown, so a working process
    # walk always sees it; without it the leftover check below is a no-op.
    if "sleep" not in started.values():
        failures.append(
            "the process walk did not find the log stream's stand-in (sleep) among "
            f"the adapter's children (found: {describe_pids(started)}), so the "
            "leftover-process check covers nothing"
        )
    if code is None:
        failures.append("the adapter did not exit after disconnect and stdin EOF")
    if left:
        failures.append(f"processes the adapter started are still running: {describe_pids(left)}")
    if failures:
        for failure in failures:
            print(f"  FAIL: {failure}", file=sys.stderr)
        print("--- xcode-dap stderr ---", file=sys.stderr)
        print(client.dump_stderr(), file=sys.stderr)
        log_path = os.path.join(home, ".zedxcode", "logs", "xcode-dap.log")
        if os.path.exists(log_path):
            with open(log_path, errors="replace") as f:
                print("--- xcode-dap.log (last 40 lines) ---", file=sys.stderr)
                print("".join(f.readlines()[-40:]), file=sys.stderr)
        return 1
    print("  ok: every stdout byte is inside a Content-Length frame; no canary")
    print("purity: PASS")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(
        description="Scripted DAP client smoke test for xcode-dap"
    )
    parser.add_argument(
        "--binary",
        default="target/debug/xcode-dap",
        help="path to the xcode-dap binary under test",
    )
    sub = parser.add_subparsers(dest="subcommand", required=True)

    p_roundtrip = sub.add_parser(
        "roundtrip",
        help="initialize/disconnect roundtrip against real lldb-dap (gate 1)",
    )
    p_roundtrip.set_defaults(func=cmd_roundtrip)

    p_session = sub.add_parser(
        "session",
        help="full scripted DAP session: launch -> breakpoints -> app output "
        "-> disconnect (gate 3)",
    )
    p_session.add_argument(
        "--mock-pipeline",
        action="store_true",
        help="pass the hidden --mock-pipeline flag (no Xcode needed; "
        "breakpoint may be unverified)",
    )
    p_session.add_argument(
        "--kill-after-response",
        action="store_true",
        help="with --mock-pipeline: SIGKILL the adapter right after the "
        "disconnect response, as Zed does, and assert teardown was done by "
        "then (HOME is redirected to a temp dir for the run)",
    )
    p_session.add_argument("--workspace", help="path to .xcworkspace/.xcodeproj")
    p_session.add_argument("--scheme", help="Xcode scheme")
    p_session.add_argument("--device", help="simulator name or UDID")
    p_session.add_argument("--os", help="simulator OS version, e.g. 26.3")
    p_session.add_argument("--configuration", help="build configuration")
    p_session.add_argument("--preflight", help="preflight command")
    p_session.add_argument("--bp-file", help="source file for setBreakpoints")
    p_session.add_argument("--bp-line", type=int, help="breakpoint line")
    p_session.add_argument(
        "--timeout",
        type=float,
        help="launch/build timeout in seconds (default: 60 mock, 1800 real)",
    )
    p_session.set_defaults(func=cmd_session)

    p_purity = sub.add_parser(
        "purity",
        help="stdout purity: a real launch against PATH-shimmed fakes that "
        "print a canary; stdout must hold only Content-Length frames",
    )
    p_purity.add_argument(
        "--timeout",
        type=float,
        help="launch timeout in seconds (default: 60)",
    )
    p_purity.set_defaults(func=cmd_purity)

    args = parser.parse_args()
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
