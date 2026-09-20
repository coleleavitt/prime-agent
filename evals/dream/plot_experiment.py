#!/usr/bin/env python3
"""Plot a Dream-RSI experiment against its fixed-exploration control.

Reads one or more ``result.json`` files written by ``prime-agent dream experiment``
(schema ``prime-agent.dream.experiment/1``, ``core/dream/experiment.ts``) and
renders the paper's evidence figures for our fork:

    round_best.png   round-best points + cumulative-best step per arm vs round   (Fig 6a)
    compute.png      cumulative best vs cumulative discovery compute per arm     (Figs 3b/5)
    attempts.png     evaluated attempts per round per arm, policy changes marked (Fig 6b)
    proposals.png    LLM-proposal validity per round per arm: accepted vs rejected
                     by reason, with the local fallbacks the rejections caused
    dreaming.png     the dreaming audit: every candidate's replay value per dreaming
                     step (filled = eligible, hollow = not), the incumbent's value,
                     the chosen policy, the lever gap and the support coverage
    headline.png     the multipliers against the "fixed" arm, or the literal words
                     "not reached" / "not comparable" when a multiplier is undefined,
                     the noise floor across seeds and the verdict
    report.html      the six figures with captions built from the result metadata

Nothing here is illustrative: every series is read from the result files. Several
files are treated as seeds of one experiment (same task, rounds, budget, objective
and arms, distinct seeds) and reduced to mean/min/max per round; a single file is
plotted as is. Files that disagree on any of those, the replay objective's
``beta1``/``beta2``/``beta3`` included, are refused with the differing values named:
seeds scored by different objectives are not one experiment. Seeds are independent
replicates: the pool a dreaming step replays grows across rounds within one arm
only, never across arms or seeds, which is what makes the paired per-seed
comparison below valid.

Noise floor and verdict (honesty rule for the comparison): the fixed arm's final
best and probes-to-target vary from seed to seed with nothing but the proposer's
sampling, so their spread across seeds is the floor any dream-vs-fixed difference
has to clear. Per non-fixed arm the page lists the paired per-seed delta of the
final best (arm minus fixed, both arms sharing round 1 within a seed) and states
one of: ``single seed: no verdict`` (one file: there is no floor to measure);
``exceeds noise floor`` only when the absolute mean paired delta is larger than
the fixed arm's min..max spread AND every seed's delta has the same sign;
otherwise ``within noise floor``. When the dreaming arm's policy never changed in
any seed there was no treatment, and the verdict is forced to ``within noise
floor (dreaming inert)`` whatever the numbers say.

Dreaming audit: a round record's ``dreaming`` block (the step that chose that
round's policy) may carry ``candidateVerdicts`` (one record per candidate: value,
quality, anytime, cost, roundsSaved, support coverage, eligibility and the reason
it won, tied, lost or was not measurable), ``dreamer`` and a ``leverScan`` (the
best value a fixed grid of local policies reached on the same pool, independent
of what the dreamer proposed). A file written before the audit carries only
``currentScore``/``chosenScore``/``improved``/``candidates``; the panel then
draws those and says the per-candidate scores are not recorded.

The data layer (``load_results``, ``series``, ``headline``, ``--check``) is stdlib
only. matplotlib is imported inside ``render`` so ``--check`` works on any
interpreter. When matplotlib is missing for the running interpreter, the script
looks for a fallback interpreter (``$DREAM_PLOT_PYTHON``, then
``~/Documents/AISpecies/.venv/bin/python``) that has it, reports which one it is
using, and re-executes itself there; if none has it, it exits 3 with an
actionable message.

Compute axis (honesty rule): a probe is an evaluated attempt (a revealed non-root
node). On the LLM path a probe's candidate is either AGENT-GENERATED (the child's
output parsed and entered the tree, ``origin: "llm"``) or a LOCAL FALLBACK (the
child's output was rejected and the local mutator stood in; the child's tokens
were still spent). The paper's "agent calls" are the agent-generated ones, so when
every arm recorded provenance and a child proposer ran, ``compute.png`` puts
``cumulativeAgentGeneratedCalls`` on the compute axis (bold) with
``cumulativeProbes`` as a thin secondary series, and says so. Otherwise, and on
the local path, the axis is ``cumulativeProbes`` and the subtitle says why. The
headline multipliers stay on probes on every path. Handler invocations (proposer,
dreamer, guidance) and child tokens are COST and are never on that axis.

Provenance fields (``agentGeneratedCalls``, ``cumulativeAgentGeneratedCalls``,
``localFallbacks``, ``llmProposals``, ``llmAccepted``, ``llmRejected`` by reason,
and the same names under ``totals``) are read when present. A file written before
origin tracking has none: that is "not recorded", never 0, and the page says so
rather than plotting an empty series as a measurement.

Field names: the reader takes the file's names (``probes``, ``cumulativeProbes``,
``handlerCalls``, ``totals``, ``policyScoreOnOwnPool``, ``probesToTarget``,
``callsMultiplier``, ``equalBudget``, ``bestAtBudget``, ``scoreMultiplier``,
``deltaBest``) and tolerates the equivalent ``attemptsEvaluated`` /
``cumulativeCalls`` / ``callsToTarget`` / ``bestAtEqualBudget`` / ``multipliers``
spelling of the same quantities.

Exact headline: a round record may carry ``probesToRoundBest`` (the probe, in
reveal order, at which the rollout found its best node) and ``improvements``
(``[{probe, score}]``, the best-so-far curve at the probes where it rose), and the
headline may carry ``probesToTargetExact`` / ``callsMultiplierExact``: the
compute at the FIRST PROBE whose score reaches T, not the end of the round that
contains it. The exact multiplier is shown beside the rollout-granular one, and
recomputed from ``improvements`` when the file has the curve but not the number.
A file without either reads ``exact probes to T not recorded``.

Every field newer than the round/arm/headline core (``candidateVerdicts``,
``dreamer``, ``leverScan``, ``probesToRoundBest``, ``improvements``,
``primingTreeIds``/``primingProbes``, ``stoppedEarly``, ``probesToTargetExact``,
``mode.thinking``/``mode.maxOutputTokens``, ``objective.beta3``) is optional:
absent is "not recorded", never 0, and an older file plots exactly as before.

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

# PROPOSAL_REJECT_REASONS in core/dream/proposer.ts, in its order. A reason the file
# carries that is not listed here is kept and printed, never dropped.
REJECT_REASONS: tuple[str, ...] = (
    "parse",
    "shape",
    "invalid-candidate",
    "error",
    "length",
    "aborted",
    "turn-limit",
    "budget",
)
# Rejected segments are neutral ink (a wasted call has no identity of its own; the
# arm hue is reserved for the candidates the agent generated). One fixed step per
# reason, interleaved light/dark so reasons adjacent in the list stay apart in a
# stack; an unlisted reason takes the next unused step.
REASON_GREYS = {
    "parse": "#d3d7dd",
    "shape": "#7b838f",
    "invalid-candidate": "#aab1bb",
    "error": "#4c535e",
    "length": "#c0c5cd",
    "aborted": "#626a76",
    "turn-limit": "#8f97a2",
    "budget": "#3e4552",
}
EXTRA_GREYS = ("#e2e5ea", "#565d69", "#9ca4ae")


class ResultError(ValueError):
    """A result file cannot be used as experiment evidence."""


class SchemaError(ResultError):
    """The file does not carry the expected experiment schema."""


# ------------------------------------------------------------------------- shapes
# The internal shapes. Every optional value is spelled ``| None`` here so that a
# consumer has to narrow it; nothing downstream assumes a field is present.


class CandidateVerdict(TypedDict):
    """One candidate of one dreaming step, as ``core/dream/improve.ts`` records it.

    ``value`` is required (an entry without a numeric value is dropped); every other
    field is None / ``?`` / empty when the file does not carry it.
    """

    index: int
    policyId: str
    origin: str
    changed: list[str]
    duplicateOf: int | None
    value: float
    quality: float | None
    anytime: float | None
    cost: float | None
    roundsSaved: float | None
    N: int | None
    rounds: int | None
    outOfSupportCells: int | None
    inSupportMean: float | None
    inSupportMin: float | None
    eligible: bool
    reason: str


class LeverScan(TypedDict):
    policies: int | None
    eligible: int | None
    bestValue: float | None
    bestPolicyId: str
    gap: float | None


class DreamingRecord(TypedDict):
    """The dreaming step that chose a round's policy; ``candidateVerdicts`` is None when the audit is not recorded."""

    currentScore: float | None
    chosenScore: float | None
    improved: bool | None
    candidates: int | None
    candidateVerdicts: list[CandidateVerdict] | None
    dreamer: str | None
    leverScan: LeverScan | None


class RoundRow(TypedDict):
    round: int
    treeId: str
    policyId: str
    roundBest: float
    cumulativeBest: float
    probes: int
    cumulativeProbes: int
    # Provenance: None is "not recorded" (a file written before origin tracking), never 0.
    agentGeneratedCalls: int | None
    cumulativeAgentGeneratedCalls: int | None
    localFallbacks: int | None
    llmProposals: int | None
    llmAccepted: int | None
    llmRejected: dict[str, int] | None
    handlerCalls: int
    cumulativeHandlerCalls: int
    decisionRounds: int | None
    tokens: int
    cumulativeTokens: int
    poolSize: int
    policyScoreOnReplay: float | None
    dreaming: DreamingRecord | None
    # Exact headline inputs: None is "not recorded".
    probesToRoundBest: int | None
    improvements: list[tuple[int, float]] | None
    # Pool priming (round 1 only): None is "not recorded" or "none".
    primingTreeIds: list[str] | None
    primingProbes: int | None


class Arm(TypedDict):
    arm: str
    fixedPolicy: bool
    guided: bool
    proposer: str
    dreamer: str
    model: str | None
    thinking: str | None
    maxOutputTokens: int | None
    storeDir: str
    runId: str
    initialPolicyId: str
    finalPolicyId: str
    selectedPolicyId: str
    initialPolicyScore: float | None
    finalPolicyScore: float | None
    improved: bool
    policyChanges: int
    finalBest: float
    totalProbes: int
    totalAgentGeneratedCalls: int | None
    totalLocalFallbacks: int | None
    totalLlmProposals: int | None
    totalLlmAccepted: int | None
    totalLlmRejected: dict[str, int] | None
    totalHandlerCalls: int
    totalTokens: int
    # Rollouts that stopped before k1: the file's count, else derived from decisionRounds < k1, else None.
    stoppedEarly: int | None
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
    # The exact (per-probe) headline; None when neither the file nor its rounds carry it.
    probesToTargetExact: dict[str, int | None] | None
    callsMultiplierExact: dict[str, float | None] | None


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
    initialPolicyBeta: float | None
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


class SpreadStats(TypedDict):
    values: list[float | None]
    defined: int
    min: float | None
    max: float | None
    spread: float | None
    std: float | None


