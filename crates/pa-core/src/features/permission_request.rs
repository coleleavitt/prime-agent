//! Permission requests: before a tool call or kernel host request is put to
//! the user ([`super::confirm()`]), a feature may answer for them (Claude
//! Code's `PermissionRequest` hooks). The policy seam scopes each review to
//! the action it is about ([`with_confirmation_subject`]), so a confirmation
//! asked while it runs (a feature's `Ask`, a permission rule's ask) knows
//! which tool call or host request it is for; a confirmation outside that
//! scope (a project MCP server's approval, a goal proposal) is never
//! answered by a reviewer.
//!
//! A reviewer is registered per session ([`register_permission_reviewer`])
//! and held weakly, like the other per-session sinks.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};

use serde_json::Value;

use super::FeatureFuture;

/// The action a confirmation is about.
#[derive(Debug, Clone, PartialEq)]
pub enum ConfirmationSubject {
    /// A tool call of the model's.
    ToolCall { tool_name: String, args: Value },
    /// A kernel host request (`bash.run`, `mcp.session.call_tool`, ...).
    HostRequest { request_type: String, data: Value },
}

/// A permission request a reviewer sees.
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionRequestObservation {
    /// What would run, as the user would be shown it.
    pub action: String,
    /// Why the user would be asked.
    pub reason: String,
    /// What allowing it for good would remember (a permission rule), when
    /// the asker offers it: Claude Code's `permission_suggestions`.
    pub remember: Option<String>,
    pub subject: ConfirmationSubject,
}

/// A reviewer's answer for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionReview {
    /// Allow it without asking.
    Allow,
    /// Refuse it without asking, with the reason the model reads.
    Deny(String),
}

/// Answers a session's permission requests; `None` lets the user be asked.
pub type PermissionReviewer = Arc<
    dyn Fn(&PermissionRequestObservation) -> FeatureFuture<Option<PermissionReview>> + Send + Sync,
>;

type Reviewers = Mutex<
    HashMap<
        String,
        Weak<
            dyn Fn(&PermissionRequestObservation) -> FeatureFuture<Option<PermissionReview>>
                + Send
                + Sync,
        >,
    >,
>;

fn reviewers() -> &'static Reviewers {
    static REVIEWERS: OnceLock<Reviewers> = OnceLock::new();
    REVIEWERS.get_or_init(Reviewers::default)
}

tokio::task_local! {
    static SUBJECT: ConfirmationSubject;
}

/// Let `reviewer` answer session `session_id`'s permission requests while
/// the caller keeps it alive (the registry holds it weakly); replaces an
/// earlier reviewer.
pub fn register_permission_reviewer(session_id: &str, reviewer: &PermissionReviewer) {
    let mut reviewers = reviewers().lock().unwrap_or_else(PoisonError::into_inner);
    reviewers.retain(|_, reviewer| reviewer.strong_count() > 0);
    reviewers.insert(session_id.to_string(), Arc::downgrade(reviewer));
}

/// Run `review` with `subject` as the action any confirmation it asks is
/// about.
pub async fn with_confirmation_subject<T>(
    subject: ConfirmationSubject,
    review: impl std::future::Future<Output = T>,
) -> T {
    SUBJECT.scope(subject, review).await
}

/// The session reviewer's answer for a confirmation about to be asked;
/// `None` (no reviewer, no subject in scope, or no answer) asks the user.
pub(super) async fn review(
    session_id: &str,
    action: &str,
    reason: &str,
    remember: Option<&str>,
) -> Option<PermissionReview> {
    let subject = SUBJECT.try_with(Clone::clone).ok()?;
    let reviewer = reviewers()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(session_id)
        .and_then(Weak::upgrade)?;
    reviewer(&PermissionRequestObservation {
        action: action.to_string(),
        reason: reason.to_string(),
        remember: remember.map(str::to_string),
        subject,
    })
    .await
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::super::{ConfirmationAnswer, confirm, confirm_offering};
    use super::*;

    #[tokio::test]
    async fn a_reviewer_answers_only_inside_a_subject_scope() {
        let session = "permission-review";
        let seen: Arc<Mutex<Vec<PermissionRequestObservation>>> = Arc::default();
        let reviewer: PermissionReviewer = {
            let seen = Arc::clone(&seen);
            Arc::new(move |request| {
                seen.lock().unwrap().push(request.clone());
                let answer = if request.action.contains("rm") {
                    Some(PermissionReview::Deny("no deletes".to_string()))
                } else if request.action.contains("ls") {
                    Some(PermissionReview::Allow)
                } else {
                    None
                };
                Box::pin(async move { answer })
            })
        };
        register_permission_reviewer(session, &reviewer);
        let subject = ConfirmationSubject::HostRequest {
            request_type: "bash.run".to_string(),
            data: serde_json::json!({ "command": "ls" }),
        };
        // Outside a scope the reviewer is not asked (and nobody can be).
        assert_eq!(
            confirm(session, "Run `ls`", "why", Duration::from_secs(1)).await,
            ConfirmationAnswer::Unavailable
        );
        assert!(seen.lock().unwrap().is_empty());
        assert_eq!(
            with_confirmation_subject(
                subject.clone(),
                confirm_offering(
                    session,
                    "Run `ls`",
                    "why",
                    Some("Bash(ls)"),
                    Duration::from_secs(1)
                )
            )
            .await,
            ConfirmationAnswer::Allowed
        );
        assert_eq!(
            with_confirmation_subject(
                subject.clone(),
                confirm(session, "Run `rm x`", "why", Duration::from_secs(1))
            )
            .await,
            ConfirmationAnswer::Refused("no deletes".to_string())
        );
        // No answer: the user is asked (none can be here).
        assert_eq!(
            with_confirmation_subject(
                subject.clone(),
                confirm(session, "Run `make`", "why", Duration::from_secs(1))
            )
            .await,
            ConfirmationAnswer::Unavailable
        );
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        assert_eq!(
            seen[0],
            PermissionRequestObservation {
                action: "Run `ls`".to_string(),
                reason: "why".to_string(),
                remember: Some("Bash(ls)".to_string()),
                subject,
            }
        );
        drop(reviewer);
    }
}
