//! The goal evaluator's verdict parsing and its effect on the goal.

use pa_agent::scripted::ScriptedProvider;

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

fn assistant(text: &str, stop_reason: StopReason) -> pa_agent::types::AssistantMessage {
    pa_agent::types::AssistantMessage {
        content: vec![pa_agent::types::AssistantContent::Text(
            pa_agent::types::TextContent {
                text: text.to_string(),
                text_signature: None,
            },
        )],
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
    }
}

fn stop_turn(text: &str) -> ShouldStopAfterTurnContext {
    let message = assistant(text, StopReason::Stop);
    ShouldStopAfterTurnContext {
        message: message.clone(),
        tool_results: Vec::new(),
        context: pa_agent::types::AgentContext {
            system_prompt: String::new(),
            messages: vec![
                AgentMessage::Standard(pa_agent::types::Message::User(
                    pa_agent::types::UserMessage {
                        content: pa_agent::types::UserContent::Text("make the tests pass".into()),
                        timestamp: 0,
                    },
                )),
                AgentMessage::Standard(pa_agent::types::Message::Assistant(message)),
            ],
            tools: Vec::new(),
        },
        new_messages: Vec::new(),
    }
}

struct Fixture {
    provider: Arc<ScriptedProvider>,
    evaluator: GoalEvaluator,
    _dir: crate::test_support::ThreadTempDir,
}

fn fixture(objective: Option<&str>) -> Fixture {
    let dir = crate::test_support::ThreadTempDir::new();
    let session_dir = dir.path().join("session");
    std::fs::create_dir_all(&session_dir).unwrap();
    let mut session = SessionManager::in_memory(dir.path());
    session.materialize_session_file(Some(session_dir));
    let mut driver = GoalDriver::new();
    if let Some(objective) = objective {
        driver.start(&mut session, objective, None).unwrap();
    }
    let provider = Arc::new(ScriptedProvider::new(test_model()));
    Fixture {
        evaluator: GoalEvaluator {
            side: SideModel::Session {
                stream_fn: provider.stream_fn(),
                model: test_model(),
            },
            driver: Arc::new(tokio::sync::Mutex::new(driver)),
            session: Arc::new(tokio::sync::Mutex::new(session)),
        },
        provider,
        _dir: dir,
    }
}

#[test]
fn verdicts_parse_from_bare_or_fenced_json() {
    assert_eq!(
        parse_verdict(r#"{"ok": true, "reason": "tests pass"}"#),
        Some(GoalVerdict::Met {
            reason: "tests pass".into()
        })
    );
    assert_eq!(
        parse_verdict("```json\n{\"ok\": false, \"reason\": \"2 failing\"}\n```"),
        Some(GoalVerdict::NotMet {
            reason: "2 failing".into()
        })
    );
    assert_eq!(
        parse_verdict(r#"{"ok": false, "impossible": true, "reason": "no network"}"#),
        Some(GoalVerdict::Impossible {
            reason: "no network".into()
        })
    );
    assert_eq!(parse_verdict("sure, looks done"), None);
    assert_eq!(parse_verdict(r#"{"reason": "no ok"}"#), None);
}

#[tokio::test]
async fn a_met_verdict_completes_the_goal_and_the_evaluator_reads_the_transcript() {
    let fixture = fixture(Some("all tests pass"));
    fixture
        .provider
        .push_text_turn(r#"{"ok": true, "reason": "\"12 passed\""}"#);
    let outcome = fixture
        .evaluator
        .evaluate_turn(&stop_turn("12 passed"))
        .await;
    assert_eq!(
        outcome,
        EvaluatorOutcome::Settled(GoalVerdict::Met {
            reason: "\"12 passed\"".into()
        })
    );
    assert_eq!(
        fixture.evaluator.driver.lock().await.state().status,
        GoalStatus::Complete
    );
    let calls = fixture.provider.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].system_prompt.as_deref(),
        Some(GOAL_EVALUATOR_SYSTEM_PROMPT)
    );
    let pa_agent::types::Message::User(user) = &calls[0].messages[0] else {
        panic!("expected the evaluation prompt");
    };
    let pa_agent::types::UserContent::Text(text) = &user.content else {
        panic!("expected text");
    };
    assert_eq!(
        text,
        &evaluation_prompt(
            "User: make the tests pass\n\nAssistant: 12 passed",
            "all tests pass"
        )
    );
}

#[tokio::test]
async fn a_not_met_verdict_continues_with_the_reason_and_keeps_the_goal() {
    let fixture = fixture(Some("all tests pass"));
    fixture
        .provider
        .push_text_turn(r#"{"ok": false, "reason": "2 tests fail"}"#);
    let outcome = fixture.evaluator.evaluate_turn(&stop_turn("done?")).await;
    assert_eq!(
        outcome,
        EvaluatorOutcome::Continued(not_met_text("all tests pass", "2 tests fail"))
    );
    assert_eq!(
        fixture.evaluator.driver.lock().await.state().status,
        GoalStatus::Active
    );
}

#[tokio::test]
async fn an_impossible_verdict_pauses_the_goal_with_the_reason() {
    let fixture = fixture(Some("deploy to prod"));
    fixture
        .provider
        .push_text_turn(r#"{"ok": false, "impossible": true, "reason": "no credentials"}"#);
    let outcome = fixture.evaluator.evaluate_turn(&stop_turn("cannot")).await;
    assert_eq!(
        outcome,
        EvaluatorOutcome::Settled(GoalVerdict::Impossible {
            reason: "no credentials".into()
        })
    );
    let driver = fixture.evaluator.driver.lock().await;
    assert_eq!(driver.state().status, GoalStatus::Paused);
    assert_eq!(
        driver.state().last_reason.as_deref(),
        Some("Goal evaluator: impossible: no credentials")
    );
}

#[tokio::test]
async fn no_goal_a_tool_turn_or_the_cap_skips_without_a_model_call() {
    let fixture_without = fixture(None);
    assert_eq!(
        fixture_without
            .evaluator
            .evaluate_turn(&stop_turn("hi"))
            .await,
        EvaluatorOutcome::Skipped
    );
    let fixture = fixture(Some("all tests pass"));
    let mut cut = stop_turn("partial");
    cut.message.stop_reason = StopReason::Length;
    assert_eq!(
        fixture.evaluator.evaluate_turn(&cut).await,
        EvaluatorOutcome::Skipped
    );
    let mut capped = stop_turn("again");
    let row = super::super::session_message_to_loop(&super::super::SessionAgentMessage::Custom(
        super::super::plan_mode::hidden_row(GOAL_EVALUATOR_CUSTOM_TYPE, "x"),
    ))
    .unwrap();
    capped.new_messages = vec![row; MAX_EVALUATOR_BLOCKS_PER_RUN];
    assert_eq!(
        fixture.evaluator.evaluate_turn(&capped).await,
        EvaluatorOutcome::Skipped
    );
    assert!(fixture.provider.calls().is_empty());
    assert!(fixture_without.provider.calls().is_empty());
}

#[tokio::test]
async fn an_unparseable_reply_fails_and_leaves_the_goal_to_its_continuation() {
    let fixture = fixture(Some("all tests pass"));
    fixture.provider.push_text_turn("I think so");
    assert_eq!(
        fixture.evaluator.evaluate_turn(&stop_turn("done")).await,
        EvaluatorOutcome::Failed("the evaluator reply is not a verdict".into())
    );
    assert_eq!(
        fixture.evaluator.driver.lock().await.state().status,
        GoalStatus::Active
    );
}
