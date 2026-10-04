//! The TS `dream-replay.test.ts` and `dream-rollout.test.ts` behaviour: replay
//! accounting and legality, the shared improvement epsilon, the store
//! round-trip, and rollout provenance and determinism.

// Exact float equality is the claim: replay and the objective are deterministic
// IEEE-754 arithmetic, and the expected values are exact or recorded.
#![allow(clippy::float_cmp)]

mod support;

use std::path::Path;

use pa_dream::improve::CandidateOrigin;
use pa_dream::interpreter::{assert_legal_batch, interpret_policy, ObservationView, IMPROVE_EPS};
use pa_dream::policy::{ExplorationPolicy, SelectionRule, StopRule, DEFAULT_POLICY};
use pa_dream::proposer::{LocalProposer, ProposeOutcome, Proposer};
use pa_dream::records::{NodeOrigin, RevealRecord, RevealTag, TreeRecord};
use pa_dream::replay::{simulate_policy, ReplayConfig, ReplaySimulator};
use pa_dream::rng::{Seed, SeededRng};
use pa_dream::rollout::{
    attempt_rng_label, improvements_of, run_online_exploration, ExploreOptions, ExploreResult,
    ScoreImprovement,
};
use pa_dream::store::{read_tree, RecordedTree, TreeWriter};
use pa_dream::task::{
    Artifact, ArtifactShapeError, DynTask, Evaluation, ProposeParams, ScoredTask,
};
use pa_dream::tasks::{resolve_task, DreamTaskId};
use serde_json::{json, Value};
use support::{policy, tree};

fn t1() -> RecordedTree {
    tree(
        "t1",
        2,
        &[
            (0, None, 0, 0, 0.3),
            (1, Some(0), 0, 0, 0.5),
            (2, Some(1), 0, 0, 0.7),
            (3, Some(0), 0, 1, 0.4),
        ],
    )
}

fn t2() -> RecordedTree {
    tree(
        "t2",
        2,
        &[
            (0, None, 0, 0, 0.2),
            (1, Some(0), 0, 0, 0.6),
            (2, Some(1), 0, 0, 0.8),
        ],
    )
}

fn best_first() -> ExplorationPolicy {
    policy(|p| {
        p.selection_rule = SelectionRule::BestFirst;
        p.stop_rule = StopRule::Never;
        p.batch_size = 2;
    })
}

fn explore_root_never() -> ExplorationPolicy {
    policy(|p| {
        p.selection_rule = SelectionRule::ExploreRoot;
        p.stop_rule = StopRule::Never;
        p.batch_size = 1;
    })
}

fn explore_root_patience() -> ExplorationPolicy {
    policy(|p| {
        p.selection_rule = SelectionRule::ExploreRoot;
        p.stop_rule = StopRule::Patience;
        p.beta = 1;
        p.batch_size = 1;
    })
}

fn k2(k2: u32) -> ReplayConfig {
    ReplayConfig { k2 }
}

