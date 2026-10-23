//! `rlm.enter_plan_mode` / `rlm.exit_plan_mode`: the model asks to enter
//! plan mode, and presents a finished plan for the user to approve, Claude
//! Code's `ExitPlanMode` and its plan-approval dialog (2.1.309
//! `chunk-7ak0g5xy` `spt`/`apt`).
//!
//! Both run on the session's confirmation surface (the same surface as
//! [`super::ask_user`]): `enter` asks for consent
//! ([`crate::features::QUESTION_KIND_ENTER_PLAN_MODE`]), `exit` writes the
//! plan to the plan file ([`super::plan_file`]) and asks for approval
//! ([`crate::features::QUESTION_KIND_PLAN_APPROVAL`]) with Claude Code's
//! three choices (auto-accept edits / approve manually / keep planning with
//! feedback). The decision is applied at the session's next turn boundary
//! through a [`PlanModeControl`] the engine binds — the host handler cannot
//! restart the kernel mid-cell, so it never toggles plan mode directly.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use super::plan_file::PlanFile;
use super::plan_mode::{self, PlanExitChoice};
use super::rlm_host::RlmHostBridge;
use crate::features::{
    QUESTION_KIND_ENTER_PLAN_MODE,
    QUESTION_KIND_PLAN_APPROVAL,
    QuestionAnswer,
    ask_question,
};
use crate::kernel::shared::{HostRequestHandlers, host_handler};

/// The host request `rlm.enter_plan_mode` sends.
pub const ENTER_PLAN_REQUEST: &str = "plan.enter";
/// The host request `rlm.exit_plan_mode` sends.
pub const EXIT_PLAN_REQUEST: &str = "plan.exit";

/// Why plan mode cannot be driven here: no interactive user.
pub const UNAVAILABLE_MESSAGE: &str = "plan mode is unavailable: no one can approve plan-mode changes in this session (print mode, or no interactive client is attached).";
/// Why a subagent cannot drive plan mode.
pub const SUBAGENT_MESSAGE: &str =
    "plan mode is only driven from the main session: a subagent cannot enter or exit plan mode.";
/// What `rlm.exit_plan_mode` raises when the session is not in plan mode.
pub const NOT_IN_PLAN_MESSAGE: &str =
    "rlm.exit_plan_mode is only available in plan mode; the session is not in plan mode.";
/// What a withdrawn or unconfigured apply reports to the model.
pub const NO_CONTROL_MESSAGE: &str =
    "plan mode cannot be applied in this session: no plan-mode controller is bound.";

/// Applies a model-driven plan-mode change at the session's next turn
/// boundary. The host handler runs the approval dialog and then calls this;
/// the engine binds an implementation that calls
/// [`crate::session_engine::engine::SessionEngine::set_plan_mode`] at the
/// boundary (the handler itself cannot, because the kernel is busy running
/// the model's cell). `permission_mode` is the `pa_permissions` spelling the
/// approval chose (`acceptEdits` / `default`); `None` leaves it (keep
/// planning), and a session whose feature set has no permissions feature
/// ignores it.
pub trait PlanModeControl: Send + Sync {
    /// Queue plan mode to become `enabled` at the next boundary, switching
    /// the permission mode to `permission_mode` when given.
    fn request_plan_mode(&self, enabled: bool, permission_mode: Option<String>);
}

/// A [`PlanModeControl`] that queues the change on the session's turn
/// boundary; [`crate::session_engine::engine::SessionEngine::consume_pending_plan_mode`]
/// applies it after the model's cell settles.
pub struct TurnBoundaryPlanControl {
    requests: Arc<super::turn_boundary::TurnBoundaryRequests>,
}

impl TurnBoundaryPlanControl {
    /// Over the session's turn-boundary requests.
    #[must_use]
    pub fn new(requests: Arc<super::turn_boundary::TurnBoundaryRequests>) -> Self {
        Self { requests }
    }
}

impl PlanModeControl for TurnBoundaryPlanControl {
    fn request_plan_mode(&self, enabled: bool, permission_mode: Option<String>) {
        self.requests
            .queue_plan_mode(super::turn_boundary::PlanModeRequest {
                enabled,
                permission_mode,
            });
    }
}

/// Whether the session is in plan mode, per the bridge's switch.
fn plan_mode_enabled(bridge: &RlmHostBridge) -> bool {
    bridge
        .plan_mode
        .get()
        .is_some_and(super::plan_mode::PlanModeSwitch::is_enabled)
}

