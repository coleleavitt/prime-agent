//! The TS `dream-loop.test.ts` and `dream-dreams-log.test.ts` behaviour: the
//! fixed control, determinism across clocks, run labels, priming, the dreams
//! log and the probation that reverts an adoption.

// Exact float equality is the claim: replay and the objective are deterministic
// IEEE-754 arithmetic, and the expected values are exact or recorded.
#![allow(clippy::float_cmp)]

mod support;

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use pa_dream::dream_loop::{
    dream_run_id, judge_probation, merged_round_curve, priming_tree_id, run_dream_loop,
    DreamLoopOptions, DreamLoopResult, DreamRoundRecord, PROBATION_EPS,
};
use pa_dream::dreams::{
    dreams_path, read_dreams_log, DreamProbationRecord, DreamsLogContext, DreamsLogLine,
};
use pa_dream::improve::{CandidateReason, DreamResult, DreamerKind, PoolScore};
use pa_dream::json;
use pa_dream::objective::DEFAULT_OBJECTIVE;
use pa_dream::policy::{
    policy_id, sha256_hex, ExplorationPolicy, StopRule, DEFAULT_POLICY, PRIMING_DIVERSE,
};
use pa_dream::rng::{Seed, SeededRng};
use pa_dream::store::{list_trees, read_tree};
use pa_dream::task::{ArtifactShapeError, DynTask, Evaluation, ProposeParams, ScoredTask};
use pa_dream::tasks::{resolve_task, DreamTaskId};
use serde_json::{json, Map, Value};
use support::Fixed;

const FIXED_CLOCK: u64 = 1_700_000_000_000;

#[allow(clippy::struct_field_names)] // the loop's own option names
struct Run<'a> {
    task: &'a dyn DynTask,
    clock: &'a dyn Fn() -> u64,
    seed: i64,
    fixed: bool,
    initial: ExplorationPolicy,
    priming: Vec<ExplorationPolicy>,
    run_label: Option<String>,
    context: DreamsLogContext,
    iterations: u32,
    workers: u32,
    k1: u32,
    k2: u32,
    dreams: usize,
    candidates: Option<&'a mut Fixed>,
}

fn run(dir: &Path, options: Run<'_>) -> DreamLoopResult {
    run_dream_loop(DreamLoopOptions {
        task: options.task,
        task_id: "sum-difference".to_string(),
        n: None,
        seed: Seed::Number(options.seed),
        clock: options.clock,
        workers: options.workers,
        k1: options.k1,
        k2: options.k2,
        dreams: options.dreams,
        iterations: options.iterations,
        dir,
        objective: DEFAULT_OBJECTIVE,
        rng: None,
        initial_policy: options.initial,
        fixed_policy: options.fixed,
        candidates: options
            .candidates
            .map(|source| source as &mut dyn pa_dream::improve::CandidateSource),
        run_label: options.run_label,
        dreams_log_context: options.context,
        priming_policies: options.priming,
    })
    .expect("loop")
}

/// The TS `options()` defaults: sum-difference, seed 7, W 3, k1 5, k2 10, M 4, 2 iterations.
fn defaults<'a>(task: &'a dyn DynTask, clock: &'a dyn Fn() -> u64) -> Run<'a> {
    Run {
        task,
        clock,
        seed: 7,
        fixed: false,
        initial: DEFAULT_POLICY,
        priming: Vec::new(),
        run_label: None,
        context: DreamsLogContext::default(),
        iterations: 2,
        workers: 3,
        k1: 5,
        k2: 10,
        dreams: 4,
        candidates: None,
    }
}

fn sum_difference() -> std::sync::Arc<dyn DynTask> {
    resolve_task(DreamTaskId::SumDifference, None).expect("task")
}