#[test]
fn replay_reveals_recorded_children_and_charges_out_of_support_cells() {
    let first = simulate_policy(&t1(), &best_first(), k2(10));
    assert_eq!(first, simulate_policy(&t1(), &best_first(), k2(10)));
    assert_eq!(first.revealed_ids, ["t1-n0", "t1-n1", "t1-n2", "t1-n3"]);
    assert_eq!((first.n, first.rounds, first.best_score), (3, 3, 0.7));
    assert_eq!(
        (
            first.out_of_support_cells,
            first.selected_cells,
            first.in_support
        ),
        (1, 4, 0.75)
    );
    assert_eq!(first.best_so_far, [0.5, 0.7, 0.7, 0.7]);
    assert_eq!((first.probes_to_best, first.rounds_to_best), (2, 2));

    let chain = simulate_policy(&t2(), &explore_root_never(), k2(5));
    assert_eq!(chain.revealed_ids, ["t2-n0", "t2-n1"]);
    assert_eq!(
        (
            chain.out_of_support_cells,
            chain.selected_cells,
            chain.rounds
        ),
        (4, 5, 5)
    );
    assert!((chain.in_support - 0.2).abs() < 1e-12);
    assert_eq!(chain.best_so_far, [0.6; 5]);
    assert_eq!((chain.probes_to_best, chain.rounds_to_best), (1, 1));

    let wide = tree(
        "t5",
        3,
        &[
            (0, None, 0, 0, 0.3),
            (1, Some(0), 0, 0, 0.5),
            (2, Some(1), 0, 0, 0.7),
            (3, Some(0), 0, 1, 0.4),
        ],
    );
    let wide_policy = policy(|p| {
        p.stop_rule = StopRule::Never;
        p.batch_size = 3;
    });
    let result = simulate_policy(&wide, &wide_policy, k2(4));
    assert_eq!(result.revealed_ids, ["t5-n0", "t5-n1", "t5-n2", "t5-n3"]);
    assert_eq!(
        (result.probes_to_best, result.rounds_to_best, result.rounds),
        (2, 2, 3)
    );
    let shallow = simulate_policy(&wide, &explore_root_never(), k2(2));
    assert_eq!(
        (
            shallow.best_score,
            shallow.probes_to_best,
            shallow.rounds_to_best,
            shallow.rounds
        ),
        (0.5, 1, 1, 2)
    );

    let root_best = tree(
        "t3",
        2,
        &[
            (0, None, 0, 0, 0.9),
            (1, Some(0), 0, 0, 0.5),
            (2, Some(0), 0, 1, 0.4),
        ],
    );
    let result = simulate_policy(&root_best, &explore_root_never(), k2(2));
    assert_eq!(
        (
            result.best_score,
            result.probes_to_best,
            result.rounds_to_best
        ),
        (0.9, 0, 0)
    );
    assert_eq!(
        (
            result.out_of_support_cells,
            result.in_support,
            result.best_so_far
        ),
        (0, 1.0, vec![0.9, 0.9])
    );
    let alone = simulate_policy(
        &tree("t4", 2, &[(0, None, 0, 0, 0.1)]),
        &best_first(),
        k2(3),
    );
    assert_eq!(
        (alone.selected_cells, alone.in_support, alone.probes_to_best),
        (0, 1.0, 0)
    );
    assert!(alone.best_so_far.is_empty());
}

#[test]
fn replay_stops_at_k2_when_exhausted_and_by_the_stop_rule() {
    let capped = simulate_policy(&t1(), &best_first(), k2(1));
    assert_eq!(
        (capped.rounds, capped.n, capped.revealed_ids),
        (1, 1, vec!["t1-n0".to_string(), "t1-n1".to_string()])
    );
    let exhausted = simulate_policy(&t1(), &best_first(), k2(50));
    assert!(exhausted.revealed_ids.len() == 4 && exhausted.rounds < 50);
    let patience = simulate_policy(&t1(), &explore_root_patience(), k2(12));
    let never = simulate_policy(&t1(), &explore_root_never(), k2(12));
    assert_eq!((patience.rounds, never.rounds), (2, 12));
    assert_ne!(patience, never);
}

#[test]
fn a_batch_never_holds_a_node_with_its_parent() {
    let tree = t1();
    let mut sim = ReplaySimulator::new(&tree);
    let root = sim.view().legal_actions().remove(0);
    sim.reveal_for(&root);
    let view = sim.view();
    let legal = view.legal_actions();
    assert_eq!(
        legal
            .iter()
            .map(|cell| cell.node_id.as_str())
            .collect::<Vec<_>>(),
        ["t1-n0", "t1-n1"]
    );
    let batch = interpret_policy(&best_first(), &view);
    assert_eq!(
        batch
            .iter()
            .map(|cell| cell.node_id.as_str())
            .collect::<Vec<_>>(),
        ["t1-n1"]
    );
    assert!(assert_legal_batch(&view, &batch).is_ok());
    assert!(assert_legal_batch(&view, &legal).is_err());
}

/// TS `nearTieTask`: the first child beats the root by `delta`, deeper ones by 0.4.
struct NearTie {
    delta: f64,
}

impl ScoredTask for NearTie {
    type Artifact = (f64, u32);

    fn id(&self) -> &'static str {
        "sum-difference"
    }

    fn root(&self, _rng: &mut SeededRng) -> (f64, u32) {
        (0.5, 0)
    }

    fn propose(
        &self,
        parent: Option<&(f64, u32)>,
        _params: &ProposeParams,
        _rng: &mut SeededRng,
        _round: u32,
    ) -> (f64, u32) {
        match parent {
            None => (0.5, 0),
            Some((value, depth)) => (
                value + if *depth == 0 { self.delta } else { 0.4 },
                depth + 1,
            ),
        }
    }

    fn evaluate(&self, candidate: &(f64, u32)) -> Evaluation {
        Evaluation::valid(candidate.0)
    }

    fn serialize(&self, candidate: &(f64, u32)) -> Value {
        json!({ "value": candidate.0, "depth": candidate.1 })
    }

    fn deserialize(&self, value: &Value) -> Result<(f64, u32), ArtifactShapeError> {
        let shape = || ArtifactShapeError("expected {value, depth}".to_string());
        let depth = value["depth"]
            .as_u64()
            .and_then(|depth| u32::try_from(depth).ok())
            .ok_or_else(shape)?;
        Ok((value["value"].as_f64().ok_or_else(shape)?, depth))
    }
}

