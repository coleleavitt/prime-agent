//! The refine planner: proposal parsing, edit validation, application, and rollback.

use serde::{Deserialize, Serialize};

use super::{
    AppliedRefinementEdit,
    HarnessEntry,
    HarnessRefinementEvent,
    HarnessScope,
    HarnessState,
    RefinementAction,
    RefinementKind,
};

pub const REFINEMENT_SYSTEM_PROMPT: &str = "You are Prime Agent's /refine continual harness subsystem.\n\nYour job is to improve the editable continual harness state from the current trajectory.\nThis is similar in spirit to context compaction, but instead of summarizing the\nconversation you emit precise Create, Update, or Delete edits to reusable state.\nThe continual harness is the persistent, editable set of prompt notes, memories,\nskills, and subagent specs that lets Prime Agent improve reusable behavior\noutside the token history.\nUse \"continual harness\" for that persistent artifact layer; keep \"RLM\" for the\nruntime, Python REPL kernel, and native call interface that executes those artifacts.\n\nContinual harness components:\n- prompt: supplemental prompt notes only. The base system prompt is immutable and MUST NOT be rewritten.\n- memory: durable facts, decisions, failures, preferences, and outcomes.\n- skill: installed Python REPL skill. Skill create/update edits MUST include a `reference` object with `{\"type\":\"python\"}`, a Python import, and a callable or call pattern; they also MUST include an `arguments` object describing accepted inputs, required fields, defaults, and constraints. Use `{}` for `arguments` only when the Python callable truly needs no external inputs. Include the RLM-native call form `await <skill_import>(...)`.\n- subagent: reusable delegation specs, including purpose, instructions, and when to invoke. Include the RLM-native call form: compose a concise task prompt and spawn with `handle = await rlm.spawn(\"sub-task\", name=\"worker\")`; admission returns immediately with `rlm_child_id`, `name`, `session_dir`, and `model`, never the child's answer. Results arrive only through explicit `agent_message` replies or files; children reply with `await agent_message.send(message, receiver_role=\"parent\")`. Use `await rlm.list_subagents()` to recover direct child handles and `await agent_message.send(..., receiver_role=\"child\", receiver_name=handle.name)` for follow-ups. Do not invent wrappers like `run_subagent(...)`.\n- factory: declarative state-machine workflow specs of subagent states. The spec lives in `arguments.machine` (the original DAG sugar in `arguments.dag` compiles to machine form; pass exactly one form). The kernel validator (`rlm.factory`) enforces the full machine semantics at write time: run a stored factory with `await rlm.factory.run('<id>')`, watch with `await rlm.factory.status(run_id)`, stop with `await rlm.factory.stop(run_id)`, and resume an escalate-paused run with `await rlm.factory.resume(run_id)`.\n\nScope and persistence policy:\n- The default editable continual harness store is local to the current Prime Agent session. Use it for session-specific progress, active task state, current-run coordination notes, temporary blockers, and project facts that should not affect other sessions.\n- A caller may explicitly request global refinement. Global edits must be stable cross-session lessons, durable user preferences, reusable skills/subagents, or tool/environment facts that should affect future sessions.\n- Entry ids in the harness overview may carry a display-only `local:`, `global:`, or `package:` prefix. Always use the bare id (no prefix) in edits.\n- Package entries (`package:` prefix) are read-only runtime overlays mounted from installed Prime Agent packages. Never propose update or delete edits for them. A create edit with the same kind and bare id is allowed when an editable local or global override is genuinely justified.\n- All edits in one refinement apply only to the requested scope's store. During a local refinement, global entries are read-only context: never propose update or delete edits for them; create a local entry instead when a session-specific override is genuinely needed.\n- Project/workspace-specific lessons may be persisted globally only when the title, path, or content explicitly names the project/workspace and the lesson is likely to be reused in future sessions for that project. Prefer local edits when the lesson only belongs in the current conversation.\n- Use memory for declarative facts and preferences, skill for repeatable procedures exposed as Python calls, prompt for narrow behavioral policy addendums, and subagent for reusable delegation roles.\n- Entries carry an `enabled` flag. A disabled entry stays stored but is hidden from the system prompt, so a disabled subagent spec is never available for delegation. Prefer an update edit with `\"enabled\": false` over delete when an entry may become useful again, and re-enable with `\"enabled\": true`. Entries marked `[disabled]` in the overview are inactive; do not recreate them under a new id.\n- Create or update the smallest relevant component: repeated delegation roles should become subagent specs, repeated procedures should become skills, durable facts/preferences should become memories, and narrow behavioral policies should become prompt addendums.\n- When an edit is persisted, include metadata such as `{\"scope\":\"local\"}` or `{\"scope\":\"global\"}` when that helps future review understand the intended blast radius.\n\nEditing model:\n- An update replaces the entry: its content (and a skill's reference and arguments) become exactly what you send. Nothing you leave out survives.\n- The harness overview shows each entry's first 240 characters. An entry ending in `... (+N chars not shown)` is truncated: you have not seen all of it, so never update it (such updates are refused). Create a new, narrower entry instead.\n\nUse the trajectory, current continual harness state, and prior refinement history. Prefer\nsmall evidence-backed edits. If prior refinements caused issues, rollback or\nreplace the faulty editable entries. Never edit source files directly. Output\nJSON only with this exact shape:\n\n{\n  \"summary\": \"one sentence\",\n  \"rationale\": \"why these edits are justified by trajectory evidence\",\n  \"expectedOutcome\": \"what should improve and how to validate it\",\n  \"edits\": [\n    {\n      \"action\": \"create|update|delete\",\n      \"kind\": \"prompt|memory|skill|subagent|factory\",\n      \"id\": \"stable id for update/delete, optional for create\",\n      \"title\": \"required for create/update except delete\",\n      \"content\": \"required for create/update except delete\",\n      \"path\": \"optional grouping path\",\n      \"enabled\": \"optional boolean; false disables the entry without deleting it\",\n      \"reference\": {\"type\": \"python\", \"import\": \"package.module\", \"callable\": \"function_name\", \"call_pattern\": \"await function_name(...)\"},\n      \"arguments\": {\"name\": {\"type\": \"string\", \"required\": true, \"description\": \"accepted input\"}},\n      \"metadata\": {},\n      \"reason\": \"why this edit is useful\"\n    }\n  ]\n}";

