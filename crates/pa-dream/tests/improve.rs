//! The TS `dream-improve.test.ts` behaviour: the objective, the selection
//! rule, the measured pool, verdicts, the lever scan, the local dreamer and
//! the recorded regressions with their pinned numbers.

// Exact float equality is the claim: replay and the objective are deterministic
// IEEE-754 arithmetic, and the expected values are exact or recorded.
#![allow(clippy::float_cmp)]

mod support;

use std::collections::HashSet;

use pa_dream::improve::{
    dreamer_kind_of, lever_scan_grid, measure_pool, mutate_policy, propose_policies, run_dreaming,
    run_lever_scan, score_policy_on_pool, select_best_policy, CandidateInput, CandidateOrigin,
    CandidateReason, DreamerKind, DreamingOptions, DreamingScoreConfig, PoolScore, SpendCharge,
    LEVER_SCAN_BETAS,
};
use pa_dream::objective::{
    compute_objective, compute_objective_terms, normalized_quality, pool_score_scale,
    ObjectiveBudget, ObjectiveEvidence, ObjectiveScale, ReplayObjectiveConfig, DEFAULT_OBJECTIVE,
};
use pa_dream::policy::{
    parse_exploration_policy, policy_fields_differing, policy_id, ExplorationPolicy, PolicyField,
    RecoveryPolicy, SelectionRule, StopRule, DEFAULT_POLICY, REPLAY_DEAD_FIELDS, SELECTION_RULES,
    STOP_RULES,
};
use pa_dream::replay::{simulate_policy, ReplayConfig, ReplayResult};
use pa_dream::store::RecordedTree;
use support::{fixture, local, policy, rng, tree, Fixed};

const OBJECTIVE: ReplayObjectiveConfig = DEFAULT_OBJECTIVE;

fn cfg(k1: u32, k2: u32) -> DreamingScoreConfig {
    DreamingScoreConfig {
        k1,
        k2,
        objective: DEFAULT_OBJECTIVE,
        quality_eps: 0.0,
    }
}

fn with_objective(
    base: DreamingScoreConfig,
    objective: ReplayObjectiveConfig,
) -> DreamingScoreConfig {
    DreamingScoreConfig { objective, ..base }
}

const SYNTH_SCALE: ObjectiveScale = ObjectiveScale {
    score_min: 0.3,
    score_max: 0.9,
};
const SYNTH_BUDGET: ObjectiveBudget = ObjectiveBudget { workers: 2, k1: 5 };

fn synth(tree_id: &str) -> RecordedTree {
    tree(
        tree_id,
        2,
        &[
            (0, None, 0, 0, 0.3),
            (1, Some(0), 0, 0, 0.5),
            (2, Some(0), 0, 1, 0.4),
            (3, Some(0), 0, 2, 0.9),
        ],
    )
}

fn pool() -> Vec<RecordedTree> {
    vec![synth("synth")]
}

fn twin_pool() -> Vec<RecordedTree> {
    vec![synth("synth"), synth("synth2")]
}

fn current() -> ExplorationPolicy {
    policy(|p| {
        p.selection_rule = SelectionRule::ExploreRoot;
        p.stop_rule = StopRule::Patience;
        p.beta = 1;
        p.batch_size = 1;
    })
}

fn better() -> ExplorationPolicy {
    policy(|p| {
        p.selection_rule = SelectionRule::ExploreRoot;
        p.stop_rule = StopRule::Never;
        p.batch_size = 1;
    })
}

fn worse() -> ExplorationPolicy {
    policy(|p| {
        p.selection_rule = SelectionRule::BestFirst;
        p.stop_rule = StopRule::Never;
        p.batch_size = 1;
    })
}

const CFG: DreamingScoreConfig = DreamingScoreConfig {
    k1: 5,
    k2: 10,
    objective: DEFAULT_OBJECTIVE,
    quality_eps: 0.0,
};

fn select(
    current: &ExplorationPolicy,
    candidates: &[ExplorationPolicy],
    pool: &[RecordedTree],
    cfg: &DreamingScoreConfig,
) -> pa_dream::improve::PolicySelection {
    select_best_policy(current, &local(candidates), pool, cfg, &HashSet::new())
}

fn replay(
    n: u32,
    rounds: u32,
    best: f64,
    out_of_support: u32,
    best_so_far: Option<Vec<f64>>,
) -> ReplayResult {
    let selected = n + out_of_support;
    ReplayResult {
        policy_id: "x".to_string(),
        tree_id: "t".to_string(),
        revealed_ids: Vec::new(),
        n,
        rounds,
        best_score: best,
        out_of_support_cells: out_of_support,
        selected_cells: selected,
        in_support: if selected == 0 {
            1.0
        } else {
            f64::from(n) / f64::from(selected)
        },
        best_so_far: best_so_far.unwrap_or_else(|| vec![best; selected as usize]),
        probes_to_best: u32::from(selected != 0),
        rounds_to_best: u32::from(selected != 0),
    }
}

fn reasons(selection: &[pa_dream::improve::CandidateVerdict]) -> Vec<CandidateReason> {
    selection.iter().map(|verdict| verdict.reason).collect()
}

// --- the objective -----------------------------------------------------------

#[test]
fn v_is_quality_anytime_charged_cost_and_rounds_saved() {
    let base = replay(3, 3, 0.9, 0, None);
    let terms = compute_objective_terms(&base, &OBJECTIVE, SYNTH_SCALE, SYNTH_BUDGET, None);
    let expected = 0.75 + 0.25 - 0.05 * (3.0 / 10.0) + 0.1 * (1.0 - 3.0 / 5.0);
    assert_eq!((terms.quality, terms.anytime), (1.0, 1.0));
    assert_close!(terms.cost, 0.3, 12);
    assert_close!(terms.rounds_saved, 0.4, 12);
    assert_close!(terms.value, expected, 12);
    assert_close!(terms.value, 1.025, 12);

    let rising = replay(5, 3, 0.9, 0, Some(vec![0.3, 0.6, 0.6, 0.9, 0.9]));
    let terms = compute_objective_terms(&rising, &OBJECTIVE, SYNTH_SCALE, SYNTH_BUDGET, None);
    assert_close!(
        terms.anytime,
        (0.0 + 0.5 + 0.5 + 1.0 + 1.0 + 5.0) / 10.0,
        12
    );
    let betas = ReplayObjectiveConfig {
        beta1: 0.2,
        beta2: 0.1,
        beta3: 0.5,
    };
    assert_close!(
        compute_objective(&rising, &betas, SYNTH_SCALE, SYNTH_BUDGET),
        0.5 + 0.5 * 0.8 - 0.2 * 0.5 + 0.1 * (1.0 - 3.0 / 5.0),
        12
    );

    let full = compute_objective_terms(
        &replay(10, 5, 0.9, 0, None),
        &OBJECTIVE,
        SYNTH_SCALE,
        SYNTH_BUDGET,
        None,
    );
    assert_eq!((full.cost, full.rounds_saved), (1.0, 0.0));
    assert_close!(full.value, 1.0 - OBJECTIVE.beta1, 12);
    let over = compute_objective_terms(
        &replay(6, 10, 0.9, 6, None),
        &OBJECTIVE,
        SYNTH_SCALE,
        SYNTH_BUDGET,
        None,
    );
    assert_close!(over.cost, 1.2, 12);
    assert_eq!(over.rounds_saved, -1.0);
    assert!(
        (over.value - over.quality).abs() <= OBJECTIVE.beta1 * over.cost + OBJECTIVE.beta2 + 1e-12
    );
    assert_close!(
        compute_objective(
            &replay(0, 0, 0.9, 0, None),
            &OBJECTIVE,
            SYNTH_SCALE,
            SYNTH_BUDGET
        ),
        1.0 + OBJECTIVE.beta2,
        12
    );
}

