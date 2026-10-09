#!/usr/bin/env python3
"""Smoke test for scripts/release.sh.

Runs the release script in a throwaway git repository that holds a copy of
the files it reads (manifests, lockfiles, crate and extension sources, the
scripts), so the checkout is never touched. The copy's scripts/gate.sh is a
stub that records that it ran and fails on request: the real gate is what
runs this test.

Covered: version validation; the dry run (shows all seven changes, writes
nothing, skips the gate); the refusals (an existing tag, a lower version,
unstaged edits besides the version in a file the script stages, including
next to a version set by hand); a run whose gate fails (changes left
unstaged), the re-run after it (refused while another edit is unstaged, then
writes nothing new, runs the gate, stages exactly the seven files, never
commits or tags, and ignores a GIT_DIR, GIT_WORK_TREE or GIT_INDEX_FILE it
inherits); a re-run once staged; and the note about staged edits besides
the version.

On macOS the script runs under /bin/bash 3.2 and the BSD tools, which is
what this test is there to keep working. It needs git, and cargo with the
crates of both lockfiles in the local cache (the script refreshes the
lockfiles offline); the gate's earlier builds put them there.

Usage:
  python3 tests/release_smoke.py
"""

import functools
import hashlib
import os
import re
import shutil
import subprocess
import sys
import tempfile

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

# What the release script rewrites and stages.
DECLARATIONS = [
    "crates/xcode-dap/Cargo.toml",
    "crates/xcode-dap-config/Cargo.toml",
    "extension/Cargo.toml",
    "extension/extension.toml",
    "extension/src/lib.rs",
]
LOCKFILES = ["Cargo.lock", "extension/Cargo.lock"]
SEVEN = DECLARATIONS + LOCKFILES

# Copied into the throwaway repository (tracked and untracked files, never
# ignored build output).
COPY_PATHS = [
    ".gitignore",
    "CHANGELOG.md",
    "Cargo.toml",
    "Cargo.lock",
    "crates",
    "extension",
    "scripts",
]

# git without the user's or the system's configuration (signing, hooks,
# templates), with a neutral identity for the one setup commit.
GIT_ENV = {
    "GIT_CONFIG_NOSYSTEM": "1",
    "GIT_CONFIG_GLOBAL": os.devnull,
    "GIT_AUTHOR_NAME": "Release Smoke",
    "GIT_AUTHOR_EMAIL": "release-smoke@example.com",
    "GIT_COMMITTER_NAME": "Release Smoke",
    "GIT_COMMITTER_EMAIL": "release-smoke@example.com",
}


def check(cond: bool, what: str, detail: str = "") -> None:
    if cond:
        print("  ok: %s" % what)
        return
    print("  FAIL: %s" % what, file=sys.stderr)
    if detail:
        print(detail, file=sys.stderr)
    sys.exit(1)


@functools.lru_cache(maxsize=None)
def local_git_vars() -> frozenset:
    """The variables that point git at a repository, an index or extra
    configuration, as git itself lists them."""
    out = subprocess.run(
        ["git", "rev-parse", "--local-env-vars"], check=True, capture_output=True, text=True
    )
    return frozenset(out.stdout.split())


def env(**extra: str) -> dict:
    # Git exports GIT_DIR and GIT_INDEX_FILE to hooks, so the gate (and this
    # test) can run with them set; inherited, they would point every git
    # command here at the real checkout instead of the throwaway repository.
    e = {k: v for k, v in os.environ.items() if k not in local_git_vars()}
    e.update(GIT_ENV)
    e.update(extra)
    return e


def git(cwd: str, *args: str) -> str:
    out = subprocess.run(
        ["git", "-c", "init.defaultBranch=main", "-c", "commit.gpgsign=false", *args],
        cwd=cwd,
        env=env(),
        check=True,
        capture_output=True,
        text=True,
    )
    return out.stdout


def release(repo: str, *args: str, extra_env: dict = None) -> subprocess.CompletedProcess:
    return subprocess.run(
        [os.path.join(repo, "scripts", "release.sh"), *args],
        cwd=repo,
        env=env(**(extra_env or {})),
        stdin=subprocess.DEVNULL,
        capture_output=True,
        text=True,
    )


def output(result: subprocess.CompletedProcess) -> str:
    return "--- exit %d\n--- stdout\n%s--- stderr\n%s" % (
        result.returncode,
        result.stdout,
        result.stderr,
    )


def package_version(path: str) -> str:
    in_package = False
    with open(path, encoding="utf-8") as f:
        for line in f:
            stripped = line.strip()
            if stripped.startswith("["):
                in_package = stripped == "[package]"
                continue
            m = re.match(r'version\s*=\s*"([^"]*)"', stripped)
            if in_package and m:
                return m.group(1)
    raise SystemExit("no [package] version in %s" % path)