/// TS `treeFiles`: every tree file and blob, keyed in `readdir().sort()` order.
fn tree_files(dir: &Path) -> Map<String, Value> {
    let trees = dir.join("trees");
    let mut names: Vec<String> = std::fs::read_dir(&trees)
        .expect("trees")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
    let mut files = Map::new();
    for name in names {
        if Path::new(&name)
            .extension()
            .is_some_and(|extension| extension == "jsonl")
        {
            let text = std::fs::read_to_string(trees.join(&name)).expect("tree");
            files.insert(name, Value::from(text));
        } else {
            let blobs = trees.join(&name).join("blobs");
            let mut blob_names: Vec<String> = std::fs::read_dir(&blobs)
                .expect("blobs")
                .map(|entry| {
                    entry
                        .expect("entry")
                        .file_name()
                        .to_string_lossy()
                        .into_owned()
                })
                .collect();
            blob_names.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            for blob in blob_names {
                let text = std::fs::read_to_string(blobs.join(&blob)).expect("blob");
                files.insert(format!("{name}/blobs/{blob}"), Value::from(text));
            }
        }
    }
    files
}

/// The TS-pinned digest of the plain loop's trees (`dream-loop.test.ts`, from before the
/// objective, verdict, lever-scan, dreams-log, runLabel and priming changes).
const PRE_CHANGE_TREE_DIGEST: &str =
    "ef7ed6f1f3279f829d89a1ef60ec03d89930b505aefc35d00f2d2865cd51ff6b";

fn tree_digest(dir: &Path) -> String {
    sha256_hex(json::stringify(&tree_files(dir)).as_bytes())
}

#[test]
fn a_plain_loop_grows_the_trees_the_ts_test_pinned_dreaming_or_fixed() {
    let task = sum_difference();
    let clock = || FIXED_CLOCK;
    for fixed in [true, false] {
        let dir = tempfile::tempdir().expect("tempdir");
        let result = run(
            dir.path(),
            Run {
                fixed,
                ..defaults(task.as_ref(), &clock)
            },
        );
        assert_eq!(tree_files(dir.path()).len(), 34);
        assert_eq!(tree_digest(dir.path()), PRE_CHANGE_TREE_DIGEST);
        assert_eq!(result.run_id, format!("sum-difference-s7-r{FIXED_CLOCK}"));
        assert_eq!(result.rounds[0].priming_tree_ids, None);
        assert_eq!(list_trees(dir.path()).len(), 3);
    }
}

#[test]
fn the_fixed_control_never_dreams_and_shares_iteration_0_with_the_dreaming_loop() {
    let task = sum_difference();
    let clock = || FIXED_CLOCK;
    let fixed_dir = tempfile::tempdir().expect("tempdir");
    let fixed = run(
        fixed_dir.path(),
        Run {
            fixed: true,
            ..defaults(task.as_ref(), &clock)
        },
    );
    assert!(fixed.fixed_policy && !fixed.improved);
    assert_eq!((fixed.rounds.len(), fixed.tree_ids.len()), (3, 3));
    assert_eq!(fixed.final_policy_id, fixed.initial_policy_id);
    assert_eq!(fixed.final_policy_score, fixed.initial_policy_score);
    for record in &fixed.rounds {
        assert!(record.dreaming.is_none());
        assert_eq!(record.policy_id, fixed.initial_policy_id);
    }
    assert_eq!(
        fixed
            .rounds
            .iter()
            .map(|record| record.pool_size)
            .collect::<Vec<_>>(),
        [0, 1, 2]
    );

    let dream_dir = tempfile::tempdir().expect("tempdir");
    let dream = run(dream_dir.path(), defaults(task.as_ref(), &clock));
    assert_eq!(dream.tree_ids, fixed.tree_ids);
    assert_eq!(dream.rounds[0], fixed.rounds[0]);
    let (fixed_files, dream_files) = (tree_files(fixed_dir.path()), tree_files(dream_dir.path()));
    let first: Vec<&String> = fixed_files
        .keys()
        .filter(|name| name.starts_with(&fixed.tree_ids[0]))
        .collect();
    assert!(first.len() > 1);
    for name in first {
        assert_eq!(dream_files[name.as_str()], fixed_files[name.as_str()]);
    }
    for record in &dream.rounds[1..] {
        let dreaming = record.dreaming.as_ref().expect("dreaming");
        assert_eq!(
            (dreaming.candidates, dreaming.candidate_verdicts.len()),
            (4, 4)
        );
        assert!(dreaming.chosen_score >= dreaming.current_score);
        assert_eq!(dreaming.dreamer, DreamerKind::Local);
        assert!(dreaming
            .lever_scan
            .as_ref()
            .is_some_and(|scan| scan.gap >= 0.0));
        let winners = dreaming
            .candidate_verdicts
            .iter()
            .filter(|verdict| verdict.reason == CandidateReason::Winner)
            .count();
        assert_eq!(winners, usize::from(dreaming.improved));
    }
    assert_eq!(dream.final_selection.len(), 2);

    let initial = ExplorationPolicy {
        batch_size: 2,
        beta: 3,
        stop_rule: StopRule::FixedRounds,
        ..DEFAULT_POLICY
    };
    let explicit = tempfile::tempdir().expect("tempdir");
    let result = run(
        explicit.path(),
        Run {
            fixed: true,
            initial,
            ..defaults(task.as_ref(), &clock)
        },
    );
    assert_eq!(
        (result.initial_policy_id.clone(), result.final_policy),
        (policy_id(&initial), initial)
    );
    assert!(list_trees(explicit.path())
        .iter()
        .all(|summary| summary.policy_id == policy_id(&initial)));
}

