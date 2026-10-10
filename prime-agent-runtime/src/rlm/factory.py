"""The kernel's ``rlm.factory`` client: factory specs and runs.

A continual-harness ``factory`` entry stores a declarative state machine of
subagent states in ``arguments["machine"]``: entry states (which declare
no inputs), guarded transitions between states, and bounded re-entry
(``max_entries``). The original DAG form in ``arguments["dag"]`` stays as
sugar: it compiles to machine form (each node becomes a state entered
once; a node's full effective dependency set compiles to ONE join
transition that waits for every predecessor). A state carrying a ``wait``
block is rejected at write time until the watch host handlers exist.

The factory itself runs in the Prime Agent host (``pa_core::factory``):
the write-time validator and dag compiler (``validate_factory_spec``,
``canonicalize_factory_spec`` and friends are thin clients over it), the
machine library (``MACHINE.md`` parse, render, list, resolve, import,
export: thin clients over ``pa_core::factory::library``), and the executor
(``FactoryExecutor`` and the ``rlm.factory`` namespace:
run/status/stop/resume/graph/watch), which admits states through the
session's ``rlm.spawn`` path, owns the run state host-side, and writes a
durable record per run. A kernel restart or crash never touches a running
workflow; a host restart pauses the runs that were in flight as
interrupted, and ``rlm.factory.resume(run_id)`` continues them. This
module keeps what only the kernel can do: encode the Python values a call
carries (and decode the ones handed back), name the harness store and the
library directories a call resolves against, and the opt-in gate.

The full agent-facing reference — authoring rules, guards/joins/cycles,
foreach, budgets, stall detectors, and the ``rlm.factory`` API with worked
examples — is embedded in this module as ``FACTORY_HELP``;
``rlm.factory.help()`` returns it with no filesystem resolution, so
packaged kernels (where the repo layout is not adjacent) see the same
guide.

The namespace is opt-in: while the ``factory.enabled`` setting is off (the
default; the user turns it on with ``/factory on``), every ``rlm.factory``
call except ``help()`` and every factory harness write refuses with one
clean message (``FACTORY_DISABLED_MESSAGE``), never a crash.
"""

from __future__ import annotations

import json
import math
import os
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable

from . import _host
from ._state_guard import guard_state_path


def _is_number(value: Any) -> bool:
    """True for real numbers; booleans are not accepted as numbers."""
    return isinstance(value, (int, float)) and not isinstance(value, bool)


#: The host's cap on one spawn name (``pa_core::factory::labels``): a
#: configured inline subagent ``name`` longer than this is rejected at
#: write time instead of failing every spawn admission.
SUBAGENT_NAME_MAX_LENGTH = 64

MAX_GUARD_VALUE_DEPTH = 256
"""Nesting bound on one guard comparison value (``when.value``), enforced by
the host validator: a container nested deeper is not finite JSON data."""


# ---------------------------------------------------------------------------
# The spec client: validation, compilation, and canonicalization run in the
# Prime Agent host (one implementation, ``pa_core::factory::spec``); these
# functions ship the spec there and return its verdict.
#
# The validator's rules are Python semantics (a tuple is not a list, True is
# not an int, a NaN float is a number but not finite JSON), so a spec travels
# as a node table (``rlm._host.encode_value``), and a value handed back (a
# canonical machine's passthrough fields) decodes against the call's own
# registry of the originals.
#
# Transport: one blocking host request (``factory.spec``: harness writes call
# the validator synchronously inside a cell), in a serving kernel or through
# the host binary's one-shot outside one (``rlm._host``).
# ---------------------------------------------------------------------------

HOST_BINARY_ENV = _host.HOST_BINARY_ENV
# The node-table encoding of Python values (``rlm._host``), under the names
# the spec client always used.
_encode_value = _host.encode_value
_decode_value = _host.decode_value


def _dev_host_binary() -> "str | None":
    """The checkout's own build of the host, for a runtime imported from a
    source tree (an installed runtime is pointed at it through
    ``HOST_BINARY_ENV``)."""
    checkout = _host.checkout_host()
    return None if checkout is None else str(checkout)


def _spec_op(op: str, value: Any) -> "tuple[dict[str, Any], list[Any]]":
    """Run one spec operation in the host; returns the reply and the opaque
    registry the reply's value tables decode against."""
    table, registry = _encode_value(value)
    reply = _host.request({"type": "factory.spec", "op": op, "value": table}, client="rlm.factory", timeout_s=30.0)
    if not isinstance(reply, dict):
        raise RuntimeError("factory.spec returned an invalid reply")
    return reply, registry


def _spec_errors(reply: dict[str, Any]) -> list[str]:
    errors = reply.get("errors")
    if not isinstance(errors, list) or not all(isinstance(error, str) for error in errors):
        raise RuntimeError("factory.spec returned an invalid error list")
    return errors


def validate_factory_machine(machine: Any) -> list[str]:
    """Dry-run validation for a machine-form factory spec.

    Returns a list of human-readable error sentences; an empty list means
    the machine is valid. Rules: states are 1..1024 with unique slug ids and
    at least one entry state; every state requires a subagent and entry
    states declare no inputs; resident states declare no outputs, foreach,
    or outgoing transitions; wait blocks are rejected (the watch host
    handlers arrive with the communication series); transitions reference
    existing states (self-loops are legal re-entry) and may carry one guard
    over the from-state's latest settle output; a transition with a LIST of
    from-states is a join that fires once every source settled (guards are
    single-source only). There is no acyclicity requirement: arbitrary
    state machines, including cycles, validate.
    """
    reply, _ = _spec_op("validate_machine", machine)
    return _spec_errors(reply)


def compile_factory_dag(dag: Any) -> "tuple[dict[str, Any] | None, list[str]]":
    """Compile a dag-form spec into machine form.

    Returns ``(machine, errors)``: on success the machine is a spec-shaped
    dict (defaults are applied later by ``canonicalize_factory_spec``) and the
    error list is empty; on any dag-level error the machine is ``None`` and
    the errors carry the V1 dag wording. Each node becomes a state with
    ``entry`` set when it has no effective dependencies and ``max_entries``
    1; the node's full effective dependency set becomes ONE guard-less join
    transition (a single dependency stays a plain ``from`` string; several
    become a ``from`` list).
    """
    reply, registry = _spec_op("compile_dag", dag)
    errors = _spec_errors(reply)
    machine = reply.get("machine")
    return (None if machine is None else _decode_value(machine, registry)), errors


