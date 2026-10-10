"""Factory harness entry kind: spec validation, dag compilation, and executor.

Consolidated port of TS-era PRs #2397 (factory entry kind, validator, dag
compiler) and #2401 (the executor: run/status/stop/resume, guarded
transitions, re-entry, failure policies, budgets, rate-limit backoff, stop
races) plus the accepted review findings from both:

- a fan-in dag node's full dependency set compiles to ONE join transition
  (never per-edge transitions that would let the node start after a single
  parent settles);
- a node reading its own output is rejected like depends_on: [self];
- explicit JSON null on typed fields (run.max_parallel, state.retries,
  state.max_entries, ...) is rejected with the field's own message instead
  of surviving canonicalization as None;
- port validation is linear in the port count (seen-sets and one
  output-type map per source), so thousands of ports stay write-time cheap;
- quiescence counts the INSTANCE layer: a failed foreach entry's still
  running siblings are collected before the run ends, and a resident
  entry's queued instances are admitted before completion is reported;
  admitted resident instances alone never block completion;
- queued siblings of a terminal (failed/cancelled) entry are never
  admitted (_next_pending_instance serves running entries only);
- the run budget is enforced before each admission, including run()'s and
  resume()'s admission phase;
- rate-limit backoff never blocks the sole control lane: admission defers
  to a deadline and the loop waits it out in bounded slices while children
  keep being collected;
- concurrent stop() calls run one cancellation pass (the transitional
  "stopping" state is guarded too);
- resume() bumps the control-loop generation so the pause-path loop can
  never continue as a second concurrent control loop;
- long state ids disambiguate their spawn names with a digest, so two
  states sharing a 20-character prefix never collide on the supervisor's
  unique sibling-name requirement;
- a configured inline subagent name labels the spawned children verbatim
  (the first instance), with the generated label's -i<n>/-a<n> suffixes on
  re-entry, foreach fan-out, and retries; over-length names and names
  duplicated across states are rejected at write time (Macroscope review
  finding: the name was dropped, so children always got the generated
  label);
- milestone notices go to the host as validated "factory.progress"
  payloads exactly once per kind per run, and a dead bridge leaves the
  milestone in the ledger instead of wedging the run.

The validator and the executor now run in the Prime Agent host
(pa_core::factory): the validator battery below runs unchanged against the
host's implementation through the kernel client, and the executor battery
(FactoryExecutorTest, FactoryGraphWatchTest, FactoryFrameCapTest, the label
helpers' tests) is ported to Rust beside the executor
(crates/pa-core/src/factory/executor/tests/, factory/labels.rs). What stays
here is the kernel's half: the client contract, the opt-in gate, the help
text, and the machine library.
"""

from __future__ import annotations

import asyncio
import json
import os
import re
import shutil
import subprocess
import time
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from typing import Any
from unittest.mock import patch

import rlm as rlm_module
from rlm import factory as factory_module
from rlm.factory import (
    SUBAGENT_NAME_MAX_LENGTH,
    FactoryExecutor,
    MachineFile,
    MachineResolutionError,
    _scan_machine_library,
    canonicalize_factory_spec,
    cli_dispatch,
    compile_factory_dag,
    export_factory_spec,
    export_library_machine,
    export_machine,
    import_machine,
    list_machines,
    machine_description_errors,
    machine_name_errors,
    parse_machine_file,
    render_machine_file,
    repo_machines_dir,
    resolve_machine,
    topological_order,
    user_machines_dir,
    validate_factory_machine,
    validate_factory_spec,
)
from rlm.harness import HarnessState


# ---------------------------------------------------------------------------
# Spec fixtures
# ---------------------------------------------------------------------------


def state(state_id: str, **overrides: Any) -> dict[str, Any]:
    base: dict[str, Any] = {"id": state_id, "subagent": "worker"}
    base.update(overrides)
    return base


def valid_machine() -> dict[str, Any]:
    """Review-loop machine: collect -> reviewing (max 4 entries) with a
    guarded switch to fixing (max 3 entries) and a self-loop, fixing re-enters
    reviewing."""
    return {
        "run": {"budget_ms": 600_000, "failure_policy": "continue", "max_parallel": 4, "max_transitions": 40},
        "states": [
            {
                "id": "collect",
                "entry": True,
                "subagent": "researcher",
                "outputs": [{"name": "findings", "type": "text"}],
            },
            {
                "id": "reviewing",
                "subagent": {"prompt": "Review the draft."},
                "inputs": [{"name": "draft", "type": "text", "from": "collect.findings"}],
                "outputs": [{"name": "verdict", "type": "json"}],
                "max_entries": 4,
                "retries": 1,
            },
            {"id": "fixing", "subagent": {"prompt": "Fix the findings."}, "max_entries": 3},
        ],
        "transitions": [
            {"from": "collect", "to": "reviewing"},
            {
                "from": "reviewing",
                "to": "fixing",
                "when": {"output": "verdict", "path": "approved", "op": "eq", "value": False},
            },
            {"from": "reviewing", "to": "reviewing", "when": {"output": "verdict", "op": "exists"}},
            {"from": "fixing", "to": "reviewing"},
        ],
    }


def node(node_id: str, **overrides: Any) -> dict[str, Any]:
    base: dict[str, Any] = {"id": node_id, "subagent": "worker"}
    base.update(overrides)
    return base


def valid_dag() -> dict[str, Any]:
    return {
        "run": {"budget_ms": 600_000, "failure_policy": "continue", "max_parallel": 4},
        "nodes": [
            {
                "id": "collect",
                "subagent": "researcher",
                "outputs": [{"name": "findings", "type": "text"}],
            },
            {
                "id": "fan-out",
                "subagent": {"prompt": "Expand each item.", "name": "expander", "model": "m1", "thinking": "low"},
                "depends_on": ["collect"],
                "inputs": [{"name": "items", "type": "text", "from": "collect.findings"}],
            },
            {
                "id": "review",
                "subagent": {"prompt": "Review the fan-out."},
                "depends_on": ["collect", "fan-out"],
                "inputs": [{"name": "draft", "type": "text", "from": "collect.findings"}],
                "retries": 2,
                "budget_ms": 100_000,
                "failure_policy": "fail_fast",
            },
        ],
    }


# ---------------------------------------------------------------------------
# Dag-form validation (write-time dry run)
# ---------------------------------------------------------------------------


class ValidateFactorySpecTest(unittest.TestCase):
    def test_valid_spec_has_no_errors(self) -> None:
        self.assertEqual(validate_factory_spec(valid_dag()), [])

    def test_dag_must_be_an_object(self) -> None:
        for bad in (None, [], "nodes", 42):
            errors = validate_factory_spec(bad)
            self.assertEqual(len(errors), 1)
            self.assertIn("factory dag must be a JSON object", errors[0])

    def test_nodes_required_and_must_be_a_list(self) -> None:
        self.assertEqual(
            validate_factory_spec({"nodes": "nope"}),
            ["factory dag requires a nodes list"],
        )
        self.assertEqual(
            validate_factory_spec({"run": "bad", "nodes": "nope"}),
            ["run must be an object", "factory dag requires a nodes list"],
        )

    def test_node_cap(self) -> None:
        at_cap = {"nodes": [node(f"n{i}") for i in range(1024)]}
        self.assertEqual(validate_factory_spec(at_cap), [])
        over_cap = {"nodes": [node(f"n{i}") for i in range(1025)]}
        errors = validate_factory_spec(over_cap)
        self.assertEqual(len(errors), 1)
        self.assertIn("between 1 and 1024 nodes", errors[0])

    def test_node_ids(self) -> None:
        for good in ("a", "node-1", "1st-node", "a" * 64):
            self.assertEqual(validate_factory_spec({"nodes": [node(good)]}), [], good)
        for bad in ("-abc", "ABC", "a_b", "a.b", "a" * 65):
            errors = validate_factory_spec({"nodes": [{"id": bad, "subagent": "w"}]})
            self.assertEqual(len(errors), 1, bad)
            self.assertIn("id must match", errors[0])
        for bad in ("", None, 5):
            errors = validate_factory_spec({"nodes": [{"id": bad, "subagent": "w"}]})
            self.assertEqual(errors, ["nodes[0] requires a non-empty id"], repr(bad))

    def test_duplicate_node_ids(self) -> None:
        errors = validate_factory_spec({"nodes": [node("dup"), node("dup")]})
        self.assertEqual(len(errors), 1)
        self.assertIn("duplicates node id 'dup'", errors[0])

    def test_subagent_forms(self) -> None:
        by_ref = {"nodes": [node("a", subagent="reviewer")]}
        self.assertEqual(validate_factory_spec(by_ref), [])
        inline = {"nodes": [node("a", subagent={"prompt": "Do work."})]}
        self.assertEqual(validate_factory_spec(inline), [])
        inline_full = {
            "nodes": [
                node("a", subagent={"prompt": "Do work.", "name": "w", "model": "m", "thinking": "high"})
            ]
        }
        self.assertEqual(validate_factory_spec(inline_full), [])

        missing = {"nodes": [{"id": "a"}]}
        errors = validate_factory_spec(missing)
        self.assertEqual(len(errors), 1)
        self.assertIn("requires a subagent", errors[0])

        empty_ref = {"nodes": [node("a", subagent="")]}
        errors = validate_factory_spec(empty_ref)
        self.assertEqual(len(errors), 1)
        self.assertIn("requires a subagent", errors[0])

        empty_prompt = {"nodes": [node("a", subagent={"prompt": ""})]}
        errors = validate_factory_spec(empty_prompt)
        self.assertEqual(len(errors), 1)
        self.assertIn("requires a non-empty prompt", errors[0])

        bad_name = {"nodes": [node("a", subagent={"prompt": "p", "model": 5})]}
        errors = validate_factory_spec(bad_name)
        self.assertEqual(len(errors), 1)
        self.assertIn("model must be a non-empty string", errors[0])

        bad_thinking = {"nodes": [node("a", subagent={"prompt": "p", "thinking": ""})]}
        errors = validate_factory_spec(bad_thinking)
        self.assertEqual(len(errors), 1)
        self.assertIn("thinking must be a non-empty string", errors[0])

        # Whitespace-only values are rejected at write time: runtime
        # resolution strips them (_resolve_subagents /
        # _validate_spawn_settings), so a whitespace-only field is a
        # persistable factory that can never spawn.
        whitespace_prompt = {"nodes": [node("a", subagent={"prompt": "  \t "})]}
        errors = validate_factory_spec(whitespace_prompt)
        self.assertEqual(errors, ["node a inline subagent requires a non-empty prompt"])
        for key in ("name", "model", "thinking"):
            whitespace_field = {"nodes": [node("a", subagent={"prompt": "p", key: "  \t "})]}
            errors = validate_factory_spec(whitespace_field)
            self.assertEqual(
                errors, [f"node a inline subagent {key} must be a non-empty string when provided"], key
            )

        # The dag form compiles to machine form first, so the name rules
        # (length, cross-state uniqueness) apply to nodes too.
        dag_duplicate = {
            "nodes": [
                node("a", subagent={"prompt": "p", "name": "w"}),
                node("b", subagent={"prompt": "p", "name": "w"}, depends_on=["a"]),
            ]
        }
        errors = validate_factory_spec(dag_duplicate)
        self.assertEqual(errors, ["state b subagent name 'w' is already configured by state 'a'"])

    def test_lifecycle(self) -> None:
        for good in ("task", "resident"):
            self.assertEqual(validate_factory_spec({"nodes": [node("a", lifecycle=good)]}), [])
        errors = validate_factory_spec({"nodes": [node("a", lifecycle="daemon")]})
        self.assertEqual(len(errors), 1)
        self.assertIn("lifecycle must be 'task' or 'resident'", errors[0])

    def test_run_budget_and_node_budget(self) -> None:
        ok = {
            "run": {"budget_ms": 1000},
            "nodes": [node("a", budget_ms=1000)],
        }
        self.assertEqual(validate_factory_spec(ok), [])

        over = {
            "run": {"budget_ms": 1000},
            "nodes": [node("a", budget_ms=1001)],
        }
        errors = validate_factory_spec(over)
        self.assertEqual(len(errors), 1)
        self.assertIn("exceeds the run budget_ms", errors[0])

        for bad in (0, -5, 1.5, "10", True):
            errors = validate_factory_spec({"run": {"budget_ms": bad}, "nodes": [node("a")]})
            self.assertEqual(errors, ["run budget_ms must be a positive integer"], bad)
            errors = validate_factory_spec({"nodes": [node("a", budget_ms=bad)]})
            self.assertEqual(errors, ["node a budget_ms must be a positive integer"], bad)

        # No run budget set: any positive node budget is fine.
        self.assertEqual(validate_factory_spec({"nodes": [node("a", budget_ms=999_999)]}), [])

    def test_run_budget_invalid(self) -> None:
        errors = validate_factory_spec({"run": "bad", "nodes": [node("a")]})
        self.assertEqual(errors, ["run must be an object"])

    def test_run_failure_policy(self) -> None:
        for good in ("fail_fast", "continue", "escalate"):
            self.assertEqual(validate_factory_spec({"run": {"failure_policy": good}, "nodes": [node("a")]}), [])
        errors = validate_factory_spec({"run": {"failure_policy": "stop"}, "nodes": [node("a")]})
        self.assertEqual(len(errors), 1)
        self.assertIn("run failure_policy must be one of", errors[0])

    def test_run_max_parallel(self) -> None:
        for good in (1, 8, 64):
            self.assertEqual(validate_factory_spec({"run": {"max_parallel": good}, "nodes": [node("a")]}), [])
        for bad in (0, 65, -1, 1.5, "8", True):
            errors = validate_factory_spec({"run": {"max_parallel": bad}, "nodes": [node("a")]})
            self.assertEqual(errors, ["run max_parallel must be an integer between 1 and 64"], bad)

    def test_run_max_transitions(self) -> None:
        for good in (1, 40, 10_000):
            self.assertEqual(validate_factory_spec({"run": {"max_transitions": good}, "nodes": [node("a")]}), [])
        for bad in (0, -1, 10_001, 1.5, "5", True):
            errors = validate_factory_spec({"run": {"max_transitions": bad}, "nodes": [node("a")]})
            self.assertEqual(errors, ["run max_transitions must be a positive integer no greater than 10000"], bad)

    def test_run_max_children(self) -> None:
        # The run-wide child budget: total admissions over the run's life.
        for good in (1, 40, 1_000_000):
            self.assertEqual(validate_factory_spec({"run": {"max_children": good}, "nodes": [node("a")]}), [])
        for bad in (0, -1, 1_000_001, 1.5, "5", True):
            errors = validate_factory_spec({"run": {"max_children": bad}, "nodes": [node("a")]})
            self.assertEqual(errors, ["run max_children must be a positive integer no greater than 1000000"], bad)

    def test_node_failure_policy(self) -> None:
        for good in ("fail_fast", "continue", "escalate"):
            self.assertEqual(validate_factory_spec({"nodes": [node("a", failure_policy=good)]}), [])
        errors = validate_factory_spec({"nodes": [node("a", failure_policy="retry")]})
        self.assertEqual(len(errors), 1)
        self.assertIn("node a failure_policy must be one of", errors[0])

    def test_retries(self) -> None:
        for good in (0, 5, 10):
            self.assertEqual(validate_factory_spec({"nodes": [node("a", retries=good)]}), [])
        for bad in (-1, 11, 1.5, "2", True):
            errors = validate_factory_spec({"nodes": [node("a", retries=bad)]})
            self.assertEqual(errors, ["node a retries must be an integer between 0 and 10"], bad)

    def test_depends_on(self) -> None:
        ok = {"nodes": [node("a"), node("b", depends_on=["a"])]}
        self.assertEqual(validate_factory_spec(ok), [])

        self_dep = {"nodes": [node("a", depends_on=["a"])]}
        errors = validate_factory_spec(self_dep)
        self.assertEqual(errors, ["node a cannot depend on itself"])

        unknown = {"nodes": [node("a", depends_on=["ghost"])]}
        errors = validate_factory_spec(unknown)
        self.assertEqual(errors, ["node a depends on unknown node 'ghost'"])

        not_list = {"nodes": [node("a", depends_on="b")]}
        errors = validate_factory_spec(not_list)
        self.assertEqual(errors, ["node a depends_on must be a list of node ids"])

        bad_entry = {"nodes": [node("a", depends_on=[5])]}
        errors = validate_factory_spec(bad_entry)
        self.assertEqual(errors, ["node a depends_on entries must be non-empty node id strings"])

    def test_self_input_edge_is_rejected_like_a_self_dependency(self) -> None:
        # Review finding: a node reading its own output would compile to a
        # never-reachable self-loop state, so the self-dependency rule covers
        # the EFFECTIVE edge set (depends_on union inputs[].from), not only
        # depends_on.
        self_output = {
            "nodes": [
                node("a", outputs=[{"name": "o", "type": "text"}]),
                node("b", inputs=[{"name": "i", "type": "text", "from": "b.o"}]),
            ]
        }
        errors = validate_factory_spec(self_output)
        self.assertIn("node b cannot depend on itself", errors)
        machine, compile_errors = compile_factory_dag(self_output)
        self.assertIsNone(machine)
        self.assertIn("node b cannot depend on itself", compile_errors)

        # A cross-node input edge is of course fine.
        ok = {
            "nodes": [
                node("a", outputs=[{"name": "o", "type": "text"}]),
                node("b", inputs=[{"name": "i", "type": "text", "from": "a.o"}]),
            ]
        }
        self.assertEqual(validate_factory_spec(ok), [])

    def test_explicit_null_typed_fields_are_rejected_not_defaulted(self) -> None:
        # Review finding: explicit JSON null on a typed field used to survive
        # canonicalization as None and reach the executor without a usable
        # limit. Presence-based checks reject null with the field's own
        # message instead of treating it as an omitted default.
        self.assertEqual(
            validate_factory_spec({"run": {"max_parallel": None}, "nodes": [node("a")]}),
            ["run max_parallel must be an integer between 1 and 64"],
        )
        self.assertEqual(
            validate_factory_spec({"run": {"max_transitions": None}, "nodes": [node("a")]}),
            ["run max_transitions must be a positive integer no greater than 10000"],
        )
        self.assertEqual(
            validate_factory_spec({"run": {"max_children": None}, "nodes": [node("a")]}),
            ["run max_children must be a positive integer no greater than 1000000"],
        )
        self.assertEqual(
            validate_factory_spec({"run": {"budget_ms": None}, "nodes": [node("a")]}),
            ["run budget_ms must be a positive integer"],
        )
        self.assertEqual(
            validate_factory_spec({"run": {"failure_policy": None}, "nodes": [node("a")]}),
            ["run failure_policy must be one of ['fail_fast', 'continue', 'escalate'], got None"],
        )
        self.assertEqual(
            validate_factory_spec({"nodes": [node("a", retries=None)]}),
            ["node a retries must be an integer between 0 and 10"],
        )
        self.assertEqual(
            validate_factory_spec({"nodes": [node("a", budget_ms=None)]}),
            ["node a budget_ms must be a positive integer"],
        )
        self.assertEqual(
            validate_factory_spec({"nodes": [node("a", failure_policy=None)]}),
            ["node a failure_policy must be one of ['fail_fast', 'continue', 'escalate'], got None"],
        )
        self.assertEqual(
            validate_factory_spec({"nodes": [node("a", lifecycle=None)]}),
            ["node a lifecycle must be 'task' or 'resident', got None"],
        )
        machine_nulls = {
            "states": [
                {
                    "id": "a",
                    "entry": True,
                    "subagent": "w",
                    "max_entries": None,
                    "retries": None,
                }
            ],
            "transitions": [],
        }
        self.assertEqual(
            validate_factory_machine(machine_nulls),
            [
                "state a retries must be an integer between 0 and 10",
                "state a max_entries must be an integer >= 1",
            ],
        )
        self.assertEqual(
            validate_factory_machine({"states": [{"id": "a", "entry": None, "subagent": "w"}], "transitions": []}),
            [
                "state a entry must be a boolean",
                "factory machine requires at least one entry state",
            ],
        )

    def test_data_edges(self) -> None:
        ok = {
            "nodes": [
                node("a", outputs=[{"name": "out", "type": "text"}]),
                node("b", inputs=[{"name": "in", "type": "text", "from": "a.out"}]),
            ]
        }
        self.assertEqual(validate_factory_spec(ok), [])

        # A data edge implies ordering even without depends_on.
        no_declared_dep = {
            "nodes": [
                node("a", outputs=[{"name": "out", "type": "json"}]),
                node("b", inputs=[{"name": "in", "type": "json", "from": "a.out"}]),
            ]
        }
        self.assertEqual(validate_factory_spec(no_declared_dep), [])

        unknown_source = {
            "nodes": [node("b", inputs=[{"name": "in", "type": "text", "from": "ghost.out"}])]
        }
        errors = validate_factory_spec(unknown_source)
        self.assertEqual(len(errors), 1)
        self.assertIn("references unknown node 'ghost'", errors[0])

        undeclared_output = {
            "nodes": [
                node("a"),
                node("b", inputs=[{"name": "in", "type": "text", "from": "a.missing"}]),
            ]
        }
        errors = validate_factory_spec(undeclared_output)
        self.assertEqual(len(errors), 1)
        self.assertIn("does not declare", errors[0])

        type_mismatch = {
            "nodes": [
                node("a", outputs=[{"name": "out", "type": "text"}]),
                node("b", inputs=[{"name": "in", "type": "json", "from": "a.out"}]),
            ]
        }
        errors = validate_factory_spec(type_mismatch)
        self.assertEqual(len(errors), 1)
        self.assertIn("cannot read from output", errors[0])
        self.assertIn("of type 'text'", errors[0])

        malformed_from = {
            "nodes": [
                node("a", outputs=[{"name": "out", "type": "text"}]),
                node("b", inputs=[{"name": "in", "type": "text", "from": "no-dot"}]),
            ]
        }
        errors = validate_factory_spec(malformed_from)
        self.assertEqual(len(errors), 1)
        self.assertIn("requires a 'from' reference", errors[0])

    def test_input_output_ports(self) -> None:
        ok = {
            "nodes": [
                node(
                    "a",
                    outputs=[{"name": "o1", "type": "text"}, {"name": "o2", "type": "json"}],
                )
            ]
        }
        self.assertEqual(validate_factory_spec(ok), [])

        bad_output_type = {"nodes": [node("a", outputs=[{"name": "o", "type": "yaml"}])]}
        errors = validate_factory_spec(bad_output_type)
        self.assertEqual(len(errors), 1)
        self.assertIn("output 'o' type must be 'text' or 'json'", errors[0])

        dup_output = {
            "nodes": [node("a", outputs=[{"name": "o", "type": "text"}, {"name": "o", "type": "json"}])]
        }
        errors = validate_factory_spec(dup_output)
        self.assertEqual(errors, ["node a declares duplicate output name 'o'"])

        bad_input_type = {
            "nodes": [
                node("b", outputs=[{"name": "o", "type": "text"}]),
                node("a", inputs=[{"name": "i", "type": "yaml", "from": "b.o"}]),
            ]
        }
        errors = validate_factory_spec(bad_input_type)
        self.assertEqual(errors, ["node a input 'i' type must be 'text' or 'json'"])

        dup_input = {
            "nodes": [
                node(
                    "a",
                    inputs=[
                        {"name": "i", "type": "text", "from": "b.o1"},
                        {"name": "i", "type": "text", "from": "b.o2"},
                    ],
                ),
                node("b", outputs=[{"name": "o1", "type": "text"}, {"name": "o2", "type": "text"}]),
            ]
        }
        errors = validate_factory_spec(dup_input)
        self.assertEqual(errors, ["node a declares duplicate input name 'i'"])

        missing_name = {"nodes": [node("a", outputs=[{"type": "text"}])]}
        errors = validate_factory_spec(missing_name)
        self.assertEqual(errors, ["node a outputs[0] requires a non-empty name"])

        not_a_list = {"nodes": [node("a", outputs="nope")]}
        errors = validate_factory_spec(not_a_list)
        self.assertEqual(errors, ["node a outputs must be a list"])
        not_a_list = {"nodes": [node("a", inputs="nope")]}
        errors = validate_factory_spec(not_a_list)
        self.assertEqual(errors, ["node a inputs must be a list"])

    def test_resident_rules(self) -> None:
        resident_ok = {"nodes": [node("watcher", lifecycle="resident")]}
        self.assertEqual(validate_factory_spec(resident_ok), [])

        depended_on = {
            "nodes": [node("watcher", lifecycle="resident"), node("task", depends_on=["watcher"])]
        }
        errors = validate_factory_spec(depended_on)
        self.assertEqual(errors, ["node task cannot depend on resident node 'watcher'"])

        read_from = {
            "nodes": [
                node("watcher", lifecycle="resident"),
                node("task", inputs=[{"name": "i", "type": "text", "from": "watcher.o"}]),
            ]
        }
        errors = validate_factory_spec(read_from)
        self.assertEqual(errors, ["node task input 'i' cannot read from resident node 'watcher'"])

        declares_outputs = {"nodes": [node("watcher", lifecycle="resident", outputs=[{"name": "o", "type": "text"}])]}
        errors = validate_factory_spec(declares_outputs)
        self.assertEqual(errors, ["resident node watcher cannot declare outputs"])

        uses_foreach = {
            "nodes": [
                node("src", outputs=[{"name": "items", "type": "json"}]),
                node(
                    "watcher",
                    lifecycle="resident",
                    inputs=[{"name": "items", "type": "json", "from": "src.items"}],
                    foreach={"over": "items", "max": 4},
                ),
            ]
        }
        errors = validate_factory_spec(uses_foreach)
        self.assertEqual(errors, ["resident node watcher cannot use foreach"])

        # Empty outputs list on a resident node is fine: nothing is declared.
        empty_outputs = {"nodes": [node("watcher", lifecycle="resident", outputs=[])]}
        self.assertEqual(validate_factory_spec(empty_outputs), [])

    def test_foreach(self) -> None:
        ok = {
            "nodes": [
                node("a", outputs=[{"name": "items", "type": "json"}]),
                node(
                    "b",
                    inputs=[{"name": "items", "type": "json", "from": "a.items"}],
                    foreach={"over": "items", "max": 16},
                ),
            ]
        }
        self.assertEqual(validate_factory_spec(ok), [])

        for good_max in (1, 256):
            ok_max = {
                "nodes": [
                    node("a", outputs=[{"name": "items", "type": "json"}]),
                    node(
                        "b",
                        inputs=[{"name": "items", "type": "json", "from": "a.items"}],
                        foreach={"over": "items", "max": good_max},
                    ),
                ]
            }
            self.assertEqual(validate_factory_spec(ok_max), [])

        for bad_max in (0, 257, -1, 1.5, "8", True):
            bad = {
                "nodes": [
                    node("a", outputs=[{"name": "items", "type": "json"}]),
                    node(
                        "b",
                        inputs=[{"name": "items", "type": "json", "from": "a.items"}],
                        foreach={"over": "items", "max": bad_max},
                    ),
                ]
            }
            errors = validate_factory_spec(bad)
            self.assertEqual(errors, ["node b foreach.max must be an integer between 1 and 256"], bad_max)

        wrong_port = {
            "nodes": [
                node("a", outputs=[{"name": "items", "type": "json"}]),
                node(
                    "b",
                    inputs=[{"name": "items", "type": "json", "from": "a.items"}],
                    foreach={"over": "not-an-input", "max": 4},
                ),
            ]
        }
        errors = validate_factory_spec(wrong_port)
        self.assertEqual(
            errors, ["node b foreach.over must name one of this node's inputs, got 'not-an-input'"]
        )

        text_port = {
            "nodes": [
                node("a", outputs=[{"name": "draft", "type": "text"}]),
                node(
                    "b",
                    inputs=[{"name": "draft", "type": "text", "from": "a.draft"}],
                    foreach={"over": "draft", "max": 4},
                ),
            ]
        }
        errors = validate_factory_spec(text_port)
        self.assertEqual(errors, ["node b foreach.over input 'draft' must have type 'json'"])

        not_object = {"nodes": [node("a", foreach=["bad"])]}
        errors = validate_factory_spec(not_object)
        self.assertEqual(errors, ["node a foreach must be an object"])

    def test_cycles_are_legal_when_an_entry_state_exists(self) -> None:
        # A cycle that does not cover the whole dag compiles to a machine with
        # an entry state; cycles are legal in machine form, so this validates.
        cycle_with_entry = {
            "nodes": [
                node("a"),
                node("b", depends_on=["c"]),
                node("c", depends_on=["b"]),
            ]
        }
        self.assertEqual(validate_factory_spec(cycle_with_entry), [])

        data_cycle_with_entry = {
            "nodes": [
                node("a"),
                node("b", depends_on=["a"], outputs=[{"name": "o", "type": "json"}]),
                node("c", inputs=[{"name": "i", "type": "json", "from": "b.o"}]),
            ]
        }
        self.assertEqual(validate_factory_spec(data_cycle_with_entry), [])

    def test_fully_cyclic_dag_compiles_to_a_machine_without_entry_states(self) -> None:
        depends_cycle = {
            "nodes": [
                node("a", depends_on=["b"]),
                node("b", depends_on=["a"]),
            ]
        }
        self.assertEqual(
            validate_factory_spec(depends_cycle),
            ["factory machine requires at least one entry state"],
        )

        data_cycle = {
            "nodes": [
                node("a", outputs=[{"name": "o", "type": "json"}], inputs=[
                    {"name": "i", "type": "json", "from": "b.o"}
                ]),
                node("b", outputs=[{"name": "o", "type": "json"}], inputs=[
                    {"name": "i", "type": "json", "from": "a.o"}
                ]),
            ]
        }
        self.assertEqual(
            validate_factory_spec(data_cycle),
            ["factory machine requires at least one entry state"],
        )

        three_cycle = {
            "nodes": [
                node("a", depends_on=["c"]),
                node("b", depends_on=["a"]),
                node("c", depends_on=["b"]),
            ]
        }
        self.assertEqual(
            validate_factory_spec(three_cycle),
            ["factory machine requires at least one entry state"],
        )

    def test_collects_multiple_errors(self) -> None:
        dag = {
            "run": {"max_parallel": 99, "failure_policy": "nope"},
            "nodes": [
                node("a", depends_on=["ghost"]),
                node("b", retries=99),
                node("c", failure_policy="retry"),
            ],
        }
        errors = validate_factory_spec(dag)
        self.assertEqual(
            errors,
            [
                "run failure_policy must be one of ['fail_fast', 'continue', 'escalate'], got 'nope'",
                "run max_parallel must be an integer between 1 and 64",
                "node a depends on unknown node 'ghost'",
                "node b retries must be an integer between 0 and 10",
                "node c failure_policy must be one of ['fail_fast', 'continue', 'escalate'], got 'retry'",
            ],
        )