fn clock_free(record: &DreamRoundRecord) -> DreamRoundRecord {
    DreamRoundRecord {
        tree_id: String::new(),
        ..record.clone()
    }
}

#[test]
fn two_clocks_change_only_the_clock_bearing_ids() {
    let task = sum_difference();
    let early = || 1_789_842_143_996;
    let late = || 1;
    for fixed in [false, true] {
        let (a, b) = (
            tempfile::tempdir().expect("a"),
            tempfile::tempdir().expect("b"),
        );
        let first = run(
            a.path(),
            Run {
                fixed,
                ..defaults(task.as_ref(), &early)
            },
        );
        let second = run(
            b.path(),
            Run {
                fixed,
                ..defaults(task.as_ref(), &late)
            },
        );
        assert_ne!(first.tree_ids, second.tree_ids);
        assert_ne!(first.run_id, second.run_id);
        assert_eq!(
            first.rounds.iter().map(clock_free).collect::<Vec<_>>(),
            second.rounds.iter().map(clock_free).collect::<Vec<_>>()
        );
        assert_eq!(
            (
                first.final_policy_id,
                first.final_policy_score,
                first.best_node_score
            ),
            (
                second.final_policy_id,
                second.final_policy_score,
                second.best_node_score
            )
        );
    }
    let clock = || FIXED_CLOCK;
    let seven = run(
        tempfile::tempdir().expect("7").path(),
        defaults(task.as_ref(), &clock),
    );
    let eight = run(
        tempfile::tempdir().expect("8").path(),
        Run {
            seed: 8,
            ..defaults(task.as_ref(), &clock)
        },
    );
    assert_ne!(
        seven.rounds.iter().map(clock_free).collect::<Vec<_>>(),
        eight.rounds.iter().map(clock_free).collect::<Vec<_>>()
    );
}

#[test]
fn a_run_label_reaches_the_run_id_and_log_key_but_never_a_tree() {
    let task = sum_difference();
    let clock = || FIXED_CLOCK;
    let dir = tempfile::tempdir().expect("tempdir");
    let labelled = run(
        dir.path(),
        Run {
            run_label: Some("exp-1/dream".to_string()),
            ..defaults(task.as_ref(), &clock)
        },
    );
    assert_eq!(
        labelled.run_id,
        format!("sum-difference-s7-r{FIXED_CLOCK}-exp-1_dream")
    );
    assert!(dreams_path(dir.path(), &labelled.run_id).exists());
    assert_eq!(tree_digest(dir.path()), PRE_CHANGE_TREE_DIGEST);
    let seven = Seed::Number(7);
    assert_eq!(
        dream_run_id("sum-difference", &seven, 5, Some("a b/c")),
        "sum-difference-s7-r5-a_b_c"
    );
    assert_eq!(
        dream_run_id("sum-difference", &seven, 5, None),
        "sum-difference-s7-r5"
    );
    assert_eq!(
        dream_run_id("sum-difference", &seven, 5, Some("")),
        "sum-difference-s7-r5"
    );
}

