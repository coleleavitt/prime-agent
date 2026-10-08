from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from git_isolation import scrub_repository_selection
from test_repl import ReplProcess

SRC_DIR = str(Path(__file__).resolve().parent.parent / "src")

# Each case runs in a fresh interpreter: an audit hook can never be removed.
PREAMBLE = """
import os, subprocess, sys, tempfile
sys.path.insert(0, {src!r})
from rlm import plan_guard
from rlm.plan_guard import PlanModeError

# The checkout (and every temp dir) may itself live under /tmp, a default
# writable root: pin the defaults to one scratch dir so `work` is guarded.
scratch = os.path.realpath(tempfile.mkdtemp())
work = os.path.realpath(tempfile.mkdtemp())
plan_guard._default_writable_roots = lambda: {{scratch, "/dev"}}

TOKEN = "host-token"
control = plan_guard.claim_host_controller()
control(TOKEN, False, [])

def enable(extra=()):
    control(TOKEN, True, list(extra))

def disable():
    control(TOKEN, False, [])

def expect_blocked(fn):
    try:
        fn()
    except PlanModeError:
        return
    raise AssertionError("expected PlanModeError")
"""


def run_guarded(body: str) -> subprocess.CompletedProcess[str]:
    script = PREAMBLE.format(src=SRC_DIR) + body
    # The scripts run git under the guard (`git version`, and commits that must be refused): an
    # inherited GIT_DIR must not give a regressed guard a real repository to write to.
    env = dict(os.environ)
    scrub_repository_selection(env)
    return subprocess.run([sys.executable, "-c", script], capture_output=True, text=True, timeout=120, env=env)


class PlanGuardTest(unittest.TestCase):
    def assert_ok(self, body: str) -> None:
        result = run_guarded(body)
        self.assertEqual(result.returncode, 0, msg=f"stdout={result.stdout}\nstderr={result.stderr}")

    def test_writes_blocked_outside_writable_roots(self) -> None:
        self.assert_ok("""
target = os.path.join(work, "f.txt")
open(target, "w").write("before")
enable()
expect_blocked(lambda: open(target, "w"))
expect_blocked(lambda: open(target, "a"))
expect_blocked(lambda: os.open(target, os.O_WRONLY))
expect_blocked(lambda: os.remove(target))
expect_blocked(lambda: os.rename(target, target + ".bak"))
expect_blocked(lambda: os.mkdir(os.path.join(work, "d")))
import pathlib, shutil
expect_blocked(lambda: pathlib.Path(target).write_text("x"))
expect_blocked(lambda: shutil.copyfile(target, os.path.join(work, "copy.txt")))
assert open(target).read() == "before"
""")

    def test_reads_and_temp_writes_allowed(self) -> None:
        self.assert_ok("""
target = os.path.join(work, "f.txt")
open(target, "w").write("data")
enable()
assert open(target).read() == "data"
note = os.path.join(scratch, "plan_guard_scratch.txt")
open(note, "w").write("ok")
import shutil
shutil.copyfile(target, note)
os.remove(note)
open(os.devnull, "w").write("x")
""")

    def test_symlink_in_temp_cannot_reach_a_guarded_file(self) -> None:
        self.assert_ok("""
target = os.path.join(work, "f.txt")
open(target, "w").write("before")
link = os.path.join(scratch, "link")
os.symlink(target, link)
enable()
expect_blocked(lambda: open(link, "w"))
assert open(target).read() == "before"
""")

    def test_extra_roots_and_git_carveout(self) -> None:
        self.assert_ok("""
os.makedirs(os.path.join(work, ".git"))
enable([work])
open(os.path.join(work, "notes.txt"), "w").write("ok")
expect_blocked(lambda: open(os.path.join(work, ".git", "index"), "w"))
""")

    def test_protected_root_inside_a_writable_root_stays_guarded(self) -> None:
        # A checkout under a temp dir: the host protects the workspace, and
        # a deeper writable root inside it is writable again.
        self.assert_ok("""
repo = os.path.join(scratch, "repo")
build = os.path.join(repo, "build")
os.makedirs(build)
control(TOKEN, True, [build], [repo])
expect_blocked(lambda: open(os.path.join(repo, "main.py"), "w"))
open(os.path.join(build, "out.txt"), "w").write("ok")
open(os.path.join(scratch, "note.txt"), "w").write("ok")
""")

    def test_disable_restores_writes(self) -> None:
        self.assert_ok("""
target = os.path.join(work, "f.txt")
enable()
expect_blocked(lambda: open(target, "w"))
disable()
open(target, "w").write("ok")
""")

    def test_kernel_code_cannot_switch_the_guard_off(self) -> None:
        self.assert_ok("""
target = os.path.join(work, "f.txt")
enable()
# A second controller cannot be claimed.
try:
    plan_guard.claim_host_controller()
except PermissionError:
    pass
else:
    raise AssertionError("expected PermissionError")
# The bound token cannot be rebound.
try:
    control("guess", False, [])
except PermissionError:
    pass
else:
    raise AssertionError("expected PermissionError")
assert plan_guard.is_enabled()
expect_blocked(lambda: open(target, "w"))
""")

    def test_every_spawn_is_blocked(self) -> None:
        # The guard is the fallback for a machine with no OS sandbox: nothing
        # can run a command read-only, so no command runs.
        self.assert_ok("""
enable()
expect_blocked(lambda: os.system("true"))
expect_blocked(lambda: os.posix_spawn("/bin/true", ["/bin/true"], os.environ))
expect_blocked(lambda: os.fork())
expect_blocked(lambda: subprocess.run(["ls", "/"]))
expect_blocked(lambda: subprocess.run("echo hi", shell=True))
disable()
assert subprocess.run(["true"]).returncode == 0
""")

    def test_runtime_bash_is_refused_before_any_process_exists(self) -> None:
        self.assert_ok("""
import asyncio
from rlm.bash import bash
enable()
async def main():
    target = os.path.join(work, "f")
    try:
        await bash("touch " + target)
    except PlanModeError:
        pass
    else:
        raise AssertionError("expected PlanModeError")
    assert not os.path.exists(target)
asyncio.run(main())
""")


