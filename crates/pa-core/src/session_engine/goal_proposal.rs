//! Model-proposed goals: the model proposes a completion condition and the
//! user approves it with one keypress.
//!
//! Claude Code 2.1.296's `ProposeGoal` tool (`wygp9s4p`, `fdbb8cx6`, gate
//! `tengu_propose_goal`, setting `modelProposedGoals`, default off) is
//! non-blocking: the proposal renders alongside the work, and approving it
//! sets the goal like `/goal` would (`tmrtnnjz:Uke`). Here `goal.propose`
//! (the goal skill's `goal.propose(condition)`) validates the condition,
//! returns at once, and asks the user through the session's confirmation
//! surface ([`crate::features::confirm`], the TUI's yes/no panel) in the
//! background; the answer reaches the model as a notice row (it wakes an
//! idle session). Off unless `experimental.modelProposedGoals` (or
//! `PRIME_AGENT_MODEL_PROPOSED_GOALS`) turns it on.

use std::sync::Arc;

use serde_json::{Value, json};

use super::goal_driver::GoalDriver;
use crate::features::{
    CONFIRMATION_TIMEOUT,
    ConfirmationAnswer,
    FeatureNotice,
    confirm,
    deliver_feature_notice,
};
use crate::goals::{GoalStatus, validate_goal_objective};
use crate::kernel::shared::{HostRequestHandlers, host_handler};
use crate::session::manager::SessionManager;

/// The host request the goal skill's `propose` sends.
pub const GOAL_PROPOSE_REQUEST: &str = "goal.propose";

/// The notices' feature name.
const NOTICE_FEATURE: &str = "goal";

/// What the user is asked (Claude Code's approval copy, `Uke`).
#[must_use]
pub fn approval_reason(condition: &str) -> String {
    format!(
        "The agent proposes this session goal: \"{condition}\". Approving sets it as the session goal, like running /goal: the agent keeps working until the condition is met."
    )
}

/// The notice the model reads once the user answered.
#[must_use]
pub fn answer_notice(
    condition: &str,
    answer: &ConfirmationAnswer,
    started: Result<(), String>,
) -> String {
    match (answer, started) {
        (ConfirmationAnswer::Allowed, Ok(())) => format!(
            "The user approved your proposed goal; it is now the session goal: \"{condition}\". Keep working toward it until the condition is met."
        ),
        (ConfirmationAnswer::Allowed, Err(error)) => format!(
            "The user approved your proposed goal \"{condition}\", but it could not be set: {error}"
        ),
        (ConfirmationAnswer::Denied, _) => format!(
            "The user declined your proposed goal \"{condition}\". Continue without it; do not propose it again unless the user asks."
        ),
        (ConfirmationAnswer::TimedOut(_), _) => {
            format!("Your proposed goal \"{condition}\" was not answered in time; it was not set.")
        }
        (ConfirmationAnswer::Unavailable, _) => format!(
            "Your proposed goal \"{condition}\" could not be shown to the user (no interactive client); it was not set."
        ),
    }
}

/// Why a goal cannot be proposed now, when it cannot.
fn refusal(driver: &GoalDriver) -> Option<&'static str> {
    match driver.state().status {
        GoalStatus::Active => {
            Some("cannot propose a goal because this thread already has an active goal")
        }
        GoalStatus::Paused | GoalStatus::BudgetLimited => Some(
            "cannot propose a goal because a paused or budget-limited goal exists; ask the user to resume or clear it",
        ),
        GoalStatus::Idle | GoalStatus::Complete | GoalStatus::Error => None,
    }
}

/// Settle one answered proposal: an approval starts the goal (unless one
/// started meanwhile).
async fn settle(
    driver: &tokio::sync::Mutex<GoalDriver>,
    session: &tokio::sync::Mutex<SessionManager>,
    condition: &str,
    answer: &ConfirmationAnswer,
) -> Result<(), String> {
    if *answer != ConfirmationAnswer::Allowed {
        return Ok(());
    }
    let mut driver = driver.lock().await;
    if let Some(refusal) = refusal(&driver) {
        return Err(refusal.to_string());
    }
    let mut session = session.lock().await;
    driver
        .start(&mut session, condition, None)
        .map(|_| ())
        .map_err(|error| format!("{error:#}"))
}