#[test]
fn priming_rolls_out_at_iteration_0_and_charges_round_1() {
    let task = sum_difference();
    let clock = || FIXED_CLOCK;
    let (plain_dir, dir) = (
        tempfile::tempdir().expect("plain"),
        tempfile::tempdir().expect("primed"),
    );
    let plain = run(plain_dir.path(), defaults(task.as_ref(), &clock));
    let primed = run(
        dir.path(),
        Run {
            priming: PRIMING_DIVERSE.to_vec(),
            ..defaults(task.as_ref(), &clock)
        },
    );
    let first = &primed.rounds[0];
    let ids: Vec<String> = (0..2)
        .map(|index| priming_tree_id("sum-difference", &Seed::Number(7), index, FIXED_CLOCK))
        .collect();
    assert_eq!(
        ids,
        [
            "sum-difference-s7-i0p0-1700000000000",
            "sum-difference-s7-i0p1-1700000000000"
        ]
    );
    assert_eq!(first.priming_tree_ids.as_ref(), Some(&ids));
    assert_eq!(primed.tree_ids, plain.tree_ids);
    let initial_file = format!("{}.jsonl", plain.tree_ids[0]);
    assert_eq!(
        tree_files(dir.path())[initial_file.as_str()],
        tree_files(plain_dir.path())[initial_file.as_str()]
    );
    let summaries = list_trees(dir.path());
    assert_eq!(summaries.len(), 5);
    let priming_probes: usize = ids
        .iter()
        .map(|id| {
            summaries
                .iter()
                .find(|summary| &summary.tree_id == id)
                .expect("primed")
                .node_count
                - 1
        })
        .sum();
    assert_eq!(
        first.priming_probes,
        Some(u32::try_from(priming_probes).expect("count"))
    );
    assert_eq!(
        first.probes,
        plain.rounds[0].probes + first.priming_probes.unwrap_or(0)
    );
    assert_eq!(
        first.improvements.last().map(|point| point.score),
        Some(first.round_best)
    );
    assert_eq!(primed.rounds[1].pool_size, 3);
    let fixed = run(
        tempfile::tempdir().expect("fixed").path(),
        Run {
            priming: PRIMING_DIVERSE.to_vec(),
            fixed: true,
            ..defaults(task.as_ref(), &clock)
        },
    );
    assert_eq!(&fixed.rounds[0], first);
    assert_eq!(fixed.rounds[1].pool_size, 3);
    assert_eq!(merged_round_curve(&[]), (0, Vec::new()));
}

#[test]
fn every_step_is_logged_with_its_context_and_the_final_selection_as_iteration_minus_1() {
    let task = sum_difference();
    let clock = || FIXED_CLOCK;
    let dir = tempfile::tempdir().expect("tempdir");
    let result = run(
        dir.path(),
        Run {
            context: DreamsLogContext {
                experiment_id: Some("exp-1".to_string()),
                arm: Some("dream".to_string()),
            },
            ..defaults(task.as_ref(), &clock)
        },
    );
    let path = dreams_path(dir.path(), &result.run_id);
    assert_eq!(
        path,
        dir.path()
            .join("dreams")
            .join(format!("{}.jsonl", result.run_id))
    );
    let lines = read_dreams_log(&path).expect("log");
    let order: Vec<(String, i64)> = lines
        .iter()
        .map(|line| match line {
            DreamsLogLine::Candidate { iteration, .. } => ("candidate".to_string(), *iteration),
            DreamsLogLine::Step { iteration, .. } => ("step".to_string(), *iteration),
            DreamsLogLine::Probation { iteration, .. } => ("probation".to_string(), *iteration),
        })
        .collect();
    let steps: Vec<i64> = order
        .iter()
        .filter(|(kind, _)| kind == "step")
        .map(|(_, iteration)| *iteration)
        .collect();
    assert_eq!(steps, [1, 2, -1]);
    let count = |iteration: i64| {
        order
            .iter()
            .filter(|entry| **entry == ("candidate".to_string(), iteration))
            .count()
    };
    assert_eq!((count(1), count(2), count(-1)), (4, 4, 2));
    for line in &lines {
        let (DreamsLogLine::Candidate { line, .. }
        | DreamsLogLine::Step { line, .. }
        | DreamsLogLine::Probation { line, .. }) = line;
        assert_eq!(
            (
                line["ts"].as_u64(),
                line["experimentId"].as_str(),
                line["arm"].as_str()
            ),
            (Some(FIXED_CLOCK), Some("exp-1"), Some("dream"))
        );
        let keys: Vec<&str> = line
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .take(5)
            .collect();
        assert_eq!(keys, ["type", "ts", "experimentId", "arm", "iteration"]);
    }
    let final_step = lines
        .iter()
        .find_map(|line| match line {
            DreamsLogLine::Step {
                iteration: -1,
                line,
            } => Some(line.clone()),
            _ => None,
        })
        .expect("final step");
    assert_eq!(
        final_step["chosenPolicyId"],
        Value::from(result.final_policy_id.as_str())
    );
    assert_eq!(
        (
            final_step["leverScan"].clone(),
            final_step["poolSize"].as_u64()
        ),
        (Value::Null, Some(3))
    );
}

