#!/usr/bin/env python3
"""Plot a Dream-RSI experiment against its fixed-exploration control.

Reads one or more ``result.json`` files written by ``prime-agent dream experiment``
(schema ``prime-agent.dream.experiment/1``, ``core/dream/experiment.ts``) and
renders the paper's evidence figures for our fork:

    round_best.png   round-best points + cumulative-best step per arm vs round   (Fig 6a)
    compute.png      cumulative best vs cumulative discovery compute per arm     (Figs 3b/5)
    attempts.png     evaluated attempts per round per arm, policy changes marked (Fig 6b)
    headline.png     the multipliers against the "fixed" arm, or the literal words
                     "not reached" / "not comparable" when a multiplier is undefined
    report.html      the four figures with captions built from the result metadata

Nothing here is illustrative: every series is read from the result files. Several
files are treated as seeds of one experiment (same task, rounds, budget, objective
and arms, distinct seeds) and reduced to mean/min/max per round; a single file is
plotted as is. Files that disagree on any of those, the replay objective's
``beta1``/``beta2`` included, are refused with the differing values named: seeds
scored by different objectives are not one experiment.

The data layer (``load_results``, ``series``, ``headline``, ``--check``) is stdlib
only. matplotlib is imported inside ``render`` so ``--check`` works on any
interpreter. When matplotlib is missing for the running interpreter, the script
looks for a fallback interpreter (``$DREAM_PLOT_PYTHON``, then
``~/Documents/AISpecies/.venv/bin/python``) that has it, reports which one it is
using, and re-executes itself there; if none has it, it exits 3 with an
actionable message.

Compute axis (honesty rule): ``cumulativeProbes`` is discovery compute, the
paper's "agent calls": evaluated attempts (revealed non-root nodes) on every
path. Handler invocations (proposer, dreamer, guidance) and child tokens are
COST and are never mixed into that axis; the report prints them separately.

Field names: the reader takes the file's names (``probes``, ``cumulativeProbes``,
``handlerCalls``, ``totals``, ``policyScoreOnOwnPool``, ``probesToTarget``,
``callsMultiplier``, ``equalBudget``, ``bestAtBudget``, ``scoreMultiplier``,
``deltaBest``) and tolerates the equivalent ``attemptsEvaluated`` /
``cumulativeCalls`` / ``callsToTarget`` / ``bestAtEqualBudget`` / ``multipliers``
spelling of the same quantities.

Missing and undefined values: a multiplier the file leaves null (``not reached``,
``not comparable``) stays undefined all the way to the page; it is never clamped,
defaulted or averaged in. A headline whose reference arm did not run is no
headline (there is no control). A malformed sub-field (a number where an object
is expected, a string where a number is) falls back to the value recomputed from
the rounds, and ``--check`` says whether the file and the recomputation agree.

Direction (honesty rule for the words): a ratio at or above 1 reads ``1.11x fewer
calls`` / ``1.03x higher score``; a ratio below 1 is never written as ``0.83x
fewer``. It is written the right way round, as its inverse with the direction
spelled out: ``1.20x MORE calls (72 vs 60)``, ``1.03x LOWER score``. The tone
(ok/bad) still follows the raw ratio, and the operands are always printed in the
same order (arm vs reference). An aggregate below 1 prints the inverse of the
median ratio.

Across seeds (honesty rule for the aggregate lines): the success colour needs the
ratio defined in every seed. A ratio defined in one seed is that seed's ratio,
never a "median"; a ratio defined in k of N seeds says "k/N" and is warn-toned,
except that a partial median below 1 is bad, because the seeds where it is
undefined never reached T (calls) or had nothing inside B (score) and cannot
rescue it.
"""

from __future__ import annotations

import argparse
import base64
import html
import json
import os
import statistics
import subprocess
import sys
import textwrap
from pathlib import Path
from typing import TypedDict

EXPECTED_SCHEMA = "prime-agent.dream.experiment/1"
FALLBACK_PYTHON = "~/Documents/AISpecies/.venv/bin/python"
REEXEC_ENV = "DREAM_PLOT_REEXEC"
PYTHON_ENV = "DREAM_PLOT_PYTHON"
EPS = 1e-9

EXIT_OK = 0
EXIT_DATA = 1
EXIT_USAGE = 2
EXIT_NO_MATPLOTLIB = 3

# Dashboard surface and ink tokens (make_dashboard.py).
BG, FG, GRID, MUTED = "#12151a", "#e8eaed", "#2a2f38", "#9aa3ad"
# Status colors: reserved for guide lines / verdict words, never for a series.
OK, BAD, WARN, ACC = "#4caf82", "#d9534f", "#d9a441", "#5b9bd5"

# Categorical slots stepped for a dark surface (validated: adjacent-pair CVD
# Delta E 8.4, normal-vision 19.8, all >= 3:1 on BG). Color follows the arm,
# never its rank: an arm keeps its color whichever arms are present.
ARM_ORDER = ("dream", "fixed", "dream-guided", "fixed-guided")
ARM_COLORS = {
    "dream": "#3987e5",
    "fixed": "#d95926",
    "dream-guided": "#199e70",
    "fixed-guided": "#c98500",
}
ARM_LINESTYLES = {"dream": "-", "fixed": "--", "dream-guided": "-.", "fixed-guided": ":"}
ARM_MARKERS = {"dream": "o", "fixed": "s", "dream-guided": "D", "fixed-guided": "^"}
EXTRA_COLORS = ("#d55181", "#9085e9", "#e66767", "#008300")

REFERENCE_ARM = "fixed"
ABLATION_PAIRS = (("dream", "dream-guided"), ("fixed", "fixed-guided"))


class ResultError(ValueError):
    """A result file cannot be used as experiment evidence."""


class SchemaError(ResultError):
    """The file does not carry the expected experiment schema."""


# ------------------------------------------------------------------------- shapes
# The internal shapes. Every optional value is spelled ``| None`` here so that a
# consumer has to narrow it; nothing downstream assumes a field is present.


class RoundRow(TypedDict):
    round: int
    treeId: str
    policyId: str
    roundBest: float
    cumulativeBest: float
    probes: int
    cumulativeProbes: int
    handlerCalls: int
    cumulativeHandlerCalls: int
    decisionRounds: int
    tokens: int
    cumulativeTokens: int
    poolSize: int
    policyScoreOnReplay: float | None
    dreaming: dict[str, object] | None


class Arm(TypedDict):
    arm: str
    fixedPolicy: bool
    guided: bool
    proposer: str
    dreamer: str
    model: str | None
    storeDir: str
    runId: str
    initialPolicyId: str
    finalPolicyId: str
    initialPolicyScore: float | None
    finalPolicyScore: float | None
    improved: bool
    policyChanges: int
    finalBest: float
    totalProbes: int
    totalHandlerCalls: int
    totalTokens: int
    rounds: list[RoundRow]


class AblationRow(TypedDict):
    unguidedArm: str
    guidedArm: str
    guidedMinusUnguidedFinalBest: float
    unguidedProbesToTarget: int | None
    guidedProbesToTarget: int | None


class Headline(TypedDict):
    reference: str
    target: float
    equalBudget: int
    budgetIsReferenceTotal: bool
    probesToTarget: dict[str, int | None]
    callsMultiplier: dict[str, float | None]
    bestAtBudget: dict[str, float | None]
    scoreMultiplier: dict[str, float | None]
    deltaBest: dict[str, float | None]
    deltaAtBudget: dict[str, float | None]
    ablation: list[AblationRow] | None


class Result(TypedDict):
    path: str
    schema: str
    experimentId: str
    task: str
    n: int | None
    seed: int | str
    rounds: int
    budget: dict[str, object]
    objective: dict[str, object] | None
    initialPolicyId: str
    proposer: str
    dreamer: str
    model: str | None
    sharedInitialRollout: bool | None
    createdTs: float | None
    arms: list[Arm]
    headline: Headline | None
    notes: list[str]


class SeedHeadline(TypedDict):
    seed: int | str
    experimentId: str
    headline: Headline | None


