//! The TS `dream-experiment.test.ts` behaviour: planning and validation, the
//! notes, per-arm stores, the fixed control and the headline arithmetic.

// Exact float equality is the claim: replay and the objective are deterministic
// IEEE-754 arithmetic, and the expected values are exact or recorded.
#![allow(clippy::float_cmp)]

mod support;

use std::path::Path;

use pa_dream::dream_loop::DreamHandlerCalls;
use pa_dream::experiment::{
    compute_headline, exact_probes_to_target, k1_stop_rule_note, plan_experiment,
    read_experiment_result, run_experiment, timing_scoring_note, ExperimentArm, ExperimentArmMode,
    ExperimentArmResult, ExperimentArmTotals, ExperimentBudget, ExperimentError,
    ExperimentRoundRow, ExperimentRunOptions, ExperimentSpec, PolicyScoreOnOwnPool,
    EXPERIMENT_ARMS, OBJECTIVE_NOTE,
};
use pa_dream::policy::{ExplorationPolicy, StopRule, DEFAULT_POLICY};
use pa_dream::proposer::RejectCounts;
use pa_dream::rng::Seed;
use pa_dream::rollout::ScoreImprovement;
use pa_dream::store::{list_experiment_ids, list_trees};
use pa_dream::tasks::DreamTaskId;
use serde_json::{json, Value};

const FIXED_CLOCK: u64 = 1_700_000_000_000;

fn spec(arms: &[ExperimentArm]) -> ExperimentSpec {
    ExperimentSpec::new(
        DreamTaskId::SumDifference,
        Seed::Number(7),
        3,
        ExperimentBudget {
            workers: 3,
            k1: 5,
            k2: 10,
            dreams: 4,
        },
        arms.to_vec(),
    )
}

fn options<'a>(dir: &'a Path, clock: &'a dyn Fn() -> u64) -> ExperimentRunOptions<'a> {
    ExperimentRunOptions {
        dir,
        clock,
        notes: Vec::new(),
        overwrite: false,
    }
}

#[test]
fn every_arm_gets_its_own_store_an_identical_round_1_and_the_fixed_arm_never_dreams() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = || FIXED_CLOCK;
    let result = run_experiment(
        &spec(&[ExperimentArm::Fixed, ExperimentArm::Dream]),
        &options(dir.path(), &clock),
    )
    .expect("experiment");
    assert_eq!(
        result.arms.iter().map(|arm| arm.arm).collect::<Vec<_>>(),
        [ExperimentArm::Fixed, ExperimentArm::Dream]
    );
    assert_eq!(list_trees(dir.path()), Vec::new());
    assert_eq!(
        list_experiment_ids(dir.path()),
        std::slice::from_ref(&result.experiment_id)
    );
    let (fixed, dream) = (&result.arms[0], &result.arms[1]);
    assert_eq!(fixed.rounds[0].tree_id, dream.rounds[0].tree_id);
    assert_eq!(fixed.rounds[0].round_best, dream.rounds[0].round_best);
    for arm in &result.arms {
        let store = dir.path().join(&arm.store_dir).join("trees");
        assert!(store
            .join(format!("{}.jsonl", arm.rounds[0].tree_id))
            .exists());
    }
    let first_tree = |arm: &ExperimentArmResult| {
        std::fs::read(
            dir.path()
                .join(&arm.store_dir)
                .join("trees")
                .join(format!("{}.jsonl", arm.rounds[0].tree_id)),
        )
        .expect("tree")
    };
    assert_eq!(first_tree(fixed), first_tree(dream));
    assert!(fixed.fixed_policy && fixed.rounds.iter().all(|row| row.dreaming.is_none()));
    assert_eq!(fixed.policy_changes, 0);
    assert!(dream.rounds[1..].iter().all(|row| row.dreaming.is_some()));
    assert_eq!(
        dream.rounds.iter().map(|row| row.round).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert_eq!(
        dream
            .rounds
            .iter()
            .map(|row| row.pool_size)
            .collect::<Vec<_>>(),
        [0, 1, 2]
    );
    assert_eq!(
        result.notes,
        [
            k1_stop_rule_note(5, &DEFAULT_POLICY).expect("note"),
            OBJECTIVE_NOTE.to_string()
        ]
    );
    assert!(read_experiment_result(dir.path(), &result.experiment_id).is_ok());

    let again = run_experiment(&spec(&[ExperimentArm::Dream]), &options(dir.path(), &clock));
    assert!(matches!(again, Err(ExperimentError::Store(_))));
    let replaced = run_experiment(
        &spec(&[ExperimentArm::Dream]),
        &ExperimentRunOptions {
            overwrite: true,
            ..options(dir.path(), &clock)
        },
    )
    .expect("overwrite");
    assert_eq!(replaced.arms.len(), 1);
    assert!(!dir.path().join(&fixed.store_dir).exists());
    assert_eq!(replaced.headline, None);
}