#[test]
fn a_fixed_run_logs_only_its_final_selection_and_a_bad_line_is_an_error() {
    let task = sum_difference();
    let clock = || FIXED_CLOCK;
    let dir = tempfile::tempdir().expect("tempdir");
    let fixed_dir = tempfile::tempdir().expect("tempdir");
    let fixed = run(
        fixed_dir.path(),
        Run {
            fixed: true,
            ..defaults(task.as_ref(), &clock)
        },
    );
    let fixed_lines = read_dreams_log(&dreams_path(fixed_dir.path(), &fixed.run_id)).expect("log");
    assert!(matches!(
        fixed_lines.as_slice(),
        [DreamsLogLine::Step { iteration: -1, .. }]
    ));

    assert_eq!(
        read_dreams_log(&dir.path().join("dreams/absent.jsonl")).expect("empty"),
        Vec::new()
    );
    std::fs::write(
        dir.path().join("bad.jsonl"),
        "{\"type\":\"step\",\"ts\":1}\n",
    )
    .expect("write");
    assert!(read_dreams_log(&dir.path().join("bad.jsonl")).is_err());
}

// --- probation -------------------------------------------------------------

/// The TS scripted task: the root is 0; trees 0, 1 and 3+ find 1.0 in round 1 and
/// 0.5 later; tree 2 (the probation rollout) finds `probation_first`, then 0.9.
struct Scripted {
    trees: AtomicUsize,
    probation_first: f64,
}

impl ScoredTask for Scripted {
    type Artifact = f64;

    fn id(&self) -> &'static str {
        "sum-difference"
    }

    fn root(&self, _rng: &mut SeededRng) -> f64 {
        self.trees.fetch_add(1, Ordering::SeqCst);
        0.0
    }

    fn propose(
        &self,
        _parent: Option<&f64>,
        _params: &ProposeParams,
        _rng: &mut SeededRng,
        round: u32,
    ) -> f64 {
        if self.trees.load(Ordering::SeqCst) == 3 {
            return if round == 1 {
                self.probation_first
            } else {
                0.9
            };
        }
        if round == 1 {
            1.0
        } else {
            0.5
        }
    }

    fn evaluate(&self, candidate: &f64) -> Evaluation {
        Evaluation::valid(*candidate)
    }

    fn serialize(&self, candidate: &f64) -> Value {
        json!({ "v": candidate })
    }

    fn deserialize(&self, value: &Value) -> Result<f64, ArtifactShapeError> {
        value["v"]
            .as_f64()
            .ok_or_else(|| ArtifactShapeError("expected { v: number }".to_string()))
    }
}

fn collapse() -> ExplorationPolicy {
    ExplorationPolicy {
        stop_rule: StopRule::FixedRounds,
        beta: 1,
        ..DEFAULT_POLICY
    }
}

fn scripted(dir: &Path, probation_first: f64) -> DreamLoopResult {
    let task = Scripted {
        trees: AtomicUsize::new(0),
        probation_first,
    };
    let clock = || FIXED_CLOCK;
    let mut source = Fixed(support::local(&[collapse()]));
    run(
        dir,
        Run {
            workers: 3,
            k1: 4,
            k2: 8,
            dreams: 1,
            iterations: 3,
            candidates: Some(&mut source),
            ..defaults(&task, &clock)
        },
    )
}

