"""The workflow script API: deterministic multi-agent orchestration from the kernel.

Claude Code's Workflow tool runs a JavaScript script whose ``agent()``,
``pipeline()``, ``parallel()``, ``phase()`` and ``log()`` hooks fan work out
to subagents. Here the script is an ``async`` Python function and every
``agent()`` call is one ``rlm.spawn`` child collected with ``rlm.collect``:

    async def main(wf):
        wf.phase("Review")
        found = await wf.parallel([
            lambda d=d: wf.agent(d, label=f"review:{i}", schema=FINDINGS)
            for i, d in enumerate(DIMENSIONS)
        ])
        return [f for r in found if r for f in r["findings"]]

    result = await rlm.workflow.run(main, name="review-changes")

The semantics follow Claude Code's runtime:

- ``pipeline(items, *stages)`` runs each item through every stage with no
  barrier between stages; a stage that raises drops that item to ``None``.
- ``parallel(thunks)`` is a barrier that never raises: a failed thunk is
  ``None`` in the result.
- Every run journals its agents to ``<session dir>/workflows/<run id>/
  journal.jsonl``. The journal key of one ``agent()`` call is
  ``"v2:" + sha256(previous key, prompt, the call's result-shaping options)``,
  chained in call order, so ``resume_from=<run id>`` replays the longest
  unchanged prefix of calls from the journal and runs everything from the
  first changed call on live.
- At most ``min(16, cpus - 2)`` agents run at once, 1000 per run, 4096 items
  per ``pipeline``/``parallel`` call; ``workflow()`` nests one level deep.

``start()`` runs a workflow as a background task of the kernel and delivers
a completion notice on the session's monitor lane (``[monitor wf_... ...]``),
so the model keeps working instead of waiting.
"""

from __future__ import annotations

import asyncio
import contextvars
import hashlib
import inspect
import json
import math
import os
import re
import secrets
import tempfile
import time
from collections.abc import Awaitable, Callable, Iterable
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Protocol

from . import workflow_schema

__all__ = [
    "AgentOutcome",
    "AgentRequest",
    "Budget",
    "WorkflowBudgetExceeded",
    "WorkflowContext",
    "WorkflowError",
    "WorkflowHandle",
    "WorkflowResult",
    "agent",
    "get_budget",
    "get_args",
    "journal_key",
    "load_journal",
    "log",
    "parallel",
    "phase",
    "pipeline",
    "result",
    "run",
    "runs",
    "size_guideline",
    "start",
    "status",
    "workflow",
]

KEY_VERSION = "v2"
# The agent() options that change what a call returns: the journal key
# covers exactly these (Claude Code's list; label and phase are display-only).
KEY_OPTIONS = (
    "schema",
    "model",
    "effort",
    "isolation",
    "agentType",
    "disallowedTools",
    "bashCommandClamp",
)
RUN_ID_PATTERN = re.compile(r"^wf_[a-z0-9-]{6,}$")
MAX_AGENTS = 1000
MAX_ITEMS = 4096
MAX_SCHEMA_REPAIRS = 2
EFFORTS = ("low", "medium", "high", "xhigh", "max")
SIZE_GUIDELINES: dict[str, int | None] = {"small": 5, "medium": 10, "large": 50, "unrestricted": None}
DEFAULT_SIZE_GUIDELINE = "medium"
_COLLECT_WAIT_MS = 60_000
_RESULT_PREVIEW_CHARS = 600

_CURRENT: contextvars.ContextVar[WorkflowContext | None] = contextvars.ContextVar(
    "rlm_workflow_context", default=None
)
_RUNS: dict[str, WorkflowHandle] = {}


class WorkflowError(RuntimeError):
    """A workflow call broke one of the runtime's rules (caps, nesting, arguments)."""


class WorkflowBudgetExceeded(WorkflowError):
    """``agent()`` was called after the run's token budget was spent."""


# --- journal keys ---------------------------------------------------------


def _is_array_index(key: str) -> bool:
    return key.isdigit() and (key == "0" or not key.startswith("0")) and int(key) < 2**32 - 1


def _js_key_order(keys: Iterable[str]) -> list[str]:
    """``Object.keys(o).sort()`` re-inserted into a fresh object, as JS orders it.

    A JS object lists array-index keys first in numeric order, then the
    other keys in insertion (here: sorted) order.
    """
    ordered = sorted(keys)
    indices = sorted((key for key in ordered if _is_array_index(key)), key=int)
    return indices + [key for key in ordered if not _is_array_index(key)]


def _canonical(value: Any) -> Any:
    if callable(value) and not isinstance(value, (dict, list, str)):
        return None
    if isinstance(value, (list, tuple)):
        return [_canonical(item) for item in value]
    if isinstance(value, dict):
        named = {str(name): item for name, item in value.items()}
        out: dict[str, Any] = {}
        for key in _js_key_order(named):
            item = named[key]
            if key == "__proto__" or (callable(item) and not isinstance(item, (dict, list, str))):
                continue
            out[key] = _canonical(item)
        return out
    if isinstance(value, float) and math.isfinite(value) and value.is_integer():
        # JSON.stringify(1.0) is "1".
        return int(value)
    return value


