use serde_json::json;

use super::*;
use crate::features::{answer_pending, pending_questions, register_confirmation_surface};
use crate::kernel::shared::HostRequestPayload;

fn library_question() -> Value {
    json!({
        "question": "Which library should we use for date formatting?",
        "header": "Library",
        "options": [
            { "label": "date-fns (Recommended)", "description": "Modular, tree-shakeable" },
            { "label": "Luxon", "description": "Time zones built in", "preview": "DateTime.now()" },
        ],
    })
}

fn features_question() -> Value {
    json!({
        "question": "Which features do you want to enable?",
        "header": "Features",
        "options": [
            { "label": "Search", "description": "" },
            { "label": "Export, CSV", "description": "" },
            { "label": "Sync", "description": "" },
        ],
        "multiSelect": true,
    })
}

#[test]
fn questions_follow_claude_codes_bounds_and_messages() {
    let parsed = parse_questions(&json!({ "questions": [library_question()] })).unwrap();
    assert_eq!(
        parsed,
        vec![UserQuestion {
            question: "Which library should we use for date formatting?".to_string(),
            header: "Library".to_string(),
            options: vec![
                QuestionOption {
                    label: "date-fns (Recommended)".to_string(),
                    description: "Modular, tree-shakeable".to_string(),
                    preview: None,
                },
                QuestionOption {
                    label: "Luxon".to_string(),
                    description: "Time zones built in".to_string(),
                    preview: Some("DateTime.now()".to_string()),
                },
            ],
            multi_select: false,
        }]
    );
    let error = |payload: Value| parse_questions(&payload).unwrap_err().to_string();
    assert_eq!(
        error(json!({ "questions": [] })),
        "questions must hold 1-4 questions (got 0); split more into separate calls"
    );
    assert_eq!(
        error(json!({ "questions": vec![library_question(); 5] })),
        "questions must hold 1-4 questions (got 5); split more into separate calls"
    );
    let mut single = library_question();
    single["options"] = json!([{ "label": "Only", "description": "" }]);
    // The steer wins even beside a valid question.
    assert_eq!(
        error(json!({ "questions": [features_question(), single] })),
        TOO_FEW_OPTIONS
    );
    let mut five = features_question();
    five["options"] = json!([
        { "label": "a" }, { "label": "b" }, { "label": "c" }, { "label": "d" }, { "label": "e" }
    ]);
    assert!(
        error(json!({ "questions": [five] })).starts_with("questions[0].options must hold 2-4")
    );
    assert_eq!(
        error(json!({ "questions": [library_question(), library_question()] })),
        NOT_UNIQUE
    );
    let mut repeated = features_question();
    repeated["options"][1]["label"] = json!("Search");
    assert_eq!(error(json!({ "questions": [repeated] })), NOT_UNIQUE);
    let mut headerless = library_question();
    headerless.as_object_mut().unwrap().remove("header");
    assert_eq!(
        error(json!({ "questions": [headerless] })),
        "questions[0].header is required"
    );
}

#[test]
fn multi_select_answers_join_and_split_like_claude_code() {
    let labels = vec!["Search".to_string(), "Export, CSV".to_string()];
    let joined = join_labels(&labels);
    assert_eq!(joined, "Search, \"Export, CSV\"");
    assert_eq!(split_labels(&joined), Some(labels));
    assert_eq!(
        split_labels("a, b"),
        Some(vec!["a".to_string(), "b".to_string()])
    );
    assert_eq!(split_labels("a, "), None);
    assert_eq!(split_labels("a\"b"), None);
}

#[test]
fn answers_are_admitted_with_claude_codes_limits() {
    let questions = parse_questions(&json!({
        "questions": [library_question(), features_question()]
    }))
    .unwrap();
    let admitted = admit_answer(
        &questions,
        &json!({
            "answers": {
                "Which library should we use for date formatting?": "Luxon",
                "Which features do you want to enable?": ["Search", "Export, CSV"],
            },
            "annotations": {
                "Which library should we use for date formatting?": { "preview": "DateTime.now()", "notes": "keep it small" },
            },
        }),
    )
    .unwrap();
    assert_eq!(
        admitted.answers["Which features do you want to enable?"],
        "Search, \"Export, CSV\""
    );
    let refused = |answer: Value| admit_answer(&questions, &answer).unwrap_err().to_string();
    assert_eq!(
        refused(json!({ "answers": { "Unknown?": "x" } })),
        "answers names a question that was not asked"
    );
    assert_eq!(
        refused(
            json!({ "answers": { "Which library should we use for date formatting?": ["Luxon"] } })
        ),
        "an answer must be a string (a list for a multi-select)"
    );
    assert_eq!(
        refused(
            json!({ "annotations": { "Which library should we use for date formatting?": { "preview": "forged" } } })
        ),
        "an annotation's preview must be one the question showed"
    );
    assert_eq!(
        refused(json!({ "response": "x".repeat(MAX_FIELD_CHARS + 1) })),
        "the response is too long"
    );
}

