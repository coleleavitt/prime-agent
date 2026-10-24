"""Prime Agent code-review skill: Claude Code's ``/code-review`` as a workflow.

``await code_review.review("high --fix")`` gathers the diff under review,
fans finder angles out to subagents through ``rlm.workflow``, verifies every
candidate, and returns the findings as a ranked table. ``/code-review`` (and
``/review``) in the session expands to this skill.

Findings carry Claude Code's fields: ``file``, ``line``, ``summary``,
``short_summary`` (at most 60 characters), ``failure_scenario``,
``category`` and ``verdict`` (``CONFIRMED`` / ``PLAUSIBLE`` when a verify
pass produced one). ``report_outcomes`` records what became of each finding
after ``--fix`` (``fixed`` / ``no_change_needed`` / ``skipped``).

``ultra`` is Claude Code's cloud review, which runs on Anthropic's
infrastructure; here it runs the local ``max``-effort review and says so.
"""

from __future__ import annotations

import json
import os
import re
import secrets
import subprocess
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from ._recipes import (
    CLEANUP_CATEGORIES,
    DEFAULT_FINDINGS_CAP,
    FINDINGS_SCHEMA,
    LEVELS,
    MAX_FINDINGS_CAP,
    PLANS,
    SIMPLIFY_PLAN,
    VERDICT_SCHEMA,
    Plan,
    _finder_prompt,
    _low_prompt,
    _scope_text,
    _sweep_prompt,
    _verifier_prompt,
)
from ._security import (
    FILTER_SCHEMA,
    MIN_CONFIDENCE,
    SECURITY_FINDINGS_SCHEMA,
    SEVERITIES,
    _filter_prompt,
)
from ._security import _finder_prompt as _security_finder_prompt

__all__ = [
    "Review",
    "ReviewArgs",
    "load_review",
    "report_findings",
    "report_outcomes",
    "review",
    "security_review",
    "simplify",
]

OUTCOMES = ("fixed", "no_change_needed", "skipped")
_MAX_FINDINGS = re.compile(r"(^|\s)--max-findings(?:(?:\s*=\s*|\s+)(?!--)(\S+)|\s*=\S*)?(?=\s|$)")
_LEVEL_LIKE = re.compile(r"^(low|med|hig|xhi|max)[a-z]*$", re.IGNORECASE)
_FLAGS = ("comment", "fix", "post", "no-post")
_INLINE_DIFF_CHARS = 40_000
_STATE_FILE = "code-review.json"
_REVIEW_DIR = "code-review"


@dataclass(frozen=True)
class ReviewArgs:
    """``/code-review`` arguments, parsed the way Claude Code's ``Te()`` parses them."""

    level: str | None = None
    target: str = ""
    fix: bool = False
    comment: bool = False
    post: bool = False
    max_findings: int | str | None = None
    max_findings_ignored: bool = False
    unrecognized_level: str | None = None
    ultra: bool = False


def _parse_args(text: str = "") -> ReviewArgs:
    """Parse ``[low|medium|high|xhigh|max|ultra] [--fix] [--comment] [<pr#>|<branch>|<path>]
    [--max-findings <n>|all|default]``."""
    if not isinstance(text, str):
        raise TypeError("args must be a string")
    raw_first = text.split()[0] if text.split() else ""
    tokens = _MAX_FINDINGS.sub(r"\1", text).split()
    flags = {token[2:] for token in tokens if token.startswith("--") and token[2:] in _FLAGS}
    rest = [token for token in tokens if not (token.startswith("--") and token[2:] in _FLAGS)]
    matches = list(_MAX_FINDINGS.finditer(text))
    asked = (matches[-1].group(2) or "").lower() if matches else None
    max_findings: int | str | None = None
    if asked in ("all", "default"):
        max_findings = asked
    elif asked is not None and asked.isdigit() and int(asked) > 0:
        max_findings = int(asked)
    common = {
        "fix": "fix" in flags,
        "comment": "comment" in flags,
        "post": "post" in flags,
        "max_findings": max_findings,
        "max_findings_ignored": asked is not None and max_findings is None,
    }
    if raw_first.lower() == "ultra":
        return ReviewArgs(target=_target(rest[1:]), ultra=True, **common)
    first = rest[0] if rest else ""
    if first.lower() in LEVELS:
        return ReviewArgs(level=first.lower(), target=_target(rest[1:]), **common)
    unrecognized = first if first and _LEVEL_LIKE.match(first) else None
    return ReviewArgs(target=_target(rest), unrecognized_level=unrecognized, **common)


