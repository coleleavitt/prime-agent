"""The blocking host-request transport of the kernel's synchronous clients.

``rlm.harness`` and ``rlm.factory`` are clients of state and logic the Prime
Agent host owns (the harness store, the factory spec validator, the machine
library). Their APIs are synchronous, so each call is one blocking request.
Inside a serving kernel it is a kernel host request
(``repl.host_request_blocking``); anywhere else (the runtime's own tests,
scripts) it is one run of the host binary's one-shot,
``prime-agent --prime-agent-harness-request`` (stdin: the request, stdout:
the reply), which serves every request that needs no session:
``harness.*``, ``factory.spec`` and ``factory.library``.
"""

from __future__ import annotations

import copy
import json
import math
import os
import subprocess
from collections.abc import Mapping
from pathlib import Path
from typing import Any

# Python values on the wire. A value whose host-side rules are Python
# semantics (a factory spec: a tuple is not a list, True is not an int, a NaN
# float is a number but not finite JSON, a container may contain itself) does
# not travel as plain JSON: it travels as a flat node table -- one tagged
# entry per value, children by index -- whose own nesting is constant.
# Anything JSON cannot spell (a tuple, a set, bytes, any other object, a
# back-reference that closes a cycle, a container nested past the encoding
# bound) becomes an opaque leaf carrying its repr, truthiness, type name, and
# json.dumps spelling if it has one; a value handed back (a canonical
# machine's passthrough fields) decodes an opaque leaf to a deep copy of the
# original object.

_ENCODE_DEPTH_CAP = 320
"""Containers deeper than this become opaque leaves: past the factory
validator's guard-value bound (256 below a guard's own position in a spec)
nothing the host reads can change, and the host rebuilds a bounded tree."""

def _opaque_node(value: Any, registry: list[Any]) -> list[Any]:
    registry.append(value)
    try:
        text = repr(value)
    except Exception:  # noqa: BLE001 - a hostile __repr__ must not break validation
        text = f"<{type(value).__name__} object>"
    try:
        truthy = bool(value)
    except Exception:  # noqa: BLE001 - same for __bool__/__len__
        truthy = True
    try:
        # What the machine renderer prints for a leaf JSON can still spell
        # (a tuple): the encoder's own spelling.
        spelled: str | None = json.dumps(value)
    except Exception:  # noqa: BLE001 - anything else has no spelling
        spelled = None
    return ["o", len(registry) - 1, text, truthy, type(value).__name__, spelled]


def encode_value(value: Any) -> "tuple[dict[str, Any], list[Any]]":
    """Encode one Python value as the host's node table plus the registry of
    opaque originals. Iterative (no recursion), pre-order: a node's slot is
    reserved before its children, so every child index exceeds its parent's.
    Cycle detection is per branch (a shared-but-acyclic object encodes once
    per occurrence, like the validator's ancestry check)."""
    nodes: list[Any] = [None]
    registry: list[Any] = []
    active: set[int] = set()
    # Work items: ("visit", value, slot, depth) or ("exit", id).
    stack: list[tuple[Any, ...]] = [("visit", value, 0, 0)]
    while stack:
        item = stack.pop()
        if item[0] == "exit":
            active.discard(item[1])
            continue
        _, current, slot, depth = item
        if current is None:
            nodes[slot] = ["n"]
        elif isinstance(current, bool):
            nodes[slot] = ["b", current]
        elif isinstance(current, int):
            nodes[slot] = ["i", str(int(current))]
        elif isinstance(current, float):
            if math.isnan(current):
                nodes[slot] = ["f", "nan"]
            elif math.isinf(current):
                nodes[slot] = ["f", "inf" if current > 0 else "-inf"]
            else:
                nodes[slot] = ["f", float(current)]
        elif isinstance(current, str):
            nodes[slot] = ["s", str(current)]
        elif isinstance(current, (list, dict)):
            if id(current) in active or depth > _ENCODE_DEPTH_CAP:
                nodes[slot] = _opaque_node(current, registry)
                continue
            active.add(id(current))
            stack.append(("exit", id(current)))
            pending: list[tuple[Any, ...]] = []
            if isinstance(current, list):
                children = []
                for child in current:
                    children.append(len(nodes))
                    nodes.append(None)
                    pending.append(("visit", child, children[-1], depth + 1))
                nodes[slot] = ["l", children]
            else:
                pairs = []
                for key, child in current.items():
                    key_slot = len(nodes)
                    nodes.append(None)
                    value_slot = len(nodes)
                    nodes.append(None)
                    pairs.append([key_slot, value_slot])
                    pending.append(("visit", key, key_slot, depth + 1))
                    pending.append(("visit", child, value_slot, depth + 1))
                nodes[slot] = ["d", pairs]
            stack.extend(reversed(pending))
        else:
            nodes[slot] = _opaque_node(current, registry)
    return {"nodes": nodes, "root": 0}, registry


