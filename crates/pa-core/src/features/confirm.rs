//! Confirmations: a feature review that answers [`super::FeatureVerdict::Ask`]
//! puts the question to the user through the session's confirmation
//! surface. The daemon worker registers a surface for its session; print
//! mode registers none, so there an `Ask` is refused at once. Pending
//! questions are published as the [`CONFIRMATION_FEATURE`] status, which a
//! client renders as a yes/no prompt (a client attaching later sees it on
//! the roster summary) and answers with the daemon's
//! `answer_feature_confirmation` command ([`answer_confirmation`]).
//!
//! A question may offer to remember the answer ([`confirm_offering`]): the
//! request carries the offer's label (a permission rule, say), a client
//! shows a third choice for it, and an answer that takes it ends the
//! question as [`ConfirmationAnswer::AllowedAlways`]; what remembering means
//! is the asker's business.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Duration;

use serde::Serialize;
use tokio::sync::oneshot;

use super::status::{FeatureStatus, publish_feature_status};

/// The feature status pending confirmations publish under.
pub const CONFIRMATION_FEATURE: &str = "confirmation";

/// How long an action waits for the user before it is refused: the agent
/// is blocked meanwhile, and a session nobody watches must not hang.
pub const CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(300);

/// One question to the user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfirmationRequest {
    /// Unique within the process; the answer names it.
    pub id: String,
    /// What would run: the tool and its arguments, or a `bash.run`'s
    /// command.
    pub action: String,
    /// Why the user is asked: the asking features' reasons.
    pub reason: String,
    /// What allowing it for good would remember (a permission rule, say);
    /// `None` when the asker offers only yes or no.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remember: Option<String>,
}

/// How a confirmation ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmationAnswer {
    Allowed,
    /// Allowed, and the user took the question's offer to remember it
    /// ([`ConfirmationRequest::remember`]); only a question that offered it
    /// ends this way.
    AllowedAlways,
    Denied,
    /// The session has no confirmation surface (print mode), or it was
    /// withdrawn while the question was pending.
    Unavailable,
    /// Nobody answered within the timeout.
    TimedOut(Duration),
}

/// The user's answer to one question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    Deny,
    Allow,
    AllowAlways,
}

struct Pending {
    request: ConfirmationRequest,
    answer: oneshot::Sender<Answer>,
}

#[derive(Default)]
struct SessionConfirmations {
    /// Registered surfaces; questions are asked only while one is.
    surfaces: usize,
    /// Pending questions, oldest first.
    pending: Vec<Pending>,
}

type Sessions = Mutex<HashMap<String, SessionConfirmations>>;

fn sessions() -> &'static Sessions {
    static SESSIONS: OnceLock<Sessions> = OnceLock::new();
    SESSIONS.get_or_init(Sessions::default)
}

fn lock() -> std::sync::MutexGuard<'static, HashMap<String, SessionConfirmations>> {
    sessions().lock().unwrap_or_else(PoisonError::into_inner)
}

/// Publish `session_id`'s pending questions. Called with the registry lock
/// held, so concurrent changes publish in the order they were made.
fn publish(session_id: &str, session: Option<&SessionConfirmations>) {
    let pending: Vec<&ConfirmationRequest> = session
        .map(|session| {
            session
                .pending
                .iter()
                .map(|pending| &pending.request)
                .collect()
        })
        .unwrap_or_default();
    let line = pending
        .first()
        .map(|request| format!("waiting for your confirmation: {}", request.action));
    publish_feature_status(
        session_id,
        FeatureStatus {
            feature: CONFIRMATION_FEATURE.to_string(),
            line,
            status: serde_json::json!({ "confirmations": pending }),
        },
    );
}

/// A session's confirmation surface, registered while the guard lives.
/// Dropping the last one withdraws the session's pending questions, which
/// end as [`ConfirmationAnswer::Unavailable`].
#[must_use = "the surface is withdrawn when the guard drops"]
pub struct ConfirmationSurface {
    session_id: String,
}

impl std::fmt::Debug for ConfirmationSurface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfirmationSurface")
            .field("session_id", &self.session_id)
            .finish()
    }
}

impl Drop for ConfirmationSurface {
    fn drop(&mut self) {
        let mut sessions = lock();
        let Some(session) = sessions.get_mut(&self.session_id) else {
            return;
        };
        session.surfaces = session.surfaces.saturating_sub(1);
        if session.surfaces == 0 {
            let withdrawn = sessions.remove(&self.session_id);
            if withdrawn.is_some_and(|session| !session.pending.is_empty()) {
                publish(&self.session_id, None);
            }
        }
    }
}

/// Let `session_id`'s features ask the user while the returned guard lives
/// (the embedding has a client surface that renders the questions).
pub fn register_confirmation_surface(session_id: &str) -> ConfirmationSurface {
    lock().entry(session_id.to_string()).or_default().surfaces += 1;
    ConfirmationSurface {
        session_id: session_id.to_string(),
    }
}

/// Withdraws a pending question when its asker stops waiting (answered,
/// timed out, or the action was aborted and the future dropped).
struct Withdraw<'a> {
    session_id: &'a str,
    id: String,
}