def validate_factory_spec(spec: Any) -> list[str]:
    """Dry-run validation for a factory spec in either form.

    Detects the form first: a spec carrying "states" or "transitions" is
    machine form; anything else is dag form and compiles to machine form
    first. A spec carrying both dag and machine keys is rejected outright.
    Returns a list of human-readable error sentences; an empty list means
    the specification is valid. Every rule is enforced before a factory entry
    is stored, so an invalid spec never reaches the store.
    """
    reply, _ = _spec_op("validate_spec", spec)
    return _spec_errors(reply)


def canonicalize_factory_spec(spec: Any) -> dict[str, Any]:
    """Validate a spec in either form and return the canonical MACHINE form.

    Raises ``ValueError`` with the joined error list when the spec is
    invalid (including the both-forms rejection). Dag specs compile to
    machine form first, so the executor sees one shape:
    ``{"run": ..., "states": [...], "transitions": [...]}`` with defaults
    applied (run failure_policy 'escalate', max_parallel 8, max_transitions
    10 per state capped at 10000, max_children 10000; state entry False,
    max_entries 1, lifecycle 'task', retries 0, failure_policy from the run;
    transition on 'settled').
    """
    reply, registry = _spec_op("canonicalize", spec)
    if "error" in reply:
        raise ValueError(reply["error"])
    return _decode_value(reply.get("machine"), registry)


def topological_order(nodes: list[dict[str, Any]]) -> list[str]:
    """Return node ids in a dependency-respecting order.

    Edges are the effective dependencies: ``depends_on`` plus every
    ``inputs[].from`` source node. Raises ``ValueError`` on a duplicate id,
    an unknown dependency, or a cycle. The order is stable: among ready
    nodes, input order wins. Retained as a public helper for inspecting
    dag-form specs; the machine form has no acyclicity requirement.
    """
    reply, _ = _spec_op("topological_order", nodes)
    if "error" in reply:
        raise ValueError(reply["error"])
    order = reply.get("order")
    if not isinstance(order, list):
        raise RuntimeError("factory.spec returned an invalid order")
    return order


__all__ = [
    "FACTORY_DISABLED_MESSAGE",
    "FACTORY_HELP",
    "FactoryExecutor",
    "MachineFile",
    "MachineResolutionError",
    "canonicalize_factory_spec",
    "cli_dispatch",
    "compile_factory_dag",
    "default_factory_executor",
    "export_factory_spec",
    "export_library_machine",
    "export_machine",
    "factory_enabled",
    "import_machine",
    "list_machines",
    "machine_library_dirs",
    "parse_machine_file",
    "render_machine_file",
    "repo_machines_dir",
    "require_factory_enabled",
    "resolve_machine",
    "resume_factory",
    "run_factory",
    "status_factory",
    "stop_factory",
    "topological_order",
    "user_machines_dir",
    "validate_factory_machine",
    "validate_factory_spec",
]


# ---------------------------------------------------------------------------
# The executor client: runs live in the Prime Agent host.
#
# The executor (``pa_core::factory::executor``) runs host-side, one per
# session, with a durable record per run: a kernel restart or crash never
# touches a running workflow, and the next kernel reads the same runs
# through these calls. The harness store resolves a run's stored factory
# entry (else the library machine a template run names) and the subagents
# its states reference in one ``harness.resolve_factory`` request against
# the store this kernel's ``rlm.harness`` names; validation,
# canonicalization, admission, transitions, budgets, and every report are
# the executor's.
# ---------------------------------------------------------------------------

WATCH_TIMEOUT_CAP_SECONDS = 60.0
"""Upper bound on one ``factory.watch`` timeout (seconds)."""


def _executor_result(request_type: str, reply: Any) -> Any:
    """One executor reply: the result, or the host's refusal as ValueError."""
    if not isinstance(reply, dict):
        raise RuntimeError(f"host request {request_type} returned an invalid reply")
    if "error" in reply:
        raise ValueError(reply["error"])
    return reply.get("result")


async def _executor_call(request_type: str, payload: dict[str, Any]) -> Any:
    from . import host_request

    return _executor_result(request_type, await host_request(request_type, payload))


def _executor_call_blocking(request_type: str, payload: dict[str, Any]) -> Any:
    """A synchronous executor read (``graph``, ``export_machine``'s run
    lookup) through the serving kernel's blocking host request."""
    from . import repl

    if not repl.is_active():
        raise RuntimeError(
            f"{request_type} needs the Prime Agent host: the factory executor runs there, "
            "and this process is not a serving kernel"
        )
    reply = _host.request({**payload, "type": request_type}, client="rlm.factory", timeout_s=30.0)
    return _executor_result(request_type, reply)


def _entry_spec(entry: Any) -> Any:
    """A stored factory entry's spec: ``machine``, else ``dag``."""
    arguments = entry.arguments if isinstance(entry.arguments, dict) else {}
    spec = arguments.get("machine")
    if spec is None:
        spec = arguments.get("dag")
    return spec