class ArmAggregate(TypedDict):
    callsMultiplierMedian: float | None
    scoreMultiplierMedian: float | None
    deltaBestMedian: float | None
    callsMultiplierDefined: int
    scoreMultiplierDefined: int
    reached: int
    comparable: int
    n: int


class AblationSummary(TypedDict):
    deltas: list[float]
    median: float | None


class HeadlineSummary(TypedDict):
    reference: str | None
    n: int
    perSeed: list[SeedHeadline]
    aggregate: dict[str, ArmAggregate]
    ablation: dict[str, AblationSummary]


# --------------------------------------------------------------------------- data


def _num(value: object) -> float | None:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return float(value)


def _num_or(value: object, default: float) -> float:
    parsed = _num(value)
    return default if parsed is None else parsed


def _int(value: object, default: int = 0) -> int:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return default
    return int(value)


def _dict(value: object) -> dict[str, object]:
    """The value when it is an object, else an empty one: a malformed sub-field never raises."""
    if not isinstance(value, dict):
        return {}
    return {str(k): v for k, v in value.items()}


def _get(mapping: object, *keys: str, default: object = None) -> object:
    if not isinstance(mapping, dict):
        return default
    for key in keys:
        if key in mapping and mapping[key] is not None:
            return mapping[key]
    return default


def _diff(a: float | None, b: float | None) -> float | None:
    return None if a is None or b is None else a - b


def _ratio(numerator: float | None, denominator: float | None) -> float | None:
    """numerator / denominator, undefined (None) when either is missing or the denominator is 0."""
    if numerator is None or denominator is None or denominator == 0:
        return None
    return numerator / denominator


def normalize_round(row: object, index: int, previous: RoundRow | None) -> RoundRow:
    """One round record into the internal shape (see the module docstring for the accepted names)."""
    if not isinstance(row, dict):
        raise ResultError(f"round {index + 1} is not an object")
    round_no = _int(_get(row, "round"), index + 1)
    probes = _int(_get(row, "probes", "attemptsEvaluated", "agentCalls"))
    handler = _get(row, "handlerCalls")
    if isinstance(handler, dict):
        handler_calls = sum(_int(handler.get(role)) for role in ("proposer", "dreamer", "guidance"))
    else:
        handler_calls = _int(_get(row, "overheadCalls"))
    round_best = _num_or(_get(row, "roundBest"), 0.0)
    cumulative_best = _num_or(
        _get(row, "cumulativeBest"),
        round_best if previous is None else max(previous["cumulativeBest"], round_best),
    )
    prev_probes = previous["cumulativeProbes"] if previous else 0
    cumulative_probes = _int(_get(row, "cumulativeProbes", "cumulativeCalls"), prev_probes + probes)
    prev_handler = previous["cumulativeHandlerCalls"] if previous else 0
    cumulative_handler = _int(_get(row, "cumulativeHandlerCalls"), prev_handler + handler_calls)
    tokens_raw = _get(row, "tokens", default=0)
    tokens = sum(_int(v) for v in tokens_raw.values()) if isinstance(tokens_raw, dict) else _int(tokens_raw)
    prev_tokens = previous["cumulativeTokens"] if previous else 0
    cumulative_tokens = _int(_get(row, "cumulativeTokens"), prev_tokens + tokens)
    dreaming_raw = _get(row, "dreaming")
    dreaming = _dict(dreaming_raw) if isinstance(dreaming_raw, dict) else None
    replay = _num(_get(row, "policyScoreOnReplay"))
    if replay is None and dreaming is not None:
        replay = _num(dreaming.get("chosenScore"))
    return {
        "round": round_no,
        "treeId": str(_get(row, "treeId", default="")),
        "policyId": str(_get(row, "policyId", default="")),
        "roundBest": round_best,
        "cumulativeBest": cumulative_best,
        "probes": probes,
        "cumulativeProbes": cumulative_probes,
        "handlerCalls": handler_calls,
        "cumulativeHandlerCalls": cumulative_handler,
        "decisionRounds": _int(_get(row, "decisionRounds")),
        "tokens": tokens,
        "cumulativeTokens": cumulative_tokens,
        "poolSize": _int(_get(row, "poolSize"), round_no - 1),
        "policyScoreOnReplay": replay,
        "dreaming": dreaming,
    }


def normalize_arm(raw: object, index: int) -> Arm:
    if not isinstance(raw, dict):
        raise ResultError(f"arm {index} is not an object")
    name = raw.get("arm")
    if not isinstance(name, str):
        raise ResultError(f"arm {index} has no name")
    rounds_raw = raw.get("rounds")
    if not isinstance(rounds_raw, list) or not rounds_raw:
        raise ResultError(f"arm {name} has no rounds")
    rounds: list[RoundRow] = []
    for i, row in enumerate(rounds_raw):
        rounds.append(normalize_round(row, i, rounds[-1] if rounds else None))
    mode = raw.get("mode")
    if isinstance(mode, dict):
        proposer = _get(mode, "proposer", default="local")
        dreamer = _get(mode, "dreamer", default="local")
        model = mode.get("model")
    else:
        proposer = _get(raw, "proposer", default=mode if isinstance(mode, str) else "local")
        dreamer = _get(raw, "dreamer", default=mode if isinstance(mode, str) else "local")
        model = raw.get("model")
    totals = _dict(raw.get("totals"))
    own_pool = _dict(raw.get("policyScoreOnOwnPool"))
    last = rounds[-1]
    policy_changes_raw = raw.get("policyChanges")
    if isinstance(policy_changes_raw, int) and not isinstance(policy_changes_raw, bool):
        policy_changes = policy_changes_raw
    else:
        policy_changes = sum(1 for a, b in zip(rounds, rounds[1:], strict=False) if a["policyId"] != b["policyId"])
    initial_score = _num(own_pool.get("initial"))
    if initial_score is None:
        initial_score = _num(_get(raw, "initialPolicyScore"))
    final_score = _num(own_pool.get("final"))
    if final_score is None:
        final_score = _num(_get(raw, "finalPolicyScore"))
    improved_raw = raw.get("improved")
    if isinstance(improved_raw, bool):
        improved = improved_raw
    else:
        improved = initial_score is not None and final_score is not None and final_score > initial_score
    return {
        "arm": name,
        "fixedPolicy": bool(_get(raw, "fixedPolicy", default=name.startswith("fixed"))),
        "guided": bool(_get(raw, "guided", default=name.endswith("-guided"))),
        "proposer": str(proposer),
        "dreamer": str(dreamer),
        "model": model if isinstance(model, str) else None,
        "storeDir": str(_get(raw, "storeDir", default="")),
        "runId": str(_get(raw, "runId", default="")),
        "initialPolicyId": str(_get(raw, "initialPolicyId", default="")),
        "finalPolicyId": str(_get(raw, "finalPolicyId", default="")),
        "initialPolicyScore": initial_score,
        "finalPolicyScore": final_score,
        "improved": improved,
        "policyChanges": policy_changes,
        "finalBest": _num_or(totals.get("finalBest"), _num_or(_get(raw, "finalBest"), last["cumulativeBest"])),
        "totalProbes": _int(
            totals.get("probes"), _int(_get(raw, "totalCalls", "totalAttempts"), last["cumulativeProbes"])
        ),
        "totalHandlerCalls": _int(
            totals.get("handlerCalls"), _int(_get(raw, "totalOverheadCalls"), last["cumulativeHandlerCalls"])
        ),
        "totalTokens": _int(totals.get("tokens"), _int(_get(raw, "totalTokens"), last["cumulativeTokens"])),
        "rounds": rounds,
    }


