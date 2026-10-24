//! Cache-prefix compaction over the faux provider, whose per-session
//! prompt-cache simulation reads a request's common prefix with the
//! previous one as cached: the summary that continues the live request
//! reads nearly all of it from the cache, the serialized request none.

use std::sync::Arc;

use pa_agent::agent::{Agent, AgentInitialState, AgentOptions, AgentPromptInput};
use pa_types::ai::UserContent;
use pa_types::session::AgentMessage as SessionAgentMessage;

use super::*;
use crate::session_engine::compact_session::CompactOutcome;
use crate::session_engine::{AgentSession, SessionManager};

fn faux_reply(text: &str) -> pa_ai::faux::FauxResponseStep {
    pa_ai::faux::FauxResponseStep::Message(pa_ai::faux::faux_assistant_text_message(
        text,
        pa_ai::faux::FauxAssistantMessageOptions::default(),
    ))
}

fn register(
    responses: Vec<pa_ai::faux::FauxResponseStep>,
) -> pa_ai::faux::FauxProviderRegistration {
    let api = format!("faux-cache-prefix-{:?}", std::time::SystemTime::now());
    let registration =
        pa_ai::faux::register_faux_provider(pa_ai::faux::RegisterFauxProviderOptions {
            api: Some(api),
            provider: Some("faux-cache-prefix".to_string()),
            models: Some(vec![pa_ai::faux::FauxModelDefinition {
                id: "cache-m".to_string(),
                context_window: Some(200_000),
                max_tokens: Some(8_192),
                ..Default::default()
            }]),
            ..Default::default()
        });
    registration.set_responses(responses);
    registration
}

/// The session's stream with one prompt-cache key, as a provider that
/// caches by content sees every request of the conversation.
fn keyed_stream_fn(model: &pa_types::ai::Model) -> StreamFn {
    let inner = super::super::provider_adapter::real_stream_fn(None, model.clone());
    Arc::new(move |requested, context, mut options| {
        options.session_id = Some("cache-prefix-session".to_string());
        inner(requested, context, options)
    })
}

fn loop_convert() -> ConvertToLlmFn {
    Arc::new(|messages| {
        Box::pin(async move { Ok(super::super::messages::loop_convert_to_llm(messages)) })
    })
}

/// A session with a long conversation whose last request ran through the
/// keyed stream (so the provider holds its prefix), set up for compaction.
async fn session_after_a_turn(
    model: &pa_types::ai::Model,
    cache_prefix: bool,
) -> (Arc<AgentSession>, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let mut manager = SessionManager::in_memory(tmp.path());
    for i in 0..12 {
        manager
            .append_message(SessionAgentMessage::User(pa_types::ai::UserMessage {
                content: UserContent::Text(format!(
                    "turn {i}: please look at module {i} and explain what its functions do in detail"
                )),
                timestamp: 0,
                rest: pa_types::JsonMap::default(),
            }))
            .unwrap();
        manager
            .append_message(SessionAgentMessage::Assistant(
                pa_ai::faux::faux_assistant_text_message(
                    &format!(
                        "module {i} parses the config, validates paths and writes the cache file"
                    ),
                    pa_ai::faux::FauxAssistantMessageOptions::default(),
                ),
            ))
            .unwrap();
    }
    let stream_fn = keyed_stream_fn(model);
    let agent = Arc::new(Agent::new(AgentOptions {
        initial_state: AgentInitialState {
            system_prompt: Some("You are a coding agent working in a Python kernel.".to_string()),
            model: super::super::provider_adapter::json_round_trip(model),
            ..Default::default()
        },
        stream_fn: Some(Arc::clone(&stream_fn)),
        convert_to_llm: Some(loop_convert()),
        ..Default::default()
    }));
    let session = AgentSession::new(Arc::clone(&agent), manager, Vec::new())
        .await
        .unwrap();
    session.set_compaction_settings(crate::session_engine::compaction::CompactionSettings {
        keep_recent_tokens: 40,
        cache_prefix,
        ..Default::default()
    });
    session.set_compaction_cache_prefix(CachePrefixSeam {
        stream_fn,
        convert_to_llm: loop_convert(),
    });
    agent
        .prompt(AgentPromptInput::text("and module 12?"))
        .await
        .unwrap();
    agent.wait_for_idle().await;
    (Arc::new(session), tmp)
}

async fn compact_usage(cache_prefix: bool) -> (pa_types::ai::Usage, String, u64) {
    let registration = register(vec![
        faux_reply("module 12 renders the report"),
        faux_reply("## Goal\nexplain the modules"),
    ]);
    let model = registration.get_model();
    let (session, _tmp) = session_after_a_turn(&model, cache_prefix).await;
    let outcome = session.compact(None, &model, None, None).await.unwrap();
    let CompactOutcome::Ran(run) = outcome else {
        panic!("the compaction ran");
    };
    (
        run.result.usage.expect("the summary call billed usage"),
        run.result.summary,
        registration.call_count(),
    )
}

#[tokio::test]
async fn the_summary_continuing_the_live_request_reads_its_prefix_from_the_cache() {
    let (cached, summary, calls) = compact_usage(true).await;
    // One turn, one summary call: the split-turn prefix needs no second call.
    assert_eq!(calls, 2);
    assert!(summary.starts_with("## Goal\nexplain the modules"));
    // Everything but the appended summary turn is a cache read.
    assert!(cached.cache_read > 0, "{cached:?}");
    assert!(cached.cache_read > 4 * cached.input, "{cached:?}");

    let (serialized, _, calls) = compact_usage(false).await;
    assert_eq!(calls, 2);
    assert_eq!(serialized.cache_read, 0, "{serialized:?}");
    // The serialized request pays its whole prompt as uncached input.
    assert!(
        serialized.input > cached.input,
        "{serialized:?} vs {cached:?}"
    );
}

#[tokio::test]
async fn a_reply_without_text_falls_back_to_the_serialized_request() {
    let registration = register(vec![
        faux_reply("module 12 renders the report"),
        pa_ai::faux::FauxResponseStep::Message(pa_ai::faux::faux_assistant_message(
            vec![pa_ai::faux::faux_tool_call(
                "python_repl",
                &serde_json::json!({"code": "1"}),
                None,
            )],
            pa_ai::faux::FauxAssistantMessageOptions::default(),
        )),
        faux_reply("## Goal\nthe serialized summary"),
    ]);
    let model = registration.get_model();
    let (session, _tmp) = session_after_a_turn(&model, true).await;
    let CompactOutcome::Ran(run) = session.compact(None, &model, None, None).await.unwrap() else {
        panic!("the compaction ran");
    };
    assert_eq!(registration.call_count(), 3);
    assert!(
        run.result
            .summary
            .starts_with("## Goal\nthe serialized summary")
    );
}

#[test]
fn the_summary_turn_frames_the_session_prompt_as_text_only() {
    let text = summary_turn_text(Some("keep the module list"));
    assert!(text.starts_with("CRITICAL: Respond with TEXT ONLY. Do NOT call any tools."));
    assert!(text.contains("Create a structured context checkpoint summary"));
    assert!(text.contains("<user-instructions>"));
    assert!(text.contains("keep the module list"));
    assert!(text.contains(EARLIER_SUMMARY_NOTE));
    assert!(text.ends_with(TEXT_ONLY_REMINDER));
}