impl Drop for Withdraw<'_> {
    fn drop(&mut self) {
        let mut sessions = lock();
        let Some(session) = sessions.get_mut(self.session_id) else {
            return;
        };
        let before = session.pending.len();
        session
            .pending
            .retain(|pending| pending.request.id != self.id);
        if session.pending.len() != before {
            publish(self.session_id, Some(session));
        }
    }
}

/// Ask the user whether `action` may run, because of `reason`, and wait up
/// to `timeout` for the answer.
pub async fn confirm(
    session_id: &str,
    action: &str,
    reason: &str,
    timeout: Duration,
) -> ConfirmationAnswer {
    confirm_offering(session_id, action, reason, None, timeout).await
}

/// [`confirm`], offering to remember the answer as `remember` (shown to the
/// user as a third choice): taking it ends the question as
/// [`ConfirmationAnswer::AllowedAlways`].
pub async fn confirm_offering(
    session_id: &str,
    action: &str,
    reason: &str,
    remember: Option<&str>,
    timeout: Duration,
) -> ConfirmationAnswer {
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    let id = format!("confirm-{}", NEXT_ID.fetch_add(1, Ordering::Relaxed));
    let (answer, answered) = oneshot::channel();
    {
        let mut sessions = lock();
        let Some(session) = sessions
            .get_mut(session_id)
            .filter(|session| session.surfaces > 0)
        else {
            return ConfirmationAnswer::Unavailable;
        };
        session.pending.push(Pending {
            request: ConfirmationRequest {
                id: id.clone(),
                action: action.to_string(),
                reason: reason.to_string(),
                remember: remember.map(str::to_string),
            },
            answer,
        });
        publish(session_id, Some(session));
    }
    let _withdraw = Withdraw { session_id, id };
    match tokio::time::timeout(timeout, answered).await {
        Ok(Ok(Answer::Allow)) => ConfirmationAnswer::Allowed,
        Ok(Ok(Answer::AllowAlways)) => ConfirmationAnswer::AllowedAlways,
        Ok(Ok(Answer::Deny)) => ConfirmationAnswer::Denied,
        Ok(Err(_)) => ConfirmationAnswer::Unavailable,
        Err(_) => ConfirmationAnswer::TimedOut(timeout),
    }
}

/// Answer `session_id`'s pending question `id`; `false` when no such
/// question is pending (already answered, timed out, or withdrawn).
#[must_use = "`false` means the question was no longer pending"]
pub fn answer_confirmation(session_id: &str, id: &str, allow: bool) -> bool {
    answer_confirmation_remembering(session_id, id, allow, false)
}

/// [`answer_confirmation`], taking the question's offer to remember the
/// answer when `remember` (and `allow`) is set; a question without an offer
/// is plainly allowed.
#[must_use = "`false` means the question was no longer pending"]
pub fn answer_confirmation_remembering(
    session_id: &str,
    id: &str,
    allow: bool,
    remember: bool,
) -> bool {
    let mut sessions = lock();
    let Some(session) = sessions.get_mut(session_id) else {
        return false;
    };
    let Some(index) = session
        .pending
        .iter()
        .position(|pending| pending.request.id == id)
    else {
        return false;
    };
    let pending = session.pending.remove(index);
    publish(session_id, Some(session));
    let answer = match (allow, remember && pending.request.remember.is_some()) {
        (false, _) => Answer::Deny,
        (true, false) => Answer::Allow,
        (true, true) => Answer::AllowAlways,
    };
    // The asker may have stopped waiting a moment ago; the question is
    // answered either way.
    let _ = pending.answer.send(answer);
    true
}

