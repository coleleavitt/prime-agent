from __future__ import annotations

import asyncio
import json
import os
import resource
import signal
import sys
import tempfile
import unittest
from concurrent.futures import ThreadPoolExecutor
from types import FunctionType, SimpleNamespace
from unittest import mock

from rlm import bash

# The package re-exports the bash() function under the same name, so reach the
# module through sys.modules for internals.
bash_module = sys.modules["rlm.bash"]


class BashTest(unittest.IsolatedAsyncioTestCase):
    async def test_activity_handles_are_scoped_and_tail_is_bounded(self):
        handle = bash("printf 'first\nsecond\n'; sleep 20")
        from rlm.bash import activity_request

        activity_id = handle._activity_id
        rows = activity_request("list")["activities"]
        listed = next(row for row in rows if row["id"] == activity_id)
        self.assertEqual(listed["pid"], handle.pid)
        self.assertIsNone(listed["exitCode"])
        self.assertIn("T", listed["startedAt"])
        for _ in range(100):
            if "second" in activity_request("tail", activity_id, 1)["tail"]:
                break
            await asyncio.sleep(0.01)
        self.assertEqual(activity_request("tail", activity_id, 1)["tail"], "second")
        self.assertEqual(activity_request("kill", activity_id)["killed"], True)
        await handle
        finished = next(
            row for row in activity_request("list")["activities"] if row["id"] == activity_id
        )
        self.assertEqual(finished["status"], "finished")
        self.assertIsInstance(finished["exitCode"], int)
        with self.assertRaises(KeyError):
            activity_request("kill", "not-a-handle")
        with self.assertRaises(ValueError):
            activity_request("tail", activity_id, 201)
        self.assertFalse(activity_request("kill", activity_id)["killed"])

    async def test_activity_tail_frame_stays_under_the_wire_cap(self):
        # json escaping can expand one byte to six (\uXXXX), so the cap
        # must hold on the serialized frame, not the decoded slice.
        handle = bash('python3 -c "print(chr(0) * 20000)"')
        await handle
        from rlm.bash import activity_request

        activity_id = handle._activity_id
        tail = activity_request("tail", activity_id, 200)["tail"]
        self.assertGreater(len(tail), 0)
        self.assertLessEqual(len(json.dumps({"tail": tail})), 16_384)

    async def test_activity_list_frame_stays_under_the_wire_cap(self):
        handles = [bash("sleep 3 # " + str(index) * 80) for index in range(30)]
        try:
            from rlm.bash import activity_request

            rows = activity_request("list")["activities"]
            self.assertGreater(len(rows), 1)
            self.assertLessEqual(len(json.dumps({"activities": rows})), 16_384)
            self.assertTrue(all(len(row["command"]) <= 512 for row in rows))
        finally:
            for handle in handles:
                handle.kill()

    async def test_activity_tail_keeps_the_newest_output_under_the_cap(self):
        # Escaped output shrinks from the oldest end: the newest line is
        # always the surviving one.
        handle = bash('python3 -c "print(chr(0) * 20000); print(chr(65) * 8)"')
        await handle
        from rlm.bash import activity_request

        activity_id = handle._activity_id
        tail = activity_request("tail", activity_id, 200)["tail"]
        self.assertTrue(tail.endswith("AAAAAAAA"), tail[-60:])
        self.assertLessEqual(len(json.dumps({"tail": tail})), 16_384)

    async def test_await_returns_result(self):
        result = await bash("echo hi")
        self.assertEqual(result.exit_code, 0)
        self.assertIn("hi", result.output)
        self.assertGreaterEqual(result.duration, 0)

        handle = bash("echo again")
        awaited = await handle
        self.assertEqual(handle.poll(), awaited)

    async def test_awaiting_a_result_again_returns_it(self):
        # Models write `h = await bash(...)` and then `await h` (8 times in
        # user logs): awaiting the result is harmless and gives it back.
        result = await bash("echo hi")
        self.assertIs(await result, result)

    async def test_subscripting_the_output_method_names_both_spellings(self):
        # `handle.output[-2000:]` (6 times in user logs) raised "'method'
        # object is not subscriptable"; the error now says what to write.
        handle = bash("echo hi")
        result = await handle
        with self.assertRaises(TypeError) as raised:
            handle.output[-2000:]  # noqa: B018 - the misuse under test
        self.assertIn(".output()", str(raised.exception))
        self.assertIn("(await h).output", str(raised.exception))
        with self.assertRaises(AttributeError) as raised:
            handle.output.splitlines()
        self.assertIn(".output()", str(raised.exception))
        self.assertEqual(handle.output(), result.output)
        self.assertTrue(callable(bash_module.BashHandle.output))

    def test_construction_cleanup_uses_windows_signal_without_sigkill(self):
        failure = RuntimeError("task construction failed")
        loop = mock.Mock()
        loop.create_task.side_effect = failure
        bridge = SimpleNamespace(emit=mock.Mock())
        namespace = dict(bash_module.BashHandle._schedule_background_completion_notice.__globals__)

        def isolated_import(name, globals=None, locals=None, fromlist=(), level=0):
            if level == 1 and name == "" and fromlist == ("repl",):
                return SimpleNamespace(repl=bridge)
            return __import__(name, globals, locals, fromlist, level)

        namespace.update(
            _IS_POSIX=False,
            signal=SimpleNamespace(SIGTERM=15),
            asyncio=SimpleNamespace(get_running_loop=lambda: loop),
            __builtins__={**vars(__import__("builtins")), "__import__": isolated_import},
        )
        schedule = FunctionType(
            bash_module.BashHandle._schedule_background_completion_notice.__code__, namespace
        )
        handle = mock.Mock(_pid=42)
        with self.assertRaises(RuntimeError) as caught:
            schedule(handle)
        self.assertIs(caught.exception, failure)
        handle.kill.assert_called_once_with(15)
        handle._notify_background_completion.return_value.close.assert_called_once_with()
        activity = bridge.emit.call_args_list[0].args[0]
        mime = "application/vnd.prime-agent.bash-activity+json"
        self.assertTrue(activity[mime]["active"])
        bridge.emit.assert_called_with({mime: {**activity[mime], "active": False}})

    async def test_status_pipe_survives_high_fds_and_strict_posix_shell(self):
        # Regression: dash rejects multi-digit fds in redirections at parse
        # time, so the script must never reference the raw status-pipe fd.
        dummies = [os.open(os.devnull, os.O_RDONLY) for _ in range(30)]
        self.addCleanup(lambda: [os.close(fd) for fd in dummies])
        if os.path.exists("/bin/dash"):
            with mock.patch.dict(os.environ, {"PRIME_AGENT_BASH_SHELL": "/bin/dash"}):
                result = await bash("echo ok")
            self.assertEqual(result.exit_code, 0)
            self.assertIn("ok", result.output)
        result = await bash("echo ok-default")
        self.assertEqual(result.exit_code, 0)
        self.assertIn("ok-default", result.output)

    async def test_backgrounded_tail_and_kill(self):
        handle = bash("echo start; sleep 30")
        self.assertIsNone(handle.poll())
        for _ in range(100):
            if "start" in handle.tail():
                break
            await asyncio.sleep(0.05)
        self.assertIn("start", handle.tail())
        self.assertTrue(handle.running)
        handle.kill(grace=0.2)
        result = await asyncio.wait_for(handle, timeout=5)
        self.assertNotEqual(result.exit_code, 0)

    async def test_kill_escalates_to_sigkill(self):
        handle = bash("trap '' TERM; echo up; sleep 30")
        for _ in range(100):
            if "up" in handle.output():
                break
            await asyncio.sleep(0.05)
        handle.kill(grace=0.2)
        result = await asyncio.wait_for(handle, timeout=10)
        self.assertEqual(result.exit_code, -9)

    async def test_buffer_cap_keeps_head_and_tail(self):
        result = await bash("seq 1 400000")
        self.assertLessEqual(len(result.output), 2 * 1024 * 1024 + 256)
        self.assertTrue(result.output.startswith("1\n"))
        self.assertIn("400000", result.output)
        self.assertIn("bytes dropped", result.output)

    def test_child_env_is_non_interactive(self):
        """Agent shells have no usable stdin: interactive prompts (git commit
        opening $EDITOR, credential asks, pagers) can only hang. _child_env()
        must neutralize them, overriding inherited terminal settings."""
        with mock.patch.dict(
            os.environ,
            {
                "EDITOR": "vim",
                "PAGER": "less",
                "GIT_SEQUENCE_EDITOR": "vim",
                "GIT_ASKPASS": "/usr/bin/git-credential-manager",
                "SSH_ASKPASS_REQUIRE": "force",
            },
        ):
            env = bash_module._child_env()
        self.assertEqual(env["GIT_EDITOR"], "true")
        self.assertEqual(env["GIT_SEQUENCE_EDITOR"], "true")
        self.assertEqual(env["EDITOR"], "true")
        self.assertEqual(env["VISUAL"], "true")
        self.assertEqual(env["GIT_TERMINAL_PROMPTS"], "0")
        self.assertEqual(env["GIT_ASKPASS"], "true")
        self.assertEqual(env["SSH_ASKPASS_REQUIRE"], "never")
        self.assertEqual(env["PAGER"], "cat")
        self.assertEqual(env["GIT_PAGER"], "cat")
        self.assertEqual(env["DEBIAN_FRONTEND"], "noninteractive")

    async def test_spawned_shell_receives_non_interactive_env(self):
        handle = bash(
            'echo "$GIT_EDITOR|$GIT_SEQUENCE_EDITOR|$GIT_TERMINAL_PROMPTS|$GIT_ASKPASS|$SSH_ASKPASS_REQUIRE"'
        )
        result = await handle
        self.assertEqual(result.exit_code, 0)
        self.assertIn("true|true|0|true|never", result.output)

    async def test_env_prefix_and_journal(self):
        with tempfile.TemporaryDirectory() as tmp:
            journal = os.path.join(tmp, "journal.jsonl")
            with mock.patch.dict(
                os.environ,
                {
                    "PRIME_AGENT_BASH_COMMAND_PREFIX": "echo prefixed",
                    "PRIME_AGENT_INTERNAL_ORPHAN_PROCESS_JOURNAL": journal,
                    "PRIME_AGENT_KERNEL_OWNER_PID": str(os.getpid()),
                },
            ):
                handle = bash('echo "$NO_COLOR $TERM"')
                # The handle keeps the caller's text for display (the
                # completion notice and repr), not the prefixed script.
                self.assertEqual(handle.command, 'echo "$NO_COLOR $TERM"')
                result = await handle
                # The inactive record lands slightly after finalize, once the group exits.
                records = await _poll_journal(journal, count=2)
            self.assertEqual(result.exit_code, 0)
            lines = result.output.splitlines()
            self.assertEqual(lines[0], "prefixed")
            self.assertIn("1 dumb", lines[1])

            self.assertEqual([r["active"] for r in records], [True, False])
            for record in records:
                self.assertEqual(record["pid"], handle.pid)
                self.assertEqual(record["ownerPid"], os.getpid())
                self.assertEqual(record["kernelPid"], os.getpid())
            self.assertTrue(records[0]["processStartId"].startswith(("proc:", "ps:")))

    async def test_await_returns_when_shell_backgrounds_child(self):
        with tempfile.TemporaryDirectory() as tmp:
            journal = os.path.join(tmp, "journal.jsonl")
            with mock.patch.dict(
                os.environ,
                {
                    "PRIME_AGENT_INTERNAL_ORPHAN_PROCESS_JOURNAL": journal,
                    "PRIME_AGENT_KERNEL_OWNER_PID": str(os.getpid()),
                },
            ):
                handle = bash("echo fg; sleep 30 &")
                result = await asyncio.wait_for(handle, timeout=5)
                self.assertEqual(result.exit_code, 0)
                self.assertIn("fg", result.output)
                # The shell stays alive as group leader, anchoring its background job.
                os.killpg(handle.pid, 0)
                records = await _poll_journal(journal, count=1)
                self.assertTrue(records[-1]["active"])
                handle.kill(signal.SIGKILL)
                await _poll_group_dead(handle.pid)
                deadline = asyncio.get_running_loop().time() + 10
                while asyncio.get_running_loop().time() < deadline:
                    records = await _poll_journal(journal, count=1)
                    if records and not records[-1]["active"]:
                        break
                    await asyncio.sleep(0.05)
            self.assertFalse(records[-1]["active"])

    async def test_early_shell_exit_returns_and_kills_group(self):
        handle = bash("sleep 30 & exit 7")
        result = await asyncio.wait_for(handle, timeout=5)
        self.assertEqual(result.exit_code, 7)
        # The leader died without draining, so the stale group must be killed.
        await _poll_group_dead(handle.pid)

    async def test_term_ignoring_child_is_escalated(self):
        with tempfile.TemporaryDirectory() as tmp:
            journal = os.path.join(tmp, "journal.jsonl")
            with mock.patch.dict(
                os.environ,
                {
                    "PRIME_AGENT_INTERNAL_ORPHAN_PROCESS_JOURNAL": journal,
                    "PRIME_AGENT_KERNEL_OWNER_PID": str(os.getpid()),
                },
            ):
                handle = bash("sh -c 'trap \"\" TERM; echo ready; sleep 30' &")
                await asyncio.wait_for(handle, timeout=5)
                for _ in range(100):
                    if "ready" in handle.output():
                        break
                    await asyncio.sleep(0.05)
                handle.kill(signal.SIGTERM)
                await _poll_group_dead(handle.pid)
                deadline = asyncio.get_running_loop().time() + 10
                while asyncio.get_running_loop().time() < deadline:
                    records = await _poll_journal(journal, count=1)
                    if records and not records[-1]["active"]:
                        break
                    await asyncio.sleep(0.05)
            self.assertFalse(records[-1]["active"])

    async def test_status_survives_pipe_fds_above_fd_setsize(self):
        # select.select() rejects fds >= FD_SETSIZE (1024); the delivered status
        # must still win when the status/wake pipes land above that boundary.
        limits = resource.getrlimit(resource.RLIMIT_NOFILE)
        if limits[0] < 1100:
            try:
                resource.setrlimit(resource.RLIMIT_NOFILE, (1100, limits[1]))
            except (ValueError, OSError):
                self.skipTest("cannot raise RLIMIT_NOFILE above FD_SETSIZE")
            self.addCleanup(resource.setrlimit, resource.RLIMIT_NOFILE, limits)
        held: list[int] = []
        self.addCleanup(lambda: [os.close(fd) for fd in held])
        while True:
            fd = os.open(os.devnull, os.O_RDONLY)
            held.append(fd)
            if fd >= 1024:
                break
        handle = bash("echo hi; sleep 30 & true")
        self.addCleanup(handle.kill, signal.SIGKILL)
        result = await asyncio.wait_for(handle, timeout=5)
        self.assertEqual(result.exit_code, 0)
        self.assertIn("hi", result.output)

    async def test_awaits_do_not_hold_executor_threads(self):
        loop = asyncio.get_running_loop()
        executor = ThreadPoolExecutor(max_workers=1)
        loop.set_default_executor(executor)
        tasks = [asyncio.ensure_future(bash("sleep 0.5")._wait()) for _ in range(3)]
        await asyncio.sleep(0.1)
        # Old executor-parked waits would deadlock this 1-thread pool.
        value = await asyncio.wait_for(loop.run_in_executor(None, lambda: 42), timeout=0.3)
        self.assertEqual(value, 42)
        results = await asyncio.gather(*tasks)
        self.assertTrue(all(r.exit_code == 0 for r in results))

    async def test_running_reflects_group_liveness(self):
        handle = bash("echo fg; sleep 30 &")
        result = await asyncio.wait_for(handle, timeout=5)
        self.assertEqual(result.exit_code, 0)
        # The foreground result is in, but the group still anchors `sleep 30 &`.
        self.assertIsNotNone(handle.poll())
        self.assertTrue(handle.running)
        handle.kill(signal.SIGKILL)
        for _ in range(100):
            if not handle.running:
                break
            await asyncio.sleep(0.05)
        self.assertFalse(handle.running)

    async def test_cancelled_direct_await_kills_group(self):
        with tempfile.TemporaryDirectory() as tmp:
            marker = os.path.join(tmp, "marker")
            pids: list[int] = []
            original_init = bash_module.BashHandle.__init__

            def capturing_init(handle_self, command, script=None, _validated=False):
                original_init(handle_self, command, script=script, _validated=_validated)
                pids.append(handle_self._pid)

            async def run_oneshot():
                await bash(f"sleep 1.0 && touch {marker}")

            with mock.patch.object(bash_module.BashHandle, "__init__", capturing_init):
                task = asyncio.ensure_future(run_oneshot())
                await asyncio.sleep(0.3)
                task.cancel()
                with self.assertRaises(asyncio.CancelledError):
                    await task
            # The cancel path awaits confirmed group death before propagating.
            if bash_module._IS_POSIX:
                with self.assertRaises(ProcessLookupError):
                    os.killpg(pids[0], 0)
            await asyncio.sleep(1.0)
            self.assertFalse(os.path.exists(marker))

    async def test_cancelled_direct_await_escalates_past_term_trap(self):
        # A TERM-trapping command must be group-KILLed before the cancel
        # resolves, so its later side effects never land.
        with tempfile.TemporaryDirectory() as tmp:
            marker = os.path.join(tmp, "marker")
            pids: list[int] = []
            original_init = bash_module.BashHandle.__init__

            def capturing_init(handle_self, command, script=None, _validated=False):
                original_init(handle_self, command, script=script, _validated=_validated)
                pids.append(handle_self._pid)

            async def run_oneshot():
                await bash(f"trap '' TERM; sleep 1.0; touch {marker}; sleep 30")

            with mock.patch.object(bash_module, "_CANCEL_TERM_GRACE", 0.2):
                with mock.patch.object(bash_module.BashHandle, "__init__", capturing_init):
                    task = asyncio.ensure_future(run_oneshot())
                    await asyncio.sleep(0.2)
                    task.cancel()
                    with self.assertRaises(asyncio.CancelledError):
                        await task
            if bash_module._IS_POSIX:
                with self.assertRaises(ProcessLookupError):
                    os.killpg(pids[0], 0)
            await asyncio.sleep(1.2)
            self.assertFalse(os.path.exists(marker))

    async def test_background_handle_survives_cancel_of_creating_context(self):
        handles: list[bash_module.BashHandle] = []

        async def run_background():
            h = bash("sleep 30")
            handles.append(h)
            h.pid  # released as a deliberate background handle
            await asyncio.sleep(10)

        task = asyncio.ensure_future(run_background())
        await asyncio.sleep(0.3)
        task.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await task
        handle = handles[0]
        try:
            os.killpg(handle._pid, 0)  # still alive
        finally:
            handle.kill(signal.SIGKILL)
        await asyncio.wait_for(handle, timeout=5)

    async def test_cancelling_await_on_released_handle_does_not_kill(self):
        handle = bash("sleep 30")
        self.assertTrue(handle.running)  # release as background handle

        async def wait_for_it():
            await handle

        task = asyncio.ensure_future(wait_for_it())
        await asyncio.sleep(0.3)
        task.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await task
        try:
            os.killpg(handle._pid, 0)  # still alive
        finally:
            handle.kill(signal.SIGKILL)
        await asyncio.wait_for(handle, timeout=5)

    async def test_second_await_after_cancelled_oneshot_only_waits(self):
        handle = bash("echo done")
        # First await consumes the one-shot ownership; later awaits only wait.
        result = await handle
        self.assertEqual(result.exit_code, 0)
        self.assertTrue(handle._released)
        again = await handle
        self.assertEqual(again, result)

    async def test_second_cancel_during_cleanup_still_confirms_group_death(self):
        # Python 3.11: an await inside an except-CancelledError block of a
        # cancelled task is re-cancelled immediately; the shielded confirm task
        # must survive repeated cancels and the group must be dead on return.
        pids: list[int] = []
        original_init = bash_module.BashHandle.__init__

        def capturing_init(handle_self, command, script=None, _validated=False):
            original_init(handle_self, command, script=script, _validated=_validated)
            pids.append(handle_self._pid)

        async def run_oneshot():
            await bash("trap '' TERM; sleep 30")

        with mock.patch.object(bash_module, "_CANCEL_TERM_GRACE", 0.2):
            with mock.patch.object(bash_module.BashHandle, "__init__", capturing_init):
                task = asyncio.ensure_future(run_oneshot())
                await asyncio.sleep(0.2)
                task.cancel()
                await asyncio.sleep(0.05)
                task.cancel()  # lands inside the cleanup awaits
                await asyncio.sleep(0.05)
                task.cancel()
                with self.assertRaises(asyncio.CancelledError):
                    await task
        with self.assertRaises(ProcessLookupError):
            os.killpg(pids[0], 0)

    async def test_user_alias_cannot_replace_completion_emitter(self):
        if os.path.basename(bash_module._shell()) != "bash":
            self.skipTest("bash alias expansion semantics")
        handle = bash(
            "shopt -s expand_aliases; alias command='printf alias-expanded'; sleep 30 &"
        )
        try:
            result = await asyncio.wait_for(handle, timeout=5)
            self.assertEqual(result.exit_code, 0)
            self.assertNotIn("alias-expanded", result.output)
        finally:
            handle.kill(signal.SIGKILL)
            await _poll_group_dead(handle.pid)

    async def test_user_function_cannot_replace_completion_emitter(self):
        # The backslash in `\command` defeats alias expansion only: a shell
        # function named `command` would otherwise swallow both fence frames
        # and wedge the await behind the background job until shell death.
        handle = bash("command() { printf function-expanded; }; sleep 30 &")
        try:
            result = await asyncio.wait_for(handle, timeout=5)
            self.assertEqual(result.exit_code, 0)
            self.assertNotIn("function-expanded", result.output)
        finally:
            handle.kill(signal.SIGKILL)
            await _poll_group_dead(handle.pid)

    async def test_shell_killed_before_sentinel_finalizes_from_exit(self):
        result = await asyncio.wait_for(
            bash("printf output-before-shell-kill; kill -KILL $$"), timeout=5
        )
        self.assertEqual(result.exit_code, -signal.SIGKILL)
        self.assertIn("output-before-shell-kill", result.output)

    async def test_relative_bash_shell_override_rejected(self):
        with mock.patch.dict(os.environ, {"PRIME_AGENT_BASH_SHELL": "bash"}):
            with self.assertRaises(ValueError):
                bash("echo hi")

    async def test_journal_configured_but_unwritable_runs_untracked(self):
        # Best-effort tracking: a journal that cannot be written (here a
        # directory) leaves the command untracked; it never fails the spawn.
        with tempfile.TemporaryDirectory() as tmp:
            marker = os.path.join(tmp, "marker")
            with mock.patch.dict(
                os.environ,
                {
                    "PRIME_AGENT_INTERNAL_ORPHAN_PROCESS_JOURNAL": tmp,  # a directory: open fails
                    "PRIME_AGENT_KERNEL_OWNER_PID": str(os.getpid()),
                },
            ):
                result = await bash(f"touch {marker}")
            self.assertEqual(result.exit_code, 0)
            self.assertTrue(os.path.exists(marker))

    async def test_journal_bad_owner_pid_rejects(self):
        with tempfile.TemporaryDirectory() as tmp:
            journal = os.path.join(tmp, "journal.jsonl")
            with mock.patch.dict(
                os.environ,
                {
                    "PRIME_AGENT_INTERNAL_ORPHAN_PROCESS_JOURNAL": journal,
                    "PRIME_AGENT_KERNEL_OWNER_PID": "notanint",
                },
            ):
                with self.assertRaises(RuntimeError):
                    bash("echo hi")