def make_copy(repo: str, gate_marker: str, gate_fail: str) -> None:
    listed = subprocess.run(
        ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard", "--", *COPY_PATHS],
        cwd=REPO,
        check=True,
        capture_output=True,
    ).stdout
    for rel in sorted(set(p.decode() for p in listed.split(b"\0") if p)):
        src = os.path.join(REPO, rel)
        if not os.path.lexists(src):
            continue  # deleted from the working tree, still in the index
        dst = os.path.join(repo, rel)
        os.makedirs(os.path.dirname(dst), exist_ok=True)
        if os.path.islink(src):
            os.symlink(os.readlink(src), dst)
        else:
            shutil.copy2(src, dst)
    # The stub gate writes its marker outside the repository, so its run
    # leaves no file behind that git would see, and fails while gate_fail
    # exists.
    gate = os.path.join(repo, "scripts", "gate.sh")
    with open(gate, "w", encoding="utf-8") as f:
        f.write("#!/bin/sh\necho ran >'%s'\ntest ! -e '%s'\n" % (gate_marker, gate_fail))
    os.chmod(gate, 0o755)


def snapshot(repo: str) -> dict:
    """Every file below repo except .git: content hash and mode."""
    files = {}
    for root, dirs, names in os.walk(repo):
        dirs[:] = [d for d in dirs if d != ".git"]
        for name in names:
            path = os.path.join(root, name)
            rel = os.path.relpath(path, repo)
            if os.path.islink(path):
                files[rel] = ("link", os.readlink(path))
                continue
            with open(path, "rb") as f:
                digest = hashlib.sha256(f.read()).hexdigest()
            files[rel] = (digest, os.stat(path).st_mode)
    return files


def state(repo: str) -> tuple:
    return (
        snapshot(repo),
        git(repo, "status", "--porcelain=v1", "--untracked-files=all"),
        git(repo, "rev-parse", "HEAD"),
        git(repo, "tag", "-l"),
    )


def read(path: str) -> str:
    with open(path, encoding="utf-8") as f:
        return f.read()


def write(path: str, text: str) -> None:
    with open(path, "w", encoding="utf-8") as f:
        f.write(text)