#[test]
fn the_result_text_follows_claude_codes_tool_result() {
    let questions = parse_questions(&json!({
        "questions": [library_question(), features_question()]
    }))
    .unwrap();
    let message =
        |answer: Value| answer_message(&questions, &admit_answer(&questions, &answer).unwrap());
    assert_eq!(
        message(json!({ "answers": {
            "Which library should we use for date formatting?": "Luxon",
            "Which features do you want to enable?": ["Search", "Sync"],
        } })),
        "Your questions have been answered: \"Which library should we use for date formatting?\"=\"Luxon\", \"Which features do you want to enable?\"=\"Search, Sync\". You can now continue with these answers in mind."
    );
    // A free-text "Other" answer asks the model to read carefully.
    assert_eq!(
        message(
            json!({ "answers": { "Which library should we use for date formatting?": "Neither, use Temporal" } })
        ),
        "The user answered: \"Which library should we use for date formatting?\"=\"Neither, use Temporal\". Read the answers carefully \u{2014} they may request clarification, changes, or that you not proceed \u{2014} and follow what they actually say."
    );
    assert_eq!(
        message(
            json!({ "annotations": { "Which library should we use for date formatting?": { "notes": "ask me later" } } })
        ),
        "The user answered: \"Which library should we use for date formatting?\"=(no option selected) notes: ask me later. Read the answers carefully \u{2014} they may request clarification, changes, or that you not proceed \u{2014} and follow what they actually say."
    );
    assert_eq!(
        message(
            json!({ "answers": { "Which library should we use for date formatting?": "Luxon" }, "annotations": {
            "Which library should we use for date formatting?": { "preview": "DateTime.now()" } } })
        ),
        "Your questions have been answered: \"Which library should we use for date formatting?\"=\"Luxon\" selected preview:\nDateTime.now(). You can now continue with these answers in mind."
    );
    assert_eq!(
        message(json!({ "response": "Let's talk first" })),
        "The user responded: Let's talk first"
    );
    assert_eq!(message(json!({})), "The user did not answer the questions.");
    assert_eq!(
        timed_out_message(Duration::from_secs(300)),
        "No response after 300s \u{2014} the user may be away from keyboard. Proceed using your best judgment based on the context so far; you can re-ask this question later if it's still relevant."
    );
}

#[test]
fn the_timeout_setting_reads_claude_codes_values() {
    assert_eq!(parse_timeout(None), None);
    assert_eq!(parse_timeout(Some("never")), None);
    assert_eq!(parse_timeout(Some("default")), None);
    assert_eq!(parse_timeout(Some("60s")), Some(Duration::from_secs(60)));
    assert_eq!(parse_timeout(Some("5m")), Some(Duration::from_secs(300)));
    assert_eq!(parse_timeout(Some("0s")), None);
}

async fn call(handlers: &HostRequestHandlers, data: Value) -> anyhow::Result<Value> {
    let handler = handlers.get(ASK_USER_REQUEST).unwrap().clone();
    handler(HostRequestPayload { data }).await
}

#[tokio::test]
async fn the_host_request_asks_on_the_surface_and_answers_the_kernel() {
    let session = "ask-user-host";
    let mut handlers = HostRequestHandlers::new();
    register_host_handlers(&mut handlers, session.to_string(), 0, None);
    // No surface: the print-mode refusal.
    assert_eq!(
        call(&handlers, json!({ "questions": [library_question()] }))
            .await
            .unwrap_err()
            .to_string(),
        UNAVAILABLE_MESSAGE
    );
    let _surface = register_confirmation_surface(session);
    let handlers = std::sync::Arc::new(handlers);
    let asking = {
        let handlers = std::sync::Arc::clone(&handlers);
        tokio::spawn(
            async move { call(&handlers, json!({ "questions": [library_question()] })).await },
        )
    };
    let question = loop {
        if let Some(question) = pending_questions(session).pop() {
            break question;
        }
        tokio::task::yield_now().await;
    };
    assert_eq!(question.kind, QUESTION_KIND_ASK_USER);
    assert_eq!(question.line, "waiting for your answer: Library");
    assert_eq!(
        question.payload["questions"][0]["options"][1]["preview"],
        "DateTime.now()"
    );
    assert!(answer_pending(
        session,
        &question.id,
        true,
        false,
        Some(json!({ "answers": { "Which library should we use for date formatting?": "Luxon" } }))
    ));
    assert_eq!(
        asking.await.unwrap().unwrap(),
        json!({
            "status": "answered",
            "answers": { "Which library should we use for date formatting?": "Luxon" },
            "message": "Your questions have been answered: \"Which library should we use for date formatting?\"=\"Luxon\". You can now continue with these answers in mind.",
        })
    );
    // A dismissal raises Claude Code's rejection.
    let declined = {
        let handlers = std::sync::Arc::clone(&handlers);
        tokio::spawn(
            async move { call(&handlers, json!({ "questions": [library_question()] })).await },
        )
    };
    let question = loop {
        if let Some(question) = pending_questions(session).pop() {
            break question;
        }
        tokio::task::yield_now().await;
    };
    assert!(answer_pending(session, &question.id, false, false, None));
    assert_eq!(
        declined.await.unwrap().unwrap_err().to_string(),
        DECLINED_MESSAGE
    );
}

#[tokio::test]
async fn a_subagent_and_a_timeout_answer_without_the_user() {
    let mut child = HostRequestHandlers::new();
    register_host_handlers(&mut child, "ask-user-child".to_string(), 1, None);
    assert_eq!(
        call(&child, json!({ "questions": [library_question()] }))
            .await
            .unwrap_err()
            .to_string(),
        SUBAGENT_MESSAGE
    );
    let session = "ask-user-timeout";
    let _surface = register_confirmation_surface(session);
    let questions = parse_questions(&json!({ "questions": [library_question()] })).unwrap();
    assert_eq!(
        ask(session, &questions, Some(Duration::from_millis(20)))
            .await
            .unwrap(),
        json!({ "status": "timed_out", "answers": {}, "message": timed_out_message(Duration::from_millis(20)) })
    );
}