def _target(tokens: list[str]) -> str:
    if not tokens:
        return ""
    first = tokens[0].replace("`", "")
    first = first[1:] if first.startswith("#") else first
    return " ".join(token for token in [first, *tokens[1:]] if token)


# --- persistence ------------------------------------------------------------


def _state_path() -> Path | None:
    agent_dir = os.environ.get("PRIME_AGENT_CODING_AGENT_DIR")
    return Path(agent_dir) / _STATE_FILE if agent_dir else None


def _load_state() -> dict[str, Any]:
    path = _state_path()
    if path is None:
        return {}
    try:
        state = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}
    return state if isinstance(state, dict) else {}


def _save_state(state: dict[str, Any]) -> None:
    path = _state_path()
    if path is None:
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(".json.tmp")
    temporary.write_text(json.dumps(state, indent=2), encoding="utf-8")
    os.replace(temporary, path)


def _reviews_dir() -> Path:
    session_dir = os.environ.get("RLM_SESSION_DIR")
    if session_dir:
        return Path(session_dir) / _REVIEW_DIR
    import tempfile

    return Path(tempfile.gettempdir()) / "prime-agent-code-review"


# --- the review record ------------------------------------------------------


@dataclass
class Review:
    """One review: its findings (ranked, capped) and how they render."""

    review_id: str
    level: str
    target: str
    scope: str
    findings: list[dict[str, Any]] = field(default_factory=list)
    notices: list[str] = field(default_factory=list)
    next_steps: list[str] = field(default_factory=list)
    candidates: int = 0
    dropped: int = 0
    run_id: str | None = None
    status: str = "completed"
    error: str | None = None
    args: str = ""
    cwd: str = ""
    # Set for a review that is not `/code-review`'s (`/simplify`,
    # `/security-review`, `report_findings`): the table's heading.
    title: str | None = None

    def table(self) -> str:
        """The findings as a Markdown table plus one detail entry per finding."""
        with_outcomes = any(finding.get("outcome") for finding in self.findings)
        heading = self.title or f"Code review ({self.level} effort)"
        header = f"{heading}: {_plural(len(self.findings), 'finding')}"
        extra = []
        if self.candidates:
            extra.append(f"{_plural(self.candidates, 'candidate')} considered")
        if self.dropped:
            extra.append(f"{self.dropped} more over the cap not shown")
        if extra:
            header += f" ({'; '.join(extra)})"
        lines = [header]
        if self.status != "completed":
            lines.append(f"The review {self.status}: {self.error}")
        if not self.findings:
            return "\n".join(lines)
        # Security findings carry a severity where the verify pass's verdict goes.
        rated = any(finding.get("severity") for finding in self.findings)
        columns = ["#", "Location", "Category", "Severity" if rated else "Verdict", "Finding"] + (
            ["Outcome"] if with_outcomes else []
        )
        lines += ["", "| " + " | ".join(columns) + " |", "|" + "---|" * len(columns)]
        for finding in self.findings:
            cells = [
                str(finding["index"]),
                f"{finding.get('file', '?')}:{finding.get('line', '?')}",
                finding.get("category") or "correctness",
                (finding.get("severity") if rated else finding.get("verdict")) or "-",
                finding.get("short_summary") or finding.get("summary", ""),
            ]
            if with_outcomes:
                cells.append(finding.get("outcome") or "-")
            lines.append("| " + " | ".join(_cell(cell) for cell in cells) + " |")
        lines.append("")
        for finding in self.findings:
            lines.append(
                f"{finding['index']}. {finding.get('file')}:{finding.get('line')} - {finding.get('summary')}"
            )
            if finding.get("failure_scenario"):
                lines.append(f"   Scenario: {finding['failure_scenario']}")
            if finding.get("recommendation"):
                lines.append(f"   Fix: {finding['recommendation']}")
        return "\n".join(lines)

    def __repr__(self) -> str:
        parts = [*self.notices, self.table()]
        if self.next_steps:
            parts.append("\n\n".join(self.next_steps))
        return "\n\n".join(part for part in parts if part)

    __str__ = __repr__

    def to_json(self) -> dict[str, Any]:
        return {
            "reviewId": self.review_id,
            "level": self.level,
            "target": self.target,
            "scope": self.scope,
            "findings": self.findings,
            "notices": self.notices,
            "nextSteps": self.next_steps,
            "candidates": self.candidates,
            "dropped": self.dropped,
            "runId": self.run_id,
            "status": self.status,
            "error": self.error,
            "args": self.args,
            "cwd": self.cwd,
            "title": self.title,
        }

    @classmethod
    def from_json(cls, data: dict[str, Any]) -> Review:
        return cls(
            review_id=data["reviewId"],
            level=data["level"],
            target=data.get("target", ""),
            scope=data.get("scope", ""),
            findings=list(data.get("findings") or []),
            notices=list(data.get("notices") or []),
            next_steps=list(data.get("nextSteps") or []),
            candidates=int(data.get("candidates") or 0),
            dropped=int(data.get("dropped") or 0),
            run_id=data.get("runId"),
            status=data.get("status", "completed"),
            error=data.get("error"),
            args=data.get("args", ""),
            cwd=data.get("cwd", ""),
            title=data.get("title"),
        )

    def save(self) -> Path:
        directory = _reviews_dir()
        directory.mkdir(parents=True, exist_ok=True)
        path = directory / f"{self.review_id}.json"
        temporary = path.with_suffix(".json.tmp")
        temporary.write_text(json.dumps(self.to_json(), indent=2, ensure_ascii=False), encoding="utf-8")
        os.replace(temporary, path)
        (directory / "latest").write_text(self.review_id, encoding="utf-8")
        return path