fn explore(
    task: &dyn DynTask,
    dir: &Path,
    grow: ExplorationPolicy,
    workers: u32,
    k1: u32,
    seed: i64,
    clock: u64,
) -> ExploreResult {
    let clock = move || clock;
    run_online_exploration(ExploreOptions {
        task,
        task_id: task.id().to_string(),
        n: None,
        seed: Seed::Number(seed),
        rng: SeededRng::new(&Seed::Number(seed)),
        clock: &clock,
        workers,
        k1,
        dir,
        policy: grow,
        iteration: 0,
        proposer: None,
        tree_id: None,
    })
    .expect("rollout")
}

#[test]
fn online_and_replay_stop_on_the_same_round_around_improve_eps() {
    let patience_one = policy(|p| {
        p.stop_rule = StopRule::Patience;
        p.beta = 1;
        p.batch_size = 1;
    });
    let never = policy(|p| {
        p.stop_rule = StopRule::Never;
        p.batch_size = 1;
    });
    for (delta, rounds) in [(IMPROVE_EPS / 10.0, 1), (IMPROVE_EPS * 10.0, 4)] {
        let task = NearTie { delta };
        let (chain_dir, patient_dir) = (
            tempfile::tempdir().expect("chain"),
            tempfile::tempdir().expect("patient"),
        );
        let chain = explore(&task, chain_dir.path(), never, 1, 4, 1, 1);
        assert_eq!(chain.revealed_count, 4);
        let patient = explore(&task, patient_dir.path(), patience_one, 1, 4, 1, 1);
        assert_eq!((patient.rounds, patient.revealed_count), (rounds, rounds));
        let recorded = read_tree(&chain.tree_id, chain_dir.path()).expect("tree");
        let replay = simulate_policy(&recorded, &patience_one, k2(8));
        assert_eq!(
            (replay.rounds, replay.n, replay.best_score),
            (patient.rounds, patient.revealed_count, patient.best_score)
        );
    }
}

#[test]
fn a_tree_round_trips_through_the_store_and_a_rerun_truncates() {
    let dir = tempfile::tempdir().expect("tempdir");
    let writer = TreeWriter::new("t1", dir.path());
    let tree = t1();
    for _ in 0..2 {
        writer.write_header(&tree.header).expect("header");
        for node in &tree.nodes {
            writer.append_node(node).expect("node");
        }
    }
    writer
        .append_reveal(&RevealRecord {
            record_type: RevealTag::Tag,
            round: 0,
            ids: vec!["t1-n1".to_string()],
        })
        .expect("reveal");
    let from_disk = read_tree("t1", dir.path()).expect("read");
    assert_eq!(from_disk.nodes.len(), 4);
    assert_eq!(from_disk.reveals.len(), 1);
    assert_eq!(
        simulate_policy(&from_disk, &best_first(), k2(10)),
        simulate_policy(&tree, &best_first(), k2(10))
    );
    assert!(read_tree("absent", dir.path()).is_err());
}

