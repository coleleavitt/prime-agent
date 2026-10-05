"""Plan-mode (no-edit) guard for the Prime Agent kernel.

While enabled, filesystem mutations raise ``PlanModeError`` and subprocesses
run inside a read-only OS sandbox (bwrap / sandbox-exec) so read-only commands
keep working; without a sandbox binary only classifiable read-only commands
run. Enforcement uses ``sys.addaudithook``: a hook cannot be removed once
installed.

Only the host can switch the guard. The REPL claims the one host controller at
startup, before any cell runs, and answers the host's ``plan_guard`` protocol
frame through it; the controller also checks the host-held token the first
frame binds. Kernel code can import this module, read ``is_enabled()`` and call
``check_bash()``, but it cannot claim a second controller, rebind the token, or
mark its own process spawns as mediated (a spawn is mediated only when the
patched ``Popen.__init__`` frame itself is on the stack).

This is a guard against an agent that edits when it should be planning, not a
sandbox for hostile code: arbitrary Python can still reach the guard's closure
state through interpreter introspection, or call libc through ``ctypes``
(left open: ``dill``, which the namespace snapshot needs, calls into the C API
through it). The host refuses its own mutating tools independently.
"""

from __future__ import annotations

import hashlib
import hmac
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
from collections.abc import Callable, Iterable, Sequence
from pathlib import Path
from typing import Any

_PLAN_MODE_MESSAGE = (
    "Plan mode is active: {action} is blocked. You may read files, run "
    "read-only commands, and write only under temp/cache directories. Do not "
    "attempt to work around this. Present your plan or answer, and ask the "
    "user to exit plan mode if edits are needed."
)

_FALLBACK_BLOCK_ACTION = (
    "running this command (no bwrap/sandbox-exec on this machine, so only "
    "classifiable read-only commands run: git log/diff/status, rg, grep, ls, cat, ...)"
)


class PlanModeError(RuntimeError):
    """A mutation refused because plan mode is active."""

    def __init__(self, action: str) -> None:
        super().__init__(_PLAN_MODE_MESSAGE.format(action=action))


# io.open write intent letters / os.open write intent flags.
_WRITE_MODE_CHARS = frozenset("wax+")
_WRITE_OPEN_FLAGS = os.O_WRONLY | os.O_RDWR | os.O_APPEND | os.O_CREAT | os.O_TRUNC

# Audit events that mutate the filesystem; every str/bytes/PathLike argument is
# treated as a target path (rename/link must have BOTH ends writable).
_FS_MUTATION_EVENTS = frozenset(
    {
        "os.remove",
        "os.rename",
        "os.rmdir",
        "os.mkdir",
        "os.chmod",
        "os.chown",
        "os.chflags",
        "os.lchflags",
        "os.link",
        "os.symlink",
        "os.truncate",
        "os.utime",
        "os.setxattr",
        "os.removexattr",
        "shutil.rmtree",
        "shutil.move",
        "shutil.chown",
    }
)

# Copies read their source; only the destination (the second argument) is written.
_FS_COPY_EVENTS = frozenset({"shutil.copyfile", "shutil.copymode", "shutil.copystat"})

# Process spawns must go through the mediated Popen wrapper.
_SPAWN_EVENTS = frozenset(
    {
        "subprocess.Popen",
        "os.system",
        "os.exec",
        "os.posix_spawn",
        "os.spawn",
        "os.startfile",
        "os.fork",
        "os.forkpty",
        "pty.spawn",
    }
)

# Read-only commands permitted when no OS sandbox binary is available.
_FALLBACK_ALLOWED_COMMANDS = frozenset(
    {
        "awk",
        "basename",
        "cat",
        "column",
        "cut",
        "df",
        "diff",
        "dirname",
        "du",
        "echo",
        "file",
        "find",
        "grep",
        "head",
        "hostname",
        "jq",
        "ls",
        "nl",
        "printf",
        "ps",
        "pwd",
        "readlink",
        "realpath",
        "rg",
        "sort",
        "stat",
        "tail",
        "tr",
        "tree",
        "true",
        "uname",
        "uniq",
        "wc",
        "which",
        "whoami",
    }
)

_FALLBACK_GIT_SUBCOMMANDS = frozenset(
    {
        "blame",
        "describe",
        "diff",
        "grep",
        "log",
        "ls-files",
        "ls-remote",
        "ls-tree",
        "rev-list",
        "rev-parse",
        "shortlog",
        "show",
        "status",
        "var",
        "version",
    }
)