def _js_json(value: Any) -> str:
    # Keys are already ordered: no sort_keys (it would undo the array-index order).
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"))


def canonical_options(options: dict[str, Any] | None) -> str:
    """The option part of a journal key: Claude Code's ``Hr`` over :data:`KEY_OPTIONS`."""
    if not options:
        return "{}"
    picked: dict[str, Any] = {}
    for name in KEY_OPTIONS:
        value = options.get(name)
        if value is None or (callable(value) and not isinstance(value, (dict, list, str))):
            continue
        picked[name] = value
    return _js_json(_canonical(picked))


def journal_key(previous: str, prompt: str, options: dict[str, Any] | None) -> str:
    """One ``agent()`` call's journal key, chained to the previous call's key."""
    digest = hashlib.sha256()
    digest.update(previous.encode("utf-8"))
    digest.update(b"\0")
    digest.update(prompt.encode("utf-8"))
    digest.update(b"\0")
    digest.update(canonical_options(options).encode("utf-8"))
    return f"{KEY_VERSION}:{digest.hexdigest()}"


# --- the journal ----------------------------------------------------------


@dataclass
class JournalState:
    """A journal read back: each key's newest result, its start lines, and the failed keys."""

    results: dict[str, dict[str, Any]] = field(default_factory=dict)
    started: dict[str, list[dict[str, Any]]] = field(default_factory=dict)
    failed: set[str] = field(default_factory=set)


class Journal:
    """``journal.jsonl``: one ``started``/``result``/``failed`` line per agent event."""

    def __init__(self, path: Path) -> None:
        self.path = path

    def load(self) -> JournalState:
        state = JournalState()
        try:
            text = self.path.read_text(encoding="utf-8")
        except FileNotFoundError:
            return state
        for line in text.splitlines():
            if not line.strip():
                continue
            try:
                entry = json.loads(line)
            except ValueError:
                continue
            if not isinstance(entry, dict) or not isinstance(entry.get("key"), str):
                continue
            kind = entry.get("type")
            if kind == "result":
                state.results[entry["key"]] = entry
            elif kind == "started":
                state.started.setdefault(entry["key"], []).append(entry)
            elif kind == "failed":
                state.failed.add(entry["key"])
        return state

    def append(self, entry: dict[str, Any]) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        with self.path.open("a", encoding="utf-8") as handle:
            handle.write(json.dumps(entry, ensure_ascii=False, default=str) + "\n")


# --- the agent backend ----------------------------------------------------


@dataclass(frozen=True)
class AgentRequest:
    """One child run the backend starts and waits for."""

    prompt: str
    name: str
    model: str | None = None
    effort: str | None = None
    cwd: str | None = None
    # ``"worktree"``: the child runs in its own git worktree, which the host
    # cuts and removes (``rlm.spawn(isolation="worktree")``).
    isolation: str | None = None


@dataclass(frozen=True)
class AgentOutcome:
    """A finished child: its final answer, or ``text=None`` with the error."""

    text: str | None
    agent_id: str = ""
    error: str | None = None
    tokens: int | None = None


class AgentBackend(Protocol):
    async def run(self, request: AgentRequest) -> AgentOutcome: ...


class RlmBackend:
    """Runs each agent as an ``rlm.spawn`` child and waits on ``rlm.collect``.

    An isolated agent (``isolation="worktree"``) runs in the managed worktree
    the host cuts for it (``agent-<id>`` from ``HEAD``); once it settles the
    child is deleted, which removes that worktree when it holds no work. A
    changed worktree stays, named in the child's task notification.
    """

    async def run(self, request: AgentRequest) -> AgentOutcome:
        from . import collect, spawn

        kwargs: dict[str, Any] = {"name": request.name}
        if request.model is not None:
            kwargs["model"] = request.model
        if request.cwd is not None:
            kwargs["cwd"] = request.cwd
        if request.isolation is not None:
            kwargs["isolation"] = request.isolation
        if request.effort is not None:
            kwargs["thinking"] = request.effort
        try:
            handle = await spawn(request.prompt, **kwargs)
        except Exception as error:
            if "thinking" not in kwargs or "thinking" not in str(error).lower():
                raise
            # A level the child's model does not support: run at its default.
            del kwargs["thinking"]
            handle = await spawn(request.prompt, **kwargs)
        try:
            while True:
                results = await collect(handle, timeout_ms=_COLLECT_WAIT_MS)
                row = results[0] if results else None
                if row is not None and row.settled:
                    break
        finally:
            if request.isolation is not None:
                await _release_isolated_child(handle)
        if row.status == "done":
            return AgentOutcome(
                text=row.answer_text if row.answer_text is not None else row.answer_preview or "",
                agent_id=handle.rlm_child_id,
            )
        return AgentOutcome(text=None, agent_id=handle.rlm_child_id, error=row.error or row.status)