#[test]
fn the_strictly_cost_form_and_the_out_of_support_charge() {
    let cost_only = ReplayObjectiveConfig {
        beta1: 0.05,
        beta2: 0.0,
        beta3: 0.0,
    };
    for result in [
        replay(4, 2, 0.6, 3, Some(vec![0.3, 0.3, 0.6, 0.6, 0.6, 0.6, 0.6])),
        replay(10, 5, 0.9, 0, None),
        replay(1, 1, 0.4, 0, None),
    ] {
        let terms = compute_objective_terms(&result, &cost_only, SYNTH_SCALE, SYNTH_BUDGET, None);
        let charged = f64::from(result.n + result.out_of_support_cells);
        assert_close!(terms.value, terms.quality - 0.05 * (charged / 10.0), 12);
    }
    let on = compute_objective_terms(
        &replay(4, 3, 0.9, 0, Some(vec![0.5, 0.5, 0.9, 0.9])),
        &OBJECTIVE,
        SYNTH_SCALE,
        SYNTH_BUDGET,
        None,
    );
    let off = compute_objective_terms(
        &replay(4, 3, 0.9, 1, Some(vec![0.5, 0.5, 0.5, 0.9, 0.9])),
        &OBJECTIVE,
        SYNTH_SCALE,
        SYNTH_BUDGET,
        None,
    );
    assert_eq!(off.quality, on.quality);
    assert!(off.anytime <= on.anytime && off.cost > on.cost && off.value < on.value);
}

#[test]
fn an_evidence_horizon_charges_the_spend_and_never_below_it() {
    let cheap = replay(2, 1, 0.9, 0, None);
    let raw = compute_objective_terms(&cheap, &OBJECTIVE, SYNTH_SCALE, SYNTH_BUDGET, None);
    assert_eq!((raw.charged_probes, raw.charged_rounds), (2.0, 1.0));
    let evidence = |probes: f64, rounds: f64| Some(ObjectiveEvidence { probes, rounds });
    let backed = compute_objective_terms(
        &cheap,
        &OBJECTIVE,
        SYNTH_SCALE,
        SYNTH_BUDGET,
        evidence(7.0, 4.0),
    );
    assert_eq!((backed.charged_probes, backed.charged_rounds), (7.0, 4.0));
    assert_close!(backed.cost, 0.7, 12);
    assert_close!(backed.rounds_saved, 0.2, 12);
    assert_eq!((backed.quality, backed.anytime), (raw.quality, raw.anytime));
    assert_close!(
        raw.value - backed.value,
        OBJECTIVE.beta1 * 0.5 + OBJECTIVE.beta2 * 0.6,
        12
    );
    let under = compute_objective_terms(
        &cheap,
        &OBJECTIVE,
        SYNTH_SCALE,
        SYNTH_BUDGET,
        evidence(1.0, 0.0),
    );
    assert_eq!(under, raw);
    let alone = compute_objective_terms(
        &cheap,
        &OBJECTIVE,
        SYNTH_SCALE,
        SYNTH_BUDGET,
        evidence(10.0, 5.0),
    );
    assert_eq!((alone.cost, alone.rounds_saved), (1.0, 0.0));
    assert_close!(alone.value, 1.0 - OBJECTIVE.beta1, 12);
}

#[test]
fn quality_normalizes_to_the_pool_range_and_guards_a_degenerate_scale() {
    let checks = [
        normalized_quality(0.3, SYNTH_SCALE),
        normalized_quality(0.9, SYNTH_SCALE),
        normalized_quality(0.1, SYNTH_SCALE),
        normalized_quality(5.0, SYNTH_SCALE),
        normalized_quality(0.0, SYNTH_SCALE),
    ];
    assert_eq!(checks, [0.0, 1.0, 0.0, 1.0, 0.0]);
    assert_close!(normalized_quality(0.6, SYNTH_SCALE), 0.5, 12);
    let flat = ObjectiveScale {
        score_min: 0.7,
        score_max: 0.7,
    };
    let zero = ObjectiveScale {
        score_min: 0.0,
        score_max: 0.0,
    };
    assert_eq!(
        [
            normalized_quality(0.7, flat),
            normalized_quality(0.71, flat),
            normalized_quality(0.69, flat),
            normalized_quality(0.0, flat),
            normalized_quality(0.0, zero),
        ],
        [1.0, 1.0, 0.0, 0.0, 1.0]
    );
    assert_eq!(pool_score_scale(&pool()), SYNTH_SCALE);
    assert_eq!(pool_score_scale(&[] as &[RecordedTree]), zero);
    let mut with_invalid = tree(
        "synth",
        2,
        &[
            (0, None, 0, 0, 0.3),
            (1, Some(0), 0, 0, 0.0),
            (2, Some(0), 0, 1, 7.0),
        ],
    );
    with_invalid.nodes[1].valid = false;
    assert_eq!(
        pool_score_scale([&with_invalid]),
        ObjectiveScale {
            score_min: 0.3,
            score_max: 7.0
        }
    );
}

// --- pool scores and the selection rule ----------------------------------

#[test]
fn a_lone_tree_charges_every_candidate_the_whole_budget() {
    let trees = pool();
    let replayed = simulate_policy(&trees[0], &current(), ReplayConfig { k2: 10 });
    let budget = ObjectiveBudget { workers: 2, k1: 5 };
    let terms = compute_objective_terms(
        &replayed,
        &OBJECTIVE,
        pool_score_scale(&trees),
        budget,
        Some(ObjectiveEvidence {
            probes: 10.0,
            rounds: 5.0,
        }),
    );
    let score = score_policy_on_pool(&current(), &trees, &CFG, SpendCharge::Evidence);
    assert_close!(score.value, terms.value, 12);
    assert_eq!(
        (score.charged_probes, score.charged_rounds, score.n),
        (10.0, 5.0, 2.0)
    );
    let raw = compute_objective_terms(
        &replayed,
        &OBJECTIVE,
        pool_score_scale(&trees),
        budget,
        None,
    );
    assert_close!(
        raw.value - score.value,
        OBJECTIVE.beta1 * 0.8 + OBJECTIVE.beta2 * 0.6,
        12
    );
    assert_eq!(
        score_policy_on_pool(&current(), &[], &CFG, SpendCharge::Evidence),
        PoolScore::empty()
    );
}

#[test]
fn the_selection_picks_a_strictly_better_candidate_and_keeps_current_on_ties_and_losses() {
    let won = select(&current(), &[better()], &pool(), &CFG);
    assert!(won.improved && won.chosen_policy == better() && won.chosen_score > won.current_score);
    assert_eq!((won.scored_count, won.quality_rejected), (2, 0));
    let kept = select(&current(), &[worse()], &pool(), &CFG);
    assert!(!kept.improved && kept.chosen_policy == current());
    let tie = select(&current(), &[current()], &pool(), &CFG);
    assert!(!tie.improved && tie.chosen_policy == current());
    let mixed = select(&current(), &[worse(), better(), current()], &pool(), &CFG);
    assert!(mixed.improved && mixed.chosen_policy == better());
}

#[test]
fn the_quality_guard_rejects_a_cheaper_lower_quality_candidate_unless_relaxed() {
    let one_probe = policy(|p| {
        p.selection_rule = SelectionRule::ExploreRoot;
        p.stop_rule = StopRule::FixedRounds;
        p.beta = 1;
        p.batch_size = 1;
    });
    let heavy = with_objective(
        CFG,
        ReplayObjectiveConfig {
            beta1: 5.0,
            beta2: 0.0,
            beta3: 0.0,
        },
    );
    let better_score = score_policy_on_pool(&better(), &twin_pool(), &heavy, SpendCharge::Evidence);
    let cheap = score_policy_on_pool(&one_probe, &twin_pool(), &heavy, SpendCharge::Evidence);
    assert_eq!(
        (cheap.charged_probes, better_score.charged_probes),
        (1.0, 3.0)
    );
    assert_close!(cheap.value, 1.0 / 3.0 - 0.5, 12);
    assert_close!(better_score.value, 1.0 - 1.5, 12);
    assert!(cheap.value > better_score.value && cheap.quality < better_score.quality);
    let selection = select(&better(), &[one_probe], &twin_pool(), &heavy);
    assert!(!selection.improved && selection.chosen_policy == better());
    assert_eq!(
        (
            selection.quality_rejected,
            selection.current_quality,
            selection.evidence_trees
        ),
        (1, 1.0, 1)
    );
    let relaxed = select(
        &better(),
        &[one_probe],
        &twin_pool(),
        &DreamingScoreConfig {
            quality_eps: 1.0,
            ..heavy
        },
    );
    assert!(relaxed.chosen_policy == one_probe && relaxed.quality_rejected == 0);
    let alone = score_policy_on_pool(&one_probe, &pool(), &heavy, SpendCharge::Evidence);
    assert_eq!(alone.charged_probes, 10.0);
    assert!(
        alone.value < score_policy_on_pool(&better(), &pool(), &heavy, SpendCharge::Evidence).value
    );
}

