//! The request-context seam: installed features rewrite the message list one
//! model request sends (context management: dropping spent outputs,
//! replacing old history with summaries), in installation order. The stored
//! session and the transcript are never touched; only the request's copy.
//!
//! A hook that errors or panics leaves the messages it was given: a broken
//! feature costs context size, never the turn.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;

use futures::FutureExt as _;
use pa_agent::agent_loop::TransformContextFn;
use pa_agent::types::AgentMessage;

use super::{FeatureFuture, SessionFeature, SessionFeatureContext};

/// One feature's rewrite of a model request's messages.
pub trait RequestContextHook: Send + Sync {
    /// The messages to send instead of `messages`. Called before every
    /// model request of the session (tool continuations included), so the
    /// result must be a pure function of `messages` and the feature's own
    /// state, or the provider's prompt cache misses.
    fn transform(
        &self,
        messages: Vec<AgentMessage>,
    ) -> FeatureFuture<anyhow::Result<Vec<AgentMessage>>>;
}

/// The hooks `features` offer the session, in installation order.
pub(crate) fn session_request_context_hooks(
    features: &[Arc<dyn SessionFeature>],
    context: &Arc<SessionFeatureContext>,
) -> Vec<Arc<dyn RequestContextHook>> {
    features
        .iter()
        .filter_map(|feature| feature.request_context_hook(context))
        .collect()
}

/// Run `hooks` over `messages` in order; a failing hook is skipped.
pub(crate) async fn apply(
    hooks: &[Arc<dyn RequestContextHook>],
    mut messages: Vec<AgentMessage>,
) -> Vec<AgentMessage> {
    for hook in hooks {
        let input = messages.clone();
        match AssertUnwindSafe(hook.transform(input)).catch_unwind().await {
            Ok(Ok(transformed)) => messages = transformed,
            Ok(Err(error)) => tracing::warn!(
                error = %format!("{error:#}"),
                "request context hook failed; the request keeps its messages"
            ),
            Err(_payload) => {
                tracing::error!("request context hook panicked; the request keeps its messages");
            }
        }
    }
    messages
}

/// The session's transform: `hooks` in order, or a pass-through without any.
pub(crate) fn transform_fn(hooks: Vec<Arc<dyn RequestContextHook>>) -> TransformContextFn {
    let hooks = Arc::new(hooks);
    Arc::new(move |messages, _signal| {
        let hooks = Arc::clone(&hooks);
        Box::pin(async move { Ok(apply(&hooks, messages).await) })
    })
}

#[cfg(test)]
mod tests {
    use pa_agent::types::{Message, UserContent, UserMessage};

    use super::*;

    fn user(text: &str) -> AgentMessage {
        AgentMessage::Standard(Message::User(UserMessage {
            content: UserContent::Text(text.to_string()),
            timestamp: 1,
        }))
    }

    fn text(message: &AgentMessage) -> String {
        match message {
            AgentMessage::Standard(Message::User(UserMessage {
                content: UserContent::Text(text),
                ..
            })) => text.clone(),
            _ => String::new(),
        }
    }

    /// Appends its suffix to every user text.
    struct Suffix(&'static str);

    impl RequestContextHook for Suffix {
        fn transform(
            &self,
            messages: Vec<AgentMessage>,
        ) -> FeatureFuture<anyhow::Result<Vec<AgentMessage>>> {
            let suffix = self.0;
            Box::pin(async move {
                Ok(messages
                    .iter()
                    .map(|message| user(&format!("{}{suffix}", text(message))))
                    .collect())
            })
        }
    }

    struct Failing;

    impl RequestContextHook for Failing {
        fn transform(
            &self,
            _messages: Vec<AgentMessage>,
        ) -> FeatureFuture<anyhow::Result<Vec<AgentMessage>>> {
            Box::pin(async { anyhow::bail!("broken") })
        }
    }

    struct Panicking;

    impl RequestContextHook for Panicking {
        fn transform(
            &self,
            _messages: Vec<AgentMessage>,
        ) -> FeatureFuture<anyhow::Result<Vec<AgentMessage>>> {
            Box::pin(async { panic!("hook bug") })
        }
    }

    #[tokio::test]
    async fn hooks_compose_in_order_and_failures_pass_through() {
        let hooks: Vec<Arc<dyn RequestContextHook>> = vec![
            Arc::new(Suffix("-a")),
            Arc::new(Failing),
            Arc::new(Panicking),
            Arc::new(Suffix("-b")),
        ];
        let transform = transform_fn(hooks);
        let out = transform(vec![user("x")], pa_agent::abort::AbortSignal::default())
            .await
            .unwrap();
        assert_eq!(out.iter().map(text).collect::<Vec<_>>(), ["x-a-b"]);
        let none = transform_fn(Vec::new());
        let out = none(vec![user("y")], pa_agent::abort::AbortSignal::default())
            .await
            .unwrap();
        assert_eq!(out, vec![user("y")]);
    }
}