pub const AUTO_REFINE_REVIEW_SYSTEM_PROMPT: &str = "You are Prime Agent's automatic /refine review gate.\n\nDecide whether this checkpoint should run /refine. Auto /refine writes local continual harness state by default, so approve when the trajectory contains evidence useful to this session's future turns.\nReject one-off noise, unsupported hypotheses, and transient tool outputs. Ask for global refinement only for durable cross-session lessons or explicitly project-qualified lessons likely to be reused in future sessions.\n\nReturn JSON only:\n{\n  \"shouldRefine\": true|false,\n  \"rationale\": \"short reason\",\n  \"instructions\": \"optional concise instructions for /refine if shouldRefine is true\"\n}";

/// Output caps (reasoning off shares the model's output budget with JSON).
pub const REFINEMENT_MAX_OUTPUT_TOKENS: u64 = 32_000;
pub const AUTO_REFINE_REVIEW_MAX_OUTPUT_TOKENS: u64 = 4_096;
pub const REFINEMENT_CONTEXT_OVERHEAD_TOKENS: u64 = 1_024;

pub const TRUNCATED_JSON_ERROR: &str = "the model stopped before completing its JSON object. This usually means the output budget was exhausted; retry with a smaller request.";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefinementEdit {
    pub action: Option<RefinementAction>,
    pub kind: Option<RefinementKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Map<String, serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// `false` disables the entry without deleting it, `true` re-enables
    /// it (#1118); absent keeps the entry's flag.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "lenient_enabled"
    )]
    pub enabled: Option<bool>,
}

/// A non-boolean `enabled` (a model echoing the schema's placeholder text)
/// reads as absent instead of discarding the whole edit.
fn lenient_enabled<'de, D>(deserializer: D) -> Result<Option<bool>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = <serde_json::Value as serde::Deserialize>::deserialize(deserializer)?;
    Ok(value.as_bool())
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RefinementProposal {
    pub summary: String,
    pub rationale: String,
    pub expected_outcome: String,
    pub edits: Vec<RefinementEdit>,
}

/// Whether a JSON candidate ends mid-value (unterminated string, unclosed
/// object/array): a truncated reply, as opposed to a malformed-but-balanced one.
#[must_use]
pub fn is_incomplete_json(candidate: &str) -> bool {
    let mut depth = 0i64;
    let mut in_string = false;
    let mut escaped = false;
    for char in candidate.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if in_string {
            if char == '\\' {
                escaped = true;
            } else if char == '"' {
                in_string = false;
            }
            continue;
        }
        match char {
            '"' => in_string = true,
            '{' | '[' => depth += 1,
            '}' | ']' => depth -= 1,
            _ => {}
        }
    }
    in_string || depth > 0
}

fn parse_json_candidate(candidate: &str) -> Result<serde_json::Value, String> {
    match serde_json::from_str::<serde_json::Value>(candidate) {
        Ok(value) => Ok(value),
        Err(error) => {
            if is_incomplete_json(candidate) {
                Err(TRUNCATED_JSON_ERROR.to_string())
            } else {
                Err(format!("the model did not return valid JSON: {error}"))
            }
        }
    }
}

/// Extract the proposal JSON from a reply: direct, fenced, or brace-sliced
/// out of prose (with truncation diagnosed against the original text).
///
/// # Errors
///
/// Human-readable error: no JSON object, an invalid candidate, or a truncated reply.
pub fn extract_json_object(text: &str) -> Result<serde_json::Value, String> {
    let trimmed = text.trim();
    if trimmed.starts_with('{') && trimmed.ends_with('}') {
        return parse_json_candidate(trimmed);
    }
    // Fenced block: ``` or ```json ... ```.
    if let Some(start) = trimmed.find("```") {
        let after_fence = &trimmed[start + 3..];
        let after_lang = after_fence.trim_start_matches("json").trim_start();
        if let Some(end) = after_lang.find("```") {
            return parse_json_candidate(after_lang[..end].trim());
        }
    }
    let start = trimmed.find('{');
    let end = trimmed.rfind('}');
    if let (Some(start), Some(end)) = (start, end) {
        if end > start {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&trimmed[start..=end]) {
                return Ok(value);
            }
            return parse_json_candidate(&trimmed[start..]);
        }
    }
    if is_incomplete_json(trimmed) {
        return Err(TRUNCATED_JSON_ERROR.to_string());
    }
    Err("Refiner did not return a JSON object".to_string())
}

/// Normalize an untrusted proposal, preserving invalid edit fields for
/// apply-time validation.
#[must_use]
pub fn normalize_refinement_proposal(value: &serde_json::Value) -> RefinementProposal {
    let record = value.as_object().cloned().unwrap_or_default();
    let string_field = |key: &str, fallback: &str| -> String {
        record
            .get(key)
            .and_then(|value| value.as_str())
            .unwrap_or(fallback)
            .to_string()
    };
    let edits = record
        .get("edits")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|edit| {
            edit.as_object()?;
            serde_json::from_value::<RefinementEdit>(edit.clone()).ok()
        })
        .collect();
    RefinementProposal {
        summary: string_field("summary", "Refined continual harness state"),
        rationale: string_field("rationale", ""),
        expected_outcome: string_field("expectedOutcome", ""),
        edits,
    }
}

/// Parse and normalize a refinement proposal from a model reply.
///
/// # Errors
///
/// Error when the JSON cannot be extracted or the top level is not an object.
pub fn parse_proposal(text: &str) -> Result<RefinementProposal, String> {
    let value = extract_json_object(text)?;
    if !value.is_object() {
        return Err("Refiner JSON must be an object".to_string());
    }
    Ok(normalize_refinement_proposal(&value))
}

fn slug(raw: &str, fallback: &str) -> String {
    let mut normalized = String::new();
    for char in raw.trim().to_lowercase().chars() {
        if char.is_ascii_lowercase() || char.is_ascii_digit() {
            normalized.push(char);
        } else if !normalized.ends_with('_') {
            normalized.push('_');
        }
    }
    let normalized = normalized.trim_matches('_').to_string();
    let truncated: String = normalized.chars().take(80).collect();
    if truncated.is_empty() {
        fallback.to_string()
    } else {
        truncated
    }
}