// --- the recorded circle-packing collapse (fixtures, W 4, k1 12, k2 24) ----

fn recorded_pool() -> Vec<RecordedTree> {
    vec![
        fixture("circle-packing-s7-i0-1789842143996.jsonl"),
        fixture("circle-packing-s7-i1-1789842143996.jsonl"),
    ]
}

fn collapsed() -> ExplorationPolicy {
    policy(|p| {
        p.selection_rule = SelectionRule::Weighted;
        p.stop_rule = StopRule::FixedRounds;
        p.beta = 1;
    })
}

fn old_raw_objective(replay: &ReplayResult) -> f64 {
    replay.best_score - 0.01 * f64::from(replay.n)
        + (0.02 * f64::from(replay.n)) / f64::from(replay.rounds.max(1))
}

#[test]
fn the_recorded_collapse_reproduces_under_the_raw_objective() {
    let trees = recorded_pool();
    assert_eq!(policy_id(&DEFAULT_POLICY), "1be99d403b0405a3");
    assert_eq!(policy_id(&collapsed()), "f559ec93fc3b1773");
    let exploring: Vec<ReplayResult> = trees
        .iter()
        .map(|tree| simulate_policy(tree, &DEFAULT_POLICY, ReplayConfig { k2: 24 }))
        .collect();
    let collapse: Vec<ReplayResult> = trees
        .iter()
        .map(|tree| simulate_policy(tree, &collapsed(), ReplayConfig { k2: 24 }))
        .collect();
    let mean = |results: &[ReplayResult], f: &dyn Fn(&ReplayResult) -> f64| {
        results.iter().map(f).sum::<f64>() / 2.0
    };
    assert!(mean(&exploring, &|r| r.best_score) > mean(&collapse, &|r| r.best_score) + 0.2);
    assert!(collapse.iter().all(|r| r.n == 1 && r.rounds == 1));
    assert!(exploring.iter().all(|r| r.n >= 30));
    assert!(old_raw_objective(&collapse[0]) > old_raw_objective(&exploring[0]) + 0.1);
    assert!(old_raw_objective(&exploring[1]) - old_raw_objective(&collapse[1]) < 0.01);
    assert!(mean(&collapse, &old_raw_objective) > mean(&exploring, &old_raw_objective) + 0.05);
}

#[test]
fn the_normalized_objective_ranks_exploring_above_the_collapse_with_the_pinned_numbers() {
    let trees = recorded_pool();
    let scale = pool_score_scale(&trees);
    assert_close!(scale.score_min, 0.774_615, 6);
    assert_close!(scale.score_max, 1.258_098, 6);
    for tree in &trees {
        let budget = ObjectiveBudget {
            workers: tree.header.w,
            k1: 12,
        };
        let exploring = compute_objective(
            &simulate_policy(tree, &DEFAULT_POLICY, ReplayConfig { k2: 24 }),
            &OBJECTIVE,
            scale,
            budget,
        );
        let collapse = compute_objective(
            &simulate_policy(tree, &collapsed(), ReplayConfig { k2: 24 }),
            &OBJECTIVE,
            scale,
            budget,
        );
        assert!(exploring > collapse + 0.2);
    }
    let recorded_cfg = cfg(12, 24);
    let exploring = score_policy_on_pool(
        &DEFAULT_POLICY,
        &trees,
        &recorded_cfg,
        SpendCharge::Evidence,
    );
    let collapse = score_policy_on_pool(&collapsed(), &trees, &recorded_cfg, SpendCharge::Evidence);
    assert_close!(exploring.value, 0.797_39, 5);
    assert_close!(collapse.value, 0.435_252, 5);
    assert_close!(exploring.anytime, 0.708_267, 5);
    assert_close!(exploring.cost, 37.0 / 48.0, 12);
    assert_eq!(exploring.rounds_saved, 0.0);
    assert_close!(collapse.rounds_saved, 11.0 / 12.0, 12);
    let break_even = (0.75 * (exploring.quality - collapse.quality)
        + 0.25 * (exploring.anytime - collapse.anytime)
        + 0.1 * (exploring.rounds_saved - collapse.rounds_saved))
        / (exploring.cost - collapse.cost);
    assert_close!(break_even, 0.5329, 3);
}

#[test]
fn the_collapse_never_wins_and_the_chain_loses_on_rounds() {
    let trees = recorded_pool();
    let recorded_cfg = cfg(12, 24);
    let keep = select(
        &DEFAULT_POLICY,
        &[
            collapsed(),
            policy(|p| {
                p.batch_size = 1;
                p.beta = 1;
            }),
        ],
        &trees,
        &recorded_cfg,
    );
    assert!(keep.chosen_policy == DEFAULT_POLICY && !keep.improved && keep.quality_rejected >= 1);
    let recover = select(&collapsed(), &[DEFAULT_POLICY], &trees, &recorded_cfg);
    assert!(recover.chosen_policy == DEFAULT_POLICY && recover.improved);
    assert!(recover.chosen_quality > recover.current_quality);

    let one_probe = policy(|p| {
        p.selection_rule = SelectionRule::ExploreRoot;
        p.stop_rule = StopRule::Never;
        p.batch_size = 1;
    });
    for current in [
        DEFAULT_POLICY,
        policy(|p| p.beta = 3),
        policy(|p| p.selection_rule = SelectionRule::Weighted),
    ] {
        let selection = select(&current, &[one_probe], &trees, &recorded_cfg);
        assert_eq!(selection.chosen_policy, current);
        assert!(!selection.candidates[0].eligible);
        assert_eq!(
            selection.candidates[0].reason,
            CandidateReason::Unmeasurable
        );
        assert_eq!(selection.quality_rejected, 0);
    }
    let chain = policy(|p| {
        p.stop_rule = StopRule::Never;
        p.batch_size = 1;
    });
    let chain_score = score_policy_on_pool(&chain, &trees, &recorded_cfg, SpendCharge::Evidence);
    let exploring = score_policy_on_pool(
        &DEFAULT_POLICY,
        &trees,
        &recorded_cfg,
        SpendCharge::Evidence,
    );
    assert_close!(chain_score.quality, exploring.quality, 12);
    assert_eq!((chain_score.rounds, chain_score.rounds_saved), (24.0, -1.0));
    assert!(chain_score.cost < exploring.cost && chain_score.anytime > exploring.anytime);
    assert_close!(chain_score.value, 0.734_635, 5);
    let selection = select(&DEFAULT_POLICY, &[chain], &trees, &recorded_cfg);
    assert_eq!(selection.chosen_policy, DEFAULT_POLICY);
    assert!(selection.candidates[0].eligible && selection.candidates[0].in_support_min < 1.0);
    assert_eq!(
        selection.candidates[0].reason,
        CandidateReason::Unmeasurable
    );
}