async def _release_isolated_child(handle: Any) -> None:
    """Delete a settled (or abandoned) isolated child so the host removes its unchanged worktree."""
    from . import delete_subagent

    try:
        await delete_subagent(handle)
    except Exception:  # noqa: BLE001 - the worktree merely stays; the run goes on
        pass


# --- budget ---------------------------------------------------------------


class Budget:
    """The run's token target: ``total`` (None when unset), ``spent()``, ``remaining()``.

    ``spent()`` counts the usage the backend reports per agent (the default
    ``rlm`` backend reports none; bound children with ``rlm.spawn``'s
    delegation budget instead). Once ``spent()`` reaches ``total``, further
    ``agent()`` calls raise :class:`WorkflowBudgetExceeded`.
    """

    def __init__(self, total: int | None) -> None:
        if total is not None and (not isinstance(total, int) or isinstance(total, bool) or total <= 0):
            raise TypeError("budget must be a positive int or None")
        self.total = total
        self._spent = 0

    def spent(self) -> int:
        return self._spent

    def remaining(self) -> float:
        if self.total is None:
            return math.inf
        return max(0, self.total - self._spent)

    def _add(self, tokens: int | None) -> None:
        if tokens:
            self._spent += int(tokens)

    def __repr__(self) -> str:
        return f"Budget(total={self.total}, spent={self._spent})"


# --- settings -------------------------------------------------------------


def size_guideline() -> tuple[str, int | None]:
    """The ``workflowSizeGuideline`` setting: its name and agent count (None when unrestricted)."""
    name = DEFAULT_SIZE_GUIDELINE
    agent_dir = os.environ.get("PRIME_AGENT_CODING_AGENT_DIR")
    if agent_dir:
        try:
            settings = json.loads((Path(agent_dir) / "settings.json").read_text(encoding="utf-8"))
        except (OSError, ValueError):
            settings = None
        value = settings.get("workflowSizeGuideline") if isinstance(settings, dict) else None
        if value in SIZE_GUIDELINES:
            name = value
    return name, SIZE_GUIDELINES[name]


# --- runs -----------------------------------------------------------------


def _new_run_id() -> str:
    alphabet = "abcdefghijklmnopqrstuvwxyz0123456789"
    return "wf_" + "".join(secrets.choice(alphabet) for _ in range(12))


def _workflows_dir() -> Path:
    session_dir = os.environ.get("RLM_SESSION_DIR")
    if session_dir:
        return Path(session_dir) / "workflows"
    return Path(tempfile.gettempdir()) / "prime-agent-workflows"


def _run_dir(run_id: str) -> Path:
    return _workflows_dir() / run_id


def _slug(text: str, limit: int) -> str:
    slug = re.sub(r"[^a-z0-9]+", "-", text.lower()).strip("-")
    return slug[:limit].strip("-")


def _preview(value: Any, limit: int = _RESULT_PREVIEW_CHARS) -> str:
    try:
        text = value if isinstance(value, str) else json.dumps(value, ensure_ascii=False, default=str)
    except (TypeError, ValueError):
        text = repr(value)
    return text if len(text) <= limit else text[:limit] + " ..."


def _emit_progress(text: str) -> None:
    from . import repl

    if not repl.is_active() or repl._current_cell.get() is None:
        return
    try:
        repl.emit({"text/plain": text})
    except Exception:
        pass


@dataclass
class AgentRecord:
    """One ``agent()`` call as the run saw it (``WorkflowResult.agents``)."""

    index: int
    label: str
    phase: str | None
    key: str
    state: str = "queued"
    cached: bool = False
    agent_id: str = ""
    error: str | None = None


@dataclass
class WorkflowResult:
    """A finished workflow: what the script returned, or why it did not."""

    run_id: str
    name: str
    status: str
    result: Any = None
    error: str | None = None
    agents: list[AgentRecord] = field(default_factory=list)
    logs: list[str] = field(default_factory=list)
    run_dir: Path | None = None
    duration_s: float = 0.0

    @property
    def journal_path(self) -> Path | None:
        return None if self.run_dir is None else self.run_dir / "journal.jsonl"

    @property
    def cached(self) -> int:
        return sum(1 for record in self.agents if record.cached)

    def summary(self) -> str:
        cached = f", {self.cached} cached" if self.cached else ""
        failed = sum(1 for record in self.agents if record.state == "failed")
        failures = f", {failed} failed" if failed else ""
        head = (
            f"workflow {self.name} ({self.run_id}) {self.status}: "
            f"{len(self.agents)} agents{cached}{failures} in {self.duration_s:.1f}s"
        )
        if self.error:
            return f"{head}\nerror: {self.error}"
        return head

    def __repr__(self) -> str:
        lines = [self.summary()]
        if self.status == "completed":
            lines.append(f"result: {_preview(self.result)}")
        if self.journal_path is not None:
            lines.append(f"journal: {self.journal_path}")
        return "\n".join(lines)