/// Validation errors mirror the TS messages exactly.
fn validate_edit(edit: &RefinementEdit, computed_id: Option<&str>) -> Option<String> {
    let Some(action) = edit.action else {
        return Some("unsupported action".to_string());
    };
    let Some(kind) = edit.kind else {
        return Some("unsupported kind".to_string());
    };
    match action {
        RefinementAction::Create | RefinementAction::Update | RefinementAction::Delete => {}
    }
    match kind {
        RefinementKind::Prompt
        | RefinementKind::Memory
        | RefinementKind::Skill
        | RefinementKind::Subagent
        | RefinementKind::Factory => {}
    }
    if kind == RefinementKind::Prompt
        && (edit.id.as_deref() == Some("base_system_prompt")
            || computed_id == Some("base_system_prompt"))
    {
        return Some("base system prompt is not editable".to_string());
    }
    if action != RefinementAction::Create && edit.id.is_none() {
        return Some(format!("{action:?} requires id").to_lowercase());
    }
    if action != RefinementAction::Delete && (edit.title.is_none() || edit.content.is_none()) {
        return Some(format!("{action:?} requires title and content").to_lowercase());
    }
    if action != RefinementAction::Delete && kind == RefinementKind::Skill {
        if edit.arguments.is_none() {
            return Some(format!("{action:?} skill requires arguments").to_lowercase());
        }
        let Some(reference) = &edit.reference else {
            return Some(format!("{action:?} skill requires python reference").to_lowercase());
        };
        if reference.get("type").and_then(|value| value.as_str()) != Some("python") {
            return Some(format!("{action:?} skill reference.type must be python").to_lowercase());
        }
        let has_import = reference
            .get("import")
            .and_then(|value| value.as_str())
            .is_some_and(|import| !import.is_empty())
            || reference
                .get("python_import")
                .and_then(|value| value.as_str())
                .is_some_and(|import| !import.is_empty());
        let has_callable = reference
            .get("callable")
            .and_then(|value| value.as_str())
            .is_some_and(|callable| !callable.is_empty())
            || reference
                .get("call_pattern")
                .and_then(|value| value.as_str())
                .is_some_and(|callable| !callable.is_empty());
        if !has_import {
            return Some(format!("{action:?} skill requires python import").to_lowercase());
        }
        if !has_callable {
            return Some(
                format!("{action:?} skill requires callable or call_pattern").to_lowercase(),
            );
        }
    }
    if action != RefinementAction::Delete
        && kind == RefinementKind::Factory
        && (action == RefinementAction::Create || edit.arguments.is_some())
    {
        // Structural check only: the kernel validator (`rlm.factory`) enforces
        // the full machine semantics at write time; do not reimplement it here.
        // A create requires its spec; an update may omit `arguments` entirely
        // and keep the stored spec (apply preserves `before.arguments`),
        // exactly like update_factory treats dag/machine.
        let arguments = edit.arguments.as_ref();
        // JSON null is treated as absent, exactly like the kernel's Python
        // writers (arguments.get("machine") returning None): a supplied
        // "machine": null never counts as the machine form.
        let dag = arguments
            .and_then(|args| args.get("dag"))
            .filter(|value| !value.is_null());
        let machine = arguments
            .and_then(|args| args.get("machine"))
            .filter(|value| !value.is_null());
        if dag.is_some() && machine.is_some() {
            return Some("pass either dag or machine form, not both".to_string());
        }
        let spec = machine.or(dag);
        if !matches!(spec, Some(serde_json::Value::Object(_))) {
            return Some("factory entry requires a dag or machine object in arguments".to_string());
        }
    }
    None
}

fn now_iso() -> String {
    crate::session::manager::format_iso_now()
}

/// The id an edit applies under: its own, or for a create the slug of its
/// title (or kind).
fn computed_edit_id(edit: &RefinementEdit) -> Option<String> {
    edit.id.clone().or_else(|| {
        (edit.action == Some(RefinementAction::Create)).then(|| {
            slug(
                edit.title
                    .as_deref()
                    .unwrap_or(kind_name(edit.kind.unwrap_or(RefinementKind::Memory))),
                kind_name(edit.kind.unwrap_or(RefinementKind::Memory)),
            )
        })
    })
}

/// How many of a proposal's edits pass the apply path's structural
/// validation (TS `countValidRefinementEdits`).
#[must_use]
pub fn count_valid_refinement_edits(proposal: &RefinementProposal) -> usize {
    proposal
        .edits
        .iter()
        .filter(|edit| validate_edit(edit, computed_edit_id(edit).as_deref()).is_none())
        .count()
}

/// A proposal's edits as rows none of which applied, each refused with
/// `error`, under the id apply would have given it (TS
/// `rejectedRefinementResult`'s edit rows).
#[must_use]
pub fn refused_refinement_edits(
    proposal: &RefinementProposal,
    error: &str,
) -> Vec<AppliedRefinementEdit> {
    proposal
        .edits
        .iter()
        .map(|edit| {
            let mut row = AppliedRefinementEdit::planned(
                edit,
                edit.action.unwrap_or(RefinementAction::Create),
                edit.kind.unwrap_or(RefinementKind::Memory),
                computed_edit_id(edit).unwrap_or_default(),
            );
            row.error = Some(error.to_string());
            row
        })
        .collect()
}

pub struct ApplyOptions {
    pub id: String,
    pub rollback_of: Option<String>,
    pub scope: Option<HarnessScope>,
    /// Target-scope state captured before planning; edits whose entry changed
    /// since the baseline are rejected.
    pub baseline_state: Option<HarnessState>,
    /// The resolved `factory.enabled` opt-in (default off). While it is off,
    /// factory create/update edits refuse with the one disabled message, the
    /// same gate the kernel-side factory writers raise
    /// (`rlm.factory.require_factory_enabled`).
    pub factory_enabled: bool,
    /// The read-only package overlay (upstream #2298): update and delete
    /// edits against its ids are refused; a same-id create is an editable
    /// override.
    pub package_state: Option<std::sync::Arc<HarnessState>>,
}

