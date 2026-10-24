//! Plan mode: a session mode in which the agent investigates and plans but
//! does not change files (upstream #305).
//!
//! Enforcement has three layers, none of which the model can switch off:
//! the kernel's confinement (the OS sandbox tightened to `read-only`, the
//! kernel restarting into it; the runtime's in-kernel write guard where this
//! machine has no OS sandbox; see [`crate::kernel::plan_guard`]), the host's
//! refusal of its own mutating tools (`edit`, `write`, `bash`) before they
//! execute, and the host's refusal of host requests that would act outside
//! the confined kernel. The model learns the mode from a per-turn
//! context row (never a system-prompt change, so the static prompt layers
//! stay cache-stable) and a one-shot notice when it ends.
//!
//! The mode is session state: a durable `plan_mode_change` row records each
//! change (resume restores the newest one on the active branch), and an
//! `rlm.spawn` child inherits its parent's mode.

use std::sync::Arc;

use pa_agent::agent_loop::BeforeToolCallFn;
use pa_types::session::{AgentMessage, CustomMessage, FileEntry};

pub use crate::kernel::plan_guard::PlanModeSwitch;
use crate::kernel::shared::HostRequestHandlers;

/// The durable row recording one plan-mode change (`details.enabled`).
pub const PLAN_MODE_CHANGE_CUSTOM_TYPE: &str = "plan_mode_change";
/// The per-turn row telling the model plan mode is on (not displayed).
pub const PLAN_MODE_CONTEXT_CUSTOM_TYPE: &str = "plan_mode_context";
/// The one-shot row telling the model plan mode ended (not displayed).
pub const PLAN_MODE_EXITED_CUSTOM_TYPE: &str = "plan_mode_exited";

/// The adoption telemetry event for one plan-mode change (`pa-telemetry`
/// catalog, schema v4): `enabled` and `source` (`command` for `/plan` and
/// its key, `flag` for `--plan`) only.
pub const PLAN_MODE_TOGGLED_EVENT: &str = "plan mode toggled";

/// Host tools refused while plan mode is on; `ipython` is confined with the
/// kernel instead.
pub const PLAN_MODE_BLOCKED_TOOLS: [&str; 3] = ["edit", "write", "bash"];

/// Host requests refused while plan mode is on: each acts outside the
/// confined kernel (a fresh top-level session that would not inherit the mode,
/// and an environment adapter process the host spawns itself).
pub const PLAN_MODE_REFUSED_HOST_REQUESTS: [&str; 2] = ["rlm.create_session", "system_router.run"];

const PLAN_MODE_PROMPT: &str = "<plan_mode>
Plan mode is active. Your job this turn is to investigate and produce a plan the user can act on — not to make changes. It stays active until the user turns it off; treat any request to make changes as a request to plan those changes, not perform them.

You may explore and run non-mutating actions that improve the plan. You must not perform mutating actions.

Allowed (non-mutating): reading and searching files, static analysis and repo exploration, read-only commands, and dry-runs / tests / builds that only touch caches or build artifacts, not repo-tracked files.

Not allowed (mutating): editing, creating, or deleting files; running formatters or linters that rewrite files; applying patches, migrations, or codegen; git commits; and any side-effectful command whose purpose is to carry out the work rather than plan it.

When in doubt: if the action is better described as \"doing the work\" than \"planning the work,\" don't do it. Mutating operations are blocked (they fail with a permission error or raise PlanModeError) — do not retry them or look for a workaround; the block is intentional.

Explore first, then present a concrete plan: the goal, the specific changes you'd make (files/functions/approach), and how you'd verify them. Make it detailed enough to hand off. When the plan is ready, tell the user they can turn off plan mode to proceed. If the user's request is a pure question rather than a change, just answer it — no plan needed.
</plan_mode>";

const PLAN_MODE_EXITED_PROMPT: &str = "<plan_mode_off>Plan mode has been turned off. You may now edit files and run commands normally.</plan_mode_off>";

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

pub(crate) fn hidden_row(custom_type: &str, text: &str) -> CustomMessage {
    CustomMessage {
        custom_type: custom_type.to_string(),
        content: pa_types::ai::UserContent::Text(text.to_string()),
        display: false,
        details: None,
        timestamp: now_millis(),
        rest: serde_json::Map::default(),
    }
}

