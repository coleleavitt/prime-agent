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

import json
import os
import subprocess
from collections.abc import Mapping
from pathlib import Path

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