/// Apply a proposal to the state (mutating entries and recording the event).
///
/// # Panics
///
/// The unwraps cannot fire: validation runs first; the state pre-populates every kind map.
pub fn apply_refinement_proposal(
    state: &mut HarnessState,
    proposal: &RefinementProposal,
    options: ApplyOptions,
) -> super::RefinementResult {
    let mut applied_edits: Vec<AppliedRefinementEdit> = Vec::new();
    let mut proposal_modified_keys: std::collections::HashSet<String> =
        std::collections::HashSet::default();
    for edit in &proposal.edits {
        let computed_id = computed_edit_id(edit);
        let id = computed_id.clone().unwrap_or_default();
        let validation_error = validate_edit(edit, computed_id.as_deref());
        let Some(kind) = edit.kind else {
            let mut row = AppliedRefinementEdit::planned(
                edit,
                RefinementAction::Create,
                RefinementKind::Memory,
                id.clone(),
            );
            row.error = validation_error;
            applied_edits.push(row);
            continue;
        };
        if let Some(error) = validation_error {
            let mut row = AppliedRefinementEdit::planned(
                edit,
                edit.action.unwrap_or(RefinementAction::Create),
                kind,
                id.clone(),
            );
            row.error = Some(error);
            applied_edits.push(row);
            continue;
        }
        let action = edit.action.unwrap();
        let records = state.entries.get_mut(&kind).unwrap();
        let before = records.get(&id).cloned();
        // Package entries never exist in the editable store: an update or
        // delete that finds no editable entry but a package one is a
        // refused mutation, not a missing entry.
        if before.is_none()
            && matches!(action, RefinementAction::Update | RefinementAction::Delete)
            && options.package_state.as_ref().is_some_and(|package| {
                package
                    .entries
                    .get(&kind)
                    .is_some_and(|entries| entries.contains_key(&id))
            })
        {
            let mut row = AppliedRefinementEdit::planned(edit, action, kind, id.clone());
            row.error = Some(
                "package harness entry is read-only; create an editable same-kind, same-id override instead"
                    .to_string(),
            );
            applied_edits.push(row);
            continue;
        }
        let entry_key = format!("{}:{id}", kind_name(kind));
        let baseline = options.baseline_state.as_ref().and_then(|baseline| {
            baseline
                .entries
                .get(&kind)
                .and_then(|entries| entries.get(&id).cloned())
        });
        // Unmodelled keys are other producers' bookkeeping (TS compares the
        // entries without `trust`, settled at turn boundaries): their moving
        // meanwhile is no edit of the entry.
        if options.baseline_state.is_some()
            && !proposal_modified_keys.contains(&entry_key)
            && serde_json::to_value(before.as_ref().map(HarnessEntry::modelled)).ok()
                != serde_json::to_value(baseline.as_ref().map(HarnessEntry::modelled)).ok()
        {
            let mut row = AppliedRefinementEdit::planned(edit, action, kind, id.clone());
            row.before = before;
            row.error = Some("entry changed during refinement planning".to_string());
            applied_edits.push(row);
            continue;
        }
        // The opt-in gate, the host-side mirror of the kernel writers'
        // one refusal (`require_factory_enabled`): while `factory.enabled`
        // is off, a refinement cannot author or re-author factory entries,
        // exactly like every kernel factory write. The refusal precedes
        // the create/update existence checks, so the disabled message is
        // unconditional while off. A delete is not authoring: cleanup
        // stays available, the gate's documented split.
        if kind == RefinementKind::Factory
            && action != RefinementAction::Delete
            && !options.factory_enabled
        {
            let mut row = AppliedRefinementEdit::planned(edit, action, kind, id.clone());
            row.before = before;
            row.error = Some(super::FACTORY_DISABLED_MESSAGE.to_string());
            applied_edits.push(row);
            continue;
        }
        if action == RefinementAction::Delete {
            if before.is_none() {
                let mut row = AppliedRefinementEdit::planned(edit, action, kind, id.clone());
                row.error = Some("entry not found".to_string());
                applied_edits.push(row);
                continue;
            }
            records.remove(&id);
            proposal_modified_keys.insert(entry_key);
            let mut row = AppliedRefinementEdit::planned(edit, action, kind, id);
            row.before = before;
            row.applied = true;
            applied_edits.push(row);
            continue;
        }
        if action == RefinementAction::Create && before.is_some() {
            let mut row = AppliedRefinementEdit::planned(edit, action, kind, id.clone());
            row.before = before;
            row.error = Some("entry already exists".to_string());
            applied_edits.push(row);
            continue;
        }
        if action == RefinementAction::Update && before.is_none() {
            let mut row = AppliedRefinementEdit::planned(edit, action, kind, id.clone());
            row.error = Some("entry not found".to_string());
            applied_edits.push(row);
            continue;
        }
        // An update replaces the entry wholesale, and the refiner only saw the
        // overview's snippet of it: an entry it saw truncated would lose the
        // part it never saw (#1317). A rollback restores recorded snapshots,
        // not refiner output, so it is exempt; an entry this proposal itself
        // wrote is whole in the proposal.
        if action == RefinementAction::Update
            && options.rollback_of.is_none()
            && !proposal_modified_keys.contains(&entry_key)
        {
            let hidden = before
                .as_ref()
                .map_or(0, super::executor::overview_hidden_chars);
            if hidden > 0 {
                let mut row = AppliedRefinementEdit::planned(edit, action, kind, id.clone());
                row.before = before;
                row.error = Some(format!(
                    "entry was truncated in the refiner's view (+{hidden} chars not shown); an update would replace content it never saw"
                ));
                applied_edits.push(row);
                continue;
            }
        }
        let after = HarnessEntry {
            id: id.clone(),
            kind,
            title: edit
                .title
                .clone()
                .or_else(|| before.as_ref().map(|entry| entry.title.clone()))
                .unwrap_or_else(|| id.clone()),
            content: edit
                .content
                .clone()
                .or_else(|| before.as_ref().map(|entry| entry.content.clone()))
                .unwrap_or_default(),
            path: edit
                .path
                .clone()
                .or_else(|| before.as_ref().map(|entry| entry.path.clone()))
                .unwrap_or_else(|| "general".to_string()),
            scope: before
                .as_ref()
                .and_then(|entry| entry.scope)
                .or(options.scope)
                .or(Some(HarnessScope::Local)),
            reference: edit
                .reference
                .clone()
                .or_else(|| before.as_ref().map(|entry| entry.reference.clone()))
                .unwrap_or_default(),
            arguments: edit
                .arguments
                .clone()
                .or_else(|| before.as_ref().map(|entry| entry.arguments.clone()))
                .unwrap_or_default(),
            metadata: edit
                .metadata
                .clone()
                .or_else(|| before.as_ref().map(|entry| entry.metadata.clone()))
                .unwrap_or_default(),
            source: "refine".to_string(),
            created_at: before
                .as_ref()
                .map_or_else(now_iso, |entry| entry.created_at.clone()),
            updated_at: now_iso(),
            version: before.as_ref().map_or(1, |entry| entry.version + 1),
            // TS spreads `before` first: a key this function does not model
            // survives an update instead of being dropped by the rewrite.
            extensions: before
                .as_ref()
                .map(|entry| entry.extensions.clone())
                .unwrap_or_default(),
        };
        let mut after = after;
        if let Some(enabled) = edit.enabled {
            after.set_enabled(enabled);
        }
        records.insert(id.clone(), after.clone());
        proposal_modified_keys.insert(entry_key);
        let mut row = AppliedRefinementEdit::planned(edit, action, kind, id);
        row.before = before;
        row.after = Some(after);
        row.applied = true;
        applied_edits.push(row);
    }
    let changes: Vec<String> = applied_edits
        .iter()
        .filter(|edit| edit.applied)
        .map(|edit| {
            format!(
                "{} {}:{}",
                action_name(edit.action),
                kind_name(edit.kind),
                edit.id
            )
        })
        .collect();
    state.refinements.push(HarnessRefinementEvent {
        id: options.id.clone(),
        trigger: proposal.summary.clone(),
        changes,
        evidence: proposal.rationale.clone(),
        outcome: proposal.expected_outcome.clone(),
        created_at: now_iso(),
        reason: None,
    });
    super::RefinementResult {
        id: options.id,
        summary: proposal.summary.clone(),
        rationale: proposal.rationale.clone(),
        expected_outcome: proposal.expected_outcome.clone(),
        applied_edits,
        harness_state_path: String::new(),
        rollback_of: options.rollback_of,
        scope: options.scope,
        extensions: serde_json::Map::new(),
    }
}