#[test]
fn probation_is_judged_against_the_incumbents_lowest_replay_best() {
    let current = PoolScore {
        value: 0.9,
        quality: 1.0,
        anytime: 1.0,
        cost: 0.5,
        rounds_saved: 0.0,
        n: 6.0,
        rounds: 4.0,
        out_of_support_cells: 0.0,
        in_support_mean: 1.0,
        in_support_min: 1.0,
        charged_probes: 6.0,
        charged_rounds: 4.0,
    };
    let step = DreamResult {
        chosen_policy: collapse(),
        chosen_policy_id: policy_id(&collapse()),
        chosen_score: 0.0,
        current_score: 0.0,
        chosen_quality: 0.0,
        current_quality: 0.0,
        current,
        current_min_best: 0.7,
        improved: true,
        scored_count: 0,
        quality_rejected: 0,
        candidate_policy_ids: Vec::new(),
        candidates: Vec::new(),
        dreamer: DreamerKind::Local,
        lever_scan: None,
        pool_size: 0,
        measured_trees: 2,
        evidence_trees: 1,
        simulations: 0,
        tokens: 0,
    };
    let rollout = |best: f64| pa_dream::rollout::ExploreResult {
        tree_id: "t".to_string(),
        tree: pa_dream::tree::DiscoveryTree::with_root(
            support::header("t", 1),
            "r".to_string(),
            0.0,
            true,
        ),
        rounds: 1,
        revealed_count: 0,
        agent_generated_count: 0,
        best_score: best,
        best_node_id: "t-n0".to_string(),
        probes_to_best: 0,
        improvements: Vec::new(),
        root_score: 0.0,
        tokens: 0,
    };
    assert_eq!(
        judge_probation(&step, &DEFAULT_POLICY, &rollout(0.7)),
        DreamProbationRecord {
            policy_id: policy_id(&collapse()),
            incumbent_policy_id: policy_id(&DEFAULT_POLICY),
            tree_id: "t".to_string(),
            round_best: 0.7,
            floor: 0.7,
            charged_probes: 6.0,
            charged_rounds: 4.0,
            incumbent_charged_probes: 6.0,
            incumbent_charged_rounds: 4.0,
            evidence_trees: 1,
            reverted: false,
        }
    );
    assert!(!judge_probation(&step, &DEFAULT_POLICY, &rollout(0.7 - PROBATION_EPS / 2.0)).reverted);
    assert!(judge_probation(&step, &DEFAULT_POLICY, &rollout(0.7 - 2.0 * PROBATION_EPS)).reverted);
}