# Shells whose `-c` script can be classified.
_SHELL_NAMES = frozenset({"bash", "dash", "sh", "zsh"})


def _norm(path: Any) -> str | None:
    try:
        if isinstance(path, int):
            return None
        # realpath: a symlink under a writable root must not reach a file outside it.
        return os.path.realpath(os.fsdecode(path))
    except (TypeError, ValueError):
        return None


def _default_writable_roots() -> set[str]:
    roots = {tempfile.gettempdir(), "/tmp", "/dev", str(Path.home() / ".cache")}
    tmpdir = os.environ.get("TMPDIR")
    if tmpdir:
        roots.add(tmpdir)
    if sys.platform == "darwin":
        roots.update({"/private/tmp", "/private/var/folders"})
    return roots


# (root, writable) pairs, shortest root first: the deepest root containing a
# path decides, and a protected root beats a writable one of the same path.
Roots = tuple[tuple[str, bool], ...]


def _resolve_roots(writable: Iterable[str], protected: Iterable[str] = ()) -> Roots:
    pairs = {(os.path.realpath(r), True) for r in writable if r}
    protected_paths = {os.path.realpath(r) for r in protected if r}
    pairs = {pair for pair in pairs if pair[0] not in protected_paths}
    pairs.update((root, False) for root in protected_paths)
    return tuple(sorted(pairs, key=lambda pair: (len(pair[0]), not pair[1], pair[0])))


def _under(path: str, root: str) -> bool:
    return path == root or path.startswith(root.rstrip(os.sep) + os.sep)


def _is_write_allowed(path: str, roots: Roots) -> bool:
    parts = path.split(os.sep)
    if "__pycache__" in parts or path.endswith(".pyc"):
        return True
    deciding: tuple[str, bool] | None = None
    for root, writable in roots:
        if _under(path, root):
            deciding = (root, writable)
    if deciding is None or not deciding[1]:
        return False
    # Repository metadata stays read-only even inside writable roots.
    rest = path[len(deciding[0]) :].split(os.sep)
    return ".git" not in rest


def _open_wants_write(mode: Any, flags: Any) -> bool:
    if isinstance(mode, str):
        return bool(_WRITE_MODE_CHARS.intersection(mode))
    if isinstance(flags, int):
        return bool(flags & _WRITE_OPEN_FLAGS)
    return False


def _sandbox_prefix(roots: Roots) -> list[str] | None:
    """Read-only OS sandbox argv prefix for subprocesses, or None if unavailable.

    Roots apply shortest first, so a deeper root's mount (or rule) wins, like
    ``_is_write_allowed``.
    """
    if sys.platform == "linux":
        bwrap = shutil.which("bwrap")
        if not bwrap:
            return None
        prefix = [bwrap, "--ro-bind", "/", "/", "--dev-bind", "/dev", "/dev"]
        for root, writable in roots:
            if root != "/dev" and os.path.isdir(root):
                prefix += ["--bind" if writable else "--ro-bind", root, root]
        prefix += ["--die-with-parent", "--"]
        return prefix
    if sys.platform == "darwin" and os.path.exists("/usr/bin/sandbox-exec"):
        rules = " ".join(
            f'({"allow" if writable else "deny"} file-write* (subpath "{root}"))' for root, writable in roots
        )
        profile = f"(version 1) (allow default) (deny file-write*) {rules}"
        return ["/usr/bin/sandbox-exec", "-p", profile]
    return None