def compute_headline(
    arms: list[Arm], reference: str = REFERENCE_ARM, target: float | None = None, budget: int | None = None
) -> Headline | None:
    """The headline definitions of ``core/dream/experiment.ts``, recomputed from the rounds.

    T = the reference arm's final best; B = min over arms of total probes (the
    equal budget). A file may carry its own T and B; pass them to verify its
    arithmetic on its own terms. None when the reference arm did not run: there
    is no control, so no multiplier is defined.
    probesToTarget(a) = cumulativeProbes at the FIRST round with cumulativeBest >= T - EPS, else None.
    callsMultiplier(a) = probesToTarget(ref) / probesToTarget(a); None when either is None or a's is 0.
    bestAtBudget(a) = cumulativeBest at the last round with cumulativeProbes <= B, else None.
    scoreMultiplier(a) = bestAtBudget(a) / bestAtBudget(ref); None when either is None or ref's is 0.
    deltaBest(a) = finalBest(a) - T. Nothing is clamped.
    """
    by_name = {a["arm"]: a for a in arms}
    ref = by_name.get(reference)
    if ref is None:
        return None
    target_value = ref["finalBest"] if target is None else target
    budget_value = min(a["totalProbes"] for a in arms) if budget is None else budget
    probes_to_target: dict[str, int | None] = {}
    best_at_budget: dict[str, float | None] = {}
    delta_best: dict[str, float | None] = {}
    for arm in arms:
        reached = [r["cumulativeProbes"] for r in arm["rounds"] if r["cumulativeBest"] >= target_value - EPS]
        probes_to_target[arm["arm"]] = min(reached) if reached else None
        within = [r["cumulativeBest"] for r in arm["rounds"] if r["cumulativeProbes"] <= budget_value]
        best_at_budget[arm["arm"]] = within[-1] if within else None
        delta_best[arm["arm"]] = arm["finalBest"] - target_value
    ref_probes = probes_to_target[reference]
    ref_best = best_at_budget[reference]
    calls_multiplier: dict[str, float | None] = {}
    score_multiplier: dict[str, float | None] = {}
    delta_at_budget: dict[str, float | None] = {}
    for arm in arms:
        name = arm["arm"]
        p = probes_to_target[name]
        b = best_at_budget[name]
        calls_multiplier[name] = _ratio(ref_probes, p)
        score_multiplier[name] = _ratio(b, ref_best)
        delta_at_budget[name] = _diff(b, ref_best)
    return {
        "reference": reference,
        "target": target_value,
        "equalBudget": budget_value,
        "budgetIsReferenceTotal": budget_value == ref["totalProbes"],
        "probesToTarget": probes_to_target,
        "callsMultiplier": calls_multiplier,
        "bestAtBudget": best_at_budget,
        "scoreMultiplier": score_multiplier,
        "deltaBest": delta_best,
        "deltaAtBudget": delta_at_budget,
        "ablation": ablation_rows(by_name, probes_to_target),
    }


def ablation_rows(by_name: dict[str, Arm], probes_to_target: dict[str, int | None]) -> list[AblationRow] | None:
    """Guided minus unguided final best for every (unguided, guided) pair that ran."""
    rows: list[AblationRow] = []
    for unguided, guided in ABLATION_PAIRS:
        if unguided in by_name and guided in by_name:
            rows.append(
                {
                    "unguidedArm": unguided,
                    "guidedArm": guided,
                    "guidedMinusUnguidedFinalBest": by_name[guided]["finalBest"] - by_name[unguided]["finalBest"],
                    "unguidedProbesToTarget": probes_to_target.get(unguided),
                    "guidedProbesToTarget": probes_to_target.get(guided),
                }
            )
    return rows or None


def normalize_headline(raw: object, arms: list[Arm]) -> Headline | None:
    """The file's headline into the internal shape; recomputed when the file has none.

    None when the reference arm the file names did not run: a headline without its
    control is not evidence. A sub-field that is not the object or number it should
    be is treated as absent, so a value the file omits or leaves null stays
    undefined (``not reached`` / ``not comparable``) and never raises.
    """
    if not isinstance(raw, dict):
        return compute_headline(arms)
    names = [a["arm"] for a in arms]
    by_name = {a["arm"]: a for a in arms}
    reference_raw = raw.get("reference")
    reference = reference_raw if isinstance(reference_raw, str) else REFERENCE_ARM
    ref_arm = by_name.get(reference)
    if ref_arm is None:
        return None
    target = _num_or(raw.get("target"), ref_arm["finalBest"])
    budget = _int(_get(raw, "equalBudget", "budget"), min(a["totalProbes"] for a in arms))
    computed = compute_headline(arms, reference, target, budget)
    probes = _dict(_get(raw, "probesToTarget", "callsToTarget"))
    best = _dict(_get(raw, "bestAtBudget", "bestAtEqualBudget"))
    delta_raw = _dict(raw.get("deltaBest"))
    mult_raw = _dict(raw.get("multipliers"))
    calls_raw = _dict(raw.get("callsMultiplier"))
    score_raw = _dict(raw.get("scoreMultiplier"))
    calls_multiplier: dict[str, float | None] = {}
    score_multiplier: dict[str, float | None] = {}
    for name in names:
        per_arm = mult_raw.get(name)
        if isinstance(per_arm, dict):
            calls_multiplier[name] = _num(_get(per_arm, "callsFewer"))
            score_multiplier[name] = _num(_get(per_arm, "scoreHigher"))
        else:
            calls_multiplier[name] = _num(calls_raw.get(name))
            score_multiplier[name] = _num(score_raw.get(name))
    delta_best: dict[str, float | None] = {}
    for name in names:
        delta_best[name] = _num_or(delta_raw.get(name), by_name[name]["finalBest"] - target)
    probes_to_target: dict[str, int | None] = {}
    for name in names:
        value = _num(probes.get(name))
        probes_to_target[name] = None if value is None else int(value)
    best_at_budget: dict[str, float | None] = {n: _num(best.get(n)) for n in names}
    ref_best = best_at_budget.get(reference)
    return {
        "reference": reference,
        "target": target,
        "equalBudget": budget,
        "budgetIsReferenceTotal": budget == ref_arm["totalProbes"],
        "probesToTarget": probes_to_target,
        "callsMultiplier": calls_multiplier,
        "bestAtBudget": best_at_budget,
        "scoreMultiplier": score_multiplier,
        "deltaBest": delta_best,
        "deltaAtBudget": {n: _diff(best_at_budget[n], ref_best) for n in names},
        "ablation": computed["ablation"] if computed is not None else None,
    }


def load_result(path_arg: str | os.PathLike[str]) -> Result:
    path = Path(path_arg)
    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
    except OSError as exc:
        raise ResultError(f"{path}: cannot read: {exc}") from exc
    except json.JSONDecodeError as exc:
        raise ResultError(f"{path}: not JSON: {exc}") from exc
    if not isinstance(raw, dict):
        raise SchemaError(f"{path}: expected an object with schema {EXPECTED_SCHEMA!r}")
    doc = _dict(raw)
    schema = doc.get("schema")
    if schema != EXPECTED_SCHEMA:
        raise SchemaError(f"{path}: schema {schema!r} is not the expected {EXPECTED_SCHEMA!r}")
    arms_raw = doc.get("arms")
    if not isinstance(arms_raw, list) or not arms_raw:
        raise ResultError(f"{path}: no arms")
    arms = [normalize_arm(a, i) for i, a in enumerate(arms_raw)]
    rounds = _int(doc.get("rounds"), max(len(a["rounds"]) for a in arms))
    for arm in arms:
        if len(arm["rounds"]) != rounds:
            raise ResultError(f"{path}: arm {arm['arm']} has {len(arm['rounds'])} rounds, file says {rounds}")
    budget = _dict(doc.get("budget"))
    notes_raw = doc.get("notes")
    notes = notes_raw if isinstance(notes_raw, list) else []
    seed_raw = doc.get("seed")
    seed: int | str = seed_raw if isinstance(seed_raw, (int, str)) and not isinstance(seed_raw, bool) else "?"
    n_raw = doc.get("n")
    n = n_raw if isinstance(n_raw, int) and not isinstance(n_raw, bool) else None
    objective_raw = doc.get("objective")
    objective = _dict(objective_raw) if isinstance(objective_raw, dict) else None
    shared_raw = doc.get("sharedInitialRollout")
    shared = shared_raw if isinstance(shared_raw, bool) else None
    return {
        "path": str(path),
        "schema": EXPECTED_SCHEMA,
        "experimentId": str(_get(doc, "experimentId", default=path.parent.name)),
        "task": str(_get(doc, "task", default="?")),
        "n": n,
        "seed": seed,
        "rounds": rounds,
        "budget": {k: budget.get(k) for k in ("workers", "k1", "k2", "dreams")},
        "objective": objective,
        "initialPolicyId": str(_get(doc, "initialPolicyId", default=arms[0]["initialPolicyId"])),
        "proposer": "/".join(sorted({a["proposer"] for a in arms})),
        "dreamer": "/".join(sorted({a["dreamer"] for a in arms})),
        "model": next((a["model"] for a in arms if a["model"]), None),
        "sharedInitialRollout": shared,
        "createdTs": _num(doc.get("createdTs")),
        "arms": arms,
        "headline": normalize_headline(doc.get("headline"), arms),
        "notes": [str(note) for note in notes],
    }


