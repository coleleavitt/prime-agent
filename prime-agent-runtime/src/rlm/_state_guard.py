"""Test processes never touch the user's real agent state.

Mirrors ``pa_types::platform::test_isolation``: in a test process (one the
Rust test support isolated, ``PA_TEST_ISOLATED=1``, or a ``python -m
unittest`` run) a state path inside a protected home's state -- the passwd
entry's home, not ``$HOME``, plus ``PA_TEST_PROTECTED_HOME`` -- is refused
loudly. ``PA_TEST_ALLOW_REAL_STATE=1`` is the explicit opt-in. Outside a test
process the guard is a no-op: production behaviour does not change.
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

ISOLATED_ENV = "PA_TEST_ISOLATED"
PROTECTED_HOME_ENV = "PA_TEST_PROTECTED_HOME"
ALLOW_REAL_STATE_ENV = "PA_TEST_ALLOW_REAL_STATE"

_PROTECTED_STATE = (
    ".prime",
    ".anthropic-accounts",
    ".claude",
    ".claude.json",
    ".pi",
    ".grok",
    ".config/opencode",
    ".config/jfc",
    ".local/share/prime",
)


class RealStateError(RuntimeError):
    """A test process resolved a path inside the real home's state."""


def is_test_process() -> bool:
    """Whether this process belongs to a test run."""
    # ``python -m unittest`` rewrites ``argv[0]`` to ``<executable> -m unittest``.
    return os.environ.get(ISOLATED_ENV) == "1" or (bool(sys.argv) and sys.argv[0].endswith(" -m unittest"))


def _passwd_home() -> Path | None:
    try:
        import pwd

        home = pwd.getpwuid(os.getuid()).pw_dir
    except (ImportError, KeyError, AttributeError):
        return None
    return Path(home) if home else None


def protected_homes() -> list[Path]:
    """The passwd home, plus ``PA_TEST_PROTECTED_HOME`` when set."""
    homes = [home for home in (_passwd_home(),) if home is not None]
    extra = os.environ.get(PROTECTED_HOME_ENV)
    if extra:
        homes.append(Path(extra))
    return homes


def real_state_violation(what: str, path: Path) -> str | None:
    """Why a test process must not use ``path`` as its ``what``, or ``None``."""
    if not is_test_process() or os.environ.get(ALLOW_REAL_STATE_ENV) == "1":
        return None
    resolved = Path(path).expanduser().resolve()
    for home in protected_homes():
        home = home.expanduser().resolve()
        if any(resolved.is_relative_to(home / state) for state in _PROTECTED_STATE):
            return (
                f"a test process resolved its {what} to {resolved}, inside the real home's "
                f"state ({home}). Tests must not read or write the user's live agent dir. "
                f"Point PRIME_AGENT_CODING_AGENT_DIR at a temp dir, or set "
                f"{ALLOW_REAL_STATE_ENV}=1 if this test must use the real state."
            )
    return None


def guard_state_path(what: str, path: Path) -> Path:
    """Return ``path``, or raise :class:`RealStateError` when a test process
    resolved it inside the real home's state."""
    violation = real_state_violation(what, path)
    if violation is not None:
        raise RealStateError(f"refusing to touch real state: {violation}")
    return path
