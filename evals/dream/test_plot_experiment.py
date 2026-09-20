"""Tests for evals/dream/plot_experiment.py on a synthetic result set.

Run with either interpreter:

    python3 -m unittest evals/dream/test_plot_experiment.py
    ~/Documents/AISpecies/.venv/bin/python -m unittest evals/dream/test_plot_experiment.py

The data-layer tests need only the stdlib. The render tests are skipped (not
failed) when matplotlib is missing for the running interpreter, and the
exit-3 test runs only there.

The fixture mirrors the shape ``core/dream/experiment.ts`` writes (schema
``prime-agent.dream.experiment/1``): three arms, three rounds, two seeds,
including a "not reached" and a "not comparable" case. ``Honesty`` adds two-arm
seeds where the dream arm reaches T early, late or never, to pin the across-seeds
rule (a ratio defined in 1 of N seeds is a single-seed ratio, never a median in
the success colour), a headline naming a reference arm that did not run, and a
file whose optional fields are all malformed. ``Direction`` pins the wording of a
ratio below 1 (``1.20x MORE calls (72 vs 60)``, never ``0.83x fewer``), and
``DataLayer`` the refusal to pool files scored by different replay objectives.
``Provenance`` covers the per-round provenance fields (``agentGeneratedCalls``,
``localFallbacks``, ``llmProposals``, ``llmAccepted``, ``llmRejected`` by reason):
a file that carries them puts agent-generated calls on the compute axis and gets
the validity panel; a file written before origin tracking reads as "not recorded",
never 0, and pools with a newer seed without inventing zeros for it. ``Verdict``
pins the noise-floor rule (one seed: no verdict; exceeds only when the mean paired
delta clears the fixed arm's spread with one sign in every seed; forced within when
dreaming was inert), ``DreamingAudit`` the ``candidateVerdicts`` / ``dreamer`` /
``leverScan`` reader and its fallback on an older file, ``ExactHeadline`` the
first-probe headline from ``improvements``, and ``NewFields`` ``stoppedEarly``,
priming, the child mode fields and ``beta3``.
"""

from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import os
import re
import statistics
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from typing import Any

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import plot_experiment as pe  # noqa: E402

HAS_MPL = importlib.util.find_spec("matplotlib") is not None
NOTE = (
    "python-speedup: evaluate is wall-clock timed; tree shapes and ids are deterministic, "
    "scores are not byte-deterministic"
)
POLICY_A = "3f9c2a7b1e6d4c05"
POLICY_B = "a81e0d4f77c2b913"


def rows(best: list[float], attempts: list[int], policies: list[str]) -> list[dict[str, Any]]:
    """Round records in the ExperimentRoundRow shape of experiment.ts."""
    out: list[dict[str, Any]] = []
    cum_best: float | None = None
    cum_probes = 0
    for i, (b, a, p) in enumerate(zip(best, attempts, policies, strict=True)):
        cum_best = b if cum_best is None else max(cum_best, b)
        cum_probes += a
        dreaming = None
        if i > 0 and p != POLICY_A:
            dreaming = {"currentScore": 1.2, "chosenScore": 1.25, "improved": True, "candidates": 4}
        out.append(
            {
                "round": i + 1,
                "treeId": f"t-i{i}",
                "policyId": p,
                "roundBest": b,
                "cumulativeBest": cum_best,
                "probes": a,
                "cumulativeProbes": cum_probes,
                "decisionRounds": 5,
                "poolSize": i,
                "handlerCalls": {"proposer": 0, "dreamer": 0, "guidance": 0},
                "cumulativeHandlerCalls": 0,
                "tokens": 0,
                "cumulativeTokens": 0,
                "dreaming": dreaming,
            }
        )
    return out


def arm(name: str, rounds_: list[dict[str, Any]], fixed: bool = False, guided: bool = False) -> dict[str, Any]:
    last = rounds_[-1]
    return {
        "arm": name,
        "fixedPolicy": fixed,
        "guided": guided,
        "mode": {"proposer": "local", "dreamer": "local"},
        "storeDir": f"experiments/x/{name}",
        "runId": "run",
        "initialPolicyId": POLICY_A,
        "finalPolicyId": last["policyId"],
        "policyScoreOnOwnPool": {"initial": 1.2, "final": 1.25 if last["policyId"] != POLICY_A else 1.2},
        "policyChanges": sum(1 for x, y in zip(rounds_, rounds_[1:], strict=False) if x["policyId"] != y["policyId"]),
        "rounds": rounds_,
        "totals": {
            "probes": last["cumulativeProbes"],
            "handlerCalls": 0,
            "tokens": 0,
            "finalBest": last["cumulativeBest"],
        },
    }


def file_headline(arms: list[dict[str, Any]]) -> dict[str, Any] | None:
    """The headline block as experiment.ts writes it (equalBudget = min arm total)."""
    normalized = [pe.normalize_arm(a, i) for i, a in enumerate(arms)]
    h = pe.compute_headline(normalized)
    if h is None:
        return None
    return {
        "reference": h["reference"],
        "target": h["target"],
        "probesToTarget": h["probesToTarget"],
        "callsMultiplier": h["callsMultiplier"],
        "equalBudget": h["equalBudget"],
        "bestAtBudget": h["bestAtBudget"],
        "scoreMultiplier": h["scoreMultiplier"],
        "deltaBest": h["deltaBest"],
    }


def result(
    seed: int, arms: list[dict[str, Any]], notes: list[str] | None = None, headline: Any = "auto"
) -> dict[str, Any]:
    return {
        "schema": pe.EXPECTED_SCHEMA,
        "experimentId": f"python-speedup-s{seed}-n3-1700000000000",
        "task": "python-speedup",
        "seed": seed,
        "rounds": 3,
        "budget": {"workers": 3, "k1": 5, "k2": 10, "dreams": 4},
        "objective": {"beta1": 0.01, "beta2": 0.02},
        "initialPolicyId": POLICY_A,
        "initialPolicy": {},
        "arms": arms,
        "headline": file_headline(arms) if headline == "auto" else headline,
        "sharedInitialRollout": False,
        "createdTs": 1700000000000,
        "notes": [NOTE] if notes is None else notes,
    }


def seed1() -> dict[str, Any]:
    # fixed reaches its own final best at round 2 (30 probes); dream reaches it at 27;
    # dream-guided never reaches it (not reached). Equal budget B = min totals = 39.
    return result(
        1,
        [
            arm("fixed", rows([1.3012, 1.3521, 1.3298], [15, 15, 15], [POLICY_A] * 3), fixed=True),
            arm("dream", rows([1.3012, 1.3902, 1.3610], [15, 12, 12], [POLICY_A, POLICY_B, POLICY_B])),
            arm(
                "dream-guided",
                rows([1.3012, 1.3100, 1.3400], [15, 14, 14], [POLICY_A, POLICY_B, POLICY_B]),
                guided=True,
            ),
        ],
    )


def seed2() -> dict[str, Any]:
    # dream never reaches the fixed arm's final best (not reached); dream-guided
    # spends more than the equal budget in round 1 alone (not comparable).
    return result(
        2,
        [
            arm("fixed", rows([1.29, 1.30, 1.33], [15, 15, 15], [POLICY_A] * 3), fixed=True),
            arm("dream", rows([1.29, 1.32, 1.325], [15, 10, 10], [POLICY_A, POLICY_B, POLICY_B])),
            arm("dream-guided", rows([1.29, 1.335, 1.336], [50, 40, 10], [POLICY_A, POLICY_B, POLICY_B]), guided=True),
        ],
    )


def seed_reaching_late(seed: int) -> dict[str, Any]:
    # fixed reaches its own final best at round 2 (30 probes); dream reaches it only at
    # round 3 (45 probes), so its calls multiplier is 30/45 < 1.
    return result(
        seed,
        [
            arm("fixed", rows([1.30, 1.35, 1.33], [15, 15, 15], [POLICY_A] * 3), fixed=True),
            arm("dream", rows([1.30, 1.31, 1.36], [15, 15, 15], [POLICY_A, POLICY_B, POLICY_B])),
        ],
    )


def seed_reaching_early(seed: int) -> dict[str, Any]:
    # dream reaches the fixed arm's final best at round 2 with 25 probes vs fixed's 30.
    return result(
        seed,
        [
            arm("fixed", rows([1.30, 1.35, 1.33], [15, 15, 15], [POLICY_A] * 3), fixed=True),
            arm("dream", rows([1.30, 1.36, 1.36], [15, 10, 10], [POLICY_A, POLICY_B, POLICY_B])),
        ],
    )


def seed_never_reaching(seed: int) -> dict[str, Any]:
    return result(
        seed,
        [
            arm("fixed", rows([1.30, 1.35, 1.33], [15, 15, 15], [POLICY_A] * 3), fixed=True),
            arm("dream", rows([1.30, 1.31, 1.32], [15, 10, 10], [POLICY_A, POLICY_B, POLICY_B])),
        ],
    )


def seed_more_calls(seed: int) -> dict[str, Any]:
    # fixed first reaches its final best 1.35 at round 3 (60 probes); dream reaches it at
    # round 3 too but after 72 probes, so the calls ratio is 60/72 = 0.83 and must read as
    # 1.20x MORE calls (72 vs 60). Inside B = 60 dream has 1.31 vs fixed's 1.35 (0.97, LOWER).
    return result(
        seed,
        [
            arm("fixed", rows([1.30, 1.31, 1.35], [20, 20, 20], [POLICY_A] * 3), fixed=True),
            arm("dream", rows([1.30, 1.31, 1.36], [24, 24, 24], [POLICY_A, POLICY_B, POLICY_B])),
        ],
    )


def rows_llm(
    best: list[float],
    accepted: list[int],
    rejected: list[dict[str, int]],
    fallbacks: list[int],
    policies: list[str],
) -> list[dict[str, Any]]:
    """Round records with the provenance fields the LLM path writes after origin tracking.

    probes = accepted + fallbacks (every attempt is an accepted child result or a local
    fallback), llmProposals = accepted + sum(rejected), agentGeneratedCalls = accepted.
    """
    out = rows(best, [a + f for a, f in zip(accepted, fallbacks, strict=True)], policies)
    cum_generated = 0
    for row, acc, rej, fb in zip(out, accepted, rejected, fallbacks, strict=True):
        counts = dict.fromkeys(pe.REJECT_REASONS, 0)
        counts.update(rej)
        cum_generated += acc
        row["agentGeneratedCalls"] = acc
        row["cumulativeAgentGeneratedCalls"] = cum_generated
        row["localFallbacks"] = fb
        row["llmProposals"] = acc + sum(counts.values())
        row["llmAccepted"] = acc
        row["llmRejected"] = counts
        row["handlerCalls"] = {"proposer": row["llmProposals"], "dreamer": 0, "guidance": 0}
        row["tokens"] = 300 * row["llmProposals"]
    return out


def arm_llm(name: str, rounds_: list[dict[str, Any]], fixed: bool = False, guided: bool = False) -> dict[str, Any]:
    """An arm record on the LLM path with the provenance totals experiment.ts writes."""
    a = arm(name, rounds_, fixed=fixed, guided=guided)
    a["mode"] = {"proposer": "llm", "dreamer": "llm", "model": "anthropic/claude-sonnet-5"}
    rejected = dict.fromkeys(pe.REJECT_REASONS, 0)
    for r in rounds_:
        for reason, count in r["llmRejected"].items():
            rejected[reason] = rejected.get(reason, 0) + count
    a["totals"].update(
        {
            "agentGeneratedCalls": rounds_[-1]["cumulativeAgentGeneratedCalls"],
            "localFallbacks": sum(r["localFallbacks"] for r in rounds_),
            "llmProposals": sum(r["llmProposals"] for r in rounds_),
            "llmAccepted": sum(r["llmAccepted"] for r in rounds_),
            "llmRejected": rejected,
            "handlerCalls": sum(r["llmProposals"] for r in rounds_),
            "tokens": sum(r["tokens"] for r in rounds_),
        }
    )
    return a


def seed_llm(seed: int) -> dict[str, Any]:
    # The same probes and bests as seed_reaching_early (dream reaches T = 1.35 at 25
    # probes vs the fixed arm's 30), but on the LLM path with provenance: most child
    # results were rejected and the local mutator stood in, as in the measured run.
    return result(
        seed,
        [
            arm_llm(
                "fixed",
                rows_llm(
                    [1.30, 1.35, 1.33],
                    [0, 1, 0],
                    [{"parse": 12, "shape": 3}, {"parse": 10, "shape": 4}, {"parse": 13, "shape": 2}],
                    [15, 14, 15],
                    [POLICY_A] * 3,
                ),
                fixed=True,
            ),
            arm_llm(
                "dream",
                rows_llm(
                    [1.30, 1.36, 1.36],
                    [0, 2, 1],
                    [{"parse": 11, "shape": 4}, {"parse": 6, "shape": 2}, {"parse": 7, "error": 2}],
                    [15, 8, 9],
                    [POLICY_A, POLICY_B, POLICY_B],
                ),
            ),
        ],
    )