class NoiseFloor(TypedDict):
    """The reference arm's own seed-to-seed variation: the floor a paired delta has to clear."""

    n: int
    finalBest: SpreadStats
    probesToTarget: SpreadStats
    probesToTargetExact: SpreadStats | None


class PairedEffect(TypedDict):
    """One non-reference arm against the reference, seed by seed (both arms share round 1 within a seed)."""

    arm: str
    deltas: list[float | None]
    deltasAtBudget: list[float | None]
    callsDeltas: list[int | None]
    callsDeltasExact: list[int | None] | None
    mean: float | None
    positive: int
    negative: int
    inertSeeds: int
    verdict: str


class DreamingSummary(TypedDict):
    """One arm's dreaming in one seed: how often it ran, how often it accepted a candidate, whether it was inert."""

    phases: int
    improved: int
    auditRecorded: int
    policyChanges: int
    inert: bool


class HeadlineSummary(TypedDict):
    reference: str | None
    n: int
    perSeed: list[SeedHeadline]
    aggregate: dict[str, ArmAggregate]
    ablation: dict[str, AblationSummary]
    noiseFloor: NoiseFloor | None
    paired: dict[str, PairedEffect]
    dreaming: dict[str, list[DreamingSummary]]


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


def _int_or_none(value: object) -> int | None:
    """An integer count, or None when the field is absent or not a number: "not recorded", never 0."""
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return int(value)


def _dict(value: object) -> dict[str, object]:
    """The value when it is an object, else an empty one: a malformed sub-field never raises."""
    if not isinstance(value, dict):
        return {}
    return {str(k): v for k, v in value.items()}


def _reject_counts(value: object) -> dict[str, int] | None:
    """``llmRejected`` as reason -> count with every known reason present (0 when unseen).

    None when the field is absent or not an object. A reason the file carries that
    is not in REJECT_REASONS is kept; a count that is not a number is treated as absent.
    """
    if not isinstance(value, dict):
        return None
    counts: dict[str, int] = {reason: 0 for reason in REJECT_REASONS}
    for key, raw in value.items():
        count = _int_or_none(raw)
        if count is not None:
            counts[str(key)] = count
    return counts


def _sum_counts(dicts: list[dict[str, int]]) -> dict[str, int]:
    total: dict[str, int] = {reason: 0 for reason in REJECT_REASONS}
    for d in dicts:
        for key, count in d.items():
            total[key] = total.get(key, 0) + count
    return total


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


def _str_or(value: object, default: str) -> str:
    return value if isinstance(value, str) else default


def _opt_float(value: int | float | None) -> float | None:
    return None if value is None else float(value)


# CandidateVerdict.reason vocabulary of core/dream/improve.ts; the eligible set is the
# candidates that competed in the argmax (the others never could, whatever their value).
CANDIDATE_REASONS: tuple[str, ...] = (
    "winner",
    "tie",
    "worse",
    "quality-rejected",
    "unmeasurable",
    "identical",
    "duplicate",
)
ELIGIBLE_REASONS = frozenset({"winner", "tie", "worse"})
CANDIDATE_ORIGINS = ("llm", "local")
DREAMERS = ("llm", "local", "mixed")


def _candidate_verdict(raw: object, index: int) -> CandidateVerdict | None:
    """One ``candidateVerdicts`` entry; None (dropped) when it is not an object with a numeric ``value``."""
    if not isinstance(raw, dict):
        return None
    entry = _dict(raw)
    value = _num(entry.get("value"))
    if value is None:
        return None
    changed_raw = entry.get("changed")
    changed = [str(c) for c in changed_raw] if isinstance(changed_raw, list) else []
    reason = _str_or(entry.get("reason"), "?")
    eligible_raw = entry.get("eligible")
    eligible = eligible_raw if isinstance(eligible_raw, bool) else reason in ELIGIBLE_REASONS
    origin_raw = entry.get("origin")
    return {
        "index": _int(entry.get("index"), index),
        "policyId": _str_or(entry.get("policyId"), ""),
        "origin": origin_raw if origin_raw in CANDIDATE_ORIGINS else "?",
        "changed": changed,
        "duplicateOf": _int_or_none(entry.get("duplicateOf")),
        "value": value,
        "quality": _num(entry.get("quality")),
        "anytime": _num(entry.get("anytime")),
        "cost": _num(entry.get("cost")),
        "roundsSaved": _num(entry.get("roundsSaved")),
        "N": _int_or_none(entry.get("N")),
        "rounds": _int_or_none(entry.get("rounds")),
        "outOfSupportCells": _int_or_none(entry.get("outOfSupportCells")),
        "inSupportMean": _num(entry.get("inSupportMean")),
        "inSupportMin": _num(entry.get("inSupportMin")),
        "eligible": eligible,
        "reason": reason,
    }


def _lever_scan(raw: object) -> LeverScan | None:
    if not isinstance(raw, dict):
        return None
    scan = _dict(raw)
    return {
        "policies": _int_or_none(scan.get("policies")),
        "eligible": _int_or_none(scan.get("eligible")),
        "bestValue": _num(scan.get("bestValue")),
        "bestPolicyId": _str_or(scan.get("bestPolicyId"), ""),
        "gap": _num(scan.get("gap")),
    }


def _dreaming_record(raw: object) -> DreamingRecord | None:
    """The round's ``dreaming`` block; None when absent or not an object (a fixed arm, round 1, or malformed).

    ``candidates`` is the count the file has always written; ``candidateVerdicts`` is
    the audit list, None when the file predates it. A file that (against the schema)
    wrote a list under ``candidates`` is read as that list with its length as the count.
    """
    if not isinstance(raw, dict):
        return None
    block = _dict(raw)
    verdicts_raw = block.get("candidateVerdicts")
    candidates_raw = block.get("candidates")
    if verdicts_raw is None and isinstance(candidates_raw, list):
        verdicts_raw = candidates_raw
    verdicts: list[CandidateVerdict] | None = None
    if isinstance(verdicts_raw, list):
        verdicts = [v for v in (_candidate_verdict(c, i) for i, c in enumerate(verdicts_raw)) if v is not None]
    count = len(candidates_raw) if isinstance(candidates_raw, list) else _int_or_none(candidates_raw)
    improved_raw = block.get("improved")
    dreamer_raw = block.get("dreamer")
    return {
        "currentScore": _num(block.get("currentScore")),
        "chosenScore": _num(block.get("chosenScore")),
        "improved": improved_raw if isinstance(improved_raw, bool) else None,
        "candidates": count,
        "candidateVerdicts": verdicts,
        "dreamer": dreamer_raw if dreamer_raw in DREAMERS else None,
        "leverScan": _lever_scan(block.get("leverScan")),
    }


def _improvements(raw: object) -> list[tuple[int, float]] | None:
    """``improvements: [{probe, score}]`` as (probe, score) pairs in probe order; None when absent or malformed."""
    if not isinstance(raw, list):
        return None
    pairs: list[tuple[int, float]] = []
    for entry in raw:
        if not isinstance(entry, dict):
            return None
        probe = _int_or_none(entry.get("probe"))
        score = _num(entry.get("score"))
        if probe is None or score is None:
            return None
        pairs.append((probe, score))
    return sorted(pairs)


def _str_list(raw: object) -> list[str] | None:
    if not isinstance(raw, list):
        return None
    return [str(v) for v in raw]


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
    # Provenance: absent stays None. A file that carries only the cumulative count
    # gives up the per-round count as the difference; one that carries only the
    # per-round count gives up the cumulative one as the running sum.
    agent_generated = _int_or_none(_get(row, "agentGeneratedCalls"))
    cumulative_agent_raw = _int_or_none(_get(row, "cumulativeAgentGeneratedCalls"))
    prev_agent = (previous["cumulativeAgentGeneratedCalls"] if previous else None) or 0
    if agent_generated is None and cumulative_agent_raw is not None:
        agent_generated = cumulative_agent_raw - prev_agent
    if agent_generated is None:
        cumulative_agent: int | None = None
    elif cumulative_agent_raw is not None:
        cumulative_agent = cumulative_agent_raw
    else:
        cumulative_agent = prev_agent + agent_generated
    prev_handler = previous["cumulativeHandlerCalls"] if previous else 0
    cumulative_handler = _int(_get(row, "cumulativeHandlerCalls"), prev_handler + handler_calls)
    tokens_raw = _get(row, "tokens", default=0)
    tokens = sum(_int(v) for v in tokens_raw.values()) if isinstance(tokens_raw, dict) else _int(tokens_raw)
    prev_tokens = previous["cumulativeTokens"] if previous else 0
    cumulative_tokens = _int(_get(row, "cumulativeTokens"), prev_tokens + tokens)
    dreaming = _dreaming_record(_get(row, "dreaming"))
    replay = _num(_get(row, "policyScoreOnReplay"))
    if replay is None and dreaming is not None:
        replay = dreaming["chosenScore"]
    return {
        "round": round_no,
        "treeId": str(_get(row, "treeId", default="")),
        "policyId": str(_get(row, "policyId", default="")),
        "roundBest": round_best,
        "cumulativeBest": cumulative_best,
        "probes": probes,
        "cumulativeProbes": cumulative_probes,
        "agentGeneratedCalls": agent_generated,
        "cumulativeAgentGeneratedCalls": cumulative_agent,
        "localFallbacks": _int_or_none(_get(row, "localFallbacks")),
        "llmProposals": _int_or_none(_get(row, "llmProposals")),
        "llmAccepted": _int_or_none(_get(row, "llmAccepted")),
        "llmRejected": _reject_counts(_get(row, "llmRejected")),
        "handlerCalls": handler_calls,
        "cumulativeHandlerCalls": cumulative_handler,
        "decisionRounds": _int_or_none(_get(row, "decisionRounds")),
        "tokens": tokens,
        "cumulativeTokens": cumulative_tokens,
        "poolSize": _int(_get(row, "poolSize"), round_no - 1),
        "policyScoreOnReplay": replay,
        "dreaming": dreaming,
        "probesToRoundBest": _int_or_none(_get(row, "probesToRoundBest")),
        "improvements": _improvements(_get(row, "improvements")),
        "primingTreeIds": _str_list(_get(row, "primingTreeIds")),
        "primingProbes": _int_or_none(_get(row, "primingProbes")),
    }


def stopped_early_count(rounds: list[RoundRow], k1: int | None) -> int | None:
    """Rollouts whose online decision rounds fell short of k1; None unless every round recorded them and k1 is known."""
    if k1 is None:
        return None
    decisions = [row["decisionRounds"] for row in rounds]
    if any(d is None for d in decisions):
        return None
    return sum(1 for d in decisions if d is not None and d < k1)


