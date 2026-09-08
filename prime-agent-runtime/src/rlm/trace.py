"""W3C trace-context propagation for the Python kernel runtime.

The format and API surface mirror OpenTelemetry without depending on it: a
``TraceContext`` is one ``traceparent`` header, ``start_span`` mints child
spans and keeps the active context in a ``contextvars.ContextVar``, and every
finished span is handed to an installed emitter (``set_span_emitter``) as a
``span_end`` event dict. ``rlm.repl`` installs an emitter that ships those
dicts to the host over the protocol stream. See ``docs/observability.md`` in
the repository root for the cross-process contract.
"""

from __future__ import annotations

import contextlib
import contextvars
import os
import secrets
import time
import threading
from collections.abc import Callable, Iterator
from dataclasses import dataclass, field
from typing import Any

__all__ = [
    "Span",
    "Token",
    "TraceContext",
    "child_context",
    "current",
    "emit_event",
    "format_traceparent",
    "from_env",
    "inject_env",
    "new_span_id",
    "new_trace_id",
    "parse_traceparent",
    "reset",
    "set_current",
    "set_span_emitter",
    "start_span",
]

TRACEPARENT_ENV = "TRACEPARENT"
_VERSION = "00"
_HEX = frozenset("0123456789abcdef")


@dataclass(frozen=True)
class TraceContext:
    """One propagated ``traceparent``: the ids of the span that is active."""

    trace_id: str
    span_id: str
    flags: str = "01"
    parent_span_id: str | None = None


def _is_hex(value: str, length: int) -> bool:
    return len(value) == length and set(value) <= _HEX


def _is_id(value: str, length: int) -> bool:
    """Lowercase hex of the exact length and not the all-zero invalid id."""
    return _is_hex(value, length) and value.count("0") != length


def parse_traceparent(value: Any) -> TraceContext | None:
    """Parse a W3C ``traceparent`` string; ``None`` for anything invalid.

    Strict: version ``00``, lowercase hex only, ids must not be all zeros.
    Never raises, so callers can pass untrusted request fields straight in.
    """
    if not isinstance(value, str):
        return None
    parts = value.split("-")
    if len(parts) != 4:
        return None
    version, trace_id, span_id, flags = parts
    if version != _VERSION:
        return None
    if not _is_id(trace_id, 32) or not _is_id(span_id, 16) or not _is_hex(flags, 2):
        return None
    return TraceContext(trace_id=trace_id, span_id=span_id, flags=flags)


def format_traceparent(ctx: TraceContext) -> str:
    return f"{_VERSION}-{ctx.trace_id}-{ctx.span_id}-{ctx.flags}"


def _random_id(nbytes: int) -> str:
    while True:
        value = secrets.token_hex(nbytes)
        if value.count("0") != len(value):
            return value


def new_trace_id() -> str:
    return _random_id(16)


def new_span_id() -> str:
    return _random_id(8)


_current: contextvars.ContextVar[TraceContext | None] = contextvars.ContextVar("rlm_trace_context", default=None)


def current() -> TraceContext | None:
    """The active trace context for this task/thread, if any."""
    return _current.get()


@dataclass(frozen=True)
class Token:
    """Restore handle from :func:`set_current`; pass back to :func:`reset`."""

    _var_token: contextvars.Token[TraceContext | None]
    # Opaque OpenTelemetry attach token (None without the bridge) so the OTel
    # context is detached exactly where the trace context is restored.
    _otel_token: Any = None


def set_current(ctx: TraceContext | None) -> Token:
    """Make ``ctx`` current; pair with :func:`reset` to restore the previous value."""
    var_token = _current.set(ctx)
    otel_token = _otel_attach(ctx) if ctx is not None else None
    return Token(var_token, otel_token)


def reset(token: Token) -> None:
    _current.reset(token._var_token)
    _otel_detach(token._otel_token)


def _otel_attach(ctx: TraceContext) -> Any:
    """Mirror ``ctx`` into the OpenTelemetry context when the API is importable.

    Returns an opaque token for :func:`_otel_detach`, or ``None`` when the
    package is absent or anything fails; the bridge never raises.
    """
    try:
        from opentelemetry import context as otel_context
        from opentelemetry import trace as otel_trace

        span_context = otel_trace.SpanContext(
            trace_id=int(ctx.trace_id, 16),
            span_id=int(ctx.span_id, 16),
            is_remote=True,
            trace_flags=otel_trace.TraceFlags(int(ctx.flags, 16)),
        )
        span = otel_trace.NonRecordingSpan(span_context)
        return otel_context.attach(otel_trace.set_span_in_context(span))
    except BaseException:  # noqa: BLE001 - the bridge is best-effort by design
        return None


def _otel_detach(token: Any) -> None:
    if token is None:
        return
    try:
        from opentelemetry import context as otel_context

        otel_context.detach(token)
    except BaseException:  # noqa: BLE001 - see _otel_attach
        return


SpanEmitter = Callable[[dict[str, Any]], None]


def _noop_emitter(event: dict[str, Any]) -> None:
    return None


_emitter: SpanEmitter = _noop_emitter


