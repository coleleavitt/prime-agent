"""Keep the suites' git off any repository the test runner inherited.

Git exports ``GIT_DIR``, ``GIT_WORK_TREE`` and their siblings to every hook and
``git rebase --exec`` command. A suite started from one inherits them, and every
``git`` it runs -- the fixture setup, the commands ``bash()`` sends to the host,
the guard's own probes -- then acts on that outer repository instead of the
fixture under its ``cwd``. Suites that run git call
:func:`scrub_repository_selection` in ``setUp`` (after snapshotting
``os.environ`` for their restore) and build explicit child environments with
:func:`fixture_git_env`.

Mirrors ``pa_core::git_env`` (``REPOSITORY_SELECTION_ENV``, ``isolate_fixture``).
"""

from __future__ import annotations

import os
import tempfile
from collections.abc import MutableMapping

REPOSITORY_SELECTION_ENV = (
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
)

_INJECTED_CONFIG_ENV = ("GIT_CONFIG_PARAMETERS", "GIT_CONFIG_COUNT", "GIT_CONFIG")


def scrub_repository_selection(environ: MutableMapping[str, str] | None = None) -> None:
    """Drop the repository-selecting and config-injecting git variables from ``environ``
    (``os.environ`` by default), and stop discovery at the temp root."""
    env = os.environ if environ is None else environ
    for name in (*REPOSITORY_SELECTION_ENV, *_INJECTED_CONFIG_ENV):
        env.pop(name, None)
    for name in [key for key in env if key.startswith(("GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_"))]:
        env.pop(name, None)
    env["GIT_CEILING_DIRECTORIES"] = tempfile.gettempdir()


def fixture_git_env(home: str | os.PathLike[str]) -> dict[str, str]:
    """``os.environ`` scrubbed for a fixture git: no inherited repository, no user or
    system config, an empty ``HOME`` at ``home``, discovery stopped at the temp root."""
    env = dict(os.environ)
    scrub_repository_selection(env)
    env.update(
        {
            "HOME": str(home),
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_TERMINAL_PROMPT": "0",
        }
    )
    return env