# ---------------------------------------------------------------------------
# Canonicalization
# ---------------------------------------------------------------------------


class CanonicalizeFactorySpecTest(unittest.TestCase):
    def test_applies_defaults(self) -> None:
        dag = {"nodes": [{"id": "a", "subagent": "worker"}]}
        self.assertEqual(
            canonicalize_factory_spec(dag),
            {
                "run": {
                    "failure_policy": "escalate",
                    "max_parallel": 8,
                    "max_transitions": 10,
                    "max_children": 10_000,
                },
                "states": [
                    {
                        "id": "a",
                        "entry": True,
                        "max_entries": 1,
                        "subagent": "worker",
                        "lifecycle": "task",
                        "retries": 0,
                        "failure_policy": "escalate",
                    }
                ],
                "transitions": [],
            },
        )

    def test_preserves_explicit_values(self) -> None:
        dag = {
            "run": {
                "budget_ms": 5000,
                "failure_policy": "continue",
                "max_parallel": 2,
                "max_transitions": 7,
                "max_children": 5,
            },
            "nodes": [
                {
                    "id": "a",
                    "subagent": {"prompt": "Work."},
                    "lifecycle": "task",
                    "retries": 3,
                    "failure_policy": "fail_fast",
                    "budget_ms": 4000,
                    "depends_on": [],
                    "outputs": [{"name": "o", "type": "text"}],
                }
            ],
        }
        self.assertEqual(
            canonicalize_factory_spec(dag),
            {
                "run": {
                    "failure_policy": "continue",
                    "max_parallel": 2,
                    "max_transitions": 7,
                    "max_children": 5,
                    "budget_ms": 5000,
                },
                "states": [
                    {
                        "id": "a",
                        "entry": True,
                        "max_entries": 1,
                        "subagent": {"prompt": "Work."},
                        "lifecycle": "task",
                        "retries": 3,
                        "failure_policy": "fail_fast",
                        "budget_ms": 4000,
                        "outputs": [{"name": "o", "type": "text"}],
                    }
                ],
                "transitions": [],
            },
        )

    def test_state_failure_policy_defaults_to_run_policy(self) -> None:
        dag = {
            "run": {"failure_policy": "continue"},
            "nodes": [{"id": "a", "subagent": "w"}, {"id": "b", "subagent": "w", "failure_policy": "escalate"}],
        }
        result = canonicalize_factory_spec(dag)
        self.assertEqual(result["states"][0]["failure_policy"], "continue")
        self.assertEqual(result["states"][1]["failure_policy"], "escalate")

    def test_max_transitions_defaults_to_ten_per_state_capped(self) -> None:
        ten_states = {"nodes": [node(f"n{i}") for i in range(10)]}
        self.assertEqual(canonicalize_factory_spec(ten_states)["run"]["max_transitions"], 100)
        many_states = {"nodes": [node(f"n{i}") for i in range(1024)]}
        self.assertEqual(canonicalize_factory_spec(many_states)["run"]["max_transitions"], 10_000)

    def test_deduplicates_depends_on_into_one_transition(self) -> None:
        dag = {
            "nodes": [
                {"id": "a", "subagent": "w"},
                {"id": "b", "subagent": "w", "depends_on": ["a", "a", "a"]},
            ]
        }
        result = canonicalize_factory_spec(dag)
        self.assertEqual(result["transitions"], [{"from": "a", "to": "b", "on": "settled"}])

    def test_machine_form_canonicalization(self) -> None:
        machine = {
            "run": {"failure_policy": "continue", "max_parallel": 2, "budget_ms": 5000},
            "states": [
                {"id": "seed", "entry": True, "subagent": {"prompt": "Seed."}, "outputs": [{"name": "o", "type": "json"}]},
                {
                    "id": "act",
                    "subagent": {"prompt": "Act."},
                    "inputs": [{"name": "data", "type": "json", "from": "seed.o"}],
                    "max_entries": 2,
                },
            ],
            "transitions": [
                {"from": "seed", "to": "act", "when": {"output": "o", "path": "ready", "op": "eq", "value": False}},
            ],
        }
        self.assertEqual(
            canonicalize_factory_spec(machine),
            {
                "run": {
                    "failure_policy": "continue",
                    "max_parallel": 2,
                    "max_transitions": 20,
                    "max_children": 10_000,
                    "budget_ms": 5000,
                },
                "states": [
                    {
                        "id": "seed",
                        "entry": True,
                        "max_entries": 1,
                        "lifecycle": "task",
                        "retries": 0,
                        "failure_policy": "continue",
                        "subagent": {"prompt": "Seed."},
                        "outputs": [{"name": "o", "type": "json"}],
                    },
                    {
                        "id": "act",
                        "entry": False,
                        "max_entries": 2,
                        "subagent": {"prompt": "Act."},
                        "lifecycle": "task",
                        "retries": 0,
                        "failure_policy": "continue",
                        "inputs": [{"name": "data", "type": "json", "from": "seed.o"}],
                    }
                ],
                "transitions": [
                    {
                        "from": "seed",
                        "to": "act",
                        "on": "settled",
                        "when": {"output": "o", "path": "ready", "op": "eq", "value": False},
                    }
                ],
            },
        )

    def test_raises_with_joined_errors_on_invalid_input(self) -> None:
        with self.assertRaises(ValueError) as ctx:
            canonicalize_factory_spec({"nodes": [node("a", depends_on=["ghost"])]})
        message = str(ctx.exception)
        self.assertIn("depends on unknown node", message)

        with self.assertRaises(ValueError) as ctx:
            canonicalize_factory_spec("not a dag")
        self.assertIn("factory dag must be a JSON object", str(ctx.exception))

        with self.assertRaises(ValueError) as ctx:
            canonicalize_factory_spec({"states": [state("a")], "transitions": [{"from": "a", "to": "ghost"}]})
        self.assertIn("references unknown to-state", str(ctx.exception))

    def test_does_not_mutate_input(self) -> None:
        dag = {"nodes": [{"id": "a", "subagent": {"prompt": "p"}, "outputs": [{"name": "o", "type": "json"}]}]}
        snapshot = {"nodes": [dict(dag["nodes"][0])]}
        result = canonicalize_factory_spec(dag)
        result["states"][0]["subagent"]["prompt"] = "mutated"
        result["states"][0]["outputs"][0]["type"] = "text"
        self.assertEqual(dag["nodes"][0]["subagent"]["prompt"], "p")
        self.assertEqual(dag["nodes"][0]["outputs"][0]["type"], "json")
        self.assertEqual(snapshot["nodes"][0]["id"], "a")

        machine = {
            "states": [
                state("a", entry=True, outputs=[{"name": "o", "type": "json"}]),
                state("b", inputs=[{"name": "i", "type": "json", "from": "a.o"}]),
            ],
            "transitions": [{"from": "a", "to": "b", "when": {"output": "o", "op": "exists"}}],
        }
        result = canonicalize_factory_spec(machine)
        result["states"][0]["outputs"][0]["type"] = "mutated"
        result["transitions"][0]["when"]["output"] = "mutated"
        self.assertEqual(machine["states"][0]["outputs"][0]["type"], "json")
        self.assertEqual(machine["transitions"][0]["when"]["output"], "o")


# ---------------------------------------------------------------------------
# Machine-form validation
# ---------------------------------------------------------------------------