def normalize_arm(raw: object, index: int, k1: int | None = None) -> Arm:
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
    thinking: object = None
    max_output_tokens: object = None
    if isinstance(mode, dict):
        proposer = _get(mode, "proposer", default="local")
        dreamer = _get(mode, "dreamer", default="local")
        model = mode.get("model")
        thinking = mode.get("thinking")
        max_output_tokens = mode.get("maxOutputTokens")
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

    def total_count(key: str, per_round: list[int | None]) -> int | None:
        """The file's total, else the sum of the rounds when every round recorded it, else None."""
        recorded = _int_or_none(totals.get(key))
        if recorded is not None:
            return recorded
        if all(v is not None for v in per_round):
            return sum(v for v in per_round if v is not None)
        return None

    agent_generated_total = _int_or_none(totals.get("agentGeneratedCalls"))
    if agent_generated_total is None:
        agent_generated_total = last["cumulativeAgentGeneratedCalls"]
    rejected_total = _reject_counts(totals.get("llmRejected"))
    if rejected_total is None:
        per_round_rejected = [row["llmRejected"] for row in rounds]
        if all(d is not None for d in per_round_rejected):
            rejected_total = _sum_counts([d for d in per_round_rejected if d is not None])
    stopped_early = _int_or_none(raw.get("stoppedEarly"))
    if stopped_early is None:
        stopped_early = stopped_early_count(rounds, k1)
    final_policy_id = str(_get(raw, "finalPolicyId", default=last["policyId"]))
    return {
        "arm": name,
        "fixedPolicy": bool(_get(raw, "fixedPolicy", default=name.startswith("fixed"))),
        "guided": bool(_get(raw, "guided", default=name.endswith("-guided"))),
        "proposer": str(proposer),
        "dreamer": str(dreamer),
        "model": model if isinstance(model, str) else None,
        "thinking": thinking if isinstance(thinking, str) else None,
        "maxOutputTokens": _int_or_none(max_output_tokens),
        "storeDir": str(_get(raw, "storeDir", default="")),
        "runId": str(_get(raw, "runId", default="")),
        "initialPolicyId": str(_get(raw, "initialPolicyId", default="")),
        "finalPolicyId": final_policy_id,
        "selectedPolicyId": str(_get(raw, "selectedPolicyId", default=final_policy_id)),
        "initialPolicyScore": initial_score,
        "finalPolicyScore": final_score,
        "improved": improved,
        "policyChanges": policy_changes,
        "finalBest": _num_or(totals.get("finalBest"), _num_or(_get(raw, "finalBest"), last["cumulativeBest"])),
        "totalProbes": _int(
            totals.get("probes"), _int(_get(raw, "totalCalls", "totalAttempts"), last["cumulativeProbes"])
        ),
        "totalAgentGeneratedCalls": agent_generated_total,
        "totalLocalFallbacks": total_count("localFallbacks", [row["localFallbacks"] for row in rounds]),
        "totalLlmProposals": total_count("llmProposals", [row["llmProposals"] for row in rounds]),
        "totalLlmAccepted": total_count("llmAccepted", [row["llmAccepted"] for row in rounds]),
        "totalLlmRejected": rejected_total,
        "totalHandlerCalls": _int(
            totals.get("handlerCalls"), _int(_get(raw, "totalOverheadCalls"), last["cumulativeHandlerCalls"])
        ),
        "totalTokens": _int(totals.get("tokens"), _int(_get(raw, "totalTokens"), last["cumulativeTokens"])),
        "stoppedEarly": stopped_early,
        "rounds": rounds,
    }


def improvements_recorded(arm: Arm) -> bool:
    """Whether every round of this arm carries the ``improvements`` curve (the exact headline's input)."""
    return all(row["improvements"] is not None for row in arm["rounds"])


def exact_probes_to_target(arm: Arm, target: float) -> int | None:
    """cumulativeProbes before the rollout + the probe of its first improvement reaching T; None when never reached.

    Requires ``improvements`` on every round (``improvements_recorded``); the caller
    checks that, because "not recorded" and "not reached" are different answers.
    """
    before = 0
    for row in arm["rounds"]:
        for probe, score in row["improvements"] or []:
            if score >= target - EPS:
                return before + probe
        before = row["cumulativeProbes"]
    return None


def compute_exact_probes(arms: list[Arm], target: float) -> dict[str, int | None] | None:
    """Per arm the exact probes-to-target from the rounds' ``improvements``; None unless every arm recorded them."""
    if not all(improvements_recorded(a) for a in arms):
        return None
    return {a["arm"]: exact_probes_to_target(a, target) for a in arms}


def exact_multipliers(
    reference: str, arms: list[Arm], probes_exact: dict[str, int | None] | None
) -> dict[str, float | None] | None:
    if probes_exact is None:
        return None
    ref_exact = probes_exact.get(reference)
    return {a["arm"]: _ratio(ref_exact, probes_exact.get(a["arm"])) for a in arms}


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
    probesToTargetExact(a) = cumulativeProbes before the round + the probe of the first
    improvement reaching T (from the rounds' ``improvements``); None as a whole when
    any arm lacks the curve, so "not recorded" never reads as "not reached".
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
    probes_exact = compute_exact_probes(arms, target_value)
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
        "probesToTargetExact": probes_exact,
        "callsMultiplierExact": exact_multipliers(reference, arms, probes_exact),
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
    # The exact headline: the file's own numbers when it wrote them, else recomputed
    # from the rounds' improvements when every arm has the curve, else not recorded.
    exact_raw = raw.get("probesToTargetExact")
    probes_exact: dict[str, int | None] | None
    calls_exact: dict[str, float | None] | None
    if isinstance(exact_raw, dict):
        exact = _dict(exact_raw)
        probes_exact = {}
        for name in names:
            value = _num(exact.get(name))
            probes_exact[name] = None if value is None else int(value)
        calls_exact_raw = _dict(raw.get("callsMultiplierExact"))
        if calls_exact_raw:
            calls_exact = {n: _num(calls_exact_raw.get(n)) for n in names}
        else:
            calls_exact = exact_multipliers(reference, arms, probes_exact)
    else:
        probes_exact = computed["probesToTargetExact"] if computed is not None else None
        calls_exact = computed["callsMultiplierExact"] if computed is not None else None
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
        "probesToTargetExact": probes_exact,
        "callsMultiplierExact": calls_exact,
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
    budget = _dict(doc.get("budget"))
    k1 = _int_or_none(budget.get("k1"))
    arms = [normalize_arm(a, i, k1) for i, a in enumerate(arms_raw)]
    rounds = _int(doc.get("rounds"), max(len(a["rounds"]) for a in arms))
    for arm in arms:
        if len(arm["rounds"]) != rounds:
            raise ResultError(f"{path}: arm {arm['arm']} has {len(arm['rounds'])} rounds, file says {rounds}")
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
        "initialPolicyBeta": _num(_dict(doc.get("initialPolicy")).get("beta")),
        "proposer": "/".join(sorted({a["proposer"] for a in arms})),
        "dreamer": "/".join(sorted({a["dreamer"] for a in arms})),
        "model": next((a["model"] for a in arms if a["model"]), None),
        "sharedInitialRollout": shared,
        "createdTs": _num(doc.get("createdTs")),
        "arms": arms,
        "headline": normalize_headline(doc.get("headline"), arms),
        "notes": [str(note) for note in notes],
    }


# The two-term objective every file has; ``beta3`` (the anytime weight) joined it later
# and is part of the pooling key when present, printed only when the file records it.
OBJECTIVE_FIELDS = ("beta1", "beta2")
OBJECTIVE_OPTIONAL_FIELDS = ("beta3",)


def objective_key(objective: dict[str, object] | None) -> tuple[float | None, ...] | None:
    """The replay objective as the tuple that must agree across seeds; None when the file recorded none.

    A file without ``beta3`` was scored by the two-term objective and does not pool
    with one that carries a ``beta3``, whatever its value.
    """
    if objective is None:
        return None
    return tuple(_num(objective.get(field)) for field in (*OBJECTIVE_FIELDS, *OBJECTIVE_OPTIONAL_FIELDS))


def objective_text(objective: dict[str, object] | None) -> str:
    """`beta1=0.05 beta2=0.05` (`beta3=0.25` appended when recorded), or `none recorded` when the file has none."""
    if objective is None:
        return "none recorded"
    parts = [f"{name}={'?' if (v := _num(objective.get(name))) is None else f'{v:g}'}" for name in OBJECTIVE_FIELDS]
    for name in OBJECTIVE_OPTIONAL_FIELDS:
        value = _num(objective.get(name))
        if value is not None:
            parts.append(f"{name}={value:g}")
    return " ".join(parts)


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
    "agentGeneratedCalls",
    "cumulativeAgentGeneratedCalls",
    "localFallbacks",
    "llmProposals",
    "llmAccepted",
    "handlerCalls",
    "cumulativeHandlerCalls",
    "tokens",
    "cumulativeTokens",
    "probesToRoundBest",
)


def arm_names(results: list[Result]) -> list[str]:
    return [a["arm"] for a in results[0]["arms"]]


def arm_of(result: Result, name: str) -> Arm:
    return next(a for a in result["arms"] if a["arm"] == name)


def _field(row: RoundRow, field: str) -> float | None:
    value = row.get(field)
    return _num(value)


def provenance_recorded(arm: Arm) -> bool:
    """Whether this arm record says which probes the agent generated (a file written after origin tracking)."""
    return arm["totalAgentGeneratedCalls"] is not None


def reasons_seen(results: list[Result]) -> list[str]:
    """The reject reasons to show: the known list in its order, then any unlisted reason a file carries."""
    extra: set[str] = set()
    for r in results:
        for arm in r["arms"]:
            for counts in [arm["totalLlmRejected"], *(row["llmRejected"] for row in arm["rounds"])]:
                if counts is not None:
                    extra.update(k for k in counts if k not in REJECT_REASONS)
    return [*REJECT_REASONS, *sorted(extra)]


def reason_grey(reason: str, reasons: list[str]) -> str:
    known = REASON_GREYS.get(reason)
    if known is not None:
        return known
    unlisted = [r for r in reasons if r not in REASON_GREYS]
    return EXTRA_GREYS[unlisted.index(reason) % len(EXTRA_GREYS)]


class StatSeries(TypedDict):
    perSeed: list[list[float | None]]
    mean: list[float | None]
    min: list[float | None]
    max: list[float | None]


def _stat_lists(per_seed_values: list[list[float | None]], rounds: int) -> StatSeries:
    by_round = [_stats([vals[i] for vals in per_seed_values]) for i in range(rounds)]
    return {
        "perSeed": per_seed_values,
        "mean": [s["mean"] for s in by_round],
        "min": [s["min"] for s in by_round],
        "max": [s["max"] for s in by_round],
    }


