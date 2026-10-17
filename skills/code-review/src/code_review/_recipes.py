"""The review recipes: Claude Code's ``/code-review`` finder angles, verifier and
sweep prompts per effort level, worded for a kernel subagent that reads the
diff from a file and answers with JSON.

Level shapes (CC 2.1.296's generic cells):

- low: one diff pass, no verify.
- medium: 3 correctness + 5 cleanup/altitude/conventions angles, up to 6
  candidates each, a 1-vote verify that keeps CONFIRMED and PLAUSIBLE.
- high: the medium angles with a recall-biased verifier.
- xhigh / max: 5 correctness + 5 other angles, up to 8 candidates each, a
  recall-mode 1-vote verify, then one sweep finder for gaps (up to 8).
"""

from __future__ import annotations

from dataclasses import dataclass

LEVELS = ("low", "medium", "high", "xhigh", "max")
DEFAULT_FINDINGS_CAP = 15
MAX_FINDINGS_CAP = 32
CLEANUP_CATEGORIES = frozenset({"reuse", "simplification", "efficiency", "altitude", "conventions"})

FINDING_SCHEMA = {
    "type": "object",
    "properties": {
        "file": {"type": "string"},
        "line": {"type": "integer", "minimum": 0},
        "summary": {"type": "string"},
        "short_summary": {"type": "string"},
        "failure_scenario": {"type": "string"},
        "category": {"type": "string"},
    },
    "required": ["file", "line", "summary", "short_summary", "failure_scenario", "category"],
}
FINDINGS_SCHEMA = {
    "type": "object",
    "properties": {"findings": {"type": "array", "items": FINDING_SCHEMA}},
    "required": ["findings"],
}
VERDICT_SCHEMA = {
    "type": "object",
    "properties": {
        "verdict": {"type": "string", "enum": ["CONFIRMED", "PLAUSIBLE", "REFUTED"]},
        "evidence": {"type": "string"},
    },
    "required": ["verdict", "evidence"],
}


@dataclass(frozen=True)
class Angle:
    key: str
    title: str
    category: str
    text: str


ANGLE_A = Angle(
    "line-scan",
    "Angle A - line-by-line diff scan",
    "correctness",
    "Read every hunk in the diff, line by line. Then read the enclosing function for each "
    "hunk - bugs in unchanged lines of a touched function are in scope (the change "
    "re-exposes or fails to fix them). For every line ask: what input, state, timing, or "
    "platform makes this line wrong? Look for inverted/wrong conditions, off-by-one, "
    "null/None dereference, missing `await`, falsy-zero checks, wrong-variable copy-paste, "
    "error swallowed in a catch, unescaped regex metacharacters.",
)
ANGLE_B = Angle(
    "removed-behavior",
    "Angle B - removed-behavior auditor",
    "correctness",
    "For every line the diff DELETES or replaces, name the invariant or behavior it "
    "enforced, then search the new code for where that invariant is re-established. If "
    "you can't find it, that's a candidate: a removed guard, a dropped error path, a "
    "narrowed validation, a deleted test that was covering a real case.",
)
ANGLE_C = Angle(
    "cross-file",
    "Angle C - cross-file tracer",
    "correctness",
    "For each function the diff changes, find its callers (search for the symbol) and "
    "check whether the change breaks any call site: a new precondition, a changed return "
    "shape, a new exception, a timing/ordering dependency. Also check callees: does a "
    "parallel change in the same diff make a call unsafe?",
)
ANGLE_D = Angle(
    "language-pitfalls",
    "Angle D - language-pitfall specialist",
    "correctness",
    "Scan for the classic pitfalls of the diff's language/framework - for example: JS "
    "falsy-zero, `==` coercion, closure-captured loop var; Python mutable default args, "
    "late-binding closures; Go nil-map write, range-var capture; SQL injection; "
    "timezone/DST drift; float equality. Flag any instance the diff introduces.",
)
ANGLE_E = Angle(
    "wrappers",
    "Angle E - wrapper/proxy correctness",
    "correctness",
    "When the diff adds or modifies a type that wraps another (cache, proxy, decorator, "
    "adapter): check that every method routes to the wrapped instance and not back through "
    "a registry/session/global - e.g. a caching provider holding a `delegate` field that "
    "resolves IDs via `session.get(...)` instead of `delegate.get(...)` will re-enter the "
    "cache or recurse. Also check that the wrapper forwards all the methods the callers "
    "actually use.",
)
REUSE = Angle(
    "reuse",
    "Reuse",
    "reuse",
    "This angle hunts for cleanup in the changed code, not bugs. Flag new code that "
    "re-implements something the codebase already has - search shared/utility modules and "
    "files adjacent to the change, and name the existing helper to call instead.",
)
SIMPLIFICATION = Angle(
    "simplification",
    "Simplification",
    "simplification",
    "Flag unnecessary complexity the diff adds: redundant or derivable state, copy-paste "
    "with slight variation, deep nesting, dead code left behind. Name the simpler form "
    "that does the same job.",
)
EFFICIENCY = Angle(
    "efficiency",
    "Efficiency",
    "efficiency",
    "Flag wasted work the diff introduces: redundant computation or repeated I/O, "
    "independent operations run sequentially, blocking work added to startup or hot "
    "paths. Also flag long-lived objects built from closures or captured environments - "
    "they keep the entire enclosing scope alive for the object's lifetime (a memory leak "
    "when that scope holds large values); prefer a class/struct that copies only the "
    "fields it needs. Name the cheaper alternative.",
)
ALTITUDE = Angle(
    "altitude",
    "Altitude",
    "altitude",
    "Check that each change fixes the root cause at the right depth rather than patching "
    "a symptom with a fragile bandaid. Special cases layered on shared infrastructure are a "
    "sign the fix isn't deep enough - prefer the simpler, more general change to the "
    "underlying mechanism over adding special cases, and name that change.",
)
CONVENTIONS = Angle(
    "conventions",
    "Conventions (CLAUDE.md / AGENTS.md)",
    "conventions",
    "Find the instruction files that govern the changed code: the repo-root CLAUDE.md and "
    "AGENTS.md, plus any CLAUDE.md, CLAUDE.local.md or AGENTS.md in a directory that is an "
    "ancestor of a changed file (a directory's file only applies to files at or below it). "
    "Read each one that exists, then check the diff for clear violations of the rules they "
    "state. Only flag a violation when you can quote the exact rule and the exact line that "
    "breaks it - no style preferences, no vague 'spirit of the doc' inferences. In the "
    "finding, name the file path and quote the rule. If no such file applies, return no "
    "findings for this angle.",
)