/// The proposal that reverts a previously applied result.
#[must_use]
pub fn rollback_proposal(target: &super::RefinementResult) -> RefinementProposal {
    let mut edits: Vec<RefinementEdit> = Vec::new();
    for edit in target.applied_edits.iter().rev() {
        if !edit.applied {
            continue;
        }
        if let Some(before) = &edit.before {
            edits.push(RefinementEdit {
                action: Some(if edit.after.is_some() {
                    RefinementAction::Update
                } else {
                    RefinementAction::Create
                }),
                kind: Some(edit.kind),
                id: Some(edit.id.clone()),
                title: Some(before.title.clone()),
                content: Some(before.content.clone()),
                path: Some(before.path.clone()),
                reference: Some(before.reference.clone()),
                arguments: Some(before.arguments.clone()),
                metadata: Some(before.metadata.clone()),
                reason: Some(format!("Rollback {}", target.id)),
                // The flag reverts only when this refinement changed it: a
                // later, independent enable/disable stays.
                enabled: (edit.after.as_ref().map(HarnessEntry::is_enabled)
                    != Some(before.is_enabled()))
                .then_some(before.is_enabled()),
            });
        } else if edit.after.is_some() {
            edits.push(RefinementEdit {
                action: Some(RefinementAction::Delete),
                kind: Some(edit.kind),
                id: Some(edit.id.clone()),
                reason: Some(format!("Rollback {}", target.id)),
                ..Default::default()
            });
        }
    }
    RefinementProposal {
        summary: format!("Rollback refinement {}", target.id),
        rationale: format!(
            "Restores continual harness state snapshots from refinement {}.",
            target.id
        ),
        expected_outcome: "Faulty refinement edits are reverted.".to_string(),
        edits,
    }
}

fn action_name(action: RefinementAction) -> &'static str {
    match action {
        RefinementAction::Create => "create",
        RefinementAction::Update => "update",
        RefinementAction::Delete => "delete",
    }
}

fn kind_name(kind: RefinementKind) -> &'static str {
    match kind {
        RefinementKind::Prompt => "prompt",
        RefinementKind::Memory => "memory",
        RefinementKind::Skill => "skill",
        RefinementKind::Subagent => "subagent",
        RefinementKind::Factory => "factory",
    }
}

/// Byte-based input token bound (one token per UTF-8 byte).
fn refinement_input_token_bound(text: &str) -> u64 {
    text.len() as u64
}

/// Fit the refinement request into the model context: trim conversation from
/// the front (binary search) and clamp output tokens.
///
/// # Errors
///
/// Error when even the trimmed prompt leaves no room for output tokens.
pub fn refinement_request(
    model: &pa_types::ai::Model,
    system_prompt: &str,
    conversation_text: &str,
    build_prompt: &dyn Fn(&str) -> String,
    output_reserve: u64,
) -> anyhow::Result<(u64, String)> {
    let system_reserve =
        refinement_input_token_bound(system_prompt) + REFINEMENT_CONTEXT_OVERHEAD_TOKENS;
    let input_budget = model.context_window.saturating_sub(
        model
            .max_tokens
            .min(output_reserve)
            .min(model.context_window / 2),
    );
    let mut user_prompt = build_prompt(conversation_text);
    if system_reserve + refinement_input_token_bound(&user_prompt) > input_budget
        && !conversation_text.is_empty()
    {
        let prompt_for_length = |length: usize| -> String {
            let start = conversation_text.len().saturating_sub(length);
            // Avoid splitting a UTF-8 sequence (the TS surrogate skip).
            let mut start = start.min(conversation_text.len());
            while start < conversation_text.len() && !conversation_text.is_char_boundary(start) {
                start += 1;
            }
            build_prompt(&format!(
                "[Earlier conversation omitted to fit the model context.]\n{}",
                &conversation_text[start..]
            ))
        };
        let mut low = 0usize;
        let mut high = conversation_text.len();
        while low < high {
            let length = (low + high).div_ceil(2);
            if system_reserve + refinement_input_token_bound(&prompt_for_length(length))
                <= input_budget
            {
                low = length;
            } else {
                high = length - 1;
            }
        }
        user_prompt = prompt_for_length(low);
    }
    let max_tokens = model.max_tokens.min(
        model
            .context_window
            .saturating_sub(system_reserve + refinement_input_token_bound(&user_prompt)),
    );
    if max_tokens == 0 {
        anyhow::bail!(
            "Refinement prompt leaves no room for output in the model's context window; retry with a smaller request."
        );
    }
    Ok((max_tokens, user_prompt))
}

#[cfg(test)]
mod tests {
    use super::super::empty_harness_state;
    use super::*;

