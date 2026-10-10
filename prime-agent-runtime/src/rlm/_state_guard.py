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
TEST_HOME_ENV = "PA_TEST_HOME"

# The variables that point a process's state somewhere, beside ``HOME``.
_STATE_REDIRECT_VARS = (
    "PRIME_AGENT_CODING_AGENT_DIR",
    "PI_CODING_AGENT_DIR",
    "PRIME_AGENT_SESSION_DIR",
    "PRIME_AGENT_KERNEL_VENV",
    "PRIME_AGENT_KERNEL_PYTHON",
    "ANTHROPIC_ACCOUNTS_FILE",
    "ANTHROPIC_ACCOUNTS_DIR",
    "CLAUDE_CONFIG_DIR",
    "CLAUDE_SECURESTORAGE_CONFIG_DIR",
    "PI_AGENT_DIR",
    "OPENCODE_CONFIG_DIR",
    "PI_ANTHROPIC_AUTH_FILE",
    "OPENCODE_ANTHROPIC_AUTH_FILE",
    "PI_ANTHROPIC_AUTH_ROUTING_STATE_FILE",
    "GROK_HOME",
)

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


def _is_unittest_run() -> bool:
    # ``python -m unittest`` rewrites ``argv[0]`` to ``<executable> -m unittest``.
    return bool(sys.argv) and sys.argv[0].endswith(" -m unittest")


def is_test_process() -> bool:
    """Whether this process belongs to a test run."""
    return os.environ.get(ISOLATED_ENV) == "1" or _is_unittest_run()


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


def _is_protected_home(home: Path) -> bool:
    resolved = home.expanduser().resolve()
    return any(resolved == protected.expanduser().resolve() for protected in protected_homes())


def _test_home() -> Path:
    """``PA_TEST_HOME``, else a non-protected ``HOME``, else a per-user temp dir."""
    explicit = os.environ.get(TEST_HOME_ENV)
    if explicit:
        return Path(explicit)
    home = os.environ.get("HOME")
    if home and not _is_protected_home(Path(home)):
        return Path(home)
    import tempfile

    fallback = Path(tempfile.gettempdir()) / f"pa-test-home-{os.getuid() if hasattr(os, 'getuid') else 'user'}"
    fallback.mkdir(parents=True, exist_ok=True)
    return fallback


def isolate_unittest_process() -> None:
    """A ``python -m unittest`` run never works on the real state: it drops
    inherited redirects into a protected home's state, moves a protected
    ``HOME`` to the test home, and marks its environment
    (``PA_TEST_ISOLATED=1``) so the host binaries and kernels it spawns run
    the Rust guards too. The Rust test harness does the same
    (``pa_types::platform::test_isolation``)."""
    if not _is_unittest_run() or os.environ.get(ALLOW_REAL_STATE_ENV) == "1":
        return
    for name in _STATE_REDIRECT_VARS:
        value = os.environ.get(name)
        if value and real_state_violation(name, Path(value)) is not None:
            print(f"test isolation: ignoring the inherited {name}, which points at the real state", file=sys.stderr)
            del os.environ[name]
    home = os.environ.get("HOME")
    if home and _is_protected_home(Path(home)):
        os.environ["HOME"] = str(_test_home())
    os.environ[ISOLATED_ENV] = "1"


isolate_unittest_process()