class _Run:
    """The state one workflow run shares across its script and any nested workflow."""

    def __init__(
        self,
        *,
        run_id: str,
        name: str,
        backend: AgentBackend,
        journal: Journal,
        replay: JournalState,
        concurrency: int,
        budget: Budget,
        background: bool,
    ) -> None:
        self.run_id = run_id
        self.name = name
        self.backend = backend
        self.journal = journal
        self.replay = replay
        self.semaphore = asyncio.Semaphore(concurrency)
        self.budget = budget
        self.background = background
        self.previous_key = ""
        # Claude Code's `T`: once a call misses the journal, every later call runs live.
        self.live = False
        self.records: list[AgentRecord] = []
        self.logs: list[str] = []
        self.phase: str | None = None
        self.guideline_name, self.guideline_count = size_guideline()
        self.guideline_warned = False

    def note(self, message: str) -> None:
        self.logs.append(message)
        if not self.background:
            _emit_progress(f"[workflow {self.name}] {message}")


def _call_stage(stage: Callable[..., Any], previous: Any, item: Any, index: int) -> Any:
    """Call a pipeline stage with as many of ``(previous, item, index)`` as it takes."""
    try:
        parameters = inspect.signature(stage).parameters.values()
    except (TypeError, ValueError):
        return stage(previous, item, index)
    if any(parameter.kind == inspect.Parameter.VAR_POSITIONAL for parameter in parameters):
        return stage(previous, item, index)
    positional = [
        parameter
        for parameter in parameters
        if parameter.kind in (inspect.Parameter.POSITIONAL_ONLY, inspect.Parameter.POSITIONAL_OR_KEYWORD)
    ]
    return stage(*(previous, item, index)[: max(1, len(positional))])


async def _resolved(value: Any) -> Any:
    # A cached result still yields once, like an awaited JS promise: a
    # replayed pipeline reaches its later stages in the same call order as
    # a live one.
    await asyncio.sleep(0)
    return value