#[test]
fn v_is_scale_invariant() {
    let trees = recorded_pool();
    let scaled: Vec<RecordedTree> = trees
        .iter()
        .map(|tree| {
            let mut records = vec![pa_dream::records::TreeRecord::Header(tree.header.clone())];
            records.extend(tree.nodes.iter().map(|node| {
                let mut node = node.clone();
                node.score *= 1000.0;
                pa_dream::records::TreeRecord::Node(node)
            }));
            RecordedTree::from_records(records).expect("scaled")
        })
        .collect();
    let recorded_cfg = cfg(12, 24);
    let policies = [
        DEFAULT_POLICY,
        policy(|p| {
            p.batch_size = 1;
            p.stop_rule = StopRule::FixedRounds;
            p.beta = 1;
        }),
        policy(|p| {
            p.selection_rule = SelectionRule::RoundRobin;
            p.beta = 2;
        }),
        policy(|p| {
            p.selection_rule = SelectionRule::ExploreRoot;
            p.stop_rule = StopRule::Never;
            p.batch_size = 2;
        }),
    ];
    for candidate in &policies {
        let original =
            score_policy_on_pool(candidate, &trees, &recorded_cfg, SpendCharge::Evidence);
        let rescaled =
            score_policy_on_pool(candidate, &scaled, &recorded_cfg, SpendCharge::Evidence);
        assert_close!(rescaled.value, original.value, 9);
        assert_close!(rescaled.quality, original.quality, 9);
    }
    let a = select(&policies[0], &policies[1..], &trees, &recorded_cfg);
    let b = select(&policies[0], &policies[1..], &scaled, &recorded_cfg);
    assert_eq!(policy_id(&a.chosen_policy), policy_id(&b.chosen_policy));
    assert_eq!(a.quality_rejected, b.quality_rejected);
}

// --- the recorded run-3 collapse and the run-2 lever (autocorrelation) -----

const RUN3_TREE: &str = "autocorrelation-s7-i0-1789923274195.jsonl";

fn run2_trees() -> Vec<RecordedTree> {
    (0..4)
        .map(|index| fixture(&format!("autocorrelation-s7-i{index}-1789858196752.jsonl")))
        .collect()
}

fn run3_collapse() -> ExplorationPolicy {
    policy(|p| {
        p.stop_rule = StopRule::FixedRounds;
        p.beta = 1;
    })
}

fn run3_patient() -> ExplorationPolicy {
    policy(|p| {
        p.stop_rule = StopRule::Patience;
        p.beta = 2;
    })
}

#[test]
fn run_3_reproduces_under_raw_terms() {
    let tree = fixture(RUN3_TREE);
    assert_eq!(tree.header.w, 3);
    assert_eq!(policy_id(&run3_collapse()), "9edb7a5b887e861c");
    assert_eq!(policy_id(&run3_patient()), "a70474c08b2c78a1");
    let incumbent = simulate_policy(&tree, &DEFAULT_POLICY, ReplayConfig { k2: 26 });
    assert_eq!(
        (
            incumbent.n,
            incumbent.rounds,
            incumbent.out_of_support_cells,
            incumbent.probes_to_best,
            incumbent.rounds_to_best
        ),
        (13, 7, 0, 1, 1)
    );
    assert_eq!(incumbent.best_score, 0.524_727_268_236_976_9);
    let collapse = simulate_policy(&tree, &run3_collapse(), ReplayConfig { k2: 26 });
    assert_eq!(
        (collapse.n, collapse.rounds, collapse.probes_to_best),
        (1, 1, 1)
    );
    assert_eq!(collapse.best_score, incumbent.best_score);
    let scale = pool_score_scale([&tree]);
    let budget = ObjectiveBudget { workers: 3, k1: 13 };
    let incumbent_raw = compute_objective_terms(&incumbent, &OBJECTIVE, scale, budget, None);
    let collapse_raw = compute_objective_terms(&collapse, &OBJECTIVE, scale, budget, None);
    assert_eq!(
        (
            incumbent_raw.quality,
            incumbent_raw.anytime,
            collapse_raw.anytime
        ),
        (1.0, 1.0, 1.0)
    );
    assert_close!(incumbent_raw.value, 1.029_487, 6);
    assert_close!(collapse_raw.value, 1.091_026, 6);
    assert_close!(
        collapse_raw.value - incumbent_raw.value,
        0.05 * (12.0 / 39.0) + 0.1 * (6.0 / 13.0),
        12
    );
}

#[test]
fn run_3_earns_no_stop_early_credit_on_a_single_tree() {
    let trees = vec![fixture(RUN3_TREE)];
    let threshold = policy(|p| {
        p.stop_rule = StopRule::Threshold;
        p.target_score = 0.524_727_268_236_976_9;
    });
    let run3_cfg = cfg(13, 26);
    let selection = select(
        &DEFAULT_POLICY,
        &[run3_collapse(), threshold],
        &trees,
        &run3_cfg,
    );
    assert!(!selection.improved && selection.chosen_policy == DEFAULT_POLICY);
    assert_eq!((selection.measured_trees, selection.evidence_trees), (1, 0));
    assert_eq!(
        (
            selection.current.charged_probes,
            selection.current.charged_rounds
        ),
        (13.0, 7.0)
    );
    assert_close!(selection.current_score, 1.029_487, 6);
    assert_eq!(selection.current_min_best, 0.524_727_268_236_976_9);
    for verdict in &selection.candidates {
        assert_eq!(verdict.reason, CandidateReason::Worse);
        assert!(verdict.eligible);
        assert_eq!(
            (
                verdict.n,
                verdict.rounds,
                verdict.charged_probes,
                verdict.charged_rounds,
                verdict.evidence_trees
            ),
            (1.0, 1.0, 39.0, 13.0, 0)
        );
        assert_close!(verdict.value, 0.95, 12);
    }
    let scan = run_lever_scan(&DEFAULT_POLICY, &trees, &run3_cfg);
    assert_eq!((scan.policies, scan.eligible, scan.gap), (337, 336, 0.0));
    assert_eq!(scan.best_policy_id, policy_id(&DEFAULT_POLICY));
}

#[test]
fn run_3_charges_a_candidate_the_spend_it_still_needed_on_the_other_tree() {
    let pool = vec![
        fixture(RUN3_TREE),
        fixture("autocorrelation-s7-i0-1789858196752.jsonl"),
    ];
    let incumbent_b = simulate_policy(&pool[1], &DEFAULT_POLICY, ReplayConfig { k2: 26 });
    assert_eq!(
        (
            incumbent_b.n,
            incumbent_b.rounds,
            incumbent_b.probes_to_best,
            incumbent_b.rounds_to_best
        ),
        (13, 6, 8, 5)
    );
    let patient_a = simulate_policy(&pool[0], &run3_patient(), ReplayConfig { k2: 26 });
    assert_eq!(
        (patient_a.n, patient_a.rounds, patient_a.probes_to_best),
        (4, 3, 1)
    );
    let run3_cfg = cfg(13, 26);
    let selection = select(
        &DEFAULT_POLICY,
        &[run3_patient(), run3_collapse()],
        &pool,
        &run3_cfg,
    );
    assert_eq!((selection.measured_trees, selection.evidence_trees), (2, 1));
    let (patient, collapse) = (&selection.candidates[0], &selection.candidates[1]);
    assert_eq!(
        (
            patient.in_support_min,
            patient.n,
            patient.rounds,
            patient.charged_probes,
            patient.charged_rounds
        ),
        (1.0, 8.5, 4.5, 10.5, 5.5)
    );
    assert_close!(patient.cost, f64::midpoint(8.0 / 39.0, 13.0 / 39.0), 12);
    assert_close!(
        patient.rounds_saved,
        f64::midpoint(1.0 - 5.0 / 13.0, 1.0 - 6.0 / 13.0),
        12
    );
    assert_eq!(
        (
            selection.current.charged_probes,
            selection.current.charged_rounds
        ),
        (13.0, 6.5)
    );
    assert_eq!(
        selection.current,
        score_policy_on_pool(&DEFAULT_POLICY, &pool, &run3_cfg, SpendCharge::Raw)
    );
    assert!(patient.eligible && patient.reason == CandidateReason::Winner && selection.improved);
    assert_eq!(collapse.reason, CandidateReason::QualityRejected);
    assert_eq!(
        simulate_policy(&pool[1], &run3_collapse(), ReplayConfig { k2: 26 }).best_score,
        0.5
    );
}