/// The per-turn plan-mode context row.
#[must_use]
pub fn plan_mode_context_row() -> CustomMessage {
    hidden_row(PLAN_MODE_CONTEXT_CUSTOM_TYPE, PLAN_MODE_PROMPT)
}

/// The one-shot row for the first turn after plan mode ends: without it the
/// per-turn row just stops appearing, which the model cannot observe.
#[must_use]
pub fn plan_mode_exited_row() -> CustomMessage {
    hidden_row(PLAN_MODE_EXITED_CUSTOM_TYPE, PLAN_MODE_EXITED_PROMPT)
}

/// The durable, displayed row recording a plan-mode change.
#[must_use]
pub fn plan_mode_change_row(enabled: bool) -> CustomMessage {
    let text = if enabled {
        "Plan mode on: the agent investigates and plans; file edits are blocked until plan mode is turned off (/plan off)."
    } else {
        "Plan mode off: the agent may edit files again."
    };
    CustomMessage {
        custom_type: PLAN_MODE_CHANGE_CUSTOM_TYPE.to_string(),
        content: pa_types::ai::UserContent::Text(text.to_string()),
        display: true,
        details: Some(serde_json::json!({ "enabled": enabled })),
        timestamp: now_millis(),
        rest: serde_json::Map::default(),
    }
}

fn change_enabled(custom_type: &str, details: Option<&serde_json::Value>) -> Option<bool> {
    if custom_type != PLAN_MODE_CHANGE_CUSTOM_TYPE {
        return None;
    }
    details?.get("enabled")?.as_bool()
}

/// The plan-mode state one session entry records, if it is a change row.
#[must_use]
pub fn plan_mode_of_entry(entry: &FileEntry) -> Option<bool> {
    match entry {
        FileEntry::CustomMessage { payload, .. } => {
            change_enabled(&payload.custom_type, payload.details.as_ref())
        }
        FileEntry::Message {
            message: AgentMessage::Custom(message),
            ..
        } => change_enabled(&message.custom_type, message.details.as_ref()),
        _ => None,
    }
}

/// The newest plan-mode state along a branch's entries (oldest first);
/// `None` when the branch never changed it.
#[must_use]
pub fn plan_mode_in_entries<'a>(
    entries: impl DoubleEndedIterator<Item = &'a FileEntry>,
) -> Option<bool> {
    entries.rev().find_map(plan_mode_of_entry)
}

/// One `/plan` invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanCommand {
    /// Bare `/plan`: flip the mode.
    Toggle,
    On,
    Off,
    Status,
    /// `/plan view`: show the current plan file.
    View,
}

/// Parse `/plan [on|off|status|view]`.
///
/// # Errors
///
/// Returns the usage text for any other argument.
pub fn parse_plan_command(args: &str) -> Result<PlanCommand, String> {
    match args.trim() {
        "" => Ok(PlanCommand::Toggle),
        "on" => Ok(PlanCommand::On),
        "off" => Ok(PlanCommand::Off),
        "status" => Ok(PlanCommand::Status),
        "view" => Ok(PlanCommand::View),
        _ => Err("Usage: /plan [on|off|status|view]".to_string()),
    }
}

// Model-callable enter/exit plan mode (Claude Code's ExitPlanMode and the
// plan-approval dialog, 2.1.309 `chunk-7ak0g5xy` `spt`/`apt`).

/// Claude Code's plan-approval prompt (`chunk-7ak0g5xy`).
pub const PLAN_APPROVAL_PROMPT: &str =
    "Claude has written up a plan and is ready to execute. Would you like to proceed?";
/// Claude Code's "auto-accept edits" choice label.
pub const PLAN_CHOICE_ACCEPT_EDITS_LABEL: &str = "Yes, and auto-accept edits";
/// Claude Code's "manually approve edits" choice label.
pub const PLAN_CHOICE_MANUAL_LABEL: &str = "Yes, and manually approve edits";
/// Claude Code's "keep planning" choice label.
pub const PLAN_CHOICE_KEEP_PLANNING_LABEL: &str = "No, keep planning";