_CLEANUP_NOTE = (
    "Cleanup, altitude, and conventions candidates use the same file/line/summary shape; in "
    "failure_scenario, state the concrete cost (what is duplicated, wasted, harder to "
    "maintain, or which rule is broken) instead of a crash."
)


@dataclass(frozen=True)
class Plan:
    """One effort level's review shape."""

    level: str
    angles: tuple[Angle, ...]
    candidates_per_angle: int
    verify: str | None  # None, "precision", "recall-biased" or "recall"
    sweep: bool
    goal: str


_THREE_PLUS_FIVE = (ANGLE_A, ANGLE_B, ANGLE_C, REUSE, SIMPLIFICATION, EFFICIENCY, ALTITUDE, CONVENTIONS)
_FIVE_PLUS_FIVE = (
    ANGLE_A,
    ANGLE_B,
    ANGLE_C,
    ANGLE_D,
    ANGLE_E,
    REUSE,
    SIMPLIFICATION,
    EFFICIENCY,
    ALTITUDE,
    CONVENTIONS,
)

PLANS = {
    "low": Plan("low", (), 0, None, False, "low effort: one diff pass, no verify"),
    "medium": Plan(
        "medium",
        _THREE_PLUS_FIVE,
        6,
        "precision",
        False,
        "You are reviewing for **precision** at medium effort: every finding you surface should "
        "be one a maintainer would act on.",
    ),
    "high": Plan(
        "high",
        _THREE_PLUS_FIVE,
        6,
        "recall-biased",
        False,
        "You are reviewing for **recall** at high effort: catch every real bug a careful reviewer "
        "would catch in one sitting. At this level, catching real bugs matters more than avoiding "
        "false positives. Err on the side of surfacing.",
    ),
    "xhigh": Plan(
        "xhigh",
        _FIVE_PLUS_FIVE,
        8,
        "recall",
        True,
        "You are reviewing for **recall** at extra-high effort: catch every real bug. At this "
        "level, catching real bugs matters more than avoiding false positives - a missed bug "
        "ships. Err on the side of surfacing.",
    ),
    "max": Plan(
        "max",
        _FIVE_PLUS_FIVE,
        8,
        "recall",
        True,
        "You are reviewing for **recall** at maximum effort: catch every real bug. At this level, "
        "catching real bugs matters more than avoiding false positives - a missed bug ships. Err "
        "on the side of surfacing.",
    ),
}


def _scope_text(diff_path: str, diff: str, inline_limit: int, cwd: str) -> str:
    """How a subagent reaches the diff under review."""
    text = (
        f"The review scope is the unified diff saved at {diff_path} (working directory {cwd}). "
        "Read it, and open the surrounding files for context as needed."
    )
    if len(diff) <= inline_limit:
        text += f"\n\nThe diff:\n```diff\n{diff}\n```"
    return text