#[test]
fn the_run_2_pool_keeps_its_lever_under_the_evidence_rule() {
    let trees = run2_trees();
    let run2_cfg = cfg(6, 12);
    let incumbent = score_policy_on_pool(&DEFAULT_POLICY, &trees, &run2_cfg, SpendCharge::Raw);
    assert_eq!(
        (
            incumbent.n,
            incumbent.charged_probes,
            incumbent.charged_rounds
        ),
        (11.75, 11.75, 6.0)
    );
    assert_close!(incumbent.quality, 0.883_344, 6);
    assert_close!(incumbent.anytime, 0.849_147, 6);
    assert_close!(incumbent.cost, 11.75 / 18.0, 12);
    assert_close!(incumbent.value, 0.842_156, 6);
    let surcharged =
        score_policy_on_pool(&DEFAULT_POLICY, &trees, &run2_cfg, SpendCharge::Evidence);
    assert_eq!(surcharged.charged_probes, 12.0);
    assert_close!(surcharged.value, 0.841_462, 6);
    let scan = run_lever_scan(&DEFAULT_POLICY, &trees, &run2_cfg);
    assert_eq!((scan.policies, scan.eligible), (337, 84));
    assert_close!(scan.gap, 0.007_109, 6);
    let winner = policy(|p| {
        p.selection_rule = SelectionRule::Weighted;
        p.stop_rule = StopRule::FixedRounds;
        p.batch_size = 2;
        p.beta = 6;
    });
    assert_eq!(scan.best_policy_id, policy_id(&winner));
    let winner_score = score_policy_on_pool(&winner, &trees, &run2_cfg, SpendCharge::Evidence);
    assert_close!(winner_score.quality, incumbent.quality, 12);
    assert_eq!(
        (
            winner_score.n,
            winner_score.charged_probes,
            winner_score.rounds
        ),
        (9.0, 9.25, 6.0)
    );
    assert_close!(winner_score.value, 0.849_265, 6);
    let selection = select(&DEFAULT_POLICY, &[winner], &trees, &run2_cfg);
    assert_eq!(selection.current, incumbent);
    assert_close!(
        selection.chosen_score - selection.current_score,
        scan.gap,
        12
    );
}

// --- (c) the incumbent's raw spend; (a) same best for less ------------------

fn flip_pool() -> Vec<RecordedTree> {
    vec![
        tree(
            "flip-early",
            2,
            &[
                (0, None, 0, 0, 0.798_354),
                (1, Some(0), 1, 0, 0.793_301),
                (2, Some(0), 2, 1, 0.763_679),
            ],
        ),
        tree(
            "flip-late",
            2,
            &[
                (0, None, 0, 0, 0.802_616),
                (1, Some(0), 1, 0, 0.788_096),
                (2, Some(0), 2, 1, 0.820_952),
                (3, Some(2), 3, 0, 0.824_312),
                (4, Some(1), 3, 0, 0.788_096),
                (5, Some(3), 4, 0, 0.865_302),
                (6, Some(0), 4, 2, 0.800_283),
                (7, Some(5), 5, 0, 0.840_815),
                (8, Some(0), 5, 3, 0.828_011),
                (9, Some(7), 6, 0, 0.806_434),
                (10, Some(8), 6, 0, 0.802_616),
            ],
        ),
    ]
}

#[test]
fn the_incumbent_is_charged_its_raw_spend_and_a_chain_cannot_flip_it() {
    let pool = flip_pool();
    let flip_cfg = cfg(6, 12);
    let incumbent_policy = policy(|p| {
        p.batch_size = 2;
        p.beta = 2;
    });
    let chain_policy = policy(|p| {
        p.batch_size = 1;
        p.beta = 2;
    });
    let early = simulate_policy(&pool[0], &incumbent_policy, ReplayConfig { k2: 12 });
    assert_eq!(
        (
            early.n,
            early.rounds,
            early.probes_to_best,
            early.best_score
        ),
        (2, 2, 0, 0.798_354)
    );
    let late = simulate_policy(&pool[1], &incumbent_policy, ReplayConfig { k2: 12 });
    assert_eq!(
        (
            late.n,
            late.rounds,
            late.probes_to_best,
            late.rounds_to_best
        ),
        (10, 6, 5, 4)
    );
    let raw = score_policy_on_pool(&incumbent_policy, &pool, &flip_cfg, SpendCharge::Raw);
    let surcharged =
        score_policy_on_pool(&incumbent_policy, &pool, &flip_cfg, SpendCharge::Evidence);
    let chain = score_policy_on_pool(&chain_policy, &pool, &flip_cfg, SpendCharge::Evidence);
    assert_eq!((raw.charged_probes, raw.charged_rounds), (6.0, 4.0));
    assert_eq!(
        (surcharged.charged_probes, surcharged.charged_rounds),
        (7.5, 5.0)
    );
    assert_eq!(
        (chain.n, chain.charged_probes, chain.charged_rounds),
        (4.0, 5.0, 5.0)
    );
    assert!(chain.value < raw.value && chain.value > surcharged.value);
    assert_close!(raw.value, 0.659_565, 6);
    assert_close!(surcharged.value, 0.636_648, 6);
    assert_close!(chain.value, 0.651_266, 6);
    let selection = select(&incumbent_policy, &[chain_policy], &pool, &flip_cfg);
    assert!(!selection.improved);
    assert_eq!(selection.current, raw);
    assert_eq!(selection.current_min_best, 0.798_354);
    assert_eq!(selection.candidates[0].reason, CandidateReason::Worse);
    assert_eq!(selection.candidates[0].charged_probes, 5.0);
    for (current, pool, cfg) in [
        (incumbent_policy, flip_pool(), flip_cfg),
        (DEFAULT_POLICY, run2_trees(), cfg(6, 12)),
        (exact_incumbent(), exact_pool(), cfg(3, 6)),
    ] {
        let selection = select(&current, &lever_scan_grid(&current, 4), &pool, &cfg);
        let raw = score_policy_on_pool(&current, &pool, &cfg, SpendCharge::Raw);
        assert_eq!(
            selection.current.charged_probes,
            raw.n + raw.out_of_support_cells
        );
        assert_eq!(selection.current.charged_rounds, raw.rounds);
        for verdict in &selection.candidates {
            if matches!(
                verdict.reason,
                CandidateReason::Identical | CandidateReason::Unmeasurable
            ) {
                continue;
            }
            assert!(verdict.charged_probes >= verdict.n + verdict.out_of_support_cells - 1e-12);
            assert!(verdict.charged_rounds >= verdict.rounds - 1e-12);
        }
    }
}

fn exact_tree(tree_id: &str) -> RecordedTree {
    tree(
        tree_id,
        2,
        &[
            (0, None, 0, 0, 0.3),
            (1, Some(0), 1, 0, 0.5),
            (2, Some(1), 2, 0, 0.9),
            (3, Some(2), 3, 0, 0.8),
            (4, Some(0), 3, 1, 0.4),
        ],
    )
}

fn exact_pool() -> Vec<RecordedTree> {
    vec![exact_tree("exact"), exact_tree("exact2")]
}

fn exact_incumbent() -> ExplorationPolicy {
    policy(|p| {
        p.selection_rule = SelectionRule::BestFirst;
        p.stop_rule = StopRule::FixedRounds;
        p.beta = 3;
        p.batch_size = 2;
    })
}

fn fewer_probes() -> ExplorationPolicy {
    ExplorationPolicy {
        batch_size: 1,
        ..exact_incumbent()
    }
}

fn fewer_rounds() -> ExplorationPolicy {
    ExplorationPolicy {
        beta: 2,
        ..exact_incumbent()
    }
}