class ValidateFactoryMachineTest(unittest.TestCase):
    """Every machine-form validator rule, valid and invalid."""

    def test_valid_machine_has_no_errors(self) -> None:
        # Includes an entry state, a guard switch, a self-loop, re-entry
        # bounds, and a reviewing<->fixing cycle: all legal in machine form.
        self.assertEqual(validate_factory_spec(valid_machine()), [])
        self.assertEqual(validate_factory_machine(valid_machine()), [])

    def test_machine_must_be_an_object(self) -> None:
        for bad in (None, [], "states", 42):
            self.assertEqual(validate_factory_machine(bad), ["factory machine must be a JSON object"], repr(bad))

    def test_states_required_and_must_be_a_list(self) -> None:
        self.assertEqual(
            validate_factory_machine({"transitions": []}),
            ["factory machine requires a states list"],
        )
        self.assertEqual(
            validate_factory_machine({"states": "nope"}),
            ["factory machine requires a states list"],
        )
        self.assertEqual(
            validate_factory_machine({"run": "bad", "states": "nope"}),
            ["run must be an object", "factory machine requires a states list"],
        )

    def test_state_cap(self) -> None:
        at_cap = {"states": [state(f"s{i}") for i in range(1024)], "transitions": []}
        at_cap["states"][0]["entry"] = True
        self.assertEqual(validate_factory_machine(at_cap), [])
        over_cap = {"states": [state(f"s{i}") for i in range(1025)], "transitions": []}
        over_cap["states"][0]["entry"] = True
        self.assertEqual(
            validate_factory_machine(over_cap),
            ["factory machine must declare between 1 and 1024 states, got 1025"],
        )
        empty = {"states": [], "transitions": []}
        self.assertEqual(
            validate_factory_machine(empty),
            ["factory machine must declare between 1 and 1024 states, got 0"],
        )

    def test_state_ids(self) -> None:
        for good in ("a", "state-1", "1st-state", "a" * 64):
            machine = {"states": [{"id": good, "entry": True, "subagent": "w"}], "transitions": []}
            self.assertEqual(validate_factory_machine(machine), [], good)
        for bad in ("-abc", "ABC", "a_b", "a.b", "a" * 65):
            machine = {"states": [{"id": bad, "entry": True, "subagent": "w"}], "transitions": []}
            errors = validate_factory_machine(machine)
            self.assertEqual(len(errors), 1, bad)
            self.assertIn("id must match", errors[0])
        for bad in ("", None, 5):
            machine = {"states": [{"id": bad, "subagent": "w"}], "transitions": []}
            self.assertEqual(
                validate_factory_machine(machine),
                ["states[0] requires a non-empty id"],
                repr(bad),
            )

    def test_duplicate_state_ids(self) -> None:
        machine = {"states": [state("dup"), state("dup")], "transitions": []}
        machine["states"][0]["entry"] = True
        errors = validate_factory_machine(machine)
        self.assertEqual(len(errors), 1)
        self.assertIn("duplicates state id 'dup'", errors[0])

    def test_entry_state_required(self) -> None:
        no_entry = {"states": [state("a"), state("b")], "transitions": [{"from": "a", "to": "b"}]}
        self.assertEqual(
            validate_factory_machine(no_entry),
            ["factory machine requires at least one entry state"],
        )
        explicit_false = {"states": [state("a", entry=False)], "transitions": []}
        self.assertEqual(
            validate_factory_machine(explicit_false),
            ["factory machine requires at least one entry state"],
        )
        bad_flag = {"states": [state("a", entry="yes")], "transitions": []}
        self.assertEqual(
            validate_factory_machine(bad_flag),
            ["state a entry must be a boolean", "factory machine requires at least one entry state"],
        )

    def test_optional_input_flag(self) -> None:
        ok = {
            "states": [
                {"id": "seed", "entry": True, "subagent": "w", "outputs": [{"name": "o", "type": "json"}]},
                {
                    "id": "loop",
                    "subagent": "w",
                    "inputs": [{"name": "o", "type": "json", "from": "seed.o", "optional": True}],
                },
            ],
            "transitions": [{"from": "seed", "to": "loop"}],
        }
        self.assertEqual(validate_factory_machine(ok), [])
        for bad in ("yes", 1, []):
            machine = {
                "states": [
                    state("seed", entry=True, outputs=[{"name": "o", "type": "json"}]),
                    {
                        "id": "loop",
                        "subagent": "w",
                        "inputs": [{"name": "o", "type": "json", "from": "seed.o", "optional": bad}],
                    },
                ],
                "transitions": [{"from": "seed", "to": "loop"}],
            }
            self.assertEqual(
                validate_factory_machine(machine),
                ["state loop input 'o' optional must be a boolean when provided"],
                repr(bad),
            )

    def test_required_self_input_is_rejected_optional_stays_the_loop_form(self) -> None:
        # A required self-input can never bind: the first entry waits for its
        # own prior settle, which cannot exist yet -- the entry stays pending
        # until the stall detector fails the run naming the state. Optional
        # self-inputs are the designed loop form (first entry binds the null
        # sentinel) and stay valid; the rule is the machine-form mirror of
        # the dag compiler's "cannot depend on itself" rejection.
        absent = object()  # sentinel: the optional key is omitted entirely

        def machine(optional: Any = absent) -> dict[str, Any]:
            loop_inputs: list[dict[str, Any]] = [{"name": "last", "type": "text", "from": "loop.last"}]
            if optional is not absent:
                loop_inputs[0]["optional"] = optional
            return {
                "states": [
                    {"id": "seed", "entry": True, "subagent": "w", "outputs": [{"name": "go", "type": "text"}]},
                    {
                        "id": "loop",
                        "subagent": "w",
                        "inputs": loop_inputs,
                        "outputs": [{"name": "last", "type": "text"}],
                        "max_entries": 3,
                    },
                ],
                "transitions": [{"from": "seed", "to": "loop"}, {"from": "loop", "to": "loop"}],
            }

        self.assertEqual(validate_factory_machine(machine(True)), [])
        for required in (False, absent):
            self.assertEqual(
                validate_factory_machine(machine(required)),
                [
                    "state loop input 'last' cannot require itself: mark the self-input optional - "
                    "a required one can never bind on the state's first entry"
                ],
                repr(required),
            )

    def test_entry_states_cannot_declare_inputs(self) -> None:
        with_inputs = {
            "states": [
                state("src", entry=True, inputs=[{"name": "i", "type": "text", "from": "peer.o"}]),
                state("peer", outputs=[{"name": "o", "type": "text"}]),
            ],
            "transitions": [{"from": "peer", "to": "src"}],
        }
        self.assertEqual(
            validate_factory_machine(with_inputs),
            ["entry state src cannot declare inputs"],
        )
        entry_without_inputs = {"states": [state("a", entry=True)], "transitions": []}
        self.assertEqual(validate_factory_machine(entry_without_inputs), [])
        # Compiled dags only mark dep-free nodes as entry states, so the rule
        # never fires on the dag path.
        dag = {
            "nodes": [
                node("a", outputs=[{"name": "o", "type": "text"}]),
                node("b", depends_on=["a"], inputs=[{"name": "i", "type": "text", "from": "a.o"}]),
            ]
        }
        self.assertEqual(validate_factory_spec(dag), [])

    def test_max_entries(self) -> None:
        for good in (1, 2, 99):
            machine = {"states": [state("a", entry=True, max_entries=good)], "transitions": []}
            self.assertEqual(validate_factory_machine(machine), [], good)
        for bad in (0, -1, 1.5, "2", True):
            machine = {"states": [state("a", entry=True, max_entries=bad)], "transitions": []}
            self.assertEqual(
                validate_factory_machine(machine),
                ["state a max_entries must be an integer >= 1"],
                repr(bad),
            )

    def test_subagent_required(self) -> None:
        missing = {"states": [{"id": "a", "entry": True}], "transitions": []}
        errors = validate_factory_machine(missing)
        self.assertEqual(len(errors), 1)
        self.assertIn("requires a subagent", errors[0])
        self.assertIn("state a", errors[0])

        empty_ref = {"states": [state("a", entry=True, subagent="")], "transitions": []}
        errors = validate_factory_machine(empty_ref)
        self.assertEqual(len(errors), 1)
        self.assertIn("requires a subagent", errors[0])

        empty_prompt = {"states": [state("a", entry=True, subagent={"prompt": ""})], "transitions": []}
        errors = validate_factory_machine(empty_prompt)
        self.assertEqual(len(errors), 1)
        self.assertIn("requires a non-empty prompt", errors[0])

        bad_model = {"states": [state("a", entry=True, subagent={"prompt": "p", "model": 5})], "transitions": []}
        errors = validate_factory_machine(bad_model)
        self.assertEqual(len(errors), 1)
        self.assertIn("model must be a non-empty string", errors[0])

        # Whitespace-only inline fields are write-time invalid in machine
        # form too (the shared state validation strips like the runtime
        # resolvers do), so no persistable machine can be unspawnable.
        whitespace = {
            "states": [state("a", entry=True, subagent={"prompt": "  ", "model": " \t "})],
            "transitions": [],
        }
        self.assertEqual(
            validate_factory_machine(whitespace),
            [
                "state a inline subagent requires a non-empty prompt",
                "state a inline subagent model must be a non-empty string when provided",
            ],
        )

        inline_ok = {
            "states": [state("a", entry=True, subagent={"prompt": "Do work.", "name": "w", "thinking": "high"})],
            "transitions": [],
        }
        self.assertEqual(validate_factory_machine(inline_ok), [])

    def test_inline_subagent_name_length_and_uniqueness(self) -> None:
        # The configured name labels spawned children, and the host caps
        # subagent session names at 64 characters: reject an over-length
        # name at write time (a persistable factory must be spawnable)
        # instead of failing every spawn admission.
        too_long = {
            "states": [state("a", entry=True, subagent={"prompt": "p", "name": "x" * (SUBAGENT_NAME_MAX_LENGTH + 1)})],
            "transitions": [],
        }
        self.assertEqual(
            validate_factory_machine(too_long),
            [
                f"state a inline subagent name must be at most {SUBAGENT_NAME_MAX_LENGTH} characters, "
                f"got {SUBAGENT_NAME_MAX_LENGTH + 1}"
            ],
        )

        # Two states configured with one name would collide on the
        # supervisor's unique sibling-name requirement at spawn time: reject
        # the duplicate at write time, like duplicate state ids.
        duplicate = {
            "states": [
                {"id": "a", "entry": True, "subagent": {"prompt": "p", "name": "reviewer"}},
                {"id": "b", "subagent": {"prompt": "p", "name": "reviewer"}},
            ],
            "transitions": [{"from": "a", "to": "b"}],
        }
        self.assertEqual(
            validate_factory_machine(duplicate),
            ["state b subagent name 'reviewer' is already configured by state 'a'"],
        )
        # Distinct configured names are fine, and string references never
        # carry a name.
        distinct = {
            "states": [
                {"id": "a", "entry": True, "subagent": {"prompt": "p", "name": "reviewer"}},
                {"id": "b", "subagent": {"prompt": "p", "name": "fixer"}},
                {"id": "c", "subagent": "worker"},
            ],
            "transitions": [{"from": "a", "to": "b"}, {"from": "b", "to": "c"}],
        }
        self.assertEqual(validate_factory_machine(distinct), [])

        # A name another state's name can suffix onto (Macroscope review
        # finding: foo's later instances spawn foo-i1, so a state configured
        # foo-i1 collides at spawn time) is rejected at write time, in
        # either order; the attempt chain form is caught too.
        shadowing = {
            "states": [
                {"id": "a", "entry": True, "subagent": {"prompt": "p", "name": "foo"}},
                {"id": "b", "subagent": {"prompt": "p", "name": "foo-i1"}},
            ],
            "transitions": [{"from": "a", "to": "b"}],
        }
        errors = validate_factory_machine(shadowing)
        self.assertEqual(len(errors), 1)
        self.assertIn("collides with the suffixed spawn labels of state 'a'", errors[0])
        self.assertIn("re-entry, foreach, and retries name children 'foo'-i<n> and 'foo'-a<n>", errors[0])
        # Reversed declaration order: the CURRENT state's name generates the
        # suffixed labels, and the message must attribute them to it, not to
        # the earlier state (Cursor review finding).
        reversed_shadowing = {
            "states": [
                {"id": "a", "entry": True, "subagent": {"prompt": "p", "name": "foo-i1"}},
                {"id": "b", "subagent": {"prompt": "p", "name": "foo"}},
            ],
            "transitions": [{"from": "a", "to": "b"}],
        }
        errors = validate_factory_machine(reversed_shadowing)
        self.assertEqual(len(errors), 1)
        self.assertEqual(
            errors[0],
            "state b subagent name 'foo' suffixed by re-entry, foreach, and retries "
            "('foo'-i<n>, 'foo'-a<n>) collides with state 'a' (configured 'foo-i1')",
        )
        for shadowed_name in ("foo-a2", "foo-i1-a2", "foo-i9"):
            machine = {
                "states": [
                    {"id": "a", "entry": True, "subagent": {"prompt": "p", "name": "foo"}},
                    {"id": "b", "subagent": {"prompt": "p", "name": shadowed_name}},
                ],
                "transitions": [{"from": "a", "to": "b"}],
            }
            self.assertEqual(len(validate_factory_machine(machine)), 1, shadowed_name)
        # The never-generated -i0/-a1 do not shadow, and unrelated names pass.
        no_shadow = {
            "states": [
                {"id": "a", "entry": True, "subagent": {"prompt": "p", "name": "foo"}},
                {"id": "b", "subagent": {"prompt": "p", "name": "foo-i0"}},
                {"id": "c", "subagent": {"prompt": "p", "name": "foo-a1"}},
                {"id": "d", "subagent": {"prompt": "p", "name": "bar-i1"}},
            ],
            "transitions": [{"from": "a", "to": "b"}, {"from": "b", "to": "c"}, {"from": "c", "to": "d"}],
        }
        self.assertEqual(validate_factory_machine(no_shadow), [])

    def test_wait_states_are_gated_until_the_communication_series(self) -> None:
        # The rlm.watch.* host handlers do not exist on this stack, so wait
        # blocks are rejected outright (machine form and dag form alike).
        machine = {
            "states": [
                {"id": "watch", "entry": True, "subagent": "w", "wait": {"kind": "path", "target": "/tmp/x", "timeout_ms": 5}},
                state("act"),
            ],
            "transitions": [{"from": "watch", "to": "act"}],
        }
        errors = validate_factory_machine(machine)
        self.assertEqual(
            errors,
            [
                "state watch: wait states require the watch host handlers (rlm.watch.*); "
                "they arrive with the communication series - remove the wait block until then"
            ],
        )
        self.assertEqual(validate_factory_spec(machine), errors)

        dag_wait = {"nodes": [node("a", wait={"kind": "path", "target": "t", "timeout_ms": 5})]}
        errors = validate_factory_spec(dag_wait)
        self.assertEqual(
            errors,
            [
                "node a: wait states require the watch host handlers (rlm.watch.*); "
                "they arrive with the communication series - remove the wait block until then"
            ],
        )

    def test_resident_states(self) -> None:
        resident_ok = {
            "states": [
                {"id": "entry", "entry": True, "subagent": "w", "outputs": [{"name": "o", "type": "text"}]},
                {"id": "watcher", "subagent": "w", "lifecycle": "resident"},
            ],
            "transitions": [{"from": "entry", "to": "watcher"}],
        }
        self.assertEqual(validate_factory_machine(resident_ok), [])

        declares_outputs = {
            "states": [state("watcher", entry=True, lifecycle="resident", outputs=[{"name": "o", "type": "text"}])],
            "transitions": [],
        }
        self.assertEqual(
            validate_factory_machine(declares_outputs),
            ["resident state watcher cannot declare outputs"],
        )
        uses_foreach = {
            "states": [
                state("src", entry=True, outputs=[{"name": "items", "type": "json"}]),
                {
                    "id": "watcher",
                    "subagent": "w",
                    "lifecycle": "resident",
                    "inputs": [{"name": "items", "type": "json", "from": "src.items"}],
                    "foreach": {"over": "items", "max": 4},
                },
            ],
            "transitions": [{"from": "src", "to": "watcher"}],
        }
        self.assertEqual(validate_factory_machine(uses_foreach), ["resident state watcher cannot use foreach"])
        leaves_resident = {
            "states": [
                {"id": "entry", "entry": True, "subagent": "w"},
                {"id": "watcher", "subagent": "w", "lifecycle": "resident"},
            ],
            "transitions": [{"from": "watcher", "to": "entry"}],
        }
        self.assertEqual(
            validate_factory_machine(leaves_resident),
            ["transitions[0] cannot leave resident state 'watcher'"],
        )

    def test_input_cannot_read_from_resident_state(self) -> None:
        machine = {
            "states": [
                state("watcher", lifecycle="resident"),
                state("task", inputs=[{"name": "i", "type": "text", "from": "watcher.o"}]),
                state("seed", entry=True),
            ],
            "transitions": [],
        }
        self.assertEqual(
            validate_factory_machine(machine),
            ["state task input 'i' cannot read from resident state 'watcher'"],
        )

    def test_port_rules(self) -> None:
        dup_output = {"states": [state("a", entry=True, outputs=[{"name": "o", "type": "text"}, {"name": "o", "type": "json"}])], "transitions": []}
        self.assertEqual(validate_factory_machine(dup_output), ["state a declares duplicate output name 'o'"])
        bad_type = {"states": [state("a", entry=True, outputs=[{"name": "o", "type": "yaml"}])], "transitions": []}
        self.assertEqual(validate_factory_machine(bad_type), ["state a output 'o' type must be 'text' or 'json'"])
        unknown_source = {
            "states": [state("a", entry=True), state("b", inputs=[{"name": "i", "type": "text", "from": "ghost.o"}])],
            "transitions": [],
        }
        self.assertEqual(
            validate_factory_machine(unknown_source),
            ["state b input 'i' references unknown state 'ghost'"],
        )
        undeclared_output = {
            "states": [
                state("a", entry=True),
                state("b", inputs=[{"name": "i", "type": "text", "from": "a.missing"}]),
            ],
            "transitions": [],
        }
        self.assertEqual(
            validate_factory_machine(undeclared_output),
            ["state b input 'i' references output 'missing' that state 'a' does not declare"],
        )
        type_mismatch = {
            "states": [
                state("a", entry=True, outputs=[{"name": "o", "type": "text"}]),
                state("b", inputs=[{"name": "i", "type": "json", "from": "a.o"}]),
            ],
            "transitions": [],
        }
        errors = validate_factory_machine(type_mismatch)
        self.assertEqual(len(errors), 1)
        self.assertIn("cannot read from output", errors[0])
        malformed = {
            "states": [
                state("a", entry=True, outputs=[{"name": "o", "type": "text"}]),
                state("b", inputs=[{"name": "i", "type": "text", "from": "nodot"}]),
            ],
            "transitions": [],
        }
        errors = validate_factory_machine(malformed)
        self.assertEqual(len(errors), 1)
        self.assertIn("requires a 'from' reference", errors[0])

    def test_budgets_and_retries_and_policies(self) -> None:
        over = {
            "run": {"budget_ms": 1000},
            "states": [state("a", entry=True, budget_ms=1001)],
            "transitions": [],
        }
        self.assertEqual(
            validate_factory_machine(over),
            ["state a budget_ms 1001 exceeds the run budget_ms 1000"],
        )
        for bad in (0, -5, 1.5, "10", True):
            machine = {"run": {"budget_ms": bad}, "states": [state("a", entry=True)], "transitions": []}
            self.assertEqual(validate_factory_machine(machine), ["run budget_ms must be a positive integer"], bad)
            machine = {"states": [state("a", entry=True, budget_ms=bad)], "transitions": []}
            self.assertEqual(validate_factory_machine(machine), ["state a budget_ms must be a positive integer"], bad)
        for bad in (-1, 11, 1.5, "2", True):
            machine = {"states": [state("a", entry=True, retries=bad)], "transitions": []}
            self.assertEqual(
                validate_factory_machine(machine),
                ["state a retries must be an integer between 0 and 10"],
                bad,
            )
        bad_policy = {"states": [state("a", entry=True, failure_policy="retry")], "transitions": []}
        self.assertEqual(
            validate_factory_machine(bad_policy),
            ["state a failure_policy must be one of ['fail_fast', 'continue', 'escalate'], got 'retry'"],
        )
        bad_lifecycle = {"states": [state("a", entry=True, lifecycle="daemon")], "transitions": []}
        self.assertEqual(
            validate_factory_machine(bad_lifecycle),
            ["state a lifecycle must be 'task' or 'resident', got 'daemon'"],
        )

    def test_run_max_transitions(self) -> None:
        for good in (1, 40, 10_000):
            machine = {"run": {"max_transitions": good}, "states": [state("a", entry=True)], "transitions": []}
            self.assertEqual(validate_factory_machine(machine), [], good)
        for bad in (0, -1, 10_001, 1.5, "5", True):
            machine = {"run": {"max_transitions": bad}, "states": [state("a", entry=True)], "transitions": []}
            self.assertEqual(
                validate_factory_machine(machine),
                ["run max_transitions must be a positive integer no greater than 10000"],
                bad,
            )

    def test_foreach_rules(self) -> None:
        ok = {
            "states": [
                state("a", entry=True, outputs=[{"name": "items", "type": "json"}]),
                state(
                    "b",
                    inputs=[{"name": "items", "type": "json", "from": "a.items"}],
                    foreach={"over": "items", "max": 16},
                ),
            ],
            "transitions": [{"from": "a", "to": "b"}],
        }
        self.assertEqual(validate_factory_machine(ok), [])
        wrong_port = {
            "states": [
                state("a", entry=True, outputs=[{"name": "items", "type": "json"}]),
                state(
                    "b",
                    inputs=[{"name": "items", "type": "json", "from": "a.items"}],
                    foreach={"over": "not-an-input", "max": 4},
                ),
            ],
            "transitions": [{"from": "a", "to": "b"}],
        }
        self.assertEqual(
            validate_factory_machine(wrong_port),
            ["state b foreach.over must name one of this state's inputs, got 'not-an-input'"],
        )
        text_port = {
            "states": [
                state("a", entry=True, outputs=[{"name": "draft", "type": "text"}]),
                state(
                    "b",
                    inputs=[{"name": "draft", "type": "text", "from": "a.draft"}],
                    foreach={"over": "draft", "max": 4},
                ),
            ],
            "transitions": [{"from": "a", "to": "b"}],
        }
        self.assertEqual(validate_factory_machine(text_port), ["state b foreach.over input 'draft' must have type 'json'"])
        bad_max = {
            "states": [
                state("a", entry=True, outputs=[{"name": "items", "type": "json"}]),
                state(
                    "b",
                    inputs=[{"name": "items", "type": "json", "from": "a.items"}],
                    foreach={"over": "items", "max": 257},
                ),
            ],
            "transitions": [{"from": "a", "to": "b"}],
        }
        self.assertEqual(validate_factory_machine(bad_max), ["state b foreach.max must be an integer between 1 and 256"])
        not_object = {"states": [state("a", entry=True, foreach=["bad"])], "transitions": []}
        self.assertEqual(validate_factory_machine(not_object), ["state a foreach must be an object"])

    def test_transitions_reference_existing_states(self) -> None:
        unknown_from = {
            "states": [state("a", entry=True)],
            "transitions": [{"from": "ghost", "to": "a"}],
        }
        self.assertEqual(
            validate_factory_machine(unknown_from),
            ["transitions[0] references unknown from-state 'ghost'"],
        )
        unknown_to = {"states": [state("a", entry=True)], "transitions": [{"from": "a", "to": "ghost"}]}
        self.assertEqual(
            validate_factory_machine(unknown_to),
            ["transitions[0] references unknown to-state 'ghost'"],
        )
        not_object = {"states": [state("a", entry=True)], "transitions": ["bad"]}
        self.assertEqual(validate_factory_machine(not_object), ["transitions[0] must be an object"])
        not_a_list = {"states": [state("a", entry=True)], "transitions": "bad"}
        self.assertEqual(validate_factory_machine(not_a_list), ["factory machine transitions must be a list"])
        bad_on = {"states": [state("a", entry=True)], "transitions": [{"from": "a", "to": "a", "on": "manual"}]}
        self.assertEqual(
            validate_factory_machine(bad_on),
            ["transitions[0] on must be one of ['settled'], got 'manual'"],
        )

    def test_join_transitions_from_a_list(self) -> None:
        # A ``from`` LIST is a join transition: it fires once every source
        # state settled (the compiled dag fan-in shape).
        ok = {
            "states": [
                state("a", entry=True),
                state("b", entry=True),
                state("d"),
            ],
            "transitions": [{"from": ["a", "b"], "to": "d"}],
        }
        self.assertEqual(validate_factory_machine(ok), [])

        unknown_source = {
            "states": [state("a", entry=True), state("d")],
            "transitions": [{"from": ["a", "ghost"], "to": "d"}],
        }
        self.assertEqual(
            validate_factory_machine(unknown_source),
            ["transitions[0] references unknown from-state 'ghost'"],
        )
        repeated_source = {
            "states": [state("a", entry=True), state("d")],
            "transitions": [{"from": ["a", "a"], "to": "d"}],
        }
        self.assertEqual(
            validate_factory_machine(repeated_source),
            ["transitions[0] from must not repeat a state"],
        )
        empty_source = {
            "states": [state("a", entry=True), state("d")],
            "transitions": [{"from": [], "to": "d"}],
        }
        self.assertEqual(
            validate_factory_machine(empty_source),
            ["transitions[0] from must name at least one state"],
        )
        non_string_entry = {
            "states": [state("a", entry=True), state("d")],
            "transitions": [{"from": ["a", 5], "to": "d"}],
        }
        self.assertEqual(
            validate_factory_machine(non_string_entry),
            ["transitions[0] from entries must be non-empty state id strings"],
        )
        # A guard needs exactly one from-state's latest settle output.
        guard_on_join = {
            "states": [
                state("a", entry=True),
                state("b", entry=True),
                state("d"),
            ],
            "transitions": [
                {"from": ["a", "b"], "to": "d", "when": {"output": "o", "op": "exists"}},
            ],
        }
        self.assertEqual(
            validate_factory_machine(guard_on_join),
            [
                "transitions[0] with multiple from-states cannot carry a when guard; "
                "use single-state transitions for guards"
            ],
        )
        resident_source = {
            "states": [
                state("a", entry=True),
                {"id": "watcher", "subagent": "w", "lifecycle": "resident"},
                state("d"),
            ],
            "transitions": [{"from": ["a", "watcher"], "to": "d"}],
        }
        self.assertEqual(
            validate_factory_machine(resident_source),
            ["transitions[0] cannot leave resident state 'watcher'"],
        )
        # Regression (bot review): UNHASHABLE malformed entries (a dict, a
        # list) used to raise a raw TypeError from the set() dedupe before
        # the type check; validation must report an error instead.
        for bad_from in ([{}], [[1]], ["a", {}]):
            malformed = {
                "states": [state("a", entry=True), state("d")],
                "transitions": [{"from": bad_from, "to": "d"}],
            }
            self.assertEqual(
                validate_factory_machine(malformed),
                ["transitions[0] from entries must be non-empty state id strings"],
            )
            self.assertEqual(
                validate_factory_spec(
                    {
                        "states": [state("a", entry=True), state("d")],
                        "transitions": [{"from": bad_from, "to": "d"}],
                    }
                ),
                ["transitions[0] from entries must be non-empty state id strings"],
            )

    def test_self_loop_is_legal_re_entry(self) -> None:
        machine = {
            "states": [state("a", entry=True, max_entries=5, outputs=[{"name": "o", "type": "json"}])],
            "transitions": [{"from": "a", "to": "a"}],
        }
        self.assertEqual(validate_factory_machine(machine), [])

    def test_cyclic_machine_validates(self) -> None:
        machine = {
            "states": [
                state("seed", entry=True, outputs=[{"name": "o", "type": "text"}]),
                state("b"),
                state("c"),
            ],
            "transitions": [
                {"from": "seed", "to": "b"},
                {"from": "b", "to": "c"},
                {"from": "c", "to": "b"},
            ],
        }
        self.assertEqual(validate_factory_machine(machine), [])
        self.assertEqual(validate_factory_spec(machine), [])

    def test_guard_values_must_be_finite(self) -> None:
        # Regression (bot review): a non-finite float in a guard comparison
        # value serializes as the non-JSON NaN/Infinity tokens and breaks
        # every strict consumer of the activity reply frames (the host
        # bridge's parser included) — validation rejects them at the
        # machine's source, deeply (a contains needle list carries the
        # same rule).
        def machine_with(when: Any) -> dict[str, Any]:
            return {
                "states": [
                    state("a", entry=True, outputs=[{"name": "verdict", "type": "json"}]),
                    state("b"),
                ],
                "transitions": [{"from": "a", "to": "b", "when": when}],
            }

        for bad in (float("nan"), float("inf"), float("-inf")):
            self.assertEqual(
                validate_factory_machine(
                    machine_with({"output": "verdict", "op": "eq", "value": bad})
                ),
                ["transitions[0] when.value must be finite JSON data (JSON carries no NaN or Infinity, and only JSON shapes serialize: lists, objects, strings, numbers, booleans, null, and no container nests deeper than 256 levels)"],
                repr(bad),
            )
        nested = machine_with(
            {"output": "verdict", "op": "contains", "value": ["ok", {"x": float("nan")}]}
        )
        self.assertEqual(
            validate_factory_machine(nested),
            ["transitions[0] when.value must be finite JSON data (JSON carries no NaN or Infinity, and only JSON shapes serialize: lists, objects, strings, numbers, booleans, null, and no container nests deeper than 256 levels)"],
        )

    def test_guard_value_object_keys_must_be_strings(self) -> None:
        # The same wire-cleanliness rule at the object's keys: a
        # non-finite float key serializes as the non-JSON ``NaN`` token
        # and breaks the strict consumers, and a non-string key is either
        # coerced by the encoder (the wire object no longer matches the
        # declared machine) or rejected by it — a guard declaring one
        # never survives the reply frames.
        def machine_with(value: Any) -> dict[str, Any]:
            return {
                "states": [
                    state("a", entry=True, outputs=[{"name": "verdict", "type": "json"}]),
                    state("b"),
                ],
                "transitions": [
                    {"from": "a", "to": "b", "when": {"output": "verdict", "op": "contains", "value": value}}
                ],
            }

        for bad in (
            {float("nan"): 1},
            {1: "x"},
            {"ok": {("tuple",): 2}},
            # Non-JSON container leaves reject at the source: a tuple
            # serializes as something other than the declared shape (an
            # array) if the encoder accepts it at all, and the floats it
            # carries would ride past the finiteness traversal.
            (float("nan"),),
            ("plain", "tuple"),
            {"set", "of", "strings"},
            b"bytes",
        ):
            self.assertEqual(
                validate_factory_machine(machine_with(["ok", bad])),
                ["transitions[0] when.value must be finite JSON data (JSON carries no NaN or Infinity, and only JSON shapes serialize: lists, objects, strings, numbers, booleans, null, and no container nests deeper than 256 levels)"],
                repr(bad),
            )
        # String keys with finite values stay valid.
        self.assertEqual(
            validate_factory_machine(machine_with(["ok", {"flag": True, "nested": {"count": 2}}])),
            [],
        )

    def test_guard_value_cycles_reject_without_exhausting_the_stack(self) -> None:
        # A self-referential container can never serialize (the encoder
        # refuses circular references outright), so it is not a valid
        # comparison value: the traversal must reject it at the cycle
        # instead of chasing it to a RecursionError, and the write path
        # must answer the validation error, not crash.
        def machine_with(value: Any) -> dict[str, Any]:
            return {
                "states": [
                    state("a", entry=True, outputs=[{"name": "verdict", "type": "json"}]),
                    state("b"),
                ],
                "transitions": [
                    {"from": "a", "to": "b", "when": {"output": "verdict", "op": "contains", "value": value}}
                ],
            }

        cycle: list[Any] = ["ok"]
        cycle.append(cycle)
        self.assertEqual(
            validate_factory_machine(machine_with(cycle)),
            ["transitions[0] when.value must be finite JSON data (JSON carries no NaN or Infinity, and only JSON shapes serialize: lists, objects, strings, numbers, booleans, null, and no container nests deeper than 256 levels)"],
        )
        nested: dict[str, Any] = {"flag": True}
        nested["self"] = nested
        self.assertEqual(
            validate_factory_machine(machine_with([nested])),
            ["transitions[0] when.value must be finite JSON data (JSON carries no NaN or Infinity, and only JSON shapes serialize: lists, objects, strings, numbers, booleans, null, and no container nests deeper than 256 levels)"],
        )
        # A shared-but-acyclic reference is NOT a cycle: the same object
        # appearing twice (a diamond) stays a valid comparison value.
        shared = {"flag": True}
        self.assertEqual(validate_factory_machine(machine_with([shared, shared])), [])

    def test_guard_value_depth_rejects_without_exhausting_the_stack(self) -> None:
        # Deep-but-ACYCLIC nesting is the cycle rule's other half: the
        # traversal descends one level per recursion, so a value nested
        # past the interpreter's stack would raise RecursionError on the
        # write path instead of answering the validation error. Every
        # downstream seam recurses per level the same way (the snapshot's
        # deep copy, the wire conversion, the reply frames' encoder), so
        # the nesting rejects at the bound with the same message — and a
        # value under the bound (realistic guard values nest a handful of
        # levels) stays valid.
        def machine_with(value: Any) -> dict[str, Any]:
            return {
                "states": [
                    state("a", entry=True, outputs=[{"name": "verdict", "type": "json"}]),
                    state("b"),
                ],
                "transitions": [
                    {"from": "a", "to": "b", "when": {"output": "verdict", "op": "contains", "value": value}}
                ],
            }

        deep: list[Any] = []
        node = deep
        for _ in range(factory_module.MAX_GUARD_VALUE_DEPTH + 50):
            child: list[Any] = []
            node.append(child)
            node = child
        self.assertEqual(
            validate_factory_machine(machine_with(deep)),
            ["transitions[0] when.value must be finite JSON data (JSON carries no NaN or Infinity, and only JSON shapes serialize: lists, objects, strings, numbers, booleans, null, and no container nests deeper than 256 levels)"],
        )
        within: list[Any] = ["verdict"]
        for _ in range(10):
            within = [within]
        self.assertEqual(validate_factory_machine(machine_with(within)), [])

    def test_guard_rules(self) -> None:
        def machine_with(when: Any) -> dict[str, Any]:
            return {
                "states": [state("a", entry=True, outputs=[{"name": "verdict", "type": "json"}]), state("b")],
                "transitions": [{"from": "a", "to": "b", "when": when}],
            }

        unknown_output = machine_with({"output": "missing", "op": "exists"})
        self.assertEqual(
            validate_factory_machine(unknown_output),
            ["transitions[0] when.output 'missing' is not a declared output of state 'a'"],
        )
        empty_output = machine_with({"output": "", "op": "exists"})
        self.assertEqual(
            validate_factory_machine(empty_output),
            ["transitions[0] when requires a non-empty output"],
        )
        not_object = machine_with("nope")
        self.assertEqual(validate_factory_machine(not_object), ["transitions[0] when must be an object"])
        path_on_text = {
            "states": [
                state("a", entry=True, outputs=[{"name": "note", "type": "text"}]),
                state("b"),
            ],
            "transitions": [{"from": "a", "to": "b", "when": {"output": "note", "path": "x", "op": "eq", "value": 1}}],
        }
        self.assertEqual(
            validate_factory_machine(path_on_text),
            ["transitions[0] when.path requires a json output, got text output 'note'"],
        )
        bad_op = machine_with({"output": "verdict", "op": "matches", "value": 1})
        self.assertEqual(
            validate_factory_machine(bad_op),
            ["transitions[0] when.op must be one of ['eq', 'ne', 'gt', 'gte', 'lt', 'lte', 'exists', 'contains'], got 'matches'"],
        )
        for op in ("gt", "gte", "lt", "lte"):
            non_numeric = machine_with({"output": "verdict", "op": op, "value": "1"})
            self.assertEqual(
                validate_factory_machine(non_numeric),
                [f"transitions[0] when.op {op!r} requires a numeric value"],
                op,
            )
        contains_needs_list = machine_with({"output": "verdict", "op": "contains", "value": "x"})
        self.assertEqual(
            validate_factory_machine(contains_needs_list),
            ["transitions[0] when.op 'contains' requires a non-empty list value"],
        )
        # Review finding: an empty list is not a legal contains needle.
        contains_empty_list = machine_with({"output": "verdict", "op": "contains", "value": []})
        self.assertEqual(
            validate_factory_machine(contains_empty_list),
            ["transitions[0] when.op 'contains' requires a non-empty list value"],
        )
        eq_rejects_list = machine_with({"output": "verdict", "op": "eq", "value": [1]})
        self.assertEqual(
            validate_factory_machine(eq_rejects_list),
            ["transitions[0] when.op 'eq' requires a scalar value"],
        )
        ne_rejects_list = machine_with({"output": "verdict", "op": "ne", "value": [1]})
        self.assertEqual(
            validate_factory_machine(ne_rejects_list),
            ["transitions[0] when.op 'ne' requires a scalar value"],
        )
        empty_path = machine_with({"output": "verdict", "path": "", "op": "exists"})
        self.assertEqual(
            validate_factory_machine(empty_path),
            ["transitions[0] when.path must be a non-empty dotted path"],
        )
        # Valid guard shapes across every op.
        for when in (
            {"output": "verdict", "path": "approved", "op": "eq", "value": False},
            {"output": "verdict", "path": "approved", "op": "ne", "value": True},
            {"output": "verdict", "path": "score", "op": "gt", "value": 1.5},
            {"output": "verdict", "path": "score", "op": "gte", "value": 2},
            {"output": "verdict", "path": "score", "op": "lt", "value": 0},
            {"output": "verdict", "path": "score", "op": "lte", "value": -3},
            {"output": "verdict", "path": "findings", "op": "exists"},
            {"output": "verdict", "path": "tags", "op": "contains", "value": ["a", "b"]},
            {"output": "verdict", "op": "exists"},
        ):
            self.assertEqual(validate_factory_machine(machine_with(when)), [], repr(when))

    def test_collects_multiple_errors(self) -> None:
        machine = {
            "run": {"max_parallel": 99, "failure_policy": "nope"},
            "states": [
                {"id": "a", "entry": True, "subagent": "w", "retries": 99},
                {"id": "b", "subagent": "w", "max_entries": 0},
            ],
            "transitions": [{"from": "a", "to": "ghost"}],
        }
        self.assertEqual(
            validate_factory_machine(machine),
            [
                "run failure_policy must be one of ['fail_fast', 'continue', 'escalate'], got 'nope'",
                "run max_parallel must be an integer between 1 and 64",
                "state a retries must be an integer between 0 and 10",
                "state b max_entries must be an integer >= 1",
                "transitions[0] references unknown to-state 'ghost'",
            ],
        )