def seed_local_with_provenance(seed: int) -> dict[str, Any]:
    # The local path after origin tracking: provenance recorded, every count 0 (no
    # child proposer ran, so every probe is a local candidate by design, not a fallback).
    payload = seed_reaching_early(seed)
    for a in payload["arms"]:
        cum = 0
        for r in a["rounds"]:
            r.update(
                {
                    "agentGeneratedCalls": 0,
                    "cumulativeAgentGeneratedCalls": cum,
                    "localFallbacks": 0,
                    "llmProposals": 0,
                    "llmAccepted": 0,
                    "llmRejected": dict.fromkeys(pe.REJECT_REASONS, 0),
                }
            )
        a["totals"].update(
            {
                "agentGeneratedCalls": 0,
                "localFallbacks": 0,
                "llmProposals": 0,
                "llmAccepted": 0,
                "llmRejected": dict.fromkeys(pe.REJECT_REASONS, 0),
            }
        )
    return payload


def seed_paired(seed: int, fixed_final: float, dream_final: float, changes: bool = True) -> dict[str, Any]:
    """Two arms whose final bests are set directly: the fixed arm reaches its own T at round 2 (30 probes).

    With ``changes`` the dream arm switches to POLICY_B at round 2 (its dreaming step
    improved); without it the arm keeps POLICY_A throughout and never dreams a change:
    the inert case.
    """
    policies = [POLICY_A, POLICY_B, POLICY_B] if changes else [POLICY_A] * 3
    dream_rows = rows([1.30, 1.31, dream_final], [15, 10, 10], policies)
    if not changes:
        for row in dream_rows[1:]:
            row["dreaming"] = {"currentScore": 1.0, "chosenScore": 1.0, "improved": False, "candidates": 4}
    return result(
        seed,
        [
            arm("fixed", rows([1.30, fixed_final, 1.31], [15, 15, 15], [POLICY_A] * 3), fixed=True),
            arm("dream", dream_rows),
        ],
    )


def verdict(
    index: int,
    policy_id: str,
    origin: str,
    value: float,
    quality: float,
    reason: str,
    in_support_min: float = 1.0,
    duplicate_of: int | None = None,
) -> dict[str, Any]:
    """One CandidateVerdict as core/dream/improve.ts records it."""
    return {
        "index": index,
        "policyId": policy_id,
        "policy": {"selectionRule": "best-first", "beta": 6},
        "origin": origin,
        "changed": ["beta"] if reason != "identical" else [],
        "duplicateOf": duplicate_of,
        "value": value,
        "quality": quality,
        "anytime": quality - 0.01,
        "cost": 0.5,
        "roundsSaved": 0.2,
        "N": 12,
        "rounds": 5,
        "outOfSupportCells": 0 if in_support_min >= 1 else 2,
        "inSupportMean": 1.0 if in_support_min >= 1 else 0.9,
        "inSupportMin": in_support_min,
        "eligible": reason in ("winner", "tie", "worse"),
        "reason": reason,
    }


ROUND2_VERDICTS = [
    verdict(0, POLICY_B, "llm", 1.26, 1.27, "winner"),
    verdict(1, "c1c1c1c1c1c1c1c1", "local", 1.18, 1.19, "worse"),
    verdict(2, "c2c2c2c2c2c2c2c2", "llm", 1.30, 0.90, "quality-rejected"),
    verdict(3, POLICY_B, "llm", 1.26, 1.27, "duplicate", duplicate_of=0),
]
ROUND3_VERDICTS = [
    verdict(0, POLICY_B, "llm", 1.25, 1.26, "identical"),
    verdict(1, "c4c4c4c4c4c4c4c4", "llm", 1.22, 1.23, "worse"),
    verdict(2, "c5c5c5c5c5c5c5c5", "local", 1.24, 1.25, "tie"),
    verdict(3, "c6c6c6c6c6c6c6c6", "llm", 1.28, 1.29, "unmeasurable", in_support_min=0.5),
]


def seed_audited(seed: int) -> dict[str, Any]:
    """The dream arm of seed_reaching_early with the per-candidate audit on both dreaming steps.

    Round 2's step accepted the llm winner (1.26 over the incumbent's 1.20; the lever
    scan found 1.27, a gap of 0.07); round 3's step kept the incumbent (no eligible
    candidate beat 1.25; lever gap 0; one candidate left the support).
    """
    payload = seed_reaching_early(seed)
    dream_rows = payload["arms"][1]["rounds"]
    dream_rows[1]["dreaming"] = {
        "currentScore": 1.2,
        "chosenScore": 1.26,
        "improved": True,
        "candidates": 4,
        "candidateVerdicts": ROUND2_VERDICTS,
        "dreamer": "mixed",
        "leverScan": {
            "policies": 40,
            "eligible": 30,
            "bestValue": 1.27,
            "bestPolicyId": "1e1e1e1e1e1e1e1e",
            "gap": 0.07,
        },
    }
    dream_rows[2]["dreaming"] = {
        "currentScore": 1.25,
        "chosenScore": 1.25,
        "improved": False,
        "candidates": 4,
        "candidateVerdicts": ROUND3_VERDICTS,
        "dreamer": "llm",
        "leverScan": {"policies": 40, "eligible": 28, "bestValue": 1.25, "bestPolicyId": POLICY_B, "gap": 0},
    }
    return payload


def with_improvements(payload: dict[str, Any], curves: dict[str, list[list[tuple[int, float]]]]) -> dict[str, Any]:
    """Attach ``improvements`` / ``probesToRoundBest`` per round per arm (the exact headline's inputs)."""
    for a in payload["arms"]:
        for row, curve in zip(a["rounds"], curves[a["arm"]], strict=True):
            row["improvements"] = [{"probe": p, "score": s} for p, s in curve]
            row["probesToRoundBest"] = curve[-1][0] if curve else 0
    return payload


def seed_exact(seed: int) -> dict[str, Any]:
    """seed_reaching_early with improvement curves: T = 1.35 is reached by fixed at probe 15 + 7 = 22 and by dream
    at 15 + 4 = 19 (the rollout-granular counts are 30 and 25)."""
    return with_improvements(
        seed_reaching_early(seed),
        {
            "fixed": [[(1, 1.30)], [(3, 1.33), (7, 1.35)], [(2, 1.32)]],
            "dream": [[(1, 1.30)], [(4, 1.36)], []],
        },
    )


# A ratio below 1 written as `0.83x fewer` / `0.97x higher`; the caption's own "never as 0.83x fewer" is allowed.
INVERTED_WORDING = re.compile(r"(?<!never as )\b0\.\d+x (fewer|higher)")


def num(value: float | None) -> float:
    """Narrow an optional number the fixture is known to define (``or 0`` would turn a real 0.0 into the default)."""
    assert value is not None
    return value


def write(dir_: str, name: str, payload: dict[str, Any]) -> str:
    path = Path(dir_) / name
    path.write_text(json.dumps(payload, indent=2), encoding="utf-8")
    return str(path)


def aggregate_line(lines: list[tuple[str, str]], name: str, kind: str) -> tuple[str, str]:
    """The one across-seeds (text, tone) row of headline_lines for this arm and ratio kind."""
    start = next(i for i, (text, _tone) in enumerate(lines) if text.startswith("across "))
    needle = "calls" if kind == "calls" else "score"
    matches = [
        (text, tone) for text, tone in lines[start + 1 :] if text.strip().startswith(f"{name}:") and needle in text
    ]
    assert len(matches) == 1, (name, kind, lines)
    return matches[0]


