//! A failed tool call's fingerprint reaches the span it runs in, through the
//! generic span-attributes event (TS `_tagFailureFingerprintOnCurrentSpan`).
//! Its own test binary: `tracing` caches callsite interest per process, and
//! another test emitting the same callsite under no subscriber would race it.

use std::sync::{Arc, Mutex};

use pa_core::features::{SessionFeature, SessionFeatureContext, ToolResultObservation};
use pa_ledger::{FailureKind, FailureLedgerFeature, LedgerOptions, fingerprint_failure};
use serde_json::{Value, json};
use tracing_subscriber::layer::SubscriberExt as _;

/// Events under the span-attributes target, as (field, value) pairs.
#[derive(Clone, Default)]
struct Annotations(Arc<Mutex<Vec<(String, String)>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Annotations {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        struct Visit<'a>(&'a Mutex<Vec<(String, String)>>);
        impl tracing::field::Visit for Visit<'_> {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                self.0
                    .lock()
                    .unwrap()
                    .push((field.name().to_string(), format!("{value:?}")));
            }
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                self.0
                    .lock()
                    .unwrap()
                    .push((field.name().to_string(), value.to_string()));
            }
        }
        if event.metadata().target() == pa_types::trace_context::SPAN_ATTRIBUTES_TARGET {
            event.record(&mut Visit(&self.0));
        }
    }
}

fn result(tool: &str, text: &str, is_error: bool) -> ToolResultObservation {
    ToolResultObservation {
        tool_call_id: "c".to_string(),
        tool_name: tool.to_string(),
        args: json!({}),
        is_error,
        content: vec![pa_agent::types::ToolResultContent::text(text)],
        details: Value::Null,
        host_facts: Value::Null,
        earlier_results_of_tool: 0,
    }
}

#[tokio::test]
async fn failed_results_annotate_their_span_and_successes_do_not() {
    let root = tempfile::tempdir().unwrap();
    let context = Arc::new(SessionFeatureContext {
        agent_dir: root.path().join("agent"),
        cwd: root.path().to_path_buf(),
        session_id: "s".to_string(),
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
        session_artifact_dir: None,
    });
    let feature = FailureLedgerFeature::new(LedgerOptions {
        resolution_index: false,
        ..LedgerOptions::default()
    });
    let annotations = Annotations::default();
    let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(annotations.clone()));
    let traceback = "Traceback (most recent call last):\n  File \"<cell>\", line 1, in <module>\nKeyError: 'results'";
    let calls = [
        // A cell that raised returns normally: the traceback alone marks it.
        result("ipython", traceback, false),
        result("edit", "file not found", true),
        result("ipython", "42", false),
    ];
    for call in &calls {
        let pending = tracing::dispatcher::with_default(&dispatch, || {
            feature.after_tool_call(&context, call)
        });
        assert_eq!(pending.await, None);
    }
    let python = fingerprint_failure(
        FailureKind::PythonException,
        Some("ipython"),
        Some("KeyError"),
        "'results'",
    );
    let edit = fingerprint_failure(FailureKind::ToolError, Some("edit"), None, "file not found");
    assert_eq!(
        *annotations.0.lock().unwrap(),
        vec![
            ("failure.fingerprint".to_string(), python.id),
            ("failure.fingerprint".to_string(), edit.id),
        ]
    );
}
