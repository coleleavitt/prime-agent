from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

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

def no_sandbox():
    import rlm.plan_guard as pg
    pg._sandbox_prefix = lambda roots: None
"""


def run_guarded(body: str) -> subprocess.CompletedProcess[str]:
    script = PREAMBLE.format(src=SRC_DIR) + body
    return subprocess.run([sys.executable, "-c", script], capture_output=True, text=True, timeout=120)


def bwrap_usable() -> bool:
    if sys.platform != "linux" or shutil.which("bwrap") is None:
        return False
    probe = subprocess.run(
        ["bwrap", "--ro-bind", "/", "/", "--dev-bind", "/dev", "/dev", "--", "true"],
        capture_output=True,
        timeout=30,
    )
    return probe.returncode == 0


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

    def test_sandboxed_subprocess_respects_protected_roots(self) -> None:
        if not bwrap_usable():
            self.skipTest("bwrap is not usable here")
        self.assert_ok("""
repo = os.path.join(scratch, "repo")
os.makedirs(repo)
control(TOKEN, True, [], [repo])
out = subprocess.run("echo x > " + os.path.join(repo, "f"), shell=True, capture_output=True, text=True)
assert out.returncode != 0, "a write into the protected workspace must fail"
assert not os.path.exists(os.path.join(repo, "f"))
out = subprocess.run("echo x > " + os.path.join(scratch, "f"), shell=True, capture_output=True, text=True)
assert out.returncode == 0, out.stderr
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

    def test_direct_spawns_blocked(self) -> None:
        self.assert_ok("""
enable()
expect_blocked(lambda: os.system("true"))
expect_blocked(lambda: os.posix_spawn("/bin/true", ["/bin/true"], os.environ))
expect_blocked(lambda: os.fork())
""")

    def test_subprocess_read_only_command_runs(self) -> None:
        self.assert_ok("""
enable()
out = subprocess.run(["ls", "/"], capture_output=True, text=True)
assert out.returncode == 0, out.stderr
assert "etc" in out.stdout or "Users" in out.stdout
out = subprocess.run("echo hello | tr a-z A-Z", shell=True, capture_output=True, text=True)
assert out.returncode == 0, out.stderr
assert out.stdout.strip() == "HELLO"
""")

    def test_sandboxed_subprocess_write_fails(self) -> None:
        if not bwrap_usable():
            self.skipTest("bwrap is not usable here")
        self.assert_ok("""
target = os.path.join(work, "f.txt")
enable()
out = subprocess.run("echo x > " + target, shell=True, capture_output=True, text=True)
assert out.returncode != 0, "sandboxed shell write should fail"
assert not os.path.exists(target)
note = os.path.join(scratch, "ok.txt")
out = subprocess.run("echo x > " + note, shell=True, capture_output=True, text=True)
assert out.returncode == 0, out.stderr
""")

    def test_fallback_allowlist(self) -> None:
        self.assert_ok("""
no_sandbox()
enable()
out = subprocess.run(["git", "version"], capture_output=True, text=True)
assert out.returncode == 0
expect_blocked(lambda: subprocess.run(["git", "-c", "core.fsmonitor=touch pwned", "status"]))
expect_blocked(lambda: subprocess.run(["sed", "-i", "s/a/b/", "f"]))
expect_blocked(lambda: subprocess.run(["sort", "-o", "out", "in"]))
expect_blocked(lambda: subprocess.run(["touch", "f"]))
expect_blocked(lambda: subprocess.run("echo hi > f", shell=True))
expect_blocked(lambda: subprocess.run("ls; touch f", shell=True))
expect_blocked(lambda: subprocess.run("ls $(touch f)", shell=True))
expect_blocked(lambda: subprocess.run(["git", "commit", "-m", "x"]))
out = subprocess.run(["bash", "-c", "git version | head -1"], capture_output=True, text=True)
assert out.returncode == 0 and "git" in out.stdout, out.stderr
expect_blocked(lambda: subprocess.run(["bash", "-c", "touch " + os.path.join(work, "f")]))
""")

    def test_runtime_bash_runs_read_only_commands_without_a_sandbox(self) -> None:
        self.assert_ok("""
no_sandbox()
import asyncio
from rlm.bash import bash
enable()
async def main():
    result = await bash("echo plan | tr a-z A-Z")
    assert result.exit_code == 0, result.output
    assert result.output.strip() == "PLAN", result.output
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

    def test_wrapper_check_refuses_a_forged_inner_script(self) -> None:
        self.assert_ok("""
no_sandbox()
enable()
target = os.path.join(work, "f")
# A Popen naming a harmless "inner" script cannot smuggle a different one.
expect_blocked(
    lambda: subprocess.Popen(["sh", "-c", "touch " + target], _plan_guard_inner="ls").wait()
)
assert not os.path.exists(target)
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