def _low_prompt(scope: str) -> str:
    return (
        "Review this diff for real bugs in one careful pass.\n\n"
        f"{scope}\n\n"
        "Skip test/fixture hunks (`test/`, `tests/`, `spec/`, `__tests__/`, `*_test.*`, "
        "`*.test.*`, `fixtures/`, `testdata/`) - test-file changes are not reviewed at this level. "
        "No subagents, no full-file reads.\n\n"
        "Flag runtime-correctness bugs visible from the hunk alone: inverted/wrong condition, "
        "off-by-one, null/None dereference where adjacent lines show the value can be absent, "
        "removed guard, falsy-zero check, missing `await`, wrong-variable copy-paste, error "
        "swallowed in a catch that should propagate. Also flag - still from the hunk alone - new "
        "code that duplicates an existing helper visible in the diff context, and dead code the "
        "diff leaves behind. Do **not** flag style, naming, perf, missing tests, or anything "
        "outside the hunk.\n\n"
        f"Report at most {DEFAULT_FINDINGS_CAP} findings, most severe first; `short_summary` is the "
        "claim in at most 60 characters with no rationale clause; `category` is `correctness` "
        "unless the finding is a cleanup (`reuse`, `simplification`). An empty list is a fine "
        "answer."
    )


def _finder_prompt(plan: Plan, angle: Angle, scope: str) -> str:
    cleanup = angle.category in CLEANUP_CATEGORIES
    return (
        f"{plan.goal}\n\n"
        f"You are one independent finder in a multi-agent code review. Your angle:\n\n"
        f"### {angle.title}\n\n{angle.text}\n\n"
        f"{scope}\n\n"
        f"Surface up to {plan.candidates_per_angle} candidate findings, each with `file`, `line`, "
        "a one-line `summary`, `short_summary` (the claim in at most 60 characters, no rationale "
        "or consequence clause), a concrete `failure_scenario`, and `category` "
        f"(`{angle.category}`, or a more specific kebab-case slug when one fits better). "
        + (f"{_CLEANUP_NOTE} " if cleanup else "")
        + "Do not let other angles' concerns suppress yours. Pass every candidate with a nameable "
        "failure scenario through - finders that silently drop half-believed candidates bypass "
        "the verify step and are the dominant cause of misses. If nothing qualifies, return an "
        "empty list."
    )


def _verifier_prompt(plan: Plan, finding: dict, scope: str) -> str:
    candidate = (
        f"- file: {finding.get('file')}\n- line: {finding.get('line')}\n"
        f"- summary: {finding.get('summary')}\n- failure scenario: {finding.get('failure_scenario')}\n"
        f"- category: {finding.get('category')}"
    )
    if plan.verify == "recall-biased":
        rules = (
            "**PLAUSIBLE by default** - do not refute a candidate for being 'speculative' or "
            "'depends on runtime state' when the state is realistic: concurrency races, "
            "nil/None on a rare-but-reachable path (error handler, cold cache, missing optional "
            "field), falsy-zero treated as missing, off-by-one on a boundary the code does not "
            "exclude, retry storms / partial failures, regex/allowlist that lost an anchor. These "
            "are PLAUSIBLE.\n\n**REFUTED** only when constructible from the code: factually wrong "
            "(quote the actual line); provably impossible (type/constant/invariant - show it); "
            "already handled in this diff (cite the guard); or pure style with no observable effect."
        )
    else:
        rules = (
            "- **CONFIRMED** - you can name the inputs/state that trigger it and the wrong output "
            "or crash. Quote the line.\n- **PLAUSIBLE** - the mechanism is real, the trigger is "
            "uncertain (timing, env, config). State what would confirm it.\n- **REFUTED** - "
            "factually wrong (the code doesn't say that) or guarded elsewhere. Quote the line "
            "that proves it."
        )
        if plan.verify == "recall":
            rules += (
                "\n\nThis is recall mode - a single non-REFUTED vote carries the finding. Do NOT "
                "refute on uncertainty."
            )
    return (
        "You verify one candidate finding from a code review. Read the diff and the relevant "
        f"file(s), then return exactly one verdict with your evidence.\n\n{scope}\n\n"
        f"The candidate:\n{candidate}\n\n{rules}"
    )


def _sweep_prompt(plan: Plan, verified: list[dict], scope: str) -> str:
    listed = "\n".join(
        f"- {item.get('file')}:{item.get('line')} - {item.get('summary')}" for item in verified
    ) or "- (nothing yet)"
    return (
        f"{plan.goal}\n\nYou are a fresh reviewer sweeping for gaps. Re-read the diff and the "
        "enclosing functions looking ONLY for defects not already listed below. Do not re-derive "
        "or re-confirm anything already there - the job is gaps. Focus on what the first pass "
        "tends to miss: moved/extracted code that dropped a guard or anchor; second-tier "
        "footguns (dataclass default evaluated once, `hash()` non-determinism, lock-scope "
        "shrink, predicate methods with side effects); setup/teardown asymmetry in tests; config "
        f"defaults flipped.\n\n{scope}\n\nAlready listed:\n{listed}\n\n"
        f"Surface up to {plan.candidates_per_angle} additional candidates, each naming a defect "
        "not already on the list, with the same fields as the list. If nothing new, return an "
        "empty list - do not pad."
    )
