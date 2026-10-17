"""Scheduled prompts (Claude Code's CronCreate, CronList, CronDelete and
ScheduleWakeup).

``rlm.cron.create(cron, prompt, recurring=True, durable=False)`` schedules
a prompt on a standard five-field cron in the user's local time zone
(minute hour day-of-month month day-of-week); ``recurring=False`` fires
once at the next match and then deletes itself. ``rlm.cron.list()`` and
``rlm.cron.delete(id)`` manage them. ``rlm.schedule_wakeup(delay_seconds,
prompt, reason)`` paces a self-paced ``/loop``: the host re-invokes the
session with ``prompt`` after the delay (clamped to [60, 3600]);
``schedule_wakeup(stop=True)`` ends the loop.

The host owns the schedule (the daemon keeps it while no client is
attached) and only fires while the session is idle, never mid-turn.
Recurring tasks expire after seven days. ``durable=True`` keeps a task in
the session's ``scheduled_tasks.json`` so it survives a daemon restart and
resumes with the session; use it only when the user asks for that.

Every call returns a dict whose ``text`` is the line to read.
"""

from __future__ import annotations

import math
from typing import Any


async def _request(request_type: str, payload: dict[str, Any]) -> dict[str, Any]:
    from . import host_request

    reply = await host_request(request_type, payload)
    if not isinstance(reply, dict):
        raise RuntimeError(f"{request_type} returned {type(reply).__name__}, expected a dict")
    return reply


class _CronNamespace:
    """``rlm.cron``: schedule, list and cancel prompts."""

    async def create(
        self,
        cron: str,
        prompt: str,
        recurring: bool = True,
        durable: bool = False,
    ) -> dict[str, Any]:
        """Schedule ``prompt`` on ``cron`` (five fields, local time).

        Returns ``{"id", "human_schedule", "recurring", "durable",
        "next_fire_at", "text"}`` (``next_fire_at`` in epoch ms).
        """
        if not isinstance(cron, str):
            raise TypeError(f"cron must be str, got {type(cron).__name__}")
        if not isinstance(prompt, str) or not prompt.strip():
            raise ValueError("prompt must be a non-empty string")
        if not isinstance(recurring, bool) or not isinstance(durable, bool):
            raise TypeError("recurring and durable must be booleans")
        return await _request(
            "rlm.cron.create",
            {"cron": cron, "prompt": prompt, "recurring": recurring, "durable": durable},
        )

    async def list(self) -> list[dict[str, Any]]:
        """Every scheduled task and pending loop wakeup in this session:
        ``id``, ``cron``, ``human_schedule``, ``prompt``, ``kind`` (``cron``,
        ``loop`` or ``wakeup``), ``recurring``, ``durable``, ``next_fire_at``."""
        reply = await _request("rlm.cron.list", {})
        jobs = reply.get("jobs")
        return list(jobs) if isinstance(jobs, list) else []

    async def delete(self, id: str) -> dict[str, Any]:  # noqa: A002 - Claude Code's parameter name
        """Cancel a task (or a pending loop wakeup) by id."""
        if not isinstance(id, str) or not id.strip():
            raise ValueError("id must be a non-empty string")
        return await _request("rlm.cron.delete", {"id": id})

    def __repr__(self) -> str:
        return "<rlm.cron: create(cron, prompt, recurring=True, durable=False), list(), delete(id)>"


cron = _CronNamespace()


async def schedule_wakeup(
    delay_seconds: float | None = None,
    prompt: str | None = None,
    reason: str | None = None,
    *,
    noop: bool | None = None,
    stop: bool = False,
) -> dict[str, Any]:
    """Schedule the next tick of a self-paced ``/loop``, or end it.

    ``delay_seconds`` is clamped to [60, 3600]; ``prompt`` is the ``/loop``
    input verbatim (prefixed with ``/loop``), or the
    ``<<autonomous-loop-dynamic>>`` sentinel; ``reason`` is one short,
    specific sentence the user sees; ``noop=True`` marks a tick that changed
    nothing. ``stop=True`` (alone) cancels the pending wakeup.
    """
    if stop:
        return await _request("rlm.schedule_wakeup", {"stop": True})
    payload: dict[str, Any] = {}
    if delay_seconds is not None:
        if isinstance(delay_seconds, bool) or not isinstance(delay_seconds, (int, float)):
            raise TypeError("delay_seconds must be a number")
        if not math.isfinite(delay_seconds):
            raise ValueError("delay_seconds must be finite")
        payload["delay_seconds"] = float(delay_seconds)
    if prompt is not None:
        payload["prompt"] = prompt
    if reason is not None:
        payload["reason"] = reason
    if noop is not None:
        payload["noop"] = bool(noop)
    return await _request("rlm.schedule_wakeup", payload)
