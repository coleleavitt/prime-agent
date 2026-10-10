"""The runtime's agent-dir resolvers refuse the real home's state in tests."""

from __future__ import annotations

import contextlib
import os
import tempfile
import unittest
from collections.abc import Iterator
from pathlib import Path
from unittest import mock

from rlm import _state_guard
from rlm.harness import _agent_dir as harness_agent_dir
from rlm.mcp_base import _agent_dir as mcp_agent_dir


class StateGuardTest(unittest.TestCase):
    home: Path = Path()

    def setUp(self) -> None:
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.home = Path(temp.name).resolve()
        (self.home / ".prime" / "agent").mkdir(parents=True)

    @contextlib.contextmanager
    def _env(self, **values: str) -> Iterator[None]:
        """``self.home`` as both ``HOME`` and the protected home, no agent-dir override but ``values``."""
        with mock.patch.dict(os.environ):
            for name in ("PRIME_AGENT_CODING_AGENT_DIR", "PI_CODING_AGENT_DIR", _state_guard.ALLOW_REAL_STATE_ENV):
                _ = os.environ.pop(name, None)
            os.environ.update({"HOME": str(self.home), _state_guard.PROTECTED_HOME_ENV: str(self.home), **values})
            yield

    def test_this_unittest_run_is_a_test_process(self) -> None:
        self.assertTrue(_state_guard.is_test_process())

    def test_this_unittest_run_is_isolated_for_the_children_it_spawns(self) -> None:
        # Importing rlm isolated the run: its host binaries and kernels run the Rust guards,
        # and HOME is never a protected one.
        self.assertEqual(os.environ.get(_state_guard.ISOLATED_ENV), "1")
        home = os.environ.get("HOME")
        if home is not None:
            self.assertIsNone(_state_guard.real_state_violation("home", Path(home) / ".prime"))

    def test_the_default_agent_dir_under_a_protected_home_is_refused(self) -> None:
        with self._env():
            with self.assertRaises(_state_guard.RealStateError) as raised:
                _ = harness_agent_dir()
            self.assertIn(str(self.home / ".prime" / "agent"), str(raised.exception))
            with self.assertRaises(_state_guard.RealStateError):
                _ = mcp_agent_dir()

    def test_a_leaked_override_into_the_protected_home_is_refused(self) -> None:
        with self._env(PRIME_AGENT_CODING_AGENT_DIR=str(self.home / ".prime" / "agent")):
            with self.assertRaises(_state_guard.RealStateError):
                _ = harness_agent_dir()

    def test_a_symlink_into_the_protected_state_is_refused(self) -> None:
        with tempfile.TemporaryDirectory() as elsewhere:
            link = Path(elsewhere) / "agent"
            link.symlink_to(self.home / ".prime" / "agent")
            with self._env(PRIME_AGENT_CODING_AGENT_DIR=str(link)):
                with self.assertRaises(_state_guard.RealStateError):
                    _ = harness_agent_dir()

    def test_a_temp_agent_dir_and_the_explicit_opt_in_pass(self) -> None:
        with tempfile.TemporaryDirectory() as agent_dir:
            with self._env(PRIME_AGENT_CODING_AGENT_DIR=agent_dir):
                self.assertEqual(harness_agent_dir(), Path(agent_dir).resolve())
        with self._env(**{_state_guard.ALLOW_REAL_STATE_ENV: "1"}):
            self.assertEqual(harness_agent_dir(), self.home / ".prime" / "agent")

    def test_outside_a_test_process_nothing_changes(self) -> None:
        with self._env(), mock.patch.object(_state_guard, "is_test_process", return_value=False):
            self.assertEqual(harness_agent_dir(), self.home / ".prime" / "agent")

    def test_the_passwd_home_is_protected_whatever_home_says(self) -> None:
        real = _state_guard._passwd_home()
        if real is None:
            self.skipTest("no passwd entry for this user")
        with self._env():
            self.assertIn(real, _state_guard.protected_homes())
            self.assertIsNotNone(_state_guard.real_state_violation("agent dir", real / ".prime" / "agent"))