# ---------------------------------------------------------------------------
# Dag compilation: the dag sugar compiles to machine form
# ---------------------------------------------------------------------------


class CompileFactoryDagTest(unittest.TestCase):
    def test_chain_compiles(self) -> None:
        dag = {
            "nodes": [
                node("a", outputs=[{"name": "o", "type": "text"}]),
                node("b", depends_on=["a"]),
                node("c", depends_on=["b"]),
            ]
        }
        machine, errors = compile_factory_dag(dag)
        self.assertEqual(errors, [])
        self.assertEqual(
            machine,
            {
                "states": [
                    {"id": "a", "entry": True, "max_entries": 1, "subagent": "worker", "outputs": [{"name": "o", "type": "text"}]},
                    {"id": "b", "entry": False, "max_entries": 1, "subagent": "worker"},
                    {"id": "c", "entry": False, "max_entries": 1, "subagent": "worker"},
                ],
                "transitions": [
                    {"from": "a", "to": "b"},
                    {"from": "b", "to": "c"},
                ],
            },
        )
        self.assertEqual(validate_factory_machine(machine), [])

    def test_diamond_compiles_the_fan_in_to_one_join_transition(self) -> None:
        # Review finding: a fan-in node's full effective dependency set
        # compiles to ONE join transition that waits for every predecessor.
        # Per-edge transitions would let d start after only b (or c) settled
        # and then block the other transition at max_entries, so d could run
        # with a missing input.
        dag = {
            "run": {"failure_policy": "continue", "max_parallel": 3, "budget_ms": 5000},
            "nodes": [
                node("a", outputs=[{"name": "o", "type": "text"}]),
                node("b", depends_on=["a"], outputs=[{"name": "o", "type": "text"}]),
                node("c", depends_on=["a"], outputs=[{"name": "o", "type": "text"}]),
                node(
                    "d",
                    depends_on=["b", "c"],
                    inputs=[{"name": "left", "type": "text", "from": "b.o"}, {"name": "right", "type": "text", "from": "c.o"}],
                    budget_ms=4000,
                ),
            ],
        }
        machine, errors = compile_factory_dag(dag)
        self.assertEqual(errors, [])
        self.assertEqual(
            machine,
            {
                "run": {"failure_policy": "continue", "max_parallel": 3, "budget_ms": 5000},
                "states": [
                    {"id": "a", "entry": True, "max_entries": 1, "subagent": "worker", "outputs": [{"name": "o", "type": "text"}]},
                    {"id": "b", "entry": False, "max_entries": 1, "subagent": "worker", "outputs": [{"name": "o", "type": "text"}]},
                    {"id": "c", "entry": False, "max_entries": 1, "subagent": "worker", "outputs": [{"name": "o", "type": "text"}]},
                    {
                        "id": "d",
                        "entry": False,
                        "max_entries": 1,
                        "subagent": "worker",
                        "budget_ms": 4000,
                        "inputs": [
                            {"name": "left", "type": "text", "from": "b.o"},
                            {"name": "right", "type": "text", "from": "c.o"},
                        ],
                    },
                ],
                "transitions": [
                    {"from": "a", "to": "b"},
                    {"from": "a", "to": "c"},
                    {"from": ["b", "c"], "to": "d"},
                ],
            },
        )
        self.assertEqual(validate_factory_machine(machine), [])

    def test_wide_fan_in_stays_one_join_per_target(self) -> None:
        # 300 predecessors, data edges plus depends_on: the target's compiled
        # machine carries exactly ONE join transition listing them all.
        sources = [node(f"s{i}", outputs=[{"name": "o", "type": "text"}]) for i in range(300)]
        target = node(
            "t",
            depends_on=[f"s{i}" for i in range(0, 300, 2)],
            inputs=[{"name": f"i{i}", "type": "text", "from": f"s{i}.o"} for i in range(1, 300, 2)],
        )
        machine, errors = compile_factory_dag({"nodes": [*sources, target]})
        self.assertEqual(errors, [])
        self.assertEqual(len(machine["transitions"]), 1)
        join = machine["transitions"][0]
        self.assertEqual(join["to"], "t")
        self.assertEqual(set(join["from"]), {f"s{i}" for i in range(300)})
        self.assertEqual(validate_factory_machine(machine), [])

    def test_control_only_dependency_waits_too(self) -> None:
        # The join carries depends_on-only edges as well: a node with a
        # control-only parent must still wait for that parent's settle.
        dag = {
            "nodes": [
                node("b", outputs=[{"name": "o", "type": "text"}]),
                node("c"),
                node("d", depends_on=["b", "c"], inputs=[{"name": "l", "type": "text", "from": "b.o"}]),
            ]
        }
        machine, errors = compile_factory_dag(dag)
        self.assertEqual(errors, [])
        # c contributes no data (d reads nothing from it), so the join waits
        # for it as a pure control-only parent.
        self.assertEqual(machine["transitions"], [{"from": ["b", "c"], "to": "d"}])

    def test_data_edge_alone_creates_a_transition(self) -> None:
        dag = {
            "nodes": [
                node("a", outputs=[{"name": "o", "type": "text"}]),
                node("b", inputs=[{"name": "i", "type": "text", "from": "a.o"}]),
            ]
        }
        machine, errors = compile_factory_dag(dag)
        self.assertEqual(errors, [])
        self.assertEqual(machine["states"][1]["entry"], False)
        self.assertEqual(machine["transitions"], [{"from": "a", "to": "b"}])

    def test_depends_on_and_input_edge_dedupe_into_one_transition(self) -> None:
        dag = {
            "nodes": [
                node("a", outputs=[{"name": "o", "type": "text"}]),
                node("b", depends_on=["a", "a"], inputs=[{"name": "i", "type": "text", "from": "a.o"}]),
            ]
        }
        machine, errors = compile_factory_dag(dag)
        self.assertEqual(errors, [])
        self.assertEqual(machine["transitions"], [{"from": "a", "to": "b"}])

    def test_explicit_empty_depends_on_still_enters(self) -> None:
        dag = {"nodes": [node("a", depends_on=[])]}
        machine, errors = compile_factory_dag(dag)
        self.assertEqual(errors, [])
        self.assertEqual(machine["states"][0]["entry"], True)
        self.assertEqual(machine["transitions"], [])

    def test_invalid_dag_returns_errors_without_a_machine(self) -> None:
        for bad, expected in (
            ("nope", "factory dag must be a JSON object"),
            ({"nodes": "nope"}, "factory dag requires a nodes list"),
            ({"nodes": []}, "factory dag must declare between 1 and 1024 nodes, got 0"),
            ({"nodes": [node("a", depends_on=["ghost"])]}, "node a depends on unknown node 'ghost'"),
        ):
            machine, errors = compile_factory_dag(bad)
            self.assertIsNone(machine, repr(bad))
            self.assertEqual(len(errors), 1, repr(bad))
            self.assertIn(expected, errors[0])

    def test_compiled_dag_and_handwritten_machine_canonicalize_identically(self) -> None:
        dag = {
            "run": {"failure_policy": "continue", "max_parallel": 2},
            "nodes": [
                node("a", outputs=[{"name": "o", "type": "text"}]),
                node("b", depends_on=["a"], budget_ms=1000),
            ],
        }
        machine = {
            "run": {"failure_policy": "continue", "max_parallel": 2},
            "states": [
                {"id": "a", "entry": True, "subagent": "worker", "outputs": [{"name": "o", "type": "text"}]},
                {"id": "b", "subagent": "worker", "max_entries": 1, "budget_ms": 1000},
            ],
            "transitions": [{"from": "a", "to": "b"}],
        }
        self.assertEqual(canonicalize_factory_spec(dag), canonicalize_factory_spec(machine))

        # The join form compiles identically too.
        diamond_dag = {
            "nodes": [
                node("a", outputs=[{"name": "o", "type": "text"}]),
                node("b", depends_on=["a"]),
                node("c", depends_on=["a"]),
                node("d", depends_on=["b", "c"]),
            ]
        }
        diamond_machine = {
            "states": [
                {"id": "a", "entry": True, "subagent": "worker", "outputs": [{"name": "o", "type": "text"}]},
                {"id": "b", "subagent": "worker", "max_entries": 1},
                {"id": "c", "subagent": "worker", "max_entries": 1},
                {"id": "d", "subagent": "worker", "max_entries": 1},
            ],
            "transitions": [
                {"from": "a", "to": "b"},
                {"from": "a", "to": "c"},
                {"from": ["b", "c"], "to": "d"},
            ],
        }
        self.assertEqual(canonicalize_factory_spec(diamond_dag), canonicalize_factory_spec(diamond_machine))


class ValidateFactorySpecFormTest(unittest.TestCase):
    """The unified entry point detects the form first."""

    def test_both_forms_are_rejected_together(self) -> None:
        both = {"nodes": [node("a")], "states": [state("a", entry=True)]}
        self.assertEqual(validate_factory_spec(both), ["pass either dag or machine form, not both"])
        with self.assertRaisesRegex(ValueError, "not both"):
            canonicalize_factory_spec(both)

    def test_machine_wins_when_states_or_transitions_present(self) -> None:
        transitions_only = {"transitions": [{"from": "a", "to": "b"}]}
        self.assertEqual(
            validate_factory_spec(transitions_only),
            ["factory machine requires a states list"],
        )

    def test_dag_wording_without_machine_keys(self) -> None:
        self.assertEqual(
            validate_factory_spec({"nodes": "nope"}),
            ["factory dag requires a nodes list"],
        )

    def test_validation_reports_errors_and_never_raises(self) -> None:
        # The write-time dry run touches arbitrary caller JSON: malformed
        # shapes (unhashable entries, wrong types everywhere) must surface
        # as error lists, never as raw exceptions (regression: the join
        # set() dedupe used to raise TypeError on unhashable from entries).
        hostile = [
            "str",
            12,
            None,
            {"nodes": "nope"},
            {"nodes": [None, 5, "str"]},
            {"nodes": [{"id": {}, "subagent": "w"}]},
            {"nodes": [{"id": ["x"], "subagent": "w"}]},
            {"nodes": [{"id": "a", "subagent": {"prompt": 1}}]},
            {"nodes": [{"id": "a", "subagent": "w", "depends_on": "a"}]},
            {"nodes": [{"id": "a", "subagent": "w", "depends_on": [{}]}]},
            {"nodes": [{"id": "a", "subagent": "w", "inputs": "nope"}]},
            {"nodes": [{"id": "a", "subagent": "w", "inputs": [{"from": 5}]}]},
            {"nodes": [{"id": "a", "subagent": "w", "outputs": [{"name": {}, "type": "text"}]}]},
            {"nodes": [{"id": "a", "subagent": "w", "foreach": {"over": 5, "max": "x"}}]},
            {"states": [{"id": "a", "entry": True, "subagent": "w"}], "transitions": [{"from": {}, "to": "a"}]},
            {"states": [{"id": "a", "entry": True, "subagent": "w"}], "transitions": [{"from": [{}], "to": "a"}]},
            {"states": [{"id": "a", "entry": True, "subagent": "w"}], "transitions": [{"from": ["a", {}], "to": "a"}]},
            {"states": [{"id": "a", "entry": True, "subagent": "w"}], "transitions": [{"from": ["a"], "to": {}}]},
            {"states": [{"id": "a", "entry": True, "subagent": "w"}], "transitions": [{"from": "a", "to": "a", "when": {"op": "eq", "value": [1]}}]},
            {"states": [{"id": "a", "entry": True, "subagent": "w", "max_entries": {}}]},
            {"states": [{"id": "a", "entry": True, "subagent": "w"}], "transitions": {"from": "a"}},
            {"nodes": [{"id": "a", "subagent": "w"}], "states": [{"id": "b", "entry": True, "subagent": "w"}]},
        ]
        for index, spec in enumerate(hostile):
            result = validate_factory_spec(spec)
            self.assertIsInstance(result, list, f"spec #{index}")
            self.assertTrue(all(isinstance(error, str) and error for error in result), f"spec #{index}")
            self.assertTrue(result, f"hostile spec #{index} must report errors")


    def test_non_object_specs_reject_with_dag_wording(self) -> None:
        for bad in (None, [], "nodes", 42):
            self.assertEqual(validate_factory_spec(bad), ["factory dag must be a JSON object"], repr(bad))


class TopologicalOrderTest(unittest.TestCase):
    def test_happy_path_respects_effective_dependencies(self) -> None:
        nodes = [
            node("z", inputs=[{"name": "i", "type": "text", "from": "m.o"}]),
            node("a"),
            node("m", outputs=[{"name": "o", "type": "text"}], depends_on=["a"]),
        ]
        self.assertEqual(topological_order(nodes), ["a", "m", "z"])

    def test_chain(self) -> None:
        nodes = [
            node("c", depends_on=["b"]),
            node("b", depends_on=["a"]),
            node("a"),
        ]
        self.assertEqual(topological_order(nodes), ["a", "b", "c"])

    def test_cycle_raises(self) -> None:
        nodes = [node("a", depends_on=["b"]), node("b", depends_on=["a"])]
        with self.assertRaises(ValueError) as ctx:
            topological_order(nodes)
        self.assertIn("contains a cycle", str(ctx.exception))

        self_cycle = [node("a", depends_on=["a"])]
        with self.assertRaises(ValueError) as ctx:
            topological_order(self_cycle)
        self.assertIn("contains a cycle", str(ctx.exception))

    def test_missing_dependency_raises(self) -> None:
        with self.assertRaises(ValueError) as ctx:
            topological_order([node("a", depends_on=["ghost"])])
        self.assertIn("depends on unknown node 'ghost'", str(ctx.exception))

    def test_duplicate_id_raises(self) -> None:
        with self.assertRaises(ValueError) as ctx:
            topological_order([node("a"), node("a")])
        self.assertIn("duplicate node id 'a'", str(ctx.exception))

    def test_malformed_nodes_raise(self) -> None:
        with self.assertRaises(ValueError):
            topological_order(["not an object"])  # type: ignore[list-item]
        with self.assertRaises(ValueError):
            topological_order([{"subagent": "w"}])
        with self.assertRaises(ValueError):
            topological_order([node("a", depends_on="b")])  # type: ignore[arg-type]
        with self.assertRaises(ValueError):
            topological_order([node("a", inputs="b")])  # type: ignore[arg-type]
        with self.assertRaises(ValueError):
            topological_order([node("a", inputs=["not an object"])])  # type: ignore[list-item]
        with self.assertRaises(ValueError):
            topological_order([node("a", inputs=[{"name": "i", "type": "text", "from": "nodot"}])])

    def test_stable_order_uses_input_position(self) -> None:
        nodes = [node("b"), node("c"), node("a"), node("d")]
        self.assertEqual(topological_order(nodes), ["b", "c", "a", "d"])


class NonQuadraticValidationTest(unittest.TestCase):
    """Review finding: write-time validation must stay linear in the port
    count. A rebuild-and-count duplicate check is quadratic, so a state with
    tens of thousands of ports would block the write."""

    def test_many_outputs_validate_in_linear_time(self) -> None:
        outputs = [{"name": f"o{i}", "type": "text"} for i in range(8000)]
        outputs.append({"name": "o1", "type": "json"})  # a duplicate at the end
        dag = {"nodes": [node("a", outputs=outputs)]}
        started = time.monotonic()
        errors = validate_factory_spec(dag)
        elapsed = time.monotonic() - started
        self.assertEqual(errors, ["node a declares duplicate output name 'o1'"])
        self.assertLess(elapsed, 2.0)

    def test_many_inputs_from_one_source_validate_in_linear_time(self) -> None:
        source = node("src", outputs=[{"name": f"o{i}", "type": "text"} for i in range(3000)])
        inputs = [{"name": f"i{i}", "type": "text", "from": f"src.o{i}"} for i in range(3000)]
        inputs.append({"name": "i1", "type": "text", "from": "src.o1"})  # duplicate input name
        dag = {"nodes": [source, node("dst", inputs=inputs)]}
        started = time.monotonic()
        errors = validate_factory_spec(dag)
        elapsed = time.monotonic() - started
        self.assertEqual(errors, ["node dst declares duplicate input name 'i1'"])
        self.assertLess(elapsed, 2.0)

    def test_many_duplicate_outputs_report_once(self) -> None:
        # The duplicate report is deduplicated too: every repeat of one
        # name produces exactly one error sentence.
        outputs = [{"name": "dup", "type": "text"} for _ in range(500)]
        dag = {"nodes": [node("a", outputs=outputs)]}
        self.assertEqual(validate_factory_spec(dag), ["node a declares duplicate output name 'dup'"])