def _plural(count: int, noun: str) -> str:
    return f"{count} {noun}{'' if count == 1 else 's'}"


def _cell(text: str) -> str:
    return " ".join(str(text).split()).replace("|", "\\|")


def load_review(review_id: str | None = None) -> Review:
    """A saved review (the latest one in this session when ``review_id`` is omitted)."""
    directory = _reviews_dir()
    if review_id is None:
        try:
            review_id = (directory / "latest").read_text(encoding="utf-8").strip()
        except OSError as error:
            raise LookupError("no code review has run in this session") from error
    try:
        data = json.loads((directory / f"{review_id}.json").read_text(encoding="utf-8"))
    except (OSError, ValueError) as error:
        raise LookupError(f"no saved code review {review_id}") from error
    return Review.from_json(data)


def report_outcomes(outcomes: dict[int | str, str], *, review_id: str | None = None) -> Review:
    """Record what became of each finding (``fixed``, ``no_change_needed`` or ``skipped``),
    keyed by the finding's ``#``; returns the review with its Outcome column."""
    current = load_review(review_id)
    by_index = {finding["index"]: finding for finding in current.findings}
    for key, outcome in outcomes.items():
        index = int(str(key).lstrip("#"))
        if index not in by_index:
            raise KeyError(f"review {current.review_id} has no finding #{index}")
        if outcome not in OUTCOMES:
            raise ValueError(f"outcome must be one of {', '.join(OUTCOMES)}, got {outcome!r}")
        by_index[index]["outcome"] = outcome
    current.next_steps = []
    current.save()
    return current


# --- the diff ---------------------------------------------------------------


def _git(cwd: str, *args: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        [
            "git",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=",
            "-C",
            cwd,
            *args,
        ],
        capture_output=True,
        text=True,
        check=False,
    )


def _diff(cwd: str, *spec: str) -> str | None:
    done = _git(cwd, "diff", "--no-ext-diff", "--no-textconv", *spec)
    return done.stdout if done.returncode == 0 else None


def _default_base(cwd: str) -> str | None:
    head = _git(cwd, "symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD")
    if head.returncode == 0 and head.stdout.strip():
        return head.stdout.strip()
    for name in ("main", "master"):
        if _git(cwd, "rev-parse", "--verify", "--quiet", name).returncode == 0:
            return name
    return None


