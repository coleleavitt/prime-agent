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
never 0, and pools with a newer seed without inventing zeros for it.
"""

from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import os
import re
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


# A ratio below 1 written as `0.83x fewer` / `0.97x higher`; the caption's own "never as 0.83x fewer" is allowed.
INVERTED_WORDING = re.compile(r"(?<!never as )\b0\.\d+x (fewer|higher)")


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

    def test_objective_key_only_looks_at_beta1_and_beta2(self):
        self.assertEqual(pe.objective_key({"beta1": 0.05, "beta2": 0.05}), (0.05, 0.05))
        self.assertEqual(pe.objective_key({"beta1": 0.05, "beta2": 0.05, "note": "x"}), (0.05, 0.05))
        self.assertEqual(pe.objective_key({"beta1": "0.05"}), (None, None))
        self.assertIsNone(pe.objective_key(None))
        self.assertEqual(pe.objective_text({"beta1": 0.05, "beta2": 1e-5}), "beta1=0.05 beta2=1e-05")
        self.assertEqual(pe.objective_text({"beta2": 0.05}), "beta1=? beta2=0.05")

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
        for key in ("round_best", "compute", "attempts", "proposals", "headline"):
            self.assertGreater(paths[key].stat().st_size, 0, key)

    def test_render_writes_five_pngs_and_a_report(self):
        out = Path(self.tmp.name) / "plots"
        paths = pe.render(pe.load_results([self.p1, self.p2]), out)
        for key in ("round_best", "compute", "attempts", "proposals", "headline", "report"):
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


if __name__ == "__main__":
    unittest.main()
