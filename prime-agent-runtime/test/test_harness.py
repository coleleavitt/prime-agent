from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from rlm import harness as package_harness
from rlm import rlm as callable_rlm
from rlm.harness import HarnessState, get_harness_state, normalize_trust, trust_of

PYTHON_REFERENCE = {
    "type": "python",
    "import": "agent_skills.example",
    "callable": "run",
    "call_pattern": "await run(...)",
}


class HarnessStateTest(unittest.TestCase):
    def test_crud_for_all_entry_kinds(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")

            created = {
                "prompt": state.create_prompt_note(
                    "Prompt note",
                    "Prompt content",
                    id="prompt_entry",
                    path="prompt/path",
                    metadata={"kind": "prompt"},
                ),
                "memory": state.create_memory(
                    "Memory",
                    "Memory content",
                    id="memory_entry",
                    path="memory/path",
                    metadata={"kind": "memory"},
                ),
                "skill": state.create_skill(
                    "Skill",
                    "Skill content",
                    id="skill_entry",
                    path="skill/path",
                    reference=PYTHON_REFERENCE,
                    arguments={"target": {"type": "string", "required": True}},
                    metadata={"kind": "skill"},
                ),
                "subagent": state.create_subagent(
                    "Subagent",
                    "Subagent content",
                    id="subagent_entry",
                    path="subagent/path",
                    metadata={"kind": "subagent"},
                ),
            }

            for kind, entry in created.items():
                self.assertEqual(entry.kind, kind)
                self.assertIn("content", state.get(kind, entry.id).content.lower())
                self.assertIn(entry, state.list(kind))

            state.update_prompt_note("prompt_entry", "Prompt note", "Prompt content updated")
            state.update_memory("memory_entry", "Memory", "Memory content updated")
            state.update_skill(
                "skill_entry",
                "Skill",
                "Skill content updated",
                reference=PYTHON_REFERENCE,
                arguments={"target": {"type": "string", "required": True}, "mode": {"type": "string"}},
            )
            state.update_subagent("subagent_entry", "Subagent", "Subagent content updated")

            for kind in ("prompt", "memory", "skill", "subagent"):
                entry_id = f"{kind}_entry"
                self.assertEqual(state.get(kind, entry_id).version, 2)
                self.assertIn("updated", state.get(kind, entry_id).content)
                delete_method = getattr(state, f"delete_{'prompt_note' if kind == 'prompt' else kind}")
                self.assertTrue(delete_method(entry_id))
                self.assertIsNone(state.get(kind, entry_id))
                self.assertFalse(delete_method(entry_id))

            self.assertEqual(state.list(), [])

    def test_persists_entries_and_refinements(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")

            memory = state.create_memory(
                "Prefer focused patches",
                "Small harness updates are easier to validate than broad rewrites.",
                path="engineering",
            )
            skill = state.create_skill(
                "Check failures first",
                "Inspect current failure evidence before editing code.",
                id="failure_first",
                reference=PYTHON_REFERENCE,
                arguments={"failure_log": {"type": "string", "description": "Current failure evidence."}},
            )
            subagent = state.create_subagent(
                "Reviewer",
                "Review the proposed patch for regressions and missing tests.",
                metadata={"max_turns": 3},
            )
            state.create_prompt_note("Refinement cadence", "Refine only after repeated evidence.")
            event = state.record_refinement(
                "skill failed twice",
                ["updated failure_first skill", "added reviewer subagent"],
                evidence="two failed validations",
                outcome="next validation passed",
            )

            reloaded = HarnessState(state.file_path)

            self.assertEqual(reloaded.get("memory", memory.id).content, memory.content)
            self.assertEqual(reloaded.get("skill", skill.id).version, 1)
            self.assertEqual(reloaded.get("skill", skill.id).arguments["failure_log"]["type"], "string")
            self.assertEqual(reloaded.get("subagent", subagent.id).metadata["max_turns"], 3)
            self.assertEqual(reloaded.refinements[0].id, event.id)
            self.assertIn("Prefer focused patches", reloaded.overview())
            self.assertIn(
                "Call contract: installed Python skills use await <skill_import>(...)",
                reloaded.overview(),
            )
            overview = reloaded.overview()
            self.assertIn("handle = await rlm('sub-task')", overview)
            self.assertIn("never the child's answer", overview)
            self.assertIn("receiver_role='parent'", overview)
            self.assertIn("await rlm.list_subagents()", overview)
            self.assertIn("receiver_role='child'", overview)
            self.assertIn("refinements: 1", reloaded.overview())

    def test_load_ignores_unknown_json_keys(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state_path = Path(temp_dir) / "harness_state.json"
            state_path.write_text(
                json.dumps(
                    {
                        "schema": 1,
                        "entries": {
                            "memory": {
                                "known": {
                                    "id": "mismatched",
                                    "kind": "skill",
                                    "title": "Known memory",
                                    "content": "Loaded despite extra keys.",
                                    "path": 123,
                                    "source": None,
                                    "version": "2",
                                    "metadata": "not a dict",
                                    "unexpected": True,
                                },
                                "missing_content": {
                                    "title": "Missing content",
                                }
                            }
                        },
                        "refinements": [
                            {
                                "id": "refine_extra",
                                "trigger": "extra keys",
                                "changes": [1, "loaded"],
                                "ignored": "value",
                            },
                            {
                                "id": "refine_missing_changes",
                                "trigger": "missing changes",
                            }
                        ],
                    }
                ),
                encoding="utf-8",
            )

            state = HarnessState(state_path)

            self.assertEqual(state.get("memory", "known").content, "Loaded despite extra keys.")
            self.assertEqual(state.get("memory", "known").id, "known")
            self.assertEqual(state.get("memory", "known").kind, "memory")
            self.assertEqual(state.get("memory", "known").path, "general")
            self.assertEqual(state.get("memory", "known").source, "agent")
            self.assertIsNone(state.get("memory", "mismatched"))
            self.assertEqual(state.get("memory", "known").version, 2)
            self.assertEqual(state.get("memory", "known").metadata, {})
            self.assertIsNone(state.get("memory", "missing_content"))
            self.assertEqual(state.refinements[0].id, "refine_extra")
            self.assertEqual(state.refinements[0].changes, ["1", "loaded"])
            self.assertEqual(len(state.refinements), 1)
            self.assertIn("1, loaded", state.overview())

            updated = state.update_memory("known", "Known memory", "Updated content.")
            self.assertEqual(updated.version, 3)

    def test_skill_arguments_are_first_class(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")

            created = state.create_skill(
                "Edit file",
                "Apply a targeted edit.",
                id="edit_file",
                reference={
                    "type": "python",
                    "import": "agent_skills.file_edit",
                    "callable": "file_edit",
                    "call_pattern": "await file_edit(path=..., find=..., replace=...)",
                },
                arguments={
                    "path": {"type": "string", "required": True},
                    "find": {"type": "string", "required": True},
                    "replace": {"type": "string", "required": True},
                },
            )
            updated = state.update_skill(
                "edit_file",
                "Edit file",
                "Apply a targeted edit after reading context.",
                reference={
                    "type": "python",
                    "import": "agent_skills.file_edit",
                    "callable": "file_edit",
                    "call_pattern": "await file_edit(path=..., find=..., replace=...)",
                },
                arguments={
                    "path": {"type": "string", "required": True},
                    "find": {"type": "string", "required": True},
                    "replace": {"type": "string", "required": True},
                    "validate": {"type": "boolean", "default": True},
                },
            )
            reloaded = HarnessState(state.file_path)

            self.assertEqual(created.arguments["path"]["required"], True)
            self.assertEqual(created.reference["type"], "python")
            self.assertEqual(updated.version, 2)
            self.assertEqual(reloaded.get("skill", "edit_file").arguments["validate"]["default"], True)
            self.assertEqual(reloaded.get("skill", "edit_file").reference["import"], "agent_skills.file_edit")
            self.assertIn('"path"', reloaded.overview())
            self.assertIn("agent_skills", reloaded.overview())

    def test_skill_references_must_be_python(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")

            with self.assertRaisesRegex(ValueError, "Python reference"):
                state.create_skill("No reference", "missing", arguments={})
            with self.assertRaisesRegex(ValueError, "reference.type must be 'python'"):
                state.create_skill(
                    "Shell reference",
                    "bad",
                    reference={"type": "shell", "command": "edit"},
                    arguments={},
                )
            with self.assertRaisesRegex(ValueError, "Python import"):
                state.create_skill("No import", "bad", reference={"type": "python", "callable": "run"}, arguments={})
            with self.assertRaisesRegex(ValueError, "callable or call_pattern"):
                state.create_skill(
                    "No callable",
                    "bad",
                    reference={"type": "python", "import": "agent_skills.bad"},
                    arguments={},
                )

    def test_load_tolerates_corrupt_or_non_object_state(self) -> None:
        for payload in ("not json at all", "null", "[]", '"a string"', "123"):
            with tempfile.TemporaryDirectory() as temp_dir:
                state_path = Path(temp_dir) / "harness_state.json"
                state_path.write_text(payload, encoding="utf-8")

                state = HarnessState(state_path)

                self.assertEqual(state.list(), [])
                self.assertEqual(state.refinements, [])
                # The store must remain usable and self-heal on the next write.
                created = state.create_memory("Recovered", "Works after corruption.", id="recovered")
                self.assertEqual(HarnessState(state_path).get("memory", "recovered").content, created.content)

    def test_update_skill_preserves_omitted_arguments(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")
            state.create_skill(
                "Edit file",
                "Apply an edit.",
                id="edit_file",
                reference=PYTHON_REFERENCE,
                arguments={"path": {"type": "string", "required": True}},
            )

            # Updating only title/content (arguments omitted) must keep the contract.
            state.update_skill("edit_file", "Edit file", "Apply an edit carefully.", reference=PYTHON_REFERENCE)
            self.assertEqual(state.get("skill", "edit_file").arguments, {"path": {"type": "string", "required": True}})

            # An explicit empty dict still clears it.
            state.update_skill("edit_file", "Edit file", "Now argument-free.", reference=PYTHON_REFERENCE, arguments={})
            self.assertEqual(state.get("skill", "edit_file").arguments, {})

    def test_update_skill_without_reference_preserves_contract(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")
            state.create_skill(
                "Edit file",
                "Apply an edit.",
                id="edit_file",
                reference=PYTHON_REFERENCE,
                arguments={"path": {"type": "string", "required": True}},
            )

            # A title/content-only update must not require re-sending the reference,
            # and must preserve the existing reference and arguments.
            updated = state.update_skill("edit_file", "Edit file", "Apply an edit carefully.")

            self.assertEqual(updated.version, 2)
            self.assertEqual(updated.reference, PYTHON_REFERENCE)
            self.assertEqual(updated.arguments, {"path": {"type": "string", "required": True}})
            self.assertEqual(updated.content, "Apply an edit carefully.")

    def test_update_preserves_omitted_path(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")
            state.create_memory("Grouped", "content", id="grouped", path="repo/testing")

            # Updating without a path keeps the custom grouping path.
            state.update_memory("grouped", "Grouped", "new content")
            self.assertEqual(state.get("memory", "grouped").path, "repo/testing")

            # An explicit path still moves it.
            state.update_memory("grouped", "Grouped", "newer", path="repo/other")
            self.assertEqual(state.get("memory", "grouped").path, "repo/other")

    def test_in_memory_state_never_touches_disk(self) -> None:
        previous = os.environ.get("RLM_HARNESS_STATE_DIR")
        previous_global = os.environ.get("RLM_GLOBAL_HARNESS_STATE_DIR")
        with tempfile.TemporaryDirectory() as temp_dir:
            os.environ["RLM_HARNESS_STATE_DIR"] = temp_dir
            os.environ.pop("RLM_GLOBAL_HARNESS_STATE_DIR", None)
            try:
                state = HarnessState(in_memory=True)
                created = state.create_memory("Volatile", "in memory only", id="volatile")
                state.record_refinement("trigger", ["change"])

                self.assertIsNone(state.file_path)
                self.assertEqual(created.content, "in memory only")
                self.assertEqual(state.get("memory", "volatile").content, "in memory only")
                # Local in-memory operations do not resolve or persist a path.
                self.assertEqual(list(Path(temp_dir).iterdir()), [])
            finally:
                if previous is None:
                    os.environ.pop("RLM_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_HARNESS_STATE_DIR"] = previous
                if previous_global is None:
                    os.environ.pop("RLM_GLOBAL_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = previous_global

    def test_in_memory_state_global_flag_uses_global_env_store(self) -> None:
        previous_global = os.environ.get("RLM_GLOBAL_HARNESS_STATE_DIR")
        with tempfile.TemporaryDirectory() as temp_dir:
            global_dir = Path(temp_dir) / "global"
            os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = str(global_dir)
            try:
                state = HarnessState(in_memory=True)
                global_entry = state.create_memory("Global note", "persisted", id="global_note", global_=True)
            finally:
                if previous_global is None:
                    os.environ.pop("RLM_GLOBAL_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = previous_global

            self.assertIsNone(state.file_path)
            self.assertEqual(global_entry.scope, "global")
            self.assertEqual(global_entry.content, "persisted")
            self.assertIsNone(state.get("memory", "global_note"))
            self.assertEqual(
                HarnessState(global_dir / "harness_state.json", scope="global").get("memory", "global_note").content,
                "persisted",
            )

    def test_in_memory_state_global_flag_uses_default_global_store(self) -> None:
        previous_agent_dir = os.environ.get("PRIME_AGENT_CODING_AGENT_DIR")
        previous_global = os.environ.get("RLM_GLOBAL_HARNESS_STATE_DIR")
        with tempfile.TemporaryDirectory() as temp_dir:
            agent_dir = Path(temp_dir) / "agent"
            os.environ["PRIME_AGENT_CODING_AGENT_DIR"] = str(agent_dir)
            os.environ.pop("RLM_GLOBAL_HARNESS_STATE_DIR", None)
            try:
                state = HarnessState(in_memory=True)
                global_entry = state.create_memory("Default global", "persisted", id="default_global", global_=True)
            finally:
                if previous_agent_dir is None:
                    os.environ.pop("PRIME_AGENT_CODING_AGENT_DIR", None)
                else:
                    os.environ["PRIME_AGENT_CODING_AGENT_DIR"] = previous_agent_dir
                if previous_global is None:
                    os.environ.pop("RLM_GLOBAL_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = previous_global

            self.assertIsNone(state.file_path)
            self.assertEqual(global_entry.scope, "global")
            self.assertIsNone(state.get("memory", "default_global"))
            self.assertEqual(
                HarnessState(agent_dir / "harness" / "harness_state.json", scope="global")
                .get("memory", "default_global")
                .content,
                "persisted",
            )

    def test_reloads_external_writes_before_mutating(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state_path = Path(temp_dir) / "harness_state.json"
            kernel_state = HarnessState(state_path)
            kernel_state.create_memory("Kernel note", "Written from the kernel.", id="kernel")

            # Simulate the host /refine command rewriting the same file from another
            # process. A second instance loads the current file, adds an entry, saves.
            host_state = HarnessState(state_path)
            host_state.create_memory("Host note", "Written by /refine.", id="host")
            # Guarantee the mtime advances even on coarse-resolution filesystems.
            future = state_path.stat().st_mtime + 5
            os.utime(state_path, (future, future))

            # A read on the long-lived kernel state must observe the host write.
            self.assertEqual(kernel_state.get("memory", "host").content, "Written by /refine.")

            # A mutation must merge onto the host write instead of clobbering it.
            kernel_state.create_memory("Second kernel note", "Written later.", id="kernel_2")

            reloaded = HarnessState(state_path)
            self.assertIsNotNone(reloaded.get("memory", "kernel"))
            self.assertIsNotNone(reloaded.get("memory", "host"))
            self.assertIsNotNone(reloaded.get("memory", "kernel_2"))

    def test_create_detects_externally_written_entry(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state_path = Path(temp_dir) / "harness_state.json"
            state = HarnessState(state_path)

            # Another process creates the same entry on disk after our last load.
            other = HarnessState(state_path)
            other.create_memory("External", "Written elsewhere.", id="dup")
            future = state_path.stat().st_mtime + 5
            os.utime(state_path, (future, future))

            # create() must observe the external entry and honor create-or-fail.
            with self.assertRaisesRegex(ValueError, "already exists"):
                state.create_memory("Local", "Should not overwrite.", id="dup")
            self.assertEqual(state.get("memory", "dup").content, "Written elsewhere.")

    def test_explicit_create_and_update_enforce_entry_existence(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")

            first = state.create_skill("Triage", "old", id="triage", reference=PYTHON_REFERENCE, arguments={})
            with self.assertRaisesRegex(ValueError, "already exists"):
                state.create_skill("Triage", "duplicate", id="triage", reference=PYTHON_REFERENCE, arguments={})
            with self.assertRaisesRegex(ValueError, "does not exist"):
                state.update_skill("missing", "Missing", "missing", reference=PYTHON_REFERENCE, arguments={})

            second = state.update_skill("triage", "Triage", "new", reference=PYTHON_REFERENCE, arguments={})

            self.assertEqual(first.id, second.id)
            self.assertEqual(second.content, "new")
            self.assertEqual(second.version, 2)

    def test_explicit_state_dir_cache_uses_harness_state_file(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state = get_harness_state(temp_dir)
            again = get_harness_state(temp_dir)

            self.assertIs(state, again)
            self.assertEqual(state.file_path, Path(temp_dir).resolve() / "harness_state.json")

    def test_explicit_state_dir_global_flag_uses_matching_state_file(self) -> None:
        previous_global = os.environ.get("RLM_GLOBAL_HARNESS_STATE_DIR")
        with tempfile.TemporaryDirectory() as temp_dir:
            explicit_dir = Path(temp_dir) / "explicit"
            env_global_dir = Path(temp_dir) / "env-global"
            os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = str(env_global_dir)
            try:
                state = get_harness_state(explicit_dir)
                global_entry = state.create_memory("Scoped global", "custom dir", id="scoped_global", global_=True)
            finally:
                if previous_global is None:
                    os.environ.pop("RLM_GLOBAL_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = previous_global

            self.assertEqual(global_entry.scope, "global")
            self.assertIsNotNone(
                HarnessState(explicit_dir / "harness_state.json", scope="global").get("memory", "scoped_global")
            )
            self.assertFalse((env_global_dir / "harness_state.json").exists())

    def test_env_default_state_keeps_env_global_target_after_explicit_dir_cache_hit(self) -> None:
        previous_local = os.environ.get("RLM_HARNESS_STATE_DIR")
        previous_global = os.environ.get("RLM_GLOBAL_HARNESS_STATE_DIR")
        with tempfile.TemporaryDirectory() as temp_dir:
            local_dir = Path(temp_dir) / "local"
            env_global_dir = Path(temp_dir) / "env-global"
            os.environ["RLM_HARNESS_STATE_DIR"] = str(local_dir)
            os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = str(env_global_dir)
            try:
                cached_from_env = get_harness_state()
                # An explicit state_dir that aliases the env local dir must not
                # redirect the env-default singleton's global target.
                cached_from_explicit = get_harness_state(local_dir)
                global_entry = cached_from_env.create_memory(
                    "Env global",
                    "still targets the env global dir",
                    id="env_global_after_hit",
                    global_=True,
                )
            finally:
                if previous_local is None:
                    os.environ.pop("RLM_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_HARNESS_STATE_DIR"] = previous_local
                if previous_global is None:
                    os.environ.pop("RLM_GLOBAL_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = previous_global

            self.assertIs(cached_from_env, cached_from_explicit)
            self.assertEqual(global_entry.scope, "global")
            self.assertIsNotNone(
                HarnessState(env_global_dir / "harness_state.json", scope="global").get(
                    "memory", "env_global_after_hit"
                )
            )
            self.assertIsNone(
                HarnessState(local_dir / "harness_state.json").get("memory", "env_global_after_hit")
            )

    def test_local_state_requires_local_path(self) -> None:
        previous_local = os.environ.get("RLM_HARNESS_STATE_DIR")
        previous_session = os.environ.get("RLM_SESSION_DIR")
        try:
            os.environ.pop("RLM_HARNESS_STATE_DIR", None)
            os.environ.pop("RLM_SESSION_DIR", None)
            with self.assertRaisesRegex(RuntimeError, "Local harness state requires"):
                HarnessState()
        finally:
            if previous_local is None:
                os.environ.pop("RLM_HARNESS_STATE_DIR", None)
            else:
                os.environ["RLM_HARNESS_STATE_DIR"] = previous_local
            if previous_session is None:
                os.environ.pop("RLM_SESSION_DIR", None)
            else:
                os.environ["RLM_SESSION_DIR"] = previous_session

    def test_default_state_uses_global_harness_env_dir(self) -> None:
        previous = os.environ.get("RLM_HARNESS_STATE_DIR")
        with tempfile.TemporaryDirectory() as temp_dir:
            os.environ["RLM_HARNESS_STATE_DIR"] = temp_dir
            try:
                state = HarnessState()
            finally:
                if previous is None:
                    os.environ.pop("RLM_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_HARNESS_STATE_DIR"] = previous

            self.assertEqual(state.file_path, Path(temp_dir).resolve() / "harness_state.json")

    def test_global_scope_default_state_uses_global_harness_env_dir(self) -> None:
        previous_local = os.environ.get("RLM_HARNESS_STATE_DIR")
        previous_global = os.environ.get("RLM_GLOBAL_HARNESS_STATE_DIR")
        with tempfile.TemporaryDirectory() as temp_dir:
            local_dir = Path(temp_dir) / "local"
            global_dir = Path(temp_dir) / "global"
            os.environ["RLM_HARNESS_STATE_DIR"] = str(local_dir)
            os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = str(global_dir)
            try:
                state = HarnessState(scope="global")
            finally:
                if previous_local is None:
                    os.environ.pop("RLM_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_HARNESS_STATE_DIR"] = previous_local
                if previous_global is None:
                    os.environ.pop("RLM_GLOBAL_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = previous_global

            self.assertEqual(state.scope, "global")
            self.assertEqual(state.file_path, global_dir.resolve() / "harness_state.json")

    def test_default_state_is_local_and_global_flag_targets_global_store(self) -> None:
        previous_local = os.environ.get("RLM_HARNESS_STATE_DIR")
        previous_global = os.environ.get("RLM_GLOBAL_HARNESS_STATE_DIR")
        with tempfile.TemporaryDirectory() as temp_dir:
            local_dir = Path(temp_dir) / "local"
            global_dir = Path(temp_dir) / "global"
            os.environ["RLM_HARNESS_STATE_DIR"] = str(local_dir)
            os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = str(global_dir)
            try:
                state = get_harness_state()
                global_state = get_harness_state(global_=True)
                local_entry = state.create_memory("Local note", "Only this session.", id="local_note")
                global_entry = state.create_memory("Global note", "All sessions.", id="global_note", global_=True)
                kwargs_entry = state.create_memory(
                    "Kwargs global note",
                    "All sessions via kwargs.",
                    id="kwargs_global_note",
                    **{"global": True},
                )
            finally:
                if previous_local is None:
                    os.environ.pop("RLM_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_HARNESS_STATE_DIR"] = previous_local
                if previous_global is None:
                    os.environ.pop("RLM_GLOBAL_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = previous_global

            self.assertEqual(state.file_path, local_dir.resolve() / "harness_state.json")
            self.assertEqual(global_state.file_path, global_dir.resolve() / "harness_state.json")
            self.assertEqual(local_entry.scope, "local")
            self.assertEqual(global_entry.scope, "global")
            self.assertEqual(kwargs_entry.scope, "global")
            self.assertIsNotNone(HarnessState(local_dir / "harness_state.json").get("memory", "local_note"))
            self.assertIsNone(HarnessState(local_dir / "harness_state.json").get("memory", "global_note"))
            self.assertIsNotNone(HarnessState(global_dir / "harness_state.json", scope="global").get("memory", "global_note"))
            self.assertIsNotNone(
                HarnessState(global_dir / "harness_state.json", scope="global").get("memory", "kwargs_global_note")
            )

    def test_global_kwarg_must_be_boolean(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")

            with self.assertRaisesRegex(TypeError, "global must be a bool"):
                state.create_memory("Bad global flag", "bad", id="bad_global", **{"global": "false"})

    def test_state_cache_keeps_scope_distinct_when_local_and_global_share_a_file(self) -> None:
        previous_local = os.environ.get("RLM_HARNESS_STATE_DIR")
        previous_global = os.environ.get("RLM_GLOBAL_HARNESS_STATE_DIR")
        with tempfile.TemporaryDirectory() as temp_dir:
            os.environ["RLM_HARNESS_STATE_DIR"] = temp_dir
            os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = temp_dir
            try:
                state = get_harness_state()
                global_state = get_harness_state(global_=True)
                local_entry = state.create_memory("Local note", "Only this session.", id="local_note")
                global_entry = state.create_memory("Global note", "All sessions.", id="global_note", global_=True)
            finally:
                if previous_local is None:
                    os.environ.pop("RLM_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_HARNESS_STATE_DIR"] = previous_local
                if previous_global is None:
                    os.environ.pop("RLM_GLOBAL_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = previous_global

            self.assertIsNot(state, global_state)
            self.assertEqual(state.file_path, global_state.file_path)
            self.assertEqual(state.scope, "local")
            self.assertEqual(global_state.scope, "global")
            self.assertEqual(local_entry.scope, "local")
            self.assertEqual(global_entry.scope, "global")
            reloaded = HarnessState(Path(temp_dir) / "harness_state.json")
            self.assertEqual(reloaded.get("memory", "local_note").scope, "local")
            self.assertEqual(reloaded.get("memory", "global_note").scope, "global")

    def test_scope_prefixed_ids_route_to_the_displayed_scope(self) -> None:
        previous_local = os.environ.get("RLM_HARNESS_STATE_DIR")
        previous_global = os.environ.get("RLM_GLOBAL_HARNESS_STATE_DIR")
        with tempfile.TemporaryDirectory() as temp_dir:
            local_dir = Path(temp_dir) / "local"
            global_dir = Path(temp_dir) / "global"
            os.environ["RLM_HARNESS_STATE_DIR"] = str(local_dir)
            os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = str(global_dir)
            try:
                state = get_harness_state()
                state.create_memory("Global note", "v1", id="routed", global_=True)

                # The overview displays [global:routed]; that id must be usable as-is
                # and imply the global scope without passing global_.
                updated = state.update_memory("global:routed", "Global note", "v2")
                self.assertEqual(updated.scope, "global")
                self.assertEqual(state.get("memory", "global:routed").content, "v2")
                self.assertIsNone(state.get("memory", "routed"))

                state.create_memory("Local note", "local", id="local_note")
                self.assertEqual(state.get("memory", "local:local_note").content, "local")
                self.assertTrue(state.delete_memory("local:local_note"))
                self.assertIsNone(state.get("memory", "local_note"))
            finally:
                if previous_local is None:
                    os.environ.pop("RLM_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_HARNESS_STATE_DIR"] = previous_local
                if previous_global is None:
                    os.environ.pop("RLM_GLOBAL_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = previous_global

            self.assertEqual(
                HarnessState(global_dir / "harness_state.json", scope="global").get("memory", "routed").content,
                "v2",
            )

    def test_create_with_prefixed_id_does_not_mint_literal_id(self) -> None:
        previous_local = os.environ.get("RLM_HARNESS_STATE_DIR")
        previous_global = os.environ.get("RLM_GLOBAL_HARNESS_STATE_DIR")
        with tempfile.TemporaryDirectory() as temp_dir:
            local_dir = Path(temp_dir) / "local"
            global_dir = Path(temp_dir) / "global"
            os.environ["RLM_HARNESS_STATE_DIR"] = str(local_dir)
            os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = str(global_dir)
            try:
                state = get_harness_state()
                entry = state.create_memory("Validation", "content", id="global:validation")
            finally:
                if previous_local is None:
                    os.environ.pop("RLM_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_HARNESS_STATE_DIR"] = previous_local
                if previous_global is None:
                    os.environ.pop("RLM_GLOBAL_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = previous_global

            self.assertEqual(entry.id, "validation")
            self.assertEqual(entry.scope, "global")
            global_store = HarnessState(global_dir / "harness_state.json", scope="global")
            self.assertIsNotNone(global_store.get("memory", "validation"))
            self.assertIsNone(global_store.get("memory", "global:validation"))
            self.assertFalse((local_dir / "harness_state.json").exists())

    def test_module_harness_binds_lazily_to_env_set_after_import(self) -> None:
        # Forkserver scenario: rlm is imported in the template process without the
        # per-session env; the child applies env after fork. rlm.harness must then
        # resolve against the new env instead of a store frozen at import time.
        previous_local = os.environ.get("RLM_HARNESS_STATE_DIR")
        previous_session = os.environ.get("RLM_SESSION_DIR")
        with tempfile.TemporaryDirectory() as temp_dir:
            try:
                os.environ.pop("RLM_HARNESS_STATE_DIR", None)
                os.environ.pop("RLM_SESSION_DIR", None)
                # Without local env, local writes fail loudly instead of vanishing.
                with self.assertRaisesRegex(RuntimeError, "global_=True"):
                    package_harness.create_memory("Volatile", "pre-env", id="pre_env")

                os.environ["RLM_HARNESS_STATE_DIR"] = temp_dir
                entry = package_harness.create_memory("Session note", "persisted", id="session_note")
                self.assertIsNone(package_harness.get("memory", "pre_env"))
            finally:
                if previous_local is None:
                    os.environ.pop("RLM_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_HARNESS_STATE_DIR"] = previous_local
                if previous_session is None:
                    os.environ.pop("RLM_SESSION_DIR", None)
                else:
                    os.environ["RLM_SESSION_DIR"] = previous_session

            self.assertEqual(entry.scope, "local")
            reloaded = HarnessState(Path(temp_dir) / "harness_state.json")
            self.assertEqual(reloaded.get("memory", "session_note").content, "persisted")

    def test_module_harness_without_env_raises_on_local_writes_and_reads_work(self) -> None:
        previous_local = os.environ.get("RLM_HARNESS_STATE_DIR")
        previous_session = os.environ.get("RLM_SESSION_DIR")
        try:
            os.environ.pop("RLM_HARNESS_STATE_DIR", None)
            os.environ.pop("RLM_SESSION_DIR", None)

            for mutate in (
                lambda: package_harness.create_memory("Lost", "content", id="lost"),
                lambda: package_harness.update_memory("lost", "Lost", "content"),
                lambda: package_harness.delete_memory("lost"),
                lambda: package_harness.upsert("memory", "Lost", "content", id="lost"),
                lambda: package_harness.record_refinement("trigger", ["change"]),
            ):
                with self.assertRaisesRegex(RuntimeError, "Local harness state requires.*global_=True"):
                    mutate()

            # Reads keep working against an empty view.
            self.assertIsNone(package_harness.get("memory", "lost"))
            self.assertEqual(package_harness.list(), [])
            self.assertIn("memory: 0", package_harness.overview())
            self.assertEqual(package_harness.snapshot()["refinements"], [])
        finally:
            if previous_local is None:
                os.environ.pop("RLM_HARNESS_STATE_DIR", None)
            else:
                os.environ["RLM_HARNESS_STATE_DIR"] = previous_local
            if previous_session is None:
                os.environ.pop("RLM_SESSION_DIR", None)
            else:
                os.environ["RLM_SESSION_DIR"] = previous_session

    def test_module_harness_without_env_still_routes_global_writes(self) -> None:
        previous_local = os.environ.get("RLM_HARNESS_STATE_DIR")
        previous_session = os.environ.get("RLM_SESSION_DIR")
        previous_global = os.environ.get("RLM_GLOBAL_HARNESS_STATE_DIR")
        with tempfile.TemporaryDirectory() as temp_dir:
            global_dir = Path(temp_dir) / "global"
            try:
                os.environ.pop("RLM_HARNESS_STATE_DIR", None)
                os.environ.pop("RLM_SESSION_DIR", None)
                os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = str(global_dir)
                entry = package_harness.create_memory("Lesson", "keep me", id="no_session_lesson", global_=True)
            finally:
                if previous_local is None:
                    os.environ.pop("RLM_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_HARNESS_STATE_DIR"] = previous_local
                if previous_session is None:
                    os.environ.pop("RLM_SESSION_DIR", None)
                else:
                    os.environ["RLM_SESSION_DIR"] = previous_session
                if previous_global is None:
                    os.environ.pop("RLM_GLOBAL_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = previous_global

            self.assertEqual(entry.scope, "global")
            self.assertEqual(
                HarnessState(global_dir / "harness_state.json", scope="global").get("memory", "no_session_lesson").content,
                "keep me",
            )

    def test_import_rlm_without_env_does_not_raise(self) -> None:
        env = dict(os.environ)
        env.pop("RLM_HARNESS_STATE_DIR", None)
        env.pop("RLM_SESSION_DIR", None)
        env["PYTHONPATH"] = str(Path(__file__).resolve().parents[1] / "src")
        result = subprocess.run(
            [sys.executable, "-c", "import rlm; repr(rlm.harness); rlm.harness.overview(); rlm.harness.create_memory"],
            env=env,
            capture_output=True,
            text=True,
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_empty_local_state_dir_env_is_treated_as_unset(self) -> None:
        previous_local = os.environ.get("RLM_HARNESS_STATE_DIR")
        previous_session = os.environ.get("RLM_SESSION_DIR")
        with tempfile.TemporaryDirectory() as temp_dir:
            try:
                # Empty local dir must not fall through to the global agent-dir default.
                os.environ["RLM_HARNESS_STATE_DIR"] = ""
                os.environ.pop("RLM_SESSION_DIR", None)
                with self.assertRaisesRegex(RuntimeError, "Local harness state requires"):
                    HarnessState()

                # With a session dir it takes the session fallback instead.
                os.environ["RLM_SESSION_DIR"] = temp_dir
                state = HarnessState()
                self.assertEqual(state.file_path, Path(temp_dir).resolve() / "harness" / "harness_state.json")

                # A whitespace-only session dir is also unset.
                os.environ["RLM_SESSION_DIR"] = "   "
                with self.assertRaisesRegex(RuntimeError, "Local harness state requires"):
                    HarnessState()
            finally:
                if previous_local is None:
                    os.environ.pop("RLM_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_HARNESS_STATE_DIR"] = previous_local
                if previous_session is None:
                    os.environ.pop("RLM_SESSION_DIR", None)
                else:
                    os.environ["RLM_SESSION_DIR"] = previous_session

    def test_explicit_dir_aliasing_env_local_dir_keeps_env_global_target(self) -> None:
        previous_local = os.environ.get("RLM_HARNESS_STATE_DIR")
        previous_global = os.environ.get("RLM_GLOBAL_HARNESS_STATE_DIR")
        with tempfile.TemporaryDirectory() as temp_dir:
            local_dir = Path(temp_dir) / "local"
            env_global_dir = Path(temp_dir) / "env-global"
            os.environ["RLM_HARNESS_STATE_DIR"] = str(local_dir)
            os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = str(env_global_dir)
            try:
                # First construction happens via an explicit dir that merely aliases
                # the env local dir; global writes must still hit the env global dir.
                state = get_harness_state(local_dir)
                global_entry = state.create_memory("Aliased", "still global", id="alias_global", global_=True)
            finally:
                if previous_local is None:
                    os.environ.pop("RLM_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_HARNESS_STATE_DIR"] = previous_local
                if previous_global is None:
                    os.environ.pop("RLM_GLOBAL_HARNESS_STATE_DIR", None)
                else:
                    os.environ["RLM_GLOBAL_HARNESS_STATE_DIR"] = previous_global

            self.assertEqual(global_entry.scope, "global")
            self.assertIsNotNone(
                HarnessState(env_global_dir / "harness_state.json", scope="global").get("memory", "alias_global")
            )
            self.assertIsNone(
                HarnessState(local_dir / "harness_state.json").get("memory", "alias_global")
            )

    def test_callable_rlm_exposes_harness_state_helpers(self) -> None:
        self.assertIs(callable_rlm.harness, package_harness)
        self.assertIs(callable_rlm.get_harness_state, get_harness_state)

    def test_record_refinement_accepts_single_change_string(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")

            event = state.record_refinement("manual cli test", "single change")

            self.assertEqual(event.changes, ["single change"])
            self.assertEqual(state.refinements[0].changes, ["single change"])

    def test_unknown_kind_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            state = HarnessState(Path(temp_dir) / "harness_state.json")

            with self.assertRaisesRegex(ValueError, "unknown harness kind"):
                state.upsert("tool", "Tool", "Tool content")
            with self.assertRaisesRegex(ValueError, "unknown harness kind"):
                state.get("tool", "tool")
            with self.assertRaisesRegex(ValueError, "unknown harness kind"):
                state.delete("tool", "tool")
            with self.assertRaisesRegex(ValueError, "unknown harness kind"):
                state.list("tool")


def _host_entry(id: str, **overrides: object) -> dict[str, object]:
    """An entry record shaped exactly like the TypeScript host writes it."""
    record: dict[str, object] = {
        "id": id,
        "kind": "memory",
        "title": f"Title {id}",
        "content": f"Content {id}",
        "path": "general",
        "scope": "local",
        "reference": {},
        "arguments": {},
        "metadata": {},
        "source": "refine",
        "created_at": "2026-01-01T00:00:00+00:00",
        "updated_at": "2026-01-01T00:00:00+00:00",
        "version": 1,
    }
    record.update(overrides)
    return record


def _host_state(memory: dict[str, object] | None = None) -> dict[str, object]:
    """A full state file as written by the host, including host-owned top-level keys."""
    return {
        "schema": 1,
        "entries": {"prompt": {}, "memory": dict(memory or {}), "skill": {}, "subagent": {}},
        "refinements": [
            {
                "id": "refine_0001",
                "trigger": "host",
                "changes": ["update memory:keep"],
                "evidence": "",
                "outcome": "",
                "created_at": "2026-01-01T00:00:00+00:00",
                "hostOnlyEventKey": {"nested": [1, 2, 3]},
            }
        ],
        "ravo": {
            "lineage": [{"proposalId": "p1", "untilTurn": 7, "observedRecurrence": False}],
            "opponents": {"weights": {"argmax": 0.25, "sampled": 0.75}},
            "unicode": "caf\u00e9 \u2603",
        },
        "failures": {"records": [{"fingerprint": "f1", "count": 2, "lastTurn": 5}]},
        "trustWindows": {"p1": {"proposalId": "p1", "touched": ["memory:keep", "memory:dormant"]}},
        "futureTopLevelKey": [1, {"z": None, "f": 1.5}, "text"],
    }


def _write_host_file(path: Path, document: dict[str, object]) -> bytes:
    """Write `document` exactly as the host does: JSON.stringify(state, null, 2) + newline."""
    text = json.dumps(document, indent=2, ensure_ascii=False) + "\n"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")
    return text.encode("utf-8")


class HarnessStateHostInteropTest(unittest.TestCase):
    """The kernel must never drop what the TypeScript host persists in harness_state.json."""

    def test_noop_save_is_byte_identical_to_host_written_file(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "harness_state.json"
            before = _write_host_file(
                path,
                _host_state(
                    {
                        "keep": _host_entry("keep", trust=80, hostOnlyEntryKey={"x": [1, 2]}),
                        "dormant": _host_entry("dormant", trust=10),
                        "plain": _host_entry("plain"),
                    }
                ),
            )

            HarnessState(path).save()

            self.assertEqual(path.read_bytes(), before)
            self.assertEqual(sorted(os.listdir(temp_dir)), ["harness_state.json"], "no temp file left behind")

    def test_crud_save_preserves_ravo_failures_trust_windows_and_future_keys(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "harness_state.json"
            document = _host_state({"keep": _host_entry("keep", trust=80)})
            _write_host_file(path, document)
            state = HarnessState(path)

            state.create_memory("Kernel memory", "written by the kernel", id="kernel")
            state.update_memory("keep", "Keep", "edited by the kernel")
            state.delete_memory("kernel")
            state.record_refinement("kernel", "update memory:keep")

            after = json.loads(path.read_text(encoding="utf-8"))
            for key in ("ravo", "failures", "trustWindows", "futureTopLevelKey"):
                self.assertEqual(
                    json.dumps(after[key], ensure_ascii=False),
                    json.dumps(document[key], ensure_ascii=False),
                    f"top-level {key!r} must round-trip byte-equal",
                )
            self.assertEqual(
                list(after),
                ["schema", "entries", "refinements", "ravo", "failures", "trustWindows", "futureTopLevelKey"],
            )
            self.assertEqual(after["entries"]["memory"]["keep"]["content"], "edited by the kernel")
            self.assertEqual(after["entries"]["memory"]["keep"]["trust"], 80)
            self.assertEqual(after["refinements"][0]["hostOnlyEventKey"], {"nested": [1, 2, 3]})
            self.assertEqual(after["refinements"][1]["trigger"], "kernel")

            # A fresh load of the kernel-written file still sees everything.
            reloaded = HarnessState(path)
            self.assertEqual(reloaded.extra["ravo"], document["ravo"])
            self.assertEqual(reloaded.snapshot()["trustWindows"], document["trustWindows"])
            self.assertEqual(reloaded.snapshot()["failures"], document["failures"])

    def test_host_rewrite_between_kernel_saves_is_not_clobbered(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "harness_state.json"
            _write_host_file(path, _host_state({"keep": _host_entry("keep")}))
            state = HarnessState(path)
            state.create_memory("First", "first", id="first")

            # The host settles a trust window out of process (new mtime, new content).
            document = json.loads(path.read_text(encoding="utf-8"))
            document["trustWindows"]["p1"]["outcome"] = "failure"
            document["entries"]["memory"]["keep"]["trust"] = 35
            document["ravo"]["lineage"][0]["observedRecurrence"] = True
            os.utime(path, ns=(path.stat().st_atime_ns, path.stat().st_mtime_ns + 5_000_000))
            _write_host_file(path, document)
            os.utime(path, ns=(path.stat().st_atime_ns, path.stat().st_mtime_ns + 5_000_000))

            state.create_memory("Second", "second", id="second")

            after = json.loads(path.read_text(encoding="utf-8"))
            self.assertEqual(after["trustWindows"]["p1"]["outcome"], "failure")
            self.assertTrue(after["ravo"]["lineage"][0]["observedRecurrence"])
            self.assertEqual(after["entries"]["memory"]["keep"]["trust"], 35)
            self.assertIn("second", after["entries"]["memory"])

    def test_schema_number_is_preserved(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "harness_state.json"
            document = _host_state()
            document["schema"] = 3
            _write_host_file(path, document)

            HarnessState(path).create_memory("M", "m", id="m")

            self.assertEqual(json.loads(path.read_text(encoding="utf-8"))["schema"], 3)


class HarnessTrustTest(unittest.TestCase):
    def test_trust_is_loaded_and_preserved_and_absent_stays_absent(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "harness_state.json"
            _write_host_file(
                path,
                _host_state(
                    {
                        "trusted": _host_entry("trusted", trust=80),
                        "zero": _host_entry("zero", trust=0),
                        "hundred": _host_entry("hundred", trust=100),
                        "absent": _host_entry("absent"),
                        "too_high": _host_entry("too_high", trust=250),
                        "negative": _host_entry("negative", trust=-1),
                        "fraction": _host_entry("fraction", trust=42.5),
                        "whole_float": _host_entry("whole_float", trust=42.0),
                        "boolean": _host_entry("boolean", trust=True),
                        "text": _host_entry("text", trust="50"),
                        "null": _host_entry("null", trust=None),
                    }
                ),
            )
            state = HarnessState(path)

            self.assertEqual(state.get("memory", "trusted").trust, 80)
            self.assertEqual(state.get("memory", "zero").trust, 0)
            self.assertEqual(state.get("memory", "hundred").trust, 100)
            self.assertIsNone(state.get("memory", "absent").trust)
            # Mirrors harness-trust.ts normalizeTrust: anything not an integer in [0, 100] is absent.
            for id in ("too_high", "negative", "fraction", "boolean", "text", "null"):
                self.assertIsNone(state.get("memory", id).trust, id)
            self.assertEqual(state.get("memory", "whole_float").trust, 42)
            self.assertEqual(trust_of(state.get("memory", "absent")), 50)
            self.assertEqual(trust_of(state.get("memory", "too_high")), 50)

            state.save()
            written = json.loads(path.read_text(encoding="utf-8"))["entries"]["memory"]
            self.assertEqual(written["trusted"]["trust"], 80)
            self.assertEqual(written["zero"]["trust"], 0)
            self.assertEqual(written["whole_float"]["trust"], 42)
            for id in ("absent", "too_high", "negative", "fraction", "boolean", "text", "null"):
                self.assertNotIn("trust", written[id], f"{id}: absent/invalid trust must not be written, not even null")

    def test_update_revives_dormant_entry_and_keeps_active_trust(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "harness_state.json"
            _write_host_file(
                path,
                _host_state(
                    {
                        "dormant": _host_entry("dormant", trust=10),
                        "edge_dormant": _host_entry("edge_dormant", trust=29),
                        "edge_active": _host_entry("edge_active", trust=30),
                        "trusted": _host_entry("trusted", trust=80),
                        "absent": _host_entry("absent"),
                    }
                ),
            )
            state = HarnessState(path)
            self.assertEqual([entry.id for entry in state.dormant()], ["dormant", "edge_dormant"])

            self.assertEqual(state.update_memory("dormant", "Dormant", "revived").trust, 50)
            self.assertEqual(state.update_memory("edge_dormant", "Edge", "revived").trust, 50)
            self.assertEqual(state.update_memory("edge_active", "Edge", "kept").trust, 30)
            self.assertEqual(state.update_memory("trusted", "Trusted", "kept").trust, 80)
            self.assertIsNone(state.update_memory("absent", "Absent", "kept").trust)
            # upsert() of an existing entry is an update too.
            state.get("memory", "trusted").trust = 5
            self.assertEqual(state.upsert("memory", "Trusted", "revived via upsert", id="trusted").trust, 50)

            written = json.loads(path.read_text(encoding="utf-8"))["entries"]["memory"]
            self.assertEqual(written["dormant"]["trust"], 50)
            self.assertEqual(written["edge_dormant"]["trust"], 50)
            self.assertEqual(written["edge_active"]["trust"], 30)
            self.assertEqual(written["trusted"]["trust"], 50)
            self.assertNotIn("trust", written["absent"])
            self.assertEqual(state.dormant(), [])

    def test_create_records_no_trust(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "harness_state.json"
            state = HarnessState(path)

            created = {
                "prompt": state.create_prompt_note("P", "p", id="p"),
                "memory": state.create_memory("M", "m", id="m"),
                "skill": state.create_skill("S", "s", id="s", reference=PYTHON_REFERENCE),
                "subagent": state.create_subagent("A", "a", id="a"),
                "upsert": state.upsert("memory", "U", "u", id="u"),
            }

            for name, entry in created.items():
                self.assertIsNone(entry.trust, name)
                self.assertEqual(trust_of(entry), 50, name)
            written = json.loads(path.read_text(encoding="utf-8"))["entries"]
            for kind, id in (("prompt", "p"), ("memory", "m"), ("skill", "s"), ("subagent", "a"), ("memory", "u")):
                self.assertNotIn("trust", written[kind][id], f"{kind}:{id}")

    def test_overview_hides_dormant_entries_and_names_them_in_footer(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "harness_state.json"
            document = _host_state(
                {
                    "keep": _host_entry("keep", trust=80),
                    "sleepy": _host_entry("sleepy", trust=10, title="Sleepy secret"),
                    "plain": _host_entry("plain"),
                }
            )
            document["entries"]["prompt"]["quiet"] = dict(
                _host_entry("quiet", kind="prompt", trust=0, title="Quiet secret"), scope="global"
            )
            _write_host_file(path, document)
            state = HarnessState(path)

            overview = state.overview()

            self.assertIn("memory: 2\n", overview)
            self.assertIn("prompt: 0\n", overview)
            self.assertIn("[local:keep] Title keep (general, v1 trust=80): Content keep", overview)
            self.assertIn("[local:plain] Title plain (general, v1): Content plain", overview)
            self.assertNotIn("Sleepy secret", overview)
            self.assertNotIn("Quiet secret", overview)
            self.assertNotIn("sleepy]", overview)
            self.assertIn(
                "2 dormant entries (trust < 30, not shown; an explicit update revives one): "
                "prompt:global:quiet, memory:local:sleepy",
                overview,
            )
            # The footer sits between the entry listing and the refinement summary.
            self.assertLess(overview.index("dormant entries"), overview.index("refinements: 1"))
            # Dormant entries stay visible to CRUD.
            self.assertIsNotNone(state.get("memory", "sleepy"))
            self.assertIn("sleepy", [entry.id for entry in state.list("memory")])

            state.update_memory("sleepy", "Sleepy secret", "revived")
            state.delete("prompt", "quiet")
            self.assertNotIn("dormant", state.overview())
            self.assertIn("[local:sleepy] Sleepy secret (general, v2 trust=50): revived", state.overview())

    def test_single_dormant_entry_footer_uses_singular_noun(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "harness_state.json"
            _write_host_file(path, _host_state({"only": _host_entry("only", trust=29)}))

            self.assertIn(
                "1 dormant entry (trust < 30, not shown; an explicit update revives one): memory:local:only",
                HarnessState(path).overview(),
            )

    def test_normalize_trust_matches_host_rules(self) -> None:
        normalize = normalize_trust
        self.assertEqual(normalize(0), 0)
        self.assertEqual(normalize(100), 100)
        self.assertEqual(normalize(50.0), 50)
        for invalid in (-1, 101, 49.5, True, False, "50", None, [50], {"trust": 50}):
            self.assertIsNone(normalize(invalid), repr(invalid))


class HarnessStateUnmodeledDataTest(unittest.TestCase):
    """Records and keys the kernel does not model must survive a kernel save verbatim."""

    def test_unknown_entry_keys_and_unmodeled_entries_survive_save(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "harness_state.json"
            malformed = {"id": "no_title", "content": "the host still owns this record", "trust": 40}
            _write_host_file(
                path,
                _host_state(
                    {
                        "first": _host_entry("first", hostOnlyEntryKey={"x": [1, 2]}),
                        "no_title": malformed,
                        "last": _host_entry("last"),
                    }
                ),
            )
            state = HarnessState(path)
            self.assertIsNone(state.get("memory", "no_title"))
            self.assertEqual(state.get("memory", "first").extra, {"hostOnlyEntryKey": {"x": [1, 2]}})

            state.update_memory("first", "First", "edited")
            state.create_memory("Appended", "new", id="appended")

            written = json.loads(path.read_text(encoding="utf-8"))["entries"]["memory"]
            self.assertEqual(list(written), ["first", "no_title", "last", "appended"], "on-disk order kept")
            self.assertEqual(written["no_title"], malformed)
            self.assertEqual(written["first"]["hostOnlyEntryKey"], {"x": [1, 2]})
            self.assertEqual(written["first"]["content"], "edited")
            self.assertNotIn("extra", written["first"])

            # Explicit CRUD on an unmodeled record: delete removes it, create replaces it.
            self.assertTrue(state.delete_memory("no_title"))
            self.assertNotIn("no_title", json.loads(path.read_text(encoding="utf-8"))["entries"]["memory"])
            self.assertFalse(state.delete_memory("no_title"))
            _write_host_file(path, _host_state({"no_title": malformed}))
            state = HarnessState(path)
            with self.assertRaisesRegex(ValueError, "does not exist"):
                state.update_memory("no_title", "T", "c")
            state.create_memory("Now modeled", "c", id="no_title")
            self.assertEqual(
                json.loads(path.read_text(encoding="utf-8"))["entries"]["memory"]["no_title"]["title"], "Now modeled"
            )

    def test_snapshot_serializes_entries_like_the_state_file(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            path = Path(temp_dir) / "harness_state.json"
            _write_host_file(path, _host_state({"keep": _host_entry("keep", trust=80, hostOnlyEntryKey=1)}))

            snapshot = HarnessState(path).snapshot()

            self.assertEqual(snapshot["entries"]["memory"]["keep"]["trust"], 80)
            self.assertEqual(snapshot["entries"]["memory"]["keep"]["hostOnlyEntryKey"], 1)
            self.assertNotIn("extra", snapshot["entries"]["memory"]["keep"])
            self.assertEqual(snapshot["schema"], 1)
            self.assertEqual(snapshot["ravo"]["opponents"]["weights"]["sampled"], 0.75)
            self.assertEqual(snapshot["scope"], "local")


if __name__ == "__main__":
    unittest.main()
