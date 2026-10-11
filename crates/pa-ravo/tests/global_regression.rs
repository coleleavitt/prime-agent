//! A global champion committed by an earlier session regresses in this one:
//! its claimed failure recurs inside its window on the global ordinal, the
//! global ledger flush records the recurrence on it under the harness-state
//! lock, and the flush's `harness.ledger.flush` span carries
//! `ledger.regressions`. Its own test binary: the ledger flushes on its own
//! worker thread, so the span recorder must be the process-wide subscriber.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use pa_core::features::{SessionFeature, SessionFeatureContext};
use pa_ledger::{
    FailureLedgerFeature,
    HarnessDocument,
    LedgerOptions,
    fingerprint_tool_result_text,
};
use pa_ravo::{RavoFeature, RavoOptions, ReplayEnvironment, ReplayOutcome, ReplayRunner};
use serde_json::{Value, json};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::registry::LookupSpan;

/// Span-attribute events, as (span name, field, value).
#[derive(Clone, Default)]
struct Annotations(Arc<Mutex<Vec<(String, String, String)>>>);

impl<S> tracing_subscriber::Layer<S> for Annotations
where
    S: tracing::Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_event(&self, event: &tracing::Event<'_>, ctx: tracing_subscriber::layer::Context<'_, S>) {
        struct Visit(Vec<(String, String)>);
        impl tracing::field::Visit for Visit {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0
                    .push((field.name().to_string(), format!("{value:?}")));
            }
        }
        if event.metadata().target() != pa_types::trace_context::SPAN_ATTRIBUTES_TARGET {
            return;
        }
        let span = ctx
            .event_span(event)
            .map(|span| span.name().to_string())
            .unwrap_or_default();
        let mut visit = Visit(Vec::new());
        event.record(&mut visit);
        let mut recorded = self.0.lock().unwrap();
        for (field, value) in visit.0 {
            recorded.push((span.clone(), field, value));
        }
    }
}

struct NeverRuns;

impl ReplayRunner for NeverRuns {
    fn run<'a>(
        &'a self,
        _case: &'a pa_ledger::ReplayCase,
        _environment: ReplayEnvironment,
        _sys_path: &'a [String],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ReplayOutcome> + Send + 'a>> {
        panic!("no replay runs at a turn boundary");
    }
}

fn message(value: Value) -> pa_agent::types::AgentMessage {
    serde_json::from_value(value).unwrap()
}

#[test]
fn a_global_champion_regresses_on_the_global_ordinal() {
    let annotations = Annotations::default();
    tracing::subscriber::set_global_default(
        tracing_subscriber::registry().with(annotations.clone()),
    )
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    let fingerprint = fingerprint_tool_result_text(Some("bash"), "boom: exit 1", true)
        .unwrap()
        .id;
    let global_dir = root.path().join("agent").join("harness");
    let champion = json!({
        "proposalId": "p1",
        "parentId": null,
        "score": 70,
        "artifact": null,
        "missedCriterionIds": [],
        "claimedFingerprints": [fingerprint],
        "provisional": { "committedTurn": 0, "untilTurn": 20, "clock": "ordinal" }
    });
    let mut document = HarnessDocument::empty();
    document.set(
        "ravo",
        json!({
            "lineage": [champion],
            "championId": "p1",
            "opponents": { "criteria": [] },
            "evaluatedProposalIds": ["p1"]
        }),
    );
    document.save(&global_dir).unwrap();

    let context = Arc::new(SessionFeatureContext {
        agent_dir: root.path().join("agent"),
        cwd: root.path().join("work"),
        session_id: "s2".to_string(),
        python_skill_import_names: Vec::new(),
        model: serde_json::from_value(json!({
            "id": "m1", "name": "M1", "api": "test", "provider": "p1",
            "baseUrl": "http://localhost", "reasoning": false,
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
            "contextWindow": 1000, "maxTokens": 100
        }))
        .unwrap(),
        telemetry: None,
        rlm_depth: 0,
        session_artifact_dir: Some(root.path().join("artifacts")),
    });
    let ravo = RavoFeature::new(RavoOptions {
        enabled: Some(true),
        runner: Arc::new(NeverRuns),
        replay_sys_path: Vec::new(),
    });
    let ledger = FailureLedgerFeature::with_observers(
        LedgerOptions {
            global_ledger: Some(true),
            ..LedgerOptions::default()
        },
        vec![ravo.ledger_observer()],
    );
    ravo.attach_ledger(ledger.handle());
    ledger.on_session_start(&context, &[]);
    let assistant = |stop: &str| {
        message(json!({
            "role": "assistant", "content": [], "api": "test", "provider": "p1", "model": "m1",
            "usage": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0, "totalTokens": 0 },
            "stopReason": stop, "timestamp": 1
        }))
    };
    for turn in [
        message(json!({ "role": "user", "content": "go", "timestamp": 1 })),
        assistant("toolUse"),
        message(json!({
            "role": "toolResult", "toolCallId": "c", "toolName": "bash",
            "content": [{ "type": "text", "text": "boom: exit 1" }], "isError": true,
            "timestamp": 1
        })),
        assistant("stop"),
    ] {
        ledger.on_message_end(&context, &turn);
    }
    assert!(ledger.handle().wait_idle(Duration::from_secs(30)));

    let stored = HarnessDocument::load(&global_dir);
    assert_eq!(
        stored.get("ravo").unwrap()["lineage"][0]["provisional"],
        json!({
            "committedTurn": 0,
            "untilTurn": 20,
            "clock": "ordinal",
            "observedRecurrence": { "turn": 1, "fingerprints": [fingerprint] }
        })
    );
    let annotations = annotations.0.lock().unwrap().clone();
    assert!(
        annotations.contains(&(
            "harness.ledger.flush".to_string(),
            "ledger.regressions".to_string(),
            "1".to_string()
        )),
        "{annotations:?}"
    );
}