class FactoryExecutor:
    """The kernel's client over the host's factory executor.

    Runs live in the Prime Agent host (one executor per session, a durable
    record per run): they survive kernel restarts and crashes, and a host
    restart pauses an in-flight run as interrupted for
    ``await rlm.factory.resume(run_id)``. The harness store behind
    ``harness`` (default: the session's ``rlm.harness``) resolves the
    stored factory entry and its subagents in one request
    (``harness.resolve_factory``), and the client ships them to the
    executor, which validates,
    canonicalizes, admits children through the session's ``rlm.spawn``
    path, and owns every report. ``now`` and ``sleep`` are accepted for
    compatibility and ignored: the host owns the clock.

    Machine semantics: admission enters every entry state; each settle is
    queued and its outgoing transitions evaluated once -- every guard that
    passes fires (fan-out is legal), a fire enters the target unless it is
    out of ``max_entries`` (recorded as ``transition_blocked``), and a
    self-loop or back-edge re-enters its target with freshly re-bound
    inputs. A run completes at quiescence: no state entry in flight and no
    unevaluated settle.
    """

    def __init__(
        self,
        *,
        now: "Callable[[], float] | None" = None,
        sleep: "Callable[[float], Any] | None" = None,
        harness: Any = None,
    ) -> None:
        del now, sleep  # the host owns the clock
        self._harness = harness

    def _resolve_harness(self) -> Any:
        if self._harness is not None:
            return self._harness
        from . import rlm as rlm_namespace

        return rlm_namespace.harness

    async def _start(self, resolved: dict[str, Any], *, name: "str | None", **library: str) -> dict[str, Any]:
        """Start a run from the harness's resolution of it (the spec id,
        the ``{"spec", "subagents"}`` value, a library run's origin)."""
        return await _executor_call("factory.run", {**resolved, "name": name, **library})

    async def run(self, spec_id: str, *, name: str | None = None) -> dict[str, Any]:
        """Validate a stored factory spec and start a run of it.

        The host re-validates and canonicalizes the spec (dag sugar compiles
        to machine form), resolves every state's subagent, and reports ALL
        failures in one ``ValueError``, starting nothing on any failure.
        Admission enters every entry state up to ``max_parallel`` and
        returns; the host's control loop continues the run, so the calling
        model turn ends immediately (nonblocking).
        """
        return await self._start(self._resolve_harness()._resolve_factory(spec_id), name=name)

    async def run_machine(
        self, machine: "MachineFile", *, machine_path: Path | None = None, name: str | None = None
    ) -> dict[str, Any]:
        """Validate a library machine and start a run of it. Library
        machines are templates, so a library run never creates a harness
        entry; the run records the machine's name as its spec id, and the
        result reports ``machine`` (and ``machine_path``)."""
        library = {"machine": machine.name}
        if machine_path is not None:
            library["machine_path"] = str(machine_path)
        resolved = self._resolve_harness()._resolve_factory(machine.name, held=machine.spec)
        return await self._start(resolved, name=name, **library)

    async def status(self, run_id: str) -> dict[str, Any]:
        """State reports, the trailing event window, elapsed time, and usage.
        Each call marks the events the parent has not seen yet
        (``recorded``/``arrived``) ``delivered``. Raises ``ValueError`` for an
        unknown run id."""
        return await _executor_call("factory.status", {"run_id": _run_id(run_id)})

    async def stop(self, run_id: str) -> dict[str, Any]:
        """Cancel every running child of the run and mark it stopped
        (idempotent)."""
        return await _executor_call("factory.stop", {"run_id": _run_id(run_id)})

    async def resume(self, run_id: str) -> dict[str, Any]:
        """Resume a paused run (escalate, budget, max_transitions,
        max_children, or a host-restart interruption). Raises
        ``ValueError`` when the run is not paused."""
        return await _executor_call("factory.resume", {"run_id": _run_id(run_id)})

    def graph(self, ref: str | None = None, *, compact: bool = False) -> dict[str, Any]:
        """One machine's structure fused with live runtime state: a live
        run id returns that run's snapshot, a stored factory spec id the
        static structure, and no ref every live run (plus the newest
        terminal history) as ``{"runs": [...]}``. Raises ``ValueError`` when
        ``ref`` names neither, or the stored spec fails its validation."""
        payload: dict[str, Any] = {"compact": compact}
        if ref is not None:
            if not isinstance(ref, str):
                raise ValueError(f"unknown factory run or spec {ref!r}")
            payload["ref"] = ref
            entry = self._resolve_harness().get("factory", ref)
            if entry is not None:
                payload["spec_id"] = entry.id
                payload["spec"] = _encode_value(_entry_spec(entry))[0]
        return _executor_call_blocking("factory.graph", payload)

    async def watch(
        self, run_id: str, timeout: float = 0.0, *, compact: bool = False
    ) -> dict[str, Any]:
        """Block until the run's state/instance shape changes or the bounded
        ``timeout`` (seconds, capped at ``WATCH_TIMEOUT_CAP_SECONDS``)
        elapses, then return the ``graph()`` snapshot plus ``changed``.
        Raises ``ValueError`` for an unknown run id or a negative, NaN, or
        non-numeric timeout."""
        valid = _is_number(timeout) and not math.isnan(float(timeout))
        seconds = min(float(timeout), WATCH_TIMEOUT_CAP_SECONDS) if valid else None
        return await _executor_call(
            "factory.watch", {"run_id": _run_id(run_id), "timeout": seconds, "compact": compact}
        )


def _run_id(run_id: Any) -> str:
    """A run id the host can look up; anything else is an unknown run."""
    if not isinstance(run_id, str):
        raise ValueError(f"unknown factory run {run_id!r}")
    return run_id

# ---------------------------------------------------------------------------
# The opt-in gate: `factory.enabled` in the agent-dir settings file.
# ---------------------------------------------------------------------------

#: The single refusal every gated factory call raises while the setting is
#: off. One exact message, so agents and tests can pin the refusal.
FACTORY_DISABLED_MESSAGE = "the factory is disabled; run /factory on to enable it"

_SETTINGS_FILE_NAME = "settings.json"


def factory_enabled() -> bool:
    """Read the ``factory.enabled`` opt-in setting (default off).

    The factory is opt-in: it ships disabled, and the user turns it on with
    ``/factory on`` (the persisted setting is ``factory.enabled`` in the
    agent dir's ``settings.json`` -- the same nested-camelCase document the
    daemon and TUI settings surface write, e.g. ``{"factory": {"enabled":
    true}}`` beside ``compaction``/``agentTraces``). The read mirrors the
    lenient settings loading on the Rust side: a missing file or key, a
    wrong-typed value, or a corrupt document all read as unset, and unset
    means disabled -- the opt-in default is fail-closed, so an unreadable
    settings file refuses the factory instead of silently enabling it.
    """
    # One home for the agent-dir resolution (harness.py owns it); imported
    # lazily because harness imports this module at its own top.
    from .harness import _agent_dir

    path = _agent_dir() / _SETTINGS_FILE_NAME
    try:
        with open(path, encoding="utf-8") as handle:
            document = json.load(handle)
    except (OSError, ValueError):
        return False
    if not isinstance(document, dict):
        return False
    factory = document.get("factory")
    if not isinstance(factory, dict):
        return False
    return factory.get("enabled") is True


def require_factory_enabled() -> None:
    """Refuse with one clean error while the factory is disabled.

    Every gated surface funnels through here -- the ``rlm.factory``
    namespace calls (``run``/``status``/``stop``/``resume``, and the later
    ``graph``/``watch``) and the factory harness writes -- so the refusal is
    one message at every seam. ``help()`` is deliberately exempt: the
    authoring reference must stay readable before opting in.
    """
    if not factory_enabled():
        raise ValueError(FACTORY_DISABLED_MESSAGE)


_DEFAULT_EXECUTOR: FactoryExecutor | None = None


def default_factory_executor() -> FactoryExecutor:
    """The process-wide executor client behind the ``rlm.factory`` namespace.

    Tests that resolve specs from their own harness assign a
    ``FactoryExecutor(harness=...)`` to ``factory._DEFAULT_EXECUTOR``; the
    namespace then routes through it.
    """
    global _DEFAULT_EXECUTOR
    if _DEFAULT_EXECUTOR is None:
        # The upper-case name is the tests' seam (they assign it), not a constant.
        _DEFAULT_EXECUTOR = FactoryExecutor()  # pyright: ignore[reportConstantRedefinition]
    return _DEFAULT_EXECUTOR