class WorkflowContext:
    """What a script sees: ``agent``, ``pipeline``, ``parallel``, ``phase``, ``log``, ``args``, ``budget``, ``workflow``."""

    def __init__(self, run: _Run, args: Any, *, depth: int = 0) -> None:
        self._run = run
        self.args = args
        self._depth = depth

    @property
    def run_id(self) -> str:
        return self._run.run_id

    @property
    def budget(self) -> Budget:
        return self._run.budget

    def phase(self, title: str) -> None:
        """Start a phase: later ``agent()`` calls without ``phase=`` belong to it."""
        self._run.phase = str(title)
        self._run.note(f"phase: {title}")

    def log(self, message: str) -> None:
        """Record a progress line (shown in the cell while the run is in the foreground)."""
        self._run.note(str(message))

    def agent(
        self,
        prompt: str,
        *,
        label: str | None = None,
        phase: str | None = None,
        schema: dict[str, Any] | None = None,
        model: str | None = None,
        effort: str | None = None,
        isolation: str | None = None,
        agent_type: str | None = None,
    ) -> asyncio.Future[Any]:
        """Spawn one subagent; await the returned future for its final text (or the
        schema-validated object). It resolves to ``None`` when the child fails."""
        run = self._run
        if not isinstance(prompt, str):
            raise TypeError(f"agent() prompt must be str, got {type(prompt).__name__}")
        if effort is not None and effort not in EFFORTS:
            raise ValueError(f"agent() effort must be one of {', '.join(EFFORTS)}")
        if isolation not in (None, "worktree"):
            raise ValueError("agent() isolation must be 'worktree' or None")
        if agent_type is not None:
            raise WorkflowError(
                "agent() agent_type needs typed agent definitions, which this build does not have"
            )
        if schema is not None:
            workflow_schema.check_schema(schema)
        if run.budget.total is not None and run.budget.spent() >= run.budget.total:
            raise WorkflowBudgetExceeded(
                f"the workflow's token budget ({run.budget.total}) is spent; agent() refused"
            )
        if len(run.records) >= MAX_AGENTS:
            raise WorkflowError(f"a workflow runs at most {MAX_AGENTS} agents")
        options = {"schema": schema, "model": model, "effort": effort, "isolation": isolation}
        key = journal_key(run.previous_key, prompt, options)
        run.previous_key = key
        index = len(run.records) + 1
        text_label = (
            " ".join(str(label).split()) if label is not None else " ".join(prompt.split())[:60]
        )
        record = AgentRecord(index=index, label=text_label, phase=phase or run.phase, key=key)
        run.records.append(record)
        if (
            run.guideline_count is not None
            and index > run.guideline_count
            and not run.guideline_warned
        ):
            run.guideline_warned = True
            run.note(
                f"this run passed the {run.guideline_name} workflow size guideline "
                f"({run.guideline_count} agents); it is a guideline, not a limit"
            )
        cached = None if run.live else run.replay.results.get(key)
        if cached is not None:
            record.state, record.cached = "done", True
            record.agent_id = str(cached.get("agentId") or "")
            return asyncio.ensure_future(_resolved(cached.get("result")))
        started = run.replay.started.get(key)
        if not (not run.live and started and key not in run.replay.failed):
            run.live = True
        return asyncio.ensure_future(
            self._run_agent(record, prompt, schema=schema, model=model, effort=effort, isolation=isolation)
        )

    async def _run_agent(
        self,
        record: AgentRecord,
        prompt: str,
        *,
        schema: dict[str, Any] | None,
        model: str | None,
        effort: str | None,
        isolation: str | None,
    ) -> Any:
        run = self._run
        async with run.semaphore:
            record.state = "running"
            name = f"wf-{run.run_id[3:11]}-{record.index}"
            label_slug = _slug(record.label, 30)
            if label_slug:
                name = f"{name}-{label_slug}"
            run.journal.append(
                {
                    "type": "started",
                    "key": record.key,
                    "agentId": "",
                    "label": record.label,
                    "phase": record.phase,
                }
            )
            try:
                outcome = await run.backend.run(
                    AgentRequest(
                        prompt=_child_prompt(prompt, schema),
                        name=name,
                        model=model,
                        effort=effort,
                        isolation=isolation,
                    )
                )
                record.agent_id = outcome.agent_id
                run.budget._add(outcome.tokens)
                value: Any = None
                if outcome.text is not None:
                    value = outcome.text
                    if schema is not None:
                        value = await self._structured(record, name, outcome.text, schema, model)
                if value is None:
                    record.state = "failed"
                    record.error = record.error or outcome.error or "the agent returned no result"
                    run.note(f"agent {record.label!r} failed: {record.error}")
                    run.journal.append({"type": "failed", "key": record.key, "agentId": record.agent_id})
                    return None
                record.state = "done"
                run.journal.append(
                    {"type": "result", "key": record.key, "agentId": record.agent_id, "result": value}
                )
                return value
            except asyncio.CancelledError:
                record.state = "cancelled"
                raise
            except Exception as error:
                record.state = "failed"
                record.error = f"{type(error).__name__}: {error}"
                run.journal.append({"type": "failed", "key": record.key, "agentId": record.agent_id})
                raise

    async def _structured(
        self, record: AgentRecord, name: str, text: str, schema: dict[str, Any], model: str | None
    ) -> Any:
        """The child's JSON answer, validated; a mismatch asks a repair agent for a corrected one."""
        run = self._run
        answer = text
        for attempt in range(MAX_SCHEMA_REPAIRS + 1):
            try:
                value = workflow_schema.extract_json(answer)
                problems = workflow_schema.validate(value, schema)
            except ValueError as error:
                value, problems = None, [str(error)]
            if not problems:
                return value
            if attempt == MAX_SCHEMA_REPAIRS:
                record.error = "the answer does not match the schema: " + "; ".join(problems[:5])
                return None
            run.note(f"agent {record.label!r}: answer does not match the schema, asking for a fix")
            outcome = await run.backend.run(
                AgentRequest(
                    prompt=_repair_prompt(answer, schema, problems),
                    name=f"{name[:56]}-fix{attempt + 1}",
                    model=model,
                    effort="low",
                )
            )
            run.budget._add(outcome.tokens)
            if outcome.text is None:
                record.error = outcome.error or "the repair agent returned no result"
                return None
            answer = outcome.text
        return None

    async def pipeline(self, items: Iterable[Any], *stages: Callable[..., Any]) -> list[Any]:
        """Run each item through every stage, no barrier between stages; a stage that
        raises drops that item to ``None``. Each stage gets ``(previous, item, index)``."""
        values = list(items)
        if len(values) > MAX_ITEMS:
            raise WorkflowError(f"pipeline() takes at most {MAX_ITEMS} items, got {len(values)}")
        if not stages:
            raise WorkflowError("pipeline() needs at least one stage")

        async def chain(index: int, item: Any) -> Any:
            value = item
            for stage in stages:
                try:
                    value = _call_stage(stage, value, item, index)
                    if inspect.isawaitable(value):
                        value = await value
                except asyncio.CancelledError:
                    raise
                except Exception as error:
                    self._run.note(f"pipeline item {index} dropped: {type(error).__name__}: {error}")
                    return None
            return value

        return list(await asyncio.gather(*(chain(index, item) for index, item in enumerate(values))))

    async def parallel(self, thunks: Iterable[Callable[[], Any]]) -> list[Any]:
        """Run every thunk concurrently and wait for all (a barrier); never raises —
        a failed thunk is ``None`` in the result."""
        calls = list(thunks)
        if len(calls) > MAX_ITEMS:
            raise WorkflowError(f"parallel() takes at most {MAX_ITEMS} thunks, got {len(calls)}")

        async def one(index: int, thunk: Callable[[], Any]) -> Any:
            try:
                value = thunk()
                if inspect.isawaitable(value):
                    value = await value
                return value
            except asyncio.CancelledError:
                raise
            except Exception as error:
                self._run.note(f"parallel task {index} failed: {type(error).__name__}: {error}")
                return None

        return list(await asyncio.gather(*(one(index, thunk) for index, thunk in enumerate(calls))))

    async def workflow(self, script: Any, args: Any = None) -> Any:
        """Run another workflow script inline as a sub-step (one level deep); it shares
        this run's journal chain, concurrency cap, agent count and budget."""
        if self._depth >= 1:
            raise WorkflowError("workflow() nests one level only")
        function, meta = _load_script(script)
        child = WorkflowContext(self._run, args, depth=self._depth + 1)
        self._run.note(f"> {meta.get('name') or getattr(function, '__name__', 'workflow')}")
        return await _invoke(function, child)


