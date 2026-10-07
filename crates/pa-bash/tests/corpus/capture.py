"""Capture the guard parity corpus from the Python kernel guards.

Run from `prime-agent-runtime/` while `rlm/bash.py` still holds the Python
guards (commit 7e4221ebe and its phase-1 successors):

    uv run python ../crates/pa-bash/tests/corpus/capture.py

Every call the bash test suites make into one of the six guard entry points is
recorded, deduplicated, and re-evaluated in a neutral context: an empty,
non-git working directory, an empty HOME, `PATH=/usr/bin:/bin`, no CDPATH. Every
distinct input is then judged by all six guards (not only the guard whose
suite produced it), so each guard also sees the other suites' shapes. The
neutral verdicts (per guard: refused with which error and message, or
allowed) are what `tests/guard_corpus.rs` replays against the Rust pipeline in the same context.
Verdicts that depend on the test's own files, repositories or environment are
pinned by the Python suites, which run against the Rust checker after the
switch-over; this corpus pins everything the text alone decides.
"""

from __future__ import annotations

import io
import json
import os
import sys
import tempfile
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
os.environ.pop("PRIME_AGENT_BASH_COMMAND_PREFIX", None)

import rlm.bash  # noqa: E402

bash_module = sys.modules["rlm.bash"]

# (guard name, function name, how its arguments map to (script, prefix, command))
GUARDS = {
    "destructive_git": "_guard_destructive_git",
    "destructive_chmod": "_guard_destructive_chmod",
    "force_push": "_guard_force_push",
    "secret_echo": "_guard_secret_echo",
    "pipe_to_shell": "_guard_pipe_to_shell",
    "sudo": "_guard_sudo",
}

seen: dict[tuple[str, str, str, str | None], None] = {}


def _normalize_call(guard: str, args: tuple, kwargs: dict) -> tuple[str, str, str | None, bool] | None:
    """(command, script, prefix, allow) for one guard call."""
    if guard == "destructive_git":
        command = args[0]
        allow = args[1] if len(args) > 1 else kwargs.get("allow_destructive_git", False)
        prefix = args[2] if len(args) > 2 else kwargs.get("command_prefix")
        script = args[3] if len(args) > 3 else kwargs.get("script")
        if script is None:
            script = bash_module._with_prefix(command, prefix)
        return command, script, prefix, bool(allow)
    if guard == "destructive_chmod":
        if len(args) > 3 or kwargs.get("heredoc_depth"):
            return None  # nested heredoc re-entry, reached through the outer call
        script = args[0]
        allow = args[1]
        prefix = args[2] if len(args) > 2 else kwargs.get("command_prefix")
        return script, script, prefix, bool(allow)
    if guard == "force_push":
        script = args[0]
        allow = args[1]
        prefix = args[2] if len(args) > 2 else kwargs.get("command_prefix")
        if prefix is None:
            return None  # the guard re-reads the environment; bash() always pins it
        return script, script, prefix or None, bool(allow)
    script, allow = args[0], args[1]
    return script, script, None, bool(allow)


def _wrap(guard: str, original):
    def wrapper(*args, **kwargs):
        call = _normalize_call(guard, args, kwargs)
        if call is not None and not call[3]:
            command, script, prefix, _allow = call
            seen.setdefault((guard, command, script, prefix), None)
        return original(*args, **kwargs)

    wrapper.__wrapped__ = original
    return wrapper


originals = {}
for guard, fn in GUARDS.items():
    originals[guard] = getattr(bash_module, fn)
    setattr(bash_module, fn, _wrap(guard, originals[guard]))

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
    unittest.TextTestRunner(stream=io.StringIO(), verbosity=0).run(suite)

for guard, fn in GUARDS.items():
    setattr(bash_module, fn, originals[guard])
# The suites patch module state; restore what the neutral run reads.
for flag in [name for name in dir(bash_module) if name.endswith("_AT_KERNEL_START") or name.endswith("_AT_START")]:
    setattr(bash_module, flag, False)
for flag in [name for name in dir(bash_module) if name.endswith("_late_bypass_warned")]:
    setattr(bash_module, flag, True)


def neutral_verdict(guard: str, command: str, script: str, prefix: str | None, root: str) -> dict:
    fn = originals[guard]
    try:
        with redirect_stderr(io.StringIO()):
            if guard == "destructive_git":
                fn(command, False, prefix, script)
            elif guard == "destructive_chmod":
                fn(script, False, prefix)
            elif guard == "force_push":
                fn(script, False, prefix or "")
            else:
                fn(script, False)
    except Exception as error:  # noqa: BLE001 - the verdict is the recorded value
        return {
            "refused": type(error).__name__,
            "message": str(error).replace(root, "<ROOT>"),
        }
    return {"allowed": True}


inputs = sorted({(command, script, prefix) for (_guard, command, script, prefix) in seen},
                key=lambda item: (item[1], item[2] or "", item[0]))
messages: list[str] = []
message_ids: dict[str, int] = {}
records = []
with tempfile.TemporaryDirectory(prefix="pa-bash-corpus-") as temp:
    root = os.path.realpath(temp)
    work = os.path.join(root, "work")
    home = os.path.join(root, "home")
    os.mkdir(work)
    os.mkdir(home)
    previous = os.getcwd()
    saved_env = dict(os.environ)
    os.chdir(work)
    os.environ.clear()
    os.environ.update({"HOME": home, "PATH": "/usr/bin:/bin", "LANG": "C.UTF-8"})
    try:
        for command, script, prefix in inputs:
            verdicts = {}
            for guard in GUARDS:
                verdict = neutral_verdict(guard, command, script, prefix, root)
                if "refused" in verdict:
                    message = verdict["message"]
                    if message not in message_ids:
                        message_ids[message] = len(messages)
                        messages.append(message)
                    verdicts[guard] = [verdict["refused"], message_ids[message]]
            record = {"script": script, "refused": verdicts}
            if prefix is not None:
                record["prefix"] = prefix
            if command != script:
                record["command"] = command
            records.append(record)
    finally:
        os.chdir(previous)
        os.environ.clear()
        os.environ.update(saved_env)

with (HERE / "guards.jsonl").open("w", encoding="utf-8") as fh:
    for record in records:
        fh.write(json.dumps(record, ensure_ascii=False, sort_keys=True) + "\n")
with (HERE / "messages.json").open("w", encoding="utf-8") as fh:
    json.dump(messages, fh, ensure_ascii=False, indent=0)
    fh.write("\n")
counts = {guard: 0 for guard in GUARDS}
for record in records:
    for guard in record["refused"]:
        counts[guard] += 1
print(f"wrote {len(records)} inputs x {len(GUARDS)} guards, {len(messages)} distinct messages")
for guard, refused in counts.items():
    print(f"  {guard}: {refused} refused, {len(records) - refused} allowed")