#[test]
fn same_best_for_less_needs_a_second_tree_and_then_strictly_wins() {
    let exact_cfg = cfg(3, 6);
    let single = vec![exact_tree("exact")];
    let incumbent = simulate_policy(&single[0], &exact_incumbent(), ReplayConfig { k2: 6 });
    assert_eq!(
        incumbent.revealed_ids,
        ["exact-n0", "exact-n1", "exact-n2", "exact-n3", "exact-n4"]
    );
    assert_eq!(
        (
            incumbent.n,
            incumbent.rounds,
            incumbent.probes_to_best,
            incumbent.rounds_to_best
        ),
        (4, 3, 2, 2)
    );
    let current = score_policy_on_pool(&exact_incumbent(), &single, &exact_cfg, SpendCharge::Raw);
    for candidate in [fewer_probes(), fewer_rounds()] {
        let score = score_policy_on_pool(&candidate, &single, &exact_cfg, SpendCharge::Evidence);
        assert_eq!(
            (
                score.charged_probes,
                score.charged_rounds,
                score.cost,
                score.rounds_saved
            ),
            (6.0, 3.0, 1.0, 0.0)
        );
        assert_close!(score.anytime, 16.0 / 18.0, 12);
        assert_close!(score.value, 0.75 + 0.25 * (16.0 / 18.0) - 0.05, 12);
    }
    assert_close!(
        current.value,
        0.75 + 0.25 * (16.0 / 18.0) - 0.05 * (4.0 / 6.0),
        12
    );
    let selection = select(
        &exact_incumbent(),
        &[fewer_probes(), fewer_rounds()],
        &single,
        &exact_cfg,
    );
    assert!(!selection.improved);
    assert_eq!(selection.current, current);
    assert_eq!(selection.current_min_best, 0.9);
    assert_eq!(
        reasons(&selection.candidates),
        [CandidateReason::Worse, CandidateReason::Worse]
    );
    assert_eq!(
        run_lever_scan(&exact_incumbent(), &single, &exact_cfg).gap,
        0.0
    );
}

#[test]
fn with_a_twin_tree_fewer_probes_and_fewer_rounds_strictly_win() {
    let exact_cfg = cfg(3, 6);
    let pool = exact_pool();
    let current =
        score_policy_on_pool(&exact_incumbent(), &pool, &exact_cfg, SpendCharge::Evidence);
    let probes = score_policy_on_pool(&fewer_probes(), &pool, &exact_cfg, SpendCharge::Evidence);
    assert_eq!((probes.charged_probes, current.charged_probes), (3.0, 4.0));
    assert_close!(
        probes.value - current.value,
        DEFAULT_OBJECTIVE.beta1 / 6.0,
        12
    );
    let rounds = score_policy_on_pool(&fewer_rounds(), &pool, &exact_cfg, SpendCharge::Evidence);
    assert_eq!((rounds.charged_probes, rounds.charged_rounds), (2.0, 2.0));
    assert_close!(
        rounds.value - current.value,
        DEFAULT_OBJECTIVE.beta1 * (2.0 / 6.0) + DEFAULT_OBJECTIVE.beta2 * (1.0 / 3.0),
        12
    );
    let selection = select(
        &exact_incumbent(),
        &[fewer_probes(), fewer_rounds()],
        &pool,
        &exact_cfg,
    );
    assert!(selection.improved && selection.chosen_policy == fewer_rounds());
    assert_eq!(
        reasons(&selection.candidates),
        [CandidateReason::Worse, CandidateReason::Winner]
    );
    let cost_only = with_objective(
        exact_cfg,
        ReplayObjectiveConfig {
            beta1: 0.05,
            beta2: 0.0,
            beta3: 0.0,
        },
    );
    let selection = select(
        &exact_incumbent(),
        &[fewer_probes(), fewer_rounds()],
        &pool,
        &cost_only,
    );
    assert!(selection.improved && selection.chosen_policy == fewer_rounds());
    assert_close!(
        selection.chosen_score - selection.current_score,
        0.05 * (2.0 / 6.0),
        12
    );
}

// --- the measured pool ---------------------------------------------------

fn own() -> RecordedTree {
    tree(
        "own",
        2,
        &[
            (0, None, 0, 0, 0.1),
            (1, Some(0), 1, 0, 0.5),
            (2, Some(1), 2, 0, 0.6),
            (3, Some(2), 3, 0, 0.9),
            (4, Some(0), 3, 1, 0.2),
        ],
    )
}

fn foreign() -> RecordedTree {
    tree(
        "foreign",
        2,
        &[
            (0, None, 0, 0, 0.1),
            (1, Some(0), 1, 0, 0.2),
            (2, Some(0), 2, 1, 0.95),
            (3, Some(0), 3, 2, 0.3),
        ],
    )
}

fn owner() -> ExplorationPolicy {
    policy(|p| {
        p.selection_rule = SelectionRule::BestFirst;
        p.stop_rule = StopRule::Never;
        p.batch_size = 2;
    })
}

fn foreign_friendly() -> ExplorationPolicy {
    policy(|p| {
        p.selection_rule = SelectionRule::ExploreRoot;
        p.stop_rule = StopRule::FixedRounds;
        p.beta = 2;
        p.batch_size = 1;
    })
}

fn patient() -> ExplorationPolicy {
    policy(|p| {
        p.selection_rule = SelectionRule::BestFirst;
        p.stop_rule = StopRule::Patience;
        p.beta = 4;
        p.batch_size = 2;
    })
}

#[test]
fn only_trees_the_incumbent_replays_in_support_are_measured() {
    let measured_cfg = cfg(3, 6);
    let mixed = vec![own(), foreign()];
    let on_foreign = simulate_policy(&mixed[1], &owner(), ReplayConfig { k2: 6 });
    assert_eq!(
        (
            on_foreign.n,
            on_foreign.out_of_support_cells,
            on_foreign.rounds,
            on_foreign.best_score
        ),
        (1, 5, 6, 0.2)
    );
    let measured = measure_pool(&owner(), &mixed, &measured_cfg);
    let ids = |trees: &[&RecordedTree]| {
        trees
            .iter()
            .map(|tree| tree.header.tree_id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&measured.sorted), ["foreign", "own"]);
    assert_eq!(ids(&measured.measured), ["own"]);
    assert_close!(
        measured.current_in_support,
        f64::midpoint(1.0, 1.0 / 6.0),
        12
    );

    let selection = select(&owner(), &[foreign_friendly()], &mixed, &measured_cfg);
    assert_eq!(
        (
            selection.pool_size,
            selection.measured_trees,
            selection.improved
        ),
        (2, 1, false)
    );
    let verdict = &selection.candidates[0];
    assert_eq!(verdict.reason, CandidateReason::QualityRejected);
    assert_close!(verdict.quality, 0.5, 12);
    let own_only = select(&owner(), &[foreign_friendly()], &[own()], &measured_cfg);
    assert_eq!(selection.current_score, own_only.current_score);
    assert_eq!(selection.current, own_only.current);
    assert_eq!(verdict.value, own_only.candidates[0].value);
}

#[test]
fn burning_fewer_dead_rounds_off_support_wins_nothing() {
    let measured_cfg = cfg(3, 6);
    let mixed = vec![own(), foreign()];
    let impatient = ExplorationPolicy {
        beta: 2,
        ..patient()
    };
    let selection = select(&patient(), &[impatient], &mixed, &measured_cfg);
    assert!(!selection.improved);
    let verdict = &selection.candidates[0];
    assert_eq!(
        (
            verdict.reason,
            verdict.rounds,
            verdict.n,
            verdict.charged_probes,
            verdict.rounds_saved
        ),
        (CandidateReason::Worse, 3.0, 4.0, 6.0, 0.0)
    );
    assert_close!(
        selection.current_score - verdict.value,
        DEFAULT_OBJECTIVE.beta1 * (2.0 / 6.0),
        12
    );

    let nothing = select(
        &owner(),
        &[foreign_friendly(), patient()],
        &[foreign()],
        &measured_cfg,
    );
    assert_eq!(
        (
            nothing.measured_trees,
            nothing.current_score,
            nothing.current_min_best
        ),
        (0, 0.0, 0.0)
    );
    assert_eq!(
        reasons(&nothing.candidates),
        [CandidateReason::Unmeasurable, CandidateReason::Unmeasurable]
    );
    assert_eq!((nothing.quality_rejected, nothing.simulations), (0, 1));
}