async def run_factory(spec_id: str, *, name: str | None = None) -> dict[str, Any]:
    """Validate a factory spec and start a nonblocking run of it.

    The argument names a stored factory entry (a runtime instance) first;
    when no entry carries that id, it resolves a machine from the library
    (repo directory first, user second) and runs the template directly:
    ``await rlm.factory.run("review-sweep")`` starts the library machine
    without creating a harness entry. Harness entries remain runtime
    instances; machines are templates.
    """
    require_factory_enabled()
    executor = default_factory_executor()
    resolved = executor._resolve_harness()._resolve_factory(spec_id, library=_dirs(None, None))
    return await executor._start(resolved, name=name)


async def status_factory(run_id: str) -> dict[str, Any]:
    """Return state states, the event window, elapsed time, and usage."""
    require_factory_enabled()
    return await default_factory_executor().status(run_id)


async def stop_factory(run_id: str) -> dict[str, Any]:
    """Cancel every running child of the run and mark it stopped."""
    require_factory_enabled()
    return await default_factory_executor().stop(run_id)


async def resume_factory(run_id: str) -> dict[str, Any]:
    """Resume a paused run (escalate, budget, or max_transitions pause)."""
    require_factory_enabled()
    return await default_factory_executor().resume(run_id)


def graph_factory(ref: str | None = None, *, compact: bool = False) -> dict[str, Any]:
    """Return one machine's structure fused with live state (see
    ``FactoryExecutor.graph``): a live run id, a stored spec id, or no ref
    for every live run."""
    require_factory_enabled()
    return default_factory_executor().graph(ref, compact=compact)


async def watch_factory(
    run_id: str, timeout: float = 0.0, *, compact: bool = False
) -> dict[str, Any]:
    """Block until the run's state/instance shape changes or the bounded
    timeout elapses, then return the same fused snapshot ``graph()``
    returns plus ``changed``."""
    require_factory_enabled()
    return await default_factory_executor().watch(run_id, timeout, compact=compact)


# ---------------------------------------------------------------------------
# Machine library: MACHINE.md files (import, export, share).
#
# A MACHINE.md is the shareable unit of the machine library, mirroring the
# SKILL.md/skills conventions: YAML frontmatter (name, description, version,
# author) followed by one fenced ``machine-spec`` block whose payload is a
# JSON factory spec in the exact schema ``validate_factory_spec`` accepts
# (machine form, or dag sugar that compiles to one) -- no new spec parser.
# The library resolves from two levels, repo first, user second:
#
# - repo: the bundled machines shipped INSIDE the runtime package
#   (``src/rlm/machines/<name>/MACHINE.md``, wheel package data, so every
#   installed kernel sees the same seeds a checkout does);
#   ``PRIME_AGENT_MACHINES_DIR`` redirects the level at a team directory.
# - user: ``<agent dir>/machines/<name>/MACHINE.md`` (personal machines).
#
# The library itself runs in the Prime Agent host (``pa_core::factory::
# library``, the same implementation behind ``prime-agent factory``): the
# strict file parser, the renderer, the listing scan, resolution, the import
# gate (the write-time validator: an invalid spec never persists) and the
# exports. These functions are its client: each call is one
# ``factory.library`` request. What stays here is what only the kernel
# knows -- where the library levels are (the runtime package's own
# directory, the kernel's environment), the kernel's working directory the
# paths are relative to, the stored entries and live runs an export can
# name, and the Python encoding of the values a call carries.
# ---------------------------------------------------------------------------

MACHINES_DIR_NAME = "machines"

_LIBRARY_ERRORS: dict[str, type[Exception]] = {
    "ValueError": ValueError,
    "TypeError": TypeError,
    "AttributeError": AttributeError,
    "RecursionError": RecursionError,
}


class MachineResolutionError(ValueError):
    """One library lookup failure, with its kind.

    ``broken`` distinguishes the two outcomes a caller must not blur: the
    name's only carriers are machine files that failed to parse or
    validate (the first file's errors say why) versus no machine carrying
    the name at all.
    """

    def __init__(self, message: str, *, broken: bool) -> None:
        super().__init__(message)
        self.broken = broken


def _library_error(error: Any) -> Exception:
    """The exception a ``factory.library`` refusal names."""
    if not isinstance(error, dict):
        return RuntimeError("factory.library returned an invalid error")
    kind, message = error.get("type"), str(error.get("message", ""))
    if kind == "MachineResolutionError":
        return MachineResolutionError(message, broken=bool(error.get("broken")))
    if kind == "OSError":
        return OSError(error.get("errno"), error.get("strerror"), error.get("filename"))
    if kind == "UnicodeDecodeError":
        start = int(error.get("start", 0))
        data = bytes(start) + bytes(error.get("bytes") or [])
        return UnicodeDecodeError("utf-8", data, start, int(error.get("end", start + 1)), str(error.get("reason")))
    return _LIBRARY_ERRORS.get(str(kind), RuntimeError)(message)


def _library(op: str, **fields: Any) -> Any:
    """One machine-library operation in the host: its result, or the
    exception it raised there."""
    request = {"type": "factory.library", "op": op, "cwd": os.getcwd(), **fields}
    reply = _host.request(request, client="rlm.factory", timeout_s=30.0)
    if not isinstance(reply, dict):
        raise RuntimeError("factory.library returned an invalid reply")
    if reply.get("ok") is not True:
        raise _library_error(reply.get("error"))
    return reply.get("result")


def _table(value: Any) -> dict[str, Any]:
    """One Python value as the host reads it (the spec client's node table)."""
    return _encode_value(value)[0]


def _spec_json(spec: Any) -> dict[str, Any]:
    """The fence text of a spec this process holds: Python's own
    ``json.dumps`` (its spelling of tuples, keys, and floats), or the
    exception it raised, which the host raises where the renderer would."""
    try:
        return {"spec_json": json.dumps(spec, indent=2, ensure_ascii=False)}
    except (TypeError, ValueError, RecursionError) as error:
        return {"spec_json_error": {"type": type(error).__name__, "message": str(error)}}


def machine_name_errors(name: Any) -> list[str]:
    """Name rules mirrored from the skill library (validate_name)."""
    return _library("name_errors", value=_table(name))


def machine_description_errors(description: Any) -> list[str]:
    """Description rules mirrored from the skill library (validate_description).

    One rule is the library's own: the description is one listing row, so
    embedded line breaks are a format error.
    """
    return _library("description_errors", value=_table(description))


@dataclass(frozen=True)
class MachineFile:
    """A parsed MACHINE.md: strict frontmatter plus the machine-spec payload."""

    name: str
    description: str
    version: str
    author: str
    spec: "dict[str, Any]"


def _machine_file(payload: Any) -> "MachineFile | None":
    if payload is None:
        return None
    return MachineFile(
        name=payload["name"],
        description=payload["description"],
        version=payload["version"],
        author=payload["author"],
        spec=_decode_value(payload["spec"], []),
    )