OBJECTIVE_FIELDS = ("beta1", "beta2")


def objective_key(objective: dict[str, object] | None) -> tuple[float | None, ...] | None:
    """The replay objective as the tuple that must agree across seeds; None when the file recorded none."""
    if objective is None:
        return None
    return tuple(_num(objective.get(field)) for field in OBJECTIVE_FIELDS)


def objective_text(objective: dict[str, object] | None) -> str:
    """`beta1=0.05 beta2=0.05`, or `none recorded` when the file carries no objective."""
    key = objective_key(objective)
    if key is None:
        return "none recorded"
    return " ".join(f"{name}={'?' if v is None else f'{v:g}'}" for name, v in zip(OBJECTIVE_FIELDS, key, strict=True))


def pool_key(r: Result) -> dict[str, tuple[object, str]]:
    """What must agree across the files of one experiment: field -> (comparable value, printable value)."""
    budget = json.dumps(r["budget"], sort_keys=True)
    arms = tuple(a["arm"] for a in r["arms"])
    return {
        "task": (r["task"], r["task"]),
        "rounds": (r["rounds"], str(r["rounds"])),
        "budget": (budget, budget),
        "objective": (objective_key(r["objective"]), objective_text(r["objective"])),
        "arms": (arms, ",".join(arms)),
    }


def load_results(paths: list[str]) -> list[Result]:
    """The files as seeds of one experiment; refused, naming the differing values, when they are not."""
    if not paths:
        raise ResultError("no result files given")
    results = [load_result(p) for p in paths]
    first = results[0]
    first_key = pool_key(first)
    for r in results[1:]:
        mine = pool_key(r)
        differing = [field for field in first_key if mine[field][0] != first_key[field][0]]
        if differing:
            detail = "; ".join(f"{field} {mine[field][1]} vs {first_key[field][1]}" for field in differing)
            what = f"{'/'.join(differing)} differ from {first['path']} ({detail})"
            raise ResultError(f"{r['path']}: {what}; plot one experiment at a time")
    seeds = [r["seed"] for r in results]
    if len(set(map(str, seeds))) != len(seeds):
        raise ResultError(f"duplicate seeds across result files: {seeds}")
    return results


def _stats(values: list[float | None]) -> dict[str, float | None]:
    clean = _defined(values)
    if not clean:
        return {"mean": None, "min": None, "max": None, "n": 0}
    return {"mean": statistics.fmean(clean), "min": min(clean), "max": max(clean), "n": len(clean)}


SERIES_FIELDS = (
    "roundBest",
    "cumulativeBest",
    "probes",
    "cumulativeProbes",
    "handlerCalls",
    "cumulativeHandlerCalls",
    "tokens",
    "cumulativeTokens",
)


def arm_names(results: list[Result]) -> list[str]:
    return [a["arm"] for a in results[0]["arms"]]


def arm_of(result: Result, name: str) -> Arm:
    return next(a for a in result["arms"] if a["arm"] == name)


def _field(row: RoundRow, field: str) -> float | None:
    value = row.get(field)
    return _num(value)


def series(results: list[Result]):
    """Per arm, per round: each field across seeds as per-seed lists plus mean/min/max."""
    out = {}
    rounds = results[0]["rounds"]
    for name in arm_names(results):
        per_seed = [arm_of(r, name)["rounds"] for r in results]
        fields = {}
        for field in SERIES_FIELDS:
            per_seed_values = [[_field(row, field) for row in rows] for rows in per_seed]
            by_round = [_stats([vals[i] for vals in per_seed_values]) for i in range(rounds)]
            fields[field] = {
                "perSeed": per_seed_values,
                "mean": [s["mean"] for s in by_round],
                "min": [s["min"] for s in by_round],
                "max": [s["max"] for s in by_round],
            }
        policy_changes = []
        for i in range(rounds):
            changed = 0
            for rows in per_seed:
                if i > 0 and rows[i]["policyId"] != rows[i - 1]["policyId"]:
                    changed += 1
            policy_changes.append(changed)
        out[name] = {
            "rounds": list(range(1, rounds + 1)),
            "seeds": [r["seed"] for r in results],
            "policyChanges": policy_changes,
            "finalBest": [arm_of(r, name)["finalBest"] for r in results],
            "totalProbes": [arm_of(r, name)["totalProbes"] for r in results],
            "totalHandlerCalls": [arm_of(r, name)["totalHandlerCalls"] for r in results],
            "totalTokens": [arm_of(r, name)["totalTokens"] for r in results],
            **fields,
        }
    return out


def _defined(values: list[float | None]) -> list[float]:
    return [v for v in values if v is not None]


def _median(values: list[float | None]) -> float | None:
    clean = _defined(values)
    return statistics.median(clean) if clean else None


def headline(results: list[Result]) -> HeadlineSummary:
    """Per-seed headline rows from the files plus median multipliers and reach counts.

    ``callsMultiplierDefined`` / ``scoreMultiplierDefined`` count the seeds whose
    ratio is defined; the medians are over those seeds only, and ``aggregate_text``
    says so when that is fewer than all of them.
    """
    per_seed: list[SeedHeadline] = [
        {"seed": r["seed"], "experimentId": r["experimentId"], "headline": r["headline"]} for r in results
    ]
    heads = [h for h in (p["headline"] for p in per_seed) if h is not None]
    names = arm_names(results)
    aggregate: dict[str, ArmAggregate] = {}
    for name in names:
        calls = [h["callsMultiplier"].get(name) for h in heads]
        scores = [h["scoreMultiplier"].get(name) for h in heads]
        aggregate[name] = {
            "callsMultiplierMedian": _median(calls),
            "scoreMultiplierMedian": _median(scores),
            "deltaBestMedian": _median([h["deltaBest"].get(name) for h in heads]),
            "callsMultiplierDefined": len(_defined(calls)),
            "scoreMultiplierDefined": len(_defined(scores)),
            "reached": sum(1 for h in heads if h["probesToTarget"].get(name) is not None),
            "comparable": sum(1 for h in heads if h["bestAtBudget"].get(name) is not None),
            "n": len(heads),
        }
    ablation: dict[str, list[float]] = {}
    for h in heads:
        for row in h["ablation"] or []:
            key = f"{row['unguidedArm']} vs {row['guidedArm']}"
            ablation.setdefault(key, []).append(row["guidedMinusUnguidedFinalBest"])
    return {
        "reference": heads[0]["reference"] if heads else None,
        "n": len(results),
        "perSeed": per_seed,
        "aggregate": aggregate,
        "ablation": {k: {"deltas": v, "median": statistics.median(v)} for k, v in ablation.items()},
    }


def fmt(value: object, digits: int = 4) -> str:
    if value is None:
        return "-"
    if isinstance(value, float):
        if value == int(value) and abs(value) < 1e12:
            return str(int(value))
        return f"{value:.{digits}f}"
    return str(value)


def ratio_fmt(value: float) -> str:
    """A ratio to 2 decimals, or to 4 when 2 would print a ratio that is not 1 as `1.00`."""
    text = f"{value:.2f}"
    if text == "1.00" and abs(value - 1) > EPS:
        return f"{value:.4f}"
    return text


