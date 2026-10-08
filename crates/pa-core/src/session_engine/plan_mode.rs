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

use crate::kernel::shared::HostRequestHandlers;

pub use crate::kernel::plan_guard::PlanModeSwitch;

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

fn hidden_row(custom_type: &str, text: &str) -> CustomMessage {
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
}

/// Parse `/plan [on|off|status]`.
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
        _ => Err("Usage: /plan [on|off|status]".to_string()),
    }
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
    use super::*;
    use pa_types::session::{CustomMessageEntry, EntryBase};

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
            ["", " on ", "off", "status", "maybe"].map(parse_plan_command),
            [
                Ok(PlanCommand::Toggle),
                Ok(PlanCommand::On),
                Ok(PlanCommand::Off),
                Ok(PlanCommand::Status),
                Err("Usage: /plan [on|off|status]".to_string()),
            ]
        );
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
                cell_source_code: None,
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
