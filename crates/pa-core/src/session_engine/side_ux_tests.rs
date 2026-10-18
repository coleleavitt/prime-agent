use std::sync::Arc;

use pa_agent::agent::{Agent, AgentInitialState, AgentOptions};
use pa_agent::scripted::ScriptedProvider;
use pa_agent::types::{
    AgentMessage,
    AssistantContent,
    AssistantMessage,
    Message,
    TextContent,
    UserContent,
    UserMessage,
};

use super::*;

fn test_model() -> pa_agent::types::Model {
    pa_agent::types::Model {
        id: "m".into(),
        name: "m".into(),
        api: "test".into(),
        provider: "test".into(),
        base_url: "http://localhost".into(),
        reasoning: false,
        cost: pa_agent::types::UsageCost::default(),
        context_window: 1_000,
        max_tokens: 100,
        max_tokens_explicit: false,
    }
}

fn user(text: &str) -> AgentMessage {
    AgentMessage::Standard(Message::User(UserMessage {
        content: UserContent::Text(text.to_string()),
        timestamp: 0,
    }))
}

fn assistant(text: &str, stop_reason: StopReason) -> AgentMessage {
    AgentMessage::Standard(Message::Assistant(AssistantMessage {
        content: vec![AssistantContent::Text(TextContent {
            text: text.to_string(),
            text_signature: None,
        })],
        api: "test".into(),
        provider: "test".into(),
        model: "m".into(),
        response_model: None,
        response_id: None,
        diagnostics: None,
        usage: pa_agent::types::Usage::zero(),
        stop_reason,
        stop_reason_raw: None,
        error_message: None,
        timestamp: 0,
        discarded_usage: None,
    }))
}

fn parent(provider: &Arc<ScriptedProvider>, messages: Vec<AgentMessage>) -> Arc<Agent> {
    Arc::new(Agent::new(AgentOptions {
        initial_state: AgentInitialState {
            system_prompt: Some("the session prompt".to_string()),
            model: Some(test_model()),
            thinking_level: None,
            tools: None,
            messages: Some(messages),
        },
        stream_fn: Some(provider.stream_fn()),
        ..Default::default()
    }))
}

#[test]
fn the_title_input_skips_short_messages_and_caps_long_ones() {
    assert_eq!(title_input("   fix it  "), None);
    assert_eq!(
        title_input("  the login page crashes  ").as_deref(),
        Some("the login page crashes")
    );
    let long = "a".repeat(1_500) + &"b".repeat(1_500);
    let capped = title_input(&long).unwrap();
    assert!(capped.starts_with(&"a".repeat(1_500)));
    assert!(capped.contains("[\u{2026} 3000 characters omitted \u{2026}]"));
    assert!(capped.ends_with('b'));
    assert_eq!(capped.chars().count(), 2_000);
}