class DataLayer(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.p1 = write(self.tmp.name, "s1.json", seed1())
        self.p2 = write(self.tmp.name, "s2.json", seed2())

    def tearDown(self):
        self.tmp.cleanup()

    def test_wrong_schema_is_rejected_naming_the_expected_schema(self):
        bad = seed1()
        bad["schema"] = "x/0"
        path = write(self.tmp.name, "bad.json", bad)
        with self.assertRaises(pe.SchemaError) as ctx:
            pe.load_results([path])
        self.assertIn(pe.EXPECTED_SCHEMA, str(ctx.exception))

    def test_mismatched_experiments_are_rejected(self):
        other = seed2()
        other["rounds"] = 2
        for a in other["arms"]:
            a["rounds"] = a["rounds"][:2]
        path = write(self.tmp.name, "other.json", other)
        with self.assertRaises(pe.ResultError) as ctx:
            pe.load_results([self.p1, path])
        message = str(ctx.exception)
        self.assertIn(f"{path}: rounds differ from {self.p1} (rounds 2 vs 3)", message)
        dup = write(self.tmp.name, "dup.json", seed1())
        with self.assertRaises(pe.ResultError):
            pe.load_results([self.p1, dup])

    def test_files_scored_by_different_objectives_are_refused_naming_both(self):
        other = seed2()
        other["objective"] = {"beta1": 0.05, "beta2": 0.05}
        path = write(self.tmp.name, "other-objective.json", other)
        with self.assertRaises(pe.ResultError) as ctx:
            pe.load_results([self.p1, path])
        message = str(ctx.exception)
        self.assertIn(
            f"{path}: objective differ from {self.p1} (objective beta1=0.05 beta2=0.05 vs beta1=0.01 beta2=0.02)",
            message,
            "the refusal names both files and both objectives, and only the field that differs",
        )
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            code = pe.main([self.p1, path, "--check"])
        self.assertEqual(code, 1)
        self.assertIn("beta1=0.05 beta2=0.05 vs beta1=0.01 beta2=0.02", err.getvalue())

    def test_a_file_without_an_objective_does_not_pool_with_one_that_has_it(self):
        bare = seed2()
        del bare["objective"]
        path = write(self.tmp.name, "bare.json", bare)
        with self.assertRaises(pe.ResultError) as ctx:
            pe.load_results([self.p1, path])
        self.assertIn("objective none recorded vs beta1=0.01 beta2=0.02", str(ctx.exception))
        bare_first = seed1()
        del bare_first["objective"]
        both = pe.load_results([write(self.tmp.name, "bare1.json", bare_first), path])
        self.assertEqual(len(both), 2, "two files that both record no objective are still one experiment")
        self.assertEqual(pe.objective_text(both[0]["objective"]), "none recorded")

    def test_objective_key_looks_at_beta1_beta2_and_the_optional_beta3(self):
        # beta3 (the anytime weight) joined the objective later: a file without it was
        # scored by the two-term objective, so its key carries None there and it does
        # not pool with a file that has one, whatever the value.
        self.assertEqual(pe.objective_key({"beta1": 0.05, "beta2": 0.05}), (0.05, 0.05, None))
        self.assertEqual(pe.objective_key({"beta1": 0.05, "beta2": 0.05, "note": "x"}), (0.05, 0.05, None))
        self.assertEqual(pe.objective_key({"beta1": 0.05, "beta2": 0.1, "beta3": 0.25}), (0.05, 0.1, 0.25))
        self.assertEqual(pe.objective_key({"beta1": "0.05"}), (None, None, None))
        self.assertIsNone(pe.objective_key(None))
        self.assertEqual(pe.objective_text({"beta1": 0.05, "beta2": 1e-5}), "beta1=0.05 beta2=1e-05")
        self.assertEqual(pe.objective_text({"beta2": 0.05}), "beta1=? beta2=0.05")
        self.assertEqual(
            pe.objective_text({"beta1": 0.05, "beta2": 0.1, "beta3": 0.25}), "beta1=0.05 beta2=0.1 beta3=0.25"
        )
        three_term = seed2()
        three_term["objective"] = {"beta1": 0.01, "beta2": 0.02, "beta3": 0.25}
        path = write(self.tmp.name, "beta3.json", three_term)
        with self.assertRaises(pe.ResultError) as ctx:
            pe.load_results([self.p1, path])
        self.assertIn("objective beta1=0.01 beta2=0.02 beta3=0.25 vs beta1=0.01 beta2=0.02", str(ctx.exception))

    def test_check_header_names_the_objective(self):
        text = pe.check_tables(pe.load_results([self.p1, self.p2]))
        self.assertIn("objective beta1=0.01 beta2=0.02", text.splitlines()[0])

    def test_headline_values(self):
        results = pe.load_results([self.p1, self.p2])
        h1 = results[0]["headline"]
        assert h1 is not None
        self.assertAlmostEqual(h1["target"], 1.3521)
        self.assertEqual(h1["equalBudget"], 39)
        self.assertFalse(h1["budgetIsReferenceTotal"])
        self.assertEqual(h1["probesToTarget"], {"fixed": 30, "dream": 27, "dream-guided": None})
        self.assertAlmostEqual(h1["callsMultiplier"]["dream"], 30 / 27)
        self.assertEqual(h1["bestAtBudget"], {"fixed": 1.3521, "dream": 1.3902, "dream-guided": 1.31})
        self.assertAlmostEqual(h1["scoreMultiplier"]["dream"], 1.3902 / 1.3521)
        self.assertAlmostEqual(h1["scoreMultiplier"]["dream-guided"], 1.31 / 1.3521)
        self.assertAlmostEqual(h1["deltaBest"]["dream"], 0.0381)
        self.assertAlmostEqual(h1["deltaBest"]["dream-guided"], 1.34 - 1.3521)
        self.assertEqual(h1["callsMultiplier"]["fixed"], 1.0)
        self.assertEqual(h1["scoreMultiplier"]["fixed"], 1.0)
        self.assertIsNone(h1["callsMultiplier"]["dream-guided"])
        self.assertEqual(pe.multiplier_text("calls", h1, "dream-guided"), "not reached")
        self.assertIn("1.11x fewer calls (27 vs 30)", pe.multiplier_text("calls", h1, "dream"))
        self.assertEqual(len(h1["ablation"]), 1)
        self.assertLess(h1["ablation"][0]["guidedMinusUnguidedFinalBest"], 0)

        h2 = results[1]["headline"]
        assert h2 is not None
        self.assertEqual(h2["equalBudget"], 35)
        self.assertIsNone(h2["probesToTarget"]["dream"])
        self.assertIsNone(h2["bestAtBudget"]["dream-guided"])
        self.assertEqual(pe.multiplier_text("score", h2, "dream-guided"), "not comparable")
        self.assertAlmostEqual(h2["callsMultiplier"]["dream-guided"], 45 / 90)
        self.assertAlmostEqual(h2["scoreMultiplier"]["dream"], 1.325 / 1.30)

        agg = pe.headline(results)["aggregate"]
        self.assertEqual(agg["dream"]["reached"], 1)
        self.assertEqual(agg["dream"]["n"], 2)
        self.assertEqual(agg["dream-guided"]["comparable"], 1)
        self.assertAlmostEqual(agg["dream"]["callsMultiplierMedian"], 30 / 27)
        self.assertAlmostEqual(agg["dream"]["scoreMultiplierMedian"], (1.3902 / 1.3521 + 1.325 / 1.30) / 2)

    def test_series_values(self):
        results = pe.load_results([self.p1, self.p2])
        ser = pe.series(results)
        self.assertEqual(ser["fixed"]["probes"]["mean"], [15.0, 15.0, 15.0])
        self.assertEqual(ser["dream"]["cumulativeProbes"]["mean"][2], 37.0)
        self.assertEqual(ser["dream"]["cumulativeProbes"]["min"][2], 35)
        self.assertEqual(ser["dream"]["cumulativeProbes"]["max"][2], 39)
        self.assertEqual(ser["dream"]["policyChanges"], [0, 2, 0])
        self.assertEqual(ser["fixed"]["policyChanges"], [0, 0, 0])
        self.assertAlmostEqual(ser["dream"]["cumulativeBest"]["mean"][2], (1.3902 + 1.325) / 2)
        self.assertEqual(ser["dream"]["seeds"], [1, 2])
        self.assertEqual(ser["dream"]["handlerCalls"]["mean"], [0.0, 0.0, 0.0])

    def test_check_prints_tables_and_exits_zero(self):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = pe.main([self.p1, self.p2, "--check"])
        self.assertEqual(code, 0)
        text = out.getvalue()
        for name in ("fixed", "dream", "dream-guided"):
            self.assertIn(f"arm {name}", text)
        self.assertIn("not reached", text)
        self.assertIn("not comparable", text)
        self.assertIn("agrees with the file: yes", text)
        self.assertNotIn("agrees with the file: NO", text)
        self.assertIn(NOTE, text)

    def test_missing_headline_and_no_reference_arm(self):
        only_dream = result(
            3, [arm("dream", rows([1.0, 1.1, 1.2], [5, 5, 5], [POLICY_A, POLICY_B, POLICY_B]))], headline=None
        )
        path = write(self.tmp.name, "nofixed.json", only_dream)
        results = pe.load_results([path])
        self.assertIsNone(results[0]["headline"])
        text = pe.check_tables(results)
        self.assertIn("no control", text)
        lines = pe.headline_lines(results, pe.headline(results))
        self.assertEqual(lines[0][1], "warn")

    def test_alternate_field_names_normalize_identically(self):
        canonical = pe.load_results([self.p1])[0]
        alt = seed1()
        for a in alt["arms"]:
            mode = a.pop("mode")
            a["proposer"], a["dreamer"], a["model"] = mode["proposer"], mode["dreamer"], None
            own = a.pop("policyScoreOnOwnPool")
            a["initialPolicyScore"], a["finalPolicyScore"] = own["initial"], own["final"]
            totals = a.pop("totals")
            a["finalBest"], a["totalCalls"], a["totalTokens"] = totals["finalBest"], totals["probes"], totals["tokens"]
            a["totalOverheadCalls"] = totals["handlerCalls"]
            for r in a["rounds"]:
                r["attemptsEvaluated"] = r.pop("probes")
                r["agentCalls"] = r["attemptsEvaluated"]
                r["cumulativeCalls"] = r.pop("cumulativeProbes")
                r["overheadCalls"] = sum(r.pop("handlerCalls").values())
                r.pop("cumulativeHandlerCalls")
        h = alt["headline"]
        alt["headline"] = {
            "reference": "fixed",
            "target": h["target"],
            "budget": h["equalBudget"],
            "callsToTarget": h["probesToTarget"],
            "bestAtEqualBudget": h["bestAtBudget"],
            "multipliers": {
                k: {"callsFewer": h["callsMultiplier"][k], "scoreHigher": h["scoreMultiplier"][k]}
                for k in h["callsMultiplier"]
            },
        }
        path = write(self.tmp.name, "alt.json", alt)
        loaded = pe.load_results([path])[0]
        for key in ("arms", "headline", "task", "rounds", "seed", "proposer", "dreamer", "model"):
            self.assertEqual(loaded[key], canonical[key], key)

    def test_cli_wrong_schema_exits_one(self):
        bad = seed1()
        bad["schema"] = "x/0"
        path = write(self.tmp.name, "bad.json", bad)
        proc = subprocess.run(
            [sys.executable, str(HERE / "plot_experiment.py"), path, "--check"],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(proc.returncode, 1)
        self.assertIn(pe.EXPECTED_SCHEMA, proc.stderr)

    @unittest.skipIf(HAS_MPL, "matplotlib is installed for this interpreter")
    def test_cli_without_matplotlib_exits_three(self):
        env = dict(os.environ, DREAM_PLOT_PYTHON="/nonexistent/python")
        proc = subprocess.run(
            [
                sys.executable,
                str(HERE / "plot_experiment.py"),
                self.p1,
                "--no-reexec",
                "--out",
                os.path.join(self.tmp.name, "plots"),
            ],
            capture_output=True,
            text=True,
            check=False,
            env=env,
        )
        self.assertEqual(proc.returncode, 3)
        self.assertIn("matplotlib not installed for", proc.stderr)
        self.assertIn(pe.FALLBACK_PYTHON, proc.stderr)


class Honesty(unittest.TestCase):
    """The aggregate lines never dress a partial result up as a median in the success colour."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()

    def tearDown(self):
        self.tmp.cleanup()

    def load(self, *payloads):
        paths = [write(self.tmp.name, f"s{p['seed']}.json", p) for p in payloads]
        return pe.load_results(paths)

    def test_ratio_defined_in_one_of_three_seeds_is_a_single_seed_ratio_not_a_median(self):
        results = self.load(seed_reaching_early(1), seed_never_reaching(2), seed_never_reaching(3))
        head = pe.headline(results)
        agg = head["aggregate"]["dream"]
        self.assertEqual((agg["n"], agg["reached"], agg["callsMultiplierDefined"]), (3, 1, 1))
        self.assertAlmostEqual(agg["callsMultiplierMedian"], 30 / 25)
        text, tone = aggregate_line(pe.headline_lines(results, head), "dream", "calls")
        self.assertIn("reached T in 1/3 seeds; single-seed ratio 1.20x fewer calls (not a median)", text)
        self.assertEqual(tone, "warn", "a ratio above 1 in one of three seeds must not take the success colour")
        self.assertNotIn("median 1.20x", text)
        check = pe.check_tables(results)
        self.assertIn("single-seed ratio 1.20x fewer calls (not a median)", check)
        self.assertNotIn("median calls-fewer", check)

    def test_partial_median_above_one_is_warn_and_says_k_of_n(self):
        results = self.load(seed_reaching_early(1), seed_reaching_early(2), seed_never_reaching(3))
        head = pe.headline(results)
        text, tone = aggregate_line(pe.headline_lines(results, head), "dream", "calls")
        self.assertIn("reached T in 2/3 seeds; median of the 2 defined ratios 1.20x fewer calls", text)
        self.assertEqual(tone, "warn")

    def test_partial_median_below_one_is_bad(self):
        results = self.load(seed_reaching_late(1), seed_reaching_late(2), seed_never_reaching(3))
        head = pe.headline(results)
        self.assertAlmostEqual(head["aggregate"]["dream"]["callsMultiplierMedian"], 30 / 45)
        text, tone = aggregate_line(pe.headline_lines(results, head), "dream", "calls")
        self.assertIn("reached T in 2/3 seeds; median of the 2 defined ratios 1.50x MORE calls", text)
        self.assertNotIn("0.67x fewer", text)
        self.assertEqual(tone, "bad", "the tone follows the median ratio itself, not the inverse printed")

    def test_ratio_defined_in_every_seed_keeps_the_median_and_its_tone(self):
        results = self.load(seed_reaching_early(1), seed_reaching_early(2))
        head = pe.headline(results)
        text, tone = aggregate_line(pe.headline_lines(results, head), "dream", "calls")
        self.assertIn("median 1.20x fewer calls; reached T in 2/2 seeds", text)
        self.assertEqual(tone, "ok")
        results = self.load(seed_reaching_late(4), seed_reaching_late(5))
        text, tone = aggregate_line(pe.headline_lines(results, pe.headline(results)), "dream", "calls")
        self.assertIn("median 1.50x MORE calls; reached T in 2/2 seeds", text)
        self.assertEqual(tone, "bad")

    def test_ratio_defined_nowhere_says_so_in_words(self):
        results = self.load(seed_never_reaching(1), seed_never_reaching(2))
        head = pe.headline(results)
        agg = head["aggregate"]["dream"]
        self.assertEqual(agg["callsMultiplierDefined"], 0)
        self.assertIsNone(agg["callsMultiplierMedian"])
        text, tone = aggregate_line(pe.headline_lines(results, head), "dream", "calls")
        self.assertEqual(text.strip(), "dream: reached T in 0/2 seeds; no calls ratio is defined")
        self.assertEqual(tone, "warn")
        single = self.load(seed_never_reaching(3))
        check = pe.check_tables(single)
        self.assertIn("dream: reached T in 0/1 seeds; no calls ratio is defined", check)
        self.assertNotIn("median", check.split("across 1 seed(s)")[1].split("seed 3: headline")[0])

    def test_single_seed_score_ratio_is_not_called_a_median(self):
        results = self.load(seed_never_reaching(1))
        check = pe.check_tables(results)
        self.assertIn("(one seed); comparable at B in 1/1 seeds", check)

    def test_ratio_fmt_never_prints_a_ratio_that_is_not_one_as_one(self):
        self.assertEqual(pe.ratio_fmt(1.0), "1.00")
        self.assertEqual(pe.ratio_fmt(1.11), "1.11")
        self.assertEqual(pe.ratio_fmt(0.9995568711894772), "0.9996")
        self.assertEqual(pe.ratio_fmt(1.004), "1.0040")
        self.assertEqual(pe.ratio_fmt(1 + 1e-12), "1.00")

    def test_headline_naming_a_reference_arm_that_did_not_run_is_no_headline(self):
        only_dream = result(
            3,
            [arm("dream", rows([1.0, 1.1, 1.2], [5, 5, 5], [POLICY_A, POLICY_B, POLICY_B]))],
            headline={
                "reference": "fixed",
                "target": 1.3,
                "equalBudget": 15,
                "probesToTarget": {"dream": None, "fixed": 15},
                "callsMultiplier": {"dream": None, "fixed": 1},
                "bestAtBudget": {"dream": 1.2, "fixed": 1.3},
                "scoreMultiplier": {"dream": 1.2 / 1.3, "fixed": 1},
                "deltaBest": {"dream": -0.1, "fixed": 0},
            },
        )
        results = self.load(only_dream)
        self.assertIsNone(results[0]["headline"])
        text = pe.check_tables(results)
        self.assertIn("no control", text)
        lines = pe.headline_lines(results, pe.headline(results))
        self.assertEqual(lines[0][1], "warn")
        self.assertEqual(pe.budget_meaning(results), "as recorded in each file")

    def test_malformed_optional_fields_never_raise(self):
        bad = seed1()
        bad["budget"] = 3
        bad["notes"] = "not a list"
        bad["objective"] = []
        bad["n"] = True
        bad["seed"] = False
        bad["sharedInitialRollout"] = "yes"
        bad["headline"] = {
            "reference": 7,
            "target": "1.3",
            "equalBudget": "39",
            "probesToTarget": 5,
            "callsMultiplier": "x",
            "bestAtBudget": None,
            "scoreMultiplier": [],
            "deltaBest": 3,
            "multipliers": {"dream": 4},
        }
        fixed_arm, dream_arm, guided_arm = bad["arms"]
        fixed_arm["totals"] = 5
        fixed_arm["policyScoreOnOwnPool"] = "x"
        fixed_arm["policyChanges"] = True
        fixed_arm["mode"] = "local"
        dream_arm["improved"] = "yes"
        for row in dream_arm["rounds"]:
            row["handlerCalls"] = 7
            row["tokens"] = "a"
            row["dreaming"] = 4
            row["cumulativeBest"] = None
        del guided_arm["rounds"][1]["roundBest"]
        results = self.load(bad)
        r = results[0]
        self.assertIsNone(r["n"])
        self.assertEqual(r["seed"], "?")
        self.assertEqual(r["budget"], {"workers": None, "k1": None, "k2": None, "dreams": None})
        self.assertEqual(r["notes"], [])
        self.assertIsNone(r["objective"])
        self.assertIsNone(r["sharedInitialRollout"])
        fixed = pe.arm_of(r, "fixed")
        self.assertEqual(fixed["proposer"], "local")
        self.assertEqual(fixed["policyChanges"], 0)
        self.assertAlmostEqual(fixed["finalBest"], 1.3521)
        self.assertEqual(fixed["totalProbes"], 45)
        dream = pe.arm_of(r, "dream")
        self.assertEqual([row["handlerCalls"] for row in dream["rounds"]], [0, 0, 0])
        self.assertEqual([row["tokens"] for row in dream["rounds"]], [0, 0, 0])
        self.assertEqual([row["cumulativeBest"] for row in dream["rounds"]], [1.3012, 1.3902, 1.3902])
        self.assertEqual(pe.arm_of(r, "dream-guided")["rounds"][1]["roundBest"], 0.0)
        h = r["headline"]
        assert h is not None
        self.assertEqual(h["reference"], "fixed")
        self.assertAlmostEqual(h["target"], 1.3521, msg="a non-numeric target falls back to the reference final best")
        self.assertEqual(h["equalBudget"], 39, "a non-numeric equalBudget falls back to the smallest arm total")
        self.assertEqual(set(h["callsMultiplier"].values()), {None})
        self.assertEqual(set(h["scoreMultiplier"].values()), {None})
        self.assertEqual(set(h["probesToTarget"].values()), {None})
        self.assertEqual(set(h["bestAtBudget"].values()), {None})
        self.assertAlmostEqual(h["deltaBest"]["dream"], 0.0381)
        self.assertEqual(pe.multiplier_text("calls", h, "dream"), "not reached")
        self.assertEqual(pe.multiplier_text("score", h, "dream"), "not comparable")
        check = pe.check_tables(results)
        self.assertIn("agrees with the file: NO", check)
        for text, tone in pe.headline_lines(results, pe.headline(results)):
            self.assertIn(tone, {"ok", "bad", "warn", "value", "muted"}, text)

    def test_help_works_when_docstrings_are_stripped(self):
        proc = subprocess.run(
            [sys.executable, "-OO", str(HERE / "plot_experiment.py"), "--help"],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("result.json", proc.stdout)


class Direction(unittest.TestCase):
    """A ratio below 1 reads the right way round: `1.20x MORE calls (72 vs 60)`, never `0.83x fewer`."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()

    def tearDown(self):
        self.tmp.cleanup()

    def load(self, *payloads):
        paths = [write(self.tmp.name, f"s{p['seed']}.json", p) for p in payloads]
        return pe.load_results(paths)

    def test_calls_ratio_below_one_reads_as_more_calls_with_arm_vs_reference_operands(self):
        results = self.load(seed_more_calls(1))
        h = results[0]["headline"]
        assert h is not None
        self.assertEqual(h["probesToTarget"], {"fixed": 60, "dream": 72})
        self.assertAlmostEqual(h["callsMultiplier"]["dream"], 60 / 72)
        self.assertEqual(pe.multiplier_text("calls", h, "dream"), "1.20x MORE calls (72 vs 60)")
        self.assertEqual(pe.ratio_tone(h["callsMultiplier"]["dream"]), "bad", "the tone still follows the raw ratio")
        self.assertEqual(pe.multiplier_text("calls", h, "fixed"), "1.00x fewer calls (60 vs 60)")

    def test_score_ratio_below_one_reads_as_lower_score(self):
        results = self.load(seed_more_calls(1))
        h = results[0]["headline"]
        assert h is not None
        self.assertEqual(h["equalBudget"], 60)
        self.assertAlmostEqual(h["scoreMultiplier"]["dream"], 1.31 / 1.35)
        self.assertEqual(pe.multiplier_text("score", h, "dream"), "1.03x LOWER score at budget 60 (1.3100 vs 1.3500)")
        above = self.load(seed_reaching_early(2))[0]["headline"]
        assert above is not None
        self.assertEqual(
            pe.multiplier_text("score", above, "dream"), "1.01x higher score at budget 35 (1.3600 vs 1.3500)"
        )

    def test_ratio_words_invert_below_one_and_never_print_one_for_not_one(self):
        self.assertEqual(pe.ratio_words("calls", 30 / 27), "1.11x fewer calls")
        self.assertEqual(pe.ratio_words("calls", 60 / 72), "1.20x MORE calls")
        self.assertEqual(pe.ratio_words("calls", 1.0), "1.00x fewer calls")
        self.assertEqual(pe.ratio_words("score", 1.31 / 1.3521), "1.03x LOWER score")
        self.assertEqual(pe.ratio_words("score", 1.3902 / 1.3521), "1.03x higher score")
        self.assertEqual(pe.ratio_words("score", 0.9995568711894772), "1.0004x LOWER score")
        self.assertEqual(pe.ratio_words("score", 1 - 1e-12), "1.00x higher score")
        self.assertEqual(pe.ratio_words("score", 0.0), "score ratio 0.00 (not positive)")

    def test_no_inverted_wording_anywhere_on_the_page(self):
        results = self.load(seed1(), seed2())
        check = pe.check_tables(results)
        self.assertIsNone(INVERTED_WORDING.search(check), check)
        self.assertIn("1.03x LOWER score at budget 39 (1.3100 vs 1.3521)", check)
        self.assertIn("2.00x MORE calls (90 vs 45)", check)
        card = "\n".join(text for text, _tone in pe.headline_lines(results, pe.headline(results)))
        self.assertIsNone(INVERTED_WORDING.search(card), card)
        self.assertIn("dream-guided: 1.03x LOWER score at budget 39 (1.3100 vs 1.3521)", card)

    def test_single_seed_headline_below_one_stays_a_single_seed_headline(self):
        results = self.load(seed_more_calls(1))
        check = pe.check_tables(results)
        self.assertIn("dream: 1.20x MORE calls (72 vs 60); 1.03x LOWER score at budget 60 (1.3100 vs 1.3500)", check)
        self.assertIn("dream: 1.20x MORE calls (one seed); reached T in 1/1 seeds", check)
        self.assertIn("dream: 1.03x LOWER score at B (one seed); comparable at B in 1/1 seeds", check)
        self.assertNotIn("median", check.split("across 1 seed(s)")[1].split("seed 1: headline")[0])
        lines = pe.headline_lines(results, pe.headline(results))
        self.assertNotIn("across", "\n".join(text for text, _tone in lines), "one seed has no across-seeds block")
        tones = {text.strip(): tone for text, tone in lines}
        self.assertEqual(tones["dream: 1.20x MORE calls (72 vs 60)"], "bad")
        self.assertEqual(tones["dream: 1.03x LOWER score at budget 60 (1.3100 vs 1.3500)"], "bad")

    def test_single_seed_ratio_below_one_across_seeds_is_not_a_median(self):
        results = self.load(seed_more_calls(1), seed_never_reaching(2), seed_never_reaching(3))
        text, tone = aggregate_line(pe.headline_lines(results, pe.headline(results)), "dream", "calls")
        self.assertIn("reached T in 1/3 seeds; single-seed ratio 1.20x MORE calls (not a median)", text)
        self.assertEqual(tone, "bad")


class Provenance(unittest.TestCase):
    """Agent-generated calls, local fallbacks and the proposal tally: read when present, "not recorded" when not."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()

    def tearDown(self):
        self.tmp.cleanup()

    def load(self, *payloads):
        paths = [write(self.tmp.name, f"s{p['seed']}.json", p) for p in payloads]
        return pe.load_results(paths)

    def test_legacy_file_reads_as_not_recorded_never_zero(self):
        results = self.load(seed1())
        for a in results[0]["arms"]:
            self.assertFalse(pe.provenance_recorded(a))
            for key in ("totalAgentGeneratedCalls", "totalLocalFallbacks", "totalLlmProposals", "totalLlmAccepted"):
                self.assertIsNone(a[key], key)
            self.assertIsNone(a["totalLlmRejected"])
            for row in a["rounds"]:
                for key in ("agentGeneratedCalls", "cumulativeAgentGeneratedCalls", "localFallbacks", "llmProposals"):
                    self.assertIsNone(row[key], key)
                self.assertIsNone(row["llmRejected"])
        self.assertEqual(pe.compute_axis(results), ("probes", "not recorded (result predates origin tracking)"))
        ser = pe.series(results)
        self.assertEqual(ser["dream"]["agentGeneratedCalls"]["mean"], [None, None, None])
        self.assertEqual(ser["dream"]["llmRejected"]["parse"]["mean"], [None, None, None])
        self.assertEqual(ser["dream"]["provenanceRecorded"], [False])
        check = pe.check_tables(results)
        self.assertIn("provenance: not recorded (result predates origin tracking)", check)
        self.assertIn("compute axis: probes; agent-generated calls not recorded", check)
        self.assertNotIn("tally consistent", check)
        lines = pe.provenance_lines(results)
        self.assertTrue(all(tone == "muted" for _text, tone in lines), lines)
        self.assertTrue(lines[0][0].startswith("compute axis: probes; agent-generated calls not recorded"))
        self.assertNotIn("agent-generated +", "\n".join(text for text, _tone in lines))

    def test_provenance_fields_are_read_summed_and_put_on_the_compute_axis(self):
        results = self.load(seed_llm(1))
        dream = pe.arm_of(results[0], "dream")
        self.assertTrue(pe.provenance_recorded(dream))
        self.assertEqual([r["probes"] for r in dream["rounds"]], [15, 10, 10])
        self.assertEqual([r["agentGeneratedCalls"] for r in dream["rounds"]], [0, 2, 1])
        self.assertEqual([r["cumulativeAgentGeneratedCalls"] for r in dream["rounds"]], [0, 2, 3])
        self.assertEqual([r["localFallbacks"] for r in dream["rounds"]], [15, 8, 9])
        self.assertEqual([r["llmProposals"] for r in dream["rounds"]], [15, 10, 10])
        self.assertEqual([r["llmAccepted"] for r in dream["rounds"]], [0, 2, 1])
        self.assertEqual(dream["rounds"][2]["llmRejected"]["error"], 2)
        self.assertEqual(dream["rounds"][2]["llmRejected"]["budget"], 0, "every known reason is present, 0 when unseen")
        self.assertEqual(dream["totalProbes"], 35)
        self.assertEqual(dream["totalAgentGeneratedCalls"], 3)
        self.assertEqual(dream["totalLocalFallbacks"], 32)
        self.assertEqual(dream["totalLlmProposals"], 35)
        self.assertEqual(dream["totalLlmAccepted"], 3)
        self.assertEqual(dream["totalLlmRejected"]["parse"], 24)
        self.assertEqual(sum(dream["totalLlmRejected"].values()), 32)
        self.assertTrue(pe.tally_consistent(dream))
        self.assertEqual(
            pe.provenance_text(dream),
            "35 probes = 3 agent-generated + 32 local (32 fallbacks); "
            "35 LLM proposals = 3 accepted + 32 rejected (parse 24, shape 6, error 2)",
        )
        self.assertEqual(pe.provenance_tone(dream), "warn", "fallbacks outnumber the agent's candidates")
        kind, why = pe.compute_axis(results)
        self.assertEqual(kind, "agentGenerated")
        self.assertIn("probes include the local fallbacks", why)
        ser = pe.series(results)
        self.assertEqual(ser["dream"]["cumulativeAgentGeneratedCalls"]["mean"], [0.0, 2.0, 3.0])
        self.assertEqual(ser["dream"]["llmRejected"]["parse"]["mean"], [11.0, 6.0, 7.0])
        self.assertEqual(ser["dream"]["llmRejected"]["error"]["mean"], [0.0, 0.0, 2.0])
        self.assertEqual(ser["fixed"]["totalLlmAccepted"], [1])
        # The headline is unchanged by provenance: it stays on probes.
        h = results[0]["headline"]
        assert h is not None
        self.assertEqual(h["probesToTarget"], {"fixed": 30, "dream": 25})
        self.assertEqual(h["equalBudget"], 35)
        check = pe.check_tables(results)
        self.assertIn("compute axis: agent-generated calls, the candidates a child agent produced", check)
        self.assertIn("provenance (recorded in 1/1 seeds; means over those)", check)
        self.assertIn("| parse 7, error 2", check)
        self.assertIn(
            "seed 1: 35 probes = 3 agent-generated + 32 local (32 fallbacks); 35 LLM proposals = 3 accepted + "
            "32 rejected (parse 24, shape 6, error 2); tally consistent (proposals = accepted + rejected): yes",
            check,
        )
        lines = pe.provenance_lines(results)
        tones = {text.strip(): tone for text, tone in lines}
        self.assertEqual(tones["dream: 35 probes = 3 agent-generated + 32 local (32 fallbacks)"], "warn")
        self.assertEqual(
            tones["dream: 35 LLM proposals = 3 accepted + 32 rejected (parse 24, shape 6, error 2)"], "warn"
        )

    def test_totals_and_cumulatives_are_derived_when_the_file_omits_them(self):
        payload = seed_llm(1)
        for a in payload["arms"]:
            for key in ("agentGeneratedCalls", "localFallbacks", "llmProposals", "llmAccepted", "llmRejected"):
                del a["totals"][key]
        dream_rows = payload["arms"][1]["rounds"]
        for r in dream_rows:
            del r["cumulativeAgentGeneratedCalls"]
        fixed_rows = payload["arms"][0]["rounds"]
        for r in fixed_rows:
            del r["agentGeneratedCalls"]
        results = self.load(payload)
        dream = pe.arm_of(results[0], "dream")
        self.assertEqual([r["cumulativeAgentGeneratedCalls"] for r in dream["rounds"]], [0, 2, 3])
        self.assertEqual(dream["totalAgentGeneratedCalls"], 3)
        self.assertEqual(dream["totalLocalFallbacks"], 32)
        self.assertEqual(dream["totalLlmProposals"], 35)
        self.assertEqual(dream["totalLlmRejected"]["parse"], 24)
        fixed = pe.arm_of(results[0], "fixed")
        self.assertEqual([r["agentGeneratedCalls"] for r in fixed["rounds"]], [0, 1, 0], "from the cumulative")
        self.assertEqual(fixed["totalAgentGeneratedCalls"], 1)
        canonical = pe.load_results([write(self.tmp.name, "canon.json", seed_llm(1))])[0]
        self.assertEqual(results[0]["arms"], canonical["arms"])

    def test_an_unlisted_reject_reason_is_kept_and_printed(self):
        payload = seed_llm(1)
        dream_rows = payload["arms"][1]["rounds"]
        dream_rows[1]["llmRejected"]["weird"] = 2
        dream_rows[1]["llmProposals"] += 2
        payload["arms"][1]["totals"]["llmRejected"]["weird"] = 2
        payload["arms"][1]["totals"]["llmProposals"] += 2
        results = self.load(payload)
        self.assertEqual(pe.reasons_seen(results), [*pe.REJECT_REASONS, "weird"])
        dream = pe.arm_of(results[0], "dream")
        self.assertEqual(dream["totalLlmRejected"]["weird"], 2)
        self.assertTrue(pe.tally_consistent(dream))
        self.assertIn("(parse 24, shape 6, error 2, weird 2)", pe.provenance_text(dream))
        self.assertEqual(pe.series(results)["dream"]["llmRejected"]["weird"]["mean"], [0.0, 2.0, 0.0])
        self.assertEqual(pe.reason_grey("weird", pe.reasons_seen(results)), pe.EXTRA_GREYS[0])
        self.assertEqual(pe.reason_grey("parse", pe.reasons_seen(results)), pe.REASON_GREYS["parse"])

    def test_a_legacy_seed_pools_with_a_provenance_seed_without_inventing_zeros(self):
        results = self.load(seed_reaching_early(1), seed_llm(2))
        kind, why = pe.compute_axis(results)
        self.assertEqual((kind, why), ("probes", "recorded in 2/4 arm records only"))
        ser = pe.series(results)
        self.assertEqual(ser["dream"]["provenanceRecorded"], [False, True])
        self.assertEqual(ser["dream"]["agentGeneratedCalls"]["perSeed"], [[None, None, None], [0.0, 2.0, 1.0]])
        self.assertEqual(
            ser["dream"]["agentGeneratedCalls"]["mean"], [0.0, 2.0, 1.0], "mean over the recorded seed only"
        )
        self.assertEqual(ser["dream"]["totalLlmProposals"], [None, 35])
        check = pe.check_tables(results)
        self.assertIn("provenance (recorded in 1/2 seeds; means over those)", check)
        self.assertIn("seed 1: provenance not recorded", check)
        self.assertIn("seed 2: 35 probes = 3 agent-generated + 32 local (32 fallbacks)", check)
        self.assertIn("compute axis: probes; agent-generated calls recorded in 2/4 arm records only", check)
        lines = pe.provenance_lines(results)
        texts = [text.strip() for text, _tone in lines]
        self.assertIn("seed 2 dream: 35 probes = 3 agent-generated + 32 local (32 fallbacks)", texts)
        self.assertFalse(any(t.startswith("seed 1 ") for t in texts), "the legacy seed gets no provenance line")
        # The headline is the same as for two legacy seeds: provenance never touches it.
        agg = pe.headline(results)["aggregate"]["dream"]
        self.assertEqual((agg["n"], agg["reached"]), (2, 2))
        self.assertAlmostEqual(agg["callsMultiplierMedian"], 30 / 25)

    def test_local_path_with_recorded_zero_provenance_keeps_probes_on_the_axis(self):
        results = self.load(seed_local_with_provenance(1))
        dream = pe.arm_of(results[0], "dream")
        self.assertTrue(pe.provenance_recorded(dream))
        self.assertEqual(dream["totalAgentGeneratedCalls"], 0)
        kind, why = pe.compute_axis(results)
        self.assertEqual(kind, "probes")
        self.assertIn("no child proposer ran", why)
        self.assertEqual(
            pe.provenance_text(dream),
            "35 probes = 0 agent-generated + 35 local (0 fallbacks); 0 LLM proposals = 0 accepted + 0 rejected (none)",
        )
        self.assertEqual(pe.provenance_tone(dream), "muted")
        self.assertTrue(pe.tally_consistent(dream))
        check = pe.check_tables(results)
        self.assertIn("compute axis: probes; agent-generated calls 0 (no child proposer ran", check)
        self.assertIn("| none", check)

    def test_a_tally_that_does_not_add_up_is_flagged_bad(self):
        payload = seed_llm(1)
        payload["arms"][1]["totals"]["llmProposals"] = 40
        results = self.load(payload)
        dream = pe.arm_of(results[0], "dream")
        self.assertEqual(dream["totalLlmProposals"], 40, "the file's total is taken as written")
        self.assertFalse(pe.tally_consistent(dream))
        self.assertEqual(pe.provenance_tone(dream), "bad")
        self.assertIn("tally consistent (proposals = accepted + rejected): NO", pe.check_tables(results))
        bad = [text for text, tone in pe.provenance_lines(results) if tone == "bad"]
        self.assertTrue(any("does NOT add up" in text for text in bad), bad)
        over = seed_llm(2)
        over["arms"][1]["totals"]["agentGeneratedCalls"] = 99
        results = self.load(over)
        self.assertFalse(pe.tally_consistent(pe.arm_of(results[0], "dream")), "more agent-generated than probes")

    def test_malformed_provenance_fields_read_as_not_recorded(self):
        payload = seed_llm(1)
        dream_arm = payload["arms"][1]
        dream_arm["totals"]["agentGeneratedCalls"] = "3"
        dream_arm["totals"]["llmRejected"] = "no"
        dream_arm["totals"]["llmProposals"] = True
        for r in dream_arm["rounds"]:
            r["agentGeneratedCalls"] = "x"
            r["cumulativeAgentGeneratedCalls"] = None
            r["localFallbacks"] = [1]
            r["llmProposals"] = True
            r["llmAccepted"] = {"n": 1}
            r["llmRejected"] = 5
        results = self.load(payload)
        dream = pe.arm_of(results[0], "dream")
        self.assertFalse(pe.provenance_recorded(dream))
        for row in dream["rounds"]:
            self.assertIsNone(row["agentGeneratedCalls"])
            self.assertIsNone(row["localFallbacks"])
            self.assertIsNone(row["llmProposals"])
            self.assertIsNone(row["llmAccepted"])
            self.assertIsNone(row["llmRejected"])
        self.assertIsNone(dream["totalLlmProposals"])
        self.assertIsNone(dream["totalLlmRejected"])
        self.assertEqual(pe.provenance_text(dream), "provenance not recorded (result predates origin tracking)")
        self.assertEqual(pe.compute_axis(results), ("probes", "recorded in 1/2 arm records only"))
        self.assertTrue(pe.provenance_recorded(pe.arm_of(results[0], "fixed")))
        half = dict.fromkeys(pe.REJECT_REASONS, 0)
        half["parse"] = "many"
        self.assertEqual(pe._reject_counts(half)["parse"], 0, "a non-numeric count is absent, the reason stays")
        self.assertIsNone(pe._reject_counts([]))
        pe.check_tables(results)
        for _text, tone in pe.provenance_lines(results):
            self.assertIn(tone, {"muted", "warn", "bad"})

    def test_check_cli_on_a_provenance_file_exits_zero_and_names_the_axis(self):
        path = write(self.tmp.name, "llm.json", seed_llm(1))
        proc = subprocess.run(
            [sys.executable, str(HERE / "plot_experiment.py"), path, "--check"],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("compute axis: agent-generated calls", proc.stdout)
        self.assertIn("3 accepted + 32 rejected (parse 24, shape 6, error 2)", proc.stdout)


class Verdict(unittest.TestCase):
    """The noise-floor rule: the fixed arm's own seed-to-seed spread is the floor a paired delta must clear."""

    tmp: tempfile.TemporaryDirectory[str]

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()

    def tearDown(self):
        self.tmp.cleanup()

    def load(self, *payloads):
        paths = [write(self.tmp.name, f"s{p['seed']}.json", p) for p in payloads]
        return pe.load_results(paths)

    def test_one_seed_gives_no_verdict_and_says_so_everywhere(self):
        results = self.load(seed_paired(1, 1.35, 1.40))
        head = pe.headline(results)
        effect = head["paired"]["dream"]
        self.assertEqual(effect["verdict"], pe.VERDICT_SINGLE)
        self.assertEqual(len(effect["deltas"]), 1)
        self.assertAlmostEqual(num(effect["deltas"][0]), 0.05)
        self.assertEqual((effect["positive"], effect["negative"], effect["inertSeeds"]), (1, 0, 0))
        floor = head["noiseFloor"]
        assert floor is not None
        self.assertEqual(floor["n"], 1)
        self.assertIsNone(floor["finalBest"]["std"], "a sample std needs two seeds")
        self.assertAlmostEqual(num(floor["finalBest"]["spread"]), 0.0)
        card = pe.headline_lines(results, head)
        tones = {text.strip(): tone for text, tone in card}
        self.assertEqual(tones["dream: verdict: single seed: no verdict"], "warn")
        self.assertIn("noise floor: one seed; the fixed arm's seed-to-seed spread cannot be measured", tones)
        self.assertNotIn("across", "\n".join(text for text, _tone in card))
        check = pe.check_tables(results)
        self.assertIn("dream: verdict: single seed: no verdict", check)
        self.assertIn("dream: paired delta final best vs fixed per seed [+0.0500] (1 positive, 0 negative)", check)
        self.assertIn("dream: dreaming ran 2 step(s), accepted a candidate in 2, policy changes 1", check)

    def test_exceeds_needs_the_mean_delta_above_the_spread_and_one_sign_in_every_seed(self):
        # fixed finals 1.35 / 1.36 / 1.34: spread 0.02, sample std 0.01; dream = fixed + 0.05 in every seed.
        results = self.load(seed_paired(1, 1.35, 1.40), seed_paired(2, 1.36, 1.41), seed_paired(3, 1.34, 1.39))
        head = pe.headline(results)
        floor = head["noiseFloor"]
        assert floor is not None
        self.assertEqual(floor["n"], 3)
        self.assertEqual(floor["finalBest"]["values"], [1.35, 1.36, 1.34])
        self.assertAlmostEqual(num(floor["finalBest"]["min"]), 1.34)
        self.assertAlmostEqual(num(floor["finalBest"]["max"]), 1.36)
        self.assertAlmostEqual(num(floor["finalBest"]["spread"]), 0.02)
        self.assertAlmostEqual(num(floor["finalBest"]["std"]), statistics.stdev([1.35, 1.36, 1.34]))
        self.assertEqual(floor["probesToTarget"]["values"], [30.0, 30.0, 30.0])
        self.assertAlmostEqual(num(floor["probesToTarget"]["spread"]), 0.0)
        self.assertIsNone(floor["probesToTargetExact"], "no improvements curve: the exact floor is not recorded")
        effect = head["paired"]["dream"]
        for d in effect["deltas"]:
            self.assertAlmostEqual(num(d), 0.05)
        self.assertAlmostEqual(num(effect["mean"]), 0.05)
        self.assertEqual((effect["positive"], effect["negative"]), (3, 0))
        self.assertEqual(effect["callsDeltas"], [5, 5, 5], "dream reaches T at 35 probes, fixed at 30, in every seed")
        self.assertEqual(effect["verdict"], pe.VERDICT_EXCEEDS)
        self.assertEqual(pe.verdict_tone(effect), "ok")
        check = pe.check_tables(results)
        self.assertIn(
            "noise floor (fixed arm across 3 seeds): final best [1.3500, 1.3600, 1.3400] 1.3400..1.3600 "
            "(spread 0.0200, std 0.0100); probes to T [30, 30, 30] 30..30 (spread 0, std 0)",
            check,
        )
        self.assertIn(
            "dream: paired delta final best vs fixed per seed [+0.0500, +0.0500, +0.0500], mean +0.0500", check
        )
        self.assertIn("dream: verdict: exceeds noise floor", check)
        card = pe.headline_lines(results, head)
        tones = {text.strip(): tone for text, tone in card}
        self.assertEqual(tones["dream: verdict: exceeds noise floor"], "ok")

    def test_a_negative_effect_that_exceeds_the_floor_is_bad_toned(self):
        results = self.load(seed_paired(1, 1.40, 1.35), seed_paired(2, 1.41, 1.36))
        effect = pe.headline(results)["paired"]["dream"]
        self.assertAlmostEqual(num(effect["mean"]), -0.05)
        self.assertEqual(effect["verdict"], pe.VERDICT_EXCEEDS)
        self.assertEqual(pe.verdict_tone(effect), "bad")
        self.assertEqual(effect["callsDeltas"], [None, None], "dream never reaches T: the calls delta is undefined")

    def test_mixed_signs_are_within_the_floor_whatever_the_mean(self):
        results = self.load(seed_paired(1, 1.35, 1.45), seed_paired(2, 1.36, 1.46), seed_paired(3, 1.34, 1.32))
        effect = pe.headline(results)["paired"]["dream"]
        self.assertGreater(abs(num(effect["mean"])), 0.02, "the mean clears the spread, the sign does not")
        self.assertEqual((effect["positive"], effect["negative"]), (2, 1))
        self.assertEqual(effect["verdict"], pe.VERDICT_WITHIN)
        self.assertEqual(pe.verdict_tone(effect), "warn")

    def test_a_mean_delta_inside_the_spread_is_within_the_floor(self):
        # deltas +0.01 in every seed against a fixed spread of 0.02.
        results = self.load(seed_paired(1, 1.35, 1.36), seed_paired(2, 1.36, 1.37), seed_paired(3, 1.34, 1.35))
        effect = pe.headline(results)["paired"]["dream"]
        self.assertEqual((effect["positive"], effect["negative"]), (3, 0))
        self.assertAlmostEqual(num(effect["mean"]), 0.01)
        self.assertEqual(effect["verdict"], pe.VERDICT_WITHIN)

    def test_inert_dreaming_forces_within_the_floor_whatever_the_numbers_say(self):
        results = self.load(seed_paired(1, 1.35, 1.45, changes=False), seed_paired(2, 1.36, 1.46, changes=False))
        head = pe.headline(results)
        summaries = head["dreaming"]["dream"]
        self.assertEqual([s["inert"] for s in summaries], [True, True])
        self.assertEqual([s["phases"] for s in summaries], [2, 2])
        self.assertEqual([s["improved"] for s in summaries], [0, 0])
        self.assertEqual([s["auditRecorded"] for s in summaries], [0, 0])
        self.assertEqual([s["inert"] for s in head["dreaming"]["fixed"]], [False, False], "a fixed arm is never inert")
        effect = head["paired"]["dream"]
        self.assertAlmostEqual(num(effect["mean"]), 0.10, msg="the numbers alone would exceed the floor")
        self.assertEqual(effect["inertSeeds"], 2)
        self.assertEqual(effect["verdict"], pe.VERDICT_INERT)
        card = pe.headline_lines(results, head)
        tones = {text.strip(): tone for text, tone in card}
        self.assertEqual(tones["dream: verdict: within noise floor (dreaming inert)"], "warn")
        self.assertEqual(
            tones[
                "dream: policy never changed in 2/2 seed(s): dreaming inert, the arms grew every tree with one policy"
            ],
            "warn",
        )
        ser = pe.series(results)
        self.assertEqual(ser["dream"]["policyNeverChanged"], [True, True])
        self.assertEqual(ser["fixed"]["policyNeverChanged"], [False, False])
        check = pe.check_tables(results)
        self.assertIn("dream: verdict: within noise floor (dreaming inert)", check)
        self.assertIn("dreaming of dream (4 step(s) over 2 seed(s); per-candidate audit recorded in 0/4", check)
        self.assertIn("per-candidate scores not recorded (result predates the audit)", check)

    def test_inert_in_one_seed_only_is_reported_but_not_forced(self):
        results = self.load(seed_paired(1, 1.35, 1.45, changes=False), seed_paired(2, 1.36, 1.46))
        head = pe.headline(results)
        effect = head["paired"]["dream"]
        self.assertEqual(effect["inertSeeds"], 1)
        self.assertEqual(effect["verdict"], pe.VERDICT_EXCEEDS)
        texts = [text.strip() for text, _tone in pe.headline_lines(results, head)]
        self.assertIn(
            "dream: policy never changed in 1/2 seed(s): dreaming inert, the arms grew every tree with one policy",
            texts,
        )

    def test_no_reference_arm_means_no_verdict_lines(self):
        only_dream = result(3, [arm("dream", rows([1.0, 1.1, 1.2], [5, 5, 5], [POLICY_A, POLICY_B, POLICY_B]))])
        results = self.load(only_dream)
        head = pe.headline(results)
        self.assertIsNone(head["noiseFloor"])
        self.assertEqual(head["paired"], {})
        self.assertEqual(pe.comparison_lines(head), [])

    def test_spread_text_words(self):
        self.assertEqual(pe.spread_text(pe.spread_stats([None, None])), "undefined in every seed (0/2)")
        self.assertEqual(pe.spread_text(pe.spread_stats([1.35])), "[1.3500] 1.3500..1.3500 (spread 0)")
        self.assertEqual(
            pe.spread_text(pe.spread_stats([30.0, None, 32.0]), 2),
            "[30, -, 32] 30..32 (spread 2, std 1.41) over 2/3 seeds",
        )


class DreamingAudit(unittest.TestCase):
    """The per-candidate audit of a dreaming step is read when present and reads as not recorded when absent."""

    tmp: tempfile.TemporaryDirectory[str]

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()

    def tearDown(self):
        self.tmp.cleanup()

    def load(self, *payloads):
        paths = [write(self.tmp.name, f"s{p['seed']}.json", p) for p in payloads]
        return pe.load_results(paths)

    def test_candidate_verdicts_dreamer_and_lever_scan_are_read(self):
        results = self.load(seed_audited(1))
        dream = pe.arm_of(results[0], "dream")
        self.assertIsNone(dream["rounds"][0]["dreaming"])
        r2 = dream["rounds"][1]["dreaming"]
        assert r2 is not None
        self.assertEqual(r2["candidates"], 4, "the count stays the number the file has always written")
        self.assertEqual(r2["dreamer"], "mixed")
        self.assertEqual((r2["currentScore"], r2["chosenScore"], r2["improved"]), (1.2, 1.26, True))
        verdicts = r2["candidateVerdicts"]
        assert verdicts is not None
        self.assertEqual([v["reason"] for v in verdicts], ["winner", "worse", "quality-rejected", "duplicate"])
        self.assertEqual([v["eligible"] for v in verdicts], [True, True, False, False])
        self.assertEqual([v["origin"] for v in verdicts], ["llm", "local", "llm", "llm"])
        self.assertEqual(verdicts[3]["duplicateOf"], 0)
        self.assertEqual(verdicts[0]["changed"], ["beta"])
        self.assertEqual((verdicts[0]["N"], verdicts[0]["rounds"], verdicts[0]["outOfSupportCells"]), (12, 5, 0))
        self.assertAlmostEqual(num(verdicts[0]["anytime"]), 1.26)
        scan = r2["leverScan"]
        assert scan is not None
        self.assertEqual((scan["policies"], scan["eligible"], scan["bestValue"], scan["gap"]), (40, 30, 1.27, 0.07))
        self.assertEqual(pe.best_candidate_value(verdicts), 1.26, "the quality-rejected 1.30 never competed")
        self.assertEqual(pe.support_coverage(verdicts), 1.0)
        r3 = dream["rounds"][2]["dreaming"]
        assert r3 is not None and r3["candidateVerdicts"] is not None
        self.assertEqual(pe.support_coverage(r3["candidateVerdicts"]), 0.75, "one of four left the support")
        self.assertEqual(
            pe.best_candidate_value(r3["candidateVerdicts"]), 1.24, "tie and worse are eligible, the rest not"
        )
        summary = pe.dreaming_summary(dream)
        self.assertEqual(summary, {"phases": 2, "improved": 1, "auditRecorded": 2, "policyChanges": 1, "inert": False})
        ser = pe.series(results)["dream"]["dreaming"]
        self.assertEqual(ser["recorded"], [0, 1, 1])
        self.assertEqual(ser["auditRecorded"], [0, 1, 1])
        self.assertEqual(ser["improved"], [0, 1, 0])
        self.assertEqual(ser["dreamers"], ["", "mixed", "llm"])
        self.assertEqual(ser["candidates"]["mean"], [None, 4.0, 4.0])
        self.assertEqual(ser["eligible"]["mean"], [None, 2.0, 2.0])
        self.assertEqual(ser["bestCandidateValue"]["mean"], [None, 1.26, 1.24])
        self.assertEqual(ser["supportCoverage"]["mean"], [None, 1.0, 0.75])
        self.assertEqual(ser["leverGap"]["mean"], [None, 0.07, 0.0])
        self.assertEqual(ser["currentScore"]["mean"], [None, 1.2, 1.25])
        check = pe.check_tables(results)
        self.assertIn("dreaming of dream (2 step(s) over 1 seed(s); per-candidate audit recorded in 2/2)", check)
        self.assertNotIn("predates the audit", check)
        self.assertIn("|   mixed |          4 |        2 |  1.2000 | 1.2600 | 1/1 |    0.0700 | 1", check)
        self.assertIn("|     llm |          4 |        2 |  1.2500 | 1.2500 | 0/1 |         0 | 0.75", check)
        self.assertIn(
            "dream: dreaming ran 2 step(s), accepted a candidate in 1, policy changes 1; per-candidate audit recorded in 2/2 steps",
            check,
        )
        self.assertNotIn("dreaming of fixed", check, "the fixed arm never dreams")

    def test_an_older_file_reads_as_audit_not_recorded_never_empty(self):
        results = self.load(seed1())
        dream = pe.arm_of(results[0], "dream")
        r2 = dream["rounds"][1]["dreaming"]
        assert r2 is not None
        self.assertEqual(r2["candidates"], 4)
        self.assertIsNone(r2["candidateVerdicts"])
        self.assertIsNone(r2["dreamer"])
        self.assertIsNone(r2["leverScan"])
        self.assertEqual(pe.dreaming_summary(dream)["auditRecorded"], 0)
        ser = pe.series(results)["dream"]["dreaming"]
        self.assertEqual(ser["auditRecorded"], [0, 0, 0])
        self.assertEqual(ser["eligible"]["mean"], [None, None, None])
        self.assertEqual(ser["leverGap"]["mean"], [None, None, None])
        self.assertEqual(ser["supportCoverage"]["mean"], [None, None, None])
        check = pe.check_tables(results)
        self.assertIn(
            "per-candidate audit recorded in 0/2; per-candidate scores not recorded (result predates the audit)", check
        )
        self.assertIn("| 1/1 |         - | -", check)

    def test_malformed_and_partial_audit_entries(self):
        payload = seed_audited(1)
        block = payload["arms"][1]["rounds"][1]["dreaming"]
        block["candidateVerdicts"] = [
            {"policyId": "p", "value": 1.1, "reason": "worse"},  # eligible derived from the reason
            {"policyId": "q", "value": "x"},  # no numeric value: dropped
            "not an object",  # dropped
            {"policyId": "r", "value": 1.0, "reason": "made-up", "origin": "elsewhere", "eligible": "yes"},
        ]
        block["dreamer"] = "oracle"
        block["leverScan"] = "none"
        payload["arms"][1]["rounds"][2]["dreaming"]["candidates"] = [{"value": 1.0, "reason": "tie"}]
        del payload["arms"][1]["rounds"][2]["dreaming"]["candidateVerdicts"]
        results = self.load(payload)
        dream = pe.arm_of(results[0], "dream")
        r2 = dream["rounds"][1]["dreaming"]
        assert r2 is not None and r2["candidateVerdicts"] is not None
        self.assertEqual(len(r2["candidateVerdicts"]), 2)
        first, last = r2["candidateVerdicts"]
        self.assertEqual((first["index"], first["origin"], first["eligible"], first["changed"]), (0, "?", True, []))
        self.assertIsNone(first["quality"])
        self.assertEqual((last["index"], last["origin"], last["eligible"], last["reason"]), (3, "?", False, "made-up"))
        self.assertIsNone(r2["dreamer"])
        self.assertIsNone(r2["leverScan"])
        self.assertIsNone(pe.support_coverage(r2["candidateVerdicts"]), "no entry recorded its support")
        r3 = dream["rounds"][2]["dreaming"]
        assert r3 is not None and r3["candidateVerdicts"] is not None
        self.assertEqual(r3["candidates"], 1, "a list under candidates is read as the audit, its length as the count")
        self.assertEqual(r3["candidateVerdicts"][0]["reason"], "tie")
        pe.check_tables(results)

    def test_audit_pools_across_seeds_with_an_older_seed(self):
        results = self.load(seed_audited(1), seed_reaching_early(2))
        ser = pe.series(results)["dream"]["dreaming"]
        self.assertEqual(ser["recorded"], [0, 2, 2])
        self.assertEqual(ser["auditRecorded"], [0, 1, 1])
        self.assertEqual(ser["leverGap"]["perSeed"], [[None, 0.07, 0.0], [None, None, None]])
        self.assertEqual(ser["leverGap"]["mean"], [None, 0.07, 0.0], "the mean is over the seed that recorded it")
        self.assertEqual(ser["dreamers"], ["", "mixed", "llm"])
        check = pe.check_tables(results)
        self.assertIn("dreaming of dream (4 step(s) over 2 seed(s); per-candidate audit recorded in 2/4)", check)
        self.assertIn(
            "dream: dreaming ran 4 step(s), accepted a candidate in 3, policy changes 2 (totals over 2 seeds); per-candidate audit recorded in 2/4 steps",
            check,
        )


class ExactHeadline(unittest.TestCase):
    """probesToTargetExact counts to the first PROBE reaching T; a file without the curve reads not recorded."""

    tmp: tempfile.TemporaryDirectory[str]

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()

    def tearDown(self):
        self.tmp.cleanup()

    def load(self, *payloads):
        paths = [write(self.tmp.name, f"s{p['seed']}.json", p) for p in payloads]
        return pe.load_results(paths)

    def test_exact_probes_are_recomputed_from_the_improvements_curve(self):
        results = self.load(seed_exact(1))
        r = results[0]
        fixed, dream = pe.arm_of(r, "fixed"), pe.arm_of(r, "dream")
        self.assertEqual(
            [row["improvements"] for row in fixed["rounds"]], [[(1, 1.30)], [(3, 1.33), (7, 1.35)], [(2, 1.32)]]
        )
        self.assertEqual([row["probesToRoundBest"] for row in dream["rounds"]], [1, 4, 0])
        self.assertTrue(pe.improvements_recorded(fixed))
        h = r["headline"]
        assert h is not None
        self.assertEqual(h["probesToTarget"], {"fixed": 30, "dream": 25}, "the rollout-granular headline is unchanged")
        self.assertEqual(h["probesToTargetExact"], {"fixed": 22, "dream": 19})
        assert h["callsMultiplierExact"] is not None
        self.assertAlmostEqual(num(h["callsMultiplierExact"]["dream"]), 22 / 19)
        self.assertEqual(h["callsMultiplierExact"]["fixed"], 1.0)
        self.assertEqual(pe.exact_calls_text(h, "dream"), "exact 1.16x fewer calls (19 vs 22)")
        self.assertEqual(pe.multiplier_text("calls", h, "dream"), "1.20x fewer calls (25 vs 30)")
        check = pe.check_tables(results)
        self.assertIn(
            "1.20x fewer calls (25 vs 30); 1.01x higher score at budget 35 (1.3600 vs 1.3500); "
            "+0.0100 delta final best vs fixed (T = 1.3500); exact 1.16x fewer calls (19 vs 22)",
            check,
        )
        tones = {text.strip(): tone for text, tone in pe.headline_lines(results, pe.headline(results))}
        self.assertEqual(tones["dream: exact 1.16x fewer calls (19 vs 22)"], "ok")
        ser = pe.series(results)
        self.assertEqual(ser["dream"]["probesToRoundBest"]["mean"], [1.0, 4.0, 0.0])

    def test_the_files_own_exact_numbers_win_over_the_recomputation(self):
        payload = seed_exact(1)
        payload["headline"]["probesToTargetExact"] = {"fixed": 22, "dream": 20}
        payload["headline"]["callsMultiplierExact"] = {"fixed": 1, "dream": 1.1}
        h = self.load(payload)[0]["headline"]
        assert h is not None
        self.assertEqual(h["probesToTargetExact"], {"fixed": 22, "dream": 20})
        assert h["callsMultiplierExact"] is not None
        self.assertEqual(h["callsMultiplierExact"]["dream"], 1.1)
        self.assertEqual(pe.exact_calls_text(h, "dream"), "exact 1.10x fewer calls (20 vs 22)")
        without_ratio = seed_exact(2)
        without_ratio["headline"]["probesToTargetExact"] = {"fixed": 22, "dream": None}
        h2 = self.load(without_ratio)[0]["headline"]
        assert h2 is not None
        self.assertEqual(h2["probesToTargetExact"], {"fixed": 22, "dream": None})
        assert h2["callsMultiplierExact"] is not None
        self.assertIsNone(h2["callsMultiplierExact"]["dream"])
        self.assertEqual(pe.exact_calls_text(h2, "dream"), "exact: not reached")
        self.assertEqual(h2["callsMultiplierExact"]["fixed"], 1.0, "recomputed from the file's exact counts")

    def test_a_file_without_the_curve_reads_not_recorded_not_not_reached(self):
        results = self.load(seed_reaching_early(1))
        h = results[0]["headline"]
        assert h is not None
        self.assertIsNone(h["probesToTargetExact"])
        self.assertIsNone(h["callsMultiplierExact"])
        self.assertEqual(pe.exact_calls_text(h, "dream"), pe.EXACT_NOT_RECORDED)
        tones = {text.strip(): tone for text, tone in pe.headline_lines(results, pe.headline(results))}
        self.assertEqual(tones["dream: exact probes to T not recorded"], "muted")
        self.assertIn("exact probes to T not recorded", pe.check_tables(results))
        partial = seed_exact(2)
        for row in partial["arms"][1]["rounds"]:
            del row["improvements"]
        h2 = self.load(partial)[0]["headline"]
        assert h2 is not None
        self.assertIsNone(h2["probesToTargetExact"], "one arm without the curve: the exact headline is not recorded")
        malformed = seed_exact(3)
        malformed["arms"][0]["rounds"][1]["improvements"] = [{"probe": "3", "score": 1.33}]
        h3 = self.load(malformed)[0]["headline"]
        assert h3 is not None
        self.assertIsNone(h3["probesToTargetExact"], "a malformed curve is not recorded")

    def test_never_reaching_t_is_not_reached_in_the_exact_headline_too(self):
        payload = with_improvements(
            seed_never_reaching(1),
            {"fixed": [[(1, 1.30)], [(7, 1.35)], [(2, 1.32)]], "dream": [[(1, 1.30)], [(4, 1.31)], [(3, 1.32)]]},
        )
        h = self.load(payload)[0]["headline"]
        assert h is not None
        self.assertEqual(h["probesToTargetExact"], {"fixed": 22, "dream": None})
        self.assertEqual(pe.exact_calls_text(h, "dream"), "exact: not reached")

    def test_exact_noise_floor_and_paired_calls_deltas_across_seeds(self):
        results = self.load(seed_exact(1), seed_exact(2))
        head = pe.headline(results)
        floor = head["noiseFloor"]
        assert floor is not None and floor["probesToTargetExact"] is not None
        self.assertEqual(floor["probesToTargetExact"]["values"], [22.0, 22.0])
        effect = head["paired"]["dream"]
        self.assertEqual(effect["callsDeltas"], [-5, -5])
        self.assertEqual(effect["callsDeltasExact"], [-3, -3])
        self.assertIn("exact probes to T [22, 22] 22..22 (spread 0, std 0)", pe.check_tables(results))
        mixed_head = pe.headline(self.load(seed_exact(3), seed_reaching_early(4)))
        self.assertIsNone(
            mixed_head["paired"]["dream"]["callsDeltasExact"], "one seed without the curve: no exact deltas"
        )
        mixed_floor = mixed_head["noiseFloor"]
        assert mixed_floor is not None
        self.assertIsNone(mixed_floor["probesToTargetExact"])


class NewFields(unittest.TestCase):
    """stoppedEarly, pool priming, the child mode fields and the k1 note: read when present, None when not."""

    tmp: tempfile.TemporaryDirectory[str]

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()

    def tearDown(self):
        self.tmp.cleanup()

    def load(self, *payloads):
        paths = [write(self.tmp.name, f"s{p['seed']}.json", p) for p in payloads]
        return pe.load_results(paths)

    def test_stopped_early_is_the_files_count_else_derived_from_decision_rounds(self):
        derived = self.load(seed1())[0]
        self.assertEqual([a["stoppedEarly"] for a in derived["arms"]], [0, 0, 0], "decisionRounds 5 against k1 5")
        payload = seed1()
        payload["arms"][1]["rounds"][1]["decisionRounds"] = 3
        payload["arms"][0]["stoppedEarly"] = 2
        r = self.load(payload)[0]
        self.assertEqual(pe.arm_of(r, "dream")["stoppedEarly"], 1)
        self.assertEqual(pe.arm_of(r, "fixed")["stoppedEarly"], 2, "the file's count is taken as written")
        self.assertEqual(pe.series([r])["dream"]["stoppedEarly"], [1])
        no_k1 = seed1()
        del no_k1["budget"]["k1"]
        self.assertIsNone(self.load(no_k1)[0]["arms"][1]["stoppedEarly"])
        no_decisions = seed1()
        del no_decisions["arms"][1]["rounds"][2]["decisionRounds"]
        r = self.load(no_decisions)[0]
        self.assertIsNone(pe.arm_of(r, "dream")["stoppedEarly"])
        self.assertIsNone(pe.arm_of(r, "dream")["rounds"][2]["decisionRounds"])
        self.assertEqual(pe.arm_of(r, "fixed")["stoppedEarly"], 0)

    def test_priming_fields_are_read_on_round_one_only(self):
        payload = seed1()
        first = payload["arms"][1]["rounds"][0]
        first["primingTreeIds"] = ["t-p0", "t-p1"]
        first["primingProbes"] = 10
        r = self.load(payload)[0]
        dream = pe.arm_of(r, "dream")
        self.assertEqual(dream["rounds"][0]["primingTreeIds"], ["t-p0", "t-p1"])
        self.assertEqual(dream["rounds"][0]["primingProbes"], 10)
        self.assertIsNone(dream["rounds"][1]["primingTreeIds"])
        self.assertIsNone(dream["rounds"][1]["primingProbes"])
        self.assertIsNone(pe.arm_of(r, "fixed")["rounds"][0]["primingProbes"])
        ser = pe.series([r])
        self.assertEqual(ser["dream"]["primingProbes"], [10])
        self.assertEqual(ser["fixed"]["primingProbes"], [None])

    def test_child_mode_fields_initial_policy_beta_and_the_k1_note(self):
        payload = seed1()
        payload["arms"][1]["mode"] = {
            "proposer": "llm",
            "dreamer": "llm",
            "model": "m",
            "thinking": "off",
            "maxOutputTokens": 4096,
        }
        payload["initialPolicy"] = {"beta": 6}
        payload["notes"] = [NOTE, "k1 5 <= beta 6: patience can never stop a rollout before the round cap"]
        r = self.load(payload)[0]
        dream = pe.arm_of(r, "dream")
        self.assertEqual((dream["thinking"], dream["maxOutputTokens"]), ("off", 4096))
        self.assertEqual((pe.arm_of(r, "fixed")["thinking"], pe.arm_of(r, "fixed")["maxOutputTokens"]), (None, None))
        self.assertEqual(r["initialPolicyBeta"], 6.0)
        self.assertIsNone(self.load(seed1())[0]["initialPolicyBeta"])
        self.assertIn(
            "note: k1 5 <= beta 6: patience can never stop a rollout before the round cap", pe.check_tables([r])
        )

    def test_selected_policy_id_defaults_to_the_final_one(self):
        r = self.load(seed1())[0]
        dream = pe.arm_of(r, "dream")
        self.assertEqual(dream["selectedPolicyId"], POLICY_B)
        payload = seed1()
        payload["arms"][1]["selectedPolicyId"] = POLICY_A
        self.assertEqual(pe.arm_of(self.load(payload)[0], "dream")["selectedPolicyId"], POLICY_A)


@unittest.skipUnless(HAS_MPL, "matplotlib not installed for this interpreter")
class Render(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.p1 = write(self.tmp.name, "s1.json", seed1())
        self.p2 = write(self.tmp.name, "s2.json", seed2())

    def tearDown(self):
        self.tmp.cleanup()

    def test_render_single_seed_ratio_card_is_warn_toned(self):
        paths = [
            write(self.tmp.name, f"h{s['seed']}.json", s)
            for s in (seed_reaching_early(1), seed_never_reaching(2), seed_never_reaching(3))
        ]
        out = Path(self.tmp.name) / "honest"
        report = pe.render(pe.load_results(paths), out)["report"].read_text(encoding="utf-8")
        self.assertIn("single-seed ratio 1.20x fewer calls (not a median)", report)
        self.assertIn('<div class="line warn">  dream: reached T in 1/3 seeds; single-seed ratio', report)
        self.assertNotIn('<div class="line ok">  dream: reached T in 1/3', report)
        self.assertIn("only a ratio defined in every seed can be shown", report)

    def test_render_without_a_reference_arm_and_with_malformed_headline(self):
        only_dream = result(
            3,
            [arm("dream", rows([1.0, 1.1, 1.2], [5, 5, 5], [POLICY_A, POLICY_B, POLICY_B]))],
            headline={"reference": "fixed", "probesToTarget": 5},
        )
        path = write(self.tmp.name, "nofixed.json", only_dream)
        out = Path(self.tmp.name) / "nofixed"
        paths = pe.render(pe.load_results([path]), out)
        report = paths["report"].read_text(encoding="utf-8")
        self.assertIn("no control", report)
        for key in ("round_best", "compute", "attempts", "proposals", "dreaming", "headline"):
            self.assertGreater(paths[key].stat().st_size, 0, key)

    def test_render_writes_six_pngs_and_a_report(self):
        out = Path(self.tmp.name) / "plots"
        paths = pe.render(pe.load_results([self.p1, self.p2]), out)
        for key in ("round_best", "compute", "attempts", "proposals", "dreaming", "headline", "report"):
            self.assertTrue(paths[key].exists(), key)
            self.assertGreater(paths[key].stat().st_size, 0, key)
        report = paths["report"].read_text(encoding="utf-8")
        for name in ("fixed", "dream", "dream-guided"):
            self.assertIn(name, report)
        self.assertIn("fewer calls", report)
        self.assertIn("higher", report)
        self.assertIn("not reached", report)
        self.assertIn("not comparable", report)
        self.assertIn("nothing is illustrative", report)
        self.assertIn(NOTE, report)
        self.assertIn("guidance worse", report)
        self.assertIn('<div class="line bad">  dream-guided: 1.03x LOWER score at budget 39 (1.3100 vs 1.3521)', report)
        self.assertIn('<div class="line bad">  dream-guided: 2.00x MORE calls (90 vs 45)', report)
        self.assertIsNone(INVERTED_WORDING.search(report), "no `0.83x fewer` anywhere on the page")
        self.assertIn("never as 0.83x fewer", report)
        self.assertIn("inverse of the median ratio", report)
        self.assertIn("<th>replay objective</th><td>beta1=0.01 beta2=0.02</td>", report)

    def test_render_single_seed_and_short_run(self):
        short = seed1()
        short["rounds"] = 2
        for a in short["arms"]:
            a["rounds"] = a["rounds"][:2]
            a["totals"]["probes"] = a["rounds"][-1]["cumulativeProbes"]
            a["totals"]["finalBest"] = a["rounds"][-1]["cumulativeBest"]
        short["headline"] = file_headline(short["arms"])
        path = write(self.tmp.name, "short.json", short)
        out = Path(self.tmp.name) / "short"
        paths = pe.render(pe.load_results([path]), out)
        report = paths["report"].read_text(encoding="utf-8")
        self.assertIn("too short to show a curve", report)

    def test_main_renders_to_default_out_dir(self):
        with contextlib.redirect_stdout(io.StringIO()):
            code = pe.main([self.p1, "--no-reexec"])
        self.assertEqual(code, 0)
        self.assertTrue((Path(self.tmp.name) / "plots" / "report.html").exists())

    def test_render_provenance_file_labels_the_axis_and_draws_the_validity_panel(self):
        path = write(self.tmp.name, "llm.json", seed_llm(1))
        out = Path(self.tmp.name) / "llm"
        paths = pe.render(pe.load_results([path]), out)
        self.assertGreater(paths["proposals"].stat().st_size, 0)
        report = paths["report"].read_text(encoding="utf-8")
        self.assertIn("The bold series is on agent-generated calls", report)
        self.assertIn("the thin series is on probes", report)
        self.assertIn("The compute axis of the compute figure is agent-generated calls", report)
        self.assertIn("<h2>LLM-proposal validity per round</h2>", report)
        self.assertIn("Child proposer results per round per arm", report)
        self.assertIn("<td>parse 7, error 2</td>", report)
        self.assertIn(
            '<div class="line warn">  dream: 35 probes = 3 agent-generated + 32 local (32 fallbacks)</div>', report
        )
        self.assertIn("<th>agent-generated</th><th>cum agent-generated</th><th>local fallbacks</th>", report)
        self.assertIn("not recorded, never as 0", report)
        self.assertNotIn("not recorded (result predates origin tracking)", report)

    def test_render_legacy_file_says_provenance_is_not_recorded(self):
        out = Path(self.tmp.name) / "legacy"
        paths = pe.render(pe.load_results([self.p1, self.p2]), out)
        self.assertGreater(paths["proposals"].stat().st_size, 0, "the panel is written with the words, not skipped")
        report = paths["report"].read_text(encoding="utf-8")
        self.assertIn(
            "Agent-generated calls are not recorded (result predates origin tracking), so they are not on this axis",
            report,
        )
        self.assertIn("This result records no proposal provenance", report)
        self.assertIn("are not recorded, not 0, and nothing is drawn", report)
        self.assertIn("<td>-</td><td>-</td><td>-</td><td>-</td><td>-</td><td>-</td>", report, "provenance cells read -")
        self.assertNotIn("agent-generated +", report)

    def test_render_mixed_seeds_keeps_probes_on_the_axis_and_says_how_many_recorded(self):
        paths_in = [write(self.tmp.name, f"m{p['seed']}.json", p) for p in (seed_reaching_early(1), seed_llm(2))]
        out = Path(self.tmp.name) / "mixed"
        paths = pe.render(pe.load_results(paths_in), out)
        report = paths["report"].read_text(encoding="utf-8")
        self.assertIn("recorded in 2/4 arm records only, so they are not on this axis", report)
        self.assertIn("seed 2 dream: 35 probes = 3 agent-generated + 32 local (32 fallbacks)", report)
        self.assertGreater(paths["proposals"].stat().st_size, 0)

    def test_render_audited_file_draws_the_dreaming_panel_and_its_table(self):
        paths_in = [write(self.tmp.name, f"a{p['seed']}.json", p) for p in (seed_audited(1), seed_audited(2))]
        out = Path(self.tmp.name) / "audited"
        paths = pe.render(pe.load_results(paths_in), out)
        self.assertGreater(paths["dreaming"].stat().st_size, 0)
        report = paths["report"].read_text(encoding="utf-8")
        self.assertIn("<h2>Dreaming audit per step</h2>", report)
        self.assertIn("Every candidate a dreaming step scored", report)
        self.assertIn("<th>lever gap</th><th>in support</th>", report)
        self.assertIn(
            "<td>dream</td><td>2</td><td>mixed</td><td>4</td><td>2</td><td>1.2000</td><td>1.2600</td><td>2/2</td><td>0.0700</td><td>1</td>",
            report,
        )
        self.assertIn(
            "<td>dream</td><td>3</td><td>llm</td><td>4</td><td>2</td><td>1.2500</td><td>1.2500</td><td>0/2</td><td>0</td><td>0.75</td>",
            report,
        )
        self.assertNotIn("predates the per-candidate audit", report)
        # Two byte-identical seeds: the fixed spread is 0, so the +0.01 delta clears it with one sign.
        self.assertIn('<div class="line ok">  dream: verdict: exceeds noise floor</div>', report)
        self.assertIn("The verdict follows the noise-floor rule", report)

    def test_render_legacy_file_says_the_audit_is_not_recorded(self):
        out = Path(self.tmp.name) / "legacy-audit"
        paths = pe.render(pe.load_results([self.p1]), out)
        self.assertGreater(paths["dreaming"].stat().st_size, 0, "the panel is written with the fallback, not skipped")
        report = paths["report"].read_text(encoding="utf-8")
        self.assertIn("This result predates the per-candidate audit", report)
        self.assertIn(
            "<td>dream</td><td>2</td><td>-</td><td>4</td><td>-</td><td>1.2000</td><td>1.2500</td><td>1/1</td><td>-</td><td>-</td>",
            report,
        )
        self.assertIn("dream: verdict: single seed: no verdict", report)
        self.assertIn("dream: exact probes to T not recorded", report)

    def test_render_fixed_only_and_inert_results(self):
        fixed_only = result(9, [arm("fixed", rows([1.3, 1.35, 1.33], [15, 15, 15], [POLICY_A] * 3), fixed=True)])
        path = write(self.tmp.name, "fixed-only.json", fixed_only)
        paths = pe.render(pe.load_results([path]), Path(self.tmp.name) / "fixed-only")
        report = paths["report"].read_text(encoding="utf-8")
        self.assertIn("No arm dreamed in this result", report)
        self.assertIn("no dreaming step recorded", report)
        inert = [
            write(self.tmp.name, f"i{p['seed']}.json", p)
            for p in (seed_paired(1, 1.35, 1.45, changes=False), seed_paired(2, 1.36, 1.46, changes=False))
        ]
        paths = pe.render(pe.load_results(inert), Path(self.tmp.name) / "inert")
        report = paths["report"].read_text(encoding="utf-8")
        self.assertIn('<div class="line warn">  dream: verdict: within noise floor (dreaming inert)</div>', report)
        self.assertIn("policy never changed in 2/2 seed(s): dreaming inert", report)
        self.assertGreater(paths["attempts"].stat().st_size, 0)

    def test_render_exact_headline_beside_the_rollout_granular_one(self):
        paths_in = [write(self.tmp.name, f"e{p['seed']}.json", p) for p in (seed_exact(1), seed_exact(2))]
        paths = pe.render(pe.load_results(paths_in), Path(self.tmp.name) / "exact")
        report = paths["report"].read_text(encoding="utf-8")
        self.assertIn('<div class="line ok">  dream: 1.20x fewer calls (25 vs 30)</div>', report)
        self.assertIn('<div class="line ok">  dream: exact 1.16x fewer calls (19 vs 22)</div>', report)
        self.assertIn("exact probes to T [22, 22] 22..22 (spread 0, std 0)", report)
        self.assertIn("The exact line beside it counts to the first PROBE whose score reaches T", report)


if __name__ == "__main__":
    unittest.main()
