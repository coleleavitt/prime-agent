//! The policy seams' plumbing: one session's tool calls, kernel host
//! requests and admitted prompts routed through the installed features'
//! reviews. The first `Block`, in installation order, refuses; otherwise an
//! `Ask` puts the action to the user ([`super::confirm`]). Notes features
//! return after a host request ran wait in the session's [`FeatureNotes`]
//! for the enclosing tool call's result, or for the next admitted prompt.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};

use pa_agent::agent_loop::{AfterToolCallFn, BeforeToolCallFn};
use pa_agent::types::{
    AfterToolCallResult,
    AgentMessage,
    BeforeToolCallResult,
    Message,
    ToolResultContent,
};
use serde_json::Value;

use super::confirm::{CONFIRMATION_TIMEOUT, ConfirmationAnswer, confirm};
use super::{
    FeatureVerdict,
    HostRequestObservation,
    PromptDecision,
    SessionFeature,
    SessionFeatureContext,
    ToolCallObservation,
    ToolResultObservation,
    is_observed_host_request,
    is_reviewed_host_request,
};
use crate::kernel::shared::{HostRequestGate, HostRequestPolicy};

/// The most notes held for the model; older ones are dropped first.
const MAX_PENDING_NOTES: usize = 32;

/// How much of an action's arguments a confirmation shows.
const MAX_ACTION_CHARS: usize = 600;

/// Notes features returned after kernel host requests (a `PostToolUse`
/// hook on the kernel's `bash()`), waiting for the model. One per session,
/// shared by its host-request gate, its tool hooks and its admission.
#[derive(Clone, Default, Debug)]
pub struct FeatureNotes(Arc<Mutex<Vec<String>>>);

impl FeatureNotes {
    fn push(&self, note: String) {
        let mut notes = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if notes.len() >= MAX_PENDING_NOTES {
            notes.remove(0);
        }
        notes.push(note);
    }

    /// The pending notes, oldest first; none remain.
    pub(crate) fn take(&self) -> Vec<String> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// `text` cut to `max` characters, marked when cut.
fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((end, _)) => format!("{}...", &text[..end]),
        None => text.to_string(),
    }
}

/// How a tool call is put to the user.
fn describe_tool_call(call: &ToolCallObservation) -> String {
    format!(
        "Run tool `{}` with {}",
        call.tool_name,
        clip(&call.args.to_string(), MAX_ACTION_CHARS)
    )
}

/// How a kernel host request is put to the user: a `bash.run` as its
/// command (never its environment), any other as its type and payload.
fn describe_host_request(request: &HostRequestObservation) -> String {
    if request.request_type == "bash.run" {
        let command = request
            .data
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default();
        return format!("Run `{}`", clip(command, MAX_ACTION_CHARS));
    }
    let mut payload = request.data.clone();
    if let Some(object) = payload.as_object_mut() {
        object.remove("type");
    }
    format!(
        "Kernel request `{}` with {}",
        request.request_type,
        clip(&payload.to_string(), MAX_ACTION_CHARS)
    )
}

/// Settle the features' verdicts on one action, given in installation
/// order: the first `Block` refuses it; otherwise any `Ask` puts `action`
/// to the user, and only their allow lets it run. `None` lets it run;
/// `Some` is the refusal the model sees.
async fn settle(session_id: &str, action: &str, verdicts: Vec<FeatureVerdict>) -> Option<String> {
    let mut asks = Vec::new();
    for verdict in verdicts {
        match verdict {
            FeatureVerdict::Allow => {}
            FeatureVerdict::Block(reason) => return Some(reason),
            FeatureVerdict::Ask(reason) => asks.push(reason),
        }
    }
    if asks.is_empty() {
        return None;
    }
    let reason = asks.join("; ");
    match confirm(session_id, action, &reason, CONFIRMATION_TIMEOUT).await {
        ConfirmationAnswer::Allowed => None,
        ConfirmationAnswer::Denied => Some(format!("{reason} (the user refused it)")),
        ConfirmationAnswer::Unavailable => Some(format!(
            "{reason} (it needs the user's confirmation, and no client can ask for it)"
        )),
        ConfirmationAnswer::TimedOut(after) => Some(format!(
            "{reason} (the user did not answer within {}s)",
            after.as_secs()
        )),
    }
}

