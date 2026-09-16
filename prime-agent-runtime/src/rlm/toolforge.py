"""Toolforge: publish a new Python capability from inside a running cell.

``RefinementEdit`` has no field that can carry source code, and both skill
screens require a module that already imports, so the refinement loop cannot
propose code that does not exist yet. Toolforge is the other direction: the
agent writes the source and an exit test, the host stages it as a real package
and runs a double-run gate (the exit test must FAIL against a stub that
implements nothing and PASS against the real package), and only then is the
package promoted into the skills directory, editable-installed and bound here.

The bind happens in this process, in ``__main__``, so the published name is
callable in the SAME cell that published it. The next session gets it from the
skills directory with no step from anyone.
"""

from __future__ import annotations

import importlib
import sys
from dataclasses import dataclass, field
from typing import Any

from .skill import resolve_run

__all__ = ["ToolforgeGateRun", "ToolforgeRejected", "ToolforgeSkill", "publish"]


@dataclass(frozen=True)
class ToolforgeGateRun:
    """One half of the double-run gate."""

    phase: str
    outcome: str
    detail: str
    duration_ms: int
    ok: bool


class ToolforgeRejected(RuntimeError):
    """The gate refused to publish. Carries both runs so the reason is legible."""

    def __init__(self, name: str, reason: str, gate: list[ToolforgeGateRun]) -> None:
        detail = "\n".join(f"  {run.phase}: {run.outcome} - {run.detail}" for run in gate)
        super().__init__(f"toolforge refused to publish {name!r}: {reason}" + (f"\n{detail}" if detail else ""))
        self.name = name
        self.reason = reason
        self.gate = gate


@dataclass(frozen=True)
class ToolforgeSkill:
    """A published package, already importable and already bound in this kernel."""

    name: str
    import_name: str
    package_path: str
    src_path: str
    version: int
    installed: bool
    module: Any = field(repr=False)
    gate: list[ToolforgeGateRun] = field(default_factory=list, repr=False)

    def __getattr__(self, attribute: str) -> Any:
        if attribute.startswith("_"):
            raise AttributeError(attribute)
        return getattr(object.__getattribute__(self, "module"), attribute)


def _gate_from_payload(payload: Any) -> list[ToolforgeGateRun]:
    if not isinstance(payload, list):
        return []
    runs: list[ToolforgeGateRun] = []
    for entry in payload:
        if not isinstance(entry, dict):
            continue
        runs.append(
            ToolforgeGateRun(
                phase=str(entry.get("phase") or ""),
                outcome=str(entry.get("outcome") or ""),
                detail=str(entry.get("detail") or ""),
                duration_ms=int(entry.get("duration_ms") or 0),
                ok=bool(entry.get("ok")),
            )
        )
    return runs


def bind(import_name: str, src_path: str | None = None) -> Any:
    """Make ``import_name`` importable and bound in the live ``__main__``.

    Prepending the package root covers the window before the editable install is
    visible to this interpreter, ``invalidate_caches`` covers a directory that
    did not exist when the finders last looked, and dropping any stale module
    covers a republish of a name already imported in this session.
    """
    if not isinstance(import_name, str) or not import_name:
        raise ValueError("import_name must be a non-empty str")
    if src_path and src_path not in sys.path:
        sys.path.insert(0, src_path)
    importlib.invalidate_caches()
    sys.modules.pop(import_name, None)
    module = importlib.import_module(import_name)
    resolve_run(module, import_name)
    main = sys.modules.get("__main__")
    if main is not None:
        # Same wrapper the kernel bootstrap puts on every other Python skill, so
        # a name is callable the session it is published and every session after.
        wrap = getattr(main, "_prime_agent_wrap_skill_module", None)
        if callable(wrap):
            module = wrap(module)
        setattr(main, import_name, module)
    return module


async def publish(name: str, source: str, doc: str, exit_test: str) -> ToolforgeSkill:
    """Publish ``source`` as a durable Python skill named ``name``.

    ``source`` is the body of the new module and must define ``run``.
    ``doc`` describes it for the skills prompt. ``exit_test`` is a standalone
    program that imports the module and asserts what it should do; it is run
    twice by the host and must fail without the implementation and pass with
    it. Raises ToolforgeRejected when either run says otherwise.
    """
    for label, value in (("name", name), ("source", source), ("doc", doc), ("exit_test", exit_test)):
        if not isinstance(value, str):
            raise TypeError(f"{label} must be str, got {type(value).__name__}")
        if not value.strip():
            raise ValueError(f"{label} must not be empty")

    # Local import: `rlm/__init__` imports this module, so a module-scope import
    # of the package's own host bridge would be circular.
    from . import host_request

    payload = await host_request(
        "toolforge.publish",
        {"name": name, "source": source, "doc": doc, "exit_test": exit_test},
    )
    gate = _gate_from_payload(payload.get("gate"))
    if payload.get("status") != "published":
        raise ToolforgeRejected(name, str(payload.get("reason") or "no reason reported"), gate)

    import_name = payload.get("import_name")
    if not isinstance(import_name, str) or not import_name:
        raise RuntimeError("toolforge.publish returned no import name")
    src_path = payload.get("src_path")
    module = bind(import_name, src_path if isinstance(src_path, str) else None)
    return ToolforgeSkill(
        name=name,
        import_name=import_name,
        package_path=str(payload.get("package_path") or ""),
        src_path=str(src_path or ""),
        version=int(payload.get("version") or 0),
        installed=bool(payload.get("installed")),
        module=module,
        gate=gate,
    )