def dreaming_summary(arm: Arm) -> DreamingSummary:
    """How this arm's dreaming went in one seed.

    ``phases`` counts rounds with a dreaming record, ``improved`` those whose step
    accepted a candidate, ``auditRecorded`` those carrying per-candidate verdicts.
    ``inert`` is true for a dreaming (non-fixed) arm that ran more than one round
    and never changed policy: whatever it dreamed, the control and this arm grew
    every tree with the same policy, so their difference is sampling noise, not a
    learned policy. A fixed-policy arm is never inert (it never dreams).
    """
    records = [row["dreaming"] for row in arm["rounds"] if row["dreaming"] is not None]
    return {
        "phases": len(records),
        "improved": sum(1 for rec in records if rec["improved"] is True),
        "auditRecorded": sum(1 for rec in records if rec["candidateVerdicts"] is not None),
        "policyChanges": arm["policyChanges"],
        "inert": not arm["fixedPolicy"] and len(arm["rounds"]) > 1 and arm["policyChanges"] == 0,
    }


def support_coverage(verdicts: list[CandidateVerdict]) -> float | None:
    """The fraction of candidates replayed fully in support (inSupportMin >= 1); None when none recorded it."""
    known = [v["inSupportMin"] for v in verdicts if v["inSupportMin"] is not None]
    if not known:
        return None
    return sum(1 for m in known if m >= 1 - EPS) / len(known)


def best_candidate_value(verdicts: list[CandidateVerdict]) -> float | None:
    """The best replay value among the ELIGIBLE candidates; None when none was eligible."""
    eligible = [v["value"] for v in verdicts if v["eligible"]]
    return max(eligible) if eligible else None


def _dreaming_series(per_seed: list[list[RoundRow]], rounds: int) -> dict[str, object]:
    """Per round, across seeds: how many seeds dreamed / improved / recorded the audit, and the audit means."""
    recorded = [0] * rounds
    audit = [0] * rounds
    improved = [0] * rounds
    dreamers: list[set[str]] = [set() for _ in range(rounds)]

    def stat(pick) -> StatSeries:
        values: list[list[float | None]] = []
        for rows in per_seed:
            values.append([None if row["dreaming"] is None else pick(row["dreaming"]) for row in rows])
        return _stat_lists(values, rounds)

    for rows in per_seed:
        for i, row in enumerate(rows):
            rec = row["dreaming"]
            if rec is None:
                continue
            recorded[i] += 1
            if rec["candidateVerdicts"] is not None:
                audit[i] += 1
            if rec["improved"] is True:
                improved[i] += 1
            if rec["dreamer"] is not None:
                dreamers[i].add(rec["dreamer"])

    def from_verdicts(pick):
        def inner(rec: DreamingRecord) -> float | None:
            return None if rec["candidateVerdicts"] is None else pick(rec["candidateVerdicts"])

        return inner

    return {
        "recorded": recorded,
        "auditRecorded": audit,
        "improved": improved,
        "dreamers": ["/".join(sorted(kinds)) for kinds in dreamers],
        "currentScore": stat(lambda rec: rec["currentScore"]),
        "chosenScore": stat(lambda rec: rec["chosenScore"]),
        "candidates": stat(lambda rec: None if rec["candidates"] is None else float(rec["candidates"])),
        "eligible": stat(from_verdicts(lambda vs: float(sum(1 for v in vs if v["eligible"])))),
        "bestCandidateValue": stat(from_verdicts(best_candidate_value)),
        "supportCoverage": stat(from_verdicts(support_coverage)),
        "leverGap": stat(lambda rec: None if rec["leverScan"] is None else rec["leverScan"]["gap"]),
        "poolSize": _stat_lists([[float(row["poolSize"]) for row in rows] for rows in per_seed], rounds),
    }


def series(results: list[Result]):
    """Per arm, per round: each field across seeds as per-seed lists plus mean/min/max.

    A provenance field a seed did not record is None in its per-seed list and is left
    out of the mean (``provenanceRecorded`` says which seeds have it); it is never
    read as 0. ``llmRejected`` is one such series per reason (``reasons_seen``).
    ``dreaming`` reduces the per-round dreaming records the same way
    (``_dreaming_series``); ``policyNeverChanged`` and ``stoppedEarly`` are per seed.
    """
    out = {}
    rounds = results[0]["rounds"]
    reasons = reasons_seen(results)
    for name in arm_names(results):
        arms = [arm_of(r, name) for r in results]
        per_seed = [arm["rounds"] for arm in arms]
        fields = {}
        for field in SERIES_FIELDS:
            fields[field] = _stat_lists([[_field(row, field) for row in rows] for rows in per_seed], rounds)
        rejected = {}
        for reason in reasons:
            per_seed_values: list[list[float | None]] = []
            for rows in per_seed:
                per_seed_values.append(
                    [None if row["llmRejected"] is None else float(row["llmRejected"].get(reason, 0)) for row in rows]
                )
            rejected[reason] = _stat_lists(per_seed_values, rounds)
        fields["llmRejected"] = rejected
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
            "fixedPolicy": all(arm["fixedPolicy"] for arm in arms),
            "policyChanges": policy_changes,
            "policyNeverChanged": [dreaming_summary(arm)["inert"] for arm in arms],
            "stoppedEarly": [arm["stoppedEarly"] for arm in arms],
            "primingProbes": [arm["rounds"][0]["primingProbes"] for arm in arms],
            "finalBest": [arm["finalBest"] for arm in arms],
            "totalProbes": [arm["totalProbes"] for arm in arms],
            "totalHandlerCalls": [arm["totalHandlerCalls"] for arm in arms],
            "totalTokens": [arm["totalTokens"] for arm in arms],
            "provenanceRecorded": [provenance_recorded(arm) for arm in arms],
            "totalAgentGeneratedCalls": [arm["totalAgentGeneratedCalls"] for arm in arms],
            "totalLocalFallbacks": [arm["totalLocalFallbacks"] for arm in arms],
            "totalLlmProposals": [arm["totalLlmProposals"] for arm in arms],
            "totalLlmAccepted": [arm["totalLlmAccepted"] for arm in arms],
            "totalLlmRejected": [arm["totalLlmRejected"] for arm in arms],
            "dreaming": _dreaming_series(per_seed, rounds),
            **fields,
        }
    return out


def compute_axis(results: list[Result]) -> tuple[str, str]:
    """Which cumulative count is the compute axis of compute.png, with the reason in words.

    ``("agentGenerated", why)`` when every arm record of every file recorded
    provenance and a child proposer ran (LLM proposals somewhere): the paper's
    "agent calls" are the candidates the agent generated, and probes (which include
    the local fallbacks) become the thin secondary series. Otherwise
    ``("probes", why)``, where ``why`` completes "agent-generated calls ...": not
    recorded (the file predates origin tracking), recorded in only some of the arm
    records, or 0 because no child proposer ran (every probe is a local candidate
    by design, not a fallback). The headline multipliers are on probes on every path.
    """
    arms = [a for r in results for a in r["arms"]]
    recorded = [a for a in arms if provenance_recorded(a)]
    if not recorded:
        return "probes", "not recorded (result predates origin tracking)"
    if len(recorded) < len(arms):
        return "probes", f"recorded in {len(recorded)}/{len(arms)} arm records only"
    proposals = sum(a["totalLlmProposals"] or 0 for a in recorded)
    generated = sum(a["totalAgentGeneratedCalls"] or 0 for a in recorded)
    if proposals == 0 and generated == 0:
        return "probes", "0 (no child proposer ran: every probe is a local candidate by design)"
    return "agentGenerated", "the candidates a child agent produced; probes include the local fallbacks"


def compute_axis_text(results: list[Result]) -> str:
    """One sentence naming the compute axis and why; the headline note is always on probes."""
    kind, why = compute_axis(results)
    if kind == "agentGenerated":
        return f"compute axis: agent-generated calls, {why}; headline multipliers are on probes"
    return f"compute axis: probes; agent-generated calls {why}; headline multipliers are on probes"


def tally_consistent(arm: Arm) -> bool | None:
    """proposals == accepted + sum(rejected) and agent-generated <= probes; None when not recorded."""
    if not provenance_recorded(arm):
        return None
    generated = arm["totalAgentGeneratedCalls"] or 0
    if generated > arm["totalProbes"]:
        return False
    proposals, accepted, rejected = arm["totalLlmProposals"], arm["totalLlmAccepted"], arm["totalLlmRejected"]
    if proposals is None or accepted is None or rejected is None:
        return True
    return proposals == accepted + sum(rejected.values())


def rejected_words(counts: dict[str, int] | None) -> str:
    """`parse 60, shape 21` for the nonzero reasons in list order, or `none`."""
    if not counts:
        return "none"
    order = [*REJECT_REASONS, *sorted(k for k in counts if k not in REJECT_REASONS)]
    parts = [f"{reason} {counts[reason]}" for reason in order if counts.get(reason, 0)]
    return ", ".join(parts) if parts else "none"


def provenance_parts(arm: Arm) -> tuple[str, str]:
    """(probes sentence, proposals sentence) for one arm's totals.

    ``83 probes = 2 agent-generated + 81 local (81 fallbacks)`` and
    ``83 LLM proposals = 2 accepted + 81 rejected (parse 60, shape 21)``; a tally the
    file did not record says so instead of printing zeros.
    """
    if not provenance_recorded(arm):
        return "provenance not recorded (result predates origin tracking)", ""
    generated = arm["totalAgentGeneratedCalls"] or 0
    local = arm["totalProbes"] - generated
    fallbacks = arm["totalLocalFallbacks"]
    probes = f"{arm['totalProbes']} probes = {generated} agent-generated + {local} local"
    probes += f" ({fmt(fallbacks)} fallbacks)" if fallbacks is not None else " (fallbacks not recorded)"
    proposals, accepted, rejected = arm["totalLlmProposals"], arm["totalLlmAccepted"], arm["totalLlmRejected"]
    if proposals is None or accepted is None:
        return probes, "proposal tally not recorded"
    rejected_total = sum(rejected.values()) if rejected else proposals - accepted
    return (
        probes,
        f"{proposals} LLM proposals = {accepted} accepted + {rejected_total} rejected ({rejected_words(rejected)})",
    )


def provenance_text(arm: Arm) -> str:
    """The two ``provenance_parts`` as one line."""
    probes, proposals = provenance_parts(arm)
    return f"{probes}; {proposals}" if proposals else probes


def provenance_tone(arm: Arm) -> str:
    """bad when the tally does not add up; warn when the local fallbacks outnumber the agent's candidates."""
    if tally_consistent(arm) is False:
        return "bad"
    fallbacks = arm["totalLocalFallbacks"] or 0
    if fallbacks and fallbacks >= (arm["totalAgentGeneratedCalls"] or 0):
        return "warn"
    return "muted"