def _sandbox_works(prefix: list[str], spawn: Callable[..., Any]) -> bool:
    """Whether the sandbox can start at all (bwrap needs user namespaces)."""
    true = shutil.which("true") or "/bin/true"
    try:
        proc = spawn(
            [*prefix, true],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        return proc.wait(timeout=10) == 0
    except (OSError, subprocess.SubprocessError):
        return False


def _fallback_command_allowed(argv: Sequence[str]) -> bool:
    if not argv:
        return False
    name = os.path.basename(argv[0])
    if name in _SHELL_NAMES:
        return len(argv) >= 3 and argv[1] == "-c" and _fallback_shell_allowed(argv[2])
    if name == "git":
        # Only harmless global options may precede the subcommand; -c,
        # --exec-path, etc. can make even read subcommands run arbitrary code.
        rest = argv[1:]
        i = 0
        while i < len(rest):
            arg = rest[i]
            if arg == "-C":
                i += 2
                continue
            if arg in ("-P", "--no-pager", "--no-optional-locks", "--literal-pathspecs"):
                i += 1
                continue
            if arg.startswith("-"):
                return False
            return arg in _FALLBACK_GIT_SUBCOMMANDS and not any(
                a.startswith("--output") or a == "-o" for a in rest[i + 1 :]
            )
        return False
    if name == "find":
        return not any(
            a in ("-delete", "-exec", "-execdir", "-ok", "-okdir", "-fprint", "-fprint0", "-fprintf", "-fls")
            for a in argv[1:]
        )
    if name == "sed":
        return not any(a == "-i" or a.startswith("-i") or a.startswith("--in-place") for a in argv[1:])
    if name == "sort":
        return not any(a == "-o" or a.startswith("-o") or a.startswith("--output") for a in argv[1:])
    return name in _FALLBACK_ALLOWED_COMMANDS


def _fallback_shell_allowed(command: str) -> bool:
    """Whether every segment of a shell script is a classifiable read-only command."""
    if ">" in command:
        return False
    # Evaluate each pipeline/sequence segment independently; fail closed.
    for op in ("&&", "||", ";", "|", "&", "\n"):
        command = command.replace(op, "\x00")
    for segment in filter(None, (s.strip() for s in command.split("\x00"))):
        if "$(" in segment or "`" in segment or "<(" in segment:
            return False
        try:
            argv = shlex.split(segment)
        except ValueError:
            return False
        if argv and not _fallback_command_allowed(argv):
            return False
    return True


# `rlm.bash` wraps every command in a status script; its completion halves are
# random hex. The fallback classifies the wrapped command, then proves the
# wrapper is exactly the runtime's own around it.
_STATUS_HALVES = re.compile(r"'([0-9a-f]+)' '([0-9a-f]+)' >&")


def _is_runtime_bash_wrapper(script: str, inner: str) -> bool:
    # The package rebinds `rlm.bash` to the function; import the helper itself.
    from .bash import _status_script

    match = _STATUS_HALVES.search(script)
    if match is None:
        return False
    return script == _status_script(inner, match.group(1), match.group(2))


def _popen_argv(args: Any, executable: Any) -> list[str]:
    argv = [os.fsdecode(a) for a in ([args] if isinstance(args, (str, bytes, os.PathLike)) else list(args))]
    if executable and argv:
        argv[0] = os.fsdecode(executable)
    return argv


# (token, enabled, extra_writable_roots, protected_roots) -> enabled
HostController = Callable[[str, bool, Sequence[str], Sequence[str]], bool]


def _make_guard() -> tuple[
    Callable[[], HostController],
    Callable[[], bool],
    Callable[[str], None],
]:
    state: dict[str, Any] = {
        "token_hash": None,
        "enabled": False,
        "roots": _resolve_roots(_default_writable_roots()),
        # None: not probed for the current roots; [] probed and unusable.
        "sandbox": None,
        "hook_added": False,
        "popen_patched": False,
        "claimed": False,
    }
    original_init = subprocess.Popen.__init__

    def _sandbox() -> list[str] | None:
        if state["sandbox"] is None:
            prefix = _sandbox_prefix(state["roots"])
            state["sandbox"] = prefix if prefix is not None and _sandbox_works(prefix, subprocess.Popen) else []
        return state["sandbox"] or None

    def _hook(event: str, args: tuple[Any, ...]) -> None:
        if not state["enabled"]:
            return
        if event == "open":
            if len(args) < 3 or not _open_wants_write(args[1], args[2]):
                return
            path = _norm(args[0])
            if path is not None and not _is_write_allowed(path, state["roots"]):
                raise PlanModeError(f"writing to {path}")
        elif event in _FS_MUTATION_EVENTS:
            for arg in args:
                if not isinstance(arg, (str, bytes, os.PathLike)):
                    continue
                path = _norm(arg)
                if path is not None and not _is_write_allowed(path, state["roots"]):
                    raise PlanModeError(f"{event} on {path}")
        elif event in _FS_COPY_EVENTS:
            path = _norm(args[1]) if len(args) > 1 else None
            if path is not None and not _is_write_allowed(path, state["roots"]):
                raise PlanModeError(f"{event} to {path}")
        elif event in _SPAWN_EVENTS:
            if not _mediated():
                raise PlanModeError(f"direct process spawn ({event})")

    def _mediated() -> bool:
        # Only the patched Popen.__init__ frame mediates a spawn: a flag
        # kernel code could set would make every spawn "mediated".
        frame = sys._getframe(2)
        while frame is not None:
            if frame.f_code is guarded_init.__code__:
                return True
            frame = frame.f_back
        return False

    def _wrap_popen_args(args: Any, kwargs: dict[str, Any], inner: str | None) -> tuple[Any, dict[str, Any]]:
        shell = bool(kwargs.get("shell"))
        executable = kwargs.pop("executable", None)
        prefix = _sandbox()
        if prefix is None:
            if shell:
                if isinstance(args, (str, bytes)) and _fallback_shell_allowed(os.fsdecode(args)):
                    if executable:
                        kwargs["executable"] = executable
                    return args, kwargs
                raise PlanModeError(_FALLBACK_BLOCK_ACTION)
            argv = _popen_argv(args, executable)
            if (
                inner is not None
                and len(argv) == 3
                and os.path.basename(argv[0]) in _SHELL_NAMES
                and argv[1] == "-c"
                and _is_runtime_bash_wrapper(argv[2], inner)
            ):
                if _fallback_shell_allowed(inner):
                    return argv, kwargs
                raise PlanModeError(_FALLBACK_BLOCK_ACTION)
            if not _fallback_command_allowed(argv):
                raise PlanModeError(_FALLBACK_BLOCK_ACTION)
            return argv, kwargs
        kwargs["shell"] = False
        if shell:
            sh = os.fsdecode(executable) if executable else "/bin/sh"
            return [*prefix, sh, "-c", os.fsdecode(args)], kwargs
        return [*prefix, *_popen_argv(args, executable)], kwargs

    def guarded_init(self: subprocess.Popen[Any], args: Any = None, *pargs: Any, **kwargs: Any) -> None:
        inner = kwargs.pop("_plan_guard_inner", None)
        if not state["enabled"]:
            original_init(self, args, *pargs, **kwargs)
            return
        if pargs:
            # Positional bufsize/executable/etc. are never used by the stdlib
            # helpers the kernel relies on; keep the wrapper simple.
            raise PlanModeError("subprocess with positional options in plan mode (use keyword arguments)")
        args, kwargs = _wrap_popen_args(args, kwargs, inner if isinstance(inner, str) else None)
        original_init(self, args, **kwargs)

    def _arm() -> None:
        if not state["popen_patched"]:
            subprocess.Popen.__init__ = guarded_init  # type: ignore[method-assign]
            state["popen_patched"] = True
        if not state["hook_added"]:
            sys.addaudithook(_hook)
            state["hook_added"] = True

    def _control(
        token: str,
        enabled: bool,
        extra_writable_roots: Sequence[str],
        protected_roots: Sequence[str] = (),
    ) -> bool:
        if not isinstance(token, str) or not token:
            raise PermissionError("plan_guard: invalid token")
        given = hashlib.sha256(token.encode()).digest()
        if state["token_hash"] is None:
            # The first frame binds the host's token; it arrives before any cell.
            state["token_hash"] = given
        elif not hmac.compare_digest(state["token_hash"], given):
            raise PermissionError("plan_guard: invalid token")
        if enabled:
            roots = _default_writable_roots()
            roots.update(r for r in extra_writable_roots if isinstance(r, str) and r)
            resolved = _resolve_roots(roots, (r for r in protected_roots if isinstance(r, str) and r))
            if resolved != state["roots"] or state["sandbox"] is None:
                # The sandbox probe spawns a process, which an armed guard would
                # refuse: probe with the guard off, then arm.
                state["enabled"] = False
                state["roots"] = resolved
                state["sandbox"] = None
                _sandbox()
            _arm()
        state["enabled"] = bool(enabled)
        return bool(state["enabled"])

    def claim_host_controller() -> HostController:
        if state["claimed"]:
            raise PermissionError("plan_guard: the host controller is already claimed")
        state["claimed"] = True
        return _control

    def is_enabled() -> bool:
        return bool(state["enabled"])

    def check_bash(command: str) -> None:
        if not state["enabled"] or _sandbox() is not None:
            return
        if not _fallback_shell_allowed(command):
            raise PlanModeError(_FALLBACK_BLOCK_ACTION)

    return claim_host_controller, is_enabled, check_bash


claim_host_controller, is_enabled, check_bash = _make_guard()
claim_host_controller.__doc__ = (
    "Return the one host controller ``(token, enabled, extra_writable_roots, protected_roots) -> enabled``; "
    "a second claim raises PermissionError. The REPL claims it at startup."
)
is_enabled.__doc__ = "Whether plan mode is active in this kernel."
check_bash.__doc__ = (
    "Raise PlanModeError when plan mode is active, no OS sandbox is available, "
    "and ``command`` is not a classifiable read-only shell script."
)

__all__ = ["PlanModeError", "check_bash", "claim_host_controller", "is_enabled"]