async def _poll_group_dead(pgid: int, timeout: float = 5.0) -> None:
    deadline = asyncio.get_running_loop().time() + timeout
    while asyncio.get_running_loop().time() < deadline:
        try:
            os.killpg(pgid, 0)
        except ProcessLookupError:
            return
        except PermissionError:
            pass  # transient teardown state on macOS
        await asyncio.sleep(0.05)
    raise AssertionError(f"process group {pgid} still alive after {timeout}s")


async def _poll_journal(path: str, count: int, timeout: float = 2.0) -> list[dict]:
    deadline = asyncio.get_running_loop().time() + timeout
    records: list[dict] = []
    while asyncio.get_running_loop().time() < deadline:
        with open(path) as f:
            records = [json.loads(line) for line in f if line.strip()]
        if len(records) >= count:
            return records
        await asyncio.sleep(0.05)
    return records


class BashHostSkewTest(unittest.TestCase):
    """A host or sidecar that cannot serve bash() fails with the
    host/runtime version skew named and the fix."""

    def test_a_repl_host_that_does_not_announce_bash_keeps_the_sidecar(self):
        # A REPL host that is not a Prime Agent session (this suite's
        # harness) does not serve bash.*: the sidecar serves it. A Prime
        # Agent host too old to serve bash() speaks protocol 4 and is
        # refused at the handshake before any cell runs.
        from rlm import repl

        sidecar_reply = {"status": "ok", "activities": []}
        with (
            mock.patch.object(bash_module, "_HOST_SERVES_BASH", False),
            mock.patch.object(repl, "is_active", return_value=True),
            mock.patch.object(bash_module._sidecar, "request", return_value=sidecar_reply) as request,
        ):
            self.assertEqual(bash_module._request({"type": "bash.list"}), sidecar_reply)
        request.assert_called_once_with({"type": "bash.list"})

    def test_a_host_missing_a_bash_request_type_names_the_skew(self):
        from rlm import repl

        unserved = {"status": "error", "error": 'host request type "bash.list" is not available in this session'}
        with (
            mock.patch.object(bash_module, "_HOST_SERVES_BASH", True),
            mock.patch.object(repl, "is_active", return_value=True),
            mock.patch.object(repl, "host_request_blocking", return_value=unserved),
        ):
            with self.assertRaises(bash_module.BashHostUnavailable) as caught:
                bash_module._request({"type": "bash.list"})
        message = str(caught.exception)
        self.assertIn('host request type "bash.list" is not available in this session', message)
        self.assertIn("host/runtime version skew", message)

    def test_outside_a_kernel_a_sidecar_that_exits_names_the_likely_skew(self):
        # Genuine non-kernel use keeps the sidecar; a binary that does not
        # know the sidecar flag exits at once, and the error says why.
        sidecar = bash_module._Sidecar()
        with mock.patch.dict(os.environ, {"PRIME_AGENT_EXECUTABLE": "/bin/true"}):
            with self.assertRaises(bash_module.BashHostUnavailable) as caught:
                sidecar.request({"type": "bash.list"})
        message = str(caught.exception)
        self.assertIn("the bash host (/bin/true) exited before answering", message)
        self.assertIn("--prime-agent-bash-host", message)
        self.assertIn("Reinstall prime-agent", message)


if __name__ == "__main__":
    unittest.main()