/// The agent-loop tool hooks that route one session's tool calls through
/// `features`; `None` for both when there are no features, so the native
/// loop runs without hooks. The after hook also delivers the session's
/// pending `notes` on the result.
pub(crate) fn tool_call_hooks(
    features: &[Arc<dyn SessionFeature>],
    context: &Arc<SessionFeatureContext>,
    notes: &FeatureNotes,
) -> (Option<BeforeToolCallFn>, Option<AfterToolCallFn>) {
    if features.is_empty() {
        return (None, None);
    }
    let before: BeforeToolCallFn = {
        let features = features.to_vec();
        let context = Arc::clone(context);
        Arc::new(move |call, signal| {
            let observation = ToolCallObservation {
                tool_call_id: call.tool_call.id.clone(),
                tool_name: call.tool_call.name.clone(),
                args: call.args,
            };
            let observers: Vec<_> = features
                .iter()
                .map(|feature| feature.before_tool_call(&context, &observation))
                .collect();
            let reviews: Vec<_> = features
                .iter()
                .map(|feature| feature.review_tool_call(&context, &observation))
                .collect();
            let context = Arc::clone(&context);
            Box::pin(async move {
                let (_, verdicts) = futures::future::join(
                    futures::future::join_all(observers),
                    futures::future::join_all(reviews),
                )
                .await;
                let action = describe_tool_call(&observation);
                // An aborted run stops waiting for the user.
                let refusal = tokio::select! {
                    refusal = settle(&context.session_id, &action, verdicts) => refusal,
                    () = signal.aborted() => Some("the run was aborted".to_string()),
                };
                Ok(refusal.map(|reason| BeforeToolCallResult {
                    block: true,
                    reason: Some(reason),
                }))
            })
        })
    };
    let after: AfterToolCallFn = {
        let features = features.to_vec();
        let context = Arc::clone(context);
        let notes = notes.clone();
        Arc::new(move |call, _signal| {
            let earlier_results_of_tool = call
                .context
                .messages
                .iter()
                .filter(|message| {
                    matches!(
                        message,
                        AgentMessage::Standard(Message::ToolResult(result))
                            if result.tool_name == call.tool_call.name
                                && result.tool_call_id != call.tool_call.id
                    )
                })
                .count();
            let observation = ToolResultObservation {
                tool_call_id: call.tool_call.id.clone(),
                tool_name: call.tool_call.name.clone(),
                args: call.args,
                is_error: call.is_error,
                content: call.result.content.clone(),
                details: call.result.details.clone(),
                host_facts: call.result.host_facts.clone(),
                earlier_results_of_tool,
            };
            let pending: Vec<_> = features
                .iter()
                .map(|feature| feature.after_tool_call(&context, &observation))
                .collect();
            let notes = notes.clone();
            let mut blocks = call.result.content;
            Box::pin(async move {
                let mut appended: Vec<String> = futures::future::join_all(pending)
                    .await
                    .into_iter()
                    .flatten()
                    .collect();
                // Notes from the host requests this call's cells sent come
                // after the features' own notes on the call.
                appended.extend(notes.take());
                if appended.is_empty() {
                    return Ok(None);
                }
                blocks.extend(appended.into_iter().map(ToolResultContent::text));
                Ok(Some(AfterToolCallResult {
                    content: Some(blocks),
                    ..AfterToolCallResult::default()
                }))
            })
        })
    };
    (Some(before), Some(after))
}

/// One session's [`HostRequestPolicy`] over the installed features.
struct FeaturePolicy {
    features: Vec<Arc<dyn SessionFeature>>,
    context: Arc<SessionFeatureContext>,
    notes: FeatureNotes,
    /// `bash.run` commands the host answered `EnvUnknown` after they were
    /// allowed: the kernel resends each at once with its environment
    /// attached, and that resend is the same action, not reviewed (or put
    /// to the user) again.
    resends: Mutex<Vec<String>>,
}

/// The most allowed commands awaiting their resend.
const MAX_PENDING_RESENDS: usize = 8;

