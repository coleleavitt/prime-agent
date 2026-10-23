use std::sync::{Arc, Mutex};

use serde_json::json;

use super::*;
use crate::features::{answer_pending, pending_questions, register_confirmation_surface};

/// Records the plan-mode changes a handler requests.
#[derive(Default)]
struct FakeControl {
    requests: Mutex<Vec<(bool, Option<String>)>>,
}

impl PlanModeControl for FakeControl {
    fn request_plan_mode(&self, enabled: bool, permission_mode: Option<String>) {
        self.requests
            .lock()
            .unwrap()
            .push((enabled, permission_mode));
    }
}

fn plan_file(session: &str) -> PlanFile {
    let dir = std::env::temp_dir().join(format!("pa-plan-host-{}-{session}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    PlanFile::resolve(None, &dir.join("project"), &dir.join("agent"), session)
}

async fn next_question(session: &str) -> String {
    loop {
        if let Some(question) = pending_questions(session).pop() {
            return question.id;
        }
        tokio::task::yield_now().await;
    }
}

#[tokio::test]
async fn entering_plan_mode_needs_consent_and_a_surface() {
    let control: Arc<dyn PlanModeControl> = Arc::new(FakeControl::default());
    // No surface: unavailable.
    assert_eq!(
        enter("plan-no-surface", false, Some(&control), None)
            .await
            .unwrap_err()
            .to_string(),
        UNAVAILABLE_MESSAGE
    );

    let session = "plan-enter";
    let _surface = register_confirmation_surface(session);
    let control = Arc::new(FakeControl::default());
    let dyn_control: Arc<dyn PlanModeControl> = control.clone();
    let asking = tokio::spawn(async move { enter(session, false, Some(&dyn_control), None).await });
    let id = next_question(session).await;
    assert!(answer_pending(
        session,
        &id,
        true,
        false,
        Some(json!({ "choice": "enter" }))
    ));
    let reply = asking.await.unwrap().unwrap();
    assert_eq!(reply["status"], "entered");
    assert_eq!(control.requests.lock().unwrap().as_slice(), [(true, None)]);
}

#[tokio::test]
async fn entering_when_already_on_is_a_no_op() {
    let control: Arc<dyn PlanModeControl> = Arc::new(FakeControl::default());
    let reply = enter("plan-on", true, Some(&control), None).await.unwrap();
    assert_eq!(reply["status"], "already_on");
}

#[tokio::test]
async fn declining_consent_does_not_enter() {
    let session = "plan-decline";
    let _surface = register_confirmation_surface(session);
    let control = Arc::new(FakeControl::default());
    let dyn_control: Arc<dyn PlanModeControl> = control.clone();
    let asking = tokio::spawn(async move { enter(session, false, Some(&dyn_control), None).await });
    let id = next_question(session).await;
    assert!(answer_pending(
        session,
        &id,
        true,
        false,
        Some(json!({ "choice": "decline" }))
    ));
    assert_eq!(asking.await.unwrap().unwrap()["status"], "declined");
    assert!(control.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn exiting_outside_plan_mode_is_refused() {
    let control: Arc<dyn PlanModeControl> = Arc::new(FakeControl::default());
    let file = plan_file("plan-exit-off");
    assert_eq!(
        exit(
            "plan-exit-off",
            false,
            Some(&control),
            &file,
            "# Plan",
            None
        )
        .await
        .unwrap_err()
        .to_string(),
        NOT_IN_PLAN_MESSAGE
    );
}

#[tokio::test]
async fn approving_the_plan_writes_the_file_and_turns_plan_mode_off() {
    let session = "plan-exit-accept";
    let _surface = register_confirmation_surface(session);
    let control = Arc::new(FakeControl::default());
    let dyn_control: Arc<dyn PlanModeControl> = control.clone();
    let file = plan_file(session);
    let file_for_task = file.clone();
    let asking = tokio::spawn(async move {
        exit(
            session,
            true,
            Some(&dyn_control),
            &file_for_task,
            "# Plan\n\nShip it.",
            None,
        )
        .await
    });
    let id = next_question(session).await;
    assert!(answer_pending(
        session,
        &id,
        true,
        false,
        Some(json!({ "choice": "acceptEdits" }))
    ));
    let reply = asking.await.unwrap().unwrap();
    assert_eq!(reply["status"], "approved");
    assert_eq!(reply["mode"], "acceptEdits");
    assert_eq!(file.read().as_deref(), Some("# Plan\n\nShip it."));
    assert_eq!(
        control.requests.lock().unwrap().as_slice(),
        [(false, Some("acceptEdits".to_string()))]
    );
    let _ = file.clear();
}

#[tokio::test]
async fn keeping_planning_sends_feedback_and_leaves_plan_mode_on() {
    let session = "plan-exit-keep";
    let _surface = register_confirmation_surface(session);
    let control = Arc::new(FakeControl::default());
    let dyn_control: Arc<dyn PlanModeControl> = control.clone();
    let file = plan_file(session);
    let file_for_task = file.clone();
    let asking = tokio::spawn(async move {
        exit(
            session,
            true,
            Some(&dyn_control),
            &file_for_task,
            "# Plan",
            None,
        )
        .await
    });
    let id = next_question(session).await;
    assert!(answer_pending(
        session,
        &id,
        true,
        false,
        Some(json!({ "choice": "keepPlanning", "feedback": "add tests" }))
    ));
    let reply = asking.await.unwrap().unwrap();
    assert_eq!(reply["status"], "keep_planning");
    assert_eq!(reply["feedback"], "add tests");
    assert!(control.requests.lock().unwrap().is_empty());
    let _ = file.clear();
}

#[tokio::test]
async fn a_missing_controller_reports_it() {
    let session = "plan-no-control";
    let _surface = register_confirmation_surface(session);
    let asking = tokio::spawn(async move { enter(session, false, None, None).await });
    let id = next_question(session).await;
    assert!(answer_pending(
        session,
        &id,
        true,
        false,
        Some(json!({ "choice": "enter" }))
    ));
    assert_eq!(
        asking.await.unwrap().unwrap_err().to_string(),
        NO_CONTROL_MESSAGE
    );
}
