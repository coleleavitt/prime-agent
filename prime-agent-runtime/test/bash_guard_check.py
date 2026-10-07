"""One guard's verdict through the public check path (`bash.check`, no process
starts), for the guard suites' detection tables. The guards run in the Prime
Agent host (pa-bash); their internals' own tests live there."""

from __future__ import annotations

import sys

import rlm.bash  # noqa: F401 - registers the module

bash_module = sys.modules["rlm.bash"]


def refusal(guard: str, command: str, prefix: str | None = None) -> str | None:
    """The refusal message `guard` gives `command` run under `prefix` (every
    other guard bypassed), or None."""
    _kwarg, _flag, _warned, error = bash_module._GUARDS[guard]
    allow = {other: True for key, (other, _, _, _) in bash_module._GUARDS.items() if key != guard}
    try:
        bash_module._run_kernel_bash_guards(command, bash_module._with_prefix(command, prefix), prefix, **allow)
    except error as refused:
        return str(refused)
    return None


def phrase(guard: str, command: str, before: str, after: str) -> str | None:
    """The violation phrase between `before` and `after` in `guard`'s refusal of `command`."""
    message = refusal(guard, command)
    if message is None:
        return None
    start = message.index(before) + len(before)
    return message[start : message.index(after, start)]