def provenance_lines(results: list[Result]) -> list[tuple[str, str]]:
    """(text, tone) rows for the card: the compute axis, then two lines per recorded arm record."""
    lines = [(wrapped, "muted") for wrapped in textwrap.wrap(compute_axis_text(results), 108)]
    for r in results:
        for arm in r["arms"]:
            if not provenance_recorded(arm):
                continue
            prefix = f"seed {r['seed']} " if len(results) > 1 else ""
            tone = provenance_tone(arm)
            probes, proposals = provenance_parts(arm)
            lines.append((f"  {prefix}{arm['arm']}: {probes}", tone))
            lines.append((f"  {prefix}{arm['arm']}: {proposals}", tone))
            if tally_consistent(arm) is False:
                lines.append(
                    (f"  {prefix}{arm['arm']}: tally does NOT add up (proposals != accepted + rejected)", "bad")
                )
    return lines


def _defined(values: list[float | None]) -> list[float]:
    return [v for v in values if v is not None]


def _median(values: list[float | None]) -> float | None:
    clean = _defined(values)
    return statistics.median(clean) if clean else None


VERDICT_SINGLE = "single seed: no verdict"
VERDICT_EXCEEDS = "exceeds noise floor"
VERDICT_WITHIN = "within noise floor"
VERDICT_INERT = "within noise floor (dreaming inert)"


def spread_stats(values: list[float | None]) -> SpreadStats:
    """min, max, spread (max - min) and sample std of the defined values; std needs two of them."""
    clean = _defined(values)
    return {
        "values": values,
        "defined": len(clean),
        "min": min(clean) if clean else None,
        "max": max(clean) if clean else None,
        "spread": (max(clean) - min(clean)) if clean else None,
        "std": statistics.stdev(clean) if len(clean) >= 2 else None,
    }


def noise_floor(results: list[Result]) -> NoiseFloor | None:
    """The reference arm's final best and probes-to-target across seeds; None unless it ran in every seed.

    Nothing but the proposer's sampling separates the reference arm's seeds (the
    policy never changes), so this spread is the floor a paired delta has to clear.
    """
    heads = [r["headline"] for r in results]
    if not results or any(h is None for h in heads):
        return None
    reference = heads[0]["reference"] if heads[0] is not None else REFERENCE_ARM
    finals: list[float | None] = [arm_of(r, reference)["finalBest"] for r in results]
    probes: list[float | None] = [None if h is None else _opt_float(h["probesToTarget"].get(reference)) for h in heads]
    exact_lists = [h["probesToTargetExact"] for h in heads if h is not None]
    exact: SpreadStats | None = None
    if exact_lists and all(e is not None for e in exact_lists):
        exact = spread_stats([None if e is None else _opt_float(e.get(reference)) for e in exact_lists])
    return {
        "n": len(results),
        "finalBest": spread_stats(finals),
        "probesToTarget": spread_stats(probes),
        "probesToTargetExact": exact,
    }


def verdict(deltas: list[float | None], floor: NoiseFloor | None, n: int, inert_everywhere: bool) -> str:
    """The comparison verdict (module docstring, "Noise floor and verdict").

    One seed: no verdict, the words say so (and add that dreaming was inert when the
    policy never changed, so the reader has both facts). Otherwise, inert in every
    seed forces ``within noise floor (dreaming inert)``; else ``exceeds noise floor``
    needs |mean paired delta| > the reference arm's min..max spread of final best AND
    one sign across every seed; anything else is ``within noise floor``. A delta
    undefined in some seed cannot exceed the floor either.
    """
    if n <= 1:
        return VERDICT_SINGLE + (" (dreaming inert: the arms ran the same policy)" if inert_everywhere else "")
    if inert_everywhere:
        return VERDICT_INERT
    defined = _defined(deltas)
    if floor is None or floor["finalBest"]["spread"] is None or len(defined) < n:
        return VERDICT_WITHIN + f" (paired delta defined in {len(defined)}/{n} seeds)"
    mean = statistics.fmean(defined)
    same_sign = all(d > EPS for d in defined) or all(d < -EPS for d in defined)
    if abs(mean) > max(floor["finalBest"]["spread"], EPS) and same_sign:
        return VERDICT_EXCEEDS
    return VERDICT_WITHIN


def paired_effects(
    results: list[Result], floor: NoiseFloor | None, dreaming: dict[str, list[DreamingSummary]]
) -> dict[str, PairedEffect]:
    """Per non-reference arm: the paired per-seed deltas against the reference and the verdict."""
    heads = [r["headline"] for r in results]
    reference = next((h["reference"] for h in heads if h is not None), None)
    out: dict[str, PairedEffect] = {}
    if reference is None:
        return out
    n = len(results)
    for name in arm_names(results):
        if name == reference:
            continue
        deltas = [None if h is None else h["deltaBest"].get(name) for h in heads]
        at_budget = [None if h is None else h["deltaAtBudget"].get(name) for h in heads]
        calls: list[int | None] = []
        calls_exact: list[int | None] = []
        exact_everywhere = True
        for h in heads:
            if h is None:
                calls.append(None)
                calls_exact.append(None)
                exact_everywhere = False
                continue
            mine, theirs = h["probesToTarget"].get(name), h["probesToTarget"].get(reference)
            calls.append(None if mine is None or theirs is None else mine - theirs)
            exact = h["probesToTargetExact"]
            if exact is None:
                exact_everywhere = False
                calls_exact.append(None)
            else:
                mine_x, theirs_x = exact.get(name), exact.get(reference)
                calls_exact.append(None if mine_x is None or theirs_x is None else mine_x - theirs_x)
        defined = _defined(deltas)
        inert_everywhere = bool(dreaming.get(name)) and all(s["inert"] for s in dreaming[name])
        out[name] = {
            "arm": name,
            "deltas": deltas,
            "deltasAtBudget": at_budget,
            "callsDeltas": calls,
            "callsDeltasExact": calls_exact if exact_everywhere else None,
            "mean": statistics.fmean(defined) if defined else None,
            "positive": sum(1 for d in defined if d > EPS),
            "negative": sum(1 for d in defined if d < -EPS),
            "inertSeeds": sum(1 for s in dreaming.get(name, []) if s["inert"]),
            "verdict": verdict(deltas, floor, n, inert_everywhere),
        }
    return out


def headline(results: list[Result]) -> HeadlineSummary:
    """Per-seed headline rows from the files plus median multipliers and reach counts.

    ``callsMultiplierDefined`` / ``scoreMultiplierDefined`` count the seeds whose
    ratio is defined; the medians are over those seeds only, and ``aggregate_text``
    says so when that is fewer than all of them. ``noiseFloor``, ``paired`` and
    ``dreaming`` carry the comparison verdict and what it rests on.
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
    dreaming = {name: [dreaming_summary(arm_of(r, name)) for r in results] for name in names}
    floor = noise_floor(results)
    return {
        "reference": heads[0]["reference"] if heads else None,
        "n": len(results),
        "perSeed": per_seed,
        "aggregate": aggregate,
        "ablation": {k: {"deltas": v, "median": statistics.median(v)} for k, v in ablation.items()},
        "noiseFloor": floor,
        "paired": paired_effects(results, floor, dreaming),
        "dreaming": dreaming,
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


EXACT_NOT_RECORDED = "exact probes to T not recorded"


def exact_calls_value(head: Headline, arm: str) -> float | None:
    """The exact calls multiplier: the file's when written, else probesToTargetExact(ref) / probesToTargetExact(arm)."""
    exact = head["probesToTargetExact"]
    if exact is None:
        return None
    mults = head["callsMultiplierExact"]
    if mults is not None:
        return mults.get(arm)
    return _ratio(_opt_float(exact.get(head["reference"])), _opt_float(exact.get(arm)))


def exact_calls_text(head: Headline, arm: str) -> str:
    """The exact headline beside the rollout-granular one: `exact 1.40x MORE calls (35 vs 25)` or the words.

    The exact count is the compute at the FIRST PROBE whose score reaches T (from the
    rounds' ``improvements``), not the end of the round that contains it. A file
    without the curve reads ``exact probes to T not recorded``: not recorded is not
    "not reached".
    """
    exact = head["probesToTargetExact"]
    if exact is None:
        return EXACT_NOT_RECORDED
    mine, theirs = exact.get(arm), exact.get(head["reference"])
    value = exact_calls_value(head, arm)
    if value is None:
        return "exact: not reached" if mine is None else "exact: not comparable"
    return f"exact {ratio_words('calls', value)} ({fmt(mine)} vs {fmt(theirs)})"


def spread_text(stats: SpreadStats, digits: int = 4) -> str:
    """`[1.3300, 1.3500] 1.3300..1.3500 (spread 0.0200, std 0.0141)`, with `over k/n seeds` when a seed left it undefined."""
    n = len(stats["values"])
    if stats["defined"] == 0:
        return f"undefined in every seed (0/{n})"
    values = ", ".join(fmt(v, digits) for v in stats["values"])
    text = f"[{values}] {fmt(stats['min'], digits)}..{fmt(stats['max'], digits)} (spread {fmt(stats['spread'], digits)}"
    if stats["std"] is not None:
        text += f", std {fmt(stats['std'], digits)}"
    text += ")"
    if stats["defined"] < n:
        text += f" over {stats['defined']}/{n} seeds"
    return text


def verdict_tone(effect: PairedEffect) -> str:
    """ok/bad when the effect exceeds the floor (by the sign of the mean delta); warn for every other verdict."""
    if effect["verdict"] == VERDICT_EXCEEDS:
        return "ok" if (effect["mean"] or 0) > 0 else "bad"
    return "warn"


def dreaming_text(name: str, summaries: list[DreamingSummary]) -> list[tuple[str, str]]:
    """(text, tone) rows: how the arm's dreaming went, totalled over seeds, then the inert flag when it applies."""
    n = len(summaries)
    phases = sum(s["phases"] for s in summaries)
    improved = sum(s["improved"] for s in summaries)
    changes = sum(s["policyChanges"] for s in summaries)
    audit = sum(s["auditRecorded"] for s in summaries)
    inert = sum(1 for s in summaries if s["inert"])
    text = f"{name}: dreaming ran {phases} step(s), accepted a candidate in {improved}, policy changes {changes}"
    if n > 1:
        text += f" (totals over {n} seeds)"
    if phases:
        text += f"; per-candidate audit recorded in {audit}/{phases} steps"
    lines = [(text, "muted")]
    if inert:
        lines.append(
            (
                f"{name}: policy never changed in {inert}/{n} seed(s): dreaming inert, the arms grew every tree "
                "with one policy",
                "warn",
            )
        )
    return lines