def _child_prompt(prompt: str, schema: dict[str, Any] | None) -> str:
    preamble = (
        "You are a workflow subagent. Your final message is returned verbatim to the "
        "orchestrating script as data, not shown to a person: end with the result itself, "
        "no greeting or summary of what you did."
    )
    text = f"{preamble}\n\n{prompt}"
    if schema is not None:
        text += (
            "\n\nYour final message must end with one JSON value that validates against this "
            "JSON Schema, in a ```json fenced block, with nothing after the block:\n"
            f"{json.dumps(schema, indent=2, ensure_ascii=False)}"
        )
    return text


def _repair_prompt(answer: str, schema: dict[str, Any], problems: list[str]) -> str:
    listed = "\n".join(f"- {problem}" for problem in problems[:20])
    return (
        "An agent's answer had to be one JSON value matching the JSON Schema below, and it "
        "does not. Do not redo the agent's work: rewrite its answer as the corrected JSON "
        "value, keeping its content. Reply with only that value in a ```json fenced block.\n\n"
        f"Problems:\n{listed}\n\nSchema:\n{json.dumps(schema, indent=2, ensure_ascii=False)}\n\n"
        f"The answer:\n{answer}"
    )


def _load_script(script: Any) -> tuple[Callable[..., Any], dict[str, Any]]:
    """A script is an async callable, or the path of a ``.py`` file defining ``main``
    (and optionally a ``meta`` dict)."""
    if isinstance(script, (str, Path)):
        path = Path(script).expanduser()
        try:
            source = path.read_text(encoding="utf-8")
        except OSError as error:
            raise WorkflowError(f"cannot read the workflow script {path}: {error}") from error
        namespace: dict[str, Any] = {"__name__": f"workflow_{_slug(path.stem, 40) or 'script'}"}
        exec(compile(source, str(path), "exec"), namespace)
        function = namespace.get("main")
        if not callable(function):
            raise WorkflowError(f"the workflow script {path} defines no main(wf)")
        meta = namespace.get("meta")
        return function, dict(meta) if isinstance(meta, dict) else {}
    if callable(script):
        meta = getattr(script, "meta", None)
        return script, dict(meta) if isinstance(meta, dict) else {}
    raise TypeError("a workflow script is an async callable or the path of a .py file")


async def _invoke(function: Callable[..., Any], context: WorkflowContext) -> Any:
    token = _CURRENT.set(context)
    try:
        try:
            takes_context = any(
                parameter.kind
                in (
                    inspect.Parameter.POSITIONAL_ONLY,
                    inspect.Parameter.POSITIONAL_OR_KEYWORD,
                    inspect.Parameter.VAR_POSITIONAL,
                )
                for parameter in inspect.signature(function).parameters.values()
            )
        except (TypeError, ValueError):
            takes_context = True
        value = function(context) if takes_context else function()
        if inspect.isawaitable(value):
            value = await value
        return value
    finally:
        _CURRENT.reset(token)


def _script_source(function: Callable[..., Any], script: Any) -> str | None:
    if isinstance(script, (str, Path)):
        try:
            return Path(script).expanduser().read_text(encoding="utf-8")
        except OSError:
            return None
    try:
        return inspect.getsource(function)
    except (OSError, TypeError):
        return None


def _write_run_record(run_dir: Path, record: dict[str, Any]) -> None:
    run_dir.mkdir(parents=True, exist_ok=True)
    temporary = run_dir / "run.json.tmp"
    temporary.write_text(json.dumps(record, indent=2, ensure_ascii=False, default=str), encoding="utf-8")
    os.replace(temporary, run_dir / "run.json")


def _default_concurrency() -> int:
    return max(1, min(16, (os.cpu_count() or 4) - 2))