class FactoryHelpTest(unittest.TestCase):
    """rlm.factory.help(): the embedded authoring reference (PR #3199).

    The full agent-facing reference — authoring rules, guards/joins/cycles,
    foreach, budgets, stall detectors, and the API with worked examples —
    is a module-level constant in rlm/factory.py; ``help()`` returns it
    with no filesystem resolution, so packaged kernels (where the repo
    layout is not adjacent) see the same guide.
    """

    def test_factory_help_returns_the_full_reference(self) -> None:
        doc = rlm_module.rlm.factory.help()
        self.assertIsInstance(doc, str)
        # help() returns the embedded constant, never a filesystem read.
        self.assertEqual(doc, factory_module.FACTORY_HELP)

        # The shipped section structure: store, author, dag sugar, run, safety.
        for heading in (
            "# Factory",
            "## Store the spec",
            "## Authoring reference",
            "## Dag form",
            "## Run and steer",
            "## Safety",
        ):
            self.assertIn(heading, doc)

        # Prose sections wrap at ~76 columns; flatten before matching phrases.
        flat = " ".join(doc.split())
        # The opt-in contract in the opening: disabled by default, the
        # /factory on|off|status pointer, the exact refusal, and help()
        # readable while disabled.
        self.assertIn("The factory is opt-in: it ships disabled", flat)
        self.assertIn("`/factory on` (`/factory off` disables it again, `/factory status` reports it;", flat)
        self.assertIn("the persisted setting is `factory.enabled` in the agent dir's settings.json", flat)
        self.assertIn("every `rlm.factory` call except `help()`", flat)
        self.assertIn("every factory harness write (`create_factory` and updates of factory entries)", flat)
        self.assertIn(factory_module.FACTORY_DISABLED_MESSAGE, flat)
        self.assertIn("`help()` answers while disabled", flat)
        # Guards: the op set evaluated over the from-state's latest settle.
        self.assertIn("`eq`, `ne`, `gt`, `gte`, `lt`, `lte`, `exists`, `contains`", flat)
        self.assertIn("over the from-state's latest settle", flat)
        # foreach: one child per item of the named json input, clamped at max.
        self.assertIn('**foreach**: `{"over": "<input>", "max": 1..256}`', flat)
        self.assertIn("expands one entry into one child per item of the named `json` input", flat)
        # max_parallel is the run's global budget, not a per-node limit.
        self.assertIn(
            "`run.max_parallel` (1..64, default 8) is the run's global budget of "
            "simultaneously running instances",
            flat,
        )
        self.assertIn("not a per-node limit", flat)
        # Stall detectors: dead configurations fail loudly, never wedge.
        self.assertIn("Dead configurations fail loudly, never wedge", flat)
        self.assertIn("nothing in flight and nothing pending", flat)
        # The stop/resume contract.
        self.assertIn("`stop(run_id)` cancels every running child of the run (idempotent)", flat)
        self.assertIn("`resume(run_id)` continues a paused run and raises on a non-paused one", flat)

        # The run/status snippet, as the agent types it.
        self.assertIn('result = await rlm.factory.run("pr-manager")', doc)
        self.assertIn('status = await rlm.factory.status(result["run_id"])', doc)

        # The worked examples bound their emitted payloads — captured answers
        # are capped previews (~160-200 chars), so an unbounded json payload
        # would truncate at the cap and fail to bind.
        self.assertIn('findings": ["at most three one-line findings"]', doc)
        self.assertIn("capped at the eight most relevant", doc)
        self.assertIn("an unbounded payload truncates at the cap and fails to bind", flat)

        # The Discovery section ships the machine library as present (this
        # branch merges it): the bundled location, the seed names, the CLI
        # surface, and the run-from-library fallback — every phrase
        # fact-checked against the machine-library code (parse/import
        # gate/run fallback).
        self.assertIn(
            "The machine library: machines are `MACHINE.md` files (frontmatter plus "
            "one fenced `machine-spec` block), one directory per machine, resolved "
            "from two levels — the bundled seeds shipped inside the runtime "
            "(visible in every install; `PRIME_AGENT_MACHINES_DIR` redirects the "
            "level at a team directory) first, the personal `machines/` library "
            "under the agent dir second",
            flat,
        )
        self.assertIn(
            "`prime-agent factory list | import | export` manages them: list shows "
            "only what parses and validates (broken files print as warnings), "
            "import runs the same write-time validation as a stored spec so an "
            "invalid machine never persists, and export copies a library machine "
            "verbatim to a fresh path (an existing target is refused, never "
            "overwritten)",
            flat,
        )
        self.assertIn(
            "`rlm.factory.run('<name>')` runs a library machine directly without "
            "creating a harness entry; a machine that exists but is broken names "
            "its errors instead of pretending the name is unknown",
            flat,
        )
        self.assertIn(
            "The bundled seeds are `builder`, `pr-manager`, and `review-sweep`", flat
        )

        # The configured inline subagent name contract.
        self.assertIn("The optional `name` labels the spawned children run-scoped", flat)
        self.assertIn("the spawn label is `<run6>-name`", flat)
        self.assertIn("unique across the machine's states", flat)
        self.assertIn("a name another state's name can suffix onto, `foo` vs `foo-i1`, is rejected at write time", flat)

    def test_help_advertises_only_calls_the_namespace_has(self) -> None:
        # The guide and the namespace MUST agree exactly: every dotted
        # `rlm.factory.<call>(...)` example the guide teaches must exist as
        # an attribute on the namespace, or an agent following the returned
        # reference hits an AttributeError (Macroscope review finding: the
        # guide advertised watch()/graph() that the namespace did not carry;
        # they arrive with the stacked live-view PR).
        doc = rlm_module.rlm.factory.help()
        advertised = sorted(set(re.findall(r"rlm\.factory\.(\w+)\(", doc)))
        namespace = rlm_module.rlm.factory
        missing = [name for name in advertised if not hasattr(namespace, name)]
        self.assertEqual(missing, [])
        # The core calls stay advertised (dotted examples) and the whole
        # namespace surface stays implemented. This branch IS the stacked
        # live-view PR: it ships the graph()/watch() implementations, so
        # the guide teaches them as call examples and the namespace
        # carries them (the same invariant the core pins on its own tree,
        # which trims them because its namespace stops at resume()).
        for name in ("run", "status", "stop", "graph", "watch"):
            self.assertIn(name, advertised)
        for name in ("run", "status", "stop", "resume", "help", "graph", "watch"):
            self.assertTrue(hasattr(namespace, name), name)


# ---------------------------------------------------------------------------
# The opt-in settings seam: `factory.enabled` in the agent-dir settings file
# ---------------------------------------------------------------------------


class FactorySettingReadTest(unittest.TestCase):
    """The settings seam behind the opt-in gate.

    The gate resolves the agent dir the way the rest of the runtime does and
    reads the same nested-camelCase settings document the daemon and TUI
    settings surface write. The read is lenient like the Rust loader
    (wrong-typed values read as unset) and fail-closed (a missing or corrupt
    document leaves the factory disabled, never a crash).
    """

    def setUp(self) -> None:
        temp = TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.settings_path = Path(temp.name) / "settings.json"
        previous = os.environ.get("PRIME_AGENT_CODING_AGENT_DIR")
        os.environ["PRIME_AGENT_CODING_AGENT_DIR"] = str(Path(temp.name))

        def restore() -> None:
            if previous is None:
                os.environ.pop("PRIME_AGENT_CODING_AGENT_DIR", None)
            else:
                os.environ["PRIME_AGENT_CODING_AGENT_DIR"] = previous

        self.addCleanup(restore)

    def write_settings(self, document: Any) -> None:
        self.settings_path.write_text(json.dumps(document), encoding="utf-8")

    def test_round_trips_the_real_settings_document_shape(self) -> None:
        # A settings file as the daemon writes it: flat camelCase keys and
        # nested feature objects (agentTraces/telemetry/terminal); the
        # factory key is shaped exactly like the other feature toggles.
        # /factory on writes enabled true over the same document...
        document = {
            "defaultProvider": "prime-inference",
            "defaultModel": "internal/glm-5.3-fast",
            "rlmMaxDepth": 2,
            "theme": "dark",
            "agentTraces": {"enabled": False},
            "telemetry": {"enabled": None, "noticeShown": True},
            "terminal": {"showImages": True},
            "factory": {"enabled": True},
        }
        self.write_settings(document)
        self.assertTrue(factory_module.factory_enabled())
        # ...and /factory off writes enabled false, leaving the rest intact.
        document["factory"] = {"enabled": False}
        self.write_settings(document)
        self.assertFalse(factory_module.factory_enabled())

    def test_missing_or_wrong_typed_settings_read_as_disabled(self) -> None:
        # No settings file at all: the shipped default is off.
        self.assertFalse(factory_module.factory_enabled())
        # A settings document without the factory key is the same default.
        self.write_settings({})
        self.assertFalse(factory_module.factory_enabled())
        # Wrong-typed values read as unset, like the lenient Rust loader.
        for document in (
            {"factory": None},
            {"factory": "enabled"},
            {"factory": {"enabled": None}},
            {"factory": {"enabled": "true"}},
            {"factory": {"enabled": 1}},
        ):
            self.write_settings(document)
            self.assertFalse(factory_module.factory_enabled(), str(document))
        # An enabled object with unknown sibling keys still reads enabled.
        self.write_settings({"factory": {"enabled": True, "extra": "ignored"}})
        self.assertTrue(factory_module.factory_enabled())
        # A corrupt document fails closed: a clean refusal, never a crash.
        self.settings_path.write_text("{ not json", encoding="utf-8")
        self.assertFalse(factory_module.factory_enabled())


# ---------------------------------------------------------------------------
# The executor client (the executor itself runs in the host: its battery is
# the Rust port in crates/pa-core/src/factory/executor/tests/)
# ---------------------------------------------------------------------------


def async_test(coroutine):
    """Run one async test method on a fresh event loop."""

    def wrapper(self):
        return asyncio.run(coroutine(self))

    wrapper.__name__ = coroutine.__name__
    return wrapper


class ClientHost:
    """Records the client's executor host requests and answers them with
    canned host replies: `factory.run` echoes the run identity a host
    executor reports; every other request answers `{"run_id", "state"}`.
    ``refuse`` makes one request type answer the host's refusal envelope
    (the kernel raises it as ValueError)."""

    def __init__(self) -> None:
        self.calls: list[tuple[str, dict[str, Any]]] = []
        self.refuse: dict[str, str] = {}

    def calls_of(self, request_type: str) -> list[dict[str, Any]]:
        return [payload for kind, payload in self.calls if kind == request_type]

    async def __call__(self, request_type: str, payload: dict[str, Any] | None = None) -> dict[str, Any]:
        payload = payload or {}
        self.calls.append((request_type, payload))
        if request_type in self.refuse:
            return {"error": self.refuse[request_type]}
        if request_type == "factory.run":
            result: dict[str, Any] = {"run_id": f"run-{len(self.calls)}", "spec_id": payload["spec_id"]}
            if "machine" in payload:
                result["machine"] = payload["machine"]
            if "machine_path" in payload:
                result["machine_path"] = payload["machine_path"]
            return {"result": result}
        if request_type.startswith("factory."):
            return {"result": {"run_id": payload.get("run_id"), "state": "running"}}
        raise AssertionError(f"unexpected host request type {request_type!r}")


def run_value(payload: dict[str, Any]) -> dict[str, Any]:
    """The `factory.run` payload's value table, decoded: `{"spec", "subagents"}`."""
    return factory_module._decode_value(payload["value"], [])


class _ClientTestCase(unittest.TestCase):
    """A temp-dir harness with a real subagent entry, the recording host
    behind the ``host_request`` patch seam, and the per-test default
    executor client. The opt-in gate reads the agent dir's settings file,
    so the agent dir points at a temp dir whose settings file the tests
    write in the real document shape the daemon writes."""

    def setUp(self) -> None:
        # The agent dir first: the harness load below already names it.
        agent_temp = TemporaryDirectory()
        self.addCleanup(agent_temp.cleanup)
        self.settings_path = Path(agent_temp.name) / "settings.json"
        self.write_settings({"factory": {"enabled": True}})
        self._isolate_agent_dir(agent_temp.name)
        temp = TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.harness = HarnessState(Path(temp.name) / "harness_state.json")
        self.harness.create_subagent("Worker", "Do the work carefully.", id="worker")
        self.host = ClientHost()
        self.executor = FactoryExecutor(harness=self.harness)
        previous_executor = factory_module._DEFAULT_EXECUTOR
        factory_module._DEFAULT_EXECUTOR = self.executor
        self.addCleanup(lambda: setattr(factory_module, "_DEFAULT_EXECUTOR", previous_executor))
        patcher = patch.object(rlm_module, "host_request", self.host)
        patcher.start()
        self.addCleanup(patcher.stop)

    def write_settings(self, document: Any) -> None:
        """Write the agent-dir settings document (the real file shape)."""
        self.settings_path.write_text(json.dumps(document), encoding="utf-8")

    def _isolate_agent_dir(self, agent_dir: str) -> None:
        previous = os.environ.get("PRIME_AGENT_CODING_AGENT_DIR")
        os.environ["PRIME_AGENT_CODING_AGENT_DIR"] = agent_dir

        def restore() -> None:
            if previous is None:
                os.environ.pop("PRIME_AGENT_CODING_AGENT_DIR", None)
            else:
                os.environ["PRIME_AGENT_CODING_AGENT_DIR"] = previous

        self.addCleanup(restore)

    def disable_factory(self) -> None:
        """Flip the settings file to the disabled default."""
        self.write_settings({"factory": {"enabled": False}})

    def store_factory(self, dag: dict[str, Any], spec_id: str = "sw") -> None:
        self.harness.create_factory("Factory", "Factory content", id=spec_id, dag=dag)

    def store_machine(self, machine: dict[str, Any], spec_id: str = "sw") -> None:
        self.harness.create_factory("Factory", "Factory content", id=spec_id, machine=machine)

    def node(self, node_id: str, **overrides: Any) -> dict[str, Any]:
        base: dict[str, Any] = {"id": node_id, "subagent": "worker"}
        base.update(overrides)
        return base

    async def start(self, spec_id: str = "sw") -> dict[str, Any]:
        return await rlm_module.rlm.factory.run(spec_id)


class FactoryOptInGateTest(_ClientTestCase):
    """The opt-in gate: `factory.enabled` (default off) closes the namespace.

    The factory ships disabled; the user turns it on with /factory on (the
    persisted `factory.enabled` setting in the agent dir's settings.json).
    While it is off, every rlm.factory call except help() and every factory
    harness write refuses with ONE exact message, and nothing reaches the
    host behind the refusal. (The host lane's own gate is pinned by the Rust
    port: `the_activity_lane_refuses_while_disabled`.)
    """

    # -- namespace gate --------------------------------------------------------

    @async_test
    async def test_run_proceeds_when_the_enabled_setting_is_written(self) -> None:
        self.store_factory({"nodes": [self.node("a")]}, spec_id="sw")
        result = await self.start()
        self.assertIn("run_id", result)
        self.assertEqual([payload["spec_id"] for payload in self.host.calls_of("factory.run")], ["sw"])

    @async_test
    async def test_empty_settings_refuse_run_with_the_exact_message(self) -> None:
        self.store_factory({"nodes": [self.node("a")]}, spec_id="sw")
        # An empty settings document: the factory key absent reads as the
        # disabled default, and the refusal precedes any spec resolution.
        self.write_settings({})
        with self.assertRaises(ValueError) as raised:
            await self.start()
        self.assertEqual(str(raised.exception), "the factory is disabled; run /factory on to enable it")
        self.assertEqual(str(raised.exception), factory_module.FACTORY_DISABLED_MESSAGE)
        self.assertEqual(self.host.calls, [])
        # No settings file at all is the same disabled default.
        self.settings_path.unlink()
        with self.assertRaises(ValueError) as raised:
            await self.start()
        self.assertEqual(str(raised.exception), factory_module.FACTORY_DISABLED_MESSAGE)
        self.assertEqual(self.host.calls, [])

    @async_test
    async def test_status_stop_and_resume_refuse_with_the_exact_message(self) -> None:
        self.store_factory({"nodes": [self.node("a")]}, spec_id="sw")
        result = await self.start()
        run_id = result["run_id"]
        self.disable_factory()
        sent = len(self.host.calls)
        for call in (
            rlm_module.rlm.factory.status(run_id),
            rlm_module.rlm.factory.stop(run_id),
            rlm_module.rlm.factory.resume(run_id),
        ):
            with self.assertRaises(ValueError) as raised:
                await call
            self.assertEqual(str(raised.exception), factory_module.FACTORY_DISABLED_MESSAGE)
        self.assertEqual(len(self.host.calls), sent)

    @async_test
    async def test_graph_and_watch_refuse_with_the_exact_message(self) -> None:
        self.store_factory({"nodes": [self.node("a")]}, spec_id="sw")
        result = await self.start()
        run_id = result["run_id"]
        self.disable_factory()
        for call in (
            rlm_module.rlm.factory.graph(run_id),
            rlm_module.rlm.factory.graph(),
            rlm_module.rlm.factory.graph("sw"),
            rlm_module.rlm.factory.watch(run_id, 0.5),
        ):
            with self.assertRaises(ValueError) as raised:
                await call
            self.assertEqual(str(raised.exception), factory_module.FACTORY_DISABLED_MESSAGE)

    def test_help_answers_while_disabled(self) -> None:
        self.disable_factory()
        doc = rlm_module.rlm.factory.help()
        self.assertEqual(doc, factory_module.FACTORY_HELP)
        self.assertIn("The factory is opt-in", doc)

    # -- harness write gate ----------------------------------------------------

    def test_create_factory_refuses_while_disabled(self) -> None:
        self.disable_factory()
        with self.assertRaises(ValueError) as raised:
            self.harness.create_factory("Factory", "Never stores.", id="sw", dag={"nodes": [self.node("a")]})
        self.assertEqual(str(raised.exception), factory_module.FACTORY_DISABLED_MESSAGE)
        self.assertEqual(self.harness.list("factory"), [])
        # The refusal precedes spec validation: an invalid spec never gets
        # the spec error while the factory is off, only the one message.
        with self.assertRaises(ValueError) as raised:
            self.harness.create_factory("Broken", "Never stores.", id="bad", dag={"nodes": []})
        self.assertEqual(str(raised.exception), factory_module.FACTORY_DISABLED_MESSAGE)
        self.assertEqual(self.harness.list("factory"), [])
        # The generic create path refuses identically.
        with self.assertRaises(ValueError) as raised:
            self.harness.create(
                "factory",
                "Generic",
                "content",
                id="generic",
                arguments={"dag": {"nodes": [self.node("a")]}},
            )
        self.assertEqual(str(raised.exception), factory_module.FACTORY_DISABLED_MESSAGE)
        self.assertEqual(self.harness.list("factory"), [])

    def test_factory_entry_updates_refuse_while_disabled(self) -> None:
        self.store_factory({"nodes": [self.node("a")]}, spec_id="sw")
        self.disable_factory()
        with self.assertRaises(ValueError) as raised:
            self.harness.update_factory("sw", "Factory", "content updated")
        self.assertEqual(str(raised.exception), factory_module.FACTORY_DISABLED_MESSAGE)
        with self.assertRaises(ValueError) as raised:
            self.harness.update("factory", "sw", "Factory", "content updated")
        self.assertEqual(str(raised.exception), factory_module.FACTORY_DISABLED_MESSAGE)
        with self.assertRaises(ValueError) as raised:
            self.harness.upsert(
                "factory",
                "Factory",
                "content",
                id="upserted",
                arguments={"dag": {"nodes": [self.node("a")]}},
            )
        self.assertEqual(str(raised.exception), factory_module.FACTORY_DISABLED_MESSAGE)
        # The stored entry is untouched behind the refusals.
        self.assertEqual(self.harness.get("factory", "sw").content, "Factory content")
        self.assertEqual(self.harness.list("factory"), [self.harness.get("factory", "sw")])

    def test_delete_stays_available_while_disabled(self) -> None:
        # Deleting is cleanup, not authoring or execution: the gate list is
        # run/status/stop/resume (and the later graph/watch) plus creates and
        # updates, so an operator can still clear stale entries while off.
        self.store_factory({"nodes": [self.node("a")]}, spec_id="sw")
        self.disable_factory()
        self.assertTrue(self.harness.delete_factory("sw"))
        self.assertIsNone(self.harness.get("factory", "sw"))


class FactoryExecutorClientTest(_ClientTestCase):
    """The kernel half of a run: what the client resolves and ships, and
    how host replies surface. (The executor's behavior — admission,
    binding, transitions, policies, budgets, stop races, graph and watch —
    is the Rust port of the old executor battery.)"""

    @async_test
    async def test_run_rejects_unknown_spec(self) -> None:
        with self.assertRaisesRegex(ValueError, "unknown factory spec"):
            await self.start("missing-spec")
        self.assertEqual(self.host.calls, [])

    @async_test
    async def test_run_ships_the_entry_spec_and_its_resolved_subagents(self) -> None:
        self.harness.create_subagent(
            "The Worker",
            "Template by title.",
            id="worker-md",
            metadata={"model": "pi/test-model", "thinking": "low"},
        )
        dag = {
            "run": {"max_parallel": 2},
            "nodes": [
                {"id": "x", "subagent": "The Worker"},
                {"id": "y", "subagent": "worker-md"},
                {"id": "z", "subagent": "ghost"},
                {"id": "w", "subagent": {"prompt": "Inline."}},
            ],
        }
        self.store_factory(dag)
        await rlm_module.rlm.factory.run("sw", name="the run")
        (payload,) = self.host.calls_of("factory.run")
        self.assertEqual(payload["spec_id"], "sw")
        self.assertEqual(payload["name"], "the run")
        value = run_value(payload)
        self.assertEqual(value["spec"], dag)
        template = {"content": "Template by title.", "model": "pi/test-model", "thinking": "low"}
        # By title, by id, and an unknown reference the host reports.
        self.assertEqual(value["subagents"], {"The Worker": template, "worker-md": template, "ghost": None})

    @async_test
    async def test_host_refusals_raise_value_errors(self) -> None:
        self.host.refuse["factory.status"] = "unknown factory run 'no-such-run'"
        with self.assertRaises(ValueError) as raised:
            await rlm_module.rlm.factory.status("no-such-run")
        self.assertEqual(str(raised.exception), "unknown factory run 'no-such-run'")
        self.host.refuse["factory.resume"] = "factory run 'r' is 'done', not paused"
        with self.assertRaisesRegex(ValueError, "not paused"):
            await rlm_module.rlm.factory.resume("r")

    @async_test
    async def test_non_string_run_ids_are_unknown_runs(self) -> None:
        for call in ("status", "stop", "resume"):
            with self.assertRaisesRegex(ValueError, "unknown factory run None"):
                await getattr(rlm_module.rlm.factory, call)(None)
        with self.assertRaisesRegex(ValueError, "unknown factory run 5"):
            await rlm_module.rlm.factory.watch(5, 1.0)
        self.assertEqual(self.host.calls, [])

    @async_test
    async def test_watch_ships_a_bounded_timeout_or_none(self) -> None:
        for timeout, shipped in ((0.5, 0.5), (3, 3.0), (1e9, 60.0), (float("inf"), 60.0), (-1, -1.0)):
            await rlm_module.rlm.factory.watch("r", timeout)
            self.assertEqual(self.host.calls_of("factory.watch")[-1]["timeout"], shipped, timeout)
        # Non-numbers (a bool included) and NaN ship as None: the host
        # refuses them after the run lookup, the original order.
        for timeout in ("soon", True, float("nan")):
            await rlm_module.rlm.factory.watch("r", timeout)
            self.assertIsNone(self.host.calls_of("factory.watch")[-1]["timeout"], timeout)