class PlanGuardFrameTest(unittest.TestCase):
    """The host-only `plan_guard` protocol frame."""

    def setUp(self) -> None:
        self.repl = ReplProcess()
        self.addCleanup(self.repl.close)
        self.repl.ready()
        self.work = os.path.realpath(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.work, True)
        # Repository metadata stays guarded even under a default (temp) root.
        os.makedirs(os.path.join(self.work, ".git"))

    def guard(self, rid: str, token: str, enabled: bool) -> dict:
        self.repl.send({"type": "plan_guard", "id": rid, "token": token, "enabled": enabled, "writable_roots": []})
        events = self.repl.until_done(rid)
        return events[-1]

    def test_frame_toggles_the_guard_and_cells_cannot(self) -> None:
        self.assertEqual(
            self.guard("g1", "host-token", True), {"event": "done", "id": "g1", "status": "ok", "enabled": True}
        )
        target = os.path.join(self.work, ".git", "f.txt")
        events = self.repl.execute(
            "c1",
            "from rlm import plan_guard\n"
            f"target = {target!r}\n"
            "try:\n"
            "    open(target, 'w')\n"
            "except plan_guard.PlanModeError:\n"
            "    print('blocked')\n"
            "try:\n"
            "    plan_guard.claim_host_controller()\n"
            "except PermissionError:\n"
            "    print('no controller')\n"
            "print(plan_guard.is_enabled())\n",
        )
        out = "".join(e["text"] for e in events if e.get("event") == "stdout")
        self.assertEqual(out, "blocked\nno controller\nTrue\n")
        self.assertFalse(os.path.exists(target))
        # A wrong token cannot switch it off.
        done = self.guard("g2", "guess", False)
        self.assertEqual(done["status"], "error")
        self.assertIn("PermissionError", done["reason"])
        self.assertEqual(
            self.guard("g3", "host-token", False), {"event": "done", "id": "g3", "status": "ok", "enabled": False}
        )
        events = self.repl.execute("c2", f"open({target!r}, 'w').write('ok')")
        self.assertEqual(events[-1]["status"], "ok")
        self.assertTrue(os.path.exists(target))

    def test_frame_is_answered_while_a_cell_runs(self) -> None:
        self.repl.send({"type": "execute", "id": "slow", "code": "import time\ntime.sleep(2)"})
        done = self.guard("g1", "host-token", True)
        self.assertEqual(done, {"event": "done", "id": "g1", "status": "ok", "enabled": True})
        self.repl.until_done("slow")

    def test_malformed_frame_is_refused(self) -> None:
        self.repl.send({"type": "plan_guard", "id": "g1", "token": "t", "enabled": "yes"})
        done = self.repl.until_done("g1")[-1]
        self.assertEqual(
            done, {"event": "done", "id": "g1", "status": "error", "reason": "plan_guard enabled must be a boolean"}
        )


if __name__ == "__main__":
    unittest.main()
