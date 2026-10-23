"""``load_skill(name, args="")``: Claude Code's Skill tool as a kernel function.

The host resolves ``name`` against the session's skills (the inventory in the session
context, and the skills discovered under directories the session touched) and replies
with the text Claude Code injects when the model invokes a skill: for a Claude Code
skill (``.claude/skills/<name>/SKILL.md``) ``Base directory for this skill: <dir>``
and the body, with ``args`` substituted for ``$ARGUMENTS`` / ``$N`` / ``$name``
(appended as ``ARGUMENTS: ...`` when nothing consumed them), ``${CLAUDE_SKILL_DIR}``,
``${CLAUDE_PROJECT_DIR}`` and ``${CLAUDE_SESSION_ID}`` replaced and its
``!`cmd` `` blocks run; for a Prime Agent skill its body. A skill marked
``disable-model-invocation`` is refused: only the user can run it.
"""

from __future__ import annotations

SKILL_LOAD_REQUEST = "skill.load"


async def load_skill(name: str, args: str = "") -> str:
    """Load a skill's instructions (with ``args`` substituted) and return them; follow them."""
    if not isinstance(name, str) or not name.strip():
        raise TypeError("load_skill name must be a non-empty str")
    if not isinstance(args, str):
        raise TypeError(f"load_skill args must be a str, got {type(args).__name__}")
    from . import host_request

    payload = await host_request(SKILL_LOAD_REQUEST, {"name": name, "args": args})
    text = payload.get("text") if isinstance(payload, dict) else None
    if not isinstance(text, str):
        raise RuntimeError("rlm.load_skill received an invalid reply")
    return text