#[test]
fn the_lever_scan_and_the_simulation_count_use_the_measured_pool() {
    let measured_cfg = cfg(3, 6);
    let mixed = vec![own(), foreign()];
    let mixed_scan = run_lever_scan(&patient(), &mixed, &measured_cfg);
    let own_scan = run_lever_scan(&patient(), &[own()], &measured_cfg);
    assert_eq!(
        (
            mixed_scan.policies,
            mixed_scan.eligible,
            mixed_scan.gap,
            mixed_scan.best_value
        ),
        (
            own_scan.policies,
            own_scan.eligible,
            own_scan.gap,
            own_scan.best_value
        )
    );
    assert_eq!(mixed_scan.simulations, own_scan.simulations + 1);

    let dead_only = ExplorationPolicy {
        branch_width: 7,
        ..owner()
    };
    let selection = select(
        &owner(),
        &[
            owner(),
            foreign_friendly(),
            foreign_friendly(),
            dead_only,
            patient(),
        ],
        &mixed,
        &measured_cfg,
    );
    assert_eq!(
        reasons(&selection.candidates),
        [
            CandidateReason::Identical,
            CandidateReason::QualityRejected,
            CandidateReason::Duplicate,
            CandidateReason::Unmeasurable,
            CandidateReason::Worse,
        ]
    );
    assert_eq!(selection.simulations, 4);
    let scan = run_lever_scan(&owner(), &mixed, &measured_cfg);
    assert_eq!(scan.simulations, 2 + (scan.policies - 1));
}

// --- verdicts --------------------------------------------------------------

#[test]
fn identical_duplicate_and_replay_dead_candidates_are_labelled_and_never_win() {
    let selection = select(&current(), &[current(), better()], &pool(), &CFG);
    let first = &selection.candidates[0];
    assert_eq!(
        (
            first.index,
            first.policy_id.clone(),
            first.origin,
            first.changed.len(),
            first.duplicate_of,
            first.eligible,
            first.reason
        ),
        (
            0,
            policy_id(&current()),
            CandidateOrigin::Local,
            0,
            None,
            false,
            CandidateReason::Identical
        )
    );
    assert_eq!(first.value, selection.current_score);
    assert_eq!(selection.candidates[1].reason, CandidateReason::Winner);
    assert_eq!(selection.scored_count, 2);
    assert_eq!(
        selection.candidate_policy_ids,
        [policy_id(&current()), policy_id(&better())]
    );

    let selection = select(
        &current(),
        &[better(), better(), worse(), worse()],
        &pool(),
        &CFG,
    );
    assert_eq!(selection.candidates[0].reason, CandidateReason::Winner);
    assert_eq!(
        (
            selection.candidates[1].reason,
            selection.candidates[1].duplicate_of
        ),
        (CandidateReason::Duplicate, Some(0))
    );
    assert_eq!(selection.candidates[3].duplicate_of, Some(2));
    assert_eq!(selection.candidates[1].value, selection.candidates[0].value);
    assert_eq!(selection.scored_count, 3);

    let dead_only = ExplorationPolicy {
        branch_width: 7,
        refine_depth: 0,
        recovery_policy: RecoveryPolicy::Abandon,
        ..current()
    };
    let selection = select(&current(), &[dead_only], &pool(), &CFG);
    let verdict = &selection.candidates[0];
    assert_eq!(
        (verdict.reason, verdict.eligible),
        (CandidateReason::Unmeasurable, false)
    );
    assert_eq!(
        verdict.changed,
        ["recoveryPolicy", "branchWidth", "refineDepth"]
    );
    assert!(verdict
        .changed
        .iter()
        .all(|field| REPLAY_DEAD_FIELDS.iter().any(|dead| dead.as_str() == field)));
    assert_eq!(
        (verdict.value, verdict.quality),
        (selection.current_score, selection.current_quality)
    );
    assert_eq!((selection.quality_rejected, selection.scored_count), (0, 2));
}

#[test]
fn off_support_failures_are_unmeasurable_and_in_support_ones_quality_rejected() {
    let in_support_cheap = policy(|p| {
        p.selection_rule = SelectionRule::ExploreRoot;
        p.stop_rule = StopRule::FixedRounds;
        p.beta = 1;
        p.batch_size = 1;
    });
    let selection = select(&better(), &[worse(), in_support_cheap], &pool(), &CFG);
    let (off, cheap) = (&selection.candidates[0], &selection.candidates[1]);
    assert!(off.in_support_min < 1.0 && off.quality < selection.current_quality && !off.eligible);
    assert_eq!(off.reason, CandidateReason::Unmeasurable);
    assert!(
        cheap.in_support_min == 1.0 && cheap.quality < selection.current_quality && !cheap.eligible
    );
    assert_eq!(cheap.reason, CandidateReason::QualityRejected);
    assert_eq!(selection.quality_rejected, 1);
}

#[test]
fn an_equal_value_loser_is_a_tie_and_origins_set_the_dreamer_kind() {
    let exact_cfg = cfg(3, 6);
    let twin = ExplorationPolicy {
        target_score: 999_999.0,
        ..fewer_rounds()
    };
    let mut pair = [fewer_rounds(), twin];
    pair.sort_by_key(policy_id);
    let selection = select_best_policy(
        &exact_incumbent(),
        &[
            CandidateInput {
                policy: pair[1],
                origin: CandidateOrigin::Llm,
            },
            CandidateInput {
                policy: pair[0],
                origin: CandidateOrigin::Local,
            },
        ],
        &exact_pool(),
        &exact_cfg,
        &HashSet::new(),
    );
    assert!(selection.improved);
    assert_eq!(policy_id(&selection.chosen_policy), policy_id(&pair[0]));
    assert_eq!(
        reasons(&selection.candidates),
        [CandidateReason::Tie, CandidateReason::Winner]
    );
    assert_eq!(selection.dreamer, DreamerKind::Mixed);
    let llm = [CandidateInput {
        policy: better(),
        origin: CandidateOrigin::Llm,
    }];
    assert_eq!(dreamer_kind_of(&llm), DreamerKind::Llm);
    assert_eq!(dreamer_kind_of(&local(&[better()])), DreamerKind::Local);
    assert_eq!(dreamer_kind_of(&[]), DreamerKind::Local);

    let selection = select(
        &exact_incumbent(),
        &[fewer_probes()],
        &exact_pool(),
        &exact_cfg,
    );
    let verdict = &selection.candidates[0];
    let score = score_policy_on_pool(
        &fewer_probes(),
        &exact_pool(),
        &exact_cfg,
        SpendCharge::Evidence,
    );
    assert_eq!(
        (
            verdict.value,
            verdict.quality,
            verdict.anytime,
            verdict.cost,
            verdict.rounds_saved
        ),
        (
            score.value,
            score.quality,
            score.anytime,
            score.cost,
            score.rounds_saved
        )
    );
    assert_eq!(
        (
            verdict.n,
            verdict.rounds,
            verdict.out_of_support_cells,
            verdict.in_support_mean,
            verdict.in_support_min
        ),
        (3.0, 3.0, 0.0, 1.0, 1.0)
    );
    assert_eq!(
        (
            verdict.charged_probes,
            verdict.charged_rounds,
            verdict.evidence_trees
        ),
        (3.0, 3.0, 1)
    );
}

