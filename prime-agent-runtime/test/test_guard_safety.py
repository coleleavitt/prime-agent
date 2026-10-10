"""The guard suites' safety rails hold: a refusal-expecting case never runs its
command, and what the suites run is confined to a private temporary root."""

from __future__ import annotations

import asyncio
import os
import sys
import tempfile
import unittest
from pathlib import Path

import guard_safety

# A directory outside the confined root, made before confining.
_OUTSIDE = Path(tempfile.mkdtemp(prefix="pa-guard-safety-outside-"))
_ROOT = guard_safety.confine()

from rlm import bash  # noqa: E402
from rlm.bash import PrivilegeEscalationRefusalError  # noqa: E402


class GuardSafetyTest(guard_safety.RefusalSafe, unittest.IsolatedAsyncioTestCase):
    async def test_an_allowed_command_never_runs_when_a_refusal_is_expected(self):
        work = Path(tempfile.mkdtemp())
        marker = work / "ran"
        with self.assertRaises(AssertionError) as caught:
            with self.assertRaises(PrivilegeEscalationRefusalError):
                bash(f"touch {marker}")
        self.assertIn("it was not run", str(caught.exception))
        self.assertFalse(marker.exists())
        with self.assertRaises(AssertionError):
            with guard_safety.refusal_expected():
                bash(f"touch {marker}")
        self.assertFalse(marker.exists())

    async def test_a_refusal_still_raises_inside_the_rail(self):
        with self.assertRaises(PrivilegeEscalationRefusalError):
            bash("sudo id")

    @unittest.skipIf(_ROOT is None, "no OS sandbox on this machine")
    async def test_commands_run_confined_to_the_temporary_root(self):
        inside = Path(tempfile.mkdtemp()) / "inside"
        outside = _OUTSIDE / "outside"
        result = await asyncio.wait_for(bash(f"touch {inside}; touch {outside}; echo done"), 30)
        self.assertIn("done", result.output)
        # The write outside the root fails with EACCES; the one inside works.
        self.assertIn("Permission denied", result.output)
        self.assertTrue(inside.exists())
        self.assertFalse(outside.exists())
        self.assertTrue(str(inside).startswith(str(_ROOT)))
        os.rmdir(_OUTSIDE)

    @unittest.skipIf(not guard_safety.confined(), "no OS sandbox on this machine")
    def test_the_host_itself_runs_confined(self):
        # The host re-executed itself under the sandbox, so the guards' own
        # probes are confined too, not only the commands it runs.
        bash_module = sys.modules["rlm.bash"]
        bash_module._request({"type": "bash.check", "command": "true", "script": "true"})
        proc = bash_module._sidecar._proc
        self.assertIsNotNone(proc)
        environ = Path(f"/proc/{proc.pid}/environ").read_bytes().split(b"\0")
        self.assertIn(b"PA_TEST_BASH_HOST_CONFINED=1", environ)

    @unittest.skipIf(_ROOT is None, "no OS sandbox on this machine")
    def test_the_suites_never_see_the_real_home(self):
        # HOME, TMPDIR and the agent directory all live under the private root.
        for name in ("HOME", "TMPDIR", "PRIME_AGENT_CODING_AGENT_DIR"):
            self.assertTrue(os.environ[name].startswith(str(_ROOT)), name)


class OutsideTheRailsTest(unittest.TestCase):
    """Another suite in the same process sees neither the confinement nor
    the confined host: the rails apply around each guard-suite test only."""

    def test_confinement_does_not_leak(self):
        # (Never assert on os.environ itself: a failure would print it.)
        self.assertFalse("PA_TEST_BASH_HOST_ROOT" in os.environ, "the confinement flag leaked")
        self.assertFalse(os.environ.get("HOME", "").startswith(str(_ROOT)))
        proc = sys.modules["rlm.bash"]._sidecar._proc
        self.assertTrue(proc is None or proc.poll() is not None)


if __name__ == "__main__":
    unittest.main()
