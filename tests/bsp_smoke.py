#!/usr/bin/env python3
"""BSP-level smoke harness for `xcode-dap bsp`.

A minimal scripted JSON-RPC client (Content-Length framing, same wire as DAP)
that spawns `xcode-dap bsp` and asserts the Build Server dialect sourcekit-lsp
speaks: initialize handshake, dummy targets, root-dir sources, per-file
sourceKitOptions (hit + miss), and a clean shutdown / stdin-EOF exit.

Hermetic: HOME is redirected to a temp dir (so the store cache + log file are
throwaway), the project root + build root are temp dirs, and the compile store
is pre-seeded directly in schema v1 so the run needs no real build logs.

Usage (--binary belongs to the top-level parser, before the subcommand):
  python3 tests/bsp_smoke.py [--binary target/debug/xcode-dap]
"""

import argparse
import json
import os
import select
import shutil
import subprocess
import sys
import tempfile
import time

DEFAULT_TIMEOUT = 15.0
SCHEME = "MyApp"  # no spaces/parens -> sanitized filename == scheme


# --- store-path derivation (must mirror engine/compile_store.rs) ------------


def fnv1a64(data: bytes) -> int:
    h = 0xCBF29CE484222325
    for b in data:
        h ^= b
        h = (h * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return h


def sanitize_scheme(s: str) -> str:
    out = "".join(
        c if (c.isascii() and (c.isalnum() or c in "._-")) else "-" for c in s
    )
    return out or "scheme"


def store_path(home: str, build_root: str, scheme: str) -> str:
    root_hash = fnv1a64(build_root.encode())
    scheme_hash = fnv1a64(scheme.encode())
    name = "compile-store-%016x-%016x-%s.json" % (
        root_hash,
        scheme_hash,
        sanitize_scheme(scheme),
    )
    return os.path.join(home, ".zedxcode", "cache", name)


# --- JSON-RPC client over Content-Length framing ----------------------------


class RpcClient:
    def __init__(self, argv, env):
        self.proc = subprocess.Popen(
            argv,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            env=env,
        )
        self._buf = b""
        self._id = 0

    def _write(self, obj) -> None:
        body = json.dumps(obj).encode()
        frame = b"Content-Length: %d\r\n\r\n%s" % (len(body), body)
        self.proc.stdin.write(frame)
        self.proc.stdin.flush()

    def request(self, method: str, params=None):
        self._id += 1
        rid = self._id
        msg = {"jsonrpc": "2.0", "id": rid, "method": method}
        if params is not None:
            msg["params"] = params
        self._write(msg)
        return self._wait_response(rid)

    def notify(self, method: str, params=None) -> None:
        msg = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            msg["params"] = params
        self._write(msg)

    def _read_some(self, deadline: float) -> None:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("timed out waiting for JSON-RPC data")
        fd = self.proc.stdout.fileno()
        ready, _, _ = select.select([fd], [], [], remaining)
        if not ready:
            raise TimeoutError("timed out waiting for JSON-RPC data")
        chunk = os.read(fd, 65536)
        if not chunk:
            raise EOFError("xcode-dap bsp closed stdout")
        self._buf += chunk

    def _read_message(self, timeout: float) -> dict:
        deadline = time.monotonic() + timeout
        while True:
            end = self._buf.find(b"\r\n\r\n")
            if end != -1:
                header = self._buf[:end].decode("utf-8", "replace")
                length = None
                for line in header.split("\r\n"):
                    name, _, value = line.partition(":")
                    if name.strip().lower() == "content-length":
                        length = int(value.strip())
                if length is None:
                    raise AssertionError("header without Content-Length: %r" % header)
                total = end + 4 + length
                if len(self._buf) >= total:
                    body = self._buf[end + 4 : total]
                    self._buf = self._buf[total:]
                    return json.loads(body)
            self._read_some(deadline)

    def _wait_response(self, rid: int, timeout: float = DEFAULT_TIMEOUT) -> dict:
        """Read until the response for `rid`; skip server->client notifications
        (e.g. buildTarget/didChange, which carry no id)."""
        deadline = time.monotonic() + timeout
        while True:
            msg = self._read_message(max(0.1, deadline - time.monotonic()))
            if msg.get("id") == rid:
                return msg

    # teardown
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


def check(cond: bool, what: str, client: RpcClient) -> None:
    if cond:
        print("  ok: %s" % what)
        return
    print("  FAIL: %s" % what, file=sys.stderr)
    client.kill()
    print("--- xcode-dap bsp stderr ---", file=sys.stderr)
    print(client.dump_stderr(), file=sys.stderr)
    sys.exit(1)


# --- fixture ----------------------------------------------------------------


def build_fixture(base: str):
    """Lay out a temp HOME, project root (+ a real seeded .swift file), and a
    build root; pre-seed the compile store. All paths realpath-ed so the store
    keys (which realpath) match the URIs we send. Returns a dict of facts."""
    home = os.path.realpath(os.path.join(base, "home"))
    root = os.path.realpath(os.path.join(base, "project"))
    build_root = os.path.realpath(os.path.join(base, "build_root"))
    os.makedirs(os.path.join(home, ".zedxcode", "cache"), exist_ok=True)
    os.makedirs(os.path.join(root, "Sources"), exist_ok=True)
    os.makedirs(build_root, exist_ok=True)  # no Logs/ -> newest_log() is None

    swift_file = os.path.join(root, "Sources", "A.swift")
    with open(swift_file, "w") as fh:
        fh.write("public struct A {}\n")
    swift_file = os.path.realpath(swift_file)

    index_store = build_root + "/Index.noindex/DataStore"
    working_dir = os.path.join(root, "Modules", "AlphaKit")
    args = ["-module-name", "AlphaKit", "-DDEBUG"]

    # buildServer.json (our private fields build_root/scheme/workspace).
    with open(os.path.join(root, "buildServer.json"), "w") as fh:
        json.dump(
            {
                "name": "xcode build server",
                "version": "1.3.0",
                "bspVersion": "2.2.0",
                "languages": ["c", "cpp", "objective-c", "objective-cpp", "swift"],
                "argv": ["xcode-dap", "bsp"],
                "workspace": os.path.join(root, "MyApp.xcworkspace"),
                "build_root": build_root,
                "scheme": SCHEME,
                "kind": "xcode",
            },
            fh,
        )

    alpha_module = {
        "args": args,
        "working_dir": working_dir,
        "files": [swift_file],
        "file_lists": [],
        "index_store_path": index_store,
    }
    # Pre-seed the store (schema v1) so bootstrap is a warm no-op.
    seed = {
        "version": 1,
        "build_root": build_root,
        "scheme": SCHEME,
        "modules": {"AlphaKit": alpha_module},
    }
    with open(store_path(home, build_root, SCHEME), "w") as fh:
        json.dump(seed, fh)

    return {
        "home": home,
        "root": root,
        "root_uri": "file://" + root,
        "build_root": build_root,
        "index_store": index_store,
        "swift_file": swift_file,
        "swift_uri": "file://" + swift_file,
        "working_dir": working_dir,
        "args": args,
        "alpha_module": alpha_module,
        "store_file": store_path(home, build_root, SCHEME),
    }


def spawn(binary: str, fx: dict) -> RpcClient:
    env = dict(os.environ)
    env["HOME"] = fx["home"]
    env.pop("XCODE_DAP_LOG", None)
    return RpcClient([binary, "bsp"], env)


def initialize(client: RpcClient, fx: dict) -> dict:
    resp = client.request(
        "build/initialize",
        {
            "rootUri": fx["root_uri"],
            "displayName": "sourcekit-lsp",
            "version": "1.0",
            "bspVersion": "2.2.0",
            "capabilities": {"languageIds": ["swift"]},
        },
    )
    return resp


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--binary", default="target/debug/xcode-dap")
    args = ap.parse_args()
    binary = os.path.abspath(args.binary)
    if not os.path.exists(binary):
        print("binary not found: %s (run `cargo build` first)" % binary, file=sys.stderr)
        return 2

    print("bsp smoke: %s" % binary)
    base = tempfile.mkdtemp(prefix="zedxcode-bsp-smoke-")
    try:
        fx = build_fixture(base)

        # -- scenarios 1-6 on one live server --------------------------------
        client = spawn(binary, fx)

        # (1) initialize handshake
        resp = initialize(client, fx)
        result = resp.get("result") or {}
        check(result.get("displayName") == "xcode-dap", "displayName xcode-dap", client)
        check(result.get("bspVersion") == "2.2.0", "bspVersion 2.2.0", client)
        check(result.get("rootUri") == fx["root_uri"], "rootUri echoed", client)
        check(result.get("dataKind") == "sourceKit", "dataKind sourceKit", client)
        data = result.get("data") or {}
        check(data.get("sourceKitOptionsProvider") is True, "sourceKitOptionsProvider true", client)
        check(
            data.get("indexStorePath") == fx["index_store"],
            "indexStorePath derived from build_root",
            client,
        )
        check(
            isinstance(data.get("indexDatabasePath"), str)
            and os.path.isabs(data["indexDatabasePath"])
            and "index-db-" in data["indexDatabasePath"],
            "indexDatabasePath absolute + hashed",
            client,
        )
        client.notify("build/initialized")

        # (2) buildTargets dummy shape
        targets = (client.request("workspace/buildTargets").get("result") or {}).get("targets") or []
        check(len(targets) == 1, "one dummy target", client)
        t = targets[0]
        check(t.get("id", {}).get("uri") == "dummy://dummy", "target id dummy://dummy", client)
        check(t.get("displayName") == "BuildServer", "target displayName BuildServer", client)
        check(t.get("tags") == ["test"], "target tags [test]", client)

        # (3) sources = the project root as a directory (kind 2)
        items = (client.request("buildTarget/sources").get("result") or {}).get("items") or []
        check(len(items) == 1, "one sources item", client)
        srcs = items[0].get("sources") or []
        check(len(srcs) == 1, "one source entry", client)
        check(srcs[0].get("uri") == fx["root_uri"] + "/", "source uri = root dir", client)
        check(srcs[0].get("kind") == 2, "source kind 2 (directory)", client)

        # (4) sourceKitOptions for the seeded file -> expected args + wd
        opts = client.request(
            "textDocument/sourceKitOptions",
            {"textDocument": {"uri": fx["swift_uri"]}, "language": "swift"},
        ).get("result")
        check(opts is not None, "sourceKitOptions non-null for seeded file", client)
        check(opts.get("compilerArguments") == fx["args"], "compilerArguments match seed", client)
        check(opts.get("workingDirectory") == fx["working_dir"], "workingDirectory match seed", client)

        # (5) sourceKitOptions for an unknown file -> null
        miss_uri = "file://" + os.path.join(fx["root"], "Unrelated", "Zzz.swift")
        miss = client.request(
            "textDocument/sourceKitOptions",
            {"textDocument": {"uri": miss_uri}, "language": "swift"},
        ).get("result")
        check(miss is None, "sourceKitOptions null for unknown file", client)

        # (8) external store rewrite (the build pipeline ingesting a CLI build
        #     writes the same store file) -> bsp's mtime watch reloads it and
        #     serves the newly-added module without a restart.
        gamma_dir = os.path.join(fx["root"], "Gamma")
        os.makedirs(gamma_dir, exist_ok=True)
        gamma_file = os.path.realpath(os.path.join(gamma_dir, "G.swift"))
        with open(gamma_file, "w") as fh:
            fh.write("public struct G {}\n")
        gamma_args = ["-module-name", "GammaKit", "-DRELEASE"]
        updated = {
            "version": 1,
            "build_root": fx["build_root"],
            "scheme": SCHEME,
            "modules": {
                "AlphaKit": fx["alpha_module"],
                "GammaKit": {
                    "args": gamma_args,
                    "working_dir": os.path.join(fx["root"], "Modules", "GammaKit"),
                    "files": [gamma_file],
                    "file_lists": [],
                    "index_store_path": fx["index_store"],
                },
            },
        }
        time.sleep(1.1)  # guarantee a distinct store-file mtime
        with open(fx["store_file"], "w") as fh:
            json.dump(updated, fh)
        # Poll (~1 s) picks up the external write; retry until it serves Gamma.
        deadline = time.monotonic() + 8
        served = None
        while time.monotonic() < deadline:
            served = client.request(
                "textDocument/sourceKitOptions",
                {"textDocument": {"uri": "file://" + gamma_file}, "language": "swift"},
            ).get("result")
            if served is not None:
                break
            time.sleep(0.5)
        check(served is not None, "external store reload serves the new module", client)
        check(served.get("compilerArguments") == gamma_args, "reloaded module args", client)

        # (6) shutdown -> null result, exit -> clean process exit
        shut = client.request("build/shutdown")
        check(shut.get("result", "MISSING") is None, "shutdown result null", client)
        client.notify("build/exit")
        code = client.wait_exit(timeout=DEFAULT_TIMEOUT)
        check(code == 0, "clean exit 0 after build/exit", client)

        # (7) stdin EOF -> process exits (fresh server)
        client2 = spawn(binary, fx)
        initialize(client2, fx)
        client2.notify("build/initialized")
        client2.close_stdin()
        code2 = client2.wait_exit(timeout=DEFAULT_TIMEOUT)
        check(code2 == 0, "clean exit on stdin EOF", client2)

        print("bsp smoke: PASS")
        return 0
    finally:
        shutil.rmtree(base, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
