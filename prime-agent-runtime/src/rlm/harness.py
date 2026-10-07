"""Persistent harness-state helpers for Prime Agent's RLM kernel.

The state model is intentionally small: it records prompt notes, memory,
skills, subagent specs, and refinement events in the session-local harness
store by default; pass ``global_=True`` for the cross-session global store.

The store itself lives in the Prime Agent host (``pa_core::refinement::store``),
the one implementation every reader and writer of ``harness_state.json``
shares: validation, id minting, versioning, locking and the file format are
all there. This module is its client. It resolves which store a call targets
(from the kernel's ``RLM_*`` environment), sends each call as a
``harness.<op>`` host request, and mirrors the store's state on the
``HarnessState`` it returns (``entries``, ``refinements``). Inside a kernel
the request goes over the kernel protocol; in a plain Python process it goes
through the ``prime-agent --prime-agent-harness-request`` one-shot.
"""

from __future__ import annotations

import json
import os
import subprocess
from collections.abc import Mapping
from dataclasses import asdict, dataclass, field, fields
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Literal, TypeAlias, TypedDict, TypeGuard, Unpack, cast, overload

from .factory import validate_factory_spec

HarnessKind = Literal["prompt", "memory", "skill", "subagent", "factory"]
HarnessScope = Literal["local", "global"]

_DEFAULT_FILE_NAME = "harness_state.json"
_DEFAULT_HARNESS_DIR_NAME = "harness"
# Written by the kernel, gated by nothing. The host stamps "refine" on entries
# that cleared the RAVO gate, so the two are distinguishable on disk.
KERNEL_ENTRY_SOURCE = "kernel"
_KINDS: tuple[HarnessKind, ...] = ("prompt", "memory", "skill", "subagent", "factory")
_state_cache: dict[tuple[Path, HarnessScope], "HarnessState"] = {}

# The hidden flag of the host binary that serves one request outside a kernel.
_HARNESS_REQUEST_FLAG = "--prime-agent-harness-request"
# The marker the host reads in place of a value JSON cannot carry (a set, a
# datetime): validation names its type, and a save refuses it like json.dump.
_UNSERIALIZABLE_KEY = "__rlm_harness_unserializable__"
# JSON as the host request carries it.
JsonValue: TypeAlias = "None | bool | int | float | str | list[JsonValue] | dict[str, JsonValue]"
JsonObject: TypeAlias = "dict[str, JsonValue]"


# The keyword arguments every scoped call accepts beyond its own: ``global``
# (the reserved-word spelling of ``global_``).
_ScopeKwargs = TypedDict("_ScopeKwargs", {"global": bool}, total=False)


class _HostError(TypedDict):
    """The Python exception a store call raises: its class name and message."""

    type: str
    message: str


class _HostReply(TypedDict, total=False):
    """One ``harness.<op>`` reply: ``ok`` with the call's ``result`` and the
    store's ``state`` after it, or ``ok: false`` with the ``error``."""

    ok: bool
    result: JsonValue
    state: JsonObject
    loadError: str | None
    error: _HostError


_ERRORS: dict[str, type[Exception]] = {
    "ValueError": ValueError,
    "TypeError": TypeError,
    "RuntimeError": RuntimeError,
    "TimeoutError": TimeoutError,
    "OSError": OSError,
}


def _now() -> str:
    return datetime.now(timezone.utc).isoformat()


def _agent_dir() -> Path:
    raw = (
        os.environ.get("PRIME_AGENT_CODING_AGENT_DIR")
        or os.environ.get("PI_CODING_AGENT_DIR")
        or str(Path.home() / ".prime" / "agent")
    )
    return Path(raw).expanduser().resolve()


def _resolve_global_flag(global_: bool = False, extra: Mapping[str, object] | None = None) -> bool:
    extra = dict(extra or {})
    if "global" in extra:
        value = extra.pop("global")
        if not isinstance(value, bool):
            raise TypeError(f"global must be a bool, got {type(value).__name__}")
        global_ = value
    if extra:
        unexpected = next(iter(extra))
        raise TypeError(f"unexpected keyword argument {unexpected!r}")
    return bool(global_)


@overload
def _strip_scope_prefix(id: str, global_: bool) -> tuple[str, bool]: ...
@overload
def _strip_scope_prefix(id: str | None, global_: bool) -> tuple[str | None, bool]: ...
def _strip_scope_prefix(id: str | None, global_: bool) -> tuple[str | None, bool]:
    # overview() displays entries as [local:id]/[global:id]; accept those ids
    # verbatim. A global: prefix routes to the global store unless the caller
    # already forced a scope via global_.
    if isinstance(id, str):
        scope, sep, rest = id.partition(":")
        if sep and rest and scope in ("local", "global"):
            return rest, global_ or scope == "global"
    return id, global_


def _env_dir(name: str) -> str | None:
    # Set-but-empty env values must behave as unset; a bare "" would skip the
    # session-dir fallback and land local writes in the global agent-dir default.
    value = (os.environ.get(name) or "").strip()
    return value or None


