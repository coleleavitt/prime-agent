"""Agent-to-agent messaging in Claude Code's shape: ``list_agents()`` (ListAgents)
and ``send_message(to, message)`` (SendMessage).

Both are thin clients of the daemon's family routing: ``agent_observe.list``
answers the reachable agents (parent, siblings, direct children; every root
session on the daemon is a sibling of every other), ``agent_message.send``
delivers. A running target reads the message at its next tool boundary; an
idle or finished one is woken by it and continues in the same session and
context. Sender identity is always the daemon's, never the caller's.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any

_MESSAGE_DISPLAY_MIME = "application/vnd.prime-agent.agent-message+json"
_RELATIONSHIPS = ("parent", "child", "sibling")


@dataclass(frozen=True)
class RLMAgent:
    """One reachable agent: ``relationship`` is ``parent`` | ``sibling`` |
    ``child``; ``status`` is ``running`` | ``idle`` | ``inactive`` (no live
    session in the daemon; a message wakes it)."""

    name: str
    relationship: str
    status: str
    active_session_id: str | None = None
    session_id: str | None = None
    activity: str | None = None
    cwd: str | None = None


def _agent_from_row(row: Any) -> RLMAgent | None:
    if not isinstance(row, dict):
        raise RuntimeError("rlm.list_agents received an invalid agent row")
    relationship = row.get("relationship")
    if relationship not in _RELATIONSHIPS:
        # The calling session itself (or a row outside the family) is not a target.
        return None
    name = row.get("sessionName") or row.get("sessionId") or row.get("activeSessionId")
    if not isinstance(name, str) or not name:
        raise RuntimeError("rlm.list_agents received an agent row without a name")

    def optional(key: str) -> str | None:
        value = row.get(key)
        return value if isinstance(value, str) and value else None

    status = row.get("status")
    return RLMAgent(
        name=name,
        relationship=relationship,
        status=status if isinstance(status, str) else "inactive",
        active_session_id=optional("activeSessionId"),
        session_id=optional("sessionId"),
        activity=optional("activity"),
        cwd=optional("cwd"),
    )


async def list_agents() -> list[RLMAgent]:
    """List the agents this session can message: its parent, siblings and
    direct children, live or not."""
    from . import host_request

    payload = await host_request("agent_observe.list")
    rows = payload.get("agents")
    if not isinstance(rows, list):
        raise RuntimeError("rlm.list_agents received an invalid agents list")
    agents = [_agent_from_row(row) for row in rows]
    return [agent for agent in agents if agent is not None]


def _target_name(to: Any) -> tuple[str | None, str]:
    """``(relationship or None, name)`` for a typed target; a bare string
    resolves against the family later."""
    from . import RLMSpawnHandle, RLMSubagent

    if isinstance(to, RLMAgent):
        return to.relationship, to.name
    if isinstance(to, RLMSpawnHandle):
        return "child", to.name
    if isinstance(to, RLMSubagent):
        return "child", to.session_name
    if isinstance(to, str) and to.strip():
        return None, to.strip()
    raise TypeError(
        f"to must be an agent name, 'parent', RLMAgent, RLMSpawnHandle or RLMSubagent, got {type(to).__name__}"
    )


def _resolve(name: str, agents: list[RLMAgent]) -> RLMAgent:
    matches = [
        agent for agent in agents if name in (agent.name, agent.session_id, agent.active_session_id)
    ]
    # A child shadows a sibling of the same name: the nearer agent wins.
    for relationship in ("child", "sibling", "parent"):
        for agent in matches:
            if agent.relationship == relationship:
                return agent
    reachable = ", ".join(sorted({agent.name for agent in agents})) or "none"
    raise ValueError(f"no reachable agent named {name!r} (reachable: {reachable})")


def _emit_sent(receipt: dict[str, Any], relationship: str) -> None:
    try:
        from . import emit

        display = dict(receipt)
        display["receiverRole"] = relationship
        emit(
            {
                _MESSAGE_DISPLAY_MIME: display,
                "text/plain": "Agent message queued"
                if receipt.get("deliveryStatus") == "queued"
                else "Agent message sent",
            }
        )
    except Exception:
        pass


async def send_message(to: Any, message: str) -> dict[str, Any]:
    """Send ``message`` to one reachable agent and return the delivery receipt.

    ``to`` is ``"parent"``, an agent name (a direct child first, then a
    sibling, matching session names and ids), an ``RLMAgent`` from
    ``list_agents()``, or a child's spawn handle / ``list_subagents()`` row.
    A running target reads the message at its next tool boundary
    (``deliveryStatus`` ``queued``); an idle or finished one starts a turn
    on it in the same session and context (``delivered``); a digest-lane
    target stores it (``digest``).
    """
    from . import host_request

    if not isinstance(message, str):
        raise TypeError(f"message must be str, got {type(message).__name__}")
    if not message.strip():
        raise ValueError("message must not be empty")
    relationship, name = _target_name(to)
    if relationship is None and name == "parent":
        relationship = "parent"
    if relationship is None:
        resolved = _resolve(name, await list_agents())
        relationship, name = resolved.relationship, resolved.name
    payload: dict[str, Any] = {"message": message, "receiver_role": relationship}
    payload["receiver_name"] = None if relationship == "parent" else name
    receipt = await host_request("agent_message.send", payload)
    _emit_sent(receipt, relationship)
    return receipt