/// `session_id`'s pending questions, oldest first.
#[must_use]
pub fn pending_confirmations(session_id: &str) -> Vec<ConfirmationRequest> {
    lock().get(session_id).map_or_else(Vec::new, |session| {
        session
            .pending
            .iter()
            .map(|pending| pending.request.clone())
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    /// Collects the session's `confirmation` statuses.
    fn record(
        session_id: &str,
    ) -> (
        Arc<Mutex<Vec<FeatureStatus>>>,
        super::super::FeatureStatusSink,
    ) {
        let seen: Arc<Mutex<Vec<FeatureStatus>>> = Arc::default();
        let sink: super::super::FeatureStatusSink = {
            let seen = Arc::clone(&seen);
            Arc::new(move |status| seen.lock().unwrap().push(status))
        };
        super::super::register_feature_status_sink(session_id, &sink);
        (seen, sink)
    }

    fn pending_ids(status: &FeatureStatus) -> Vec<String> {
        status.status["confirmations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|request| request["id"].as_str().unwrap().to_string())
            .collect()
    }

    #[tokio::test]
    async fn without_a_surface_nobody_is_asked() {
        assert_eq!(
            confirm(
                "confirm-no-surface",
                "Run `x`",
                "why",
                Duration::from_secs(5)
            )
            .await,
            ConfirmationAnswer::Unavailable
        );
        assert!(pending_confirmations("confirm-no-surface").is_empty());
    }

    #[tokio::test]
    async fn a_pending_question_is_published_answered_and_withdrawn() {
        let session = "confirm-answered";
        let (seen, _sink) = record(session);
        let surface = register_confirmation_surface(session);
        let asking = tokio::spawn(async move {
            confirm(
                session,
                "Run `rm -rf build`",
                "hook asks",
                Duration::from_secs(30),
            )
            .await
        });
        let request = loop {
            if let Some(request) = pending_confirmations(session).pop() {
                break request;
            }
            tokio::task::yield_now().await;
        };
        assert_eq!(request.action, "Run `rm -rf build`");
        assert_eq!(request.reason, "hook asks");
        assert!(!answer_confirmation(session, "confirm-unknown", true));
        assert!(answer_confirmation(session, &request.id, true));
        assert_eq!(asking.await.unwrap(), ConfirmationAnswer::Allowed);
        // A second answer finds nothing pending.
        assert!(!answer_confirmation(session, &request.id, false));
        let statuses = seen.lock().unwrap().clone();
        assert_eq!(statuses.len(), 2, "{statuses:?}");
        assert_eq!(statuses[0].feature, CONFIRMATION_FEATURE);
        assert_eq!(pending_ids(&statuses[0]), vec![request.id.clone()]);
        assert_eq!(
            statuses[0].line.as_deref(),
            Some("waiting for your confirmation: Run `rm -rf build`")
        );
        assert!(pending_ids(&statuses[1]).is_empty());
        assert_eq!(statuses[1].line, None);
        drop(surface);
    }

    #[tokio::test]
    async fn a_question_times_out_is_denied_or_ends_with_its_surface() {
        let session = "confirm-lifecycle";
        let surface = register_confirmation_surface(session);
        assert_eq!(
            confirm(session, "a", "r", Duration::from_millis(20)).await,
            ConfirmationAnswer::TimedOut(Duration::from_millis(20))
        );
        assert!(pending_confirmations(session).is_empty());

        let denied = tokio::spawn(confirm(session, "b", "r", Duration::from_secs(30)));
        let id = loop {
            if let Some(request) = pending_confirmations(session).pop() {
                break request.id;
            }
            tokio::task::yield_now().await;
        };
        assert!(answer_confirmation(session, &id, false));
        assert_eq!(denied.await.unwrap(), ConfirmationAnswer::Denied);

        let withdrawn = tokio::spawn(confirm(session, "c", "r", Duration::from_secs(30)));
        while pending_confirmations(session).is_empty() {
            tokio::task::yield_now().await;
        }
        drop(surface);
        assert_eq!(withdrawn.await.unwrap(), ConfirmationAnswer::Unavailable);
        assert!(pending_confirmations(session).is_empty());
    }

    #[tokio::test]
    async fn an_offer_to_remember_is_published_and_taken_only_when_offered() {
        let session = "confirm-remember";
        let (seen, _sink) = record(session);
        let _surface = register_confirmation_surface(session);
        let next = || async {
            loop {
                if let Some(request) = pending_confirmations(session).pop() {
                    break request;
                }
                tokio::task::yield_now().await;
            }
        };
        let offering = tokio::spawn(confirm_offering(
            session,
            "Run `npm test`",
            "rule asks",
            Some("Bash(npm test *)"),
            Duration::from_secs(30),
        ));
        let request = next().await;
        assert_eq!(request.remember.as_deref(), Some("Bash(npm test *)"));
        let published = seen.lock().unwrap().last().cloned().unwrap();
        assert_eq!(
            published.status["confirmations"][0]["remember"],
            "Bash(npm test *)"
        );
        assert!(answer_confirmation_remembering(
            session,
            &request.id,
            true,
            true
        ));
        assert_eq!(offering.await.unwrap(), ConfirmationAnswer::AllowedAlways);

        // Without an offer, remembering is a plain allow; a refusal ignores it.
        let plain = tokio::spawn(confirm(session, "a", "r", Duration::from_secs(30)));
        let request = next().await;
        assert_eq!(request.remember, None);
        assert!(
            serde_json::to_value(&request)
                .unwrap()
                .get("remember")
                .is_none()
        );
        assert!(answer_confirmation_remembering(
            session,
            &request.id,
            true,
            true
        ));
        assert_eq!(plain.await.unwrap(), ConfirmationAnswer::Allowed);
        let refused = tokio::spawn(confirm_offering(
            session,
            "b",
            "r",
            Some("Bash"),
            Duration::from_secs(30),
        ));
        let request = next().await;
        assert!(answer_confirmation_remembering(
            session,
            &request.id,
            false,
            true
        ));
        assert_eq!(refused.await.unwrap(), ConfirmationAnswer::Denied);
    }

    #[tokio::test]
    async fn an_abandoned_question_is_withdrawn() {
        let session = "confirm-abandoned";
        let _surface = register_confirmation_surface(session);
        let asking = tokio::spawn(confirm(session, "d", "r", Duration::from_secs(30)));
        while pending_confirmations(session).is_empty() {
            tokio::task::yield_now().await;
        }
        asking.abort();
        let _ = asking.await;
        assert!(pending_confirmations(session).is_empty());
    }
}