def _state_file(state_dir: str | Path | None = None, *, global_: bool = False) -> Path:
    root: str | Path | None = state_dir
    if root is None:
        root = _env_dir("RLM_GLOBAL_HARNESS_STATE_DIR") if global_ else _env_dir("RLM_HARNESS_STATE_DIR")
    if root is None and not global_ and (session_dir := _env_dir("RLM_SESSION_DIR")):
        root = Path(session_dir) / _DEFAULT_HARNESS_DIR_NAME
    if root is None and not global_:
        raise RuntimeError(
            "Local harness state requires RLM_HARNESS_STATE_DIR or RLM_SESSION_DIR. "
            + "Use get_harness_state(global_=True) for global state."
        )
    if root:
        return Path(root).expanduser().resolve() / _DEFAULT_FILE_NAME
    return _agent_dir() / _DEFAULT_HARNESS_DIR_NAME / _DEFAULT_FILE_NAME


@dataclass
class HarnessEntry:
    """A reusable prompt, memory, skill, or subagent record."""

    id: str
    kind: HarnessKind
    title: str
    content: str
    path: str = "general"
    scope: HarnessScope = "local"
    reference: dict[str, Any] = field(default_factory=dict)
    arguments: dict[str, Any] = field(default_factory=dict)
    metadata: dict[str, Any] = field(default_factory=dict)
    # Provenance, not decoration. Everything written through this module comes
    # from inside the kernel and passes through no RAVO screen, no judge, no
    # referee and no trust window -- unlike a `/refine` commit, which is
    # stamped "refine" by the host. Labelling them apart is what makes the
    # ungated share of the global store measurable instead of inferred; it was
    # 26 of 27 global entries when this was added.
    source: str = KERNEL_ENTRY_SOURCE
    created_at: str = field(default_factory=_now)
    updated_at: str = field(default_factory=_now)
    version: int = 1
    # Per-entry keys this dataclass does not model (`trust`, `enabled`, ...).
    # The host owns them, exactly as it owns the unmodelled top-level keys of
    # the store. They are flattened back onto the entry object by
    # _entry_payload, so `extra` itself is never a key on disk.
    extra: dict[str, Any] = field(default_factory=dict, repr=False, compare=False)

    @property
    def enabled(self) -> bool:
        """Whether the entry is active. A disabled entry stays stored (and
        rollback-able) but the host hides it from the system prompt. Absent
        means enabled, so state written before the flag keeps working."""
        return self.extra.get("enabled") is not False


@dataclass
class RefinementEvent:
    """A recorded online harness-refinement pass."""

    id: str
    trigger: str
    changes: list[str]
    evidence: str = ""
    outcome: str = ""
    created_at: str = field(default_factory=_now)
    # Why the host ran the refine (`manual`, `recurrence`, `turn_interval`, ...).
    # The host filters its prompt listing on it, so a kernel save must keep it.
    reason: str | None = None


_ENTRY_FIELDS = {field.name for field in fields(HarnessEntry)} - {"extra"}
_REFINEMENT_FIELDS = {field.name for field in fields(RefinementEvent)}
# Top-level keys HarnessState models. Everything else in the store is
# host-owned (`ravo`, `failures`, `trustWindows`) and is round-tripped.
_MODELLED_TOP_LEVEL = {"schema", "entries", "refinements"}


def _entry_payload(entry: HarnessEntry) -> dict[str, object]:
    """Serialize an entry with its unmodelled host-owned keys flattened back in.

    Modelled fields win: `extra` only ever carries keys this dataclass does not
    know about, so a stale duplicate there can never shadow a real field.
    """
    data: dict[str, object] = asdict(entry)
    del data["extra"]
    return {**entry.extra, **data}


def _refinement_payload(event: RefinementEvent) -> dict[str, object]:
    """Serialize a refinement event, leaving `reason` off the events that have none."""
    data: dict[str, object] = asdict(event)
    if data.get("reason") is None:
        del data["reason"]
    return data


def _is_kind(value: object) -> TypeGuard[HarnessKind]:
    return value in _KINDS


def _is_object(value: object) -> TypeGuard[JsonObject]:
    return isinstance(value, dict)


def _invalid(what: str) -> RuntimeError:
    return RuntimeError(f"the harness host returned an invalid {what}")


def _field_text(payload: JsonObject, key: str, what: str) -> str:
    value = payload.get(key)
    if not isinstance(value, str):
        raise _invalid(what)
    return value


def _field_record(payload: JsonObject, key: str, what: str) -> JsonObject:
    value = payload.get(key)
    if not _is_object(value):
        raise _invalid(what)
    return value


