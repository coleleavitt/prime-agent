//! `rlm.harness.overview()`: the kernel's plain-text listing of one store.

use serde_json::Value;

use super::pyfmt;
use crate::refinement::{HarnessEntry, HarnessScope, HarnessState, RefinementKind};

const CALL_CONTRACT: &str = "Call contract: installed Python skills use await <skill_import>(...) or a matching shell CLI; harness skill entries are Python REPL skills and must include a Python reference plus arguments. Spawn a subagent spec by composing a concise task prompt and calling handle = await rlm.spawn('sub-task', name='worker'); admission returns immediately with rlm_child_id, name, session_dir, and model, never the child's answer. Results arrive only through explicit agent_message replies or files; children reply with await agent_message.send(message, receiver_role='parent'). Use await rlm.list_subagents() to recover direct child handles and await agent_message.send(..., receiver_role='child', receiver_name=handle.name) for follow-ups.";

const FACTORY_CONTRACT: &str = "Factory entries declare validated state-machine workflows of subagent states in arguments['machine'] (the original DAG sugar in arguments['dag'] compiles to machine form): manage them with create_factory/update_factory/delete_factory (create_factory validates either form at write time); run them with await rlm.factory.run(\"<id>\"), watch with await rlm.factory.status(run_id), stop with await rlm.factory.stop(run_id), and resume an escalate-paused run with await rlm.factory.resume(run_id).";

/// Python's `text[:117] + "..."` past 120 characters.
fn clip(text: &str) -> String {
    if text.chars().count() > 120 {
        let head: String = text.chars().take(117).collect();
        format!("{head}...")
    } else {
        text.to_string()
    }
}

fn record_summary(label: &str, record: &serde_json::Map<String, Value>) -> String {
    if record.is_empty() {
        return String::new();
    }
    let text = pyfmt::json_dumps_sorted(&Value::Object(record.clone()));
    format!(" {label}={}", clip(&text))
}

pub(crate) fn scope_label(scope: HarnessScope) -> &'static str {
    match scope {
        HarnessScope::Local => "local",
        HarnessScope::Global => "global",
    }
}

fn entry_line(entry: &HarnessEntry) -> String {
    let summary = clip(&pyfmt::strip(&entry.content).replace('\n', " "));
    let (reference, arguments) = if entry.kind == RefinementKind::Skill {
        (
            record_summary("ref", &entry.reference),
            record_summary("args", &entry.arguments),
        )
    } else {
        (String::new(), String::new())
    };
    let disabled = if entry.is_enabled() {
        ""
    } else {
        " [disabled]"
    };
    format!(
        "  - [{}:{}]{disabled} {} ({}, v{}){reference}{arguments}: {summary}",
        scope_label(entry.scope.unwrap_or(HarnessScope::Local)),
        entry.id,
        entry.title,
        entry.path,
        entry.version
    )
}

/// The overview text. `file_label` is the store's file as the kernel
/// names it (`None` for an in-memory store); `listed` yields one kind's
/// entries in list order and `limit` slices them like Python's `[:limit]`.
pub(crate) fn render(
    state: &HarnessState,
    scope: HarnessScope,
    file_label: &str,
    limit: Option<i64>,
    listed: impl Fn(RefinementKind) -> Vec<HarnessEntry>,
) -> String {
    let mut lines = vec![
        format!("Harness state ({}): {file_label}", scope_label(scope)),
        CALL_CONTRACT.to_string(),
        FACTORY_CONTRACT.to_string(),
    ];
    for (name, kind) in super::KINDS {
        let entries = listed(kind);
        let total = entries.len();
        let shown = match limit {
            None => total,
            Some(limit) if limit >= 0 => total.min(limit as usize),
            Some(limit) => total.saturating_sub(limit.unsigned_abs() as usize),
        };
        lines.push(format!("{name}: {total}"));
        lines.extend(entries.iter().take(shown).map(entry_line));
        if total > shown {
            lines.push(format!("  - +{} more", total - shown));
        }
    }
    if state.refinements.is_empty() {
        lines.push("refinements: 0".to_string());
    } else {
        lines.push(format!("refinements: {}", state.refinements.len()));
        let skip = state.refinements.len().saturating_sub(5);
        for event in &state.refinements[skip..] {
            lines.push(format!(
                "  - [{}] {}: {}",
                event.id,
                event.trigger,
                event.changes.join(", ")
            ));
        }
    }
    lines.join("\n")
}
