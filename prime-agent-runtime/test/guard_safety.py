"""Safety rails for the bash guard suites: a payload a test expects refused is
never run, and the commands the suites do run are confined.

The suites feed the guards dangerous payloads (``chmod -R 755 /`` in a script,
``git reset --hard`` on a dirty tree, ``curl ... | sh``) and assert the
refusal. Run through the real ``bash()``, a guard regression would execute
them. Two rails make that impossible:

- :func:`refusal_expected` (and :class:`RefusalSafe`, which enters it for every
  ``assertRaises`` of a refusal class): inside it a ``bash.run`` request (check
  and spawn in one) is answered as ``bash.check``. A refusal raises exactly as
  before; a command the guards allow is *not run* and the test fails with an
  ``AssertionError`` naming it. A validated-script spawn (no guards) is refused
  outright.
- :func:`confinement` (entered by :class:`RefusalSafe` around every test):
  ``HOME``, ``TMPDIR`` and the agent directory move under a private temporary
  root, and the bash host is a test host (``PA_TEST_BASH_HOST_ROOT``) that
  re-executes itself under the OS sandbox in ``read-only`` mode with network
  off (Landlock where the kernel has it): a command that does run, and the
  guards' own probes, can write only under that root. Every request checks
  the running host is that confined one; afterwards the environment is
  restored and the host stopped, so no other suite in the process inherits
  either.
"""

from __future__ import annotations

import contextlib
import json
import os
import sys
import tempfile
import threading
import unittest
from collections.abc import Iterator
from pathlib import Path
from typing import Any

import rlm.bash  # noqa: F401 - registers the module

bash_module = sys.modules["rlm.bash"]

_state = threading.local()
_original_request = bash_module._request
_confined_root: Path | None = None


def _expecting() -> bool:
    return getattr(_state, "depth", 0) > 0


def _guarded_request(data: dict[str, Any], **kwargs: Any) -> dict[str, Any]:
    if _confining.is_set():
        _ensure_confined_host()
    if _expecting() and data.get("type") == "bash.run":
        if data.get("guards") is False:
            raise AssertionError(
                f"a refusal was expected, but {data.get('command')!r} was sent as a validated script; it was not run"
            )
        reply = _original_request(dict(data, type="bash.check"), **kwargs)
        if reply.get("status") == "ok":
            raise AssertionError(
                f"a refusal was expected, but the guards allowed {data.get('command')!r}; it was not run"
            )
        return reply
    return _original_request(data, **kwargs)


setattr(bash_module, "_request", _guarded_request)  # noqa: B010 - patch the module


@contextlib.contextmanager
def refusal_expected() -> Iterator[None]:
    """Inside: no command reaches spawn; a guard refusal raises as usual."""
    _state.depth = getattr(_state, "depth", 0) + 1
    try:
        yield
    finally:
        _state.depth -= 1


_REFUSAL_CLASSES = tuple(error for _, _, _, error in bash_module._GUARDS.values())


class _RefusalContext:
    """``assertRaises`` for a refusal class that never lets the command run."""

    def __init__(self, inner: Any) -> None:
        self._inner = inner
        self._guard = refusal_expected()

    def __enter__(self) -> Any:
        self._guard.__enter__()
        return self._inner.__enter__()

    def __exit__(self, *exc: Any) -> bool:
        try:
            return bool(self._inner.__exit__(*exc))
        finally:
            self._guard.__exit__(None, None, None)


class RefusalSafe(unittest.TestCase):
    """A guard-suite test case: it runs inside :func:`confinement`, and its
    ``assertRaises(<refusal class>)`` never runs the command."""

    def run(self, result: Any = None) -> Any:
        with confinement():
            return super().run(result)

    def assertRaises(self, expected_exception: Any, *args: Any, **kwargs: Any) -> Any:  # noqa: N802
        refusal = isinstance(expected_exception, type) and issubclass(expected_exception, _REFUSAL_CLASSES)
        if not refusal:
            return super().assertRaises(expected_exception, *args, **kwargs)
        if args:
            with refusal_expected():
                return super().assertRaises(expected_exception, *args, **kwargs)
        return _RefusalContext(super().assertRaises(expected_exception, **kwargs))