#[test]
fn titles_parse_from_json_fenced_json_or_a_plain_line() {
    assert_eq!(
        parse_title(r#"{"title": "Login page crash"}"#).as_deref(),
        Some("Login page crash")
    );
    assert_eq!(
        parse_title("```json\n{\"title\": \"  pa-daemon  roster \"}\n```").as_deref(),
        Some("pa-daemon roster")
    );
    assert_eq!(
        parse_title("\"Kernel protocol 6\"\n").as_deref(),
        Some("Kernel protocol 6")
    );
    assert_eq!(parse_title(r#"{"title": "  "}"#), None);
    assert_eq!(parse_title(r#"{"other": "x"}"#), None);
    assert_eq!(parse_title(""), None);
}

#[test]
fn session_names_fold_to_kebab_case() {
    assert_eq!(
        parse_session_name(r#"{"name": "Fix Login Bug"}"#).as_deref(),
        Some("fix-login-bug")
    );
    assert_eq!(
        parse_session_name("refactor_api client!").as_deref(),
        Some("refactor-api-client")
    );
    assert_eq!(parse_session_name(r#"{"name": "--"}"#), None);
}

#[test]
fn the_conversation_tail_keeps_the_last_thousand_characters_of_text() {
    let messages = vec![
        user("first question"),
        assistant("an answer", StopReason::Stop),
    ];
    assert_eq!(conversation_tail(&messages), "first question\nan answer");
    let long = vec![user(&"x".repeat(1_200)), assistant("end", StopReason::Stop)];
    let tail = conversation_tail(&long);
    assert_eq!(tail.chars().count(), 1_000);
    assert!(tail.ends_with("x\nend"));
}

#[tokio::test]
async fn the_title_call_sends_claude_codes_prompt_and_reads_the_json_title() {
    let provider = Arc::new(ScriptedProvider::new(test_model()));
    provider.push_text_turn(r#"{"title": "Roster delta ordering"}"#);
    let side = SideModel::Session {
        stream_fn: provider.stream_fn(),
        model: test_model(),
    };
    let title = generate_title(&side, "why do roster deltas arrive out of order?")
        .await
        .unwrap();
    assert_eq!(title.as_deref(), Some("Roster delta ordering"));
    let calls = provider.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].system_prompt.as_deref(), Some(TITLE_SYSTEM_PROMPT));
    let Message::User(sent) = &calls[0].messages[0] else {
        panic!("a user message");
    };
    let UserContent::Text(text) = &sent.content else {
        panic!("text content");
    };
    assert!(text.starts_with("<session>\nwhy do roster deltas arrive out of order?\n</session>"));
    // Too short: no call at all.
    assert_eq!(generate_title(&side, "hi").await.unwrap(), None);
    assert_eq!(provider.calls().len(), 1);
}

#[tokio::test]
async fn the_recap_forks_the_conversation_with_its_system_prompt_and_appends_the_instruction() {
    let provider = Arc::new(ScriptedProvider::new(test_model()));
    provider.push_text_turn("  Fixing the roster ordering; next, run the e2e suite.  ");
    let agent = parent(
        &provider,
        vec![
            user("fix roster ordering"),
            assistant("on it", StopReason::Stop),
        ],
    );
    let outcome = recap(&agent, &provider.stream_fn()).await.unwrap();
    assert_eq!(
        outcome,
        RecapOutcome::Recap {
            text: "Fixing the roster ordering; next, run the e2e suite.".to_string(),
            capped: false,
        }
    );
    let calls = provider.calls();
    assert_eq!(
        calls[0].system_prompt.as_deref(),
        Some("the session prompt")
    );
    assert_eq!(calls[0].messages.len(), 3);
    let Message::User(last) = &calls[0].messages[2] else {
        panic!("the instruction is a user message");
    };
    assert_eq!(last.content, UserContent::Text(RECAP_PROMPT.to_string()));
    // The parent conversation is untouched.
    assert_eq!(agent.state().await.messages.len(), 2);
}

#[tokio::test]
async fn a_recap_without_a_model_turn_makes_no_call_and_a_long_one_is_capped() {
    let provider = Arc::new(ScriptedProvider::new(test_model()));
    let empty = parent(&provider, vec![user("hello there")]);
    assert_eq!(
        recap(&empty, &provider.stream_fn()).await.unwrap(),
        RecapOutcome::NoTurn
    );
    assert!(provider.calls().is_empty());
    provider.push_text_turn(&"word ".repeat(200));
    let agent = parent(
        &provider,
        vec![user("go"), assistant("done", StopReason::Stop)],
    );
    let RecapOutcome::Recap { text, capped } = recap(&agent, &provider.stream_fn()).await.unwrap()
    else {
        panic!("a recap");
    };
    assert!(capped);
    assert!(text.chars().count() <= RECAP_MAX_CHARS);
    assert!(text.ends_with('\u{2026}'));
}

#[tokio::test]
async fn suggestions_wait_for_two_turns_skip_a_failed_turn_and_filter_the_reply() {
    let provider = Arc::new(ScriptedProvider::new(test_model()));
    let early = parent(
        &provider,
        vec![user("fix the bug"), assistant("fixed", StopReason::Stop)],
    );
    assert_eq!(
        prompt_suggestion(&early, &provider.stream_fn())
            .await
            .unwrap(),
        None
    );
    let failed = parent(
        &provider,
        vec![
            user("fix the bug"),
            assistant("fixed", StopReason::Stop),
            user("and run tests"),
            assistant("", StopReason::Error),
        ],
    );
    assert_eq!(
        prompt_suggestion(&failed, &provider.stream_fn())
            .await
            .unwrap(),
        None
    );
    assert!(provider.calls().is_empty());
    let conversation = vec![
        user("fix the bug and run tests"),
        assistant("looking", StopReason::Stop),
        user("go on"),
        assistant("the bug is fixed", StopReason::Stop),
    ];
    provider.push_text_turn("<suggestion>run the tests</suggestion>");
    let agent = parent(&provider, conversation.clone());
    assert_eq!(
        prompt_suggestion(&agent, &provider.stream_fn())
            .await
            .unwrap()
            .as_deref(),
        Some("run the tests")
    );
    assert_eq!(
        provider.calls()[0].messages.last(),
        Some(&Message::User(pa_agent::types::UserMessage {
            content: UserContent::Text(SUGGESTION_PROMPT.to_string()),
            timestamp: match provider.calls()[0].messages.last() {
                Some(Message::User(user)) => user.timestamp,
                _ => 0,
            },
        }))
    );
    provider.push_text_turn("Looks good, thanks!");
    let agent = parent(&provider, conversation);
    assert_eq!(
        prompt_suggestion(&agent, &provider.stream_fn())
            .await
            .unwrap(),
        None
    );
}

#[test]
fn the_cleanup_drops_a_wrapping_tag_and_a_leading_label() {
    assert_eq!(
        clean_suggestion("<suggestion>run it</suggestion>"),
        "run it"
    );
    assert_eq!(clean_suggestion("Suggestion: commit this"), "commit this");
    assert_eq!(clean_suggestion("Suggested reply:  push it"), "push it");
    assert_eq!(clean_suggestion("  run the tests "), "run the tests");
}

#[test]
fn the_filter_matches_claude_codes_suppression_reasons() {
    let cases = [
        ("", Some("empty")),
        ("done", Some("done")),
        ("No suggestion here", Some("meta_text")),
        ("(nothing obvious)", Some("meta_wrapped")),
        ("API Error: overloaded", Some("error_message")),
        ("Note: run tests", Some("prefixed_label")),
        ("tests", Some("too_few_words")),
        ("yes", None),
        ("/commit", None),
        (
            "one two three four five six seven eight nine ten eleven twelve thirteen",
            Some("too_many_words"),
        ),
        ("Run the tests. Then commit", Some("multiple_sentences")),
        ("run **all** tests", Some("has_formatting")),
        ("looks good to me", Some("evaluative")),
        ("Let me run the tests", Some("agent_voice")),
        ("run the tests", None),
        ("commit this", None),
    ];
    for (suggestion, expected) in cases {
        assert_eq!(suggestion_rejection(suggestion), expected, "{suggestion:?}");
    }
}