/// The consent prompt for the model's request to enter plan mode.
pub const ENTER_PLAN_PROMPT: &str = "The agent wants to enter plan mode: it will investigate and plan, and file edits are blocked until you approve a plan or turn plan mode off. Enter plan mode?";

/// The user's answer to the exit-plan approval dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanExitChoice {
    /// Approve the plan and auto-accept edits (`acceptEdits`).
    AcceptEdits,
    /// Approve the plan and approve edits manually (`default`).
    ApproveManually,
    /// Keep planning, with the user's feedback for the model.
    KeepPlanning { feedback: Option<String> },
}

impl PlanExitChoice {
    /// The answer object's `choice` string.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            PlanExitChoice::AcceptEdits => "acceptEdits",
            PlanExitChoice::ApproveManually => "default",
            PlanExitChoice::KeepPlanning { .. } => "keepPlanning",
        }
    }

    /// The permission mode a client switches to on approval; `None` for
    /// keep-planning (plan mode stays on). The daemon maps it to
    /// `pa_permissions::PermissionMode` and its `/permissions mode` command.
    #[must_use]
    pub fn permission_mode(&self) -> Option<&'static str> {
        match self {
            PlanExitChoice::AcceptEdits => Some("acceptEdits"),
            PlanExitChoice::ApproveManually => Some("default"),
            PlanExitChoice::KeepPlanning { .. } => None,
        }
    }

    /// Whether the plan was approved (plan mode turns off).
    #[must_use]
    pub fn approved(&self) -> bool {
        !matches!(self, PlanExitChoice::KeepPlanning { .. })
    }
}

/// The exit-plan approval question payload (`QUESTION_KIND_PLAN_APPROVAL`):
/// the plan text, the plan file path, Claude Code's prompt, and the three
/// choices the client renders.
#[must_use]
pub fn plan_approval_payload(plan: &str, plan_file_path: &str) -> serde_json::Value {
    serde_json::json!({
        "prompt": PLAN_APPROVAL_PROMPT,
        "plan": plan,
        "planFilePath": plan_file_path,
        "choices": [
            { "value": "acceptEdits", "label": PLAN_CHOICE_ACCEPT_EDITS_LABEL },
            { "value": "default", "label": PLAN_CHOICE_MANUAL_LABEL },
            { "value": "keepPlanning", "label": PLAN_CHOICE_KEEP_PLANNING_LABEL, "feedback": true },
        ],
    })
}

/// Read the exit-plan approval dialog's answer object
/// (`{choice, feedback?}`); an unknown or missing choice keeps planning.
#[must_use]
pub fn interpret_plan_exit_answer(answer: &serde_json::Value) -> PlanExitChoice {
    let feedback = answer
        .get("feedback")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|feedback| !feedback.is_empty())
        .map(str::to_string);
    match answer.get("choice").and_then(serde_json::Value::as_str) {
        Some("acceptEdits") => PlanExitChoice::AcceptEdits,
        Some("default") => PlanExitChoice::ApproveManually,
        _ => PlanExitChoice::KeepPlanning { feedback },
    }
}

/// The kernel reply `rlm.exit_plan_mode` returns for `choice`.
#[must_use]
pub fn plan_exit_reply(choice: &PlanExitChoice) -> serde_json::Value {
    match choice {
        PlanExitChoice::AcceptEdits => serde_json::json!({
            "status": "approved",
            "mode": "acceptEdits",
            "message": "The user approved the plan and turned on auto-accept edits. Plan mode is off; carry out the plan, applying edits without asking.",
        }),
        PlanExitChoice::ApproveManually => serde_json::json!({
            "status": "approved",
            "mode": "default",
            "message": "The user approved the plan. Plan mode is off; carry out the plan, and edits are approved as usual.",
        }),
        PlanExitChoice::KeepPlanning { feedback } => {
            let mut reply = serde_json::json!({
                "status": "keep_planning",
                "message": match feedback {
                    Some(feedback) => format!("The user wants to keep planning. Their feedback: {feedback}. Revise the plan accordingly; do not start the work yet."),
                    None => "The user wants to keep planning. Do not start the work yet; refine the plan.".to_string(),
                },
            });
            if let Some(feedback) = feedback {
                reply["feedback"] = serde_json::json!(feedback);
            }
            reply
        }
    }
}