if __name__ == "__main__":
    unittest.main()


# ---------------------------------------------------------------------------
# Machine library: MACHINE.md parse, render, gate, resolution, run.
#
# New-feature coverage (the machine library): the file format round-trips
# (export -> import -> identical validated spec), the import gate refuses
# invalid specs with the write-time validator's exact errors and never
# persists, library resolution is repo-first/user-second, and
# rlm.factory.run falls back to the library for machine names.
# ---------------------------------------------------------------------------


def machine_file_text(
    *,
    name: str = "sweep",
    description: str = "A machine that sweeps.",
    version: str = "1",
    author: str = "Tester",
    spec_json: str | None = None,
    frontmatter: str | None = None,
    body: str | None = None,
) -> str:
    """Build MACHINE.md text; ``frontmatter``/``body`` override the defaults."""
    if frontmatter is None:
        frontmatter = (
            "---\n"
            f"name: {name}\n"
            f"description: {description}\n"
            f"version: {version}\n"
            f"author: {author}\n"
            "---"
        )
    if body is None:
        spec_json = spec_json or json.dumps({"run": {"failure_policy": "continue"}, "states": [
            {"id": "a", "entry": True, "subagent": {"prompt": "Do the work."}}
        ]})
        body = f"# {name}\n\n```machine-spec\n{spec_json}\n```"
    return f"{frontmatter}\n\n{body}"


class MachineFileParseTest(unittest.TestCase):
    """The strict MACHINE.md format: frontmatter + one machine-spec fence."""

    def parse(self, text: str) -> "tuple[MachineFile | None, list[str]]":
        return parse_machine_file(text, source="test-MACHINE.md")

    def test_parses_frontmatter_and_spec(self) -> None:
        machine, errors = self.parse(machine_file_text())
        self.assertEqual(errors, [])
        assert machine is not None
        self.assertEqual(machine.name, "sweep")
        self.assertEqual(machine.description, "A machine that sweeps.")
        self.assertEqual(machine.version, "1")
        self.assertEqual(machine.author, "Tester")
        self.assertEqual(machine.spec["states"][0]["id"], "a")

    def test_parses_quoted_frontmatter_values(self) -> None:
        text = machine_file_text(
            frontmatter=(
                "---\n"
                'name: "sweep"\n'
                "description: 'It: reviews things.'\n"
                "version: 1\n"
                "author: Prime Agent\n"
                "---"
            )
        )
        machine, errors = self.parse(text)
        self.assertEqual(errors, [])
        assert machine is not None
        self.assertEqual(machine.name, "sweep")
        self.assertEqual(machine.description, "It: reviews things.")

    def test_missing_frontmatter_is_an_exact_error(self) -> None:
        machine, errors = self.parse("# just prose\n\n```machine-spec\n{}\n```")
        self.assertIsNone(machine)
        self.assertEqual(
            errors, ["test-MACHINE.md: MACHINE.md must start with a `---` frontmatter block"]
        )

    def test_unclosed_frontmatter_is_an_exact_error(self) -> None:
        machine, errors = self.parse("---\nname: sweep\nno close")
        self.assertIsNone(machine)
        self.assertEqual(
            errors, ["test-MACHINE.md: frontmatter is not closed (end it with a `---` line)"]
        )

    def test_unknown_and_duplicate_frontmatter_keys_are_exact_errors(self) -> None:
        machine, errors = self.parse(
            machine_file_text(frontmatter="---\nname: sweep\ndescription: A machine.\n---", body="x")
        )
        # Missing the spec fence is reported too, but the frontmatter still parsed:
        self.assertIsNone(machine)
        machine2, errors2 = self.parse(
            machine_file_text(frontmatter="---\nname: sweep\nname: again\ndescription: A machine.\n---")
        )
        self.assertIsNone(machine2)
        self.assertTrue(any("declared more than once" in error for error in errors2), errors2)
        machine3, errors3 = self.parse(
            machine_file_text(frontmatter="---\nname: sweep\ndescription: A machine.\nlicense: MIT\n---")
        )
        self.assertIsNone(machine3)
        self.assertTrue(any("unknown frontmatter key 'license'" in error for error in errors3), errors3)

    def test_missing_name_and_description_are_exact_errors(self) -> None:
        machine, errors = self.parse(
            machine_file_text(frontmatter="---\nversion: 1\nauthor: Tester\n---")
        )
        self.assertIsNone(machine)
        self.assertTrue(any("machine name must be a non-empty string" in error for error in errors), errors)
        self.assertTrue(any("frontmatter description is required" in error for error in errors), errors)

    def test_multiline_descriptions_are_rejected(self) -> None:
        # The description is one listing row by contract, so a quoted
        # frontmatter value that decodes an embedded line break is a format
        # error with its own sentence — not a machine whose listing renders
        # across several terminal lines.
        for description in ('"A machine that\\nsweeps."', '"A machine that\\rsweeps."'):
            machine, errors = self.parse(machine_file_text(frontmatter=(
                "---\nname: sweep\n"
                f"description: {description}\n"
                "version: 1\nauthor: Tester\n---"
            )))
            self.assertIsNone(machine, description)
            self.assertEqual(
                errors, ["frontmatter description must be a single line"]
            )
        self.assertEqual(
            machine_description_errors("One line, as the format requires."), []
        )

    def test_name_rules_mirror_the_skill_library(self) -> None:
        for bad in ("Sweep", "sweep x", "-sweep", "sweep-", "a" * 65):
            machine, errors = self.parse(machine_file_text(name=bad))
            self.assertIsNone(machine, bad)
            self.assertTrue(errors, bad)
        self.assertEqual(machine_name_errors("sweep-2"), [])
        self.assertEqual(machine_name_errors(""), ["machine name must be a non-empty string"])
        self.assertEqual(machine_name_errors(7), ["machine name must be a non-empty string"])
        self.assertIn("must not end with a hyphen", machine_name_errors("sweep-")[0])

    def test_plain_value_with_colon_demands_quotes(self) -> None:
        machine, errors = self.parse(
            machine_file_text(
                frontmatter="---\nname: sweep\ndescription: Reviews: everything\n---",
                body="# sweep\n\n```machine-spec\n{\"run\": {}}\n```",
            )
        )
        self.assertIsNone(machine)
        self.assertTrue(any("quote the value" in error for error in errors), errors)

    def test_no_fence_is_an_exact_error(self) -> None:
        machine, errors = self.parse(machine_file_text(body="# sweep\n\nNo spec here."))
        self.assertIsNone(machine)
        self.assertEqual(
            errors,
            ["test-MACHINE.md: MACHINE.md requires exactly one fenced ```machine-spec block; found none"],
        )

    def test_multiple_fences_are_an_exact_error(self) -> None:
        spec_json = '{"run": {"failure_policy": "continue"}, "states": [{"id": "a", "entry": true, "subagent": {"prompt": "P."}}]}'
        text = machine_file_text(body=f"# sweep\n\n```machine-spec\n{spec_json}\n```\n\n```machine-spec\n{spec_json}\n```")
        machine, errors = self.parse(text)
        self.assertIsNone(machine)
        self.assertEqual(
            errors,
            ["test-MACHINE.md: MACHINE.md requires exactly one fenced ```machine-spec block; found 2"],
        )

    def test_unterminated_fence_is_an_exact_error(self) -> None:
        text = machine_file_text(body="# sweep\n\n```machine-spec\n{\"run\": {}}")
        machine, errors = self.parse(text)
        self.assertIsNone(machine)
        self.assertEqual(errors, ["test-MACHINE.md: the ```machine-spec fence is never closed"])

    def test_fence_payload_must_be_a_json_object(self) -> None:
        machine, errors = self.parse(machine_file_text(body="# sweep\n\n```machine-spec\n[1, 2]\n```"))
        self.assertIsNone(machine)
        self.assertIn(
            "must contain a JSON object, got a list", errors[0]
        )
        machine2, errors2 = self.parse(machine_file_text(body="# sweep\n\n```machine-spec\nnot json\n```"))
        self.assertIsNone(machine2)
        self.assertTrue(errors2[0].startswith("test-MACHINE.md: the ```machine-spec block must contain a JSON object"), errors2)

    def test_other_fenced_blocks_do_not_confuse_the_scan(self) -> None:
        spec_json = '{"run": {"failure_policy": "continue"}, "states": [{"id": "a", "entry": true, "subagent": {"prompt": "P."}}]}'
        text = machine_file_text(
            body=(
                "# sweep\n\n"
                "Example prose with a json block:\n\n"
                "```json\n{\"not\": \"a machine\"}\n```\n\n"
                "```text\nplain text\n```\n\n"
                f"```machine-spec\n{spec_json}\n```"
            )
        )
        machine, errors = self.parse(text)
        self.assertEqual(errors, [])
        assert machine is not None
        self.assertEqual(machine.spec["run"]["failure_policy"], "continue")

    def test_crlf_and_bom_are_normalized(self) -> None:
        text = machine_file_text().replace("\n", "\r\n")
        machine, errors = self.parse("\ufeff" + text)
        self.assertEqual(errors, [])
        assert machine is not None
        self.assertEqual(machine.name, "sweep")


class MachineFileRenderTest(unittest.TestCase):
    """render_machine_file is byte-stable and parse-identical."""

    def render(self, spec: dict[str, Any]) -> str:
        machine = MachineFile(
            name="sweep",
            description="A machine that sweeps.",
            version="1",
            author="Tester",
            spec=spec,
        )
        return render_machine_file(machine)

    def test_render_is_byte_stable_and_round_trips(self) -> None:
        text = self.render(valid_machine())
        self.assertEqual(text, self.render(valid_machine()))
        machine, errors = parse_machine_file(text, source="rendered")
        self.assertEqual(errors, [])
        assert machine is not None
        self.assertEqual(machine.name, "sweep")
        self.assertEqual(machine.description, "A machine that sweeps.")
        self.assertEqual(machine.version, "1")
        self.assertEqual(machine.author, "Tester")
        self.assertEqual(machine.spec, valid_machine())

    def test_render_quotes_non_plain_values(self) -> None:
        machine = MachineFile(
            name="sweep",
            description="Reviews: everything, carefully.",
            version="1",
            author="Tester",
            spec={"states": [{"id": "a", "entry": True, "subagent": {"prompt": "P."}}]},
        )
        text = render_machine_file(machine)
        self.assertIn('description: "Reviews: everything, carefully."', text)
        reparsed, errors = parse_machine_file(text, source="rendered")
        self.assertEqual(errors, [])
        assert reparsed is not None
        self.assertEqual(reparsed.description, "Reviews: everything, carefully.")

    def test_render_generates_contract_prose_from_both_forms(self) -> None:
        machine_text = self.render(valid_machine())
        self.assertIn("Run: failure_policy=continue, max_parallel=4", machine_text)
        self.assertIn("States:", machine_text)
        self.assertIn("- collect (entry)", machine_text)
        self.assertIn("  input: draft (text) <- collect.findings", machine_text)
        self.assertIn("Transitions:", machine_text)
        self.assertIn("- reviewing -> fixing when verdict.approved eq false", machine_text)
        dag_text = self.render(valid_dag())
        self.assertIn("- fan-out", dag_text)
        self.assertIn("  input: items (text) <- collect.findings", dag_text)
        self.assertIn("```machine-spec", machine_text)

    def test_render_pretty_json_is_two_space_indented(self) -> None:
        text = self.render({"run": {"failure_policy": "continue"}, "states": [
            {"id": "a", "entry": True, "subagent": {"prompt": "P."}}
        ]})
        fence = text.split("```machine-spec\n", 1)[1].rsplit("```", 1)[0]
        self.assertIn('\n  "run": {\n    "failure_policy": "continue"\n  },', fence)


class MachineFileRoundTripTest(unittest.TestCase):
    """export -> import -> identical validated spec (the format round-trip)."""

    def setUp(self) -> None:
        temp = TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        root = Path(temp.name).resolve()
        self.library = root / "machines"
        self.out_dir = root / "out"
        # The import gate reads the agent dir's settings: never the real one.
        env = patch.dict(os.environ, {"PRIME_AGENT_CODING_AGENT_DIR": str(root / "agent")})
        env.start()
        self.addCleanup(env.stop)

    def test_exported_file_imports_to_the_identical_spec(self) -> None:
        spec = valid_dag()
        out = self.out_dir / "shared.MACHINE.md"
        export_factory_spec(spec, out, name="sweep", description="A machine that sweeps.")
        imported = import_machine(out, target_dir=self.library)
        self.assertEqual(imported["name"], "sweep")
        self.assertTrue(imported["created"])
        stored, errors = parse_machine_file(
            (self.library / "sweep" / "MACHINE.md").read_text(encoding="utf-8"), source="stored"
        )
        self.assertEqual(errors, [])
        assert stored is not None
        self.assertEqual(validate_factory_spec(stored.spec), [])
        self.assertEqual(
            canonicalize_factory_spec(stored.spec), canonicalize_factory_spec(spec)
        )

    def test_library_export_import_export_is_byte_identical(self) -> None:
        spec = valid_machine()
        first = self.out_dir / "first.MACHINE.md"
        export_factory_spec(spec, first, name="sweep", description="A machine that sweeps.")
        import_machine(first, target_dir=self.library)
        # Export the imported library machine (verbatim copy) and re-import: bytes never move.
        second = self.out_dir / "second.MACHINE.md"
        export_machine("sweep", second, repo_dir=None, user_dir=self.library)
        self.assertEqual(first.read_text(encoding="utf-8"), second.read_text(encoding="utf-8"))
        imported = import_machine(second, target_dir=self.library)
        self.assertFalse(imported["created"])


class ImportGateTest(unittest.TestCase):
    """The import gate: invalid specs never persist, with exact errors."""

    def setUp(self) -> None:
        temp = TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.library = Path(temp.name) / "machines"
        self.sources = Path(temp.name) / "sources"

    def write_source(self, text: str) -> Path:
        self.sources.mkdir(parents=True, exist_ok=True)
        path = self.sources / "machine.MACHINE.md"
        path.write_text(text, encoding="utf-8")
        return path

    def test_import_persists_valid_files_verbatim(self) -> None:
        text = machine_file_text(
            description="A machine that sweeps.",
            spec_json=json.dumps(valid_dag()),
        )
        path = self.write_source(text)
        result = import_machine(path, target_dir=self.library)
        self.assertEqual(result["name"], "sweep")
        stored = self.library / "sweep" / "MACHINE.md"
        self.assertTrue(stored.is_file())
        self.assertEqual(stored.read_text(encoding="utf-8"), text)

    def test_import_preserves_crlf_and_cr_files_byte_for_byte(self) -> None:
        # The import persists the SUPPLIED file: a valid CRLF or CR machine
        # keeps its exact bytes (a read_text/write_text round trip would
        # silently rewrite every line ending), and the stored copy still
        # parses.
        text = machine_file_text(spec_json=json.dumps(valid_dag()))
        self.sources.mkdir(parents=True, exist_ok=True)
        for line_ending in ("\r\n", "\r"):
            source = self.sources / f"{len(line_ending)}-byte-newline.MACHINE.md"
            source.write_bytes(text.replace("\n", line_ending).encode("utf-8"))
            result = import_machine(source, target_dir=self.library)
            stored = Path(result["path"])
            self.assertEqual(stored.read_bytes(), source.read_bytes())
            stored_machine, errors = parse_machine_file(
                stored.read_text(encoding="utf-8"), source=str(stored)
            )
            self.assertEqual(errors, [])
            assert stored_machine is not None
            self.assertEqual(stored_machine.name, "sweep")

    def test_import_rejects_invalid_spec_with_exact_errors_and_persists_nothing(self) -> None:
        invalid_spec = {
            "run": {"max_parallel": None},
            "states": [{"id": "a", "entry": True, "subagent": {"prompt": "P."}}],
        }
        path = self.write_source(machine_file_text(spec_json=json.dumps(invalid_spec)))
        with self.assertRaises(ValueError) as ctx:
            import_machine(path, target_dir=self.library)
        self.assertIn(
            "run max_parallel must be an integer between 1 and 64", str(ctx.exception)
        )
        self.assertFalse(self.library.exists())

    def test_import_rejects_both_forms_and_guaranteed_dead_specs(self) -> None:
        both_forms = {
            "nodes": [{"id": "a", "subagent": {"prompt": "P."}}],
            "states": [{"id": "s", "subagent": {"prompt": "P."}}],
        }
        path = self.write_source(machine_file_text(spec_json=json.dumps(both_forms)))
        with self.assertRaises(ValueError) as ctx:
            import_machine(path, target_dir=self.library)
        self.assertIn("pass either dag or machine form, not both", str(ctx.exception))
        dead = {"states": [
            {"id": "start", "entry": True, "subagent": {"prompt": "P."}},
            {
                "id": "loop",
                "subagent": {"prompt": "P."},
                "inputs": [{"name": "v", "type": "text", "from": "loop.v"}],
                "outputs": [{"name": "v", "type": "text"}],
            },
        ]}
        path = self.write_source(machine_file_text(spec_json=json.dumps(dead)))
        with self.assertRaises(ValueError) as ctx:
            import_machine(path, target_dir=self.library)
        self.assertIn("cannot require itself", str(ctx.exception))
        self.assertFalse(self.library.exists())

    def test_import_rejects_malformed_files_with_format_errors(self) -> None:
        path = self.write_source("# no frontmatter here")
        with self.assertRaises(ValueError) as ctx:
            import_machine(path, target_dir=self.library)
        self.assertIn("must start with a `---` frontmatter block", str(ctx.exception))
        self.assertFalse(self.library.exists())

    def test_import_rejects_multiline_descriptions_and_persists_nothing(self) -> None:
        # A quoted description decoding an embedded newline renders the
        # machine across several listing rows: the import gate refuses it
        # with the format's own sentence, exactly like any other parse
        # failure.
        path = self.write_source(machine_file_text(frontmatter=(
            "---\nname: multiline\n"
            'description: "A machine that\\nsweeps the branch."\n'
            "version: 1\nauthor: Tester\n---"
        )))
        with self.assertRaises(ValueError) as ctx:
            import_machine(path, target_dir=self.library)
        self.assertIn("frontmatter description must be a single line", str(ctx.exception))
        self.assertFalse(self.library.exists())

    def test_import_rejects_missing_files_and_overwrites_renamed(self) -> None:
        with self.assertRaises(ValueError) as ctx:
            import_machine(self.sources / "nope.MACHINE.md", target_dir=self.library)
        self.assertIn("machine file not found", str(ctx.exception))
        text = machine_file_text()
        first = self.write_source(text)
        import_machine(first, target_dir=self.library)
        result = import_machine(first, target_dir=self.library)
        self.assertFalse(result["created"])

    def test_import_gate_is_the_write_time_validator(self) -> None:
        # The exact sentences import_machine raises are the write-time
        # validator's: bypassing the gate must fail the reject test above.
        invalid_spec = {"states": [{"id": "a", "entry": True, "subagent": 5}]}
        path = self.write_source(machine_file_text(spec_json=json.dumps(invalid_spec)))
        with self.assertRaises(ValueError) as ctx:
            import_machine(path, target_dir=self.library)
        self.assertIn(
            "state a requires a subagent", str(ctx.exception)
        )
        self.assertFalse(self.library.exists())