def parse_machine_file(text: str, *, source: str = "machine file") -> "tuple[MachineFile | None, list[str]]":
    """Parse one MACHINE.md. Returns ``(machine, [])`` or ``(None, errors)``.

    This owns the FILE format only (frontmatter, fence, JSON payload); the
    spec stays in the existing validated schema, and the import and run
    gates pass it through ``validate_factory_spec`` separately.
    """
    if not isinstance(text, str):
        # The parser's first step, on a value that is not a str: what it
        # raises there (a bytes object's TypeError, another object's
        # AttributeError) is the call's error.
        text = text.lstrip("\ufeff").replace("\r\n", "\n").replace("\r", "\n")
    reply = _library("parse", text=_table(text), source=_table(f"{source}"))
    return _machine_file(reply["machine"]), reply["errors"]


def render_machine_file(machine: MachineFile) -> str:
    """Render a MachineFile back to canonical MACHINE.md text.

    Byte-stable: the same machine always renders to the same bytes (stable
    formatting for diffs), and ``parse_machine_file`` of the output
    recovers the same machine.
    """
    fields = {key: _table(getattr(machine, key)) for key in ("name", "description", "version", "author")}
    return _library("render", **fields, spec=_table(machine.spec), **_spec_json(machine.spec))


def repo_machines_dir() -> Path:
    """The bundled machine library shipped inside the runtime package.

    An explicit ``PRIME_AGENT_MACHINES_DIR`` wins (a team can point the
    shared level at their own directory); otherwise the library resolves
    relative to this module — ``src/rlm/machines`` in a checkout, exactly
    the wheel-package data a kernel venv installs into
    ``site-packages/rlm/machines`` — so an installed kernel sees the same
    seeds a checkout does, with no source-tree walk-up that could pick up
    a stray directory above an installed venv.
    """
    override = _machine_env_dir("PRIME_AGENT_MACHINES_DIR")
    if override:
        return Path(override).expanduser().resolve()
    return Path(__file__).resolve().parent / MACHINES_DIR_NAME


def _machine_env_dir(name: str) -> str | None:
    # Set-but-empty env values behave as unset (mirrors harness._env_dir).
    value = (os.environ.get(name) or "").strip()
    return value or None


def user_machines_dir() -> Path:
    """The personal machines directory (``<agent dir>/machines``)."""
    raw = (
        _machine_env_dir("PRIME_AGENT_CODING_AGENT_DIR")
        or _machine_env_dir("PI_CODING_AGENT_DIR")
        or str(Path.home() / ".prime" / "agent")
    )
    return guard_state_path("agent dir", Path(raw).expanduser().resolve()) / MACHINES_DIR_NAME


def machine_library_dirs(
    *, repo_dir: "str | Path | None" = None, user_dir: "str | Path | None" = None
) -> "list[tuple[str, Path]]":
    """Library levels in resolution order: repo first, user second.

    Both levels always exist (the repo level is the packaged library;
    the user level is the personal directory under the agent dir); a
    missing directory is simply empty, so listing and resolving skip it.
    """
    repo = Path(repo_dir).expanduser() if repo_dir is not None else repo_machines_dir()
    user = Path(user_dir).expanduser() if user_dir is not None else user_machines_dir()
    return [("repo", repo), ("user", user)]


def _dirs(repo_dir: "str | Path | None", user_dir: "str | Path | None") -> list[list[str]]:
    return [[source, str(path)] for source, path in machine_library_dirs(repo_dir=repo_dir, user_dir=user_dir)]


def _scan_machine_library(
    *, repo_dir: "str | Path | None" = None, user_dir: "str | Path | None" = None
) -> "tuple[list[dict[str, Any]], list[str]]":
    """One pass over both levels: the listed machines and broken-file
    warnings (``<path>: <errors>``) -- the shared scan behind
    ``list_machines`` and the CLI's ``factory list``, with the same
    validity verdict ``resolve_machine`` applies."""
    reply = _library("scan", dirs=_dirs(repo_dir, user_dir))
    return reply["machines"], reply["warnings"]


def list_machines(
    *, repo_dir: "str | Path | None" = None, user_dir: "str | Path | None" = None
) -> "list[dict[str, Any]]":
    """Library contents with descriptions, resolution-deduped (repo wins).

    Broken files are skipped silently here (the agent-facing list);
    ``cli_dispatch``'s ``list`` op surfaces them as warnings so the CLI's
    ``factory list`` can say why a machine does not show.
    """
    return _scan_machine_library(repo_dir=repo_dir, user_dir=user_dir)[0]


def resolve_machine(
    name: str,
    *,
    repo_dir: "str | Path | None" = None,
    user_dir: "str | Path | None" = None,
) -> "tuple[MachineFile, Path]":
    """Resolve one machine by name: repo directory first, user second.

    Only a file that reads, parses, and validates -- the listing's own
    verdict -- claims its DECLARED name, so ``factory list``,
    ``rlm.factory.run``, and export never disagree about a name. A name
    whose only carriers are invalid files raises
    ``MachineResolutionError`` with ``broken=True`` and the first file's
    exact errors (in repo-to-user order) -- broken, never missing; a name
    no machine carries raises it with ``broken=False``; an invalid name
    raises ``ValueError``.
    """
    reply = _library("resolve", name=_table(name), dirs=_dirs(repo_dir, user_dir))
    machine = _machine_file(reply["machine"])
    assert machine is not None
    return machine, Path(reply["path"])


def import_machine(path: "str | Path", *, target_dir: "str | Path | None" = None) -> "dict[str, Any]":
    """The library gate: parse a MACHINE.md, validate its spec, persist it.

    The spec goes through the SAME write-time validator as every factory
    write (``validate_factory_spec``): an invalid spec never persists, and
    the ``ValueError`` carries every error sentence, so the surface stays
    user-correctable. Valid files persist byte-for-byte (their own prose,
    formatting, and line endings travel with the machine) into the user
    library.
    """
    source = Path(path).expanduser()
    target = Path(target_dir).expanduser() if target_dir is not None else user_machines_dir()
    return _library("import", path=str(source), target_dir=str(target))


def export_factory_spec(
    spec: Any,
    out_path: "str | Path",
    *,
    name: str,
    description: str,
    version: str = "1",
    author: str = "",
    overwrite: bool = False,
) -> "dict[str, Any]":
    """Serialize any spec (stored entry or run machine) to MACHINE.md.

    Byte-pretty and stable: the same spec always renders to the same bytes.
    The spec passes through the write-time validator first, so an exported
    file always re-imports. The out target is never overwritten silently: an
    existing file refuses unless ``overwrite=True`` says otherwise.
    """
    return _library(
        "export_spec",
        spec=_table(spec),
        **_spec_json(spec),
        out_path=str(Path(out_path).expanduser()),
        name=_table(name),
        description=_table(description),
        version=_table(version),
        author=_table(author),
        overwrite=bool(overwrite),
    )


