from __future__ import annotations

import asyncio
import importlib
import unittest
from unittest.mock import AsyncMock, patch


rlm_module = importlib.import_module("rlm")


class RlmMessagingStatsTest(unittest.TestCase):
    def test_reads_the_snapshot_from_the_host(self) -> None:
        snapshot = {
            "arrivals": {"total": 2, "last5m": 1},
            "model_steps": {"total": 5, "last5m": 2, "tokens": 4000},
            "ingestion_steps": {"total": 2, "last5m": 1, "tokens": 1600},
            "context": {
                "estimated_agent_message_tokens": 100,
                "context_tokens": 800,
                "share": 0.125,
            },
            "sends": {"attempts": 3, "failures": 1},
        }
        host_request = AsyncMock(return_value=snapshot)

        with patch.object(rlm_module, "host_request", host_request):
            result = asyncio.run(rlm_module.rlm.messaging_stats())

        self.assertEqual(result, snapshot)
        host_request.assert_awaited_once_with("rlm.messaging_stats")


if __name__ == "__main__":
    unittest.main()