def ratio_words(kind: str, value: float) -> str:
    """The ratio the right way round, never `0.83x fewer`.

    At or above 1: `1.11x fewer calls` / `1.03x higher score`. Below 1: the inverse
    with the direction spelled out, `1.20x MORE calls` / `1.03x LOWER score`, so a
    reader never has to invert `0.83x fewer` in their head. A ratio that is not
    positive has no inverse and is printed as the bare ratio.
    """
    noun = "calls" if kind == "calls" else "score"
    if value <= 0:
        return f"{noun} ratio {ratio_fmt(value)} (not positive)"
    if value < 1 - EPS:
        return f"{ratio_fmt(1 / value)}x {'MORE' if kind == 'calls' else 'LOWER'} {noun}"
    return f"{ratio_fmt(value)}x {'fewer' if kind == 'calls' else 'higher'} {noun}"


def multiplier_text(kind: str, head: Headline, arm: str) -> str:
    """`1.11x fewer calls (27 vs 30)`, `1.20x MORE calls (72 vs 60)`, or the literal undefined words.

    The operands are always (arm vs reference), whichever way the ratio reads.
    """
    ref = head["reference"]
    if kind == "calls":
        value = head["callsMultiplier"].get(arm)
        mine, theirs = head["probesToTarget"].get(arm), head["probesToTarget"].get(ref)
        if value is None:
            return "not reached" if mine is None else "not comparable"
        return f"{ratio_words('calls', value)} ({fmt(mine)} vs {fmt(theirs)})"
    value = head["scoreMultiplier"].get(arm)
    mine, theirs = head["bestAtBudget"].get(arm), head["bestAtBudget"].get(ref)
    if value is None:
        if mine is None:
            return "not comparable"
        return f"ratio undefined (reference best {fmt(theirs)}); delta at budget {fmt(head['deltaAtBudget'].get(arm))}"
    return f"{ratio_words('score', value)} at budget {fmt(head['equalBudget'])} ({fmt(mine)} vs {fmt(theirs)})"


def delta_text(head: Headline, arm: str) -> str:
    d = head["deltaBest"].get(arm)
    if d is None:
        return "delta final best undefined"
    return f"{d:+.4f} delta final best vs {head['reference']} (T = {fmt(head['target'])})"


def budget_text(head: Headline) -> str:
    return f"{head['reference']} total" if head["budgetIsReferenceTotal"] else "smallest arm total"


def aggregate_text(kind: str, name: str, agg: ArmAggregate) -> tuple[str, str]:
    """(text, tone) for one arm's multiplier across seeds; the honesty rule of the module docstring.

    The success colour needs the ratio defined in every seed. Defined in one seed:
    that seed's ratio, never a median. Defined in k of N: "k/N", warn-toned, or
    bad when the partial median is below 1 (the undefined seeds never reached T,
    or had nothing inside B, so they cannot rescue it). Defined nowhere: the words.
    A median below 1 reads the right way round (``ratio_words``): the number printed
    is the inverse of the median ratio, and the tone still follows the median itself.
    """
    n = agg["n"]
    if kind == "calls":
        value, defined = agg["callsMultiplierMedian"], agg["callsMultiplierDefined"]
        count = f"reached T in {agg['reached']}/{n} seeds"
    else:
        value, defined = agg["scoreMultiplierMedian"], agg["scoreMultiplierDefined"]
        count = f"comparable at B in {agg['comparable']}/{n} seeds"
    if value is None or defined == 0:
        return f"{name}: {count}; no {kind} ratio is defined", "warn"
    what = ratio_words(kind, value) + ("" if kind == "calls" else " at B")
    if defined == n:
        if n == 1:
            return f"{name}: {what} (one seed); {count}", ratio_tone(value)
        return f"{name}: median {what}; {count}", ratio_tone(value)
    tone = "bad" if value < 1 - EPS else "warn"
    if defined == 1:
        return f"{name}: {count}; single-seed ratio {what} (not a median)", tone
    return f"{name}: {count}; median of the {defined} defined ratios {what}", tone


def check_tables(results: list[Result]) -> str:
    """Plain-text reduction of the result files: what --check prints."""
    ser = series(results)
    head = headline(results)
    first = results[0]
    lines: list[str] = []
    b = first["budget"]
    lines.append(
        f"experiment task={first['task']} n={first['n']} rounds={first['rounds']} seeds={[r['seed'] for r in results]} "
        f"budget W={b['workers']} k1={b['k1']} k2={b['k2']} dreams={b['dreams']} "
        f"objective {objective_text(first['objective'])} arms={arm_names(results)} "
        f"proposer={first['proposer']} dreamer={first['dreamer']} model={first['model']}"
    )
    for name, s in ser.items():
        lines.append(f"arm {name}")
        lines.append(
            "  round | best(mean)   | cum best(mean) | probes(mean) | cum probes(mean) | handler calls | tokens | "
            "policy changed (seeds)"
        )
        for i, rnd in enumerate(s["rounds"]):
            lines.append(
                f"  {rnd:>5} | {fmt(s['roundBest']['mean'][i]):>12} | {fmt(s['cumulativeBest']['mean'][i]):>14} | "
                f"{fmt(s['probes']['mean'][i]):>12} | {fmt(s['cumulativeProbes']['mean'][i]):>16} | "
                f"{fmt(s['handlerCalls']['mean'][i]):>13} | {fmt(s['tokens']['mean'][i]):>6} | "
                f"{s['policyChanges'][i]}/{len(s['seeds'])}"
            )
    lines.append("headline")
    if head["reference"] is None:
        lines.append("  no reference arm ran: no control, no multipliers")
    for p in head["perSeed"]:
        h = p["headline"]
        if h is None:
            lines.append(f"  seed {p['seed']}: no headline (no {REFERENCE_ARM} arm)")
            continue
        lines.append(
            f"  seed {p['seed']}: target T={fmt(h['target'])} equal budget B={fmt(h['equalBudget'])} ({budget_text(h)})"
        )
        for name in arm_names(results):
            lines.append(
                f"    {name:>13}: {multiplier_text('calls', h, name)}; {multiplier_text('score', h, name)}; "
                f"{delta_text(h, name)}"
            )
    if head["reference"] is not None:
        lines.append(f"  across {head['n']} seed(s)")
        for name, agg in head["aggregate"].items():
            calls_text, _calls_tone = aggregate_text("calls", name, agg)
            score_text, _score_tone = aggregate_text("score", name, agg)
            lines.append(f"    {calls_text}")
            lines.append(f"    {score_text}")
    for key, row in head["ablation"].items():
        lines.append(
            f"  ablation {key}: guided minus unguided final best = {[fmt(d) for d in row['deltas']]} "
            f"(median {fmt(row['median'])})"
        )
    for r in results:
        h = r["headline"]
        if h is None:
            continue
        recomputed = compute_headline(r["arms"], h["reference"], h["target"], h["equalBudget"])
        if recomputed is None:
            continue
        agree = all(
            _close(recomputed["callsMultiplier"].get(a), h["callsMultiplier"].get(a))
            and _close(recomputed["scoreMultiplier"].get(a), h["scoreMultiplier"].get(a))
            and _close(recomputed["deltaBest"].get(a), h["deltaBest"].get(a))
            for a in arm_names(results)
        )
        lines.append(
            f"  seed {r['seed']}: headline recomputed from the rounds (file's T and B) agrees with the file: "
            f"{'yes' if agree else 'NO'}"
        )
    for note in first["notes"]:
        lines.append(f"note: {note}")
    return "\n".join(lines)


def _close(a: float | None, b: float | None) -> bool:
    if a is None or b is None:
        return a is None and b is None
    return abs(a - b) <= 1e-6 * max(1.0, abs(a), abs(b))


# ------------------------------------------------------------------------ render


def arm_color(name, index):
    return ARM_COLORS.get(name, EXTRA_COLORS[index % len(EXTRA_COLORS)])