#[test]
fn an_adoption_below_the_floor_is_reverted_revoked_and_logged() {
    let dir = tempfile::tempdir().expect("tempdir");
    let result = scripted(dir.path(), 0.2);
    let (initial, collapse_id) = (policy_id(&DEFAULT_POLICY), policy_id(&collapse()));
    assert_eq!(
        result
            .rounds
            .iter()
            .map(|record| record.policy_id.clone())
            .collect::<Vec<_>>(),
        [
            initial.clone(),
            initial.clone(),
            collapse_id.clone(),
            initial.clone()
        ]
    );
    let first = result.rounds[1].dreaming.as_ref().expect("first");
    assert!(!first.improved && first.probation.is_none());
    assert_eq!(first.candidate_verdicts[0].reason, CandidateReason::Worse);
    let second = result.rounds[2].dreaming.as_ref().expect("second");
    assert!(second.improved);
    assert_eq!(second.measured_trees, 2);
    let winner = &second.candidate_verdicts[0];
    assert_eq!(
        (winner.reason, winner.charged_probes, winner.charged_rounds),
        (CandidateReason::Winner, 1.0, 1.0)
    );
    assert_eq!(
        (
            result.rounds[2].round_best,
            result.rounds[2].probes,
            result.rounds[1].probes
        ),
        (0.2, 1, 6)
    );
    let probation = DreamProbationRecord {
        policy_id: collapse_id,
        incumbent_policy_id: initial.clone(),
        tree_id: result.rounds[2].tree_id.clone(),
        round_best: 0.2,
        floor: 1.0,
        charged_probes: 1.0,
        charged_rounds: 1.0,
        incumbent_charged_probes: 6.0,
        incumbent_charged_rounds: 4.0,
        evidence_trees: 1,
        reverted: true,
    };
    assert_eq!(second.probation.as_ref(), Some(&probation));
    let third = result.rounds[3].dreaming.as_ref().expect("third");
    assert!(!third.improved && third.probation.is_none());
    assert_eq!(third.candidate_verdicts[0].reason, CandidateReason::Revoked);
    assert_eq!(
        (result.rounds[3].round_best, result.probation_reverts),
        (1.0, 1)
    );
    assert_eq!(result.final_policy_id, initial);
    assert_eq!(
        result
            .final_selection
            .iter()
            .map(|verdict| verdict.reason)
            .collect::<Vec<_>>(),
        [
            CandidateReason::Identical,
            CandidateReason::Revoked,
            CandidateReason::Identical
        ]
    );

    let lines = read_dreams_log(&dreams_path(dir.path(), &result.run_id)).expect("log");
    let probations: Vec<&Value> = lines
        .iter()
        .filter_map(|line| match line {
            DreamsLogLine::Probation { line, .. } => Some(line),
            _ => None,
        })
        .collect();
    let mut expected = json!({"type": "probation", "ts": FIXED_CLOCK, "iteration": 2});
    for (key, value) in serde_json::to_value(&probation)
        .expect("probation")
        .as_object()
        .expect("object")
    {
        expected[key] = value.clone();
    }
    // Compared as written: the line is byte-identical to the TS one.
    assert_eq!(probations.len(), 1);
    assert_eq!(json::stringify(probations[0]), json::stringify(&expected));
    let position = |kind: &str, at: i64| {
        lines.iter().position(|line| match line {
            DreamsLogLine::Probation { iteration, .. } => kind == "probation" && *iteration == at,
            DreamsLogLine::Step { iteration, .. } => kind == "step" && *iteration == at,
            DreamsLogLine::Candidate { .. } => false,
        })
    };
    assert!(position("probation", 2) > position("step", 2));
    assert!(position("probation", 2) < position("step", 3));
}

#[test]
fn an_adoption_that_reaches_the_floor_is_kept() {
    let dir = tempfile::tempdir().expect("tempdir");
    let result = scripted(dir.path(), 1.0);
    let collapse_id = policy_id(&collapse());
    assert_eq!(
        result
            .rounds
            .iter()
            .map(|record| record.policy_id.clone())
            .collect::<Vec<_>>(),
        [
            policy_id(&DEFAULT_POLICY),
            policy_id(&DEFAULT_POLICY),
            collapse_id.clone(),
            collapse_id.clone()
        ]
    );
    let probation = result.rounds[2]
        .dreaming
        .as_ref()
        .and_then(|dreaming| dreaming.probation.clone())
        .expect("probation");
    assert_eq!(
        (
            probation.policy_id,
            probation.round_best,
            probation.floor,
            probation.reverted
        ),
        (collapse_id, 1.0, 1.0, false)
    );
    assert_eq!(result.probation_reverts, 0);
    assert_eq!(
        result.rounds[3]
            .dreaming
            .as_ref()
            .expect("third")
            .candidate_verdicts[0]
            .reason,
        CandidateReason::Identical
    );
}

#[test]
fn every_round_records_its_exact_curve_and_the_stopped_early_count() {
    let task = sum_difference();
    let clock = || FIXED_CLOCK;
    let dir = tempfile::tempdir().expect("tempdir");
    let result = run(dir.path(), defaults(task.as_ref(), &clock));
    let stopped = result
        .rounds
        .iter()
        .filter(|record| record.decision_rounds < 5)
        .count();
    assert_eq!(result.stopped_early as usize, stopped);
    for record in &result.rounds {
        let tree = read_tree(&record.tree_id, dir.path()).expect("tree");
        let mut valid: Vec<_> = tree.nodes.iter().filter(|node| node.valid).collect();
        valid.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.seq.cmp(&b.seq)));
        assert_eq!(record.probes_to_round_best, valid[0].seq);
        let last = record.improvements.last().expect("curve");
        assert_eq!((last.probe, last.score), (valid[0].seq, record.round_best));
        assert!(record
            .improvements
            .windows(2)
            .all(|pair| pair[0].score < pair[1].score));
    }
}