class MachineLibraryResolutionTest(unittest.TestCase):
    """Repo directory first, user directory second."""

    def setUp(self) -> None:
        temp = TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        root = Path(temp.name).resolve()
        self.repo = root / "repo-machines"
        self.user = root / "user-machines"

    def store(self, root: Path, name: str, description: str, spec: dict[str, Any] | None = None) -> Path:
        directory = root / name
        directory.mkdir(parents=True, exist_ok=True)
        spec = spec if spec is not None else {"states": [
            {"id": "a", "entry": True, "subagent": {"prompt": "P."}}
        ]}
        text = machine_file_text(name=name, description=description, spec_json=json.dumps(spec))
        path = directory / "MACHINE.md"
        path.write_text(text, encoding="utf-8")
        return path

    def test_repo_dir_wins_over_user_dir(self) -> None:
        repo_path = self.store(self.repo, "sweep", "The repo machine.")
        self.store(self.user, "sweep", "The user machine.")
        machine, path = resolve_machine("sweep", repo_dir=self.repo, user_dir=self.user)
        self.assertEqual(path, repo_path)
        self.assertEqual(machine.description, "The repo machine.")

    def test_frontmatter_name_wins_over_the_directory_name(self) -> None:
        # Mirrors the skill library: the declared name is the machine's
        # name even when its directory is named differently.
        directory = self.user / "renamed-dir"
        directory.mkdir(parents=True)
        (directory / "MACHINE.md").write_text(
            machine_file_text(name="sweep", description="The renamed machine."),
            encoding="utf-8",
        )
        machine, path = resolve_machine("sweep", repo_dir=self.repo, user_dir=self.user)
        self.assertEqual(path, directory / "MACHINE.md")
        self.assertEqual(machine.description, "The renamed machine.")
        listed = list_machines(repo_dir=self.repo, user_dir=self.user)
        self.assertEqual([entry["name"] for entry in listed], ["sweep"])

    def test_the_declared_name_wins_over_the_directory_name(self) -> None:
        # The fast path reads <dir>/<name>/MACHINE.md but only returns it
        # when its DECLARED name matches: a directory named `misdir`
        # holding `name: actual` is not the machine `misdir` — it resolves
        # as `actual` (and a repo machine declared `sweep` is never
        # shadowed by a `sweep/` directory that declares another name).
        directory = self.user / "misdir"
        directory.mkdir(parents=True)
        (directory / "MACHINE.md").write_text(
            machine_file_text(name="actual", description="Declared, not directory-named."),
            encoding="utf-8",
        )
        machine, path = resolve_machine("actual", repo_dir=self.repo, user_dir=self.user)
        self.assertEqual(path, directory / "MACHINE.md")
        with self.assertRaises(ValueError) as raised:
            resolve_machine("misdir", repo_dir=self.repo, user_dir=self.user)
        self.assertIn("unknown machine 'misdir'", str(raised.exception))

    def test_a_directory_named_machine_shadowing_is_refused(self) -> None:
        # repo/sweep/ declares `other`: asking for `sweep` must not return
        # that file, and a user machine legitimately named `sweep` wins.
        misnamed = self.repo / "sweep"
        misnamed.mkdir(parents=True)
        (misnamed / "MACHINE.md").write_text(
            machine_file_text(name="other", description="Not sweep."),
            encoding="utf-8",
        )
        self.store(self.user, "sweep", "The real sweep.")
        machine, path = resolve_machine("sweep", repo_dir=self.repo, user_dir=self.user)
        self.assertEqual(path, self.user / "sweep" / "MACHINE.md")
        self.assertEqual(machine.description, "The real sweep.")

    def test_user_dir_serves_names_the_repo_does_not_have(self) -> None:
        self.store(self.repo, "repo-only", "The repo machine.")
        user_path = self.store(self.user, "mine", "The user machine.")
        machine, path = resolve_machine("mine", repo_dir=self.repo, user_dir=self.user)
        self.assertEqual(path, user_path)
        self.assertEqual(machine.description, "The user machine.")

    def test_unknown_name_lists_available_machines(self) -> None:
        self.store(self.repo, "builder", "Builds.")
        self.store(self.user, "sweep", "Sweeps.")
        with self.assertRaises(ValueError) as ctx:
            resolve_machine("missing", repo_dir=self.repo, user_dir=self.user)
        self.assertIn("unknown machine 'missing'", str(ctx.exception))
        self.assertIn("builder", str(ctx.exception))
        self.assertIn("sweep", str(ctx.exception))

    def test_invalid_machine_name_is_rejected_before_scanning(self) -> None:
        with self.assertRaises(ValueError) as ctx:
            resolve_machine("Not A Name", repo_dir=self.repo, user_dir=self.user)
        self.assertIn("invalid characters", str(ctx.exception))

    def test_list_machines_dedupes_repo_first_and_sorts_by_name(self) -> None:
        self.store(self.repo, "sweep", "The repo machine.")
        self.store(self.user, "sweep", "The user machine.")
        self.store(self.user, "alpha", "An early machine.")
        listed = list_machines(repo_dir=self.repo, user_dir=self.user)
        self.assertEqual([entry["name"] for entry in listed], ["alpha", "sweep"])
        sweep = next(entry for entry in listed if entry["name"] == "sweep")
        self.assertEqual(sweep["source"], "repo")
        self.assertEqual(sweep["description"], "The repo machine.")
        self.assertEqual(listed[0]["source"], "user")

    def test_list_machines_skips_broken_files(self) -> None:
        self.store(self.user, "good", "Good machine.")
        broken = self.user / "broken" / "MACHINE.md"
        broken.parent.mkdir(parents=True, exist_ok=True)
        broken.write_text("no frontmatter", encoding="utf-8")
        listed = list_machines(repo_dir=self.repo, user_dir=self.user)
        self.assertEqual([entry["name"] for entry in listed], ["good"])

    def test_list_machines_excludes_spec_invalid_files_with_their_errors(self) -> None:
        # The listing reports only machines resolve/run can use: a repo
        # file whose spec fails the write-time validator never wins the
        # name, a valid user machine shows instead, and the exact
        # validator sentences ride the scan warnings (the CLI list
        # surface) naming the broken file.
        invalid = self.store(self.repo, "sweep", "The broken repo machine.", spec={"states": []})
        self.store(self.user, "sweep", "The user machine.")
        listed, warnings = _scan_machine_library(repo_dir=self.repo, user_dir=self.user)
        self.assertEqual([entry["name"] for entry in listed], ["sweep"])
        self.assertEqual(listed[0]["source"], "user")
        self.assertEqual(listed[0]["description"], "The user machine.")
        self.assertEqual(
            warnings,
            [f"{invalid}: factory machine must declare between 1 and 1024 states, got 0"],
        )
        self.assertEqual(list_machines(repo_dir=self.repo, user_dir=self.user)[0]["source"], "user")

    def test_resolve_machine_raises_exact_parse_errors_for_broken_files(self) -> None:
        broken = self.user / "broken" / "MACHINE.md"
        broken.parent.mkdir(parents=True, exist_ok=True)
        broken.write_text("---\nname: broken\n---\n\n```machine-spec\n{}\n```\n", encoding="utf-8")
        with self.assertRaises(ValueError) as ctx:
            resolve_machine("broken", repo_dir=self.repo, user_dir=self.user)
        self.assertIn("frontmatter description is required", str(ctx.exception))

    def test_resolve_machine_reports_non_utf8_files_as_broken(self) -> None:
        # A machine file that exists but does not decode is a broken library
        # file, exactly like one that fails to parse: the decode error rides
        # the broken frame with the SAME sentence the listing scan warns
        # with (the shared verdict's wording, path-prefixed).
        # (UnicodeDecodeError is a ValueError, so an unguarded read would
        # instead surface through run_factory's name-rule arm as an
        # invalid id.)
        corrupt = self.user / "broken" / "MACHINE.md"
        corrupt.parent.mkdir(parents=True, exist_ok=True)
        corrupt.write_bytes(b"\xff\xfe\xff not utf-8")
        with self.assertRaises(MachineResolutionError) as ctx:
            resolve_machine("broken", repo_dir=self.repo, user_dir=self.user)
        self.assertTrue(ctx.exception.broken)
        self.assertIn("not valid UTF-8", str(ctx.exception))
        self.assertIn(str(corrupt), str(ctx.exception))

    def test_resolve_machine_reports_spec_invalid_files_as_broken(self) -> None:
        # A file that parses but carries a spec the write-time validator
        # rejects is the exists-but-broken case at resolve time when it is
        # the name's only carrier — the exact broken frame naming the file,
        # never a usable machine the run rejects late, and never a missing
        # frame. The scan path below skips one in a differently-named
        # directory, so an unknown name keeps the missing frame instead of
        # resolving a spec-invalid file, and a file at the name's directory
        # that DECLARES another name never carried the requested one: the
        # invalid file claims no name on either surface.
        invalid = self.store(self.repo, "sweep", "The broken repo machine.", spec={"states": []})
        with self.assertRaises(MachineResolutionError) as ctx:
            resolve_machine("sweep", repo_dir=self.repo, user_dir=self.user)
        self.assertTrue(ctx.exception.broken)
        message = str(ctx.exception)
        self.assertIn(str(invalid), message)
        self.assertIn("factory machine must declare between 1 and 1024 states, got 0", message)
        renamed = self.repo / "renamed-dir"
        renamed.mkdir(parents=True)
        (renamed / "MACHINE.md").write_text(
            machine_file_text(
                name="ghost", description="Ghost.", spec_json=json.dumps({"states": []})
            ),
            encoding="utf-8",
        )
        with self.assertRaises(MachineResolutionError) as scan_ctx:
            resolve_machine("ghost", repo_dir=self.repo, user_dir=self.user)
        self.assertFalse(scan_ctx.exception.broken)
        self.assertIn("unknown machine 'ghost'", str(scan_ctx.exception))
        misnamed = self.repo / "warp"
        misnamed.mkdir(parents=True)
        (misnamed / "MACHINE.md").write_text(
            machine_file_text(
                name="other", description="Not warp.", spec_json=json.dumps({"states": []})
            ),
            encoding="utf-8",
        )
        with self.assertRaises(MachineResolutionError) as misnamed_ctx:
            resolve_machine("warp", repo_dir=self.repo, user_dir=self.user)
        self.assertFalse(misnamed_ctx.exception.broken)
        self.assertIn("unknown machine 'warp'", str(misnamed_ctx.exception))

    def test_list_and_resolve_agree_on_spec_invalid_files(self) -> None:
        # Cursor's finding: the scan skips a spec-invalid repo machine so
        # the listing surfaces a valid user machine of the same name, but
        # resolve still raised broken on the repo file — `factory list`
        # advertised a machine that run and export refused. Both surfaces
        # now share _read_library_machine's verdict: an invalid file never
        # claims its name, so the valid user machine serves exactly where
        # the listing shows it, and export copies it verbatim.
        self.store(self.repo, "sweep", "The broken repo machine.", spec={"states": []})
        user_path = self.store(self.user, "sweep", "The user machine.")
        listed = list_machines(repo_dir=self.repo, user_dir=self.user)
        self.assertEqual([entry["name"] for entry in listed], ["sweep"])
        self.assertEqual(listed[0]["source"], "user")
        machine, path = resolve_machine("sweep", repo_dir=self.repo, user_dir=self.user)
        self.assertEqual(path, user_path)
        self.assertEqual(machine.description, "The user machine.")
        self.assertEqual(validate_factory_spec(machine.spec), [])
        out = self.user.parent / "exported.MACHINE.md"
        result = export_library_machine("sweep", out, repo_dir=self.repo, user_dir=self.user)
        self.assertEqual(result["source"], "library")
        self.assertEqual(out.read_text(encoding="utf-8"), user_path.read_text(encoding="utf-8"))

    def test_resolve_falls_through_parse_broken_files_to_valid_user_machines(self) -> None:
        # The shared verdict covers every invalidity class: a repo file
        # that fails to parse claims its name no more than a spec-invalid
        # one, so the valid user machine serves on both surfaces (the
        # listing always skipped it) instead of resolve raising broken on
        # the repo file.
        broken = self.repo / "sweep" / "MACHINE.md"
        broken.parent.mkdir(parents=True)
        broken.write_text("no frontmatter", encoding="utf-8")
        user_path = self.store(self.user, "sweep", "The user machine.")
        listed = list_machines(repo_dir=self.repo, user_dir=self.user)
        self.assertEqual([entry["name"] for entry in listed], ["sweep"])
        machine, path = resolve_machine("sweep", repo_dir=self.repo, user_dir=self.user)
        self.assertEqual(path, user_path)
        self.assertEqual(machine.description, "The user machine.")

    def test_one_non_utf8_file_never_poisons_the_listing_scan(self) -> None:
        # The shared scan skips a non-decodable file like any other broken
        # one (its warning rides the CLI list surface), so it can neither
        # break the listing nor reframe an unrelated unknown name.
        self.store(self.repo, "builder", "Builds.")
        corrupt = self.user / "zz-corrupt" / "MACHINE.md"
        corrupt.parent.mkdir(parents=True, exist_ok=True)
        corrupt.write_bytes(b"\xff\xfe")
        listed, warnings = _scan_machine_library(repo_dir=self.repo, user_dir=self.user)
        self.assertEqual([entry["name"] for entry in listed], ["builder"])
        self.assertTrue(any("not valid UTF-8" in warning for warning in warnings), warnings)
        with self.assertRaises(MachineResolutionError) as ctx:
            resolve_machine("missing", repo_dir=self.repo, user_dir=self.user)
        self.assertFalse(ctx.exception.broken)
        self.assertIn("unknown machine 'missing'", str(ctx.exception))
        self.assertIn("builder", str(ctx.exception))

    def test_env_overrides_drive_the_production_dirs(self) -> None:
        env = patch.dict(os.environ, {
            "PRIME_AGENT_MACHINES_DIR": str(self.repo),
            "PRIME_AGENT_CODING_AGENT_DIR": str(Path(self.repo).parent / "agent-home"),
        })
        env.start()
        self.addCleanup(env.stop)
        self.store(self.repo, "sweep", "The repo machine.")
        self.assertEqual(repo_machines_dir(), self.repo)
        self.assertEqual(user_machines_dir(), Path(self.repo).parent / "agent-home" / "machines")
        machine, path = resolve_machine("sweep")
        self.assertEqual(path, self.repo / "sweep" / "MACHINE.md")

    def test_the_bundled_library_ships_inside_the_runtime_package(self) -> None:
        # The repo level is the packaged library: the machines directory
        # beside this module (site-packages/rlm/machines in an installed
        # kernel, src/rlm/machines in a checkout), so an installed kernel
        # resolves the seeds a checkout does. The env override still wins.
        packaged = Path(factory_module.__file__).resolve().parent / "machines"
        repo_dir = repo_machines_dir()
        self.assertEqual(repo_dir, packaged)
        self.assertTrue(packaged.is_dir())
        with patch.dict(os.environ, {"PRIME_AGENT_MACHINES_DIR": str(self.repo)}):
            self.assertEqual(repo_machines_dir(), self.repo)

    def test_the_shipped_seed_machines_validate_clean(self) -> None:
        # The repo-level library resolves as the packaged directory and the
        # shipped examples parse, validate, and canonicalize: a broken seed
        # fails here before it can ship.
        repo_dir = repo_machines_dir()
        names = {entry["name"] for entry in list_machines(repo_dir=repo_dir, user_dir=Path("/nonexistent-user-machines"))}
        self.assertIn("review-sweep", names)
        self.assertIn("builder", names)
        self.assertIn("pr-manager", names)
        for name in ("review-sweep", "builder", "pr-manager"):
            machine, path = resolve_machine(name, repo_dir=repo_dir, user_dir=Path("/nonexistent-user-machines"))
            self.assertEqual(machine.name, name)
            self.assertEqual(validate_factory_spec(machine.spec), [], name)
            self.assertEqual(validate_factory_spec(canonicalize_factory_spec(machine.spec)), [], name)
            machine_two = parse_machine_file(path.read_text(encoding="utf-8"), source=str(path))[0]
            assert machine_two is not None
            self.assertEqual(machine_two.spec, machine.spec, name)


class MachineCliDispatchTest(unittest.TestCase):
    """The JSON facade the CLI's factory subcommands drive.

    The payload carries only what the user typed (an op, a path, a name, an
    out target); the dispatch process resolves every library directory
    itself through the production env seams (`PRIME_AGENT_MACHINES_DIR`,
    `PRIME_AGENT_CODING_AGENT_DIR`), so these tests exercise the same
    resolution a real CLI invocation runs.
    """

    def setUp(self) -> None:
        temp = TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        root = Path(temp.name).resolve()
        self.repo = root / "repo-machines"
        self.agent_home = root / "agent-home"
        self.sources = root / "sources"
        self.out_dir = root / "out"
        self.source_text = machine_file_text(
            description="A machine that sweeps.", spec_json=json.dumps(valid_dag())
        )
        self.sources.mkdir(parents=True, exist_ok=True)
        self.source_path = self.sources / "machine.MACHINE.md"
        self.source_path.write_text(self.source_text, encoding="utf-8")
        env = patch.dict(os.environ, {
            "PRIME_AGENT_MACHINES_DIR": str(self.repo),
            "PRIME_AGENT_CODING_AGENT_DIR": str(self.agent_home),
        })
        env.start()
        self.addCleanup(env.stop)

    def test_import_dispatch_persists_into_the_user_library(self) -> None:
        result = cli_dispatch({"op": "import", "path": str(self.source_path)})
        self.assertTrue(result["ok"], result)
        self.assertEqual(result["name"], "sweep")
        destination = self.agent_home / "machines" / "sweep" / "MACHINE.md"
        self.assertEqual(Path(result["path"]), destination)
        self.assertEqual(destination.read_text(encoding="utf-8"), self.source_text)

    def test_import_dispatch_surfaces_gate_errors_as_data(self) -> None:
        bad = self.sources / "bad.MACHINE.md"
        bad.write_text(
            machine_file_text(spec_json=json.dumps({"run": {"max_parallel": None}, "states": [
                {"id": "a", "entry": True, "subagent": {"prompt": "P."}}
            ]})),
            encoding="utf-8",
        )
        result = cli_dispatch({"op": "import", "path": str(bad)})
        self.assertFalse(result["ok"])
        self.assertFalse((self.agent_home / "machines").exists())
        self.assertTrue(any("max_parallel must be an integer between 1 and 64" in e for e in result["errors"]), result)

    def test_export_dispatch_resolves_library_machines(self) -> None:
        import_machine(self.source_path, target_dir=self.repo)
        result = cli_dispatch({
            "op": "export",
            "name": "sweep",
            "out": str(self.out_dir / "shared.MACHINE.md"),
        })
        self.assertTrue(result["ok"], result)
        self.assertEqual(result["source"], "library")
        self.assertEqual(
            Path(result["path"]).read_text(encoding="utf-8"), self.source_text
        )

    def test_export_dispatch_refuses_to_overwrite_the_target(self) -> None:
        import_machine(self.source_path, target_dir=self.repo)
        target = self.out_dir / "shared.MACHINE.md"
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text("keep me", encoding="utf-8")
        result = cli_dispatch({
            "op": "export",
            "name": "sweep",
            "out": str(target),
        })
        self.assertFalse(result["ok"])
        self.assertTrue(any("already exists" in e for e in result["errors"]), result)
        self.assertEqual(target.read_text(encoding="utf-8"), "keep me")

    def test_export_dispatch_resolves_the_library_only(self) -> None:
        # A fresh CLI process has no session state, so the dispatch resolves
        # library machines only: a stored factory entry never intercepts
        # the CLI's export, even when one exists.
        self.agent_home.mkdir(parents=True, exist_ok=True)
        (self.agent_home / "settings.json").write_text(
            json.dumps({"factory": {"enabled": True}}), encoding="utf-8"
        )
        harness = HarnessState(Path(self.agent_home) / "harness_state.json")
        previous_executor = factory_module._DEFAULT_EXECUTOR
        factory_module._DEFAULT_EXECUTOR = FactoryExecutor(harness=harness)
        self.addCleanup(lambda: setattr(factory_module, "_DEFAULT_EXECUTOR", previous_executor))
        harness.create_factory(
            "sweep", "Stored.",
            machine={"states": [{"id": "a", "entry": True, "subagent": {"prompt": "P."}}]},
        )
        result = cli_dispatch({"op": "export", "name": "sweep", "out": str(self.out_dir / "s.MACHINE.md")})
        self.assertFalse(result["ok"])
        self.assertTrue(any("unknown machine 'sweep'" in e for e in result["errors"]), result)

    def test_export_dispatch_reports_unknown_machines(self) -> None:
        result = cli_dispatch({
            "op": "export",
            "name": "ghost",
            "out": str(self.out_dir / "ghost.MACHINE.md"),
        })
        self.assertFalse(result["ok"])
        self.assertTrue(any("unknown machine 'ghost'" in e for e in result["errors"]), result)

    def test_list_dispatch_lists_the_library_with_warnings(self) -> None:
        (self.repo / "sweep").mkdir(parents=True)
        (self.repo / "sweep" / "MACHINE.md").write_text(self.source_text, encoding="utf-8")
        broken = self.agent_home / "machines" / "broken"
        broken.mkdir(parents=True)
        (broken / "MACHINE.md").write_text("no frontmatter", encoding="utf-8")
        result = cli_dispatch({"op": "list"})
        self.assertTrue(result["ok"], result)
        self.assertEqual([m["name"] for m in result["machines"]], ["sweep"])
        self.assertEqual(result["machines"][0]["source"], "repo")
        self.assertTrue(any("broken" in warning for warning in result["warnings"]), result)

    def test_dispatch_rejects_bad_payloads(self) -> None:
        self.assertEqual(cli_dispatch("nope")["ok"], False)
        missing = cli_dispatch({"op": "import"})
        self.assertFalse(missing["ok"])
        self.assertIn("requires a `path` string", missing["errors"][0])
        unknown_op = cli_dispatch({"op": "wat"})
        self.assertFalse(unknown_op["ok"])
        self.assertIn("unknown factory cli op", unknown_op["errors"][0])
        self.assertIn("'list'", unknown_op["errors"][0])