async def _execute(
    script: Any,
    *,
    args: Any,
    name: str | None,
    resume_from: str | None,
    budget: int | None,
    concurrency: int | None,
    backend: AgentBackend | None,
    background: bool,
    run_id: str,
) -> WorkflowResult:
    function, meta = _load_script(script)
    run_name = name or meta.get("name") or getattr(function, "__name__", None) or "workflow"
    run_dir = _run_dir(run_id)
    journal = Journal(run_dir / "journal.jsonl")
    replay = journal.load() if resume_from is not None else JournalState()
    if resume_from is not None and not (replay.results or replay.started):
        raise WorkflowError(f"no journal found for workflow run {resume_from}, so there is nothing to resume")
    if concurrency is not None and (not isinstance(concurrency, int) or isinstance(concurrency, bool) or concurrency < 1):
        raise TypeError("concurrency must be a positive int")
    state = _Run(
        run_id=run_id,
        name=str(run_name),
        backend=backend or RlmBackend(),
        journal=journal,
        replay=replay,
        concurrency=concurrency or _default_concurrency(),
        budget=Budget(budget),
        background=background,
    )
    source = _script_source(function, script)
    run_dir.mkdir(parents=True, exist_ok=True)
    if source is not None:
        (run_dir / "script.py").write_text(source, encoding="utf-8")
    started_at = time.time()
    record: dict[str, Any] = {
        "runId": run_id,
        "name": state.name,
        "description": meta.get("description"),
        "status": "running",
        "startedAt": started_at,
        "resumedFrom": resume_from,
        "args": args,
    }
    _write_run_record(run_dir, record)
    context = WorkflowContext(state, args)
    status, value, error = "completed", None, None
    try:
        value = await _invoke(function, context)
    except asyncio.CancelledError:
        status, error = "cancelled", "the workflow was cancelled"
        raise
    except Exception as failure:
        status, error = "failed", f"{type(failure).__name__}: {failure}"
    finally:
        outcome = WorkflowResult(
            run_id=run_id,
            name=state.name,
            status=status,
            result=value,
            error=error,
            agents=list(state.records),
            logs=list(state.logs),
            run_dir=run_dir,
            duration_s=time.time() - started_at,
        )
        record.update(
            status=status,
            endedAt=time.time(),
            error=error,
            agents=len(state.records),
            cached=outcome.cached,
            resultPreview=_preview(value) if status == "completed" else None,
        )
        try:
            _write_run_record(run_dir, record)
        except OSError:
            pass
    return outcome


async def run(
    script: Any,
    *,
    args: Any = None,
    name: str | None = None,
    resume_from: str | None = None,
    budget: int | None = None,
    concurrency: int | None = None,
    backend: AgentBackend | None = None,
) -> WorkflowResult:
    """Run a workflow script in the foreground and return its :class:`WorkflowResult`.

    ``resume_from`` re-runs a previous run (same run id, same journal): the
    longest unchanged prefix of its ``agent()`` calls returns the journaled
    results instantly and the first changed call onward runs live.
    """
    run_id = _resume_id(resume_from) or _new_run_id()
    return await _execute(
        script,
        args=args,
        name=name,
        resume_from=resume_from,
        budget=budget,
        concurrency=concurrency,
        backend=backend,
        background=False,
        run_id=run_id,
    )


def _resume_id(resume_from: str | None) -> str | None:
    if resume_from is None:
        return None
    if not isinstance(resume_from, str) or not RUN_ID_PATTERN.match(resume_from):
        raise ValueError("resume_from must be a workflow run id like 'wf_abc123def456'")
    return resume_from


@dataclass
class WorkflowHandle:
    """A background workflow: ``run_id``, ``done()``, ``await handle.result()``, ``cancel()``."""

    run_id: str
    name: str
    task: asyncio.Task[WorkflowResult]

    def done(self) -> bool:
        return self.task.done()

    async def result(self) -> WorkflowResult:
        return await asyncio.shield(self.task)

    def cancel(self) -> bool:
        return self.task.cancel()

    def __repr__(self) -> str:
        state = "done" if self.task.done() else "running"
        return (
            f"WorkflowHandle({self.run_id}, {self.name!r}, {state}): a [monitor {self.run_id} ...] "
            "notice arrives when it ends; `await rlm.workflow.result(run_id)` reads it"
        )


async def _notify(outcome: WorkflowResult) -> None:
    from . import repl

    lines = [outcome.summary().splitlines()[0]]
    if outcome.status == "completed":
        lines.append(f"result: {_preview(outcome.result, 400)}")
    if outcome.error:
        lines.append(f"error: {outcome.error}")
    lines.append(f"read it with: await rlm.workflow.result({outcome.run_id!r})")
    end: dict[str, Any] = {"reason": outcome.status}
    if outcome.error:
        end["error"] = outcome.error
    try:
        await repl.host_request(
            {
                "type": "monitor.event",
                "monitorId": outcome.run_id,
                "description": f"workflow {outcome.name}",
                "lines": lines,
                "dropped": 0,
                "end": end,
            }
        )
    except Exception:
        # No host lane (a headless kernel): the handle still holds the result.
        pass