def comparison_lines(head: HeadlineSummary) -> list[tuple[str, str]]:
    """(text, tone) rows for the noise floor, the paired per-seed deltas with their verdict, and the dreaming summary.

    The rows never use the words "median", "calls" or "score", so they stay apart from
    the across-seeds ratio block they follow.
    """
    lines: list[tuple[str, str]] = []
    ref = head["reference"]
    if ref is None:
        return lines
    n = head["n"]
    floor = head["noiseFloor"]
    if n <= 1:
        lines.append((f"noise floor: one seed; the {ref} arm's seed-to-seed spread cannot be measured", "muted"))
    elif floor is None:
        lines.append((f"noise floor: not measurable (the {ref} arm has no headline in some seed)", "warn"))
    else:
        text = (
            f"noise floor ({ref} arm across {n} seeds): final best {spread_text(floor['finalBest'])}; "
            f"probes to T {spread_text(floor['probesToTarget'], 2)}"
        )
        if floor["probesToTargetExact"] is not None:
            text += f"; exact probes to T {spread_text(floor['probesToTargetExact'], 2)}"
        lines.append((text, "muted"))
    for name, effect in head["paired"].items():
        deltas = ", ".join("-" if d is None else f"{d:+.4f}" for d in effect["deltas"])
        text = f"  {name}: paired delta final best vs {ref} per seed [{deltas}]"
        if n > 1 and effect["mean"] is not None:
            text += f", mean {effect['mean']:+.4f}"
        text += f" ({effect['positive']} positive, {effect['negative']} negative)"
        lines.append((text, "muted"))
        lines.append((f"  {name}: verdict: {effect['verdict']}", verdict_tone(effect)))
    for name, summaries in head["dreaming"].items():
        if name == ref or not any(s["phases"] or s["inert"] for s in summaries):
            continue
        lines.extend((f"  {text}", tone) for text, tone in dreaming_text(name, summaries))
    return lines


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


def dreaming_table(name, s, n_seeds):
    """--check rows for one arm's dreaming steps (``series()[arm]``); nothing for an arm that never dreamed.

    Means are over the seeds that recorded the step; ``lever gap`` and ``in support``
    read ``-`` when no seed's step carried the audit.
    """
    d = s["dreaming"]
    recorded = d["recorded"]
    if not any(recorded):
        return []
    steps, audit = sum(recorded), sum(d["auditRecorded"])
    lines = [
        f"  dreaming of {name} ({steps} step(s) over {n_seeds} seed(s); per-candidate audit recorded in {audit}/{steps}"
        + ("; per-candidate scores not recorded (result predates the audit)" if audit == 0 else "")
        + "): round | dreamer | candidates | eligible | current | chosen | improved (seeds) | lever gap | in support"
    ]
    for i, rnd in enumerate(s["rounds"]):
        if not recorded[i]:
            continue
        lines.append(
            f"  {rnd:>5} | {d['dreamers'][i] or '-':>7} | {fmt(d['candidates']['mean'][i]):>10} | "
            f"{fmt(d['eligible']['mean'][i]):>8} | {fmt(d['currentScore']['mean'][i]):>7} | "
            f"{fmt(d['chosenScore']['mean'][i]):>6} | {d['improved'][i]}/{recorded[i]} | "
            f"{fmt(d['leverGap']['mean'][i]):>9} | {fmt(d['supportCoverage']['mean'][i], 2)}"
        )
    return lines