    fn create_memory_edit(id: &str, title: &str, content: &str) -> RefinementEdit {
        RefinementEdit {
            action: Some(RefinementAction::Create),
            kind: Some(RefinementKind::Memory),
            id: Some(id.to_string()),
            title: Some(title.to_string()),
            content: Some(content.to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn json_extraction_diagnoses_truncation() {
        let parsed = parse_proposal(r#"{"summary":"ok","edits":[]}"#).unwrap();
        assert_eq!(parsed.summary, "ok");
        let fenced = parse_proposal("```json\n{\"summary\":\"fenced\"}\n```").unwrap();
        assert_eq!(fenced.summary, "fenced");
        let prose = parse_proposal("Here you go:\n{\"summary\":\"sliced\"}\nAll set.").unwrap();
        assert_eq!(prose.summary, "sliced");
        // Truncated JSON reports the output-budget cause.
        let error = parse_proposal(r#"{"summary":"cut","edits":[{"action":"cre"#).unwrap_err();
        assert_eq!(error, TRUNCATED_JSON_ERROR);
        // Non-JSON text reports missing JSON.
        assert_eq!(
            parse_proposal("no json here").unwrap_err(),
            "Refiner did not return a JSON object"
        );
    }

    #[test]
    fn validation_rules() {
        let mut edit = create_memory_edit("m", "t", "c");
        edit.action = None;
        assert!(validate_edit(&edit, None).is_some());
        // base_system_prompt is not editable.
        let mut prompt_edit = RefinementEdit {
            action: Some(RefinementAction::Update),
            kind: Some(RefinementKind::Prompt),
            id: Some("base_system_prompt".to_string()),
            title: Some("t".into()),
            content: Some("c".into()),
            ..Default::default()
        };
        assert_eq!(
            validate_edit(&prompt_edit, None),
            Some("base system prompt is not editable".to_string())
        );
        // Skill edits require python reference + arguments + callable.
        let mut skill_edit = RefinementEdit {
            action: Some(RefinementAction::Create),
            kind: Some(RefinementKind::Skill),
            title: Some("Skill".into()),
            content: Some("Does things".into()),
            ..Default::default()
        };
        assert!(
            validate_edit(&skill_edit, None)
                .unwrap()
                .contains("skill requires arguments")
        );
        skill_edit.arguments = Some(serde_json::Map::default());
        assert!(
            validate_edit(&skill_edit, None)
                .unwrap()
                .contains("skill requires python reference")
        );
        skill_edit.reference = Some(
            serde_json::from_value(
                serde_json::json!({ "type": "python", "import": "pkg.mod", "callable": "run" }),
            )
            .unwrap(),
        );
        assert_eq!(validate_edit(&skill_edit, None), None);
        skill_edit.reference = Some(
            serde_json::from_value(
                serde_json::json!({ "type": "shell", "import": "pkg.mod", "callable": "run" }),
            )
            .unwrap(),
        );
        assert!(
            validate_edit(&skill_edit, None)
                .unwrap()
                .contains("reference.type must be python")
        );
        prompt_edit.id = Some("x".to_string());
    }

    #[test]
    fn factory_edits_accept_exactly_one_spec_form() {
        // Structural check only (TS shape): the kernel validator enforces
        // the full machine semantics at write time.
        let machine = serde_json::json!({
            "states": [{ "id": "collect", "entry": true, "subagent": "worker" }],
            "transitions": []
        });
        let dag = serde_json::json!({ "nodes": [{ "id": "collect", "subagent": "worker" }] });
        let mut edit = RefinementEdit {
            action: Some(RefinementAction::Create),
            kind: Some(RefinementKind::Factory),
            id: Some("sweep".to_string()),
            title: Some("Factory".into()),
            content: Some("Sweep review across changed files.".into()),
            ..Default::default()
        };
        // A dag object passes.
        edit.arguments = Some(serde_json::from_value(serde_json::json!({ "dag": dag })).unwrap());
        assert_eq!(validate_edit(&edit, None), None);
        // A machine object passes.
        edit.arguments =
            Some(serde_json::from_value(serde_json::json!({ "machine": machine })).unwrap());
        assert_eq!(validate_edit(&edit, None), None);
        // Both forms at once are rejected with the kernel wording.
        edit.arguments = Some(
            serde_json::from_value(serde_json::json!({ "dag": dag, "machine": machine })).unwrap(),
        );
        assert_eq!(
            validate_edit(&edit, None),
            Some("pass either dag or machine form, not both".to_string())
        );
        // Neither form (or a non-object spec) is rejected.
        edit.arguments = Some(serde_json::Map::default());
        assert_eq!(
            validate_edit(&edit, None),
            Some("factory entry requires a dag or machine object in arguments".to_string())
        );
        edit.arguments = Some(
            serde_json::from_value(serde_json::json!({ "machine": "not an object" })).unwrap(),
        );
        assert_eq!(
            validate_edit(&edit, None),
            Some("factory entry requires a dag or machine object in arguments".to_string())
        );
        edit.arguments = None;
        assert!(
            validate_edit(&edit, None)
                .unwrap()
                .contains("requires a dag or machine object")
        );
        // Delete edits carry no spec requirement.
        edit.action = Some(RefinementAction::Delete);
        assert_eq!(validate_edit(&edit, None), None);
        // An update that omits `arguments` keeps the stored spec (apply
        // preserves `before.arguments`), exactly like update_factory.
        edit.action = Some(RefinementAction::Update);
        edit.arguments = None;
        assert_eq!(validate_edit(&edit, None), None);
        // An update that does supply arguments gets the same shape checks.
        edit.arguments =
            Some(serde_json::from_value(serde_json::json!({ "machine": machine })).unwrap());
        assert_eq!(validate_edit(&edit, None), None);
        // JSON null is absent, exactly like the kernel's Python writers: a
        // valid dag with "machine": null is a dag-form edit, not both forms.
        edit.arguments = Some(
            serde_json::from_value(serde_json::json!({ "dag": dag, "machine": null })).unwrap(),
        );
        assert_eq!(validate_edit(&edit, None), None);
        edit.arguments = Some(
            serde_json::from_value(serde_json::json!({ "dag": dag, "machine": machine })).unwrap(),
        );
        assert_eq!(
            validate_edit(&edit, None),
            Some("pass either dag or machine form, not both".to_string())
        );
    }

    #[test]
    fn apply_create_update_delete() {
        let mut state = empty_harness_state();
        let proposal = RefinementProposal {
            summary: "add a memory".to_string(),
            rationale: "used twice".to_string(),
            expected_outcome: "faster".to_string(),
            edits: vec![create_memory_edit("m1", "Fact", "builds are green")],
        };
        let result = apply_refinement_proposal(
            &mut state,
            &proposal,
            ApplyOptions {
                id: "r1".to_string(),
                rollback_of: None,
                scope: Some(HarnessScope::Local),
                baseline_state: None,
                factory_enabled: false,
                package_state: None,
            },
        );
        assert_eq!(result.applied_edits.len(), 1);
        assert!(result.applied_edits[0].applied);
        let entry = &state.entries[&RefinementKind::Memory]["m1"];
        assert_eq!(entry.content, "builds are green");
        assert_eq!(entry.version, 1);
        assert_eq!(entry.scope, Some(HarnessScope::Local));
        let duplicate = apply_refinement_proposal(
            &mut state,
            &RefinementProposal {
                summary: "again".to_string(),
                rationale: String::new(),
                expected_outcome: String::new(),
                edits: vec![create_memory_edit("m1", "Fact", "again")],
            },
            ApplyOptions {
                id: "r2".to_string(),
                rollback_of: None,
                scope: None,
                baseline_state: None,
                factory_enabled: false,
                package_state: None,
            },
        );
        assert!(!duplicate.applied_edits[0].applied);
        assert_eq!(
            duplicate.applied_edits[0].error.as_deref(),
            Some("entry already exists")
        );
        let mut update_edit = create_memory_edit("m1", "Fact", "updated fact");
        update_edit.action = Some(RefinementAction::Update);
        apply_refinement_proposal(
            &mut state,
            &RefinementProposal {
                summary: "update".to_string(),
                rationale: String::new(),
                expected_outcome: String::new(),
                edits: vec![update_edit],
            },
            ApplyOptions {
                id: "r3".to_string(),
                rollback_of: None,
                scope: None,
                baseline_state: None,
                factory_enabled: false,
                package_state: None,
            },
        );
        assert_eq!(state.entries[&RefinementKind::Memory]["m1"].version, 2);
        let rollback = rollback_proposal(&result);
        let rolled = apply_refinement_proposal(
            &mut state,
            &rollback,
            ApplyOptions {
                id: "r4".to_string(),
                rollback_of: Some("r1".to_string()),
                scope: None,
                baseline_state: None,
                factory_enabled: false,
                package_state: None,
            },
        );
        assert!(rolled.applied_edits[0].applied);
        // r1 created m1 with no before snapshot, so the rollback deletes it.
        assert!(!state.entries[&RefinementKind::Memory].contains_key("m1"));
    }

    #[test]
    fn factory_edits_refuse_while_the_opt_in_is_disabled() {
        // The opt-in gate on the apply path: while `factory.enabled` is
        // off, a refinement cannot author or re-author factory entries —
        // the same refusal, byte for byte, the kernel-side factory
        // writers raise (`rlm.factory.require_factory_enabled`).
        let machine = serde_json::json!({
            "states": [{ "id": "collect", "entry": true, "subagent": "worker" }],
            "transitions": []
        });
        let factory_edit = |action: RefinementAction, id: &str| RefinementEdit {
            action: Some(action),
            kind: Some(RefinementKind::Factory),
            id: Some(id.to_string()),
            title: Some("Factory".into()),
            content: Some("Sweep review across changed files.".into()),
            arguments: Some(
                serde_json::from_value(serde_json::json!({ "machine": machine })).unwrap(),
            ),
            ..Default::default()
        };
        let mut state = empty_harness_state();
        let proposal = |edits: Vec<RefinementEdit>| RefinementProposal {
            summary: "sweep".to_string(),
            rationale: String::new(),
            expected_outcome: String::new(),
            edits,
        };
        let disabled = apply_refinement_proposal(
            &mut state,
            &proposal(vec![factory_edit(RefinementAction::Create, "sweep")]),
            ApplyOptions {
                id: "r1".to_string(),
                rollback_of: None,
                scope: Some(HarnessScope::Local),
                baseline_state: None,
                factory_enabled: false,
                package_state: None,
            },
        );
        assert!(!disabled.applied_edits[0].applied);
        assert_eq!(
            disabled.applied_edits[0].error.as_deref(),
            Some(super::super::FACTORY_DISABLED_MESSAGE)
        );
        assert!(state.entries[&RefinementKind::Factory].is_empty());
        // An update of a stored entry refuses too: cleanup is not
        // authoring, but re-authoring while disabled is.
        state
            .entries
            .get_mut(&RefinementKind::Factory)
            .unwrap()
            .insert(
                "sweep".to_string(),
                HarnessEntry {
                    id: "sweep".to_string(),
                    kind: RefinementKind::Factory,
                    title: "Factory".to_string(),
                    content: "Sweep.".to_string(),
                    path: "general".to_string(),
                    scope: Some(HarnessScope::Local),
                    reference: serde_json::Map::default(),
                    arguments: serde_json::Map::default(),
                    metadata: serde_json::Map::default(),
                    source: "refine".to_string(),
                    created_at: String::new(),
                    updated_at: String::new(),
                    version: 1,
                    extensions: serde_json::Map::new(),
                },
            );
        let refused_update = apply_refinement_proposal(
            &mut state,
            &proposal(vec![factory_edit(RefinementAction::Update, "sweep")]),
            ApplyOptions {
                id: "r2".to_string(),
                rollback_of: None,
                scope: None,
                baseline_state: None,
                factory_enabled: false,
                package_state: None,
            },
        );
        assert!(!refused_update.applied_edits[0].applied);
        assert_eq!(
            refused_update.applied_edits[0].error.as_deref(),
            Some(super::super::FACTORY_DISABLED_MESSAGE)
        );
        // A delete is not authoring: cleanup stays available while
        // disabled (the gate's documented split).
        let cleanup = apply_refinement_proposal(
            &mut state,
            &proposal(vec![{
                let mut edit = factory_edit(RefinementAction::Delete, "sweep");
                edit.arguments = None;
                edit
            }]),
            ApplyOptions {
                id: "r3".to_string(),
                rollback_of: None,
                scope: None,
                baseline_state: None,
                factory_enabled: false,
                package_state: None,
            },
        );
        assert!(cleanup.applied_edits[0].applied);
        assert!(state.entries[&RefinementKind::Factory].is_empty());
    }

    /// #1118: an update edit can disable an entry without deleting it, the
    /// flag survives later edits that do not name it, a rollback of the
    /// disabling refinement re-enables, and a non-boolean `enabled` (the
    /// schema's placeholder echoed back) reads as absent instead of
    /// discarding the edit.
    #[test]
    fn refinement_edits_disable_and_rollback_re_enables() {
        let options = |id: &str, rollback_of: Option<String>| ApplyOptions {
            id: id.to_string(),
            rollback_of,
            scope: Some(HarnessScope::Local),
            baseline_state: None,
            factory_enabled: false,
            package_state: None,
        };
        let mut state = empty_harness_state();
        let created = parse_proposal(
            r#"{"summary":"s","edits":[{"action":"create","kind":"subagent","id":"reviewer","title":"Reviewer","content":"Review diffs.","enabled":"optional boolean; false disables the entry without deleting it"}]}"#,
        )
        .unwrap();
        assert_eq!(
            created.edits.len(),
            1,
            "the placeholder never drops the edit"
        );
        assert_eq!(created.edits[0].enabled, None);
        apply_refinement_proposal(&mut state, &created, options("r1", None));
        let reviewer =
            |state: &HarnessState| state.entries[&RefinementKind::Subagent]["reviewer"].clone();
        assert!(reviewer(&state).is_enabled());
        assert!(
            !reviewer(&state).extensions.contains_key("enabled"),
            "no flag written by default"
        );

        let disable = parse_proposal(
            r#"{"summary":"retire","edits":[{"action":"update","kind":"subagent","id":"reviewer","title":"Reviewer","content":"Review diffs.","enabled":false}]}"#,
        )
        .unwrap();
        let disabled = apply_refinement_proposal(&mut state, &disable, options("r2", None));
        assert!(disabled.applied_edits[0].applied);
        assert!(!reviewer(&state).is_enabled());

        let retitle = parse_proposal(
            r#"{"summary":"retitle","edits":[{"action":"update","kind":"subagent","id":"reviewer","title":"Code reviewer","content":"Review diffs."}]}"#,
        )
        .unwrap();
        let retitled = apply_refinement_proposal(&mut state, &retitle, options("r3", None));
        assert!(
            !reviewer(&state).is_enabled(),
            "an edit without the flag keeps it"
        );
        // Rolling back the retitle keeps the independent disable.
        apply_refinement_proposal(
            &mut state,
            &rollback_proposal(&retitled),
            options("r4", Some("r3".to_string())),
        );
        assert_eq!(reviewer(&state).title, "Reviewer");
        assert!(!reviewer(&state).is_enabled());
        // Rolling back the disable re-enables.
        apply_refinement_proposal(
            &mut state,
            &rollback_proposal(&disabled),
            options("r5", Some("r2".to_string())),
        );
        assert!(reviewer(&state).is_enabled());
    }

    #[test]
    fn factory_edits_apply_when_the_opt_in_is_enabled() {
        let machine = serde_json::json!({
            "states": [{ "id": "collect", "entry": true, "subagent": "worker" }],
            "transitions": []
        });
        let mut state = empty_harness_state();
        let result = apply_refinement_proposal(
            &mut state,
            &RefinementProposal {
                summary: "sweep".to_string(),
                rationale: String::new(),
                expected_outcome: String::new(),
                edits: vec![RefinementEdit {
                    action: Some(RefinementAction::Create),
                    kind: Some(RefinementKind::Factory),
                    id: Some("sweep".to_string()),
                    title: Some("Factory".into()),
                    content: Some("Sweep review across changed files.".into()),
                    arguments: Some(
                        serde_json::from_value(serde_json::json!({ "machine": machine })).unwrap(),
                    ),
                    ..Default::default()
                }],
            },
            ApplyOptions {
                id: "r1".to_string(),
                rollback_of: None,
                scope: Some(HarnessScope::Local),
                baseline_state: None,
                factory_enabled: true,
                package_state: None,
            },
        );
        assert!(result.applied_edits[0].applied);
        assert!(result.applied_edits[0].error.is_none());
        assert!(state.entries[&RefinementKind::Factory].contains_key("sweep"));
    }

    /// An update keeps the entry's unmodelled keys (TS spreads `before`
    /// first), and the "entry changed during planning" check ignores them:
    /// another producer's bookkeeping (the fork's `trust`, settled at turn
    /// boundaries) moving meanwhile is not an edit of the entry.
    #[test]
    fn an_update_keeps_unmodelled_entry_keys_and_the_planning_check_ignores_them() {
        let stored = |trust: u64| -> HarnessEntry {
            serde_json::from_value(serde_json::json!({
                "id": "s1", "kind": "memory", "title": "S", "content": "old", "path": "general",
                "scope": "local", "reference": {}, "arguments": {}, "metadata": {},
                "source": "refine", "created_at": "t0", "updated_at": "t1", "version": 1,
                "trust": {"score": trust, "updated_at": "t2", "events": []}
            }))
            .unwrap()
        };
        let with = |entry: HarnessEntry| {
            let mut state = empty_harness_state();
            state
                .entries
                .get_mut(&RefinementKind::Memory)
                .unwrap()
                .insert("s1".to_string(), entry);
            state
        };
        let baseline = with(stored(50));
        let mut state = with(stored(35));
        let result = apply_refinement_proposal(
            &mut state,
            &parse_proposal(
                r#"{"summary":"s","edits":[{"action":"update","kind":"memory","id":"s1","title":"S","content":"new"}]}"#,
            )
            .unwrap(),
            ApplyOptions {
                id: "r1".to_string(),
                rollback_of: None,
                scope: Some(HarnessScope::Local),
                baseline_state: Some(baseline),
                factory_enabled: false,
                package_state: None,
            },
        );
        assert_eq!(result.applied_edits[0].error, None);
        let after = serde_json::to_value(&state.entries[&RefinementKind::Memory]["s1"]).unwrap();
        assert_eq!(after["content"], "new");
        assert_eq!(
            after["trust"],
            serde_json::json!({"score": 35, "updated_at": "t2", "events": []})
        );
    }

    /// The screen counts the edits apply would accept structurally, and a
    /// refused proposal's rows carry the ids apply would have given them.
    #[test]
    fn valid_edit_count_and_refused_rows_follow_the_apply_path() {
        let proposal = parse_proposal(
            r#"{"summary":"s","edits":[
                {"action":"create","kind":"memory","title":"Use Tactic A","content":"c"},
                {"action":"update","kind":"skill","id":"sk","title":"t","content":"c"},
                {"action":"delete","kind":"prompt","id":"p1"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(count_valid_refinement_edits(&proposal), 2);
        let rows = refused_refinement_edits(&proposal, "refused");
        let ids: Vec<(&str, bool, Option<&str>)> = rows
            .iter()
            .map(|row| (row.id.as_str(), row.applied, row.error.as_deref()))
            .collect();
        assert_eq!(
            ids,
            [
                ("use_tactic_a", false, Some("refused")),
                ("sk", false, Some("refused")),
                ("p1", false, Some("refused")),
            ]
        );
    }
}