def export_library_machine(
    name: str,
    out_path: "str | Path",
    *,
    repo_dir: "str | Path | None" = None,
    user_dir: "str | Path | None" = None,
    overwrite: bool = False,
) -> "dict[str, Any]":
    """Export one library machine to MACHINE.md at ``out_path``.

    Resolution is the library contract only (repo directory first, user
    second); the file copies verbatim so the shared documentation travels
    with the spec. The out target is never overwritten silently: an
    existing file refuses unless ``overwrite=True`` says otherwise. The
    CLI dispatches here because a fresh CLI process has no session state
    (stored entries and live runs) to resolve from.
    """
    return _library(
        "export_library",
        name=_table(name),
        out_path=str(Path(out_path).expanduser()),
        dirs=_dirs(repo_dir, user_dir),
        overwrite=bool(overwrite),
    )


def _live_run_machine(run_id: str) -> "dict[str, Any] | None":
    """A live run's export view from the host (its canonical machine, spec
    id, name, and id), or ``None`` when no run carries that id or no host
    is serving this process (a fresh CLI process has no runs)."""
    from . import repl

    if not repl.is_active():
        return None
    return _executor_call_blocking("factory.machine", {"run_id": run_id})


def export_machine(
    target: str,
    out_path: "str | Path",
    *,
    repo_dir: "str | Path | None" = None,
    user_dir: "str | Path | None" = None,
    overwrite: bool = False,
) -> "dict[str, Any]":
    """Export one machine to MACHINE.md at ``out_path``.

    Resolution mirrors ``run_factory``: a stored factory entry first, a
    live run's canonical machine second, then the library machine (repo
    directory first, user second). Library machines copy their file
    verbatim so the shared documentation travels with the spec; entry and
    run specs render byte-pretty.
    """
    entry = default_factory_executor()._resolve_harness().get("factory", target)
    stored = None
    if entry is not None:
        stored = {key: _table(getattr(entry, key)) for key in ("arguments", "content", "title")}
    return _library(
        "export_machine",
        target=_table(target),
        entry=stored,
        run=None if entry is not None else _live_run_machine(target),
        out_path=str(Path(out_path).expanduser()),
        dirs=_dirs(repo_dir, user_dir),
        overwrite=bool(overwrite),
    )


def cli_dispatch(payload: Any) -> "dict[str, Any]":
    """JSON facade for the ``prime-agent factory`` subcommands.

    One JSON payload in (an op, a path, a name, an out target: only what
    the user typed), one JSON result out: ``{"ok": true, ...}`` or
    ``{"ok": false, "errors": [...]}``. Every error surfaces as data, so
    the exact validator sentences reach the command's output verbatim. This
    process resolves the library directories; a fresh CLI process has no
    session state (stored entries, live runs), so export resolves the
    library only.
    """
    return _library("cli", payload=_table(payload), dirs=_dirs(None, None))


