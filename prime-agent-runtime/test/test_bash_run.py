"""bash()'s one-request path (`bash.run`): a short command's whole life is one
host request, a longer one continues through `bash.follow`, a long result
arrives through a spill file, and an interrupt during the run's wait kills
the command before the KeyboardInterrupt goes on."""

from __future__ import annotations

import asyncio
import os
import signal
import sys
import tempfile
import threading
import time
import unittest
from typing import Any
from unittest import mock

from rlm import bash

bash_module = sys.modules["rlm.bash"]
AWAIT_TIMEOUT = 30


class _Counter:
    """Counts the bash.* requests that reach the transport."""

    def __init__(self) -> None:
        self.types: list[str] = []
        self._request = bash_module._sidecar.request
        self._arequest = bash_module._sidecar.arequest

    def request(self, data: dict[str, Any], **kwargs: Any) -> dict[str, Any]:
        self.types.append(str(data.get("type")))
        return self._request(data, **kwargs)

    async def arequest(self, data: dict[str, Any]) -> dict[str, Any]:
        self.types.append(str(data.get("type")))
        return await self._arequest(data)

    def patch(self, test: unittest.TestCase) -> None:
        for name in ("request", "arequest"):
            patcher = mock.patch.object(bash_module._sidecar, name, getattr(self, name))
            patcher.start()
            test.addCleanup(patcher.stop)


