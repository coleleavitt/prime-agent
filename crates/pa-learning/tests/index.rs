//! Learning-index behaviour the goldens do not pin by themselves (TS
//! `test/learning-index.test.ts`): a claimless commit in a day sealed by an
//! older build is no commit, and a claim on a fingerprint the index never
//! observed is reported unscored rather than counted as an improvement.

use pa_learning::{
    FingerprintDayStats,
    LearningDay,
    RefinementCommit,
    build_learning_report,
    normalize_day,
    read_learning_index,
    write_learning_day,
};
use serde_json::json;

fn failure(fingerprint: &str, count: u64) -> FingerprintDayStats {
    FingerprintDayStats {
        fingerprint: fingerprint.to_string(),
        name: "tool.execute".to_string(),
        status: "error".to_string(),
        failure: true,
        count,
        p50_ms: 0.0,
        p95_ms: 0.0,
        message: Some(String::new()),
        sampled: false,
    }
}

fn day(
    day: &str,
    fingerprints: Vec<FingerprintDayStats>,
    commits: Vec<RefinementCommit>,
) -> LearningDay {
    LearningDay {
        day: day.to_string(),
        sealed_at: String::new(),
        turns: 100,
        fingerprints,
        commits,
        parse_errors: 0.0,
        source_files: Vec::new(),
    }
}

#[test]
fn a_claimless_commit_in_an_old_day_file_is_no_commit() {
    let dir = tempfile::tempdir().unwrap();
    let mut legacy = day("2026-08-02", vec![failure("fp1", 2)], Vec::new()).to_json();
    legacy["commits"] = json!([
        { "at": "2026-08-02T00:00:00.000Z", "proposalId": "legacy", "addressed": [] },
        { "proposalId": "claimed", "addressed": ["fp1", 3] }
    ]);
    std::fs::write(dir.path().join("2026-08-02.json"), legacy.to_string()).unwrap();
    std::fs::write(dir.path().join("2026-08-03.json"), "{ truncated").unwrap();
    std::fs::write(dir.path().join("notes.json"), "{}").unwrap();
    write_learning_day(
        dir.path(),
        &day("2026-08-01", vec![failure("fp1", 4)], Vec::new()),
    )
    .unwrap();
    let days = read_learning_index(dir.path());
    assert_eq!(
        days.iter().map(|day| day.day.as_str()).collect::<Vec<_>>(),
        ["2026-08-01", "2026-08-02"]
    );
    assert_eq!(
        days[1].commits,
        [RefinementCommit {
            at: "2026-08-02".to_string(),
            proposal_id: "claimed".to_string(),
            addressed: vec!["fp1".to_string()],
        }]
    );
    assert_eq!(normalize_day(&json!({ "day": "2026-8-1" })), None);
}

#[test]
fn a_claim_on_a_fingerprint_never_observed_is_unscored() {
    let commit = RefinementCommit {
        at: "2026-08-02T12:00:00.000Z".to_string(),
        proposal_id: "refine_1".to_string(),
        addressed: vec!["fp1234567890abcd".to_string()],
    };
    let days = [
        day(
            "2026-08-01",
            vec![failure("fp00", 8), failure("fp01", 8)],
            Vec::new(),
        ),
        day("2026-08-02", vec![failure("fp00", 8)], vec![commit]),
        day(
            "2026-08-03",
            vec![failure("fp00", 1), failure("fp01", 8)],
            Vec::new(),
        ),
    ];
    let report = build_learning_report(&days, 5, 0);
    assert_eq!(
        (
            report.commits,
            report.unobserved_treated.clone(),
            report.treated.n(),
            report.untreated.n(),
            report.p_value,
            report.insufficient_evidence,
        ),
        (
            1,
            vec!["fp1234567890abcd".to_string()],
            0,
            2,
            None,
            Some("cohorts are too small (treated 0, untreated 2, minimum 5 each)".to_string()),
        )
    );
}