def render(results, out_dir):
    """Write the four PNGs and report.html into out_dir; returns their paths."""
    import matplotlib

    matplotlib.use("Agg")
    import matplotlib.pyplot as plt

    out = Path(out_dir)
    out.mkdir(parents=True, exist_ok=True)
    ser = series(results)
    head = headline(results)
    first = results[0]
    names = arm_names(results)
    n_seeds = len(results)
    rounds = first["rounds"]
    short = rounds < 3

    def style(ax, title, xl="", yl=""):
        ax.set_facecolor(BG)
        ax.set_title(title, color=FG, fontsize=11, pad=8, loc="left")
        ax.set_xlabel(xl, color=FG, fontsize=9)
        ax.set_ylabel(yl, color=FG, fontsize=9)
        ax.tick_params(colors=FG, labelsize=8)
        for s in ax.spines.values():
            s.set_color(GRID)
        ax.grid(True, color=GRID, lw=0.6, alpha=0.7)

    def legend(ax, loc="best"):
        ax.legend(facecolor=BG, edgecolor=GRID, labelcolor=FG, fontsize=8, loc=loc)

    def subtitle(fig, lines):
        y = 0.905
        for line in lines:
            for wrapped in textwrap.wrap(line, 125) or [""]:
                fig.text(0.07, y, wrapped, color=MUTED, fontsize=8.5, ha="left")
                y -= 0.032

    def new_fig(title, sub_lines):
        fig = plt.figure(figsize=(9, 5.8), facecolor=BG)
        fig.suptitle(title, color=FG, fontsize=13, x=0.07, ha="left", y=0.965)
        subtitle(fig, sub_lines)
        ax = fig.add_axes((0.09, 0.11, 0.86, 0.68))
        return fig, ax

    seeds_text = f"seed {first['seed']}" if n_seeds == 1 else f"{n_seeds} seeds {[r['seed'] for r in results]}"
    base_sub = (
        f"task {first['task']}"
        + (f" n={first['n']}" if first["n"] is not None else "")
        + f" · {seeds_text} · {rounds} rounds · proposer {first['proposer']}, dreamer {first['dreamer']}"
        + (f" · model {first['model']}" if first["model"] else "")
    )
    x = list(range(1, rounds + 1))

    def end_label(ax, name, xy):
        ax.annotate(name, xy, textcoords="offset points", xytext=(6, 0), color=FG, fontsize=8, va="center")

    # (a) round-best vs round -------------------------------------------------
    fig, ax = new_fig(
        "Round-best per rollout and cumulative best (Fig 6a)",
        [
            base_sub,
            "points: each seed's round best · bold step: cumulative best"
            + (" (mean over seeds; thin steps: each seed)" if n_seeds > 1 else ""),
        ],
    )
    style(ax, "", "round", "best valid node score")
    for i, name in enumerate(names):
        s = ser[name]
        color = arm_color(name, i)
        for per_seed in s["roundBest"]["perSeed"]:
            ax.scatter(
                x,
                per_seed,
                s=34,
                color=color,
                marker=ARM_MARKERS.get(name, "o"),
                alpha=0.55,
                edgecolor=BG,
                linewidth=0.8,
                zorder=3,
            )
        if n_seeds > 1:
            for per_seed in s["cumulativeBest"]["perSeed"]:
                ax.plot(
                    x,
                    per_seed,
                    color=color,
                    lw=0.9,
                    ls=ARM_LINESTYLES.get(name, "-"),
                    drawstyle="steps-mid",
                    alpha=0.35,
                    zorder=2,
                )
        mean = s["cumulativeBest"]["mean"]
        ax.plot(
            x, mean, color=color, lw=2.2, ls=ARM_LINESTYLES.get(name, "-"), drawstyle="steps-mid", label=name, zorder=4
        )
        end_label(ax, name, (x[-1] + 0.5, mean[-1]))
    ax.set_xticks(x)
    ax.set_xlim(0.5, rounds + 1.1)
    if len(names) >= 2:
        legend(ax)
    if short:
        ax.text(
            0.5,
            0.06,
            f"N = {rounds} rounds: too short to show a curve",
            transform=ax.transAxes,
            color=WARN,
            fontsize=9.5,
            ha="center",
        )
    p_round = out / "round_best.png"
    fig.savefig(p_round, dpi=130, facecolor=BG)
    plt.close(fig)

    # (b) performance vs cumulative compute -----------------------------------
    fig, ax = new_fig(
        "Cumulative best vs cumulative discovery compute (Figs 3b/5)",
        [
            base_sub,
            "x = cumulative probes (evaluated attempts, the discovery-agent calls) · handler calls and tokens are cost, "
            "not on this axis" + (" · one line per seed" if n_seeds > 1 else ""),
        ],
    )
    style(ax, "", "cumulative probes (discovery-agent calls)", "cumulative best score")
    for i, name in enumerate(names):
        s = ser[name]
        color = arm_color(name, i)
        rightmost = None
        for k, (calls, best) in enumerate(
            zip(s["cumulativeProbes"]["perSeed"], s["cumulativeBest"]["perSeed"], strict=True)
        ):
            ax.plot(
                calls,
                best,
                color=color,
                lw=2.0 if n_seeds == 1 else 1.4,
                ls=ARM_LINESTYLES.get(name, "-"),
                marker=ARM_MARKERS.get(name, "o"),
                ms=5,
                alpha=1.0 if n_seeds == 1 else 0.8,
                label=name if k == 0 else None,
                zorder=3,
            )
            if rightmost is None or calls[-1] > rightmost[0]:
                rightmost = (calls[-1], best[-1])
        if rightmost:
            end_label(ax, name, rightmost)
    ax.margins(x=0.14)
    seed_heads = [h for h in (p["headline"] for p in head["perSeed"]) if h is not None]
    targets = [h["target"] for h in seed_heads]
    budgets = [h["equalBudget"] for h in seed_heads]
    if targets and budgets:
        t = statistics.median(targets)
        b = statistics.median(budgets)
        agg = " (median over seeds)" if n_seeds > 1 else ""
        ax.axhline(t, color=MUTED, ls="--", lw=1, zorder=2)
        ax.text(
            0.01,
            t,
            f"T = {REFERENCE_ARM} final best {fmt(t)}{agg}",
            transform=ax.get_yaxis_transform(),
            color=MUTED,
            fontsize=8,
            va="bottom",
            ha="left",
        )
        ax.axvline(b, color=MUTED, ls=":", lw=1, zorder=2)
        ax.text(
            b,
            0.02,
            f"B = equal budget {fmt(b)}{agg}",
            transform=ax.get_xaxis_transform(),
            color=MUTED,
            fontsize=8,
            rotation=90,
            va="bottom",
            ha="right",
        )
    else:
        ax.text(
            0.5,
            0.06,
            f"no {REFERENCE_ARM} arm: no control lines",
            transform=ax.transAxes,
            color=WARN,
            fontsize=9.5,
            ha="center",
        )
    if len(names) >= 2:
        legend(ax)
    p_compute = out / "compute.png"
    fig.savefig(p_compute, dpi=130, facecolor=BG)
    plt.close(fig)

    # (c) attempts per round ---------------------------------------------------
    fig, ax = new_fig(
        "Evaluated attempts per round (Fig 6b, adaptivity)",
        [
            base_sub,
            "bar = probes the rollout evaluated"
            + (" (mean over seeds; whisker min-max)" if n_seeds > 1 else "")
            + " · Δ k/n = the arm's policy differs from the previous round's in k of n seeds"
            + (
                f" · {REFERENCE_ARM} runs the same policy every round by construction" if REFERENCE_ARM in names else ""
            ),
        ],
    )
    style(ax, "", "round", "evaluated attempts (probes)")
    width = 0.8 / max(1, len(names))
    for i, name in enumerate(names):
        s = ser[name]
        color = arm_color(name, i)
        means = [v or 0 for v in s["probes"]["mean"]]
        highs = [mx if mx is not None else m for m, mx in zip(means, s["probes"]["max"], strict=True)]
        xs = [r + (i - (len(names) - 1) / 2) * width for r in x]
        ax.bar(xs, means, width=width * 0.92, color=color, edgecolor=BG, linewidth=1.2, label=name, zorder=3)
        if n_seeds > 1:
            lo = [m - (mn if mn is not None else m) for m, mn in zip(means, s["probes"]["min"], strict=True)]
            hi = [h - m for m, h in zip(means, highs, strict=True)]
            ax.errorbar(xs, means, yerr=[lo, hi], fmt="none", ecolor=FG, elinewidth=0.9, capsize=2.5, zorder=4)
        for xx, h, changed in zip(xs, highs, s["policyChanges"], strict=True):
            if changed:
                ax.annotate(
                    f"Δ {changed}/{n_seeds}",
                    (xx, h),
                    textcoords="offset points",
                    xytext=(0, 4),
                    ha="center",
                    va="bottom",
                    color=FG,
                    fontsize=7,
                    zorder=5,
                )
    ax.set_xticks(x)
    ax.margins(y=0.12)
    if len(names) >= 2:
        legend(ax, loc="upper right")
    p_attempts = out / "attempts.png"
    fig.savefig(p_attempts, dpi=130, facecolor=BG)
    plt.close(fig)

    # (d) headline card ----------------------------------------------------------
    card_lines = headline_lines(results, head)
    height = 1.3 + 0.36 * len(card_lines)
    fig = plt.figure(figsize=(9, height), facecolor=BG)
    fig.text(
        0.05,
        1 - 0.55 / height,
        f"Headline against the {REFERENCE_ARM} arm (Recursive Fixed Exploration)",
        color=FG,
        fontsize=13,
        ha="left",
        va="top",
    )
    fig.text(0.05, 1 - 0.95 / height, base_sub, color=MUTED, fontsize=8.5, ha="left", va="top")
    y = 1 - 1.35 / height
    for text, tone in card_lines:
        color = {"ok": OK, "bad": BAD, "warn": WARN, "muted": MUTED}.get(tone, FG)
        fig.text(
            0.05,
            y,
            text,
            color=color,
            fontsize=10 if tone != "muted" else 8.5,
            ha="left",
            va="top",
            family="monospace" if tone != "muted" else None,
        )
        y -= 0.36 / height
    p_headline = out / "headline.png"
    fig.savefig(p_headline, dpi=130, facecolor=BG)
    plt.close(fig)

    p_report = out / "report.html"
    p_report.write_text(
        build_report(
            results,
            ser,
            head,
            {"round_best": p_round, "compute": p_compute, "attempts": p_attempts, "headline": p_headline},
        ),
        encoding="utf-8",
    )
    return {
        "round_best": p_round,
        "compute": p_compute,
        "attempts": p_attempts,
        "headline": p_headline,
        "report": p_report,
    }


