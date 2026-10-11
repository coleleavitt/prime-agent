//! The recurrence filter: a fingerprint another feature judged no longer
//! worth a reminder queues no recurrence refine, unless it recurs in the
//! session's own ledger (a live recurrence always outranks the filter).

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pa_core::features::{SessionFeature, SessionFeatureContext};
use pa_core::session_engine::turn_boundary::{RefineRequester, TurnBoundaryRequests};
use pa_ledger::{FailureLedgerFeature, LedgerOptions, fingerprint_tool_result_text};
use pa_ravo::{
    RavoFeature,
    RavoOptions,
    RecurrenceFilter,
    ReplayEnvironment,
    ReplayOutcome,
    ReplayRunner,
};
use serde_json::{Value, json};

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

/// Mutes every fingerprint it is built with, and records the live
/// recurrences it was shown.
#[derive(Default)]
struct MuteAll {
    ids: HashSet<String>,
    shown: Mutex<Vec<Vec<String>>>,
}

impl RecurrenceFilter for MuteAll {
    fn muted(
        &self,
        _context: &SessionFeatureContext,
        live_recurring: &[String],
    ) -> HashSet<String> {
        self.shown.lock().unwrap().push(live_recurring.to_vec());
        self.ids.clone()
    }
}

fn message(value: Value) -> pa_agent::types::AgentMessage {
    serde_json::from_value(value).unwrap()
}

fn fingerprint() -> String {
    fingerprint_tool_result_text(Some("bash"), "boom: exit 1", true)
        .unwrap()
        .id
}

struct Session {
    context: Arc<SessionFeatureContext>,
    ledger: FailureLedgerFeature,
    requests: Arc<TurnBoundaryRequests>,
}

fn session(root: &Path, id: &str, global: bool, filter: Option<Arc<MuteAll>>) -> Session {
    let context = Arc::new(SessionFeatureContext {
        agent_dir: root.join("agent"),
        cwd: root.join("work"),
        session_id: id.to_string(),
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
        session_artifact_dir: Some(root.join("artifacts").join(id)),
    });
    let ravo = RavoFeature::new(RavoOptions {
        enabled: Some(true),
        runner: Arc::new(NeverRuns),
        replay_sys_path: Vec::new(),
    });
    if let Some(filter) = filter {
        ravo.attach_recurrence_filter(filter);
    }
    let ledger = FailureLedgerFeature::with_observers(
        LedgerOptions {
            global_ledger: Some(global),
            ..LedgerOptions::default()
        },
        vec![ravo.ledger_observer()],
    );
    ravo.attach_ledger(ledger.handle());
    ledger.on_session_start(&context, &[]);
    let requests = Arc::new(TurnBoundaryRequests::new());
    let gate = ravo.refinement_gate(&context).unwrap();
    gate.attach_refine_requester(RefineRequester::new(&requests));
    Session {
        context,
        ledger,
        requests,
    }
}

impl Session {
    /// One turn whose `bash` call fails, flushed at its end.
    fn failing_turn(&self) {
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
            self.ledger.on_message_end(&self.context, &turn);
        }
        self.ledger.on_agent_end(&self.context);
        assert!(self.ledger.handle().wait_idle(Duration::from_secs(30)));
    }
}

/// A failure an earlier session already saw once recurs globally with this
/// session's first occurrence: the filter mutes its reminder; without the
/// filter the same boundary queues the recurrence refine.
#[tokio::test]
async fn a_muted_fingerprint_recurring_globally_queues_no_refine() {
    for muted in [true, false] {
        let root = tempfile::tempdir().unwrap();
        session(root.path(), "s1", true, None).failing_turn();
        let filter = Arc::new(MuteAll {
            ids: HashSet::from([fingerprint()]),
            ..MuteAll::default()
        });
        let second = session(root.path(), "s2", true, muted.then(|| Arc::clone(&filter)));
        second.failing_turn();
        let refine = second.requests.take_refine().await;
        if muted {
            assert_eq!(refine, None);
            assert_eq!(*filter.shown.lock().unwrap(), vec![Vec::<String>::new()]);
        } else {
            let trigger = refine.expect("a recurrence refine").trigger.unwrap().data;
            assert_eq!(trigger["triggerFingerprintIds"], json!([fingerprint()]));
        }
    }
}

/// A fingerprint recurring in the session's own ledger is a live recurrence:
/// the filter is shown it and cannot mute it.
#[tokio::test]
async fn a_live_recurrence_overrides_the_filter() {
    let root = tempfile::tempdir().unwrap();
    let filter = Arc::new(MuteAll {
        ids: HashSet::from([fingerprint()]),
        ..MuteAll::default()
    });
    let only = session(root.path(), "s1", false, Some(Arc::clone(&filter)));
    only.failing_turn();
    assert_eq!(only.requests.take_refine().await, None);
    only.failing_turn();
    let trigger = only
        .requests
        .take_refine()
        .await
        .expect("a recurrence refine")
        .trigger
        .unwrap()
        .data;
    assert_eq!(trigger["triggerFingerprintIds"], json!([fingerprint()]));
    assert_eq!(*filter.shown.lock().unwrap(), vec![vec![fingerprint()]]);
}