#[test]
fn planning_validates_before_creating_anything_and_records_the_notes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = || FIXED_CLOCK;
    let guided = run_experiment(
        &spec(&[ExperimentArm::Dream, ExperimentArm::DreamGuided]),
        &options(dir.path(), &clock),
    );
    assert!(
        matches!(guided, Err(ExperimentError::ArmUnavailable(message)) if message.contains("in-session LLM proposer"))
    );
    let mut zero_rounds = spec(&[ExperimentArm::Dream]);
    zero_rounds.rounds = 0;
    let mut zero_workers = spec(&[ExperimentArm::Dream]);
    zero_workers.budget.workers = 0;
    for bad in [
        zero_rounds,
        spec(&[ExperimentArm::Dream, ExperimentArm::Dream]),
        spec(&[]),
        zero_workers,
    ] {
        assert!(matches!(
            run_experiment(&bad, &options(dir.path(), &clock)),
            Err(ExperimentError::Range(_))
        ));
    }
    assert!(!dir.path().join("experiments").exists());

    let k1_note = k1_stop_rule_note(5, &DEFAULT_POLICY).expect("note");
    let only_dream = plan_experiment(
        &spec(&[ExperimentArm::Dream]),
        &options(dir.path(), &clock),
        EXPERIMENT_ARMS,
    )
    .expect("plan");
    assert_eq!(
        only_dream.notes,
        [
            "no fixed arm ran: the headline multipliers are undefined".to_string(),
            k1_note.clone(),
            OBJECTIVE_NOTE.to_string()
        ]
    );
    let mut speedup = spec(&[ExperimentArm::Dream, ExperimentArm::Fixed]);
    speedup.task = DreamTaskId::PythonSpeedup;
    let speedup =
        plan_experiment(&speedup, &options(dir.path(), &clock), EXPERIMENT_ARMS).expect("plan");
    assert_eq!(
        speedup.notes,
        [
            timing_scoring_note(DreamTaskId::PythonSpeedup),
            k1_note,
            OBJECTIVE_NOTE.to_string()
        ]
    );
    assert!(
        speedup.notes[0].starts_with("python-speedup:")
            && speedup.notes[0].contains("timing noise")
    );
    assert_eq!(DreamTaskId::PythonSpeedup.scoring(), "timing");
    assert_eq!(DreamTaskId::CirclePacking.scoring(), "deterministic");
}

#[test]
fn the_k1_note_fires_only_for_a_beta_driven_stop_rule_that_cannot_fire() {
    let with = |stop_rule: StopRule, beta: u32| ExplorationPolicy {
        stop_rule,
        beta,
        ..DEFAULT_POLICY
    };
    assert!(k1_stop_rule_note(6, &DEFAULT_POLICY)
        .expect("note")
        .starts_with("k1 6 <= initialPolicy.beta 6: patience can never stop"));
    assert_eq!(k1_stop_rule_note(7, &DEFAULT_POLICY), None);
    assert!(k1_stop_rule_note(3, &with(StopRule::FixedRounds, 4))
        .expect("note")
        .contains("fixed-rounds can never stop a rollout before the round cap"));
    assert_eq!(k1_stop_rule_note(1, &with(StopRule::Never, 6)), None);
    assert_eq!(k1_stop_rule_note(1, &with(StopRule::Threshold, 6)), None);
}

#[test]
fn a_single_round_experiment_ties_every_arm() {
    let dir = tempfile::tempdir().expect("tempdir");
    let clock = || FIXED_CLOCK;
    let mut one = spec(&[ExperimentArm::Dream, ExperimentArm::Fixed]);
    one.rounds = 1;
    let result = run_experiment(&one, &options(dir.path(), &clock)).expect("experiment");
    assert!(result.arms.iter().all(|arm| arm.rounds.len() == 1));
    let headline = result.headline.expect("headline");
    assert_eq!(
        (
            headline.calls_multiplier["dream"].as_f64(),
            headline.score_multiplier["dream"].as_f64()
        ),
        (Some(1.0), Some(1.0))
    );
}

// --- the headline arithmetic ------------------------------------------------

