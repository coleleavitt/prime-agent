//! Structured questions to the user on the session's confirmation surface
//! ([`super::confirm`]): the model's multiple-choice questions
//! (`rlm.ask_user`, Claude Code's `AskUserQuestion`), the plan approval
//! (`rlm.exit_plan_mode`) and the plan-mode entry consent
//! (`rlm.enter_plan_mode`). A question waits in the same per-session queue
//! as the yes/no confirmations, publishes under the `confirmation` status's
//! `questions` key (`{id, kind, payload}`, oldest first; a client that does
//! not know a kind leaves it to another client or the timeout), and is
//! answered by the daemon's `answer_feature_confirmation` command: `allow:
//! false` declines it, `allow: true` with an `answer` object answers it
//! (the `feature_questions` capability).

use std::time::Duration;

use serde::Serialize;

use super::confirm::{
    Answer,
    PendingRequest,
    Unanswered,
    ask_pending,
    next_pending_id,
    pending_requests,
};

/// The model's multiple-choice questions (`payload`: `{title?, questions}`
/// in Claude Code's `AskUserQuestion` shape).
pub const QUESTION_KIND_ASK_USER: &str = "ask_user";
/// The plan approval (`payload`: `{plan, planFilePath}`); the answer is
/// `{choice: "acceptEdits" | "default" | "keepPlanning", feedback?, plan?}`.
pub const QUESTION_KIND_PLAN_APPROVAL: &str = "plan_approval";
/// The model's request to enter plan mode (`payload`: `{}`); the answer is
/// `{choice: "enter" | "decline"}`.
pub const QUESTION_KIND_ENTER_PLAN_MODE: &str = "enter_plan_mode";

/// One structured question.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuestionRequest {
    /// Unique within the process; the answer names it.
    pub id: String,
    /// Which question this is (a `QUESTION_KIND_*`): what the client shows
    /// and what answer it sends.
    pub kind: String,
    /// The kind's content.
    pub payload: serde_json::Value,
    /// The session's status line while this is the oldest pending item.
    #[serde(skip)]
    pub line: String,
}

/// How a question ended.
#[derive(Debug, Clone, PartialEq)]
pub enum QuestionAnswer {
    /// The client's answer object.
    Answered(serde_json::Value),
    /// The user dismissed the question.
    Declined,
    /// The session has no confirmation surface (print mode), or it was
    /// withdrawn while the question was pending.
    Unavailable,
    /// Nobody answered within the timeout.
    TimedOut(Duration),
}

/// Put a `kind` question with `payload` to the user of `session_id`, with
/// `line` as the session's status line meanwhile, and wait for the answer:
/// up to `timeout`, or until it is answered, the surface goes or the asker
/// stops waiting when `None`.
pub async fn ask_question(
    session_id: &str,
    kind: &str,
    line: &str,
    payload: serde_json::Value,
    timeout: Option<Duration>,
) -> QuestionAnswer {
    let request = PendingRequest::Question(QuestionRequest {
        id: next_pending_id("question"),
        kind: kind.to_string(),
        payload,
        line: line.to_string(),
    });
    match ask_pending(session_id, request, timeout).await {
        Ok(Answer::Respond(answer)) => QuestionAnswer::Answered(answer),
        // `answer_pending` answers a question only with `Respond` or `Deny`.
        Ok(Answer::Deny | Answer::Allow | Answer::AllowAlways) => QuestionAnswer::Declined,
        Err(Unanswered::Unavailable) => QuestionAnswer::Unavailable,
        Err(Unanswered::TimedOut(timeout)) => QuestionAnswer::TimedOut(timeout),
    }
}

