//! Global trust windows (committed by earlier sessions) settle at this
//! session's global ledger flushes, and the flushes' `harness.ledger.flush`
//! spans carry the `trust.*` counts. Its own test binary: the ledger
//! flushes on its own worker thread, so the span recorder must be the
//! process-wide subscriber.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pa_core::features::{SessionFeature, SessionFeatureContext};
use pa_ledger::{FailureLedgerFeature, HarnessDocument, LedgerOptions, ReplayCase};
use pa_ravo::{RavoFeature, RavoOptions, ReplayEnvironment, ReplayOutcome, ReplayRunner};
use serde_json::{json, Value};
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

impl Annotations {
    fn take(&self) -> Vec<(String, String)> {
        self.0
            .lock()
            .unwrap()
            .drain(..)
            .filter(|(span, field, _)| {
                span == "harness.ledger.flush" && field.starts_with("trust.")
            })
            .map(|(_, field, value)| (field, value))
            .collect()
    }
}

/// Every replay raises the recorded exception.
struct Raises;

impl ReplayRunner for Raises {
    fn run<'a>(
        &'a self,
        _case: &'a ReplayCase,
        _environment: ReplayEnvironment,
        _sys_path: &'a [String],
    ) -> Pin<Box<dyn Future<Output = ReplayOutcome> + Send + 'a>> {
        Box::pin(async {
            ReplayOutcome::Raised {
                exception_class: "ModuleNotFoundError".to_string(),
                detail: "ModuleNotFoundError: No module named 'prime_probe'".to_string(),
            }
        })
    }
}

fn message(value: Value) -> pa_agent::types::AgentMessage {
    serde_json::from_value(value).unwrap()
}

const TRACEBACK: &str = "Traceback (most recent call last):\n  File \"<cell>\", line 1, in <module>\nModuleNotFoundError: No module named 'prime_probe'";

fn entry(kind: &str, id: &str, reference: &Value) -> Value {
    json!({
        "id": id, "kind": kind, "title": id, "content": id, "path": "general",
        "scope": "global", "reference": reference, "arguments": {}, "metadata": {},
        "source": "refine", "created_at": "t", "updated_at": "t", "version": 1,
        "trust": { "score": 50, "updated_at": "t", "events": [] }
    })
}

struct World {
    _root: tempfile::TempDir,
    context: Arc<SessionFeatureContext>,
    ravo: RavoFeature,
    ledger: FailureLedgerFeature,
    global_dir: std::path::PathBuf,
}

fn world(windows: Value) -> World {
    let root = tempfile::tempdir().unwrap();
    let global_dir = root.path().join("agent").join("harness");
    let mut document = HarnessDocument::empty();
    document.set(
        "entries",
        json!({
            "prompt": {},
            "memory": { "note": entry("memory", "note", &json!({})) },
            "skill": { "probe": entry("skill", "probe", &json!({"type": "python", "import": "prime_probe", "callable": "run"})) },
            "subagent": {}
        }),
    );
    document.set("trustWindows", windows);
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
        runner: Arc::new(Raises),
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
    World {
        _root: root,
        context,
        ravo,
        ledger,
        global_dir,
    }
}

impl World {
    fn failing_cell(&self) {
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
                "role": "toolResult", "toolCallId": "c", "toolName": "ipython",
                "content": [{ "type": "text", "text": TRACEBACK }], "isError": true,
                "details": { "status": "error", "error": {
                    "ename": "ModuleNotFoundError",
                    "evalue": "No module named 'prime_probe'",
                    "traceback": TRACEBACK.split('\n').collect::<Vec<_>>()
                } },
                "timestamp": 1
            })),
            assistant("stop"),
        ] {
            self.ledger.on_message_end(&self.context, &turn);
        }
        let handle = self.ledger.handle();
        assert!(handle.wait_idle(Duration::from_secs(30)));
        assert!(self.ravo.wait_replay_checks(Duration::from_secs(30)));
        assert!(handle.wait_idle(Duration::from_secs(30)));
    }

    fn stored(&self) -> HarnessDocument {
        HarnessDocument::load(&self.global_dir)
    }
}

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(field, value)| ((*field).to_string(), (*value).to_string()))
        .collect()
}

#[test]
fn global_windows_settle_at_this_sessions_global_flushes() {
    let annotations = Annotations::default();
    tracing::subscriber::set_global_default(
        tracing_subscriber::registry().with(annotations.clone()),
    )
    .unwrap();

    // A memory window: its claim recurs at ordinal 1 (inside 0..=1) and it
    // closes contested at the flush that passes its end.
    let fingerprint = pa_ledger::fingerprint_tool_result_text(Some("ipython"), TRACEBACK, true)
        .unwrap()
        .id;
    let memory = world(json!({
        "p1": {
            "proposalId": "p1", "touched": ["memory:note"], "claimedFingerprints": [fingerprint],
            "committedTurn": 0, "untilTurn": 1, "outcome": "open"
        }
    }));
    memory.failing_cell();
    assert_eq!(
        memory.stored().get("trustWindows").unwrap()["p1"]["recurrences"],
        json!({ fingerprint.clone(): 1 })
    );
    assert_eq!(
        annotations.take(),
        pairs(&[
            ("trust.recurrences", "1"),
            ("trust.adjudications", "0"),
            ("trust.faulted", "0"),
            ("trust.clean", "0"),
            ("trust.contested", "0"),
        ])
    );
    memory.failing_cell();
    let window = memory.stored().get("trustWindows").unwrap()["p1"].clone();
    assert_eq!(
        (&window["outcome"], &window["settledTurn"]),
        (&json!("contested"), &json!(2))
    );
    assert!(annotations
        .take()
        .contains(&("trust.contested".to_string(), "1".to_string())));

    // A skill window: the recurrence's case is self-checked, the awaiting
    // replay is released, raises (upheld), and its verdict is flushed to the
    // global state from the replay's own thread.
    let skill = world(json!({
        "p2": {
            "proposalId": "p2", "touched": ["skill:probe"], "claimedFingerprints": [fingerprint],
            "committedTurn": 0, "untilTurn": 20, "outcome": "open",
            "skillImports": { "skill:probe": ["prime_probe"] }
        }
    }));
    let _ = annotations.take();
    skill.failing_cell();
    let stored = skill.stored();
    let window = stored.get("trustWindows").unwrap()["p2"].clone();
    assert_eq!(
        (
            &window["outcome"],
            &window["faultedEntries"],
            &window["adjudications"][0]["status"]
        ),
        (&json!("faulted"), &json!(["skill:probe"]), &json!("upheld"))
    );
    assert_eq!(
        stored.get("entries").unwrap()["skill"]["probe"]["trust"]["score"],
        json!(35)
    );
    assert_eq!(
        stored.get("entries").unwrap()["memory"]["note"]["trust"]["score"],
        json!(50)
    );
    let flushed = annotations.take();
    assert!(
        flushed.contains(&("trust.adjudications".to_string(), "1".to_string()))
            && flushed.contains(&("trust.faulted".to_string(), "1".to_string())),
        "{flushed:?}"
    );
}
