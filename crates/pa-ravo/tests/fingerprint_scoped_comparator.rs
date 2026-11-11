//! A challenger is compared only to prior champions that competed on the
//! same fingerprints: an ONNX-preprocessing champion at 95 must not gate a
//! sub_once-anchor challenger scoring 70, because the two address disjoint
//! failure fingerprints. Before fingerprint-scoping, `ravo_best_score` was
//! `max` over a single flat lineage, so a strong unrelated champion
//! permanently raised the deep bar across every topic; now the bar is 0
//! (screen threshold only) for a challenger whose fingerprints do not
//! overlap any prior champion's claim.

use pa_ravo::{
    GateStatus,
    RavoChampion,
    RavoConfig,
    RavoEvaluation,
    RavoObservation,
    RavoOpponentPool,
    RavoProposal,
    empty_ravo_state,
    ravo_best_score,
    ravo_best_score_scoped,
    ravo_step,
};
use serde_json::{Map, Value, json};

fn empty_state() -> pa_ravo::RavoState {
    empty_ravo_state(RavoOpponentPool {
        criteria: Vec::new(),
    })
}

fn state_with_champion(champion: RavoChampion) -> pa_ravo::RavoState {
    let mut state = empty_state();
    state
        .evaluated_proposal_ids
        .push(champion.proposal_id.clone());
    state.champion_id = Some(champion.proposal_id.clone());
    state.lineage.push(champion);
    state
}

fn champion(proposal_id: &str, score: u64, claimed: &[&str]) -> RavoChampion {
    RavoChampion {
        proposal_id: proposal_id.to_string(),
        parent_id: None,
        score,
        artifact: Value::Null,
        missed_criterion_ids: Vec::new(),
        claimed_fingerprints: Some(claimed.iter().map(|id| (*id).to_string()).collect()),
        provisional: None,
        extra: Map::new(),
    }
}

fn fps(ids: &[&str]) -> Vec<String> {
    ids.iter().map(|id| (*id).to_string()).collect()
}

#[test]
fn best_score_is_scoped_to_prior_champions_on_overlapping_fingerprints() {
    let lineage = vec![
        champion("onnx", 95, &["onnx-prep"]),
        champion("nvd", 80, &["nvd-dns"]),
    ];

    // Disjoint challenger: uncontested territory, floor collapses to 0.
    assert_eq!(ravo_best_score(&lineage, &fps(&["sub-once-anchor"])), 0);

    // Overlaps one champion: that champion's score is the bar.
    assert_eq!(ravo_best_score(&lineage, &fps(&["onnx-prep"])), 95);
    assert_eq!(ravo_best_score(&lineage, &fps(&["nvd-dns"])), 80);

    // Overlaps both: the higher of the overlapping champions wins.
    assert_eq!(
        ravo_best_score(&lineage, &fps(&["onnx-prep", "nvd-dns"])),
        95
    );

    // Empty challenger fingerprints: `ravo_best_score` preserves TS parity
    // (global max over the lineage). The fork's scoped variant is the one
    // the production gate calls when `scope_unclaimed_bar_to_zero` is set;
    // it treats an unclaimed challenger as uncontested territory, so a
    // high-scoring champion on an unrelated topic never ratchets the floor.
    assert_eq!(ravo_best_score(&lineage, &[]), 95);
    assert_eq!(ravo_best_score_scoped(&lineage, &[]), 0);

    // Empty lineage: uncontested under either semantic.
    assert_eq!(ravo_best_score(&[], &fps(&["anything"])), 0);
    assert_eq!(ravo_best_score(&[], &[]), 0);
    assert_eq!(ravo_best_score_scoped(&[], &[]), 0);
}

#[test]
fn a_champion_without_claims_never_gates_any_challenger() {
    // Fact-memory champions (no fingerprint claims) must not raise the bar
    // for anyone, because they provide no evidence a challenger needs to
    // beat. Mirrors how `open_trust_window` refuses to open on an empty
    // `claimed_fingerprints` set.
    let lineage = vec![
        RavoChampion {
            proposal_id: "fact".into(),
            parent_id: None,
            score: 99,
            artifact: Value::Null,
            missed_criterion_ids: Vec::new(),
            claimed_fingerprints: None,
            provisional: None,
            extra: Map::new(),
        },
        champion("onnx", 95, &["onnx-prep"]),
    ];
    assert_eq!(ravo_best_score(&lineage, &fps(&["sub-once-anchor"])), 0);
    assert_eq!(ravo_best_score(&lineage, &fps(&["onnx-prep"])), 95);
}