#[test]
fn a_revoked_id_is_simulated_but_never_eligible() {
    let exact_cfg = cfg(3, 6);
    let plain = select(
        &exact_incumbent(),
        &[fewer_rounds(), fewer_probes()],
        &exact_pool(),
        &exact_cfg,
    );
    assert_eq!(
        reasons(&plain.candidates),
        [CandidateReason::Winner, CandidateReason::Worse]
    );
    let revoked: HashSet<String> = [policy_id(&fewer_rounds())].into_iter().collect();
    let selection = select_best_policy(
        &exact_incumbent(),
        &local(&[fewer_rounds(), fewer_probes(), fewer_rounds()]),
        &exact_pool(),
        &exact_cfg,
        &revoked,
    );
    assert_eq!(
        reasons(&selection.candidates),
        [
            CandidateReason::Revoked,
            CandidateReason::Winner,
            CandidateReason::Duplicate
        ]
    );
    assert!(!selection.candidates[0].eligible);
    assert_eq!(selection.candidates[0].value, plain.candidates[0].value);
    assert!(selection.improved && selection.chosen_policy == fewer_probes());
    assert_eq!(selection.simulations, plain.simulations);
    let own_id: HashSet<String> = [policy_id(&exact_incumbent())].into_iter().collect();
    let selfish = select_best_policy(
        &exact_incumbent(),
        &local(&[exact_incumbent()]),
        &exact_pool(),
        &exact_cfg,
        &own_id,
    );
    assert_eq!(selfish.candidates[0].reason, CandidateReason::Identical);

    let mut source = Fixed(local(&[fewer_rounds()]));
    let result = run_dreaming(DreamingOptions {
        current: exact_incumbent(),
        pool: &exact_pool(),
        dreams: 1,
        k1: 3,
        k2: 6,
        rng: rng(1),
        objective: DEFAULT_OBJECTIVE,
        quality_eps: 0.0,
        iteration: 0,
        candidates: Some(&mut source),
        lever_scan: false,
        revoked: &revoked,
    });
    assert!(!result.improved);
    assert_eq!(result.chosen_policy_id, policy_id(&exact_incumbent()));
    assert_eq!(result.candidates[0].reason, CandidateReason::Revoked);
    assert_eq!(
        result.current,
        score_policy_on_pool(
            &exact_incumbent(),
            &exact_pool(),
            &exact_cfg,
            SpendCharge::Raw
        )
    );
    assert_eq!(result.current_min_best, 0.9);
}

// --- the lever scan and the local dreamer ------------------------------------

#[test]
fn the_lever_grid_is_fixed_deduplicated_and_changes_only_the_scanned_fields() {
    let grid = lever_scan_grid(&DEFAULT_POLICY, 2);
    let ids: HashSet<String> = grid.iter().map(policy_id).collect();
    assert_eq!(ids.len(), grid.len());
    assert_eq!(
        grid.len(),
        SELECTION_RULES.len() * STOP_RULES.len() * 2 * LEVER_SCAN_BETAS.len() + 1
    );
    assert_eq!(grid[0], DEFAULT_POLICY);
    assert_eq!(
        lever_scan_grid(&DEFAULT_POLICY, 4).len(),
        SELECTION_RULES.len() * STOP_RULES.len() * 4 * LEVER_SCAN_BETAS.len()
    );
    let scanned = [
        PolicyField::SelectionRule,
        PolicyField::StopRule,
        PolicyField::BatchSize,
        PolicyField::Beta,
    ];
    for entry in &grid[1..] {
        assert!(policy_fields_differing(entry, &DEFAULT_POLICY)
            .iter()
            .all(|field| scanned.contains(field)));
    }
    let exact_cfg = cfg(3, 6);
    let first = run_lever_scan(&exact_incumbent(), &exact_pool(), &exact_cfg);
    assert_eq!(
        first,
        run_lever_scan(&exact_incumbent(), &exact_pool(), &exact_cfg)
    );
    assert_eq!(first.policies, lever_scan_grid(&exact_incumbent(), 2).len());
    assert!(first.eligible > 0 && first.gap > 0.0);
    assert_eq!(first.best_policy_id, policy_id(&fewer_rounds()));
    let none = run_lever_scan(&fewer_rounds(), &exact_pool(), &exact_cfg);
    assert_eq!(
        (none.gap, none.best_policy_id),
        (0.0, policy_id(&fewer_rounds()))
    );
}

#[test]
fn the_local_dreamer_is_deterministic_order_independent_and_never_proposes_current() {
    let a: Vec<String> = propose_policies(&current(), 5, &rng(1))
        .iter()
        .map(policy_id)
        .collect();
    let b: Vec<String> = propose_policies(&current(), 5, &rng(1))
        .iter()
        .map(policy_id)
        .collect();
    assert_eq!(a, b);
    let candidates = propose_policies(&current(), 4, &rng(1));
    let standalone = mutate_policy(&current(), &mut rng(1).fork("cand:2"));
    assert_eq!(policy_id(&candidates[2]), policy_id(&standalone));
    for current in [current(), DEFAULT_POLICY, exact_incumbent()] {
        for seed in 0..200 {
            let mutated = mutate_policy(&current, &mut rng(seed));
            assert!(parse_exploration_policy(&mutated.to_value()).is_ok());
            assert_ne!(policy_id(&mutated), policy_id(&current));
            assert_eq!(
                (
                    mutated.branch_width,
                    mutated.refine_depth,
                    mutated.recovery_policy
                ),
                (
                    current.branch_width,
                    current.refine_depth,
                    current.recovery_policy
                )
            );
        }
    }
}

#[test]
fn run_dreaming_is_never_worse_and_routes_injected_candidates_through_the_same_rule() {
    let result = run_dreaming(DreamingOptions {
        current: current(),
        pool: &pool(),
        dreams: 8,
        k1: 5,
        k2: 10,
        rng: rng(7),
        objective: OBJECTIVE,
        quality_eps: 0.0,
        iteration: 0,
        candidates: None,
        lever_scan: true,
        revoked: &HashSet::new(),
    });
    assert_eq!(result.tokens, 0);
    assert!(result.chosen_score >= result.current_score);
    assert!(result.chosen_quality >= result.current_quality - 1e-9);
    assert_eq!(result.candidate_policy_ids.len(), 8);

    let mut injected = Fixed(local(&[better()]));
    let result = run_dreaming(DreamingOptions {
        current: current(),
        pool: &pool(),
        dreams: 1,
        k1: 5,
        k2: 10,
        rng: rng(1),
        objective: OBJECTIVE,
        quality_eps: 0.0,
        iteration: 0,
        candidates: Some(&mut injected),
        lever_scan: true,
        revoked: &HashSet::new(),
    });
    assert!(result.improved);
    assert_eq!(
        (result.chosen_policy_id, result.pool_size),
        (policy_id(&better()), 1)
    );
}

#[test]
fn injected_candidates_carry_their_origins_through_run_dreaming() {
    let mut mixed = Fixed(vec![
        CandidateInput {
            policy: fewer_probes(),
            origin: CandidateOrigin::Llm,
        },
        CandidateInput::from(exact_incumbent()),
    ]);
    let result = run_dreaming(DreamingOptions {
        current: exact_incumbent(),
        pool: &exact_pool(),
        dreams: 2,
        k1: 3,
        k2: 6,
        rng: rng(1),
        objective: OBJECTIVE,
        quality_eps: 0.0,
        iteration: 3,
        candidates: Some(&mut mixed),
        lever_scan: true,
        revoked: &HashSet::new(),
    });
    assert_eq!(result.chosen_policy_id, policy_id(&fewer_probes()));
    assert_eq!(result.dreamer, DreamerKind::Mixed);
    assert_eq!(
        reasons(&result.candidates),
        [CandidateReason::Winner, CandidateReason::Identical]
    );
    assert_eq!(
        result.lever_scan,
        Some(run_lever_scan(
            &exact_incumbent(),
            &exact_pool(),
            &cfg(3, 6)
        ))
    );
    assert_eq!(
        (
            result.simulations,
            result.measured_trees,
            result.evidence_trees
        ),
        (4, 2, 1)
    );

    let mut collapse = Fixed(local(&[collapsed()]));
    let result = run_dreaming(DreamingOptions {
        current: DEFAULT_POLICY,
        pool: &recorded_pool(),
        dreams: 1,
        k1: 12,
        k2: 24,
        rng: rng(1),
        objective: OBJECTIVE,
        quality_eps: 0.0,
        iteration: 0,
        candidates: Some(&mut collapse),
        lever_scan: false,
        revoked: &HashSet::new(),
    });
    assert!(!result.improved && result.lever_scan.is_none());
    assert_eq!(
        (result.chosen_policy_id, result.quality_rejected),
        (policy_id(&DEFAULT_POLICY), 1)
    );
}