def ratio_tone(value: float | None) -> str:
    if value is None:
        return "warn"
    if value > 1 + EPS:
        return "ok"
    if value < 1 - EPS:
        return "bad"
    return "value"


def delta_tone(value: float | None) -> str:
    if value is None:
        return "warn"
    if value > EPS:
        return "ok"
    if value < -EPS:
        return "bad"
    return "value"


def headline_lines(results: list[Result], head: HeadlineSummary) -> list[tuple[str, str]]:
    """(text, tone) rows for the headline card and report."""
    lines: list[tuple[str, str]] = []
    if head["reference"] is None:
        lines.append((f"no {REFERENCE_ARM} arm ran: there is no control, so no multiplier is defined", "warn"))
        return lines
    names = arm_names(results)
    for p in head["perSeed"]:
        h = p["headline"]
        if h is None:
            lines.append((f"seed {p['seed']}: no headline (its {REFERENCE_ARM} arm did not run)", "warn"))
            continue
        lines.append(
            (
                f"seed {p['seed']}: T = {fmt(h['target'])} ({h['reference']} final best), "
                f"B = {fmt(h['equalBudget'])} probes ({budget_text(h)})",
                "muted",
            )
        )
        for name in names:
            if name == h["reference"]:
                continue
            calls_tone = ratio_tone(h["callsMultiplier"].get(name))
            score_tone = ratio_tone(h["scoreMultiplier"].get(name))
            lines.append((f"  {name}: {multiplier_text('calls', h, name)}", calls_tone))
            lines.append((f"  {name}: {multiplier_text('score', h, name)}", score_tone))
            lines.append((f"  {name}: {delta_text(h, name)}", delta_tone(h["deltaBest"].get(name))))
        for row in h["ablation"] or []:
            d = row["guidedMinusUnguidedFinalBest"]
            verdict = "guidance worse" if d < 0 else ("guidance better" if d > 0 else "no difference")
            lines.append(
                (
                    f"  ablation {row['guidedArm']} - {row['unguidedArm']} final best = {d:+.4f} ({verdict})",
                    delta_tone(d),
                )
            )
    if head["n"] > 1:
        lines.append(
            (
                f"across {head['n']} seeds: a ratio counts in the seeds where it is defined, and the line says how many",
                "muted",
            )
        )
        for name, agg in head["aggregate"].items():
            if name == head["reference"]:
                continue
            calls_text, calls_tone = aggregate_text("calls", name, agg)
            score_text, score_tone = aggregate_text("score", name, agg)
            lines.append((f"  {calls_text}", calls_tone))
            lines.append((f"  {score_text}", score_tone))
    return lines


def budget_meaning(results: list[Result]) -> str:
    """What the files' B is: the reference arm's total probes, or the smallest arm total."""
    heads = [h for h in (r["headline"] for r in results) if h is not None]
    kinds = {"reference" if h["budgetIsReferenceTotal"] else "min" for h in heads}
    if kinds == {"reference"}:
        return f"the {REFERENCE_ARM} arm's total probes"
    if kinds == {"min"}:
        return "the smallest arm total (the files' equalBudget)"
    return "as recorded in each file"