def start(
    script: Any,
    *,
    args: Any = None,
    name: str | None = None,
    resume_from: str | None = None,
    budget: int | None = None,
    concurrency: int | None = None,
    backend: AgentBackend | None = None,
    notify: bool = True,
) -> WorkflowHandle:
    """Start a workflow in the background and return its :class:`WorkflowHandle` at once.

    The run outlives the cell. When it ends, a ``[monitor wf_... "workflow
    <name>"]`` notice joins the running turn or wakes the idle session.
    """
    run_id = _resume_id(resume_from) or _new_run_id()
    function, meta = _load_script(script)
    run_name = str(name or meta.get("name") or getattr(function, "__name__", None) or "workflow")

    async def body() -> WorkflowResult:
        try:
            outcome = await _execute(
                script,
                args=args,
                name=run_name,
                resume_from=resume_from,
                budget=budget,
                concurrency=concurrency,
                backend=backend,
                background=True,
                run_id=run_id,
            )
        except asyncio.CancelledError:
            outcome = WorkflowResult(run_id=run_id, name=run_name, status="cancelled", error="cancelled", run_dir=_run_dir(run_id))
            if notify:
                await _notify(outcome)
            raise
        except Exception as error:
            outcome = WorkflowResult(
                run_id=run_id,
                name=run_name,
                status="failed",
                error=f"{type(error).__name__}: {error}",
                run_dir=_run_dir(run_id),
            )
        if notify:
            await _notify(outcome)
        return outcome

    task = asyncio.get_running_loop().create_task(body())
    handle = WorkflowHandle(run_id=run_id, name=run_name, task=task)
    _RUNS[run_id] = handle
    return handle


async def result(run_id: str) -> WorkflowResult:
    """The result of a background run started in this kernel (waits while it runs)."""
    handle = _RUNS.get(run_id)
    if handle is None:
        raise WorkflowError(f"no background workflow {run_id} in this kernel; see rlm.workflow.status()")
    return await handle.result()


def status(run_id: str) -> dict[str, Any]:
    """A run's ``run.json`` record (status, agent counts, result preview), from disk."""
    _resume_id(run_id)
    try:
        return json.loads((_run_dir(run_id) / "run.json").read_text(encoding="utf-8"))
    except FileNotFoundError as error:
        raise WorkflowError(f"no workflow run {run_id} under {_workflows_dir()}") from error


def runs() -> list[dict[str, Any]]:
    """Every workflow run recorded for this session, newest first."""
    root = _workflows_dir()
    records = []
    for path in root.glob("wf_*/run.json") if root.is_dir() else []:
        try:
            records.append(json.loads(path.read_text(encoding="utf-8")))
        except (OSError, ValueError):
            continue
    return sorted(records, key=lambda record: record.get("startedAt") or 0, reverse=True)


def load_journal(run_id: str) -> list[dict[str, Any]]:
    """A run's journal lines (each agent's ``started``/``result``/``failed`` record)."""
    _resume_id(run_id)
    path = _run_dir(run_id) / "journal.jsonl"
    if not path.exists():
        raise WorkflowError(f"no journal for workflow run {run_id}")
    entries = []
    for line in path.read_text(encoding="utf-8").splitlines():
        try:
            entries.append(json.loads(line))
        except ValueError:
            continue
    return entries


# --- module-level hooks: the running script's context ---------------------


def _context(what: str) -> WorkflowContext:
    context = _CURRENT.get()
    if context is None:
        raise WorkflowError(f"rlm.workflow.{what}() runs only inside a workflow script")
    return context


def agent(prompt: str, **options: Any) -> asyncio.Future[Any]:
    """``WorkflowContext.agent`` of the running script."""
    return _context("agent").agent(prompt, **options)


async def pipeline(items: Iterable[Any], *stages: Callable[..., Any]) -> list[Any]:
    """``WorkflowContext.pipeline`` of the running script."""
    return await _context("pipeline").pipeline(items, *stages)


async def parallel(thunks: Iterable[Callable[[], Awaitable[Any] | Any]]) -> list[Any]:
    """``WorkflowContext.parallel`` of the running script."""
    return await _context("parallel").parallel(thunks)


def phase(title: str) -> None:
    """``WorkflowContext.phase`` of the running script."""
    _context("phase").phase(title)


def log(message: str) -> None:
    """``WorkflowContext.log`` of the running script."""
    _context("log").log(message)


def get_args() -> Any:
    """The running script's ``args``."""
    return _context("get_args").args


def get_budget() -> Budget:
    """The running script's :class:`Budget`."""
    return _context("get_budget").budget


async def workflow(script: Any, args: Any = None) -> Any:
    """``WorkflowContext.workflow`` of the running script."""
    return await _context("workflow").workflow(script, args)
