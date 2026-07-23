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
              app stdout output events -> disconnect -> clean exit,
              no zombie lldb-dap/xcodebuild. (gate 3)

Usage (note: --binary belongs to the top-level parser, before the subcommand):
  python3 tests/dap_smoke.py [--binary target/debug/xcode-dap] roundtrip
  python3 tests/dap_smoke.py [--binary PATH] session --mock-pipeline
  python3 tests/dap_smoke.py [--binary PATH] session --workspace W --scheme S
          [--device D] [--os V] [--configuration C] [--preflight CMD]
          --bp-file FILE --bp-line N [--timeout SECS]
"""

import argparse
import json
import os
import select
import subprocess
import sys
import time

DEFAULT_TIMEOUT = 15.0


class DapClient:
    """Talks DAP (Content-Length framing) to a child process over stdio."""

    def __init__(self, argv):
        self.proc = subprocess.Popen(
            argv,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        self._buf = b""
        self._seq = 0

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

    def read_message(self, timeout: float = DEFAULT_TIMEOUT) -> dict:
        deadline = time.monotonic() + timeout
        while True:
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
            self.proc.stdin.close()

    def wait_exit(self, timeout: float = DEFAULT_TIMEOUT) -> int:
        return self.proc.wait(timeout=timeout)

    def dump_stderr(self) -> str:
        try:
            return self.proc.stderr.read().decode("utf-8", "replace")
        except Exception:
            return "<unreadable>"

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
        seq = client.send(
            "initialize",
            {
                "clientID": "dap-smoke",
                "clientName": "dap_smoke.py",
                "adapterID": "xcode",
                "pathFormat": "path",
                "linesStartAt1": True,
                "columnsStartAt1": True,
                "supportsRunInTerminalRequest": False,
            },
        )
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


def snapshot_pids(name: str) -> set:
    """Pids of processes whose command basename is `name`."""
    out = subprocess.run(
        ["ps", "-axo", "pid=,comm="], capture_output=True, text=True
    ).stdout
    pids = set()
    for line in out.splitlines():
        parts = line.strip().split(None, 1)
        if len(parts) == 2 and os.path.basename(parts[1]) == name:
            try:
                pids.add(int(parts[0]))
            except ValueError:
                pass
    return pids


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

    timeout = args.timeout or (60.0 if mock else 1800.0)
    argv = [binary] + (["--mock-pipeline"] if mock else [])
    print(f"session{' (mock)' if mock else ''}: {' '.join(argv)}")

    pre_lldb = snapshot_pids("lldb-dap")
    pre_xcb = snapshot_pids("xcodebuild")

    client = DapClient(argv)
    rec = Recorder(client)
    try:
        # 1. initialize
        seq = client.send(
            "initialize",
            {
                "clientID": "dap-smoke",
                "clientName": "dap_smoke.py",
                "adapterID": "xcode",
                "pathFormat": "path",
                "linesStartAt1": True,
                "columnsStartAt1": True,
                "supportsRunInTerminalRequest": False,
            },
        )
        resp = rec.response(seq, DEFAULT_TIMEOUT)
        check(resp.get("success") is True, "initialize response success", client)

        # 2. launch (config = flattened scenario config; the mock ignores it)
        if mock:
            config = {"workspace": "/nonexistent.xcworkspace", "scheme": "Mock"}
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
        disc_seq = client.send("disconnect", {"terminateDebuggee": True})
        resp = rec.response(disc_seq, DEFAULT_TIMEOUT)
        check(resp.get("command") == "disconnect", "disconnect response received", client)
        check(resp.get("success") is True, "disconnect response success", client)
        client.close_stdin()
        code = client.wait_exit()
        check(code == 0, f"clean exit 0 (got {code})", client)

        # 9. no zombies: dummy dead, no new lldb-dap / xcodebuild left.
        deadline = time.monotonic() + 5.0
        while time.monotonic() < deadline:
            new_lldb = snapshot_pids("lldb-dap") - pre_lldb
            new_xcb = snapshot_pids("xcodebuild") - pre_xcb
            dummy_dead = dummy_pid is None or not pid_alive(dummy_pid)
            if not new_lldb and not new_xcb and dummy_dead:
                break
            time.sleep(0.25)
        check(not new_lldb, f"no zombie lldb-dap (left: {sorted(new_lldb)})", client)
        check(not new_xcb, f"no zombie xcodebuild (left: {sorted(new_xcb)})", client)
        if dummy_pid is not None:
            check(not pid_alive(dummy_pid), f"dummy app (pid {dummy_pid}) terminated",
                  client)
    except (TimeoutError, EOFError, subprocess.TimeoutExpired) as e:
        print(f"  FAIL: {e}", file=sys.stderr)
        client.kill()
        print("--- xcode-dap stderr ---", file=sys.stderr)
        print(client.dump_stderr(), file=sys.stderr)
        return 1

    print(f"session{' (mock)' if mock else ''}: PASS")
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

    args = parser.parse_args()
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