def confined() -> bool:
    """Whether the guard suites' bash host runs under the OS sandbox here."""
    return _confined_root is not None and landlock_available()


def landlock_available() -> bool:
    try:
        return "landlock" in Path("/sys/kernel/security/lsm").read_text()
    except OSError:
        return sys.platform == "darwin"


def confine() -> Path:
    """The private temporary root the guard suites are confined to (made
    once). Every test of a :class:`RefusalSafe` class runs inside
    :func:`confinement` over it."""
    global _confined_root
    if _confined_root is None:
        root = Path(tempfile.mkdtemp(prefix="pa-guard-suite-")).resolve()
        agent = root / "agent"
        agent.mkdir()
        (agent / "settings.json").write_text(json.dumps({"sandbox": {"mode": "read-only", "network": False}}))
        for name in ("tmp", "home"):
            (root / name).mkdir()
        _confined_root = root
    return _confined_root


def _confined_env(root: Path) -> dict[str, str]:
    return {
        # The host re-executes itself under the read-only OS sandbox with the
        # root as its only writable directory and no network (or refuses every
        # command where that cannot be enforced).
        "PA_TEST_BASH_HOST_ROOT": str(root),
        # The workspace's test isolation (pa_types::platform::test_isolation):
        # the host is a test process, so its agent-dir and auth lookups refuse
        # the real state, and its home is the root's.
        "PA_TEST_ISOLATED": "1",
        "PA_TEST_HOME": str(root / "home"),
        "PRIME_AGENT_CODING_AGENT_DIR": str(root / "agent"),
        "TMPDIR": str(root / "tmp"),
        "HOME": str(root / "home"),
    }


# Set while a guard-suite test runs: every request first checks the host.
_confining = threading.Event()


def _host_is_confined(proc: Any, root: Path) -> bool:
    try:
        environ = Path(f"/proc/{proc.pid}/environ").read_bytes().split(b"\0")
    except OSError:
        return False
    return b"PA_TEST_BASH_HOST_CONFINED=1" in environ and f"PA_TEST_BASH_HOST_ROOT={root}".encode() in environ


def _stop_host(unless_confined_to: Path | None = None) -> None:
    sidecar = bash_module._sidecar
    with sidecar._lock:
        proc = sidecar._proc
        if proc is None or proc.poll() is not None:
            return
        if unless_confined_to is not None and _host_is_confined(proc, unless_confined_to):
            return
        sidecar._proc = None
    proc.kill()
    proc.wait(timeout=10)


def _ensure_confined_host() -> None:
    root = confine()
    # The flag that confines the host is not the test's to drop.
    for name in ("PA_TEST_BASH_HOST_ROOT", "PA_TEST_ISOLATED"):
        os.environ[name] = _confined_env(root)[name]
    _stop_host(unless_confined_to=root)


@contextlib.contextmanager
def confinement() -> Iterator[Path]:
    """Inside: ``HOME``, ``TMPDIR`` and the agent directory live under the
    private root, and the bash host is a confined test host (one started
    before is stopped). After: the environment is restored and the confined
    host stopped, so no other suite in the process uses it."""
    root = confine()
    env = _confined_env(root)
    saved = {name: os.environ.get(name) for name in env}
    os.environ.update(env)
    tempfile.tempdir = None
    _stop_host(unless_confined_to=root)
    _confining.set()
    try:
        yield root
    finally:
        _confining.clear()
        for name, value in saved.items():
            if value is None:
                os.environ.pop(name, None)
            else:
                os.environ[name] = value
        tempfile.tempdir = None
        _stop_host()