#[test]
fn a_rollout_improves_over_the_root_at_zero_tokens_and_records_its_curve() {
    let task = resolve_task(DreamTaskId::CirclePacking, Some(26)).expect("task");
    let dir = tempfile::tempdir().expect("tempdir");
    let result = explore(
        task.as_ref(),
        dir.path(),
        DEFAULT_POLICY,
        4,
        12,
        5,
        1_700_000_000_000,
    );
    assert_eq!(result.tokens, 0);
    assert_eq!(result.tree.size(), result.revealed_count as usize + 1);
    assert!(result.best_score > result.root_score);
    assert_eq!(result.agent_generated_count, 0);
    let best = result.tree.best_node().expect("best");
    assert_eq!(result.probes_to_best, best.seq);
    assert_eq!(
        result.improvements[0],
        ScoreImprovement {
            probe: 0,
            score: result.root_score
        }
    );
    assert_eq!(
        result.improvements.last(),
        Some(&ScoreImprovement {
            probe: best.seq,
            score: result.best_score
        })
    );
    assert!(result
        .improvements
        .windows(2)
        .all(|pair| pair[0].probe < pair[1].probe && pair[0].score < pair[1].score));
    assert_eq!(improvements_of([(0, 1.0, false)]), Vec::new());
    assert_eq!(attempt_rng_label(3, 7, 1), "r3:p7:b1");

    let other = tempfile::tempdir().expect("tempdir");
    let later = explore(task.as_ref(), other.path(), DEFAULT_POLICY, 4, 12, 5, 42);
    let shape = |result: &ExploreResult| {
        result
            .tree
            .all_nodes()
            .iter()
            .map(|node| {
                (
                    node.branch,
                    node.round,
                    node.score.to_bits(),
                    node.valid,
                    node.artifact_ref.clone(),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_ne!(later.tree_id, result.tree_id);
    assert_eq!(shape(&later), shape(&result));
}

/// TS `stampedProposer`: the local candidates, every `accept`-th stamped `llm`.
struct Stamped<'a> {
    local: LocalProposer<'a>,
    attempt: u32,
    accept: u32,
}

impl Proposer for Stamped<'_> {
    fn propose(
        &mut self,
        parent: Option<&Artifact>,
        params: &ProposeParams,
        rng: &mut SeededRng,
        round: u32,
    ) -> ProposeOutcome {
        self.attempt += 1;
        let outcome = self.local.propose(parent, params, rng, round);
        if self.attempt.is_multiple_of(self.accept) {
            ProposeOutcome {
                tokens: 270,
                origin: Some(CandidateOrigin::Llm),
                ..outcome
            }
        } else {
            ProposeOutcome {
                tokens: 600,
                origin: Some(CandidateOrigin::Local),
                ..outcome
            }
        }
    }
}

#[test]
fn an_injected_outcomes_origin_and_tokens_land_on_the_node_without_moving_the_tree() {
    let task = resolve_task(DreamTaskId::CirclePacking, Some(26)).expect("task");
    let (plain_dir, stamped_dir) = (
        tempfile::tempdir().expect("plain"),
        tempfile::tempdir().expect("stamped"),
    );
    let plain = explore(task.as_ref(), plain_dir.path(), DEFAULT_POLICY, 4, 12, 5, 7);
    let mut stamped = Stamped {
        local: LocalProposer::new(task.as_ref()),
        attempt: 0,
        accept: 3,
    };
    let clock = || 7;
    let result = run_online_exploration(ExploreOptions {
        task: task.as_ref(),
        task_id: "circle-packing".to_string(),
        n: None,
        seed: Seed::Number(5),
        rng: SeededRng::new(&Seed::Number(5)),
        clock: &clock,
        workers: 4,
        k1: 12,
        dir: stamped_dir.path(),
        policy: DEFAULT_POLICY,
        iteration: 0,
        proposer: Some(&mut stamped),
        tree_id: None,
    })
    .expect("rollout");
    let llm = result.revealed_count / 3;
    assert_eq!(result.agent_generated_count, llm);
    assert_eq!(
        result.tokens,
        u64::from(llm) * 270 + u64::from(result.revealed_count - llm) * 600
    );
    let scores = |result: &ExploreResult| {
        result
            .tree
            .all_nodes()
            .iter()
            .map(|node| node.score.to_bits())
            .collect::<Vec<_>>()
    };
    assert_eq!(scores(&result), scores(&plain));
    let recorded = read_tree(&result.tree_id, stamped_dir.path()).expect("tree");
    let origins: Vec<NodeOrigin> = recorded
        .nodes
        .iter()
        .map(pa_dream::records::NodeRecord::origin)
        .collect();
    assert_eq!(origins[0], NodeOrigin::Root);
    assert_eq!(
        origins
            .iter()
            .filter(|origin| **origin == NodeOrigin::Llm)
            .count(),
        llm as usize
    );
    // A pre-provenance line reads as a root plus local nodes.
    let legacy: Vec<TreeRecord> = std::iter::once(TreeRecord::Header(recorded.header.clone()))
        .chain(recorded.nodes.iter().map(|node| {
            let mut node = node.clone();
            node.origin = None;
            TreeRecord::Node(node)
        }))
        .collect();
    let legacy = RecordedTree::from_records(legacy).expect("legacy");
    assert!(legacy.nodes[1..]
        .iter()
        .all(|node| node.origin() == NodeOrigin::Local));
}
