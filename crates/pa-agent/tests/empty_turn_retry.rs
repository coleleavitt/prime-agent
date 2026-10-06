//! Empty final turns (upstream #1896): a normal-stop response with no visible
//! text and no tool call is silently re-requested instead of completing the
//! run, discarded attempts never reach the transcript or the next request,
//! their spend rides the surviving message, and the third empty attempt
//! settles as an error.

use std::sync::{Arc, Mutex};

use pa_agent::agent::{Agent, AgentOptions};
use pa_agent::scripted::{ScriptStep, ScriptedProvider, ScriptedTurn};
use pa_agent::stream::AssistantMessageEvent;
use pa_agent::types::{
    AgentEvent, AgentMessage, AssistantContent, AssistantMessage, Message, Model, StopReason,
    TextContent, ThinkingContent, Usage, UsageCost,
};

const CONTEXT_WINDOW: u64 = 100_000;

fn test_model() -> Model {
    Model {
        id: "test-model".into(),
        name: "Test Model".into(),
        api: "test".into(),
        provider: "test".into(),
        base_url: String::new(),
        reasoning: false,
        cost: UsageCost::default(),
        context_window: CONTEXT_WINDOW,
        max_tokens: 4_096,
    }
}

fn usage(input: u64, output: u64, cost: f64) -> Usage {
    Usage {
        input,
        output,
        total_tokens: input + output,
        cost: UsageCost {
            total: cost,
            ..UsageCost::default()
        },
        ..Usage::zero()
    }
}