def _entry_from_payload(payload: JsonObject) -> HarnessEntry:
    """A stored entry as the host reports it (modelled keys plus its own)."""
    kind, scope, version = payload.get("kind"), payload.get("scope"), payload.get("version")
    if not _is_kind(kind) or scope not in ("local", "global") or not isinstance(version, int):
        raise _invalid("harness entry")
    return HarnessEntry(
        id=_field_text(payload, "id", "harness entry"),
        kind=kind,
        title=_field_text(payload, "title", "harness entry"),
        content=_field_text(payload, "content", "harness entry"),
        path=_field_text(payload, "path", "harness entry"),
        scope="global" if scope == "global" else "local",
        reference=_field_record(payload, "reference", "harness entry"),
        arguments=_field_record(payload, "arguments", "harness entry"),
        metadata=_field_record(payload, "metadata", "harness entry"),
        source=_field_text(payload, "source", "harness entry"),
        created_at=_field_text(payload, "created_at", "harness entry"),
        updated_at=_field_text(payload, "updated_at", "harness entry"),
        version=version,
        extra={key: value for key, value in payload.items() if key not in _ENTRY_FIELDS},
    )


def _event_from_payload(payload: JsonObject) -> RefinementEvent:
    """A recorded refinement event as the host reports it."""
    changes, reason = payload.get("changes"), payload.get("reason")
    if not isinstance(changes, list) or not all(isinstance(change, str) for change in changes):
        raise _invalid("refinement event")
    if reason is not None and not isinstance(reason, str):
        raise _invalid("refinement event")
    return RefinementEvent(
        id=_field_text(payload, "id", "refinement event"),
        trigger=_field_text(payload, "trigger", "refinement event"),
        changes=[change for change in changes if isinstance(change, str)],
        evidence=_field_text(payload, "evidence", "refinement event"),
        outcome=_field_text(payload, "outcome", "refinement event"),
        created_at=_field_text(payload, "created_at", "refinement event"),
        reason=reason,
    )


def _unserializable_marker(value: object) -> JsonObject:
    return {_UNSERIALIZABLE_KEY: type(value).__name__}


def _wire(value: object) -> JsonValue:
    """``value`` as plain JSON data, a value JSON cannot carry replaced by a
    marker naming its type. Out-of-range floats raise ValueError."""
    text = json.dumps(
        value,
        ensure_ascii=False,
        allow_nan=False,
        default=_unserializable_marker,
    )
    return cast("JsonValue", json.loads(text))


def _checkout_host() -> Path | None:
    """The host binary of the source checkout this runtime runs from, if built."""
    name = "prime-agent.exe" if os.name == "nt" else "prime-agent"
    checkout = Path(__file__).resolve().parents[3] / "target" / "debug" / name
    return checkout if checkout.is_file() else None


def _export_checkout_host() -> None:
    """Run from a source checkout without a host-exported binary, export the
    checkout's build the way the host exports its own to a kernel, so the
    Python processes this one starts reach the same store."""
    if _env_dir("PRIME_AGENT_EXECUTABLE") is None and (checkout := _checkout_host()) is not None:
        os.environ["PRIME_AGENT_EXECUTABLE"] = str(checkout)


_export_checkout_host()


def _host_executable() -> str:
    """The host binary that serves a request outside a kernel: the one the
    host exported, else this source checkout's build."""
    configured = _env_dir("PRIME_AGENT_EXECUTABLE")
    if configured:
        return configured
    if (checkout := _checkout_host()) is not None:
        return str(checkout)
    raise RuntimeError(
        "rlm.harness needs the Prime Agent host: call it inside a Prime Agent kernel, "
        + "or set PRIME_AGENT_EXECUTABLE to the prime-agent binary"
    )


def _one_shot(payload: JsonObject) -> JsonValue:
    executable = _host_executable()
    try:
        completed = subprocess.run(
            [executable, _HARNESS_REQUEST_FLAG],
            input=json.dumps(payload, ensure_ascii=False),
            capture_output=True,
            text=True,
            encoding="utf-8",
            check=False,
        )
    except OSError as err:
        raise RuntimeError(f"rlm.harness could not start the host {executable}: {err}") from err
    if completed.returncode != 0:
        detail = completed.stderr.strip() or f"exit code {completed.returncode}"
        raise RuntimeError(f"rlm.harness host request failed: {detail}")
    try:
        reply = cast("JsonValue", json.loads(completed.stdout))
    except ValueError as err:
        raise RuntimeError(f"rlm.harness host returned an invalid reply: {err}") from err
    return reply


def _host_call(request_type: str, payload: JsonObject) -> _HostReply:
    """Send one ``harness.<op>`` request; raise the store's error as the
    Python exception it names."""
    from . import repl

    message: JsonObject = {**payload, "type": request_type}
    body: JsonValue
    if repl.is_active():
        reply: Mapping[str, object] = repl.host_request_blocking(message)
        status = reply.get("status")
        if status == "error":
            raise RuntimeError(str(reply.get("error") or f"host request {request_type} failed"))
        if status != "ok":
            raise RuntimeError(f"host request {request_type} returned unexpected status: {status!r}")
        body = cast("JsonValue", reply.get("result"))
    else:
        body = _one_shot(message)
    if not _is_object(body):
        raise RuntimeError(f"host request {request_type} returned an invalid reply")
    if body.get("ok") is True:
        state, load_error = body.get("state"), body.get("loadError")
        result: _HostReply = {"ok": True, "result": body.get("result")}
        if _is_object(state):
            result["state"] = state
        result["loadError"] = load_error if isinstance(load_error, str) else None
        return result
    error = body.get("error")
    kind = error.get("type") if _is_object(error) else None
    text = error.get("message") if _is_object(error) else None
    exception = _ERRORS.get(kind, RuntimeError) if isinstance(kind, str) else RuntimeError
    raise exception(text if isinstance(text, str) and text else f"host request {request_type} failed")