fn row(
    round: u32,
    cumulative_best: f64,
    cumulative_probes: u32,
    improvements: Vec<ScoreImprovement>,
) -> ExperimentRoundRow {
    ExperimentRoundRow {
        round,
        tree_id: format!("t{round}"),
        policy_id: "p".to_string(),
        round_best: cumulative_best,
        cumulative_best,
        probes: 0,
        cumulative_probes,
        agent_generated_calls: 0,
        cumulative_agent_generated_calls: 0,
        local_fallbacks: 0,
        llm_proposals: 0,
        llm_accepted: 0,
        llm_rejected: RejectCounts::default(),
        decision_rounds: 1,
        pool_size: 0,
        handler_calls: DreamHandlerCalls::default(),
        cumulative_handler_calls: 0,
        tokens: 0,
        cumulative_tokens: 0,
        dreaming: None,
        probes_to_round_best: 0,
        improvements,
        priming_tree_ids: None,
        priming_probes: None,
    }
}

fn plain(round: u32, best: f64, probes: u32) -> ExperimentRoundRow {
    row(round, best, probes, Vec::new())
}

fn arm(arm: ExperimentArm, rows: Vec<ExperimentRoundRow>) -> ExperimentArmResult {
    let last = rows.last().expect("rows").clone();
    ExperimentArmResult {
        arm,
        fixed_policy: arm.fixed_policy(),
        guided: arm.guided(),
        mode: ExperimentArmMode::local(),
        store_dir: format!("experiments/x/{}", arm.as_str()),
        run_id: "r".to_string(),
        initial_policy_id: "p".to_string(),
        final_policy_id: "p".to_string(),
        selected_policy_id: "p".to_string(),
        policy_score_on_own_pool: PolicyScoreOnOwnPool {
            initial: 0.0,
            final_score: 0.0,
        },
        policy_changes: 0,
        rounds: rows,
        totals: ExperimentArmTotals {
            probes: last.cumulative_probes,
            agent_generated_calls: 0,
            local_fallbacks: 0,
            llm_proposals: 0,
            llm_accepted: 0,
            llm_rejected: RejectCounts::default(),
            handler_calls: 0,
            tokens: 0,
            final_best: last.cumulative_best,
        },
        stopped_early: 0,
        final_selection: Vec::new(),
    }
}

fn fixed_control() -> ExperimentArmResult {
    arm(
        ExperimentArm::Fixed,
        vec![plain(1, 1.0, 15), plain(2, 1.5, 30), plain(3, 1.5, 45)],
    )
}

fn number(map: &serde_json::Map<String, Value>, key: &str) -> Option<f64> {
    map.get(key).and_then(Value::as_f64)
}

#[test]
fn the_target_comes_from_the_control_and_compute_from_the_first_reaching_round() {
    let dream = arm(
        ExperimentArm::Dream,
        vec![plain(1, 1.0, 15), plain(2, 1.6, 27), plain(3, 1.7, 39)],
    );
    let headline = compute_headline(&[fixed_control(), dream]).expect("headline");
    assert_eq!(headline.target, 1.5);
    assert_eq!(
        Value::Object(headline.probes_to_target.clone()),
        json!({"fixed": 30.0, "dream": 27.0})
    );
    assert_eq!(number(&headline.calls_multiplier, "fixed"), Some(1.0));
    assert!(support::close(
        number(&headline.calls_multiplier, "dream").expect("calls"),
        30.0 / 27.0,
        12
    ));
    assert_eq!(headline.equal_budget, 39);
    assert_eq!(
        Value::Object(headline.best_at_budget.clone()),
        json!({"fixed": 1.5, "dream": 1.7})
    );
    assert!(support::close(
        number(&headline.score_multiplier, "dream").expect("score"),
        1.7 / 1.5,
        12
    ));
    assert_eq!(number(&headline.delta_best, "fixed"), Some(0.0));
    assert!(support::close(
        number(&headline.delta_best, "dream").expect("delta"),
        0.2,
        12
    ));
}

#[test]
fn an_unreached_target_is_null_and_a_slower_arm_scores_below_1() {
    let short = arm(
        ExperimentArm::Dream,
        vec![plain(1, 1.0, 15), plain(2, 1.2, 30), plain(3, 1.4, 45)],
    );
    let headline = compute_headline(&[fixed_control(), short]).expect("headline");
    assert_eq!(headline.probes_to_target["dream"], Value::Null);
    assert_eq!(headline.calls_multiplier["dream"], Value::Null);
    assert_eq!(number(&headline.best_at_budget, "dream"), Some(1.4));
    assert!(support::close(
        number(&headline.delta_best, "dream").expect("delta"),
        -0.1,
        12
    ));
    let slow = arm(
        ExperimentArm::Dream,
        vec![plain(1, 1.0, 15), plain(2, 1.2, 40), plain(3, 1.5, 60)],
    );
    let headline = compute_headline(&[fixed_control(), slow]).expect("headline");
    assert_eq!(number(&headline.probes_to_target, "dream"), Some(60.0));
    assert_eq!(number(&headline.calls_multiplier, "dream"), Some(0.5));
    assert_eq!(headline.equal_budget, 45);
    assert_eq!(number(&headline.best_at_budget, "dream"), Some(1.2));
}