/// The consent question payload for entering plan mode
/// (`QUESTION_KIND_ENTER_PLAN_MODE`).
#[must_use]
pub fn enter_plan_payload() -> serde_json::Value {
    serde_json::json!({
        "prompt": ENTER_PLAN_PROMPT,
        "choices": [
            { "value": "enter", "label": "Enter plan mode" },
            { "value": "decline", "label": "No, keep working normally" },
        ],
    })
}

/// Whether the enter-plan consent answer said to enter (`{choice: "enter"}`).
#[must_use]
pub fn enter_plan_accepted(answer: &serde_json::Value) -> bool {
    answer.get("choice").and_then(serde_json::Value::as_str) == Some("enter")
}

/// The warning shown when plan mode turns on where no OS sandbox can enforce
/// it: the in-kernel guard is all that holds the kernel.
#[must_use]
pub fn fallback_notice(reason: &str) -> String {
    format!(
        "Plan mode is enforced inside the Python kernel only: this machine has no OS sandbox \
         ({reason}). The agent's commands are refused, and code that calls the C library \
         directly (ctypes) can still write files."
    )
}

/// The warning shown when a plan-mode toggle restarted the kernel in a
/// session that keeps no state snapshot.
pub const NAMESPACE_RESET_NOTICE: &str = "Switching plan mode restarted the Python kernel under a different OS sandbox. This session keeps no state snapshot, so the kernel's variables, imports and loaded data were lost.";

/// What a refused tool call tells the model.
#[must_use]
pub fn blocked_tool_reason(tool_name: &str) -> String {
    format!(
        "Plan mode is active: the {tool_name} tool is disabled. Present your plan or answer, and ask the user to exit plan mode if changes are needed."
    )
}

/// Put the plan-mode refusal in front of the session's tool hook: while the
/// mode is on, a blocked host tool never executes (and `inner`, the
/// installed features' observer, never sees it).
#[must_use]
pub fn gate_tool_calls(mode: PlanModeSwitch, inner: Option<BeforeToolCallFn>) -> BeforeToolCallFn {
    Arc::new(move |call, signal| {
        if mode.is_enabled() && PLAN_MODE_BLOCKED_TOOLS.contains(&call.tool_call.name.as_str()) {
            let reason = blocked_tool_reason(&call.tool_call.name);
            return Box::pin(async move {
                Ok(Some(pa_agent::types::BeforeToolCallResult {
                    block: true,
                    reason: Some(reason),
                    args: None,
                }))
            });
        }
        match &inner {
            Some(inner) => inner(call, signal),
            None => Box::pin(async { Ok(None) }),
        }
    })
}

