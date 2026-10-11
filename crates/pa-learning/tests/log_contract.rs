//! The structured-log contract the learning index reads, written by this
//! port's own producers through the `pa-trace` recorder: an `agent.turn`
//! span is a turn, a failed `tool.execute` span annotated with
//! `failure.fingerprint` (how `pa-ledger` stamps it) rolls up under that
//! fingerprint, and the `refinement.committed` event `pa-ravo` writes
//! (fields as `tracing` records them: `proposal_id`, and `addressed` joined
//! with commas) is a treated-cohort commit.

use std::time::Duration;

use pa_learning::{RefinementCommit, roll_up_learning_days};
use pa_types::trace_context::SPAN_ATTRIBUTES_TARGET;
use tracing_subscriber::layer::SubscriberExt as _;

#[test]
fn records_this_port_writes_roll_up_into_turns_fingerprints_and_commits() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("agent.jsonl");
    let (layer, handle) = pa_trace::recorder(pa_trace::RecorderConfig {
        log_path: log.clone(),
        inbound: None,
        otlp: None,
    });
    tracing::subscriber::with_default(tracing_subscriber::registry().with(layer), || {
        let turn = tracing::info_span!(target: "pa_agent::agent_loop", "agent.turn");
        let _turn = turn.enter();
        let tool = tracing::info_span!(target: "pa_core::tools", "tool.execute", error = tracing::field::Empty);
        {
            let _tool = tool.enter();
            // How `pa-ledger` stamps it (`FailureLedgerFeature::after_tool_call`).
            tracing::event!(
                target: SPAN_ATTRIBUTES_TARGET,
                tracing::Level::INFO,
                failure.fingerprint = "0123456789abcdef"
            );
            tool.record("error", "KeyError: 'results'");
        }
        drop(tool);
        // The `pa-ravo` outcome line (`RavoVerdict::report_outcome`).
        let addressed = ["0123456789abcdef", "fedcba9876543210"].join(",");
        tracing::info!(
            target: pa_ravo::REFINEMENT_LOG_TARGET,
            proposal_id = "refine_1",
            addressed,
            deep_score = 80.0,
            missed = 0,
            reason = "manual",
            scope = "local",
            "refinement.committed"
        );
    });
    assert!(handle.flush(Duration::from_secs(10)));
    let rolled = roll_up_learning_days(&[log], pa_ledger::now_millis()).unwrap();
    assert_eq!(rolled.parse_errors, 0);
    let [day] = rolled.days.as_slice() else {
        panic!("one day: {:?}", rolled.days);
    };
    assert_eq!(day.turns, 1);
    let failure = day
        .fingerprints
        .iter()
        .find(|stat| stat.failure)
        .expect("the failed tool call");
    assert_eq!(
        (
            failure.fingerprint.as_str(),
            failure.name.as_str(),
            failure.count,
            failure.message.as_deref()
        ),
        ("0123456789abcdef", "tool.execute", 1, Some("keyerror: ?"))
    );
    let [commit] = day.commits.as_slice() else {
        panic!("one commit: {:?}", day.commits);
    };
    assert_eq!(
        commit,
        &RefinementCommit {
            at: commit.at.clone(),
            proposal_id: "refine_1".to_string(),
            addressed: vec![
                "0123456789abcdef".to_string(),
                "fedcba9876543210".to_string()
            ],
        }
    );
    assert_eq!(&commit.at[..10], day.day);
}