class BashRunTest(unittest.IsolatedAsyncioTestCase):
    async def asyncSetUp(self) -> None:
        # Warm the sidecar so its start is not part of what a test counts.
        await asyncio.wait_for(bash("true"), AWAIT_TIMEOUT)

    async def test_a_short_command_is_one_host_request(self):
        counter = _Counter()
        counter.patch(self)
        # The window is a latency bet (25 ms); what this guards is that a
        # command done inside it needs no follow, so give it room under load.
        with mock.patch.object(bash_module, "_RUN_WINDOW_MS", 10_000):
            handle = bash("echo hi")
        self.assertEqual(counter.types, ["bash.run"])
        self.assertFalse(handle.running)  # finished and reaped inside the run
        result = await asyncio.wait_for(handle, AWAIT_TIMEOUT)
        self.assertEqual((result.exit_code, result.output), (0, "hi\n"))
        self.assertEqual(handle.output(), "hi\n")
        self.assertEqual(counter.types, ["bash.run"])

    async def test_a_longer_command_continues_through_follow(self):
        counter = _Counter()
        counter.patch(self)
        handle = bash("sleep 0.5; echo done")
        self.assertTrue(handle.running)
        result = await asyncio.wait_for(handle, AWAIT_TIMEOUT)
        self.assertEqual((result.exit_code, result.output), (0, "done\n"))
        self.assertEqual(counter.types[0], "bash.run")
        self.assertIn("bash.follow", counter.types)

    async def test_a_live_handle_skips_the_window(self):
        long = bash("sleep 30")
        try:
            with mock.patch.object(bash_module, "_RUN_WINDOW_MS", 10_000):
                started = time.monotonic()
                other = bash("sleep 2")
                self.assertLess(time.monotonic() - started, 1.5)
                self.assertTrue(other.running)
                other.kill(signal.SIGKILL)
        finally:
            long.kill(signal.SIGKILL)
        await asyncio.wait_for(long, AWAIT_TIMEOUT)

    async def test_a_finished_command_does_not_skip_the_next_window(self):
        # The first command's result is in, but its background child keeps the
        # group (and so the handle) unreaped: it is not a command in flight,
        # so the next bash() still gets its window.
        first = bash("sleep 30 >/dev/null 2>&1 & echo first")
        try:
            result = await asyncio.wait_for(first, AWAIT_TIMEOUT)
            self.assertEqual((result.exit_code, result.output), (0, "first\n"))
            counter = _Counter()
            counter.patch(self)
            with mock.patch.object(bash_module, "_RUN_WINDOW_MS", 10_000):
                handle = bash("echo hi")
            self.assertEqual(counter.types, ["bash.run"])
            self.assertFalse(handle.running)
        finally:
            first.kill(signal.SIGKILL)

    async def test_a_long_result_arrives_through_a_removed_spill_file(self):
        with tempfile.TemporaryDirectory() as spill:
            with mock.patch.object(bash_module.tempfile, "gettempdir", return_value=spill):
                result = await asyncio.wait_for(bash("head -c 300000 /dev/zero | tr '\\0' y"), AWAIT_TIMEOUT)
            self.assertEqual(result.output, "y" * 300_000)
            self.assertEqual(os.listdir(spill), [])

    async def test_an_unreadable_spill_file_falls_back_to_the_host_buffer(self):
        real_open = open

        def failing_open(path: Any, *args: Any, **kwargs: Any) -> Any:
            if str(path).startswith(tempfile.gettempdir()) and "pa-bash-output-" in str(path):
                raise PermissionError(13, "denied")
            return real_open(path, *args, **kwargs)

        with mock.patch("builtins.open", failing_open):
            result = await asyncio.wait_for(bash("head -c 300000 /dev/zero | tr '\\0' z"), AWAIT_TIMEOUT)
        self.assertEqual(result.output, "z" * 300_000)

    async def test_a_changed_environment_reaches_the_command(self):
        os.environ["PA_BASH_RUN_TEST_VALUE"] = "first"
        try:
            first = await asyncio.wait_for(bash("echo $PA_BASH_RUN_TEST_VALUE"), AWAIT_TIMEOUT)
            again = await asyncio.wait_for(bash("echo $PA_BASH_RUN_TEST_VALUE"), AWAIT_TIMEOUT)
            os.environ["PA_BASH_RUN_TEST_VALUE"] = "second"
            changed = await asyncio.wait_for(bash("echo $PA_BASH_RUN_TEST_VALUE"), AWAIT_TIMEOUT)
        finally:
            del os.environ["PA_BASH_RUN_TEST_VALUE"]
        self.assertEqual(
            [first.output, again.output, changed.output],
            ["first\n", "first\n", "second\n"],
        )

    async def test_a_host_that_lost_the_environment_gets_it_again(self):
        await asyncio.wait_for(bash("true"), AWAIT_TIMEOUT)
        # A different key than the host remembers: as after a sidecar restart.
        with bash_module._env_lock:
            key, env = bash_module._env_sent
            setattr(bash_module, "_env_sent", ("not-" + key, env))
        counter = _Counter()
        counter.patch(self)
        result = await asyncio.wait_for(bash("echo again"), AWAIT_TIMEOUT)
        self.assertEqual(result.output, "again\n")
        self.assertEqual(counter.types, ["bash.run", "bash.run"])  # the retry carries it whole
        await asyncio.wait_for(bash("true"), AWAIT_TIMEOUT)
        self.assertEqual(counter.types, ["bash.run"] * 3)  # and the host keeps it again


class BashRunInterruptTest(unittest.TestCase):
    def test_an_interrupt_during_the_run_kills_the_command_and_goes_on(self):
        if not hasattr(signal, "pthread_kill"):
            self.skipTest("needs pthread_kill")
        with tempfile.TemporaryDirectory() as tmp:
            marker = os.path.join(tmp, "marker")
            main = threading.main_thread().ident
            assert main is not None
            timer = threading.Timer(0.5, signal.pthread_kill, (main, signal.SIGINT))
            started = time.monotonic()
            with mock.patch.object(bash_module, "_RUN_WINDOW_MS", 20_000):
                timer.start()
                try:
                    with self.assertRaises(KeyboardInterrupt):
                        bash(f"echo $$ > {tmp}/pid; sleep 3 && touch {marker}")
                finally:
                    timer.cancel()
            self.assertLess(time.monotonic() - started, 5)
            with open(os.path.join(tmp, "pid"), encoding="utf-8") as recorded:
                pid = int(recorded.read())
            # The run answered only once the group was gone.
            with self.assertRaises(ProcessLookupError):
                os.killpg(pid, 0)
            time.sleep(3.5)
            self.assertFalse(os.path.exists(marker))


if __name__ == "__main__":
    unittest.main()