fn assistant(content: Vec<AssistantContent>, stop_reason: StopReason) -> AssistantMessage {
    AssistantMessage {
        content,
        api: "test".into(),
        provider: "test".into(),
        model: "test-model".into(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: Usage::zero(),
        stop_reason,
        stop_reason_raw: None,
        error_message: None,
        timestamp: 1,
        discarded_usage: None,
    }
}

fn thinking(text: &str) -> AssistantContent {
    AssistantContent::Thinking(ThinkingContent {
        thinking: text.into(),
        thinking_signature: None,
        redacted: None,
    })
}

fn text(text: &str) -> AssistantContent {
    AssistantContent::Text(TextContent {
        text: text.into(),
        text_signature: None,
    })
}

/// A provider turn that settles straight to `message` (no streamed deltas).
fn done_turn(message: AssistantMessage) -> ScriptedTurn {
    ScriptedTurn::Events(vec![ScriptStep::Event(Box::new(
        AssistantMessageEvent::Done {
            reason: message.stop_reason,
            message,
        },
    ))])
}

struct Harness {
    agent: Agent,
    provider: Arc<ScriptedProvider>,
    assistant_ends: Arc<Mutex<Vec<AssistantMessage>>>,
}

async fn harness(turns: Vec<ScriptedTurn>) -> Harness {
    let provider = Arc::new(ScriptedProvider::new(test_model()));
    for turn in turns {
        provider.push_turn(turn);
    }
    let agent = Agent::new(AgentOptions {
        stream_fn: Some(provider.stream_fn()),
        ..Default::default()
    });
    agent.set_model(test_model()).await;
    let assistant_ends: Arc<Mutex<Vec<AssistantMessage>>> = Arc::default();
    agent
        .subscribe({
            let assistant_ends = Arc::clone(&assistant_ends);
            move |event, _signal| {
                if let AgentEvent::MessageEnd {
                    message: AgentMessage::Standard(Message::Assistant(message)),
                } = event
                {
                    assistant_ends.lock().unwrap().push(message);
                }
                Box::pin(async { Ok(()) })
            }
        })
        .await;
    Harness {
        agent,
        provider,
        assistant_ends,
    }
}

impl Harness {
    async fn run(&self) -> Vec<AssistantMessage> {
        // A run failure rejects the prompt after settling its failure turn;
        // the transcript is what every case asserts.
        let _ = self.agent.prompt("Hello").await;
        self.agent.wait_for_idle().await;
        self.agent
            .state()
            .await
            .messages
            .into_iter()
            .filter_map(|message| match message {
                AgentMessage::Standard(Message::Assistant(message)) => Some(message),
                _ => None,
            })
            .collect()
    }

    fn request_assistant_counts(&self) -> Vec<usize> {
        self.provider
            .calls()
            .iter()
            .map(|call| {
                call.messages
                    .iter()
                    .filter(|message| matches!(message, Message::Assistant(_)))
                    .count()
            })
            .collect()
    }
}

#[tokio::test]
async fn silently_retries_empty_turns_and_keeps_the_transcript_clean() {
    let mut thinking_only = assistant(vec![thinking("pondering...")], StopReason::Stop);
    thinking_only.usage = usage(100, 40, 0.02);
    let mut whitespace_only = assistant(vec![text("  \n")], StopReason::Stop);
    whitespace_only.usage = usage(110, 5, 0.01);
    let good = assistant(vec![text("done")], StopReason::Stop);
    let harness = harness(vec![
        done_turn(thinking_only.clone()),
        done_turn(whitespace_only.clone()),
        done_turn(good.clone()),
    ])
    .await;

    let assistants = harness.run().await;

    // Three requests; the discarded attempts were never resent.
    assert_eq!(harness.request_assistant_counts(), vec![0, 0, 0]);
    let survivor = AssistantMessage {
        discarded_usage: Some(vec![thinking_only.usage, whitespace_only.usage]),
        ..good
    };
    assert_eq!(assistants, vec![survivor.clone()]);
    // Exactly one durable assistant message, and it carries the spend.
    assert_eq!(*harness.assistant_ends.lock().unwrap(), vec![survivor]);
}

#[tokio::test]
async fn surfaces_an_error_after_three_consecutive_empty_turns() {
    let empty = assistant(vec![thinking("...")], StopReason::Stop);
    let harness = harness(vec![
        done_turn(empty.clone()),
        done_turn(empty.clone()),
        done_turn(empty.clone()),
    ])
    .await;

    let assistants = harness.run().await;

    assert_eq!(harness.request_assistant_counts(), vec![0, 0, 0]);
    let failed = AssistantMessage {
        stop_reason: StopReason::Error,
        error_message: Some(
            "Model returned an empty response (no output content or tool calls) 3 times in a row"
                .into(),
        ),
        discarded_usage: Some(vec![Usage::zero(), Usage::zero()]),
        ..empty
    };
    assert_eq!(assistants, vec![failed.clone()]);
    assert_eq!(*harness.assistant_ends.lock().unwrap(), vec![failed]);
}

#[tokio::test]
async fn visible_content_a_length_stop_and_a_silent_overflow_are_not_retried() {
    let visible = assistant(
        vec![thinking("..."), text("partial but visible")],
        StopReason::Stop,
    );
    let length_stop = assistant(vec![thinking("...")], StopReason::Length);
    // z.ai-style silent overflow: normal stop, empty content, input past the
    // window. It must reach message_end untouched for compaction recovery.
    let mut overflow = assistant(vec![text("")], StopReason::Stop);
    overflow.usage.input = CONTEXT_WINDOW + 1;
    for message in [visible, length_stop, overflow] {
        let harness = harness(vec![done_turn(message.clone())]).await;
        let assistants = harness.run().await;
        assert_eq!(harness.request_assistant_counts(), vec![0]);
        assert_eq!(assistants, vec![message.clone()]);
        assert_eq!(*harness.assistant_ends.lock().unwrap(), vec![message]);
    }
}

#[tokio::test]
async fn discarded_spend_survives_a_later_attempt_that_throws() {
    let mut empty = assistant(vec![thinking("pondering")], StopReason::Stop);
    empty.usage = usage(50, 10, 0.02);
    let harness = harness(vec![
        done_turn(empty.clone()),
        ScriptedTurn::FailStart("provider hiccup".into()),
    ])
    .await;

    let assistants = harness.run().await;

    let failure = assistants.last().expect("the run failure settles a turn");
    assert_eq!(failure.stop_reason, StopReason::Error);
    assert_eq!(failure.error_message.as_deref(), Some("provider hiccup"));
    assert_eq!(failure.discarded_usage, Some(vec![empty.usage]));
    assert_eq!(assistants.len(), 1, "the discarded attempt never lands");
}