def _gather_diff(cwd: str, target: str) -> tuple[str, str]:
    """Phase 0: the unified diff under review and a description of its scope."""
    if _git(cwd, "rev-parse", "--is-inside-work-tree").returncode != 0:
        raise RuntimeError(f"{cwd} is not inside a git repository")
    if target:
        if re.fullmatch(r"\d+", target):
            done = subprocess.run(["gh", "pr", "diff", target], cwd=cwd, capture_output=True, text=True, check=False)
            if done.returncode != 0:
                raise RuntimeError(f"gh pr diff {target} failed: {done.stderr.strip() or 'gh is unavailable'}")
            return done.stdout, f"pull request #{target}"
        if ".." in target:
            diff = _diff(cwd, target, "--")
            if diff is None:
                raise RuntimeError(f"git diff {target} failed")
            return diff, f"the range {target}"
        if (Path(cwd) / target).exists():
            base = _default_base(cwd)
            committed = _diff(cwd, f"{base}...HEAD", "--", target) if base else ""
            working = _diff(cwd, "HEAD", "--", target) or ""
            return (committed or "") + working, f"changes to {target}"
        if _git(cwd, "rev-parse", "--verify", "--quiet", target).returncode == 0:
            base = _default_base(cwd) or "HEAD~1"
            diff = _diff(cwd, f"{base}...{target}", "--")
            if diff is None:
                raise RuntimeError(f"git diff {base}...{target} failed")
            return diff, f"the branch {target} against {base}"
        raise RuntimeError(f"the review target {target!r} is not a PR number, range, path or branch")
    # `git diff @{upstream}...HEAD`, else `main...HEAD`, else `HEAD~1`; the
    # working tree's changes join the scope whenever there are any.
    committed, scope = _diff(cwd, "@{upstream}...HEAD", "--"), "@{upstream}...HEAD"
    if committed is None:
        base = _default_base(cwd)
        if base is not None:
            committed, scope = _diff(cwd, f"{base}...HEAD", "--"), f"{base}...HEAD"
        else:
            committed, scope = _diff(cwd, "HEAD~1", "--"), "HEAD~1"
    working = _diff(cwd, "HEAD", "--") or ""
    committed = committed or ""
    if working.strip():
        return committed + working, f"{scope} plus uncommitted changes" if committed.strip() else "uncommitted changes"
    return committed, scope


# --- ranking ----------------------------------------------------------------


_VERDICT_RANK = {"CONFIRMED": 0, "PLAUSIBLE": 1}