/// `session_id`'s pending questions, oldest first.
#[must_use]
pub fn pending_questions(session_id: &str) -> Vec<QuestionRequest> {
    pending_requests(session_id)
        .into_iter()
        .filter_map(|request| match request {
            PendingRequest::Question(request) => Some(request),
            PendingRequest::Confirmation(_) => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::*;
    use crate::features::{
        FeatureStatus,
        FeatureStatusSink,
        answer_pending,
        confirm,
        pending_confirmations,
        register_confirmation_surface,
        register_feature_status_sink,
    };

    async fn next_question(session: &str) -> QuestionRequest {
        loop {
            if let Some(question) = pending_questions(session).pop() {
                return question;
            }
            tokio::task::yield_now().await;
        }
    }

    #[tokio::test]
    async fn without_a_surface_a_question_is_unavailable() {
        assert_eq!(
            ask_question("question-no-surface", "ask_user", "l", json!({}), None).await,
            QuestionAnswer::Unavailable
        );
    }

    #[tokio::test]
    async fn a_question_publishes_beside_confirmations_and_takes_an_answer_object() {
        let session = "question-answered";
        let seen: Arc<Mutex<Vec<FeatureStatus>>> = Arc::default();
        let sink: FeatureStatusSink = {
            let seen = Arc::clone(&seen);
            Arc::new(move |status| seen.lock().unwrap().push(status))
        };
        register_feature_status_sink(session, &sink);
        let _surface = register_confirmation_surface(session);
        let asking = tokio::spawn(ask_question(
            session,
            QUESTION_KIND_ASK_USER,
            "waiting for your answer: Library",
            json!({ "questions": [] }),
            None,
        ));
        let question = next_question(session).await;
        assert_eq!(question.kind, QUESTION_KIND_ASK_USER);
        // A yes/no confirmation joins the same queue, behind the question.
        let confirming = tokio::spawn(confirm(session, "Run `x`", "r", Duration::from_secs(30)));
        while pending_confirmations(session).is_empty() {
            tokio::task::yield_now().await;
        }
        let published = seen.lock().unwrap().last().cloned().unwrap();
        assert_eq!(
            published.status["questions"],
            json!([{ "id": question.id, "kind": "ask_user", "payload": { "questions": [] } }])
        );
        assert_eq!(published.status["confirmations"][0]["action"], "Run `x`");
        assert_eq!(
            published.line.as_deref(),
            Some("waiting for your answer: Library")
        );
        assert!(answer_pending(
            session,
            &question.id,
            true,
            false,
            Some(json!({ "answers": { "Which?": "A" } }))
        ));
        assert_eq!(
            asking.await.unwrap(),
            QuestionAnswer::Answered(json!({ "answers": { "Which?": "A" } }))
        );
        // The confirmation is now the oldest: its line shows.
        let published = seen.lock().unwrap().last().cloned().unwrap();
        assert_eq!(
            published.line.as_deref(),
            Some("waiting for your confirmation: Run `x`")
        );
        let id = pending_confirmations(session)[0].id.clone();
        // An answer object on a confirmation is a plain allow.
        assert!(answer_pending(session, &id, true, false, Some(json!({}))));
        assert_eq!(
            confirming.await.unwrap(),
            crate::features::ConfirmationAnswer::Allowed
        );
    }

    #[tokio::test]
    async fn a_question_is_declined_times_out_or_ends_with_its_surface() {
        let session = "question-lifecycle";
        let surface = register_confirmation_surface(session);
        let declined = tokio::spawn(ask_question(session, "k", "l", json!({}), None));
        let question = next_question(session).await;
        assert!(answer_pending(session, &question.id, false, false, None));
        assert_eq!(declined.await.unwrap(), QuestionAnswer::Declined);

        let empty = tokio::spawn(ask_question(session, "k", "l", json!({}), None));
        let question = next_question(session).await;
        assert!(answer_pending(session, &question.id, true, false, None));
        assert_eq!(empty.await.unwrap(), QuestionAnswer::Answered(json!({})));

        assert_eq!(
            ask_question(
                session,
                "k",
                "l",
                json!({}),
                Some(Duration::from_millis(20))
            )
            .await,
            QuestionAnswer::TimedOut(Duration::from_millis(20))
        );
        assert!(pending_questions(session).is_empty());

        let withdrawn = tokio::spawn(ask_question(session, "k", "l", json!({}), None));
        next_question(session).await;
        drop(surface);
        assert_eq!(withdrawn.await.unwrap(), QuestionAnswer::Unavailable);
    }
}