def decode_value(table: Any, registry: list[Any]) -> Any:
    """Rebuild a host node table into Python values (children first, so the
    pass is iterative); an opaque leaf decodes to a deep copy of the
    registry's original."""
    if not isinstance(table, dict) or not isinstance(table.get("nodes"), list):
        raise RuntimeError("the host returned an invalid value table")
    nodes = table["nodes"]
    built: list[Any] = [None] * len(nodes)
    for index in range(len(nodes) - 1, -1, -1):
        node = nodes[index]
        tag = node[0]
        if tag == "n":
            built[index] = None
        elif tag in ("b", "s"):
            built[index] = node[1]
        elif tag == "i":
            built[index] = int(node[1])
        elif tag == "f":
            built[index] = float(node[1])
        elif tag == "l":
            built[index] = [built[child] for child in node[1]]
        elif tag == "d":
            built[index] = {built[key]: built[value] for key, value in node[1]}
        elif tag == "o":
            built[index] = copy.deepcopy(registry[node[1]])
        else:
            raise RuntimeError(f"the host returned an unknown value tag {tag!r}")
    return built[table.get("root", 0)]


# The hidden flag of the host binary that serves one request outside a kernel.
ONE_SHOT_FLAG = "--prime-agent-harness-request"
# Where the host exports its own binary to a kernel.
EXECUTABLE_ENV = "PRIME_AGENT_EXECUTABLE"
# An explicit host binary for a runtime with no serving host (an installed
# runtime pointed at a build).
HOST_BINARY_ENV = "PRIME_AGENT_HOST_BINARY"


def _env_value(name: str) -> str | None:
    # Set-but-empty values behave as unset.
    value = (os.environ.get(name) or "").strip()
    return value or None


def checkout_host() -> Path | None:
    """The host binary of the source checkout this runtime runs from, if built."""
    name = "prime-agent.exe" if os.name == "nt" else "prime-agent"
    for parent in Path(__file__).resolve().parents:
        if (parent / "Cargo.toml").is_file() and (parent / "crates").is_dir():
            for profile in ("debug", "release"):
                candidate = parent / "target" / profile / name
                if candidate.is_file():
                    return candidate
            return None
    return None


def export_checkout_host() -> None:
    """Run from a source checkout without a host-exported binary, export the
    checkout's build the way the host exports its own to a kernel, so the
    Python processes this one starts reach the same host."""
    if _env_value(EXECUTABLE_ENV) is None and (checkout := checkout_host()) is not None:
        os.environ[EXECUTABLE_ENV] = str(checkout)


def host_executable(client: str) -> str:
    """The host binary that serves a request outside a kernel."""
    configured = _env_value(EXECUTABLE_ENV) or _env_value(HOST_BINARY_ENV)
    if configured:
        return configured
    if (checkout := checkout_host()) is not None:
        return str(checkout)
    raise RuntimeError(
        f"{client} needs the Prime Agent host: call it inside a Prime Agent kernel, "
        + f"or set {EXECUTABLE_ENV} to the prime-agent binary"
    )


def _one_shot(message: Mapping[str, object], client: str, timeout_s: float | None) -> object:
    executable = host_executable(client)
    try:
        completed = subprocess.run(
            [executable, ONE_SHOT_FLAG],
            input=json.dumps(message, ensure_ascii=False, allow_nan=False),
            capture_output=True,
            text=True,
            encoding="utf-8",
            timeout=timeout_s,
            check=False,
        )
    except OSError as err:
        raise RuntimeError(f"{client} could not start the host {executable}: {err}") from err
    if completed.returncode != 0:
        detail = completed.stderr.strip() or f"exit code {completed.returncode}"
        raise RuntimeError(f"{client} host request failed: {detail}")
    try:
        reply: object = json.loads(completed.stdout)
    except ValueError as err:
        raise RuntimeError(f"{client} host returned an invalid reply: {err}") from err
    return reply


def request(message: Mapping[str, object], *, client: str, timeout_s: float | None = None) -> object:
    """Send one typed request (``message["type"]``) and return the handler's
    result; a failed request raises ``RuntimeError``."""
    from . import repl

    if not repl.is_active():
        return _one_shot(message, client, timeout_s)
    request_type = message.get("type")
    reply: Mapping[str, object] = repl.host_request_blocking(message, timeout_s=timeout_s)
    status = reply.get("status")
    if status == "error":
        raise RuntimeError(str(reply.get("error") or f"host request {request_type} failed"))
    if status != "ok":
        raise RuntimeError(f"host request {request_type} returned unexpected status: {status!r}")
    return reply.get("result")


export_checkout_host()
