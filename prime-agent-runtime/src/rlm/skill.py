"""Shared helpers for Prime Agent Python skills."""

from __future__ import annotations

import asyncio
import inspect
import sys
from pathlib import Path
from typing import Any, Callable


def resolve_run(module: Any, label: str | None = None) -> Callable[..., Any]:
    """Return a skill module's callable ``run``, or raise a teaching error.

    Every Prime Agent Python skill is addressed as ``<import>.run(...)``: the
    console script dispatches to it, the kernel bootstrap wraps the module so
    that calling the module calls it, and toolforge refuses to publish a
    package without it. One definition keeps those three agreeing.
    """
    name = label or getattr(module, "__name__", "skill")
    run = getattr(module, "run", None)
    if not callable(run):
        raise RuntimeError(f"{name} does not expose a callable run()")
    return run


async def run_cli(func: Callable[..., Any], prog: str | None = None) -> None:
    """Parse CLI arguments for a skill function and print a non-None result."""
    # Imported here, not at module scope: `rlm.toolforge` imports this module on
    # every `import rlm`, and tyro is only ever needed by a console script.
    import tyro

    result = tyro.cli(func, prog=prog)
    if inspect.isawaitable(result):
        result = await result
    if result is not None:
        print(result)


def cli() -> None:
    """Run `<skill>.run` for a console script named exactly after the skill import."""
    prog = Path(sys.argv[0]).stem
    try:
        module = __import__(prog)
    except ImportError as exc:
        raise RuntimeError(
            f"Could not import Python skill module {prog!r}. "
            "The console-script name must match the skill import name exactly; "
            "use underscores instead of dashes."
        ) from exc
    asyncio.run(run_cli(resolve_run(module, prog), prog=prog))
