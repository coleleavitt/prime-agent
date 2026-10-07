"""Differential run: the Python guard suites, with every guard call also judged
by the Rust guards in the call's own live context (cwd, environment, files,
repositories), so verdicts that depend on state the neutral corpus cannot hold
are compared too.

    cargo build -p pa-bash --example guard_oracle
    cd prime-agent-runtime
    uv run python ../crates/pa-bash/tests/corpus/differential.py [guard ...]

Calls made while a test has patched a guard internal (a probe, a timeout, a
module flag) are reported separately: the Rust side cannot see the patch.
"""

from __future__ import annotations

import io
import json
import os
import subprocess
import sys
import unittest
from contextlib import redirect_stderr
from pathlib import Path

HERE = Path(__file__).resolve().parent
RUNTIME = Path.cwd()
sys.path.insert(0, str(RUNTIME / "src"))
sys.path.insert(0, str(RUNTIME / "test"))
for name in list(os.environ):
    if name.startswith("PI_BASH_ALLOW_"):
        del os.environ[name]

import rlm.bash  # noqa: E402

bash_module = sys.modules["rlm.bash"]
target_dir = os.environ.get("CARGO_TARGET_DIR") or str(HERE.parents[3] / "target")
oracle = subprocess.Popen(
    [os.path.join(target_dir, "debug", "examples", "guard_oracle")],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    text=True,
)

GUARDS = {
    "destructive_git": ("_guard_destructive_git", "_BASH_DESTRUCTIVE_GIT_BYPASS_AT_START"),
    "destructive_chmod": ("_guard_destructive_chmod", "_DESTRUCTIVE_CHMOD_BYPASS_AT_KERNEL_START"),
    "force_push": ("_guard_force_push", "_FORCE_PUSH_BYPASS_AT_KERNEL_START"),
    "secret_echo": ("_guard_secret_echo", "_SECRET_ECHO_BYPASS_AT_KERNEL_START"),
    "pipe_to_shell": ("_guard_pipe_to_shell", "_PIPE_TO_SHELL_BYPASS_AT_KERNEL_START"),
    "sudo": ("_guard_sudo", "_SUDO_BYPASS_AT_KERNEL_START"),
}
selected = set(sys.argv[1:]) or set(GUARDS)
originals = {name: getattr(bash_module, fn) for name, (fn, _flag) in GUARDS.items()}
pristine = {name: getattr(bash_module, name) for name in dir(bash_module) if name.startswith("_")}
current_test = ["?"]
results = {"match": 0, "mismatch": [], "patched": []}


def _patched_internals() -> list[str]:
    changed = []
    for name, value in pristine.items():
        if name in ("_live_handles", "_activity_handles", "_activity_order", "_hook_installed"):
            continue
        if name.endswith("_late_bypass_warned") or name == "_active_scan_budget":
            continue
        if any(name == fn for fn, _ in GUARDS.values()):
            continue
        if getattr(bash_module, name, None) is not value:
            changed.append(name)
    return changed


def _call(guard: str, args: tuple, kwargs: dict) -> dict | None:
    if guard == "destructive_git":
        command = args[0]
        allow = args[1] if len(args) > 1 else kwargs.get("allow_destructive_git", False)
        prefix = args[2] if len(args) > 2 else kwargs.get("command_prefix")
        script = args[3] if len(args) > 3 else kwargs.get("script")
        if script is None:
            if prefix is None:
                prefix = os.environ.get("PRIME_AGENT_BASH_COMMAND_PREFIX")
            script = bash_module._with_prefix(command, prefix)
    elif guard == "destructive_chmod":
        if len(args) > 3 or kwargs.get("heredoc_depth"):
            return None
        script, allow = args[0], args[1]
        prefix = args[2] if len(args) > 2 else kwargs.get("command_prefix")
        command = script
    elif guard == "force_push":
        script, allow = args[0], args[1]
        prefix = args[2] if len(args) > 2 else kwargs.get("command_prefix")
        if prefix is None:
            return None
        prefix = prefix or None
        command = script
    else:
        script, allow = args[0], args[1]
        command, prefix = script, None
    if allow:
        return None
    request = {"guard": guard, "command": command, "script": script, "cwd": os.getcwd(), "env": dict(os.environ)}
    if prefix is not None:
        request["prefix"] = prefix
    request["launchBypass"] = [name for name, (_fn, flag) in GUARDS.items() if getattr(bash_module, flag)]
    return request


def _wrap(guard: str, original):
    def wrapper(*args, **kwargs):
        request = _call(guard, args, kwargs) if guard in selected else None
        try:
            original(*args, **kwargs)
        except Exception as error:
            python = {"refused": type(error).__name__, "message": str(error)}
            raise_after = error
        else:
            python = {"allowed": True}
            raise_after = None
        if request is not None:
            oracle.stdin.write(json.dumps(request) + "\n")
            oracle.stdin.flush()
            rust = json.loads(oracle.stdout.readline())
            if rust == python:
                results["match"] += 1
            else:
                entry = (guard, current_test[0], request["script"], python, rust)
                patched = _patched_internals()
                (results["patched"] if patched else results["mismatch"]).append(entry + (patched,))
        if raise_after is not None:
            raise raise_after

    return wrapper


for guard, (fn, _flag) in GUARDS.items():
    setattr(bash_module, fn, _wrap(guard, originals[guard]))

original_run = unittest.TestCase.run


def run(self, result=None):
    current_test[0] = self.id()
    return original_run(self, result)


unittest.TestCase.run = run
suite = unittest.defaultTestLoader.loadTestsFromNames(
    [
        "test_bash",
        "test_bash_chmod_guard",
        "test_bash_forcepush_guard",
        "test_bash_git_guard",
        "test_bash_pipe_shell_guard",
        "test_bash_secret_echo_guard",
        "test_bash_sudo_guard",
        "test_trace_bash_mcp",
    ]
)
with redirect_stderr(io.StringIO()):
    outcome = unittest.TextTestRunner(stream=io.StringIO(), verbosity=0).run(suite)
oracle.stdin.close()
oracle.wait()


def show(entry) -> None:
    guard, test, script, python, rust, patched = entry
    print(f"[{guard}] {test}{' (patched: ' + ', '.join(patched) + ')' if patched else ''}")
    print(f"    script: {script[:200]!r}")
    print(f"    python: {json.dumps(python)[:400]}")
    print(f"    rust:   {json.dumps(rust)[:400]}")


limit = int(os.environ.get("PA_BASH_DIFF_LIMIT", "25"))
for entry in results["mismatch"][:limit]:
    show(entry)
print(f"suite: {outcome.testsRun} tests, {len(outcome.failures)} failures, {len(outcome.errors)} errors")
print(
    f"differential: {results['match']} match, {len(results['mismatch'])} mismatch, "
    f"{len(results['patched'])} differ under a patched internal"
)
if os.environ.get("PA_BASH_DIFF_SHOW_PATCHED"):
    for entry in results["patched"][:limit]:
        show(entry)
sys.exit(1 if results["mismatch"] else 0)
