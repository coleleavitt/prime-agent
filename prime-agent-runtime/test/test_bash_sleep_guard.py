"""Long-foreground-sleep guard for the kernel bash tool.

The rule itself (which commands count) is tested in pa-bash
(`guards::tests::long_leading_sleeps_are_refused`); this suite covers what the
kernel adds around it: the exception, the bypasses and the warn-once.
"""

from __future__ import annotations

import contextlib
import io
import os
import sys
import tempfile
import unittest
from unittest import mock

import guard_safety
from rlm import bash

bash_module = sys.modules["rlm.bash"]

guard_safety.confine()

SleepRefusalError = bash_module.SleepRefusalError
BASH_SLEEP_BYPASS_ENV = bash_module.BASH_SLEEP_BYPASS_ENV


class SleepGuardTest(guard_safety.RefusalSafe, unittest.TestCase):
    def setUp(self):
        self._cwd = os.getcwd()
        self._environ = dict(os.environ)
        os.environ.pop(BASH_SLEEP_BYPASS_ENV, None)
        os.environ.pop("PRIME_AGENT_BASH_COMMAND_PREFIX", None)
        for name, value in (("_SLEEP_BYPASS_AT_KERNEL_START", False), ("_sleep_late_bypass_warned", False)):
            patch = mock.patch.object(bash_module, name, value)
            patch.start()
            self.addCleanup(patch.stop)
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        os.chdir(tmp.name)
        self.addCleanup(self._restore)

    def _restore(self):
        os.chdir(self._cwd)
        os.environ.clear()
        os.environ.update(self._environ)

    def _mock_handle(self):
        patch = mock.patch.object(bash_module, "BashHandle")
        instance = patch.start()
        self.addCleanup(patch.stop)
        return instance

    def _refused(self, command, **kwargs):
        with self.assertRaises(SleepRefusalError) as caught:
            bash(command, **kwargs)
        return str(caught.exception)

    def test_refusal_names_the_alternatives_and_the_bypasses(self):
        message = self._refused("sleep 150 && echo retry")
        self.assertIn("sleep 150 followed by other commands", message)
        self.assertIn("until <check>; do sleep 2; done", message)
        self.assertIn("allow_sleep=True", message)
        self.assertIn(BASH_SLEEP_BYPASS_ENV, message)

    def test_refusal_happens_before_any_process_starts(self):
        spawn = mock.patch.object(bash_module, "BashHandle", side_effect=AssertionError("spawned"))
        spawn.start()
        self.addCleanup(spawn.stop)
        self._refused("sleep 150")

    def test_a_loop_or_a_short_sleep_runs(self):
        instance = self._mock_handle()
        for command in ("sleep 5", "until test -f ready; do sleep 2; done"):
            with self.subTest(command=command):
                bash(command)
        self.assertEqual(instance.call_count, 2)

    def test_kwarg_bypass_passes_the_guard_and_does_not_leak(self):
        instance = self._mock_handle()
        bash("sleep 150", allow_sleep=True)
        instance.assert_called_once_with("sleep 150", script="sleep 150", _validated=True)
        self._refused("sleep 150")

    def test_frozen_env_bypass_is_honored(self):
        instance = self._mock_handle()
        with mock.patch.object(bash_module, "_SLEEP_BYPASS_AT_KERNEL_START", True):
            bash("sleep 150")
        instance.assert_called_once_with("sleep 150", script="sleep 150", _validated=True)

    def test_late_bypass_env_is_ignored_and_warned_once(self):
        os.environ[BASH_SLEEP_BYPASS_ENV] = "1"
        stderr = io.StringIO()
        with contextlib.redirect_stderr(stderr):
            self._refused("sleep 150")
            self._refused("sleep 150")
        self.assertIn("PI_BASH_ALLOW_SLEEP appeared after kernel start", stderr.getvalue())
        self.assertEqual(stderr.getvalue().count("appeared after kernel start"), 1)

    def test_direct_handle_construction_is_guarded(self):
        with self.assertRaises(SleepRefusalError):
            bash_module.BashHandle("sleep 150")


if __name__ == "__main__":
    unittest.main()