FACTORY_HELP: str = r"""# Factory

The factory runs state-machine workflows of spawned child agents. A stored
factory entry declares the machine — states, each backed by a subagent
spec, plus guarded transitions between them. `await rlm.factory.run('<spec_id>')`
spawns each state's subagent as an ordinary child, feeds captured outputs
into the successors' prompts, and drives the run to quiescence in a
background run in the Prime Agent host; the call returns immediately and
the run continues after the model turn ends (and across kernel restarts). Use it when a workflow needs shape: fan-out,
bounded loops (review/fix until a verdict approves), joins, or one child
per list item.

The factory is opt-in: it ships disabled, and the user turns it on with
`/factory on` (`/factory off` disables it again, `/factory status` reports
it; the persisted setting is `factory.enabled` in the agent dir's
settings.json). While it is disabled, every `rlm.factory` call except
`help()` — run, status, stop, and resume — plus every factory harness
write (`create_factory` and updates of factory entries) refuses with one
clean error:
"the factory is disabled; run /factory on to enable it". `help()` answers
while disabled, so this guide stays readable before opting in.

## Store the spec

A factory spec is a continual-harness entry of kind `factory`.
`rlm.harness.create_factory(...)` validates at write time; an invalid spec
is never stored (generic `create`/`update` funnel through the same check).
The spec rides `machine=` (the native form) or `dag=` (sugar that compiles
to machine form) — pass exactly one. This review/fix loop is the shipped
pr-manager shape:

```python
rlm.harness.create_factory(
    "pr-manager",
    "Drive a PR through review/fix cycles, then keep a resident watcher on it.",
    id="pr-manager",
    machine={
        "run": {"budget_ms": 1_800_000, "max_parallel": 8, "max_transitions": 24},
        "states": [
            {
                "id": "entry", "entry": True,
                "subagent": {"prompt": (
                    "Identify the pull request for the current branch with "
                    "`gh pr view --json url`. Return a fenced json block of the form "
                    '{"pr_url": "https://github.com/owner/repo/pull/N"}. '
                    "Output only the json block.")},
                "outputs": [{"name": "pr_url", "type": "json"}],
            },
            {
                "id": "reviewing",
                "subagent": {"prompt": (
                    "Review the pull request at {pr_url} for merge-blocking "
                    "defects with `gh pr diff`. When a fix report is bound below, "
                    "verify the described fixes landed. Return a fenced json block "
                    'of the form {"verdict": {"approved": <true|false>, '
                    '"findings": ["at most three one-line findings"]}}. '
                    "Output only the json block.")},
                "inputs": [
                    {"name": "pr_url", "type": "json", "from": "entry.pr_url"},
                    {"name": "fix_report", "type": "json", "from": "fixing.fix_report", "optional": True},
                ],
                "outputs": [{"name": "verdict", "type": "json"}],
                "max_entries": 4,
            },
            {
                "id": "fixing",
                "subagent": {"prompt": (
                    "Address the review findings in the verdict below. Make the "
                    "smallest targeted fixes, run the relevant tests, and return a "
                    "fenced json block of the form "
                    '{"fix_report": {"fixed": ["finding that was addressed"], '
                    '"skipped": ["finding left alone and why"]}}. '
                    "Output only the json block.\n\n{verdict}")},
                "inputs": [{"name": "verdict", "type": "json", "from": "reviewing.verdict"}],
                "outputs": [{"name": "fix_report", "type": "json"}],
                "max_entries": 3,
            },
            {
                "id": "monitoring",
                "subagent": {"prompt": "Stay resident as the watcher for {pr_url}: "
                    "report the `gh pr checks` state once, then remain available for "
                    "follow-up questions."},
                "inputs": [{"name": "pr_url", "type": "json", "from": "entry.pr_url"}],
                "lifecycle": "resident",
            },
        ],
        "transitions": [
            {"from": "entry", "to": "reviewing"},
            {"from": "reviewing", "to": "fixing",
             "when": {"output": "verdict", "path": "approved", "op": "eq", "value": False}},
            {"from": "reviewing", "to": "monitoring",
             "when": {"output": "verdict", "path": "approved", "op": "eq", "value": True}},
            {"from": "fixing", "to": "reviewing"},
        ],
    },
)
```

The example exercises the core forms: `entry` is an entry state; the two
guards select the next state from the reviewer's `verdict`; `fix_report` is
optional, so the reviewer's first entry binds a null sentinel before the
fixer ever runs and its re-entry re-binds the real report; `max_entries`
bounds the loop; `monitoring` is a `resident` that stays alive under the
parent session after the run ends. Both worked examples bound their
emitted payloads in the prompt — a capped findings list here, a capped
file list in the review-sweep example — because the executor's capture
cap still bounds very large payloads: an unbounded payload truncates at
the cap and fails to bind.

## Authoring reference

- **States**: 1 to 1024, unique ids matching `^[a-z0-9][a-z0-9-]{0,63}$`; at
  least one state carries `"entry": true`, and entry states declare no
  inputs. A state's `subagent` is a harness subagent entry id or title (its
  content is the prompt template; `metadata.model`/`metadata.thinking` are
  spawn settings) or an inline `{"prompt": ...}` object with optional
  `name`/`model`/`thinking`. The optional `name` labels the spawned
  children run-scoped (at most 64 characters, unique across the
  machine's states — a name another state's name can suffix onto, `foo`
  vs `foo-i1`, is rejected at write time): the spawn label is
  `<run6>-name` — the supervisor's sibling names are per-parent-session,
  and a parent session outlives its runs, so the run prefix keeps two
  runs of one machine from colliding; read the exact label from the
  `spawned` event's `name` or the status node's instance rows — and
  re-entries, foreach fan-out, and retries disambiguate with the same
  `-i<n>`/`-a<n>` suffixes the generated labels use; a suffixed label
  that would pass the host's 64-character cap shrinks its base with a
  digest of the full name, like the generated labels do.
- **Ports**: inputs and outputs of type `text` or `json`. An input binds
  `"from": "<state_id>.<output_name>"`; types must match, duplicates are
  rejected, and nothing can read from a resident. Bound values render into
  `{input_name}` placeholders (one pass; inputs without a placeholder are
  appended in a trailing `## Inputs` section). A required input whose source
  has not settled yet keeps the entry pending; over a settled source that
  offers no value (an errored settle, a port the settle captured no value
  for, or a JSON capture failure) it fails the dependent entry, while
  `"optional": true` binds a null sentinel in every no-value case (a
  source that offers no value is not a value, so the dependent that
  declared the input optional proceeds). A required self-input is
  rejected at validation —
  `state X input 'name' cannot require itself: mark the self-input optional
  - a required one can never bind on the state's first entry` — while an
  optional self-input is the designed self-loop form (first entry binds
  null, re-entries bind the previous settle).
- **Transitions**: `{"from": ..., "to": ..., "on": "settled", "when": ...}`.
  Each settle is evaluated exactly once and every guard that passes fires
  (fan-out is legal); a fire onto a state at `max_entries` is recorded as a
  blocked transition. `from` may be a list of states: a join that fires
  once every source settled — once per source-settle combination — and may
  not carry a guard. Guards are `{"output": ..., "path": ..., "op": ...,
  "value": ...}` over the from-state's latest settle: `op` is one of `eq`,
  `ne`, `gt`, `gte`, `lt`, `lte`, `exists`, `contains`; `path` drills a
  dotted path into a `json` output; `eq`/`ne` compare JSON-strictly (a
  boolean never equals a number), comparison ops need a numeric value,
  `contains` a non-empty list, `exists` no value, and a missing or
  unparseable port fails every op except `exists`. A failed settle still
  fires guard-less transitions, so dependents under `continue` run; their
  required input over the failed source then fails the dependent entry,
  while an optional input over the failed source binds the null sentinel
  and the dependent proceeds.
- **Cycles are legal**: there is no acyclicity requirement — self-loops and
  back edges validate. The one rule is an entry state somewhere; a dag
  whose every node depends on another compiles to no entry states and is
  rejected.
- **foreach**: `{"over": "<input>", "max": 1..256}` expands one entry into
  one child per item of the named `json` input (clamped at `max`), each
  child rendered with its item bound as that input; an empty list settles
  the entry with no children.
- **Residents**: `"lifecycle": "resident"` states declare no outputs, no
  foreach, and no outgoing transitions, nothing reads from them, and their
  instance stays alive under the parent session after the run completes
  (stop the run to retire it).
- **Bounds and policies**: `run.max_parallel` (1..64, default 8) is the
  run's global budget of simultaneously running instances — not a
  per-node limit. `run.max_children` (default 10,000, capped at
  1,000,000) is the run's global budget of total admissions over its
  life — foreach expansions and retry re-spawns included (neither
  `max_parallel` nor `max_transitions` bounds children); reaching it
  pauses the run once, and `resume` continues past it as an explicit
  operator decision. `run.max_transitions` (default 10 per state, capped
  at 10,000) pauses the run once at the boundary, mid-settle; `resume`
  continues after the transitions that already fired without re-firing
  them. `run.budget_ms` pauses the run once when exceeded (in-flight
  children keep running). Per state: `max_entries` (default 1), `retries`
  (0..10, same rendered prompt), `budget_ms` (admission to settlement;
  exceeding it fails the attempt without a retry), and `failure_policy` —
  `fail_fast` (cancel every child, run failed), `continue` (entry stays
  errored; the run finishes and reports failed if any state errored), or
  `escalate` (the default: pause the run; resuming is the operator's
  decision).
- **Dead configurations fail loudly, never wedge**: a pending entry whose
  input source never settled, a `max_parallel` cap held entirely by
  never-settling residents with work queued, or nothing in flight and
  nothing pending each end the run as failed with the reason in the
  ledger. `wait` blocks on states are rejected at validation (not
  supported yet).

## Dag form

Sugar, not a second semantics: each node becomes a state entered once, a
node with no effective dependencies becomes an entry state, and the full
dependency set — `depends_on` plus every `inputs[].from` source — compiles
to ONE join transition, so a fan-in node waits for every parent. The
shipped review-sweep shape:

```python
rlm.harness.create_factory(
    "review-sweep",
    "Sweep the branch's changed files for findings, then merge them into one list.",
    id="review-sweep",
    dag={
        "run": {"budget_ms": 900_000, "max_parallel": 8},
        "nodes": [
            {
                "id": "files",
                "subagent": {"prompt": (
                    "List the files the current branch changes relative to the "
                    "base branch, capped at the eight most relevant. Return "
                    'a fenced json block of the form {"files": ["path/to/file", ...]}. '
                    "Output only the json block.")},
                "outputs": [{"name": "files", "type": "json"}],
            },
            {
                "id": "review",
                "subagent": {"prompt": (
                    "Review the changed file {files} for merge-blocking defects: "
                    "correctness bugs, regressions, unhandled error paths, missing "
                    "tests. Reply one short line: `<path>: <the most serious "
                    "problem, or 'clean'>`.")},
                "inputs": [{"name": "files", "type": "json", "from": "files.files"}],
                "outputs": [{"name": "found", "type": "text"}],
                "foreach": {"over": "files", "max": 8},
            },
            {
                "id": "report",
                "subagent": {"prompt": (
                    "Merge the review lines below into one fenced json block of "
                    'the form {"issues": [{"file": "path", "finding": "..."}]} '
                    "listing every file that is not clean. Output only the json "
                    "block.\n\n{found}")},
                "inputs": [{"name": "found", "type": "text", "from": "review.found"}],
            },
        ],
    },
)
```

## Run and steer

```python
result = await rlm.factory.run("pr-manager")
# {"run_id": "...", "spec_id": "pr-manager", "nodes": 4, "max_parallel": 8,
#  "started": ["entry"], "pending": []}  — returns immediately.

status = await rlm.factory.status(result["run_id"])
status["state"]    # running | stopping | paused | done | failed | stopped
status["nodes"]    # per state: status, entries_used/max_entries, instances,
                   # latest answer_preview, error
status["events"]   # trailing ledger: spawned, settled, answer_captured,
                   # transition_fired, node_error, milestone, ...
status["usage"]    # spawns, settled, tool_uses, max_parallel, max_children, running,
                    # transitions_fired
status["needs_verify"]  # (when set) states whose child EXITED carrying a
                        # provisional answer - verify the remote state; the
                        # entry row carries the provisional_answer preview
status["pause_reason"]  # (paused) why the run paused; with "last_error" (the
status["last_error"]    # last admission/bind failure) and a one-line "remedy"
```

`graph()` and `watch()` are the live monitoring views this namespace
ships alongside the stacked live-view PR's TUI page:

```python
graph = await rlm.factory.graph(result["run_id"])
# {"run_id": "...", "spec_id": "pr-manager", "state": "running",
#  "machine": {"order": [...], "states": [...],
#              "transitions": [...], "run": {...}},
#  "nodes": [...], "active_nodes": [...], "last_fired": [...],
#  "events": [...], "usage": {...}, "budget": {"limit_ms": ...,
#  "consumed_ms": ...}}  — structure fused with live state.

every = await rlm.factory.graph()        # every live run ({"runs": [...]})
spec = await rlm.factory.graph("pr-manager")  # a stored spec's static graph

watched = await rlm.factory.watch(result["run_id"], 30)
# the same fused snapshot plus "changed" — the call blocks until the
# run's state/instance shape changes or the bounded timeout elapses,
# so one call streams a run's progress without polling `status()`.
```

- `graph(ref)` fuses the machine's structure (states, guarded
  transitions, the declared order) with the live run's overlay (the
  node reports, active nodes, recently fired edges, the event tail,
  usage, budget consumed); a stored spec id answers the static
  structure, and no ref answers every live run.
- `watch(run_id, timeout)` returns immediately with the snapshot when
  nothing changed, blocks until the run's state/instance shape changes,
  and answers `"changed": false` on the bounded timeout.

- `run` re-validates the spec and resolves every subagent reference first,
  reporting all failures in one `ValueError` and starting nothing on any
  failure; `name=` labels the run in status and the TUI.
- Pause and failure notices (escalate, budget, max_transitions,
  max_children, failed, finished) arrive as quiet notices in the
  conversation once per kind per run, the pause notices with the resume
  call spelled out — a paused run does not need polling to be noticed.
- `stop(run_id)` cancels every running child of the run (idempotent);
  `resume(run_id)` continues a paused run and raises on a non-paused one.
- The activity lane the daemon and TUI speak is camelCase on the wire
  (`runId`, `specId`, `timeoutMs`); the kernel API here
  (`rlm.factory.*`) is snake_case.

## Discovering machines

- The machine library: machines are `MACHINE.md` files (frontmatter plus
  one fenced `machine-spec` block), one directory per machine, resolved
  from two levels — the bundled seeds shipped inside the runtime (visible
  in every install; `PRIME_AGENT_MACHINES_DIR` redirects the level at a
  team directory) first, the personal `machines/` library under the agent
  dir second; the earlier level wins on name conflicts. `prime-agent
  factory list | import | export` manages them: list shows only what
  parses and validates (broken files print as warnings), import runs the same
  write-time validation as a stored spec so an invalid machine never
  persists, and export copies a library machine verbatim to a fresh path
  (an existing target is refused, never overwritten). `rlm.factory.run('<name>')`
  runs a library machine directly without creating a harness entry; a
  machine that exists but is broken names its errors
  instead of pretending the name is unknown. The bundled seeds are
  `builder`, `pr-manager`, and `review-sweep`; the worked examples above
  derive from their shapes.
- The TUI factory page: the activity dock's `⚙ N factory` group (Enter or
  click) opens one live diagram per run, newest run first. The up/down
  arrows move the run selection, Enter opens the selected run's action
  rows (stop, or resume first while the run is paused — the arrows walk
  the rows, Enter runs the tracked action), and Esc backs out of the rows
  before it closes the page.

## Safety

- Every state spawns real children that spend budget. Bound loops with
  `max_entries`, `max_transitions`, and `run.max_children` (total
  admissions); the default `escalate` policy pauses
  instead of failing, so read `status` (or the notice) before resuming.
- Outputs bind from the child's full final answer (the collect envelope
  carries it; the executor caps its own capture at 8,192 characters): a
  fenced json output block binds whole as long as it fits that cap, so
  keep declared outputs compact — the rosters and ledger previews stay
  ~160 characters — and let the full answer live in the child's session.
- Runs live in the Prime Agent host, not in the kernel: a kernel restart
  or crash never touches a running workflow, and `status` keeps reading
  it. Each run keeps a durable record; a host restart pauses a run that
  was in flight as interrupted (its lost children re-queue), and
  `resume(run_id)` continues it.
- Residents outlive their run; stop the run (or tear down the session) to
  retire them. Prefer `rlm.factory.stop(run_id)` over deleting a factory
  child by hand — the executor claims and cancels children itself, and a
  hand deletion surfaces as a child failure through the state's policy.
"""