def _dedupe(findings: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Merge candidates that point at the same line and claim, keeping the most
    concrete failure scenario."""
    kept: dict[tuple[str, Any, str], dict[str, Any]] = {}
    for finding in findings:
        key = (
            str(finding.get("file", "")).strip(),
            finding.get("line"),
            " ".join(str(finding.get("short_summary") or finding.get("summary", "")).lower().split()),
        )
        previous = kept.get(key)
        if previous is None or len(finding.get("failure_scenario", "")) > len(previous.get("failure_scenario", "")):
            kept[key] = finding
    return list(kept.values())


def _rank(findings: list[dict[str, Any]]) -> list[dict[str, Any]]:
    """Correctness before cleanup, then CONFIRMED before PLAUSIBLE before unverified."""
    return sorted(
        findings,
        key=lambda finding: (
            1 if (finding.get("category") or "correctness") in CLEANUP_CATEGORIES else 0,
            _VERDICT_RANK.get(finding.get("verdict") or "", 2),
        ),
    )


def _normalize(finding: dict[str, Any], category: str) -> dict[str, Any]:
    short = " ".join(str(finding.get("short_summary") or finding.get("summary", "")).split())
    return {
        "file": str(finding.get("file", "")),
        "line": finding.get("line", 0),
        "summary": str(finding.get("summary", "")),
        "short_summary": short if len(short) <= 60 else short[:57].rstrip() + "...",
        "failure_scenario": str(finding.get("failure_scenario", "")),
        "category": str(finding.get("category") or category),
    }


def _review_script(plan: Plan, scope: str):
    async def main(wf):
        if plan.level == "low":
            wf.phase("Review")
            answer = await wf.agent(_low_prompt(scope), label="review:low", phase="Review", schema=FINDINGS_SCHEMA)
            found = [_normalize(item, "correctness") for item in (answer or {}).get("findings", [])]
            return {"findings": found, "candidates": len(found)}
        wf.phase("Find")
        answers = await wf.parallel(
            [
                lambda angle=angle: wf.agent(
                    _finder_prompt(plan, angle, scope),
                    label=f"find:{angle.key}",
                    phase="Find",
                    schema=FINDINGS_SCHEMA,
                )
                for angle in plan.angles
            ]
        )
        missing = [angle.key for angle, answer in zip(plan.angles, answers) if answer is None]
        if missing:
            wf.log(f"finder angle(s) returned nothing: {', '.join(missing)}")
        candidates = _dedupe(
            [
                _normalize(item, angle.category)
                for angle, answer in zip(plan.angles, answers)
                for item in (answer or {}).get("findings", [])[: plan.candidates_per_angle]
            ]
        )
        wf.log(f"{_plural(len(candidates), 'candidate')} after dedup")
        verified: list[dict[str, Any]] = []
        if candidates and plan.verify is None:
            verified = candidates
        elif candidates:
            wf.phase("Verify")
            verdicts = await wf.parallel(
                [
                    lambda candidate=candidate: wf.agent(
                        _verifier_prompt(plan, candidate, scope),
                        label=f"verify:{candidate['file']}:{candidate['line']}",
                        phase="Verify",
                        schema=VERDICT_SCHEMA,
                    )
                    for candidate in candidates
                ]
            )
            for candidate, verdict in zip(candidates, verdicts):
                if verdict is None:
                    # No vote: recall levels keep the candidate rather than drop it on a failure.
                    if plan.verify == "recall":
                        verified.append(candidate)
                    continue
                if verdict["verdict"] != "REFUTED":
                    verified.append({**candidate, "verdict": verdict["verdict"]})
            refuted = len(candidates) - len(verified)
            if refuted:
                wf.log(f"{refuted} refuted or unverified candidate(s) dropped")
        if plan.sweep:
            wf.phase("Sweep")
            extra = await wf.agent(
                _sweep_prompt(plan, verified, scope), label="sweep", phase="Sweep", schema=FINDINGS_SCHEMA
            )
            known = {(item["file"], item["line"]) for item in verified}
            fresh = [
                _normalize(item, "correctness")
                for item in (extra or {}).get("findings", [])[: plan.candidates_per_angle]
            ]
            verified += [item for item in fresh if (item["file"], item["line"]) not in known]
        return {"findings": verified, "candidates": len(candidates)}

    main.meta = {"name": f"code-review-{plan.level}", "description": f"{plan.level}-effort code review"}
    return main


# --- notices ------------------------------------------------------------------


def _level_notices(args: ReviewArgs, level: str, last: str | None) -> list[str]:
    notices = []
    if args.ultra:
        notices.append(
            "ultra is Claude Code's cloud review, which runs on Anthropic's infrastructure and is not "
            "available here; running the local max-effort review instead."
        )
        if args.post:
            notices.append("`--post` applies only to the cloud review and was ignored.")
        return notices
    if args.post:
        notices.append("`--post` applies only to the `ultra` cloud review and was ignored.")
    if args.unrecognized_level is not None:
        notices.append(
            f'Ignoring unrecognized effort "{args.unrecognized_level}"; valid: {", ".join(LEVELS)}. '
            f"Using {level}{', the level typed last time' if last == level else ''}."
        )
    elif args.level is None and last is not None:
        notices.append(
            f"No effort level given - reusing {last}, the level typed last time; typing a level "
            "(for example `/code-review high`) changes it."
        )
    return notices


def _cap(args: ReviewArgs, state: dict[str, Any]) -> tuple[int, list[str]]:
    notices = []
    last = state.get("lastMaxFindings")
    reused = args.max_findings is None and (last == "all" or (isinstance(last, int) and last > 0))
    asked = last if reused else (None if args.max_findings == "default" else args.max_findings)
    if args.max_findings_ignored:
        notices.append(
            "`--max-findings` was ignored: type a whole number above zero, `all`, or `default` after it."
            + ("" if reused else " Using the usual limit.")
        )
    stated = MAX_FINDINGS_CAP if asked == "all" or (isinstance(asked, int) and asked > MAX_FINDINGS_CAP) else asked
    if asked is not None and stated != asked:
        notices.append(f"This review reports at most {MAX_FINDINGS_CAP} findings, so the limit is {MAX_FINDINGS_CAP} here.")
    elif reused and asked is not None:
        notices.append(
            f"Reporting every finding (up to {MAX_FINDINGS_CAP}), as chosen last time with `--max-findings all`."
            if asked == "all"
            else f"Reporting up to {_plural(asked, 'finding')}, the limit set last time with `--max-findings`; "
            "passing `--max-findings default` goes back to the usual limit."
        )
    return (stated if isinstance(stated, int) else DEFAULT_FINDINGS_CAP), notices


def _next_steps(args: ReviewArgs, findings: list[dict[str, Any]]) -> list[str]:
    steps = []
    if args.fix and findings:
        steps.append(
            "Applying fixes (--fix): apply the findings to the working tree instead of stopping at the "
            "report: fix each one directly - correctness bugs and reuse/simplification/efficiency "
            "cleanups alike. Skip any finding whose fix would change intended behavior, require "
            "changes well outside the reviewed diff, or that you judge to be a false positive - note "
            "the skip rather than arguing with it. Then call `code_review.report_outcomes({<#>: "
            "'fixed' | 'no_change_needed' | 'skipped', ...})` with every finding; after the call, give "
            "one line per skipped finding saying why."
        )
    if args.comment:
        steps.append(
            "Posting to GitHub (--comment): if the review target is a GitHub PR, post each finding as an "
            "inline PR comment with `gh api repos/{owner}/{repo}/pulls/{pr}/comments` (one call per "
            "finding; include a suggestion block only when it fully fixes the issue). If the target is "
            "not a PR, print the findings and note that `--comment` was ignored."
        )
    if findings and not args.fix:
        steps.append(
            "If these findings get fixed later in this session, call `code_review.report_outcomes(...)` "
            "with each finding's outcome right after the fixes land."
        )
    return steps


# --- the entry point ----------------------------------------------------------


async def review(
    args: str = "",
    *,
    cwd: str | None = None,
    resume_from: str | None = None,
    backend: Any = None,
) -> Review:
    """Run a code review with ``/code-review`` arguments and return the ranked findings.

    ``resume_from`` re-runs an interrupted review by its id: its finder and
    verifier agents that already finished replay from the workflow journal.
    """
    from rlm import workflow

    previous: Review | None = None
    if resume_from is not None:
        previous = load_review(resume_from)
        args = previous.args
    parsed = _parse_args(args)
    state = _load_state()
    last = state.get("lastEffort") if state.get("lastEffort") in LEVELS else None
    if parsed.ultra:
        level = "max"
    elif parsed.level is not None:
        level = parsed.level
    else:
        level = last or "medium"
    if parsed.level is not None or parsed.max_findings is not None:
        if parsed.level is not None:
            state["lastEffort"] = parsed.level
        if parsed.max_findings is not None:
            if parsed.max_findings == "default":
                state.pop("lastMaxFindings", None)
            else:
                state["lastMaxFindings"] = parsed.max_findings
        _save_state(state)
    notices = _level_notices(parsed, level, last)
    cap, cap_notices = _cap(parsed, state)
    notices += cap_notices

    workdir = cwd or (previous.cwd if previous and previous.cwd else os.getcwd())
    review_id = previous.review_id if previous else "cr_" + secrets.token_hex(6)
    directory = _reviews_dir()
    directory.mkdir(parents=True, exist_ok=True)
    diff_path = directory / f"{review_id}.diff"
    if previous is not None and diff_path.exists():
        diff, scope_description = diff_path.read_text(encoding="utf-8"), previous.scope
    else:
        diff, scope_description = _gather_diff(workdir, parsed.target)
        diff_path.write_text(diff, encoding="utf-8")
    current = Review(
        review_id=review_id,
        level=level,
        target=parsed.target,
        scope=scope_description,
        notices=notices,
        args=args,
        cwd=workdir,
    )
    if not diff.strip():
        current.notices.append(f"Nothing to review: the diff ({scope_description}) is empty.")
        current.save()
        return current

    plan = PLANS[level]
    scope = _scope_text(str(diff_path), diff, _INLINE_DIFF_CHARS, workdir)
    outcome = await workflow.run(
        _review_script(plan, scope),
        name=f"code-review-{level}",
        resume_from=previous.run_id if previous and previous.run_id else None,
        backend=backend,
    )
    current.run_id = outcome.run_id
    if outcome.status != "completed":
        current.status, current.error = outcome.status, outcome.error
        current.notices.append(f"Resume it with `await code_review.review(resume_from={review_id!r})`.")
        current.save()
        return current
    findings = _rank(outcome.result["findings"])
    current.candidates = outcome.result["candidates"]
    current.dropped = max(0, len(findings) - cap)
    current.findings = [{**finding, "index": index} for index, finding in enumerate(findings[:cap], 1)]
    current.next_steps = _next_steps(parsed, current.findings)
    current.save()
    return current


# --- /simplify, /security-review and ReportFindings -----------------------


def _prepare(prefix: str, title: str, target: str, cwd: str | None) -> tuple[Review, str]:
    """A fresh titled review over the target's diff, its diff saved beside it."""
    workdir = cwd or os.getcwd()
    review_id = prefix + secrets.token_hex(6)
    directory = _reviews_dir()
    directory.mkdir(parents=True, exist_ok=True)
    diff, scope_description = _gather_diff(workdir, target.strip())
    diff_path = directory / f"{review_id}.diff"
    diff_path.write_text(diff, encoding="utf-8")
    current = Review(
        review_id=review_id,
        level=prefix.rstrip("_"),
        target=target.strip(),
        scope=scope_description,
        args=target,
        cwd=workdir,
        title=title,
    )
    if not diff.strip():
        current.notices.append(f"Nothing to review: the diff ({scope_description}) is empty.")
    return current, _scope_text(str(diff_path), diff, _INLINE_DIFF_CHARS, workdir)


def _finish(current: Review, outcome: Any, findings: list[dict[str, Any]], candidates: int) -> Review:
    current.run_id = outcome.run_id
    if outcome.status != "completed":
        current.status, current.error = outcome.status, outcome.error
        current.save()
        return current
    current.candidates = candidates
    current.dropped = max(0, len(findings) - MAX_FINDINGS_CAP)
    current.findings = [
        {**finding, "index": index} for index, finding in enumerate(findings[:MAX_FINDINGS_CAP], 1)
    ]
    return current


async def simplify(target: str = "", *, cwd: str | None = None, backend: Any = None) -> Review:
    """``/simplify``: four cleanup agents (reuse, simplification, efficiency,
    altitude) over the diff, no verify pass; the caller applies the findings."""
    from rlm import workflow

    current, scope = _prepare("sim_", "Simplify (reuse, simplification, efficiency, altitude)", target, cwd)
    if current.notices:
        current.save()
        return current
    outcome = await workflow.run(_review_script(SIMPLIFY_PLAN, scope), name="simplify", backend=backend)
    result = outcome.result or {}
    current = _finish(current, outcome, _dedupe(result.get("findings", [])), result.get("candidates", 0))
    if current.status == "completed":
        current.next_steps = [
            "Apply the fixes: dedup findings that point at the same line or mechanism, and fix each "
            "remaining one directly. Skip any finding whose fix would change intended behavior, require "
            "changes well outside the reviewed diff, or that you judge to be a false positive - note the "
            "skip rather than arguing with it. Then call `code_review.report_outcomes({<#>: 'fixed' | "
            "'no_change_needed' | 'skipped', ...})` with every finding and finish with a brief summary of "
            "what was fixed and what was skipped (or confirm the code was already clean)."
            if current.findings
            else "No cleanups found: say the changed code is already clean."
        ]
    current.save()
    return current


def _security_finding(item: dict[str, Any]) -> dict[str, Any]:
    severity = str(item.get("severity", "")).upper()
    description = " ".join(str(item.get("description", "")).split())
    category = str(item.get("category") or "security")
    short = f"{category}: {description}"
    return {
        "file": str(item.get("file", "")),
        "line": item.get("line", 0),
        "summary": description,
        "short_summary": short if len(short) <= 60 else short[:57].rstrip() + "...",
        "failure_scenario": str(item.get("exploit_scenario", "")),
        "recommendation": str(item.get("recommendation", "")),
        "category": category,
        "severity": severity if severity in SEVERITIES else "MEDIUM",
    }


def _security_script(scope: str):
    async def main(wf):
        wf.phase("Find")
        answer = await wf.agent(
            _security_finder_prompt(scope), label="find:security", phase="Find", schema=SECURITY_FINDINGS_SCHEMA
        )
        candidates = _dedupe([_security_finding(item) for item in (answer or {}).get("findings", [])])
        wf.log(f"{_plural(len(candidates), 'candidate')} after dedup")
        if not candidates:
            return {"findings": [], "candidates": 0}
        wf.phase("Filter")
        scores = await wf.parallel(
            [
                lambda candidate=candidate: wf.agent(
                    _filter_prompt(
                        {
                            **candidate,
                            "description": candidate["summary"],
                            "exploit_scenario": candidate["failure_scenario"],
                        },
                        scope,
                    ),
                    label=f"filter:{candidate['file']}:{candidate['line']}",
                    phase="Filter",
                    schema=FILTER_SCHEMA,
                )
                for candidate in candidates
            ]
        )
        kept = [
            {**candidate, "confidence": score["confidence"]}
            for candidate, score in zip(candidates, scores)
            if score is not None and score.get("confidence", 0) >= MIN_CONFIDENCE
        ]
        dropped = len(candidates) - len(kept)
        if dropped:
            wf.log(f"{dropped} candidate(s) below confidence {MIN_CONFIDENCE} dropped")
        return {"findings": kept, "candidates": len(candidates)}

    main.meta = {"name": "security-review", "description": "security review of the pending changes"}
    return main


def _rank_by_severity(findings: list[dict[str, Any]]) -> list[dict[str, Any]]:
    order = {severity: rank for rank, severity in enumerate(SEVERITIES)}
    return sorted(
        findings,
        key=lambda finding: (order.get(finding.get("severity") or "", len(order)), -(finding.get("confidence") or 0)),
    )


async def security_review(target: str = "", *, cwd: str | None = None, backend: Any = None) -> Review:
    """``/security-review``: Claude Code's security brief as one finder agent,
    then one false-positive filter agent per candidate (kept at confidence 8+)."""
    from rlm import workflow

    current, scope = _prepare("sec_", "Security review", target, cwd)
    if current.notices:
        current.save()
        return current
    outcome = await workflow.run(_security_script(scope), name="security-review", backend=backend)
    result = outcome.result or {}
    current = _finish(current, outcome, _rank_by_severity(result.get("findings", [])), result.get("candidates", 0))
    if current.status == "completed" and current.findings:
        current.next_steps = [
            "Report the findings above as they are; the user decides what to fix. If they ask you to fix "
            "some, call `code_review.report_outcomes(...)` after the fixes land."
        ]
    current.save()
    return current


def report_findings(
    findings: list[dict[str, Any]],
    *,
    title: str = "Review",
    target: str = "",
    cwd: str | None = None,
) -> Review:
    """Render findings a review produced as the ranked table (Claude Code's
    ``ReportFindings``): each finding a dict with ``file``, ``line`` and
    ``summary``, optionally ``failure_scenario``, ``category``, ``severity``,
    ``verdict`` and ``recommendation``. The review is saved, so
    ``report_outcomes`` works on it."""
    normalized = []
    for item in findings:
        finding = _normalize(item, str(item.get("category") or "correctness"))
        for key in ("verdict", "recommendation"):
            if item.get(key):
                finding[key] = str(item[key])
        if item.get("severity"):
            finding["severity"] = str(item["severity"]).upper()
        normalized.append(finding)
    ranked = (
        _rank_by_severity(normalized) if any(f.get("severity") for f in normalized) else _rank(normalized)
    )
    current = Review(
        review_id="rf_" + secrets.token_hex(6),
        level="reported",
        target=target,
        scope=target,
        cwd=cwd or os.getcwd(),
        title=title,
    )
    current.dropped = max(0, len(ranked) - MAX_FINDINGS_CAP)
    current.findings = [{**finding, "index": index} for index, finding in enumerate(ranked[:MAX_FINDINGS_CAP], 1)]
    current.save()
    return current