/// Run `rlm.enter_plan_mode`: ask the user for consent and, on `enter`,
/// request plan mode on through `control`.
async fn enter(
    session_id: &str,
    already_on: bool,
    control: Option<&Arc<dyn PlanModeControl>>,
    timeout: Option<Duration>,
) -> anyhow::Result<Value> {
    if already_on {
        return Ok(json!({ "status": "already_on", "message": "Plan mode is already on." }));
    }
    let answer = ask_question(
        session_id,
        QUESTION_KIND_ENTER_PLAN_MODE,
        "waiting for your plan-mode decision",
        plan_mode::enter_plan_payload(),
        timeout,
    )
    .await;
    match answer {
        QuestionAnswer::Answered(answer) if plan_mode::enter_plan_accepted(&answer) => {
            require(control)?.request_plan_mode(true, None);
            Ok(json!({
                "status": "entered",
                "message": "Plan mode is on: investigate and produce a plan; file edits are blocked until the user approves a plan or turns plan mode off.",
            }))
        }
        QuestionAnswer::Answered(_) | QuestionAnswer::TimedOut(_) | QuestionAnswer::Declined => {
            Ok(json!({
                "status": "declined",
                "message": "The user declined plan mode. Continue working normally.",
            }))
        }
        QuestionAnswer::Unavailable => anyhow::bail!("{UNAVAILABLE_MESSAGE}"),
    }
}

/// Run `rlm.exit_plan_mode`: write the plan, ask for approval, and apply the
/// user's choice through `control`.
async fn exit(
    session_id: &str,
    in_plan_mode: bool,
    control: Option<&Arc<dyn PlanModeControl>>,
    plan_file: &PlanFile,
    plan: &str,
    timeout: Option<Duration>,
) -> anyhow::Result<Value> {
    if !in_plan_mode {
        anyhow::bail!("{NOT_IN_PLAN_MESSAGE}");
    }
    if plan.trim().is_empty() {
        anyhow::bail!(
            "rlm.exit_plan_mode needs a plan: pass the plan you want the user to approve."
        );
    }
    let path = plan_file
        .write(plan)
        .map_err(|error| anyhow::anyhow!("could not write the plan file: {error}"))?;
    let payload = plan_mode::plan_approval_payload(plan, &path.to_string_lossy());
    let answer = ask_question(
        session_id,
        QUESTION_KIND_PLAN_APPROVAL,
        "waiting for your plan approval",
        payload,
        timeout,
    )
    .await;
    let choice = match answer {
        QuestionAnswer::Answered(answer) => plan_mode::interpret_plan_exit_answer(&answer),
        // A dismissal or timeout keeps planning.
        QuestionAnswer::Declined | QuestionAnswer::TimedOut(_) => {
            PlanExitChoice::KeepPlanning { feedback: None }
        }
        QuestionAnswer::Unavailable => anyhow::bail!("{UNAVAILABLE_MESSAGE}"),
    };
    if choice.approved() {
        require(control)?.request_plan_mode(false, choice.permission_mode().map(str::to_string));
    }
    Ok(plan_mode::plan_exit_reply(&choice))
}

/// The bound controller, or the no-controller error.
fn require(
    control: Option<&Arc<dyn PlanModeControl>>,
) -> anyhow::Result<&Arc<dyn PlanModeControl>> {
    control.ok_or_else(|| anyhow::anyhow!("{NO_CONTROL_MESSAGE}"))
}

/// Register `plan.enter` and `plan.exit` for a session at `rlm_depth`
/// (a subagent is refused), writing plans to `plan_file` and waiting up to
/// `timeout` for the user.
pub(crate) fn register_host_handlers(
    handlers: &mut HostRequestHandlers,
    bridge: Arc<RlmHostBridge>,
    session_id: String,
    rlm_depth: u32,
    plan_file: PlanFile,
    timeout: Option<Duration>,
) {
    {
        let bridge = Arc::clone(&bridge);
        let session_id = session_id.clone();
        handlers.register(
            ENTER_PLAN_REQUEST,
            host_handler(move |_payload| {
                let bridge = Arc::clone(&bridge);
                let session_id = session_id.clone();
                Box::pin(async move {
                    if rlm_depth > 0 {
                        anyhow::bail!("{SUBAGENT_MESSAGE}");
                    }
                    enter(
                        &session_id,
                        plan_mode_enabled(&bridge),
                        bridge.plan_control.get(),
                        timeout,
                    )
                    .await
                })
            }),
        );
    }
    handlers.register(
        EXIT_PLAN_REQUEST,
        host_handler(move |payload| {
            let bridge = Arc::clone(&bridge);
            let session_id = session_id.clone();
            let plan_file = plan_file.clone();
            Box::pin(async move {
                if rlm_depth > 0 {
                    anyhow::bail!("{SUBAGENT_MESSAGE}");
                }
                let plan = match payload.data.get("plan") {
                    Some(Value::String(plan)) => plan.clone(),
                    Some(_) => anyhow::bail!("plan must be a string"),
                    None => anyhow::bail!("rlm.exit_plan_mode needs a plan argument"),
                };
                exit(
                    &session_id,
                    plan_mode_enabled(&bridge),
                    bridge.plan_control.get(),
                    &plan_file,
                    &plan,
                    timeout,
                )
                .await
            })
        }),
    );
}

#[cfg(test)]
#[path = "plan_host_tests.rs"]
mod tests;