def _factory_spec_errors(arguments: object) -> list[str] | None:
    """What the kernel's factory validator says about the spec a generic
    factory write stores, for the host to apply in its validation order."""
    if not isinstance(arguments, dict):
        return None
    record = cast("Mapping[str, object]", arguments)
    machine = record.get("machine")
    spec = machine if machine is not None else record.get("dag")
    return validate_factory_spec(spec) if isinstance(spec, dict) else None


class HarnessState:
    """CRUD store for reset-free harness refinement state."""

    def __init__(
        self,
        file_path: str | Path | None = None,
        *,
        in_memory: bool = False,
        scope: HarnessScope = "local",
        local_write_error: str | None = None,
    ):
        # in_memory mode never resolves or touches a path. It is the safe fallback when
        # path resolution itself fails, so constructing it cannot re-raise that error.
        if in_memory:
            self.file_path: Path | None = None
        else:
            self.file_path = (
                Path(file_path).expanduser().resolve()
                if file_path
                else _state_file(global_=(scope == "global"))
            )
        self.scope: HarnessScope = scope
        # When set, local mutations raise instead of vanishing into a volatile
        # store; reads and global_=True delegation keep working.
        self._local_write_error: str | None = local_write_error
        self.entries: dict[HarnessKind, dict[str, HarnessEntry]] = {kind: {} for kind in _KINDS}
        self.refinements: list[RefinementEvent] = []
        # Top-level keys this class does not model (`ravo`, `failures`,
        # `trustWindows`, ...), as of the last call: save() writes them back.
        self._extra: JsonObject = {}
        self._schema: JsonValue = 1
        # Each viewed entry's payload as last adopted from the store.
        self._adopted: dict[tuple[HarnessKind, str], JsonObject] = {}
        self._global_target_state_dir: Path | None = None
        # Why the store's file failed to parse at the last call, if it did. The
        # host backs such a file up before a write replaces it.
        self.load_error: str | None = None
        if self.file_path is not None:
            _ = self.load()

    def _document(self) -> JsonObject:
        """The state this view holds, as the store's document."""
        return {
            **self._extra,
            "schema": self._schema,
            "entries": _wire(
                {
                    kind: {entry_id: _entry_payload(entry) for entry_id, entry in records.items()}
                    for kind, records in self.entries.items()
                }
            ),
            "refinements": _wire([_refinement_payload(event) for event in self.refinements]),
        }

    def _adopt(self, document: JsonObject | None, load_error: str | None) -> None:
        """Mirror the store's state after a call."""
        if document is None:
            return
        self._extra = {key: value for key, value in document.items() if key not in _MODELLED_TOP_LEVEL}
        self._schema = document.get("schema", 1)
        raw_entries = document.get("entries")
        entries: dict[HarnessKind, dict[str, HarnessEntry]] = {}
        adopted: dict[tuple[HarnessKind, str], JsonObject] = {}
        for kind in _KINDS:
            entries[kind] = {}
            records = raw_entries.get(kind) if _is_object(raw_entries) else None
            if not _is_object(records):
                continue
            for entry_id, payload in records.items():
                if not _is_object(payload):
                    continue
                entries[kind][entry_id] = self._entry_object(kind, entry_id, payload)
                adopted[(kind, entry_id)] = payload
        self.entries = entries
        self._adopted = adopted
        events = document.get("refinements")
        self.refinements = (
            [_event_from_payload(event) for event in events if _is_object(event)] if isinstance(events, list) else []
        )
        self.load_error = load_error

    def _entry_object(self, kind: HarnessKind, entry_id: str, payload: JsonObject) -> HarnessEntry:
        """The view's object for one stored entry. An entry keeps its object
        across calls (like the store's in-memory records always did): an
        unchanged entry keeps it untouched, a changed one is updated in place."""
        current = self.entries.get(kind, {}).get(entry_id)
        if current is None:
            return _entry_from_payload(payload)
        if self._adopted.get((kind, entry_id)) != payload:
            fresh = _entry_from_payload(payload)
            for name in _ENTRY_FIELDS | {"extra"}:
                setattr(current, name, getattr(fresh, name))
        return current

    def _entry_result(self, payload: JsonValue) -> HarnessEntry:
        """A call's entry result, as the view's object for it."""
        if not _is_object(payload):
            raise _invalid("harness entry")
        kind, entry_id = payload.get("kind"), payload.get("id")
        current = self.entries[kind].get(entry_id) if _is_kind(kind) and isinstance(entry_id, str) else None
        return current if current is not None else _entry_from_payload(payload)

    def _entry_results(self, payloads: JsonValue) -> list[HarnessEntry]:
        if not isinstance(payloads, list):
            raise _invalid("harness entry list")
        return [self._entry_result(payload) for payload in payloads]

    def _call(self, request_type: str, extra: JsonObject | None = None, **args: object) -> JsonValue:
        """Run one store operation on this store and mirror its state."""
        payload: JsonObject = {
            "store": {
                "file": str(self.file_path) if self.file_path is not None else None,
                "scope": self.scope,
                # The in-memory store's state travels with each call.
                "document": self._document() if self.file_path is None else None,
                "writeError": self._local_write_error,
            },
            "agentDir": str(_agent_dir()),
            "args": _wire(args),
            "types": {name: type(value).__name__ for name, value in args.items()},
            **(extra or {}),
        }
        reply = _host_call(request_type, payload)
        self._adopt(reply.get("state"), reply.get("loadError"))
        return reply.get("result")

    def load(self) -> "HarnessState":
        _ = self._call("harness.load")
        return self

    def _global_target(self, global_: bool, extra: Mapping[str, object] | None = None) -> "HarnessState | None":
        if not _resolve_global_flag(global_, extra):
            return None
        target = get_harness_state(state_dir=self._global_target_state_dir, global_=True)
        if self.file_path is not None and target.file_path == self.file_path and target.scope == self.scope:
            return None
        return target

    def save(self) -> "HarnessState":
        if self.file_path is None:
            # in_memory: the view is the store.
            return self
        _ = self._call("harness.save", document=self._document())
        return self

    def upsert(
        self,
        kind: HarnessKind,
        title: str,
        content: str,
        *,
        id: str | None = None,
        path: str = "general",
        reference: dict[str, Any] | None = None,
        arguments: dict[str, Any] | None = None,
        metadata: dict[str, Any] | None = None,
        source: str = KERNEL_ENTRY_SOURCE,
        global_: bool = False,
        **kwargs: Unpack[_ScopeKwargs],
    ) -> HarnessEntry:
        id, global_ = _strip_scope_prefix(id, global_)
        if target := self._global_target(global_, kwargs):
            return target.upsert(
                kind,
                title,
                content,
                id=id,
                path=path,
                reference=reference,
                arguments=arguments,
                metadata=metadata,
                source=source,
            )
        return self._write("harness.upsert", kind, id, title, content, path, reference, arguments, metadata, source)

    def _write(
        self,
        request_type: str,
        kind: object,
        id: object,
        title: object,
        content: object,
        path: object,
        reference: object,
        arguments: object,
        metadata: object,
        source: object,
    ) -> HarnessEntry:
        extra: JsonObject = {}
        if kind == "factory":
            errors = _factory_spec_errors(arguments)
            extra["factorySpecErrors"] = list(errors) if errors is not None else None
        payload = self._call(
            request_type,
            extra,
            kind=kind,
            id=id,
            title=title,
            content=content,
            path=path,
            reference=reference,
            arguments=arguments,
            metadata=metadata,
            source=source,
        )
        return self._entry_result(payload)

    def get(self, kind: HarnessKind, id: str, *, global_: bool = False, **kwargs: Unpack[_ScopeKwargs]) -> HarnessEntry | None:
        id, global_ = _strip_scope_prefix(id, global_)
        if target := self._global_target(global_, kwargs):
            return target.get(kind, id)
        payload = self._call("harness.get", kind=kind, id=id)
        return None if payload is None else self._entry_result(payload)

    def delete(self, kind: HarnessKind, id: str, *, global_: bool = False, **kwargs: Unpack[_ScopeKwargs]) -> bool:
        id, global_ = _strip_scope_prefix(id, global_)
        if target := self._global_target(global_, kwargs):
            return target.delete(kind, id)
        return bool(self._call("harness.delete", kind=kind, id=id))

    def set_enabled(
        self, kind: HarnessKind, id: str, enabled: bool, *, global_: bool = False, **kwargs: Unpack[_ScopeKwargs]
    ) -> HarnessEntry:
        """Enable or disable one entry without deleting it.

        A disabled entry stays stored and rollback-able but is hidden from the
        system prompt (a disabled subagent spec is never offered for delegation).
        """
        id, global_ = _strip_scope_prefix(id, global_)
        if target := self._global_target(global_, kwargs):
            return target.set_enabled(kind, id, enabled)
        return self._entry_result(self._call("harness.set_enabled", kind=kind, id=id, enabled=enabled))

    def enable(self, kind: HarnessKind, id: str, *, global_: bool = False, **kwargs: Unpack[_ScopeKwargs]) -> HarnessEntry:
        return self.set_enabled(kind, id, True, global_=global_, **kwargs)

    def disable(self, kind: HarnessKind, id: str, *, global_: bool = False, **kwargs: Unpack[_ScopeKwargs]) -> HarnessEntry:
        return self.set_enabled(kind, id, False, global_=global_, **kwargs)

    def enable_memory(self, id: str, *, global_: bool = False, **kwargs: Unpack[_ScopeKwargs]) -> HarnessEntry:
        return self.set_enabled("memory", id, True, global_=global_, **kwargs)

    def disable_memory(self, id: str, *, global_: bool = False, **kwargs: Unpack[_ScopeKwargs]) -> HarnessEntry:
        return self.set_enabled("memory", id, False, global_=global_, **kwargs)

    def enable_prompt_note(self, id: str, *, global_: bool = False, **kwargs: Unpack[_ScopeKwargs]) -> HarnessEntry:
        return self.set_enabled("prompt", id, True, global_=global_, **kwargs)

    def disable_prompt_note(self, id: str, *, global_: bool = False, **kwargs: Unpack[_ScopeKwargs]) -> HarnessEntry:
        return self.set_enabled("prompt", id, False, global_=global_, **kwargs)

    def enable_skill(self, id: str, *, global_: bool = False, **kwargs: Unpack[_ScopeKwargs]) -> HarnessEntry:
        return self.set_enabled("skill", id, True, global_=global_, **kwargs)

    def disable_skill(self, id: str, *, global_: bool = False, **kwargs: Unpack[_ScopeKwargs]) -> HarnessEntry:
        return self.set_enabled("skill", id, False, global_=global_, **kwargs)

    def enable_subagent(self, id: str, *, global_: bool = False, **kwargs: Unpack[_ScopeKwargs]) -> HarnessEntry:
        return self.set_enabled("subagent", id, True, global_=global_, **kwargs)

    def disable_subagent(self, id: str, *, global_: bool = False, **kwargs: Unpack[_ScopeKwargs]) -> HarnessEntry:
        return self.set_enabled("subagent", id, False, global_=global_, **kwargs)

    def list(self, kind: HarnessKind | None = None, *, global_: bool = False, **kwargs: Unpack[_ScopeKwargs]) -> list[HarnessEntry]:
        if target := self._global_target(global_, kwargs):
            return target.list(kind)
        return self._entry_results(self._call("harness.list", kind=kind))

    def create(
        self,
        kind: HarnessKind,
        title: str,
        content: str,
        *,
        id: str | None = None,
        path: str = "general",
        reference: dict[str, Any] | None = None,
        arguments: dict[str, Any] | None = None,
        metadata: dict[str, Any] | None = None,
        source: str = KERNEL_ENTRY_SOURCE,
        global_: bool = False,
        **kwargs: Unpack[_ScopeKwargs],
    ) -> HarnessEntry:
        id, global_ = _strip_scope_prefix(id, global_)
        if target := self._global_target(global_, kwargs):
            return target.create(
                kind,
                title,
                content,
                id=id,
                path=path,
                reference=reference,
                arguments=arguments,
                metadata=metadata,
                source=source,
            )
        return self._write("harness.create", kind, id, title, content, path, reference, arguments, metadata, source)

    def update(
        self,
        kind: HarnessKind,
        id: str,
        title: str,
        content: str,
        *,
        path: str | None = None,
        reference: dict[str, Any] | None = None,
        arguments: dict[str, Any] | None = None,
        metadata: dict[str, Any] | None = None,
        source: str = KERNEL_ENTRY_SOURCE,
        global_: bool = False,
        **kwargs: Unpack[_ScopeKwargs],
    ) -> HarnessEntry:
        id, global_ = _strip_scope_prefix(id, global_)
        if target := self._global_target(global_, kwargs):
            return target.update(
                kind,
                id,
                title,
                content,
                path=path,
                reference=reference,
                arguments=arguments,
                metadata=metadata,
                source=source,
            )
        return self._write("harness.update", kind, id, title, content, path, reference, arguments, metadata, source)

    def create_memory(
        self,
        title: str,
        content: str,
        *,
        id: str | None = None,
        path: str = "general",
        metadata: dict[str, Any] | None = None,
        global_: bool = False,
        **kwargs: Unpack[_ScopeKwargs],
    ) -> HarnessEntry:
        return self.create("memory", title, content, id=id, path=path, metadata=metadata, global_=global_, **kwargs)

    def update_memory(
        self,
        id: str,
        title: str,
        content: str,
        *,
        path: str | None = None,
        metadata: dict[str, Any] | None = None,
        global_: bool = False,
        **kwargs: Unpack[_ScopeKwargs],
    ) -> HarnessEntry:
        return self.update("memory", id, title, content, path=path, metadata=metadata, global_=global_, **kwargs)

    def delete_memory(self, id: str, *, global_: bool = False, **kwargs: Unpack[_ScopeKwargs]) -> bool:
        return self.delete("memory", id, global_=global_, **kwargs)

    def create_prompt_note(
        self,
        title: str,
        content: str,
        *,
        id: str | None = None,
        path: str = "policy",
        metadata: dict[str, Any] | None = None,
        global_: bool = False,
        **kwargs: Unpack[_ScopeKwargs],
    ) -> HarnessEntry:
        return self.create("prompt", title, content, id=id, path=path, metadata=metadata, global_=global_, **kwargs)

    def update_prompt_note(
        self,
        id: str,
        title: str,
        content: str,
        *,
        path: str | None = None,
        metadata: dict[str, Any] | None = None,
        global_: bool = False,
        **kwargs: Unpack[_ScopeKwargs],
    ) -> HarnessEntry:
        return self.update("prompt", id, title, content, path=path, metadata=metadata, global_=global_, **kwargs)

    def delete_prompt_note(self, id: str, *, global_: bool = False, **kwargs: Unpack[_ScopeKwargs]) -> bool:
        return self.delete("prompt", id, global_=global_, **kwargs)

    def _skill_write(
        self,
        request_type: str,
        raw_id: object,
        title: object,
        content: object,
        path: object,
        reference: object,
        arguments: object,
        metadata: object,
        global_: bool,
        kwargs: Mapping[str, object],
    ) -> HarnessEntry:
        # The reference check names the entry by the id as the caller wrote it.
        id, global_ = _strip_scope_prefix(raw_id, global_) if isinstance(raw_id, str) else (raw_id, global_)
        if target := self._global_target(global_, kwargs):
            return target._skill_write(
                request_type, raw_id, title, content, path, reference, arguments, metadata, False, {}
            )
        payload = self._call(
            request_type,
            kind="skill",
            id=id,
            describeId=raw_id,
            title=title,
            content=content,
            path=path,
            reference=reference,
            arguments=arguments,
            metadata=metadata,
            source=KERNEL_ENTRY_SOURCE,
        )
        return self._entry_result(payload)

    def create_skill(
        self,
        title: str,
        content: str,
        *,
        id: str | None = None,
        path: str = "general",
        reference: dict[str, Any] | None = None,
        arguments: dict[str, Any] | None = None,
        metadata: dict[str, Any] | None = None,
        global_: bool = False,
        **kwargs: Unpack[_ScopeKwargs],
    ) -> HarnessEntry:
        return self._skill_write(
            "harness.create_skill", id, title, content, path, reference, arguments, metadata, global_, kwargs
        )

    def update_skill(
        self,
        id: str,
        title: str,
        content: str,
        *,
        path: str | None = None,
        reference: dict[str, Any] | None = None,
        arguments: dict[str, Any] | None = None,
        metadata: dict[str, Any] | None = None,
        global_: bool = False,
        **kwargs: Unpack[_ScopeKwargs],
    ) -> HarnessEntry:
        # Omitting the reference keeps the stored one rather than forcing every
        # title/content-only update to re-send the full Python reference.
        return self._skill_write(
            "harness.update_skill", id, title, content, path, reference, arguments, metadata, global_, kwargs
        )

    def delete_skill(self, id: str, *, global_: bool = False, **kwargs: Unpack[_ScopeKwargs]) -> bool:
        return self.delete("skill", id, global_=global_, **kwargs)

    def create_subagent(
        self,
        title: str,
        content: str,
        *,
        id: str | None = None,
        path: str = "general",
        metadata: dict[str, Any] | None = None,
        global_: bool = False,
        **kwargs: Unpack[_ScopeKwargs],
    ) -> HarnessEntry:
        return self.create("subagent", title, content, id=id, path=path, metadata=metadata, global_=global_, **kwargs)

    def update_subagent(
        self,
        id: str,
        title: str,
        content: str,
        *,
        path: str | None = None,
        metadata: dict[str, Any] | None = None,
        global_: bool = False,
        **kwargs: Unpack[_ScopeKwargs],
    ) -> HarnessEntry:
        return self.update("subagent", id, title, content, path=path, metadata=metadata, global_=global_, **kwargs)

    def delete_subagent(self, id: str, *, global_: bool = False, **kwargs: Unpack[_ScopeKwargs]) -> bool:
        return self.delete("subagent", id, global_=global_, **kwargs)

    def _factory_write(
        self,
        create: bool,
        id: object,
        title: object,
        content: object,
        path: object,
        dag: object,
        machine: object,
        metadata: object,
        global_: bool,
        kwargs: Mapping[str, object],
    ) -> HarnessEntry:
        # The host gates the opt-in, refuses both forms at once, and applies
        # the factory validator's verdict on the spec, before the shared write.
        spec: object = machine if machine is not None else dag
        spec_errors = validate_factory_spec(spec) if create or spec is not None else None
        if isinstance(id, str):
            id, global_ = _strip_scope_prefix(id, global_)
        if target := self._global_target(global_, kwargs):
            return target._factory_write(create, id, title, content, path, dag, machine, metadata, False, {})
        payload = self._call(
            "harness.factory",
            {"factorySpecErrors": list(spec_errors) if spec_errors is not None else None},
            create=create,
            id=id,
            title=title,
            content=content,
            path=path,
            dag=dag,
            machine=machine,
            metadata=metadata,
            source=KERNEL_ENTRY_SOURCE,
        )
        return self._entry_result(payload)

    def create_factory(
        self,
        title: str,
        content: str,
        *,
        id: str | None = None,
        path: str = "general",
        dag: dict[str, Any] | None = None,
        machine: dict[str, Any] | None = None,
        metadata: dict[str, Any] | None = None,
        global_: bool = False,
        **kwargs: Unpack[_ScopeKwargs],
    ) -> HarnessEntry:
        return self._factory_write(True, id, title, content, path, dag, machine, metadata, global_, kwargs)

    def update_factory(
        self,
        id: str,
        title: str,
        content: str,
        *,
        path: str | None = None,
        dag: dict[str, Any] | None = None,
        machine: dict[str, Any] | None = None,
        metadata: dict[str, Any] | None = None,
        global_: bool = False,
        **kwargs: Unpack[_ScopeKwargs],
    ) -> HarnessEntry:
        # Omitting both forms keeps the stored spec, exactly like update_skill
        # treats reference.
        return self._factory_write(False, id, title, content, path, dag, machine, metadata, global_, kwargs)

    def delete_factory(self, id: str, *, global_: bool = False, **kwargs: Unpack[_ScopeKwargs]) -> bool:
        return self.delete("factory", id, global_=global_, **kwargs)

    def record_refinement(
        self,
        trigger: str,
        changes: list[str] | str,
        *,
        evidence: str = "",
        outcome: str = "",
        id: str | None = None,
        global_: bool = False,
        **kwargs: Unpack[_ScopeKwargs],
    ) -> RefinementEvent:
        if target := self._global_target(global_, kwargs):
            return target.record_refinement(trigger, changes, evidence=evidence, outcome=outcome, id=id)
        payload = self._call(
            "harness.record_refinement", trigger=trigger, changes=changes, evidence=evidence, outcome=outcome, id=id
        )
        if not _is_object(payload):
            raise _invalid("refinement event")
        return _event_from_payload(payload)

    def plan_refinement(
        self,
        observation: str,
        *,
        failing_component: str = "",
        next_step: str = "",
    ) -> list[str]:
        target = f" for {failing_component}" if failing_component else ""
        plan = [
            f"Diagnose the repeated failure or opportunity{target}: {observation}",
            "Update the smallest useful prompt note, memory item, skill, or subagent spec.",
            "Run the next action with the changed harness state, then record the outcome.",
        ]
        if next_step:
            plan.append(f"Immediate validation step: {next_step}")
        return plan

    def overview(self, *, max_entries_per_kind: int = 20, global_: bool = False, **kwargs: Unpack[_ScopeKwargs]) -> str:
        if target := self._global_target(global_, kwargs):
            return target.overview(max_entries_per_kind=max_entries_per_kind)
        overview = self._call("harness.overview", max_entries_per_kind=max_entries_per_kind)
        if not isinstance(overview, str):
            raise _invalid("overview")
        return overview

    def search(
        self,
        query: str,
        kind: HarnessKind | None = None,
        limit: int = 10,
        *,
        global_: bool = False,
        **kwargs: Unpack[_ScopeKwargs],
    ) -> list[HarnessEntry]:
        """Return harness entries ranked by weighted term overlap with *query*.

        Terms are scored against an entry's title, content, path, and id;
        matches in more distinct fields count more. Each matched term is
        discounted by its document frequency across the ranked corpus
        (tf-idf style, ``weight * log(1 + N / df)``), so a rare,
        distinctive term outranks terms present in most entries.
        """
        if target := self._global_target(global_, kwargs):
            return target.search(query, kind=kind, limit=limit)
        payloads = self._call("harness.search", query=query, kind=kind, limit=limit)
        return self._entry_results(payloads)

    def snapshot(self, *, global_: bool = False, **kwargs: Unpack[_ScopeKwargs]) -> dict[str, Any]:
        if target := self._global_target(global_, kwargs):
            return target.snapshot()
        snapshot = self._call("harness.snapshot")
        if not _is_object(snapshot):
            raise _invalid("snapshot")
        return snapshot


