"""Prime Agent advisor skill: ask a stronger reviewer model for guidance.

The advisor runs host-side; it reads this session's conversation, so the
question only needs what the conversation does not already say. This is a
thin wrapper over the generic host bridge (`rlm.host_request`); it only works
inside the Prime Agent Python kernel.
"""

from __future__ import annotations

from typing import Any

from rlm import host_request


async def ask(question: str) -> str:
    """Ask the advisor `question` and return its guidance as text.

    The advisor sees the task, your code cells and their results, and your
    replies so far. Raises when the question is empty or longer than 8000
    characters, or when the advisor model fails.
    """
    if not isinstance(question, str):
        raise TypeError(f"question must be str, got {type(question).__name__}")
    reply: dict[str, Any] = await host_request("advisor.ask", {"question": question})
    return str(reply.get("advice", ""))