/// A `bash.run` request's command.
fn bash_run_command(request_type: &str, data: &Value) -> Option<String> {
    (request_type == "bash.run")
        .then(|| data.get("command").and_then(Value::as_str))
        .flatten()
        .map(str::to_string)
}

type PolicyFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

impl HostRequestPolicy for FeaturePolicy {
    fn review(&self, request_type: &str, data: &Value) -> PolicyFuture<Option<String>> {
        if !is_reviewed_host_request(request_type) {
            return Box::pin(async { None });
        }
        if let Some(command) = bash_run_command(request_type, data) {
            let mut resends = self.resends.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(index) = resends.iter().position(|allowed| *allowed == command) {
                resends.remove(index);
                return Box::pin(async { None });
            }
        }
        let observation = HostRequestObservation {
            request_type: request_type.to_string(),
            data: data.clone(),
        };
        let reviews: Vec<_> = self
            .features
            .iter()
            .map(|feature| feature.review_host_request(&self.context, &observation))
            .collect();
        let context = Arc::clone(&self.context);
        Box::pin(async move {
            let verdicts = futures::future::join_all(reviews).await;
            let action = describe_host_request(&observation);
            settle(&context.session_id, &action, verdicts).await
        })
    }

    fn after(
        &self,
        request_type: &str,
        data: &Value,
        outcome: Result<&Value, &str>,
    ) -> PolicyFuture<()> {
        if !is_observed_host_request(request_type) {
            return Box::pin(async {});
        }
        let env_unknown = outcome.is_ok_and(|reply| {
            reply.get("status").and_then(Value::as_str) == Some("error")
                && reply.get("error").and_then(Value::as_str) == Some("EnvUnknown")
        });
        if let Some(command) = bash_run_command(request_type, data).filter(|_| env_unknown) {
            let mut resends = self.resends.lock().unwrap_or_else(PoisonError::into_inner);
            if resends.len() >= MAX_PENDING_RESENDS {
                resends.remove(0);
            }
            resends.push(command);
            // Nothing ran: the resend's outcome is the one observed.
            return Box::pin(async {});
        }
        let observation = HostRequestObservation {
            request_type: request_type.to_string(),
            data: data.clone(),
        };
        let observers: Vec<_> = self
            .features
            .iter()
            .map(|feature| feature.after_host_request(&self.context, &observation, outcome))
            .collect();
        let notes = self.notes.clone();
        Box::pin(async move {
            for note in futures::future::join_all(observers)
                .await
                .into_iter()
                .flatten()
                .filter(|note| !note.trim().is_empty())
            {
                notes.push(note);
            }
        })
    }
}

/// The kernel host-request gate that routes one session's host requests
/// through `features`' [`SessionFeature::review_host_request`] and
/// [`SessionFeature::after_host_request`], their notes into `notes`; `None`
/// when there are no features, so the kernel dispatches natively.
pub(crate) fn host_request_gate(
    features: &[Arc<dyn SessionFeature>],
    context: &Arc<SessionFeatureContext>,
    notes: &FeatureNotes,
) -> Option<HostRequestGate> {
    if features.is_empty() {
        return None;
    }
    Some(Arc::new(FeaturePolicy {
        features: features.to_vec(),
        context: Arc::clone(context),
        notes: notes.clone(),
        resends: Mutex::default(),
    }))
}