def set_span_emitter(fn: SpanEmitter | None) -> None:
    """Install the sink for ``span_end`` events (``None`` restores the no-op)."""
    global _emitter
    _emitter = fn if fn is not None else _noop_emitter


def _emit(event: dict[str, Any]) -> None:
    try:
        _emitter(event)
    except BaseException:  # noqa: BLE001 - tracing must never break the traced code
        return


def emit_event(component: str, msg: str, **fields: Any) -> None:
    """Emit one structured diagnostic event under the active trace context."""
    event: dict[str, Any] = {"event": "trace", "component": component, "msg": msg, **fields}
    ctx = current()
    if ctx is not None:
        event.setdefault("traceId", ctx.trace_id)
        event.setdefault("spanId", ctx.span_id)
        if ctx.parent_span_id is not None:
            event.setdefault("parentSpanId", ctx.parent_span_id)
    _emit(event)


@dataclass
class Span:
    """One in-flight span; :meth:`end` emits it exactly once."""

    name: str
    ctx: TraceContext
    attrs: dict[str, Any] = field(default_factory=dict)
    start: float = field(default_factory=time.monotonic)
    status: str = "ok"
    error: str | None = None
    ended: bool = False
    started_emitted: bool = False
    _lifecycle_lock: threading.Lock = field(default_factory=threading.Lock, repr=False)

    @property
    def trace_id(self) -> str:
        return self.ctx.trace_id

    @property
    def span_id(self) -> str:
        return self.ctx.span_id

    @property
    def parent_span_id(self) -> str | None:
        return self.ctx.parent_span_id

    def emit_start(self) -> None:
        """Emit ``span_start`` once with a snapshot of the current attributes."""
        with self._lifecycle_lock:
            if self.started_emitted or self.ended:
                return
            self.started_emitted = True
        event: dict[str, Any] = {
            "event": "trace",
            "msg": "span_start",
            "name": self.name,
            "traceId": self.ctx.trace_id,
            "spanId": self.ctx.span_id,
            "attrs": dict(self.attrs),
        }
        if self.ctx.parent_span_id is not None:
            event["parentSpanId"] = self.ctx.parent_span_id
        _emit(event)

    def end(self, status: str | None = None, error: BaseException | str | None = None) -> None:
        """Finish the span and emit ``span_end``; later calls are ignored."""
        with self._lifecycle_lock:
            if self.ended:
                return
            self.ended = True
        if status is not None:
            self.status = status
        if error is not None:
            self.status = "error"
            self.error = error if isinstance(error, str) else f"{type(error).__name__}: {error}"
        # Same precision as the TypeScript spans (3 decimals) so trees read uniformly.
        duration_ms = round((time.monotonic() - self.start) * 1000.0, 3)
        event: dict[str, Any] = {
            "event": "trace",
            "msg": "span_end",
            "name": self.name,
            "traceId": self.ctx.trace_id,
            "spanId": self.ctx.span_id,
        }
        if self.ctx.parent_span_id is not None:
            event["parentSpanId"] = self.ctx.parent_span_id
        event["durationMs"] = duration_ms
        event["status"] = "error" if self.status == "error" else "ok"
        attrs = dict(self.attrs)
        if self.error is not None:
            attrs["error"] = self.error
        event["attrs"] = attrs
        _emit(event)


def child_context(parent: TraceContext | None = None) -> TraceContext:
    """A child of ``parent`` (default: the current context), or a fresh trace."""
    if parent is None:
        return TraceContext(trace_id=new_trace_id(), span_id=new_span_id())
    return TraceContext(
        trace_id=parent.trace_id,
        span_id=new_span_id(),
        flags=parent.flags,
        parent_span_id=parent.span_id,
    )


@contextlib.contextmanager
def start_span(name: str, **attrs: Any) -> Iterator[Span]:
    """Run the block as a child span of the current context (or a new trace).

    The span is current inside the block and the previous context is restored
    on exit. An escaping exception marks the span ``error`` and propagates.
    Set ``span.status = "error"`` inside the block to report a handled failure.
    """
    span = Span(name=name, ctx=child_context(current()), attrs=dict(attrs))
    if name == "kernel.cell":
        span.emit_start()
    token = set_current(span.ctx)
    try:
        yield span
    except BaseException as exc:
        span.end(error=exc)
        raise
    else:
        span.end()
    finally:
        reset(token)


def inject_env(env: dict[str, str] | None = None) -> dict[str, str]:
    """Copy ``env`` (default ``os.environ``) with ``TRACEPARENT`` set from the current context.

    Without a current context the copy is returned unchanged, so an inherited
    ``TRACEPARENT`` still flows through to the child process.
    """
    copy = dict(os.environ if env is None else env)
    ctx = current()
    if ctx is not None:
        copy[TRACEPARENT_ENV] = format_traceparent(ctx)
    return copy


def from_env(env: dict[str, str] | None = None) -> TraceContext | None:
    """The context carried by ``TRACEPARENT`` in ``env`` (default ``os.environ``), if valid."""
    source = os.environ if env is None else env
    return parse_traceparent(source.get(TRACEPARENT_ENV))