#[test]
fn equal_budget_and_zero_denominators_are_null_never_infinite() {
    let big = arm(
        ExperimentArm::Dream,
        vec![plain(1, 2.0, 50), plain(2, 2.0, 100)],
    );
    let small = arm(ExperimentArm::Fixed, vec![plain(1, 1.0, 20)]);
    let headline = compute_headline(&[small, big]).expect("headline");
    assert_eq!(headline.equal_budget, 20);
    assert_eq!(
        (
            headline.best_at_budget["dream"].clone(),
            headline.score_multiplier["dream"].clone()
        ),
        (Value::Null, Value::Null)
    );
    let zero = arm(ExperimentArm::Fixed, vec![plain(1, 0.0, 10)]);
    let some = arm(ExperimentArm::Dream, vec![plain(1, 0.5, 10)]);
    let headline = compute_headline(&[zero.clone(), some]).expect("headline");
    assert_eq!(headline.score_multiplier["dream"], Value::Null);
    assert_eq!(number(&headline.delta_best, "dream"), Some(0.5));
    assert_eq!(number(&headline.calls_multiplier, "dream"), Some(1.0));
    let free = arm(ExperimentArm::Dream, vec![plain(1, 0.5, 0)]);
    let headline = compute_headline(&[zero, free]).expect("headline");
    assert_eq!(headline.calls_multiplier["dream"], Value::Null);
    assert_eq!(number(&headline.probes_to_target, "dream"), Some(0.0));
}

#[test]
fn the_exact_headline_counts_to_the_first_reaching_probe() {
    let point = |probe: u32, score: f64| ScoreImprovement { probe, score };
    let fixed = arm(
        ExperimentArm::Fixed,
        vec![
            row(1, 1.0, 15, vec![point(0, 0.2), point(3, 1.0)]),
            row(2, 1.5, 30, vec![point(7, 1.5)]),
            row(3, 1.5, 45, Vec::new()),
        ],
    );
    let dream = arm(
        ExperimentArm::Dream,
        vec![
            row(1, 1.6, 15, vec![point(2, 1.1), point(11, 1.6)]),
            row(2, 1.6, 27, Vec::new()),
        ],
    );
    assert_eq!(exact_probes_to_target(&fixed.rounds, 1.5), Some(22));
    assert_eq!(exact_probes_to_target(&dream.rounds, 1.5), Some(11));
    assert_eq!(exact_probes_to_target(&dream.rounds, 2.0), None);
    let headline = compute_headline(&[fixed.clone(), dream]).expect("headline");
    assert_eq!(
        Value::Object(headline.probes_to_target.clone()),
        json!({"fixed": 30.0, "dream": 15.0})
    );
    assert_eq!(
        Value::Object(headline.probes_to_target_exact.clone()),
        json!({"fixed": 22.0, "dream": 11.0})
    );
    assert_eq!(number(&headline.calls_multiplier, "dream"), Some(2.0));
    assert_eq!(
        (
            number(&headline.calls_multiplier_exact, "fixed"),
            number(&headline.calls_multiplier_exact, "dream")
        ),
        (Some(1.0), Some(2.0))
    );
    let root_best = arm(
        ExperimentArm::Dream,
        vec![row(1, 1.5, 15, vec![point(0, 1.5)])],
    );
    let headline = compute_headline(&[fixed, root_best]).expect("headline");
    assert_eq!(number(&headline.probes_to_target_exact, "dream"), Some(0.0));
    assert_eq!(headline.calls_multiplier_exact["dream"], Value::Null);
}

#[test]
fn there_is_no_headline_without_a_control_and_guided_arms_are_included() {
    assert_eq!(
        compute_headline(&[arm(ExperimentArm::Dream, vec![plain(1, 1.0, 1)])]),
        None
    );
    let guided = arm(
        ExperimentArm::DreamGuided,
        vec![plain(1, 1.0, 15), plain(2, 1.3, 30), plain(3, 1.4, 45)],
    );
    let headline = compute_headline(&[fixed_control(), guided]).expect("headline");
    assert!(support::close(
        number(&headline.delta_best, "dream-guided").expect("delta"),
        -0.1,
        12
    ));
    assert_eq!(headline.probes_to_target["dream-guided"], Value::Null);
}
