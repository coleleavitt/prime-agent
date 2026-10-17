from __future__ import annotations

import asyncio
import importlib
import unittest
from unittest.mock import AsyncMock, patch

rlm_module = importlib.import_module("rlm")


class RlmLoadSkillTest(unittest.TestCase):
    def test_sends_the_name_and_args_and_returns_the_text(self) -> None:
        reply = {
            "name": "release",
            "text": "Base directory for this skill: /repo/.claude/skills/release\n\nRelease 1.2.",
            "location": "/repo/.claude/skills/release/SKILL.md",
            "baseDir": "/repo/.claude/skills/release",
        }
        host_request = AsyncMock(return_value=reply)
        with patch.object(rlm_module, "host_request", host_request):
            text = asyncio.run(rlm_module.rlm.load_skill("release", args="1.2"))
            plain = asyncio.run(rlm_module.load_skill("release"))

        self.assertEqual(text, reply["text"])
        self.assertEqual(plain, reply["text"])
        self.assertEqual(
            host_request.await_args_list[0].args,
            ("skill.load", {"name": "release", "args": "1.2"}),
        )
        self.assertEqual(
            host_request.await_args_list[1].args,
            ("skill.load", {"name": "release", "args": ""}),
        )

    def test_refuses_bad_input_before_asking_the_host(self) -> None:
        host_request = AsyncMock()
        with patch.object(rlm_module, "host_request", host_request):
            with self.assertRaises(TypeError):
                asyncio.run(rlm_module.load_skill(""))
            with self.assertRaises(TypeError):
                asyncio.run(rlm_module.load_skill("x", args=["a"]))  # type: ignore[arg-type]
        host_request.assert_not_awaited()

    def test_a_reply_without_text_is_an_error(self) -> None:
        host_request = AsyncMock(return_value={"name": "x"})
        with patch.object(rlm_module, "host_request", host_request):
            with self.assertRaises(RuntimeError):
                asyncio.run(rlm_module.load_skill("x"))


if __name__ == "__main__":
    unittest.main()