def check_tables(results: list[Result]) -> str:
    """Plain-text reduction of the result files: what --check prints."""
    ser = series(results)
    head = headline(results)
    first = results[0]
    reasons = reasons_seen(results)
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
        recorded = sum(1 for flag in s["provenanceRecorded"] if flag)
        if not recorded:
            lines.append("  provenance: not recorded (result predates origin tracking)")
            continue
        lines.append(
            f"  provenance (recorded in {recorded}/{len(s['seeds'])} seeds; means over those): "
            "round | agent-generated | cum agent-generated | local fallbacks | LLM proposals | accepted | rejected"
        )
        for i, rnd in enumerate(s["rounds"]):
            rejected = ", ".join(
                f"{reason} {fmt(s['llmRejected'][reason]['mean'][i])}"
                for reason in reasons
                if (s["llmRejected"][reason]["mean"][i] or 0) > 0
            )
            lines.append(
                f"  {rnd:>5} | {fmt(s['agentGeneratedCalls']['mean'][i]):>15} | "
                f"{fmt(s['cumulativeAgentGeneratedCalls']['mean'][i]):>19} | {fmt(s['localFallbacks']['mean'][i]):>15} | "
                f"{fmt(s['llmProposals']['mean'][i]):>13} | {fmt(s['llmAccepted']['mean'][i]):>8} | {rejected or 'none'}"
            )
        for r in results:
            arm = arm_of(r, name)
            if not provenance_recorded(arm):
                lines.append(f"  seed {r['seed']}: provenance not recorded")
                continue
            consistent = tally_consistent(arm)
            lines.append(
                f"  seed {r['seed']}: {provenance_text(arm)}; tally consistent (proposals = accepted + rejected): "
                f"{'yes' if consistent else 'NO'}"
            )
    for name, s in ser.items():
        lines.extend(dreaming_table(name, s, len(results)))
    lines.append(compute_axis_text(results))
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
                f"{delta_text(h, name)}; {exact_calls_text(h, name)}"
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
    for text, _tone in comparison_lines(head):
        lines.append(f"  {text}")
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
    """Write the six PNGs and report.html into out_dir; returns their paths."""
    import matplotlib

    matplotlib.use("Agg")
    import matplotlib.pyplot as plt
    from matplotlib.lines import Line2D

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
    axis_kind, axis_why = compute_axis(results)
    agent_axis = axis_kind == "agentGenerated"
    if agent_axis:
        compute_sub = (
            "bold: x = cumulative agent-generated calls (candidates a child agent produced), the compute axis · "
            "thin: x = cumulative probes (every evaluated attempt, local fallbacks included), the headline's axis; "
            "B is drawn in probes"
        )
        x_label = "cumulative calls (bold: agent-generated · thin: all probes)"
    else:
        compute_sub = f"x = cumulative probes (evaluated attempts) · agent-generated calls {axis_why}"
        x_label = "cumulative probes (evaluated attempts)"
    fig, ax = new_fig(
        "Cumulative best vs cumulative discovery compute (Figs 3b/5)",
        [
            base_sub,
            compute_sub
            + " · handler calls and tokens are cost, not on this axis"
            + (" · one line per seed" if n_seeds > 1 else ""),
        ],
    )
    style(ax, "", x_label, "cumulative best score")
    bold_field = "cumulativeAgentGeneratedCalls" if agent_axis else "cumulativeProbes"
    for i, name in enumerate(names):
        s = ser[name]
        color = arm_color(name, i)
        rightmost = None
        for k, (calls, best) in enumerate(zip(s[bold_field]["perSeed"], s["cumulativeBest"]["perSeed"], strict=True)):
            points = [(c, v) for c, v in zip(calls, best, strict=True) if c is not None and v is not None]
            if not points:
                continue
            xs_, ys_ = [p[0] for p in points], [p[1] for p in points]
            ax.plot(
                xs_,
                ys_,
                color=color,
                lw=2.0 if n_seeds == 1 else 1.4,
                ls=ARM_LINESTYLES.get(name, "-"),
                marker=ARM_MARKERS.get(name, "o"),
                ms=5,
                alpha=1.0 if n_seeds == 1 else 0.8,
                label=name if k == 0 else None,
                zorder=3,
            )
            if rightmost is None or xs_[-1] > rightmost[0]:
                rightmost = (xs_[-1], ys_[-1])
        if agent_axis:
            for probes, best in zip(s["cumulativeProbes"]["perSeed"], s["cumulativeBest"]["perSeed"], strict=True):
                ax.plot(
                    probes,
                    best,
                    color=color,
                    lw=0.9,
                    ls=ARM_LINESTYLES.get(name, "-"),
                    marker=ARM_MARKERS.get(name, "o"),
                    ms=3,
                    mfc=BG,
                    alpha=0.6,
                    zorder=2,
                )
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
            f"B = equal budget {fmt(b)} probes{agg}" + (" (thin series)" if agent_axis else ""),
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
    if agent_axis:
        handles, labels = ax.get_legend_handles_labels()
        handles.append(Line2D([], [], color=FG, lw=2.0, label="bold: agent-generated calls (compute axis)"))
        handles.append(
            Line2D([], [], color=FG, lw=0.9, alpha=0.6, label="thin: all probes (headline axis, incl. local fallbacks)")
        )
        labels.extend(h.get_label() for h in handles[len(labels) :])
        ax.legend(handles, labels, facecolor=BG, edgecolor=GRID, labelcolor=FG, fontsize=8, loc="best")
    elif len(names) >= 2:
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
    # The inert stamp: a dreaming arm whose policy never changed grew every tree with
    # the control's policy, so its bars differ from the control's by sampling only.
    stamp_y = 0.97
    for name in names:
        never = sum(1 for flag in ser[name]["policyNeverChanged"] if flag)
        if not never:
            continue
        ax.text(
            0.01,
            stamp_y,
            f"{name}: policy never changed in {never}/{n_seeds} seed(s): dreaming inert",
            transform=ax.transAxes,
            color=WARN,
            fontsize=9,
            ha="left",
            va="top",
            zorder=6,
        )
        stamp_y -= 0.07
    p_attempts = out / "attempts.png"
    fig.savefig(p_attempts, dpi=130, facecolor=BG)
    plt.close(fig)

    # (e) LLM-proposal validity per round ------------------------------------------
    # One panel per arm that recorded provenance; stacked bars of child results per
    # round: accepted (the arm's hue: these became agent-generated nodes) then each
    # rejected reason in neutral steps; the local fallbacks those rejections caused
    # are the marked line. Handler calls and tokens are elsewhere; this is validity.
    reasons = reasons_seen(results)
    prov_names = [name for name in names if any(ser[name]["provenanceRecorded"])]
    recorded_seeds = {name: sum(1 for flag in ser[name]["provenanceRecorded"] if flag) for name in prov_names}
    prov_sub = [base_sub]
    if prov_names:
        prov_sub.append(
            "bar = child results per round: accepted (arm colour; each is one agent-generated node) stacked with "
            "rejected by reason (grey) · x-marked line = local fallbacks, attempts whose last child result was rejected"
            + (
                " · means over the seeds that recorded it: "
                + ", ".join(f"{name} {k}/{n_seeds}" for name, k in recorded_seeds.items())
                if n_seeds > 1
                else ""
            )
        )
    sub_lines = [wrapped for line in prov_sub for wrapped in (textwrap.wrap(line, 118) or [""])]
    line_in, panel_in, gap_in, bottom_in = 0.19, 1.75, 0.5, 0.5
    header_in = 0.62 + line_in * len(sub_lines) + 0.4
    n_panels = max(1, len(prov_names))
    fig_h = header_in + n_panels * panel_in + (n_panels - 1) * gap_in + bottom_in
    fig = plt.figure(figsize=(9, fig_h), facecolor=BG)
    fig.text(0.07, 1 - 0.3 / fig_h, "LLM-proposal validity per round", color=FG, fontsize=13, ha="left", va="top")
    y_text = 1 - 0.62 / fig_h
    for wrapped in sub_lines:
        fig.text(0.07, y_text, wrapped, color=MUTED, fontsize=8.5, ha="left", va="top")
        y_text -= line_in / fig_h
    if not prov_names:
        ax = fig.add_axes((0.09, bottom_in / fig_h, 0.86, panel_in / fig_h))
        style(ax, "")
        ax.set_xticks([])
        ax.set_yticks([])
        ax.text(
            0.5,
            0.5,
            "LLM-proposal provenance not recorded in this result (predates origin tracking): nothing to show",
            transform=ax.transAxes,
            color=WARN,
            fontsize=9.5,
            ha="center",
            va="center",
        )
    for j, name in enumerate(prov_names):
        s = ser[name]
        color = arm_color(name, names.index(name))
        ax_bottom = (bottom_in + (n_panels - 1 - j) * (panel_in + gap_in)) / fig_h
        ax = fig.add_axes((0.09, ax_bottom, 0.62, panel_in / fig_h))
        handles = []
        accepted = [v or 0 for v in s["llmAccepted"]["mean"]]
        handles.append(
            ax.bar(
                x,
                accepted,
                width=0.62,
                color=color,
                edgecolor=BG,
                linewidth=1.2,
                label="accepted (agent-generated)",
                zorder=3,
            )
        )
        stack_top = list(accepted)
        for reason in reasons:
            values = [v or 0 for v in s["llmRejected"][reason]["mean"]]
            if not any(values):
                continue
            handles.append(
                ax.bar(
                    x,
                    values,
                    bottom=stack_top,
                    width=0.62,
                    color=reason_grey(reason, reasons),
                    edgecolor=BG,
                    linewidth=1.2,
                    label=f"rejected: {reason}",
                    zorder=3,
                )
            )
            stack_top = [top + v for top, v in zip(stack_top, values, strict=True)]
        fallbacks = [v or 0 for v in s["localFallbacks"]["mean"]]
        handles.extend(
            ax.plot(
                x,
                fallbacks,
                color=FG,
                lw=1.4,
                ls="--",
                marker="x",
                ms=7,
                mew=1.6,
                label="local fallbacks",
                zorder=4,
            )
        )
        totals_text = ", ".join(
            f"{label} {fmt(statistics.fmean(vals))}"
            for label, vals in (
                ("proposals", [v for v in s["totalLlmProposals"] if v is not None]),
                ("accepted", [v for v in s["totalLlmAccepted"] if v is not None]),
                ("fallbacks", [v for v in s["totalLocalFallbacks"] if v is not None]),
                ("probes", [p for p, flag in zip(s["totalProbes"], s["provenanceRecorded"], strict=True) if flag]),
            )
            if vals
        )
        style(ax, f"{name} · totals: {totals_text}", "round" if j == n_panels - 1 else "", "child results")
        ax.set_xticks(x)
        ax.set_xlim(0.5, rounds + 0.5)
        ax.margins(y=0.15)
        ax.legend(
            handles=handles,
            facecolor=BG,
            edgecolor=GRID,
            labelcolor=FG,
            fontsize=8,
            loc="upper left",
            bbox_to_anchor=(1.01, 1.0),
            borderaxespad=0.0,
        )
    p_proposals = out / "proposals.png"
    fig.savefig(p_proposals, dpi=130, facecolor=BG)
    plt.close(fig)

    # (f) dreaming audit -------------------------------------------------------------
    # One panel per arm that dreamed. Per round (the step that chose that round's
    # policy): every candidate's replay value as a point, filled when it competed in
    # the argmax and hollow when not; llm candidates in the arm's hue, local ones
    # grey; the incumbent as a tick, the chosen policy starred, the best lever-scan
    # policy as a triangle, and the words improved / no change with the lever gap.
    # The strip below is the share of candidates replayed fully in support. A file
    # without candidateVerdicts falls back to the incumbent and chosen values it does
    # carry and says the audit is not recorded.
    dream_names = [name for name in names if any(ser[name]["dreaming"]["recorded"])]
    audited = {name: sum(ser[name]["dreaming"]["auditRecorded"]) for name in dream_names}
    dream_sub = [base_sub]
    if dream_names:
        dream_sub.append(
            "point = one candidate's replay value on the frozen pool (filled: eligible for the argmax; hollow: "
            "identical, duplicate, quality-rejected or unmeasurable) · llm candidates in the arm colour, local grey · "
            "tick = incumbent · star = chosen · triangle = best lever-scan policy · strip = share of candidates "
            "replayed fully in support"
        )
        if not any(audited.values()):
            dream_sub.append(
                "this result predates the per-candidate audit: only the incumbent's and the chosen value are recorded"
            )
        elif n_seeds > 1:
            dream_sub.append("one column offset per seed; the words count the seeds whose step improved")
    sub_lines = [wrapped for line in dream_sub for wrapped in (textwrap.wrap(line, 118) or [""])]
    strip_in, panel_in, gap_in, bottom_in = 0.45, 2.1, 0.55, 0.5
    header_in = 0.62 + line_in * len(sub_lines) + 0.4
    n_panels = max(1, len(dream_names))
    fig_h = header_in + n_panels * (panel_in + strip_in) + (n_panels - 1) * gap_in + bottom_in
    fig = plt.figure(figsize=(9, fig_h), facecolor=BG)
    fig.text(0.07, 1 - 0.3 / fig_h, "Dreaming audit per step", color=FG, fontsize=13, ha="left", va="top")
    y_text = 1 - 0.62 / fig_h
    for wrapped in sub_lines:
        fig.text(0.07, y_text, wrapped, color=MUTED, fontsize=8.5, ha="left", va="top")
        y_text -= line_in / fig_h
    if not dream_names:
        ax = fig.add_axes((0.09, bottom_in / fig_h, 0.86, (panel_in + strip_in) / fig_h))
        style(ax, "")
        ax.set_xticks([])
        ax.set_yticks([])
        ax.text(
            0.5,
            0.5,
            "no dreaming step recorded in this result (every arm ran a fixed policy): nothing to audit",
            transform=ax.transAxes,
            color=WARN,
            fontsize=9.5,
            ha="center",
            va="center",
        )
    offsets = [(k - (n_seeds - 1) / 2) * (0.7 / n_seeds) for k in range(n_seeds)]
    for j, name in enumerate(dream_names):
        s = ser[name]
        d = s["dreaming"]
        color = arm_color(name, names.index(name))
        block_bottom = (bottom_in + (n_panels - 1 - j) * (panel_in + strip_in + gap_in)) / fig_h
        ax_strip = fig.add_axes((0.09, block_bottom, 0.62, (strip_in - 0.15) / fig_h))
        ax = fig.add_axes((0.09, block_bottom + strip_in / fig_h, 0.62, panel_in / fig_h), sharex=ax_strip)
        for arm_rec, dx in zip((arm_of(r, name) for r in results), offsets, strict=True):
            for i, row in enumerate(arm_rec["rounds"]):
                rec = row["dreaming"]
                if rec is None:
                    continue
                xx = x[i] + dx
                for v in rec["candidateVerdicts"] or []:
                    c = color if v["origin"] == "llm" else MUTED
                    ax.plot(
                        [xx],
                        [v["value"]],
                        marker="o",
                        ms=6.5,
                        mfc=c if v["eligible"] else BG,
                        mec=c,
                        mew=1.3,
                        ls="none",
                        zorder=3,
                    )
                if rec["currentScore"] is not None:
                    ax.plot([xx], [rec["currentScore"]], marker="_", ms=16, mew=2.0, color=FG, ls="none", zorder=4)
                if rec["chosenScore"] is not None:
                    star = OK if rec["improved"] else FG
                    ax.plot(
                        [xx], [rec["chosenScore"]], marker="*", ms=11, mfc=star, mec=BG, mew=0.6, ls="none", zorder=5
                    )
                scan = rec["leverScan"]
                if scan is not None and scan["bestValue"] is not None:
                    ax.plot([xx], [scan["bestValue"]], marker="^", ms=7, mfc=BG, mec=ACC, mew=1.3, ls="none", zorder=4)
        if ax.has_data():
            lo, hi = ax.get_ylim()
            span = max(hi - lo, 1e-6)
            ax.set_ylim(lo - 0.08 * span, hi + 0.55 * span)
        for i in range(rounds):
            if not d["recorded"][i]:
                if i == 0:
                    ax.text(
                        x[i],
                        0.96,
                        "no step\n(initial policy)",
                        transform=ax.get_xaxis_transform(),
                        color=MUTED,
                        fontsize=7,
                        ha="center",
                        va="top",
                    )
                continue
            imp, rec_n = d["improved"][i], d["recorded"][i]
            word = "improved" if imp == rec_n else ("no change" if imp == 0 else f"improved {imp}/{rec_n}")
            ax.text(
                x[i],
                0.96,
                word,
                transform=ax.get_xaxis_transform(),
                color=OK if imp else MUTED,
                fontsize=8,
                ha="center",
                va="top",
                fontweight="bold" if imp else "normal",
            )
            gap = d["leverGap"]["mean"][i]
            if gap is None:
                gap_word, gap_color = "lever gap n/a" if d["auditRecorded"][i] else "lever gap not recorded", MUTED
            elif gap > EPS:
                gap_word, gap_color = f"lever gap +{gap:.4f}", ACC
            else:
                gap_word, gap_color = "lever gap 0: no lever", MUTED
            ax.text(
                x[i],
                0.88,
                gap_word,
                transform=ax.get_xaxis_transform(),
                color=gap_color,
                fontsize=7,
                ha="center",
                va="top",
            )
        summaries = head["dreaming"].get(name, [])
        phases = sum(sm["phases"] for sm in summaries)
        improved_steps = sum(sm["improved"] for sm in summaries)
        changes = sum(sm["policyChanges"] for sm in summaries)
        inert = sum(1 for sm in summaries if sm["inert"])
        title = f"{name} · steps {phases}, improved {improved_steps}, policy changes {changes}"
        if inert:
            title += f" · policy never changed in {inert}/{n_seeds} seed(s): dreaming inert"
        style(ax, title, "", "replay value")
        ax.set_xticks(x)
        ax.set_xlim(0.5, rounds + 0.5)
        ax.tick_params(labelbottom=False)
        style(ax_strip, "", "round" if j == n_panels - 1 else "", "in support")
        ax_strip.set_ylim(0, 1.3)
        ax_strip.set_yticks([0, 1])
        ax_strip.set_yticklabels(["0", "1"])
        coverage = d["supportCoverage"]["mean"]
        for i in range(rounds):
            if not d["recorded"][i]:
                continue
            if coverage[i] is None:
                ax_strip.text(x[i], 0.5, "n/a", color=MUTED, fontsize=7, ha="center", va="center")
                continue
            ax_strip.bar(x[i], coverage[i], width=0.6, color=color, edgecolor=BG, linewidth=1.0, zorder=3)
            ax_strip.text(
                x[i],
                min(coverage[i] + 0.05, 1.05),
                f"{coverage[i]:.0%}",
                color=FG,
                fontsize=7,
                ha="center",
                va="bottom",
                zorder=4,
            )
        proxies = [
            Line2D([], [], marker="o", ms=6.5, mfc=color, mec=color, ls="none", label="llm candidate, eligible"),
            Line2D(
                [],
                [],
                marker="o",
                ms=6.5,
                mfc=BG,
                mec=color,
                mew=1.3,
                ls="none",
                label="candidate not eligible (hollow)",
            ),
            Line2D([], [], marker="o", ms=6.5, mfc=MUTED, mec=MUTED, ls="none", label="local candidate"),
            Line2D([], [], marker="_", ms=16, mew=2.0, color=FG, ls="none", label="incumbent value"),
            Line2D([], [], marker="*", ms=11, mfc=FG, mec=BG, ls="none", label="chosen policy (green when improved)"),
            Line2D([], [], marker="^", ms=7, mfc=BG, mec=ACC, mew=1.3, ls="none", label="best lever-scan policy"),
        ]
        ax.legend(
            handles=proxies,
            facecolor=BG,
            edgecolor=GRID,
            labelcolor=FG,
            fontsize=8,
            loc="upper left",
            bbox_to_anchor=(1.01, 1.0),
            borderaxespad=0.0,
        )
    p_dreaming = out / "dreaming.png"
    fig.savefig(p_dreaming, dpi=130, facecolor=BG)
    plt.close(fig)

    # (d) headline card ----------------------------------------------------------
    # A row longer than the card is wrapped, its continuation indented; the
    # monospace rows (10pt) fit ~92 characters on the 9in card, the muted ones ~108.
    card_lines = []
    for text, tone in headline_lines(results, head) + provenance_lines(results):
        width = 108 if tone == "muted" else 92
        indent = " " * (len(text) - len(text.lstrip(" ")) + 4)
        wrapped = textwrap.wrap(text, width, subsequent_indent=indent) or [""]
        card_lines.extend((part, tone) for part in wrapped)
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
            {
                "round_best": p_round,
                "compute": p_compute,
                "attempts": p_attempts,
                "proposals": p_proposals,
                "dreaming": p_dreaming,
                "headline": p_headline,
            },
        ),
        encoding="utf-8",
    )
    return {
        "round_best": p_round,
        "compute": p_compute,
        "attempts": p_attempts,
        "proposals": p_proposals,
        "dreaming": p_dreaming,
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
            exact_tone = "muted" if h["probesToTargetExact"] is None else ratio_tone(exact_calls_value(h, name))
            lines.append((f"  {name}: {exact_calls_text(h, name)}", exact_tone))
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
    lines.extend(comparison_lines(head))
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
        f'<div class="line {tone}">{html.escape(text)}</div>'
        for text, tone in headline_lines(results, head) + provenance_lines(results)
    )
    axis_kind, axis_why = compute_axis(results)
    agent_axis = axis_kind == "agentGenerated"
    reasons = reasons_seen(results)
    any_provenance = any(any(ser[name]["provenanceRecorded"]) for name in names)

    def rejected_cell(s, i):
        if s["llmProposals"]["mean"][i] is None:
            return "-"
        parts = [
            f"{reason} {fmt(s['llmRejected'][reason]['mean'][i])}"
            for reason in reasons
            if (s["llmRejected"][reason]["mean"][i] or 0) > 0
        ]
        return ", ".join(parts) if parts else "0"

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
                fmt(s["agentGeneratedCalls"]["mean"][i]),
                fmt(s["cumulativeAgentGeneratedCalls"]["mean"][i]),
                fmt(s["localFallbacks"]["mean"][i]),
                fmt(s["llmProposals"]["mean"][i]),
                fmt(s["llmAccepted"]["mean"][i]),
                rejected_cell(s, i),
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
    if agent_axis:
        axis_words = (
            "Cumulative best score against cumulative compute. The bold series is on agent-generated calls, the "
            "candidates a child agent produced (origin llm nodes); the thin series is on probes, every evaluated "
            "attempt including the local fallbacks that stood in for a rejected child result. The headline "
            "multipliers and B are in probes. "
        )
    else:
        axis_words = (
            "Cumulative best score against cumulative probes, every evaluated attempt on every path. "
            f"Agent-generated calls are {axis_why}, so they are not on this axis. "
        )
    caption_b = (
        axis_words
        + f"T is the {REFERENCE_ARM} arm's final best; B is the equal-budget line, "
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
    if any_provenance:
        caption_e = (
            f"Child proposer results per round per arm{agg_note}: accepted results (the arm's colour; each became one "
            "agent-generated node) stacked with rejected results by reason (grey: parse = no JSON value in the "
            "output, shape = not the candidate shape, invalid-candidate = the task refused it, error / length / "
            "aborted / turn-limit / budget = the child did not finish). The x-marked line is the local fallbacks: "
            "attempts whose last child result was rejected, so the local mutator's candidate was evaluated with the "
            "child's tokens on its node. proposals = accepted + rejected; on the LLM path probes = accepted + "
            "fallbacks."
        )
    else:
        caption_e = (
            "This result records no proposal provenance (it was written before origin tracking), so accepted, "
            "rejected and fallback counts are not recorded, not 0, and nothing is drawn."
        )
    caption_d = (
        f"Multipliers against the {REFERENCE_ARM} arm: fewer calls = probesToTarget({REFERENCE_ARM}) / "
        f"probesToTarget(arm), the compute at the first round reaching the {REFERENCE_ARM} arm's final best; "
        f"higher score = bestAtBudget(arm) / bestAtBudget({REFERENCE_ARM}) at the equal budget B. "
        "The exact line beside it counts to the first PROBE whose score reaches T (from the rounds' improvements "
        "curve), not the end of the round; a file without the curve reads exact probes to T not recorded. "
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
        + " The verdict follows the noise-floor rule: one seed gives no verdict; with several, the paired per-seed "
        f"delta of final best exceeds the noise floor only when its mean is larger than the {REFERENCE_ARM} arm's "
        "own min..max spread of final best across seeds AND every seed's delta has the same sign; a dreaming arm "
        "whose policy never changed in any seed is within the floor by construction (dreaming inert)."
    )
    dreaming_arms = [name for name in names if any(ser[name]["dreaming"]["recorded"])]
    audit_recorded = any(sum(ser[name]["dreaming"]["auditRecorded"]) for name in dreaming_arms)
    if dreaming_arms and audit_recorded:
        caption_f = (
            f"Every candidate a dreaming step scored, per round it chose the policy for{agg_note}: its mean replay "
            "value on the frozen pool (filled when eligible for the argmax, hollow when identical, duplicate, "
            "quality-rejected or unmeasurable), llm candidates in the arm colour and local ones grey, the "
            "incumbent's value as a tick, the chosen policy starred (green when the step improved), and the best "
            "lever-scan policy (a fixed grid of local policies scored on the same pool, independent of what the "
            "dreamer proposed) as a triangle. The strip is the share of candidates replayed fully in support. "
            "A lever gap of 0 means no grid policy beat the incumbent on that pool: dreaming had nothing to find "
            "there, whatever the dreamer proposed."
        )
    elif dreaming_arms:
        caption_f = (
            "This result predates the per-candidate audit: its dreaming steps recorded only the incumbent's and the "
            "chosen policy's value (tick and star) and whether the step improved. Per-candidate scores, eligibility, "
            "the lever gap and the support coverage are not recorded, not 0, so the strip is empty and the words "
            "say n/a."
        )
    else:
        caption_f = (
            "No arm dreamed in this result (every arm ran a fixed policy), so there is no dreaming step to audit."
        )
    dreaming_rows = []
    for name in dreaming_arms:
        s = ser[name]
        d = s["dreaming"]
        for i, rnd in enumerate(s["rounds"]):
            if not d["recorded"][i]:
                continue
            cells = (
                name,
                rnd,
                d["dreamers"][i] or "-",
                fmt(d["candidates"]["mean"][i]),
                fmt(d["eligible"]["mean"][i]),
                fmt(d["currentScore"]["mean"][i]),
                fmt(d["chosenScore"]["mean"][i]),
                f"{d['improved'][i]}/{d['recorded'][i]}",
                fmt(d["leverGap"]["mean"][i]),
                fmt(d["supportCoverage"]["mean"][i], 2),
            )
            dreaming_rows.append("<tr>" + "".join(f"<td>{html.escape(str(v))}</td>" for v in cells) + "</tr>")
    dreaming_table_html = (
        "<table><tr><th>arm</th><th>round</th><th>dreamer</th><th>candidates</th><th>eligible</th><th>incumbent</th>"
        "<th>chosen</th><th>improved (seeds)</th><th>lever gap</th><th>in support</th></tr>"
        + "".join(dreaming_rows)
        + "</table>"
        if dreaming_rows
        else '<p class="sub">no dreaming step recorded</p>'
    )
    honest = (
        "Every series on this page is measured from the result files; nothing is illustrative. Handler calls and "
        "tokens are cost, not the compute axis. The compute axis of the compute figure is "
        + (
            "agent-generated calls (candidates a child agent produced), with probes (every evaluated attempt, local "
            "fallbacks included) as the thin secondary series"
            if agent_axis
            else f"probes (every evaluated attempt); agent-generated calls {axis_why}"
        )
        + "; the headline multipliers are on probes on every path. A provenance field a file does not carry is "
        "shown as not recorded, never as 0. The policy score on an arm's own pool is an in-arm replay estimate and "
        "is never compared across arms. Seeds are independent replicates: a dreaming step's pool grows across "
        "rounds within one arm only, never across arms or seeds, which is what makes the paired per-seed "
        "comparison and its noise-floor verdict valid."
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
<h2>LLM-proposal validity per round</h2>
<figure>{img(pngs["proposals"])}<figcaption>{html.escape(caption_e)}</figcaption></figure>
<h2>Dreaming audit per step</h2>
<figure>{img(pngs["dreaming"])}<figcaption>{html.escape(caption_f)}</figcaption></figure>
{dreaming_table_html}
<h2>Table{html.escape(agg_note)}</h2>
<p class="sub">Provenance columns (agent-generated through rejected) read - when the file did not record them.</p>
<table><tr><th>arm</th><th>round</th><th>round best</th><th>cum best</th><th>probes</th><th>cum probes</th><th>agent-generated</th><th>cum agent-generated</th><th>local fallbacks</th><th>LLM proposals</th><th>accepted</th><th>rejected by reason</th><th>handler calls (cost)</th><th>tokens (cost)</th><th>policy changed</th></tr>{"".join(table_rows)}</table>
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
