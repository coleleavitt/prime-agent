from __future__ import annotations

import asyncio
import importlib
import unittest
from pathlib import Path
from unittest.mock import AsyncMock, patch

rlm_module = importlib.import_module("rlm")

FAMILY = {
    "agents": [
        {"sessionName": "lead", "relationship": "parent", "status": "running", "activeSessionId": "a-lead"},
        {"sessionName": "me", "relationship": None, "status": "running", "activeSessionId": "a-me"},
        {"sessionName": "scout", "relationship": "sibling", "status": "idle", "sessionId": "s-scout"},
        {"sessionName": "scout", "relationship": "child", "status": "inactive", "sessionId": "s-kid"},
        {"sessionName": "fixer", "relationship": "child", "status": "running", "activity": "tool", "cwd": "/w"},
    ]
}

SPAWN_REPLY = {"rlm_child_id": "sub-1", "name": "kid", "session_dir": "/tmp/kid", "model": "p/m"}


def _host(*replies: object) -> AsyncMock:
    return AsyncMock(side_effect=list(replies))


class SpawnAgentKwargsTest(unittest.TestCase):
    def test_agent_and_isolation_ride_the_spawn_kwargs(self) -> None:
        host = _host(SPAWN_REPLY)
        with patch.object(rlm_module, "host_request", host):
            asyncio.run(rlm_module.rlm.spawn("review", name="kid", agent=" reviewer ", isolation="worktree"))
        host.assert_awaited_once_with(
            "rlm.run",
            {"prompt": "review", "kwargs": {"name": "kid", "agent": "reviewer", "isolation": "worktree"}},
        )

    def test_invalid_agent_arguments_fail_before_the_host(self) -> None:
        host = _host()
        with patch.object(rlm_module, "host_request", host):
            with self.assertRaisesRegex(ValueError, "isolation"):
                asyncio.run(rlm_module.spawn("x", name="kid", isolation="vm"))
            with self.assertRaisesRegex(TypeError, "agent"):
                asyncio.run(rlm_module.spawn("x", name="kid", agent=""))
        host.assert_not_awaited()


class ListAgentsTest(unittest.TestCase):
    def test_rows_become_typed_agents_without_the_caller(self) -> None:
        with patch.object(rlm_module, "host_request", _host(FAMILY)):
            agents = asyncio.run(rlm_module.rlm.list_agents())
        self.assertEqual(
            agents,
            [
                rlm_module.RLMAgent(name="lead", relationship="parent", status="running", active_session_id="a-lead"),
                rlm_module.RLMAgent(name="scout", relationship="sibling", status="idle", session_id="s-scout"),
                rlm_module.RLMAgent(name="scout", relationship="child", status="inactive", session_id="s-kid"),
                rlm_module.RLMAgent(name="fixer", relationship="child", status="running", activity="tool", cwd="/w"),
            ],
        )


class SendMessageTest(unittest.TestCase):
    def _send(self, to: object, *replies: object) -> AsyncMock:
        host = _host(*replies, {"deliveryStatus": "queued", "id": "m1"})
        with patch.object(rlm_module, "host_request", host), patch.object(rlm_module, "emit"):
            receipt = asyncio.run(rlm_module.rlm.send_message(to, "look at x"))
        self.assertEqual(receipt, {"deliveryStatus": "queued", "id": "m1"})
        return host

    def _sent(self, host: AsyncMock) -> dict[str, object]:
        call = host.await_args_list[-1]
        self.assertEqual(call.args[0], "agent_message.send")
        return call.args[1]

    def test_parent_needs_no_lookup(self) -> None:
        host = self._send("parent")
        self.assertEqual(host.await_count, 1)
        self.assertEqual(
            self._sent(host), {"message": "look at x", "receiver_role": "parent", "receiver_name": None}
        )

    def test_a_spawn_handle_addresses_the_child(self) -> None:
        handle = rlm_module.RLMSpawnHandle(rlm_child_id="sub-1", name="kid", session_dir=Path("/k"), model="p/m")
        host = self._send(handle)
        self.assertEqual(
            self._sent(host), {"message": "look at x", "receiver_role": "child", "receiver_name": "kid"}
        )

    def test_a_name_prefers_the_child_over_a_sibling(self) -> None:
        host = self._send("scout", FAMILY)
        self.assertEqual(host.await_args_list[0].args, ("agent_observe.list",))
        self.assertEqual(
            self._sent(host), {"message": "look at x", "receiver_role": "child", "receiver_name": "scout"}
        )

    def test_a_session_id_resolves_to_its_agent(self) -> None:
        host = self._send("s-scout", FAMILY)
        self.assertEqual(
            self._sent(host), {"message": "look at x", "receiver_role": "sibling", "receiver_name": "scout"}
        )

    def test_an_unknown_name_lists_the_reachable_agents(self) -> None:
        host = _host(FAMILY)
        with patch.object(rlm_module, "host_request", host):
            with self.assertRaisesRegex(ValueError, r"no reachable agent named 'ghost' \(reachable: fixer, lead, scout\)"):
                asyncio.run(rlm_module.send_message("ghost", "hi"))
            with self.assertRaisesRegex(ValueError, "empty"):
                asyncio.run(rlm_module.send_message("lead", "  "))
        self.assertEqual(host.await_count, 1)


if __name__ == "__main__":
    unittest.main()