/// Register `goal.propose`; `enabled` is the session's
/// `experimental.modelProposedGoals`.
pub(crate) fn register_host_handlers(
    handlers: &mut HostRequestHandlers,
    enabled: bool,
    driver: Arc<tokio::sync::Mutex<GoalDriver>>,
    session: Arc<tokio::sync::Mutex<SessionManager>>,
    session_id: String,
) {
    handlers.register(
        GOAL_PROPOSE_REQUEST,
        host_handler(move |payload| {
            let driver = Arc::clone(&driver);
            let session = Arc::clone(&session);
            let session_id = session_id.clone();
            Box::pin(async move {
                if !enabled {
                    anyhow::bail!(
                        "goal proposals are off: the user enables them with the experimental.modelProposedGoals setting"
                    );
                }
                let Some(condition) = payload.data.get("condition").and_then(Value::as_str) else {
                    anyhow::bail!("goal.propose condition must be a string");
                };
                let condition = validate_goal_objective(condition)?;
                if let Some(refusal) = refusal(&*driver.lock().await) {
                    anyhow::bail!("{refusal}");
                }
                let proposed = condition.clone();
                tokio::spawn(async move {
                    let answer = confirm(
                        &session_id,
                        "Set the session goal",
                        &approval_reason(&proposed),
                        CONFIRMATION_TIMEOUT,
                    )
                    .await;
                    let started = settle(&driver, &session, &proposed, &answer).await;
                    let text = answer_notice(&proposed, &answer, started);
                    deliver_feature_notice(
                        &session_id,
                        &FeatureNotice {
                            feature: NOTICE_FEATURE.to_string(),
                            text,
                        },
                    );
                });
                Ok(json!({ "proposed": true, "condition": condition, "status": "awaiting_approval" }))
            })
        }),
    );
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::kernel::shared::HostRequestPayload;

    struct Fixture {
        handlers: HostRequestHandlers,
        driver: Arc<tokio::sync::Mutex<GoalDriver>>,
        notices: Arc<std::sync::Mutex<Vec<String>>>,
        _sink: crate::features::FeatureNoticeSink,
        _surface: Option<crate::features::ConfirmationSurface>,
        _dir: crate::test_support::ThreadTempDir,
    }

    fn fixture(session_id: &str, enabled: bool, surface: bool) -> Fixture {
        let dir = crate::test_support::ThreadTempDir::new();
        let session_dir = dir.path().join("session");
        std::fs::create_dir_all(&session_dir).unwrap();
        let mut session = SessionManager::in_memory(dir.path());
        session.materialize_session_file(Some(session_dir));
        let driver = Arc::new(tokio::sync::Mutex::new(GoalDriver::new()));
        let mut handlers = HostRequestHandlers::default();
        register_host_handlers(
            &mut handlers,
            enabled,
            Arc::clone(&driver),
            Arc::new(tokio::sync::Mutex::new(session)),
            session_id.to_string(),
        );
        let notices = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink: crate::features::FeatureNoticeSink = {
            let notices = Arc::clone(&notices);
            Arc::new(move |row: pa_types::session::CustomMessage| {
                if let pa_types::ai::UserContent::Text(text) = row.content {
                    notices.lock().unwrap().push(text);
                }
            })
        };
        crate::features::register_feature_notice_sink(session_id, &sink);
        Fixture {
            handlers,
            driver,
            notices,
            _sink: sink,
            _surface: surface.then(|| crate::features::register_confirmation_surface(session_id)),
            _dir: dir,
        }
    }

    async fn propose(fixture: &Fixture, condition: &str) -> anyhow::Result<Value> {
        let handler = fixture.handlers.get(GOAL_PROPOSE_REQUEST).unwrap().clone();
        handler(HostRequestPayload {
            data: json!({ "type": GOAL_PROPOSE_REQUEST, "condition": condition }),
        })
        .await
    }

    async fn wait_for_notice(fixture: &Fixture) -> String {
        for _ in 0..200 {
            if let Some(text) = fixture.notices.lock().unwrap().first().cloned() {
                return text;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("no notice arrived");
    }

    async fn answer_pending(session_id: &str, allow: bool) {
        for _ in 0..200 {
            if let Some(pending) = crate::features::pending_confirmations(session_id).first() {
                assert!(crate::features::answer_confirmation(
                    session_id,
                    &pending.id,
                    allow
                ));
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("no confirmation was asked");
    }

    #[tokio::test]
    async fn off_by_default_the_request_is_refused() {
        let fixture = fixture("propose-off", false, true);
        let error = propose(&fixture, "tests pass").await.unwrap_err();
        assert!(error.to_string().starts_with("goal proposals are off"));
    }

    #[tokio::test]
    async fn an_approved_proposal_starts_the_goal_and_tells_the_model() {
        let session_id = "propose-approved";
        let fixture = fixture(session_id, true, true);
        let reply = propose(&fixture, "all tests pass").await.unwrap();
        assert_eq!(
            reply,
            json!({ "proposed": true, "condition": "all tests pass", "status": "awaiting_approval" })
        );
        let pending = {
            answer_pending(session_id, true).await;
            wait_for_notice(&fixture).await
        };
        assert_eq!(
            pending,
            answer_notice("all tests pass", &ConfirmationAnswer::Allowed, Ok(()))
        );
        let driver = fixture.driver.lock().await;
        assert_eq!(driver.state().status, GoalStatus::Active);
        assert_eq!(driver.state().objective.as_deref(), Some("all tests pass"));
    }

    #[tokio::test]
    async fn a_declined_proposal_leaves_no_goal() {
        let session_id = "propose-declined";
        let fixture = fixture(session_id, true, true);
        propose(&fixture, "ship it").await.unwrap();
        answer_pending(session_id, false).await;
        assert_eq!(
            wait_for_notice(&fixture).await,
            answer_notice("ship it", &ConfirmationAnswer::Denied, Ok(()))
        );
        assert_eq!(fixture.driver.lock().await.state().status, GoalStatus::Idle);
    }

    #[tokio::test]
    async fn without_a_surface_the_model_hears_it_could_not_be_asked() {
        let fixture = fixture("propose-headless", true, false);
        propose(&fixture, "ship it").await.unwrap();
        assert_eq!(
            wait_for_notice(&fixture).await,
            answer_notice("ship it", &ConfirmationAnswer::Unavailable, Ok(()))
        );
    }
}