def main() -> int:
    current = package_version(os.path.join(REPO, "crates/xcode-dap/Cargo.toml"))
    major, minor, _patch = (int(part) for part in current.split("."))
    version = "%d.%d.0" % (major, minor + 1)
    tag = "xcode-dap-v" + version
    print("release smoke: %s -> %s" % (current, version))

    with tempfile.TemporaryDirectory(prefix="zedx-release-smoke.") as base:
        repo = os.path.join(base, "repo")
        marker = os.path.join(base, "gate-ran")
        fail = os.path.join(base, "gate-fails")
        os.makedirs(repo)
        make_copy(repo, marker, fail)
        git(repo, "init", "-q")
        git(repo, "add", "-A")
        git(repo, "commit", "-q", "-m", "Fixture")
        clean = state(repo)

        print("arguments")
        for args in ([], ["--dry-run"], [version, version], [version, "--force"]):
            r = release(repo, *args)
            check(r.returncode == 2 and "usage:" in r.stderr, "usage error for %r" % args, output(r))
        for bad in ("1.2", "1.2.3.4", "01.2.3", "1.02.3", "1.2.03", "v1.2.3", tag, "1.2.3-rc.1", "1.2.x"):
            r = release(repo, bad)
            check(r.returncode == 2 and "is not a version" in r.stderr, "rejects %r" % bad, output(r))
        check(state(repo) == clean, "rejected arguments write nothing")

        print("dry run")
        r = release(repo, version, "--dry-run")
        check(r.returncode == 0, "exits 0", output(r))
        for f in SEVEN:
            check("+++ b/%s" % f in r.stdout, "shows the change to %s" % f, output(r))
        check(
            ("versions: all five declarations are %s" % version) in r.stdout,
            "check-versions passes in the temp copy",
            output(r),
        )
        check(state(repo) == clean, "writes nothing", output(r))
        check(not os.path.exists(marker), "does not run the gate")

        print("refusals")
        git(repo, "tag", tag)
        r = release(repo, version)
        check(r.returncode == 1 and "already exists" in r.stderr, "an existing tag", output(r))
        git(repo, "tag", "-d", tag)
        if current != "0.0.0":
            r = release(repo, "0.0.0")
            check(r.returncode == 2 and "lower than" in r.stderr, "a lower version", output(r))
        lib = os.path.join(repo, "extension/src/lib.rs")
        toml = os.path.join(repo, "extension/extension.toml")
        hand_set = re.sub(r'(?m)^version = "[^"]*"', 'version = "%s"' % version, read(toml), count=1)
        hand_set = re.sub(r'(?m)^description = "', 'description = "Edited. ', hand_set, count=1)
        for path, edited, what in (
            (lib, read(lib) + "// an unstaged edit\n", "an unstaged edit in a file it would change"),
            (toml, hand_set, "an unstaged edit next to a version already set by hand"),
        ):
            original = read(path)
            write(path, edited)
            dirty = state(repo)
            r = release(repo, version)
            rel = os.path.relpath(path, repo)
            check(
                r.returncode == 1 and "unstaged changes" in r.stderr and rel in r.stderr,
                what,
                output(r),
            )
            check(state(repo) == dirty, "  writes nothing")
            write(path, original)
        check(state(repo) == clean, "(test) the edits are undone")
        check(not os.path.exists(marker), "the refusals do not run the gate")

        print("release, the gate fails")
        write(fail, "")
        r = release(repo, version)
        check(r.returncode == 1 and "the gate failed" in r.stderr, "exits 1", output(r))
        check(os.path.exists(marker), "runs the gate")
        check(git(repo, "diff", "--cached", "--name-only") == "", "stages nothing")
        unstaged = sorted(git(repo, "diff", "--name-only").split())
        check(unstaged == sorted(SEVEN), "leaves the seven changes unstaged", "unstaged: %s" % unstaged)
        os.remove(fail)
        os.remove(marker)

        print("re-run after the failed gate")
        bumped = read(lib)
        write(lib, bumped + "// an unstaged fix\n")
        r = release(repo, version)
        check(
            r.returncode == 1 and "unstaged changes" in r.stderr and "extension/src/lib.rs" in r.stderr,
            "refuses while an edit besides the version is unstaged",
            output(r),
        )
        check(not os.path.exists(marker), "  does not run the gate")
        write(lib, bumped)
        # A repository that inherited variables point at; the script must
        # work on its own checkout and leave this one alone.
        decoy = os.path.join(base, "decoy")
        os.makedirs(decoy)
        write(os.path.join(decoy, "README"), "decoy\n")
        git(decoy, "init", "-q")
        git(decoy, "add", "-A")
        git(decoy, "commit", "-q", "-m", "Decoy")
        decoy_clean = state(decoy)
        decoy_env = {
            "GIT_DIR": os.path.join(decoy, ".git"),
            "GIT_WORK_TREE": decoy,
            "GIT_INDEX_FILE": os.path.join(decoy, ".git", "index"),
        }
        r = release(repo, version, extra_env=decoy_env)
        check(r.returncode == 0 and "already record" in r.stdout, "exits 0, nothing new to change", output(r))
        check(state(decoy) == decoy_clean, "ignores inherited GIT_DIR, GIT_WORK_TREE and GIT_INDEX_FILE")
        check(os.path.exists(marker), "runs the gate")
        staged = sorted(git(repo, "diff", "--cached", "--name-only").split())
        check(staged == sorted(SEVEN), "stages exactly the seven files", "staged: %s" % staged)
        check(git(repo, "diff", "--name-only") == "", "leaves nothing unstaged")
        numstat = git(repo, "diff", "--cached", "--numstat")
        changed_lines = {}
        for line in numstat.splitlines():
            added, removed, path = line.split("\t")
            changed_lines[path] = (int(added), int(removed))
        for f in DECLARATIONS:
            check(changed_lines.get(f) == (1, 1), "one line changed in %s" % f, numstat)
        for f in LOCKFILES:
            check(changed_lines.get(f) == (2, 2), "two version lines changed in %s" % f, numstat)
        versions = subprocess.run(
            [os.path.join(repo, "scripts", "check-versions.sh"), version],
            cwd=repo,
            env=env(),
            capture_output=True,
            text=True,
        )
        check(versions.returncode == 0, "check-versions.sh %s passes" % version, output(versions))
        check(git(repo, "rev-parse", "HEAD") == clean[2], "does not commit")
        check(git(repo, "tag", "-l") == "", "does not tag")
        check('git commit -m "Release %s"' % version in r.stdout, "prints the commit title", output(r))
        check(
            ('git tag -a %s -m "Release %s"' % (tag, version)) in r.stdout
            and ("git push origin %s" % tag) in r.stdout,
            "prints the tag commands",
            output(r),
        )
        released = state(repo)

        print("re-run with the same version")
        os.remove(marker)
        r = release(repo, version)
        check(r.returncode == 0 and "already record" in r.stdout, "exits 0, nothing to change", output(r))
        check(os.path.exists(marker), "runs the gate again")
        check(state(repo) == released, "leaves the staged release as it was")

        print("staged edits besides the version")
        write(lib, read(lib) + "// a staged edit\n")
        git(repo, "add", "--", "extension/src/lib.rs")
        r = release(repo, version)
        notes = [
            line.split("too:", 1)[1].split()
            for line in r.stdout.splitlines()
            if line.startswith("note: these files also have staged edits besides the version")
        ]
        check(
            r.returncode == 0 and notes == [["extension/src/lib.rs"]],
            "exits 0 and names the file",
            output(r),
        )

    print("release smoke: PASS")
    return 0


if __name__ == "__main__":
    sys.exit(main())