/// Refuse [`PLAN_MODE_REFUSED_HOST_REQUESTS`] while the mode is on; the
/// registered handler runs unchanged otherwise. Call after every handler is
/// registered.
pub fn gate_host_requests(handlers: &mut HostRequestHandlers, mode: &PlanModeSwitch) {
    for request_type in PLAN_MODE_REFUSED_HOST_REQUESTS {
        let Some(inner) = handlers.get(request_type).cloned() else {
            continue;
        };
        let mode = mode.clone();
        handlers.register(
            request_type,
            Arc::new(move |payload| {
                if mode.is_enabled() {
                    return Box::pin(async move {
                        Err(anyhow::anyhow!(
                            "Plan mode is active: {request_type} is blocked. Present your plan or answer, and ask the user to exit plan mode if changes are needed."
                        ))
                    });
                }
                inner(payload)
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use pa_types::session::{CustomMessageEntry, EntryBase};

    use super::*;

    fn base() -> EntryBase {
        serde_json::from_value(serde_json::json!({ "id": "e" })).unwrap()
    }

    fn change_entry(enabled: bool) -> FileEntry {
        let row = plan_mode_change_row(enabled);
        FileEntry::CustomMessage {
            payload: CustomMessageEntry {
                custom_type: row.custom_type,
                content: row.content,
                details: row.details,
                display: row.display,
                rest: serde_json::Map::default(),
            },
            base: base(),
        }
    }

    #[test]
    fn the_newest_change_on_the_branch_wins() {
        let entries = [change_entry(true), change_entry(false), change_entry(true)];
        assert_eq!(plan_mode_in_entries(entries.iter()), Some(true));
        assert_eq!(plan_mode_in_entries(entries[..2].iter()), Some(false));
        assert_eq!(plan_mode_in_entries([].iter()), None);
        // A wire-shaped custom message row counts the same.
        let message = FileEntry::Message {
            message: AgentMessage::Custom(plan_mode_change_row(false)),
            base: base(),
        };
        assert_eq!(
            plan_mode_in_entries([change_entry(true), message].iter()),
            Some(false)
        );
    }

    #[test]
    fn plan_command_parsing() {
        assert_eq!(
            ["", " on ", "off", "status", "view", "maybe"].map(parse_plan_command),
            [
                Ok(PlanCommand::Toggle),
                Ok(PlanCommand::On),
                Ok(PlanCommand::Off),
                Ok(PlanCommand::Status),
                Ok(PlanCommand::View),
                Err("Usage: /plan [on|off|status|view]".to_string()),
            ]
        );
    }

    #[test]
    fn the_plan_exit_dialog_maps_choices_to_modes_and_replies() {
        use serde_json::json;
        let accept = interpret_plan_exit_answer(&json!({ "choice": "acceptEdits" }));
        assert_eq!(accept, PlanExitChoice::AcceptEdits);
        assert_eq!(accept.permission_mode(), Some("acceptEdits"));
        assert!(accept.approved());
        assert_eq!(plan_exit_reply(&accept)["mode"], "acceptEdits");

        let manual = interpret_plan_exit_answer(&json!({ "choice": "default" }));
        assert_eq!(manual, PlanExitChoice::ApproveManually);
        assert_eq!(manual.permission_mode(), Some("default"));
        assert_eq!(plan_exit_reply(&manual)["status"], "approved");

        let keep = interpret_plan_exit_answer(
            &json!({ "choice": "keepPlanning", "feedback": " add tests " }),
        );
        assert_eq!(
            keep,
            PlanExitChoice::KeepPlanning {
                feedback: Some("add tests".to_string())
            }
        );
        assert_eq!(keep.permission_mode(), None);
        assert!(!keep.approved());
        let reply = plan_exit_reply(&keep);
        assert_eq!(reply["status"], "keep_planning");
        assert_eq!(reply["feedback"], "add tests");

        // An unknown or missing choice keeps planning (safe default).
        assert_eq!(
            interpret_plan_exit_answer(&json!({})),
            PlanExitChoice::KeepPlanning { feedback: None }
        );

        // The approval payload carries the plan, the path and three choices.
        let payload = plan_approval_payload("# Plan", "/p/plans/s.md");
        assert_eq!(payload["plan"], "# Plan");
        assert_eq!(payload["planFilePath"], "/p/plans/s.md");
        assert_eq!(payload["choices"].as_array().unwrap().len(), 3);
        assert_eq!(payload["prompt"], PLAN_APPROVAL_PROMPT);

        assert!(enter_plan_accepted(&json!({ "choice": "enter" })));
        assert!(!enter_plan_accepted(&json!({ "choice": "decline" })));
        assert!(!enter_plan_accepted(&json!({})));
    }

    #[tokio::test]
    async fn refused_host_requests_follow_the_switch() {
        let mut handlers = HostRequestHandlers::new();
        handlers.register(
            "rlm.create_session",
            crate::kernel::shared::host_handler(|_| async { Ok(serde_json::json!({"ok": true})) }),
        );
        let mode = PlanModeSwitch::new(true);
        gate_host_requests(&mut handlers, &mode);
        let call = || {
            handlers.get("rlm.create_session").unwrap()(crate::kernel::shared::HostRequestPayload {
                data: serde_json::json!({}),
            })
        };
        assert_eq!(
            call().await.unwrap_err().to_string(),
            "Plan mode is active: rlm.create_session is blocked. Present your plan or answer, and ask the user to exit plan mode if changes are needed."
        );
        mode.set(false);
        assert_eq!(call().await.unwrap(), serde_json::json!({"ok": true}));
    }
}

#[cfg(test)]
#[path = "plan_mode_tests.rs"]
mod session_tests;
