"""Adoption events through the host's ``telemetry.emit`` bridge.

The host validates every event against its catalog and the bridge's allowlist,
so only primitive properties of catalogued events get through.
"""

from __future__ import annotations

import uuid


def emit(name: str, **properties: str | int | bool) -> None:
    """Send one event without waiting for the reply; outside a kernel nothing is sent."""
    from . import repl

    if repl.is_active():
        repl._send(
            {
                "event": "host_request",
                "id": uuid.uuid4().hex,
                "data": {"type": "telemetry.emit", "name": name, "properties": properties},
            }
        )