def get_harness_state(
    state_dir: str | Path | None = None, *, global_: bool = False, **kwargs: Unpack[_ScopeKwargs]
) -> HarnessState:
    """Return the cached local harness state, or global when requested."""
    global_ = _resolve_global_flag(global_, kwargs)
    file_path = _state_file(state_dir, global_=global_)
    scope: HarnessScope = "global" if global_ else "local"
    cache_key = (file_path, scope)
    state = _state_cache.get(cache_key)
    if state is None:
        state = HarnessState(file_path, scope=scope)
        # Recorded at construction only: an instance created from env defaults must
        # keep targeting RLM_GLOBAL_HARNESS_STATE_DIR even when a later explicit
        # state_dir call aliases the same local file. An explicit dir that merely
        # aliases the env resolution must not sandbox later global_=True writes
        # either, so pin only when the explicit dir actually diverges.
        if state_dir is not None:
            try:
                env_file: Path | None = _state_file(global_=global_)
            except RuntimeError:
                env_file = None
            if file_path != env_file:
                state._global_target_state_dir = Path(state_dir).expanduser().resolve()
        _state_cache[cache_key] = state
    return state


__all__ = [
    "HarnessEntry",
    "HarnessKind",
    "HarnessScope",
    "HarnessState",
    "RefinementEvent",
    "get_harness_state",
]