class ExportMachineTest(unittest.TestCase):
    """export_machine serializes library machines, entries, and runs."""

    def setUp(self) -> None:
        temp = TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        root = Path(temp.name).resolve()
        self.library = root / "machines"
        self.out_dir = root / "out"
        self.out_dir.mkdir(parents=True)
        # The opt-in gate: create_factory (below) refuses while the
        # `factory.enabled` setting is off, so the agent dir points at an
        # isolated temp dir whose settings file writes the real document
        # shape the daemon writes -- the core's enabled-fixture pattern.
        # It comes first: the harness load below already names it.
        agent_temp = TemporaryDirectory()
        self.addCleanup(agent_temp.cleanup)
        self._isolate_agent_dir(agent_temp.name)
        self.write_settings({"factory": {"enabled": True}})
        self.harness = HarnessState(root / "harness_state.json")
        self.previous_executor = factory_module._DEFAULT_EXECUTOR
        self.executor = FactoryExecutor(harness=self.harness)
        factory_module._DEFAULT_EXECUTOR = self.executor
        self.addCleanup(lambda: setattr(factory_module, "_DEFAULT_EXECUTOR", self.previous_executor))

    def write_settings(self, document: Any) -> None:
        """Write the agent-dir settings document (the real file shape)."""
        settings_path = Path(os.environ["PRIME_AGENT_CODING_AGENT_DIR"]) / "settings.json"
        settings_path.write_text(json.dumps(document), encoding="utf-8")

    def _isolate_agent_dir(self, agent_dir: str) -> None:
        previous = os.environ.get("PRIME_AGENT_CODING_AGENT_DIR")
        os.environ["PRIME_AGENT_CODING_AGENT_DIR"] = agent_dir

        def restore() -> None:
            if previous is None:
                os.environ.pop("PRIME_AGENT_CODING_AGENT_DIR", None)
            else:
                os.environ["PRIME_AGENT_CODING_AGENT_DIR"] = previous

        self.addCleanup(restore)

    def test_exports_a_library_machine_verbatim(self) -> None:
        text = machine_file_text(description="A machine that sweeps.", spec_json=json.dumps(valid_dag()))
        directory = self.library / "sweep"
        directory.mkdir(parents=True)
        (directory / "MACHINE.md").write_text(text, encoding="utf-8")
        out = self.out_dir / "shared.MACHINE.md"
        result = export_machine("sweep", out, repo_dir=None, user_dir=self.library)
        self.assertEqual(result["source"], "library")
        self.assertEqual(out.read_text(encoding="utf-8"), text)

    def test_export_refuses_to_silently_overwrite_the_target(self) -> None:
        # A fresh target only: an existing file refuses (overwrite=True is
        # the explicit opt-in), so an export never clobbers a user file.
        text = machine_file_text(description="A machine that sweeps.", spec_json=json.dumps(valid_dag()))
        directory = self.library / "sweep"
        directory.mkdir(parents=True)
        (directory / "MACHINE.md").write_text(text, encoding="utf-8")
        out = self.out_dir / "shared.MACHINE.md"
        out.write_text("keep me", encoding="utf-8")
        with self.assertRaises(ValueError) as raised:
            export_machine("sweep", out, repo_dir=None, user_dir=self.library)
        self.assertIn("already exists", str(raised.exception))
        self.assertEqual(out.read_text(encoding="utf-8"), "keep me")
        result = export_machine("sweep", out, repo_dir=None, user_dir=self.library, overwrite=True)
        self.assertEqual(result["source"], "library")
        self.assertEqual(out.read_text(encoding="utf-8"), text)

    def test_export_refuses_a_symlinked_target_without_following_it(self) -> None:
        # The no-overwrite path creates the file exclusively, so a symlink
        # planted at the target refuses instead of being followed and its
        # victim keeps its bytes.
        text = machine_file_text(description="A machine that sweeps.", spec_json=json.dumps(valid_dag()))
        directory = self.library / "sweep"
        directory.mkdir(parents=True)
        (directory / "MACHINE.md").write_text(text, encoding="utf-8")
        victim = self.out_dir / "victim.txt"
        victim.write_text("keep me", encoding="utf-8")
        link = self.out_dir / "link.MACHINE.md"
        link.symlink_to(victim)
        with self.assertRaises(ValueError) as raised:
            export_machine("sweep", link, repo_dir=None, user_dir=self.library)
        self.assertIn("already exists", str(raised.exception))
        self.assertEqual(victim.read_text(encoding="utf-8"), "keep me")
        self.assertTrue(link.is_symlink())

    def test_multiline_entry_content_exports_as_one_line(self) -> None:
        # A stored entry's content is free prose; a machine description
        # must be a single line, so the export collapses it instead of
        # refusing a perfectly ordinary entry.
        self.harness.create_factory(
            "multiline", "First line of prose.\nSecond line of prose.\n\nThird paragraph.",
            machine={"states": [{"id": "a", "entry": True, "subagent": {"prompt": "P."}}]},
        )
        out = self.out_dir / "multiline.MACHINE.md"
        result = export_machine("multiline", out)
        self.assertEqual(result["source"], "spec")
        rendered = out.read_text(encoding="utf-8")
        self.assertIn(
            "description: First line of prose. Second line of prose. Third paragraph.",
            rendered,
        )

    def test_spec_export_refuses_to_silently_overwrite_the_target(self) -> None:
        out = self.out_dir / "spec.MACHINE.md"
        export_factory_spec(valid_machine(), out, name="sweep", description="A machine that sweeps.")
        out.write_text("keep me", encoding="utf-8")
        with self.assertRaises(ValueError) as raised:
            export_factory_spec(valid_machine(), out, name="sweep", description="Overwrite.")
        self.assertIn("already exists", str(raised.exception))
        self.assertEqual(out.read_text(encoding="utf-8"), "keep me")
        export_factory_spec(valid_machine(), out, name="sweep", description="Overwrite.", overwrite=True)
        self.assertIn("Overwrite.", out.read_text(encoding="utf-8"))

    def test_spec_export_multiline_description_is_one_sentence(self) -> None:
        # The single-line rule lives in machine_description_errors alone:
        # one defect, one sentence, whether the description arrives as a
        # parsed frontmatter value or an export argument.
        with self.assertRaises(ValueError) as raised:
            export_factory_spec(
                valid_machine(),
                self.out_dir / "x.MACHINE.md",
                name="sweep",
                description="A machine that\nsweeps.",
            )
        self.assertEqual(
            str(raised.exception), "frontmatter description must be a single line"
        )

    def test_exports_a_stored_entry_spec_byte_pretty(self) -> None:
        self.harness.create_factory("Sweep", "A machine that sweeps.", id="sweep", dag=valid_dag())
        out = self.out_dir / "sweep.MACHINE.md"
        result = export_machine("sweep", out, repo_dir=None, user_dir=self.library)
        self.assertEqual(result["source"], "spec")
        machine, errors = parse_machine_file(out.read_text(encoding="utf-8"), source=str(out))
        self.assertEqual(errors, [])
        assert machine is not None
        self.assertEqual(machine.name, "sweep")
        self.assertEqual(validate_factory_spec(machine.spec), [])
        self.assertEqual(canonicalize_factory_spec(machine.spec), canonicalize_factory_spec(valid_dag()))

    def test_entry_ids_that_are_not_machine_names_are_rejected(self) -> None:
        self.harness.create_factory("Sweep", "A machine that sweeps.", id="Sweep Entry", dag=valid_dag())
        with self.assertRaises(ValueError) as ctx:
            export_machine("Sweep Entry", self.out_dir / "x.MACHINE.md", repo_dir=None, user_dir=self.library)
        self.assertIn("invalid characters", str(ctx.exception))

    def test_exports_a_runs_canonical_machine(self) -> None:
        self.harness.create_factory("Sweep", "A machine that sweeps.", id="sweep", machine=valid_machine())
        # The live run is the host's: its export view (`factory.machine`)
        # carries the run's canonical machine (the Rust port pins the view;
        # this pins the export of it).
        view = {
            "run_id": "run-1",
            "spec_id": "sweep",
            "name": "the run",
            "machine": canonicalize_factory_spec(valid_machine()),
        }
        patcher = patch.object(
            factory_module, "_live_run_machine", lambda run_id: view if run_id == "run-1" else None
        )
        patcher.start()
        self.addCleanup(patcher.stop)
        out = self.out_dir / "run-machine.MACHINE.md"
        result = export_machine("run-1", out, repo_dir=None, user_dir=self.library)
        self.assertEqual(result["source"], "spec")
        machine, errors = parse_machine_file(out.read_text(encoding="utf-8"), source=str(out))
        self.assertEqual(errors, [])
        assert machine is not None
        self.assertEqual(machine.spec, canonicalize_factory_spec(valid_machine()))

    def test_unknown_targets_list_every_source(self) -> None:
        with self.assertRaises(ValueError) as ctx:
            export_machine("ghost", self.out_dir / "ghost.MACHINE.md", repo_dir=None, user_dir=self.library)
        self.assertIn("unknown machine 'ghost'", str(ctx.exception))

    def test_out_path_must_not_be_a_directory(self) -> None:
        directory = self.library / "sweep"
        directory.mkdir(parents=True)
        (directory / "MACHINE.md").write_text(
            machine_file_text(description="A machine that sweeps."), encoding="utf-8"
        )
        with self.assertRaises(ValueError) as ctx:
            export_machine("sweep", self.out_dir, repo_dir=None, user_dir=self.library)
        self.assertIn("is a directory", str(ctx.exception))

    def test_export_rejects_invalid_specs(self) -> None:
        with self.assertRaises(ValueError) as ctx:
            export_factory_spec(
                {"run": {"max_parallel": None}, "states": [{"id": "a", "entry": True, "subagent": {"prompt": "P."}}]},
                self.out_dir / "bad.MACHINE.md",
                name="sweep",
                description="A machine that sweeps.",
            )
        self.assertIn("max_parallel must be an integer between 1 and 64", str(ctx.exception))
        self.assertFalse((self.out_dir / "bad.MACHINE.md").exists())


class FactoryRunFromLibraryTest(unittest.TestCase):
    """rlm.factory.run falls back to the machine library for machine names."""

    def setUp(self) -> None:
        temp = TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        root = Path(temp.name).resolve()
        self.library = root / "machines"
        # Isolate the library resolution from this machine's real home dir,
        # before the harness load below names the agent dir.
        agent_home = root / "agent-home"
        # The library run routes through rlm.factory.run, so it inherits the
        # opt-in gate: the isolated agent dir carries the same enabled
        # settings document the core's executor tests write (the real file
        # shape the daemon writes), or the disabled default refuses the run.
        agent_home.mkdir(parents=True, exist_ok=True)
        (agent_home / "settings.json").write_text(
            json.dumps({"factory": {"enabled": True}}), encoding="utf-8"
        )
        env = patch.dict(os.environ, {
            "PRIME_AGENT_MACHINES_DIR": str(self.library),
            "PRIME_AGENT_CODING_AGENT_DIR": str(agent_home),
        })
        env.start()
        self.addCleanup(env.stop)
        self.harness = HarnessState(root / "harness_state.json")
        self.harness.create_subagent("Worker", "Do the work carefully.", id="worker")
        self.host = ClientHost()
        self.executor = FactoryExecutor(harness=self.harness)
        previous_executor = factory_module._DEFAULT_EXECUTOR
        factory_module._DEFAULT_EXECUTOR = self.executor
        self.addCleanup(lambda: setattr(factory_module, "_DEFAULT_EXECUTOR", previous_executor))
        patcher = patch.object(rlm_module, "host_request", self.host)
        patcher.start()
        self.addCleanup(patcher.stop)

    def store_machine_file(self, name: str, spec: dict[str, Any]) -> Path:
        directory = self.library / name
        directory.mkdir(parents=True, exist_ok=True)
        path = directory / "MACHINE.md"
        path.write_text(
            machine_file_text(name=name, description="A machine that sweeps.", spec_json=json.dumps(spec)),
            encoding="utf-8",
        )
        return path

    @async_test
    async def test_run_resolves_machine_names_from_the_library(self) -> None:
        path = self.store_machine_file(
            "sweep",
            {
                "run": {"failure_policy": "continue", "max_parallel": 2},
                "nodes": [
                    {"id": "a", "subagent": "worker", "outputs": [{"name": "out", "type": "text"}]},
                    {
                        "id": "b",
                        "subagent": {"prompt": "Use {draft}"},
                        "depends_on": ["a"],
                        "inputs": [{"name": "draft", "type": "text", "from": "a.out"}],
                    },
                ],
            },
        )
        result = await rlm_module.rlm.factory.run("sweep")
        self.assertEqual(result["spec_id"], "sweep")
        self.assertEqual(result["machine"], "sweep")
        self.assertEqual(result["machine_path"], str(path))
        # The template ships to the host as-is (it validates and runs it),
        # with its harness subagent references resolved.
        (payload,) = self.host.calls_of("factory.run")
        self.assertEqual((payload["spec_id"], payload["machine"], payload["machine_path"]), ("sweep", "sweep", str(path)))
        value = run_value(payload)
        self.assertEqual(value["spec"], parse_machine_file(path.read_text(encoding="utf-8"))[0].spec)
        self.assertEqual(value["subagents"], {"worker": {"content": "Do the work carefully.", "model": None, "thinking": None}})

    @async_test
    async def test_stored_entries_win_over_library_machines(self) -> None:
        self.store_machine_file(
            "sweep",
            {"nodes": [{"id": "only-a", "subagent": "worker"}]},
        )
        self.harness.create_factory(
            "Sweep", "A stored instance.", id="sweep", dag={"nodes": [
                {"id": "entry-a", "subagent": "worker"},
                {"id": "entry-b", "subagent": "worker"},
            ]}
        )
        result = await rlm_module.rlm.factory.run("sweep")
        self.assertNotIn("machine", result)
        (payload,) = self.host.calls_of("factory.run")
        self.assertNotIn("machine", payload)
        self.assertEqual(
            [node["id"] for node in run_value(payload)["spec"]["nodes"]], ["entry-a", "entry-b"]
        )

    @async_test
    async def test_unknown_names_report_the_library(self) -> None:
        with self.assertRaisesRegex(ValueError, "unknown factory spec 'missing-spec'"):
            await rlm_module.rlm.factory.run("missing-spec")
        try:
            await rlm_module.rlm.factory.run("missing-spec")
        except ValueError as error:
            self.assertIn("no stored factory entry", str(error))
            self.assertIn("no library machine with that name", str(error))

    @async_test
    async def test_an_invalid_name_still_reports_the_unknown_spec_frame(self) -> None:
        # A stored-entry id that is not a legal machine name (spaces,
        # capitals) can never resolve from the library: the lookup must
        # not surface the bare name-rule sentence, losing the unknown-spec
        # frame.
        with self.assertRaises(ValueError) as raised:
            await rlm_module.rlm.factory.run("My Spec")
        message = str(raised.exception)
        self.assertIn("unknown factory spec 'My Spec'", message)
        self.assertIn("not a valid machine name", message)
        self.assertIn("lowercase a-z, 0-9, hyphens", message)
        self.assertEqual(self.host.calls, [])

    @async_test
    async def test_a_broken_library_file_names_its_errors_not_a_missing_name(self) -> None:
        # A file that exists but fails to parse reports the exact parse
        # errors; it never pretends the name is unknown.
        directory = self.library / "broken"
        directory.mkdir(parents=True, exist_ok=True)
        (directory / "MACHINE.md").write_text(
            "---\nname: broken\n---\n\n```machine-spec\n{}\n```\n", encoding="utf-8"
        )
        with self.assertRaises(ValueError) as raised:
            await rlm_module.rlm.factory.run("broken")
        self.assertIn("exists but is broken", str(raised.exception))
        self.assertIn("frontmatter description is required", str(raised.exception))
        self.assertNotIn("no library machine with that name", str(raised.exception))
        self.assertEqual(self.host.calls, [])

    @async_test
    async def test_a_spec_invalid_library_file_is_broken_not_a_late_rejection(self) -> None:
        # A file that parses but carries a spec the write-time validator
        # rejects is the exists-but-broken case at resolve time: the run
        # refuses with the exact validator sentences naming the file,
        # never a late canonicalize error after resolution, and nothing
        # spawns.
        path = self.store_machine_file("sweep", {"states": []})
        with self.assertRaises(ValueError) as raised:
            await rlm_module.rlm.factory.run("sweep")
        message = str(raised.exception)
        self.assertIn("exists but is broken", message)
        self.assertIn("factory machine must declare between 1 and 1024 states, got 0", message)
        self.assertIn(str(path), message)
        self.assertNotIn("no library machine with that name", message)
        self.assertEqual(self.host.calls, [])

    @async_test
    async def test_an_invalid_repo_machine_never_shadows_a_valid_user_machine(self) -> None:
        # Cursor's follow-up finding: the listing scan skips a spec-invalid
        # repo machine so a valid user machine of the same name surfaces,
        # but the run still raised exists-but-broken on the repo file —
        # `factory list` advertised a machine the run refused. Run, resolve,
        # and export share the listing's verdict, so the run serves the
        # user machine the listing advertises; the invalid repo file never
        # shadows it and never masquerades as usable.
        self.store_machine_file("sweep", {"states": []})
        agent_home = Path(os.environ["PRIME_AGENT_CODING_AGENT_DIR"])
        user_file = agent_home / "machines" / "sweep" / "MACHINE.md"
        user_file.parent.mkdir(parents=True)
        user_file.write_text(
            machine_file_text(
                name="sweep",
                description="The user machine.",
                spec_json=json.dumps({"nodes": [{"id": "a", "subagent": "worker"}]}),
            ),
            encoding="utf-8",
        )
        listed = list_machines()
        self.assertEqual([entry["name"] for entry in listed], ["sweep"])
        self.assertEqual(listed[0]["source"], "user")
        result = await rlm_module.rlm.factory.run("sweep")
        self.assertEqual(result["spec_id"], "sweep")
        self.assertEqual(result["machine"], "sweep")
        self.assertEqual(result["machine_path"], str(user_file))
        self.assertEqual(
            run_value(self.host.calls_of("factory.run")[0])["spec"],
            {"nodes": [{"id": "a", "subagent": "worker"}]},
        )

    @async_test
    async def test_a_non_utf8_library_file_is_broken_not_an_invalid_name(self) -> None:
        # A machine file that exists but does not decode is a broken library
        # file: the run reports it in the exists-but-broken frame, never as
        # an invalid machine name (the name-rule arm must not swallow the
        # decode error).
        directory = self.library / "sweep"
        directory.mkdir(parents=True, exist_ok=True)
        (directory / "MACHINE.md").write_bytes(b"\xff\xfe\xff not utf-8")
        with self.assertRaises(ValueError) as raised:
            await rlm_module.rlm.factory.run("sweep")
        message = str(raised.exception)
        self.assertIn("exists but is broken", message)
        self.assertIn("not valid UTF-8", message)
        self.assertNotIn("not a valid machine name", message)
        self.assertEqual(self.host.calls, [])

    @async_test
    async def test_one_corrupt_library_file_never_reframes_unknown_names(self) -> None:
        # One non-decodable file skips in the listing scan like any broken
        # file: an unrelated unknown name keeps the unknown-spec frame, never
        # the name-rule arm the decode error would otherwise reach.
        corrupt = self.library / "zz-corrupt"
        corrupt.mkdir(parents=True, exist_ok=True)
        (corrupt / "MACHINE.md").write_bytes(b"\xff\xfe")
        with self.assertRaises(ValueError) as raised:
            await rlm_module.rlm.factory.run("missing-spec")
        message = str(raised.exception)
        self.assertIn("unknown factory spec 'missing-spec'", message)
        self.assertIn("no library machine with that name", message)
        self.assertNotIn("not a valid machine name", message)
        self.assertEqual(self.host.calls, [])

    @async_test
    async def test_the_opt_in_gate_refuses_library_runs_while_disabled(self) -> None:
        # The library run routes through rlm.factory.run, so it inherits the
        # opt-in gate and the refusal precedes library resolution: while the
        # setting is off, a stored library machine name is refused with the
        # one disabled message -- never an unknown-spec error, never a run
        # -- and nothing spawns.
        self.store_machine_file("sweep", {"nodes": [{"id": "a", "subagent": "worker"}]})
        agent_home = Path(os.environ["PRIME_AGENT_CODING_AGENT_DIR"])
        (agent_home / "settings.json").write_text(
            json.dumps({"factory": {"enabled": False}}), encoding="utf-8"
        )
        with self.assertRaises(ValueError) as raised:
            await rlm_module.rlm.factory.run("sweep")
        self.assertEqual(str(raised.exception), factory_module.FACTORY_DISABLED_MESSAGE)
        self.assertEqual(self.host.calls, [])
        # No settings file at all is the same disabled default.
        (agent_home / "settings.json").unlink()
        with self.assertRaises(ValueError) as raised:
            await rlm_module.rlm.factory.run("sweep")
        self.assertEqual(str(raised.exception), factory_module.FACTORY_DISABLED_MESSAGE)
        self.assertEqual(self.host.calls, [])

    @async_test
    async def test_run_from_library_ships_dag_sugar_to_the_host(self) -> None:
        # The host compiles dag sugar to machine form and canonicalizes it
        # (the Rust port's run battery pins the compiled run); the client
        # ships the template's own spec, dag form included.
        spec = {
            "nodes": [
                {"id": "src", "subagent": "worker", "outputs": [{"name": "v", "type": "text"}]},
                {
                    "id": "fan-in",
                    "subagent": {"prompt": "Merge {v}."},
                    "depends_on": ["src"],
                    "inputs": [{"name": "v", "type": "text", "from": "src.v"}],
                },
            ],
        }
        self.store_machine_file("pipeline", spec)
        result = await rlm_module.rlm.factory.run("pipeline")
        self.assertEqual(result["machine"], "pipeline")
        self.assertEqual(run_value(self.host.calls_of("factory.run")[0])["spec"], spec)


# ---------------------------------------------------------------------------
# The installed-runtime library: the wheel a kernel venv actually installs.
# ---------------------------------------------------------------------------

_INSTALLED_LIBRARY_RUNNER = r"""
import asyncio
import json


class ScriptedHost:
    # Deterministic fake for the host's executor: `factory.run` records the
    # run payload the installed client ships and echoes its run identity.
    def __init__(self):
        self.payloads = []

    async def __call__(self, request_type, payload=None):
        payload = payload or {}
        if request_type != "factory.run":
            raise AssertionError(f"unexpected host request {request_type!r}")
        self.payloads.append(payload)
        return {
            "result": {
                "run_id": "run-1",
                "spec_id": payload["spec_id"],
                "machine": payload.get("machine"),
                "machine_path": payload.get("machine_path"),
            }
        }


async def main():
    import rlm as rlm_module
    import rlm.factory as factory_module

    host = ScriptedHost()
    rlm_module.host_request = host
    result = await rlm_module.rlm.factory.run("review-sweep")
    spec = factory_module._decode_value(host.payloads[0]["value"], [])["spec"]
    print(json.dumps({
        "spec_id": result["spec_id"],
        "machine": result["machine"],
        "machine_path": result["machine_path"],
        "nodes": sorted(node["id"] for node in spec.get("nodes") or spec.get("states")),
    }))


asyncio.run(main())
"""


class InstalledRuntimeLibraryTest(unittest.TestCase):
    """A kernel venv built from a staged runtime runs the bundled machines.

    The kernel installs prime-agent-runtime non-editably into its venv (the
    bootstrap's ``uv pip install <staged runtime>`` builds the hatchling
    wheel, whose target package is ``src/rlm``), so the machine library
    must resolve from the installed package — ``site-packages/rlm/
    machines`` — not from any source-checkout path. This stages the
    runtime the way the release does, installs it into a fresh venv, and
    runs ``rlm.factory.run("review-sweep")`` in that interpreter against a
    scripted host, asserting the machine the client ships came from the
    installed wheel.
    """

    # Names the release staging drops from the runtime tree (the assemble
    # script's RUNTIME_EXCLUDED_NAMES): the venv bootstrap installs the
    # staged layout, so the test stages the same way.
    STAGING_EXCLUDED = frozenset({"test", "uv.lock", ".venv", "__pycache__", ".pytest_cache"})

    def setUp(self) -> None:
        temp = TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.root = Path(temp.name).resolve()

    def stage_runtime(self) -> Path:
        """Copy the runtime tree the release-staging way, minus its excludes."""
        runtime_dir = Path(__file__).resolve().parents[1]
        staged = self.root / "payload" / "prime-agent-runtime"
        for source in runtime_dir.rglob("*"):
            relative = source.relative_to(runtime_dir)
            if any(part in self.STAGING_EXCLUDED for part in relative.parts):
                continue
            target = staged / relative
            if source.is_dir():
                target.mkdir(parents=True, exist_ok=True)
            else:
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(source.read_bytes())
        return staged

    def test_kernel_venv_runs_review_sweep_from_the_installed_wheel(self) -> None:
        if os.name != "posix":
            self.skipTest("the staged kernel-venv path is POSIX-shaped")
        uv = shutil.which("uv")
        if uv is None:
            self.skipTest("uv is not available to build the kernel venv")
        staged = self.stage_runtime()
        venv = self.root / "kernel-venv"
        agent_home = self.root / "agent-home"
        agent_home.mkdir(parents=True)
        (agent_home / "settings.json").write_text(
            json.dumps({"factory": {"enabled": True}}), encoding="utf-8"
        )
        for args in (
            [uv, "venv", str(venv)],
            [uv, "pip", "install", "--python", str(venv / "bin" / "python"), "--no-deps", str(staged)],
        ):
            install = subprocess.run(
                args, capture_output=True, text=True, timeout=240, check=False
            )
            self.assertEqual(
                install.returncode, 0,
                f"{' '.join(args)} failed:\n{install.stdout}\n{install.stderr}",
            )
        run_result = subprocess.run(
            [
                str(venv / "bin" / "python"), "-I", "-c", _INSTALLED_LIBRARY_RUNNER,
            ],
            capture_output=True,
            text=True,
            timeout=120,
            check=False,
            # The installed runtime validates through the host (no serving
            # kernel here), so it is pointed at this checkout's host build.
            env={
                **os.environ,
                "PRIME_AGENT_CODING_AGENT_DIR": str(agent_home),
                factory_module.HOST_BINARY_ENV: factory_module._dev_host_binary() or "",
            },
        )
        self.assertEqual(
            run_result.returncode, 0,
            f"the installed-runtime run failed:\n{run_result.stdout}\n{run_result.stderr}",
        )
        payload = json.loads(run_result.stdout)
        self.assertEqual(payload["spec_id"], "review-sweep")
        self.assertEqual(payload["machine"], "review-sweep")
        machine_path = Path(payload["machine_path"])
        self.assertTrue(machine_path.is_file(), machine_path)
        self.assertIn("site-packages", str(machine_path), machine_path)
        self.assertIn(os.path.join("rlm", "machines"), str(machine_path), machine_path)
        # The installed package is the wheel copy, not this checkout's source.
        self.assertNotIn("prime-agent-runtime", str(machine_path), machine_path)
        self.assertEqual(payload["nodes"], ["files", "report", "review"])