/// Every installed feature's answer for a prompt being admitted: the first
/// `Block` in installation order, or the contexts joined in that order,
/// followed by the host-request notes no tool result carried.
pub(crate) async fn review_prompt(
    features: &[Arc<dyn SessionFeature>],
    context: &Arc<SessionFeatureContext>,
    notes: &FeatureNotes,
    prompt: &str,
) -> PromptDecision {
    let answers = futures::future::join_all(
        features
            .iter()
            .map(|feature| feature.prompt_context(context, prompt)),
    )
    .await;
    let mut contexts = Vec::new();
    for answer in answers {
        match answer {
            PromptDecision::Block(reason) => return PromptDecision::Block(reason),
            PromptDecision::Continue {
                context: Some(text),
            } if !text.trim().is_empty() => contexts.push(text),
            PromptDecision::Continue { .. } => {}
        }
    }
    contexts.extend(notes.take());
    PromptDecision::Continue {
        context: (!contexts.is_empty()).then(|| contexts.join("\n\n")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_are_described_without_the_environment_and_clipped() {
        let bash = HostRequestObservation {
            request_type: "bash.run".to_string(),
            data: serde_json::json!({
                "type": "bash.run",
                "command": "rm -rf build",
                "env": { "TOKEN": "secret" }
            }),
        };
        assert_eq!(describe_host_request(&bash), "Run `rm -rf build`");
        let other = HostRequestObservation {
            request_type: "computer_use.click".to_string(),
            data: serde_json::json!({ "type": "computer_use.click", "x": 1 }),
        };
        assert_eq!(
            describe_host_request(&other),
            r#"Kernel request `computer_use.click` with {"x":1}"#
        );
        let call = ToolCallObservation {
            tool_call_id: "t1".to_string(),
            tool_name: "edit".to_string(),
            args: serde_json::json!({ "text": "é".repeat(700) }),
        };
        let described = describe_tool_call(&call);
        assert!(described.starts_with("Run tool `edit` with {\"text\":\"éé"));
        assert!(described.ends_with("..."), "{described}");
        assert_eq!(clip("short", 10), "short");
    }

    #[tokio::test]
    async fn a_block_wins_over_an_ask_and_an_ask_without_a_surface_refuses() {
        let session = "policy-settle-session";
        assert_eq!(
            settle(
                session,
                "Run `x`",
                vec![
                    FeatureVerdict::Ask("check".to_string()),
                    FeatureVerdict::Block("no".to_string()),
                ],
            )
            .await,
            Some("no".to_string())
        );
        assert_eq!(
            settle(session, "Run `x`", vec![FeatureVerdict::Allow]).await,
            None
        );
        assert_eq!(
            settle(
                session,
                "Run `x`",
                vec![
                    FeatureVerdict::Ask("first".to_string()),
                    FeatureVerdict::Ask("second".to_string()),
                ],
            )
            .await,
            Some(
                "first; second (it needs the user's confirmation, and no client can ask for it)"
                    .to_string()
            )
        );
    }

    /// Counts reviews; every review asks to block.
    struct Counting(std::sync::atomic::AtomicUsize);

    impl SessionFeature for Counting {
        fn name(&self) -> &'static str {
            "counting"
        }

        fn review_host_request(
            &self,
            _context: &Arc<SessionFeatureContext>,
            _request: &HostRequestObservation,
        ) -> super::super::FeatureFuture<FeatureVerdict> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { FeatureVerdict::Allow })
        }
    }

    #[tokio::test]
    async fn an_env_unknown_resend_is_not_reviewed_again() {
        let counting = Arc::new(Counting(std::sync::atomic::AtomicUsize::new(0)));
        let context = Arc::new(super::super::tests::context("policy-resend"));
        let gate = host_request_gate(
            &[Arc::clone(&counting) as Arc<dyn SessionFeature>],
            &context,
            &FeatureNotes::default(),
        )
        .unwrap();
        let run = serde_json::json!({ "type": "bash.run", "command": "echo hi" });
        assert_eq!(gate.review("bash.run", &run).await, None);
        let env_unknown = serde_json::json!({ "status": "error", "error": "EnvUnknown" });
        gate.after("bash.run", &run, Ok(&env_unknown)).await;
        // The resend passes unreviewed, once; the next run is reviewed.
        assert_eq!(gate.review("bash.run", &run).await, None);
        assert_eq!(gate.review("bash.run", &run).await, None);
        assert_eq!(counting.0.load(std::sync::atomic::Ordering::SeqCst), 2);
        // Bookkeeping requests are never reviewed.
        assert_eq!(
            gate.review("bash.follow", &serde_json::json!({})).await,
            None
        );
        assert_eq!(counting.0.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn notes_keep_the_newest_when_full() {
        let notes = FeatureNotes::default();
        for n in 0..(MAX_PENDING_NOTES + 2) {
            notes.push(format!("note {n}"));
        }
        let taken = notes.take();
        assert_eq!(taken.len(), MAX_PENDING_NOTES);
        assert_eq!(taken[0], "note 2");
        assert!(notes.take().is_empty());
    }
}
