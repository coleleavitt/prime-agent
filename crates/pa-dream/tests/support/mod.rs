//! Shared builders for the ported TS tests.

#![allow(dead_code)] // each test binary uses a subset

pub mod spans;
pub mod stub;

use std::path::PathBuf;

use pa_dream::improve::{CandidateInput, CandidateSource};
use pa_dream::policy::{ExplorationPolicy, DEFAULT_POLICY};
use pa_dream::records::{NodeRecord, NodeTag, TreeHeaderRecord, TreeRecord, TreeTag};
use pa_dream::rng::{Seed, SeededRng};
use pa_dream::store::{parse_records, RecordedTree};

/// A header for a synthetic tree.
pub fn header(tree_id: &str, w: u32) -> TreeHeaderRecord {
    TreeHeaderRecord {
        record_type: TreeTag::Tag,
        version: 1,
        tree_id: tree_id.to_string(),
        task_id: "synthetic".to_string(),
        n: None,
        w,
        seed: Seed::Number(1),
        policy_id: "p".to_string(),
        iteration: 0,
        created_ts: 0,
    }
}

/// A node line without provenance (as a pre-provenance file has it).
pub fn node(id: &str, parent: Option<&str>, seq: u32, score: f64) -> NodeRecord {
    NodeRecord {
        record_type: NodeTag::Tag,
        id: id.to_string(),
        parent_id: parent.map(str::to_string),
        branch: 0,
        seq,
        round: 0,
        score,
        valid: true,
        fail_class: None,
        origin: None,
        artifact_ref: "ref".to_string(),
        tokens: 0,
        ts: 0,
    }
}

/// `(seq, parent seq, round, branch, score)` rows as a tree named `tree_id`.
pub fn tree(tree_id: &str, w: u32, rows: &[(u32, Option<u32>, u32, u32, f64)]) -> RecordedTree {
    let mut records = vec![TreeRecord::Header(header(tree_id, w))];
    for (seq, parent, round, branch, score) in rows {
        let parent_id = parent.map(|parent| format!("{tree_id}-n{parent}"));
        let mut record = node(
            &format!("{tree_id}-n{seq}"),
            parent_id.as_deref(),
            *seq,
            *score,
        );
        record.round = *round;
        record.branch = *branch;
        records.push(TreeRecord::Node(record));
    }
    RecordedTree::from_records(records).expect("tree")
}

/// A recorded fixture under `tests/fixtures`.
pub fn fixture(name: &str) -> RecordedTree {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    let text = std::fs::read_to_string(path).expect("fixture");
    RecordedTree::from_records(parse_records(&text).expect("records")).expect("tree")
}

/// `{...DEFAULT_POLICY, ...over}`.
pub fn policy(over: impl FnOnce(&mut ExplorationPolicy)) -> ExplorationPolicy {
    let mut policy = DEFAULT_POLICY;
    over(&mut policy);
    policy
}

/// Bare candidates (origin `local`).
pub fn local(policies: &[ExplorationPolicy]) -> Vec<CandidateInput> {
    policies.iter().copied().map(CandidateInput::from).collect()
}

/// An rng from a numeric seed.
pub fn rng(seed: i64) -> SeededRng {
    SeededRng::new(&Seed::Number(seed))
}

/// A candidate source that always proposes the same list.
pub struct Fixed(pub Vec<CandidateInput>);

impl CandidateSource for Fixed {
    fn propose(
        &mut self,
        _current: &ExplorationPolicy,
        _m: usize,
        _rng: &SeededRng,
    ) -> Vec<CandidateInput> {
        self.0.clone()
    }
}

/// `|a - b| < 10^-digits / 2`, vitest's `toBeCloseTo`.
pub fn close(actual: f64, expected: f64, digits: i32) -> bool {
    (actual - expected).abs() < 10f64.powi(-digits) / 2.0
}

#[macro_export]
macro_rules! assert_close {
    ($actual:expr, $expected:expr, $digits:expr) => {{
        let (actual, expected): (f64, f64) = ($actual, $expected);
        assert!(
            $crate::support::close(actual, expected, $digits),
            "{} = {actual} is not within 1e-{} of {expected}",
            stringify!($actual),
            $digits
        );
    }};
}
