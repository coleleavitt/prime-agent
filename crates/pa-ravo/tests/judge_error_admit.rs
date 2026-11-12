//! When the deep judge fails (`GateStatus::Error`, e.g. the model returns
//! an empty response or a parse-failing one) and the screen already
//! passed, the fork admits the proposal as unmeasured instead of silently
//! rejecting with `RavoRejection::Deep`. A transient judge-side model
//! failure must not kill an otherwise valid memory.

use pa_ravo::{
    AssistedRavoObservation,
    AuthorityInput,
    GateStatus,
    RAVO_DEFAULT_CONFIG,
    RavoConfig,
    RavoOpponentPool,
    UnclaimedCommitPolicy,
    authorize_assisted_ravo,
    empty_ravo_state,
};
use serde_json::{Value, json};

fn baseline() -> Value {
    json!({"entries": []})
}

fn artifact() -> Value {
    json!({
        "summary": "Record the unwrap.rs writing spec-sheet style for follow-up turns.",
        "edits": [{
            "action": "create",
            "kind": "memory",
            "id": "unwrap-rs-writing-style",
            "title": "unwrap.rs writing spec-sheet style",
            "content": "Spec-sheet style: short sections, bulleted claims.",
            "path": "projects/unwrap-rs/writing",
        }],
        "addressedFingerprints": [],
    })
}

fn empty_state() -> pa_ravo::RavoState {
    empty_ravo_state(RavoOpponentPool {
        criteria: Vec::new(),
    })
}

#[test]
fn judge_error_after_screen_pass_admits_as_unmeasured_in_fork_mode() {
    let state = empty_state();
    let artifact = artifact();
    let baseline = baseline();
    let observation = AssistedRavoObservation {
        status: GateStatus::Error,
        score: None,
        detail: Some("deep judge unavailable (EOF while parsing)".into()),
        failed_criteria: None,
        addressed_fingerprints: Vec::new(),
    };
    let authorization = authorize_assisted_ravo(&AuthorityInput {
        proposal_id: "refine-err",
        artifact: &artifact,
        baseline: &baseline,
        fast_score: 100,
        observation,
        state: Some(&state),
        config: RAVO_DEFAULT_CONFIG,
        failure_opponents: &[],
        referee_verdicts: &[],
        turn: Some(10),
        turn_clock: None,
        observation_window_turns: 20,
        unclaimed_commit: UnclaimedCommitPolicy::Unmeasured,
    });
    assert!(
        authorization.authorized,
        "fork mode: judge error on a screen-passing proposal must admit (got: {:?})",
        authorization.certificate.certificate.rejection
    );
    assert!(
        authorization.certificate.certificate.rejection.is_none(),
        "no rejection should be recorded on an admitted judge-error proposal"
    );
}

#[test]
fn judge_error_in_ts_parity_mode_still_rejects() {
    // With the flag off, the TS semantic is preserved: a judge error
    // leaves `GateStatus::Error` → `deep_pass = false` → rejection.
    let state = empty_state();
    let artifact = artifact();
    let baseline = baseline();
    let observation = AssistedRavoObservation {
        status: GateStatus::Error,
        score: None,
        detail: Some("deep judge unavailable".into()),
        failed_criteria: None,
        addressed_fingerprints: Vec::new(),
    };
    let config = RavoConfig {
        admit_screen_pass_on_judge_error: false,
        ..RAVO_DEFAULT_CONFIG
    };
    let authorization = authorize_assisted_ravo(&AuthorityInput {
        proposal_id: "refine-err-ts",
        artifact: &artifact,
        baseline: &baseline,
        fast_score: 100,
        observation,
        state: Some(&state),
        config,
        failure_opponents: &[],
        referee_verdicts: &[],
        turn: Some(10),
        turn_clock: None,
        observation_window_turns: 20,
        unclaimed_commit: UnclaimedCommitPolicy::Unmeasured,
    });
    assert!(
        !authorization.authorized,
        "TS parity mode: judge error must still reject"
    );
}