def build_report(results, ser, head, pngs):
    first = results[0]
    names = arm_names(results)
    n_seeds = len(results)

    def img(path):
        data = base64.b64encode(Path(path).read_bytes()).decode("ascii")
        return f'<img src="data:image/png;base64,{data}" alt="{html.escape(Path(path).stem)}">'

    b = first["budget"]
    meta = [
        ("task", f"{first['task']}" + (f" (n={first['n']})" if first["n"] is not None else "")),
        ("seeds", ", ".join(str(r["seed"]) for r in results) + f" (n={n_seeds})"),
        ("rounds", str(first["rounds"])),
        ("budget per round", f"W={b['workers']} k1={b['k1']} k2={b['k2']} dreams={b['dreams']}"),
        ("replay objective", objective_text(first["objective"])),
        ("arms", ", ".join(names)),
        ("proposer / dreamer", f"{first['proposer']} / {first['dreamer']}"),
        ("model", first["model"] or "none (local path)"),
        (
            "shared round 1",
            "yes" if first["sharedInitialRollout"] else ("no" if first["sharedInitialRollout"] is False else "n/a"),
        ),
        ("initial policy", first["initialPolicyId"]),
        ("result files", "<br>".join(html.escape(r["path"]) for r in results)),
    ]
    rows = "".join(
        f"<tr><th>{html.escape(k)}</th><td>{v if k == 'result files' else html.escape(v)}</td></tr>" for k, v in meta
    )
    notes = "".join(f"<li>{html.escape(n)}</li>" for n in first["notes"]) or "<li>none</li>"
    card = "".join(
        f'<div class="line {tone}">{html.escape(text)}</div>' for text, tone in headline_lines(results, head)
    )

    table_rows = []
    for name in names:
        s = ser[name]
        for i, rnd in enumerate(s["rounds"]):
            cells = (
                name,
                rnd,
                fmt(s["roundBest"]["mean"][i]),
                fmt(s["cumulativeBest"]["mean"][i]),
                fmt(s["probes"]["mean"][i]),
                fmt(s["cumulativeProbes"]["mean"][i]),
                fmt(s["handlerCalls"]["mean"][i]),
                fmt(s["tokens"]["mean"][i]),
                f"{s['policyChanges'][i]}/{n_seeds}",
            )
            table_rows.append("<tr>" + "".join(f"<td>{html.escape(str(v))}</td>" for v in cells) + "</tr>")
    agg_note = " (mean over seeds)" if n_seeds > 1 else ""
    caption_a = (
        "Round-best per rollout (points, one per seed) and the cumulative best "
        f"(step line{', mean with each seed as a thin step' if n_seeds > 1 else ''}) for each arm over "
        f"{first['rounds']} rounds. Round 1 uses the same initial policy in every arm."
        + (f" N = {first['rounds']} rounds is too short to show a curve." if first["rounds"] < 3 else "")
    )
    caption_b = (
        "Cumulative best score against cumulative probes, the evaluated attempts that are the discovery-agent calls "
        f"on every path. T is the {REFERENCE_ARM} arm's final best; B is the equal-budget line, "
        + budget_meaning(results)
        + (" (both median over seeds)" if n_seeds > 1 else "")
        + ". Handler calls (proposer, dreamer, guidance) and child tokens are cost, not the compute axis; "
        "they are in the table below."
    )
    caption_c = (
        f"Evaluated attempts per round per arm{agg_note}. Δ k/n marks rounds where the arm's policy differs from the "
        f"previous round's in k of n seeds; the {REFERENCE_ARM} arm runs the same policy every round by construction, "
        "so its series is flat in expectation."
    )
    caption_d = (
        f"Multipliers against the {REFERENCE_ARM} arm: fewer calls = probesToTarget({REFERENCE_ARM}) / "
        f"probesToTarget(arm), the compute at the first round reaching the {REFERENCE_ARM} arm's final best; "
        f"higher score = bestAtBudget(arm) / bestAtBudget({REFERENCE_ARM}) at the equal budget B. "
        "A ratio below 1 is written the right way round, as its inverse with the direction spelled out "
        "(1.20x MORE calls, 1.03x LOWER score), never as 0.83x fewer; the operands stay (arm vs reference). "
        "An undefined value is written out as not reached / not comparable, never clamped."
        + (
            " Across seeds a ratio is aggregated over the seeds where it is defined and the line says how many: "
            "one seed is a single-seed ratio, not a median, and only a ratio defined in every seed can be shown "
            "in the success colour. An aggregate below 1 prints the inverse of the median ratio."
            if n_seeds > 1
            else ""
        )
    )
    honest = (
        "Every series on this page is measured from the result files; nothing is illustrative. Handler calls and "
        "tokens are cost, not the compute axis: the compute axis is probes (evaluated attempts, the discovery-agent "
        "calls) on every path. The policy score on an arm's own pool is an in-arm replay estimate and is never "
        "compared across arms."
    )
    more = f" and {n_seeds - 1} more seed file(s)" if n_seeds > 1 else ""
    return f"""<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Dream-RSI experiment {html.escape(first["task"])}</title>
<style>
:root {{ color-scheme: dark; --bg: {BG}; --fg: {FG}; --grid: {GRID}; --muted: {MUTED}; --ok: {OK}; --bad: {BAD}; --warn: {WARN}; }}
body {{ background: var(--bg); color: var(--fg); font: 14px/1.5 system-ui, sans-serif; margin: 0; padding: 24px 16px; }}
main {{ max-width: 1040px; margin: 0 auto; }}
h1 {{ font-size: 22px; margin: 0 0 4px; }} h2 {{ font-size: 16px; margin: 32px 0 8px; }}
p.sub {{ color: var(--muted); margin: 0 0 16px; }}
table {{ border-collapse: collapse; width: 100%; font-size: 13px; }}
th, td {{ border: 1px solid var(--grid); padding: 4px 8px; text-align: left; vertical-align: top; }}
th {{ color: var(--muted); font-weight: 500; white-space: nowrap; }}
figure {{ margin: 0 0 8px; }} img {{ width: 100%; height: auto; display: block; border: 1px solid var(--grid); }}
figcaption {{ color: var(--muted); font-size: 12.5px; margin-top: 6px; }}
.card {{ border: 1px solid var(--grid); padding: 12px 16px; font-family: ui-monospace, monospace; font-size: 13px; }}
.line {{ white-space: pre-wrap; }} .ok {{ color: var(--ok); }} .bad {{ color: var(--bad); }} .warn {{ color: var(--warn); }} .muted {{ color: var(--muted); }}
.honest {{ border-left: 3px solid var(--grid); padding-left: 12px; color: var(--muted); font-size: 13px; }}
</style></head><body><main>
<h1>Dream-RSI experiment: {html.escape(first["task"])} vs the fixed-exploration control</h1>
<p class="sub">experiment {html.escape(first["experimentId"])}{more} · schema {html.escape(EXPECTED_SCHEMA)}</p>
<p class="honest">{honest}</p>
<h2>Setup</h2>
<table>{rows}</table>
<h2>Headline</h2>
<div class="card">{card}</div>
<figure>{img(pngs["headline"])}<figcaption>{html.escape(caption_d)}</figcaption></figure>
<h2>Round-best vs round (Fig 6a)</h2>
<figure>{img(pngs["round_best"])}<figcaption>{html.escape(caption_a)}</figcaption></figure>
<h2>Performance vs cumulative compute (Figs 3b/5)</h2>
<figure>{img(pngs["compute"])}<figcaption>{html.escape(caption_b)}</figcaption></figure>
<h2>Attempts per round (Fig 6b)</h2>
<figure>{img(pngs["attempts"])}<figcaption>{html.escape(caption_c)}</figcaption></figure>
<h2>Table{html.escape(agg_note)}</h2>
<table><tr><th>arm</th><th>round</th><th>round best</th><th>cum best</th><th>probes</th><th>cum probes</th><th>handler calls (cost)</th><th>tokens (cost)</th><th>policy changed</th></tr>{"".join(table_rows)}</table>
<h2>Notes from the result files</h2>
<ul>{notes}</ul>
</main></body></html>
"""


# ---------------------------------------------------------------------------- cli


def find_fallback_python():
    candidates = [os.environ.get(PYTHON_ENV), FALLBACK_PYTHON]
    for cand in candidates:
        if not cand:
            continue
        path = os.path.expanduser(cand)
        if not os.path.exists(path):
            continue
        try:
            probe = subprocess.run([path, "-c", "import matplotlib"], capture_output=True, timeout=60, check=False)
        except (OSError, subprocess.SubprocessError):
            continue
        if probe.returncode == 0:
            return path
    return None


def ensure_matplotlib(argv, allow_reexec):
    """True when matplotlib imports here; otherwise re-exec on a fallback interpreter or exit 3."""
    try:
        import matplotlib  # noqa: F401
    except ImportError:
        pass
    else:
        return True
    if allow_reexec and not os.environ.get(REEXEC_ENV):
        fallback = find_fallback_python()
        if fallback:
            print(f"matplotlib not installed for {sys.executable}; re-executing with {fallback}", file=sys.stderr)
            env = dict(os.environ, **{REEXEC_ENV: "1"})
            os.execve(fallback, [fallback, os.path.abspath(__file__), *argv], env)
    print(f"matplotlib not installed for {sys.executable}; try {FALLBACK_PYTHON}", file=sys.stderr)
    return False


def build_parser():
    parser = argparse.ArgumentParser(
        description=(__doc__ or "Plot a Dream-RSI experiment against its fixed-exploration control.").split("\n\n")[0],
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="exit codes: 0 ok, 1 unusable result file, 2 usage, 3 matplotlib missing",
    )
    parser.add_argument(
        "results",
        nargs="+",
        metavar="result.json",
        help="one or more experiment result files (several = seeds of one experiment)",
    )
    parser.add_argument(
        "--out", default=None, help="output directory (default: next to the first result file, in plots/)"
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="print the reduced tables and exit without rendering (no matplotlib needed)",
    )
    parser.add_argument(
        "--no-reexec", action="store_true", help="never re-execute on a fallback interpreter when matplotlib is missing"
    )
    return parser


def main(argv=None):
    argv = list(sys.argv[1:] if argv is None else argv)
    parser = build_parser()
    try:
        args = parser.parse_args(argv)
    except SystemExit as exc:
        return EXIT_USAGE if exc.code else EXIT_OK
    try:
        results = load_results(args.results)
    except ResultError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return EXIT_DATA
    if args.check:
        print(check_tables(results))
        return EXIT_OK
    if not ensure_matplotlib(argv, allow_reexec=not args.no_reexec):
        return EXIT_NO_MATPLOTLIB
    out_dir = Path(args.out) if args.out else Path(args.results[0]).resolve().parent / "plots"
    paths = render(results, out_dir)
    print(f"interpreter {sys.executable}")
    for key, path in paths.items():
        print(f"wrote {key}: {path}")
    return EXIT_OK


if __name__ == "__main__":
    sys.exit(main())