#[test]
fn ravo_step_admits_a_disjoint_challenger_that_the_old_global_max_would_have_blocked() {
    // The regression this unblocks: an ONNX champion at 95 locks the global
    // bar at 85 (95 - deep_tolerance=10), and a sub_once-anchor proposal at
    // 70 is rejected forever, even though the two edits never compete.
    // Fingerprint-scoping lets the 70-score proposal through on the first
    // turn its fingerprints are addressed, because no prior champion has
    // ever claimed them.
    let state = state_with_champion(champion("onnx-champion", 95, &["onnx-prep"]));

    let proposal = RavoProposal {
        id: "sub-once".into(),
        artifact: json!({
            "summary": "anchor-on-current-file-text policy",
            "edits": [{
                "action": "create",
                "kind": "prompt",
                "id": "repl-anchor-note",
                "title": "Anchor on current file text",
                "content": "Re-read the file right before the edit.",
                "path": "policies/repl-file-edits",
                "targetsFingerprints": ["sub-once-anchor"],
            }],
            "addressedFingerprints": ["sub-once-anchor"],
        }),
    };
    let evaluation = RavoEvaluation {
        proposal_id: "sub-once".into(),
        screen: RavoObservation {
            status: GateStatus::Pass,
            score: Some(100),
            detail: None,
        },
        deep: RavoObservation {
            status: GateStatus::Pass,
            score: Some(70),
            detail: None,
        },
        criteria: Vec::new(),
    };
    let config = RavoConfig {
        screen_threshold: 50,
        epsilon: 1,
        deep_tolerance: 10,
        scope_unclaimed_bar_to_zero: true,
        admit_screen_pass_on_judge_error: false,
    };

    let result = ravo_step(&state, &proposal, &evaluation, &config);

    // The challenger's bar is 0 (no overlapping champion), so a deep score
    // of 70 clears it; the certificate records the bar it actually faced.
    assert_eq!(result.certificate.previous_best_score, 0);
    assert!(
        result.certificate.rejection.is_none(),
        "disjoint-fingerprint challenger was rejected: {:?}",
        result.certificate.rejection
    );
    assert!(result.certificate.committed);
}

#[test]
fn ravo_step_still_gates_a_challenger_competing_on_the_same_fingerprint() {
    // The other half of the contract: when the challenger DOES overlap a
    // champion's claim, the bar is the champion's score minus tolerance
    // (unchanged from the pre-scoping behavior for the overlap case).
    let state = state_with_champion(champion("onnx-champion", 95, &["onnx-prep"]));

    let proposal = RavoProposal {
        id: "onnx-challenger".into(),
        artifact: json!({
            "summary": "ONNX preprocessing refinement",
            "edits": [{
                "action": "create",
                "kind": "memory",
                "id": "onnx-note",
                "title": "ONNX preprocessing",
                "content": "Use NCHW for RGB.",
                "path": "notes/onnx",
                "targetsFingerprints": ["onnx-prep"],
            }],
            "addressedFingerprints": ["onnx-prep"],
        }),
    };
    let evaluation = RavoEvaluation {
        proposal_id: "onnx-challenger".into(),
        screen: RavoObservation {
            status: GateStatus::Pass,
            score: Some(100),
            detail: None,
        },
        // Score 70 + tolerance 10 = 80, below the champion's 95: must reject.
        deep: RavoObservation {
            status: GateStatus::Pass,
            score: Some(70),
            detail: None,
        },
        criteria: Vec::new(),
    };
    let config = RavoConfig {
        screen_threshold: 50,
        epsilon: 1,
        deep_tolerance: 10,
        scope_unclaimed_bar_to_zero: true,
        admit_screen_pass_on_judge_error: false,
    };

    let result = ravo_step(&state, &proposal, &evaluation, &config);

    assert_eq!(result.certificate.previous_best_score, 95);
    assert!(
        result.certificate.rejection.is_some(),
        "overlapping-fingerprint challenger should have been gated"
    );
    assert!(!result.certificate.committed);
}
