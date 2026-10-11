//! The TS `dream-llm.test.ts` behaviour of `runDreamLoopWithAgent`: the
//! end-to-end loop with a scripted child runner, the detached `dream.run`
//! root, per-round accounting, the fixed control, a shared round 1, priming,
//! the dreams and rejection logs, cancellation and semantic guidance.

#![allow(clippy::float_cmp)]
// The ported TS cases stay whole, one scenario per test.
#![allow(clippy::too_many_lines)]

mod support;

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use pa_dream::child::{ChildRuntimeScope, RunAgentStatus};
use pa_dream::dream_loop::{
    DreamHandlerCalls,
    DreamLoopOptions,
    DreamLoopResult,
    DreamRoundRecord,
    DreamRoundTokens,
    dream_run_id,
    priming_tree_id,
    run_dream_loop,
};
use pa_dream::dreams::{DreamsLogContext, DreamsLogLine, dreams_path, read_dreams_log};
use pa_dream::improve::{CandidateOrigin, CandidateReason, DreamerKind};
use pa_dream::json;
use pa_dream::llm::{
    DREAMER_PROMPT_HEADER,
    DreamChildRole,
    GUIDANCE_PROMPT_HEADER,
    GuidanceInsights,
    LlmProposer,
    LlmProposerOptions,
    PROPOSER_PROMPT_HEADER,
    SemanticGuidance,
};
use pa_dream::llm_loop::{
    DreamInitialRollout,
    DreamLoopWithAgentOptions,
    DreamPhase,
    DreamProgressEvent,
    merge_primed_rollouts,
    run_dream_loop_with_agent,
};
use pa_dream::objective::DEFAULT_OBJECTIVE;
use pa_dream::policy::{DEFAULT_POLICY, ExplorationPolicy, PRIMING_DIVERSE, StopRule, policy_id};
use pa_dream::proposer::{ProposalRejectReason, ProposalTally};
use pa_dream::rejections::{RejectionRole, read_rejections, rejections_path};
use pa_dream::rng::{Seed, SeededRng};
use pa_dream::rollout::{ExploreOptions, run_online_exploration};
use pa_dream::store::{list_trees, read_tree};
use pa_dream::task::{ArtifactShapeError, DynTask, Evaluation, ProposeParams, ScoredTask};
use pa_dream::tasks::autocorrelation::Autocorrelation;
use pa_dream::tasks::{DreamTaskId, resolve_task, task_prompt_context};
use serde_json::{Value, json};
use support::spans::{SpanRecord, capture, named};
use support::stub::{Answer, DEFAULT_INSIGHTS, Stub};
use tokio_util::sync::CancellationToken;

const FIXED_CLOCK: u64 = 1_700_000_000_000;
const ARTIFACT: &str = r#"{"set":[0,1,2,4,9]}"#;
const GUIDANCE_PREFIX: &str = "Directional insights from prior trajectories";

fn fixed_clock() -> u64 {
    FIXED_CLOCK
}

fn never() -> ExplorationPolicy {
    support::policy(|p| p.stop_rule = StopRule::Never)
}

fn revised() -> String {
    json::stringify(&json!([never()]))
}

fn sum_difference() -> Arc<dyn DynTask> {
    resolve_task(DreamTaskId::SumDifference, None).expect("task")
}

/// The TS `agentOptions` defaults: sum-difference, seed 7, W 3, k1 5, k2 10,
/// M 4, two iterations, both toggles on.
fn options<'a>(
    runner: &'a Stub,
    task: &'a dyn DynTask,
    dir: &'a Path,
) -> DreamLoopWithAgentOptions<'a> {
    DreamLoopWithAgentOptions {
        runner,
        task,
        task_id: "sum-difference".to_string(),
        n: None,
        seed: Seed::Number(7),
        clock: &fixed_clock,
        workers: 3,
        k1: 5,
        k2: 10,
        dreams: 4,
        iterations: 2,
        dir,
        objective: DEFAULT_OBJECTIVE,
        rng: None,
        initial_policy: DEFAULT_POLICY,
        fixed_policy: false,
        initial_rollout: None,
        semantic_guidance: None,
        use_llm_proposer: true,
        use_llm_dreamer: true,
        scope: ChildRuntimeScope {
            max_turns: Some(2),
            token_budget: Some(500_000),
            ..ChildRuntimeScope::default()
        },
        cancel: CancellationToken::new(),
        proposer_prompt_context: None,
        child_token_budget: Some(500_000),
        on_progress: None,
        run_label: None,
        dreams_log_context: DreamsLogContext::default(),
        priming_policies: Vec::new(),
        trigger_trace_id: None,
    }
}

fn tree_files(dir: &Path, tree_id: &str) -> Vec<(String, String)> {
    let mut files = vec![(
        "tree.jsonl".to_string(),
        std::fs::read_to_string(dir.join("trees").join(format!("{tree_id}.jsonl"))).unwrap(),
    )];
    let blobs = dir.join("trees").join(tree_id).join("blobs");
    let mut names: Vec<String> = std::fs::read_dir(&blobs)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    for name in names {
        files.push((
            name.clone(),
            std::fs::read_to_string(blobs.join(name)).unwrap(),
        ));
    }
    files
}

fn sum_rounds(rounds: &[DreamRoundRecord]) -> (u64, DreamHandlerCalls) {
    let mut calls = DreamHandlerCalls::default();
    let mut tokens = 0;
    for round in rounds {
        tokens += round.tokens.rollout + round.tokens.dreamer + round.tokens.guidance;
        calls.proposer += round.handler_calls.proposer;
        calls.dreamer += round.handler_calls.dreamer;
        calls.guidance += round.handler_calls.guidance;
    }
    (tokens, calls)
}

fn role_calls(stub: &Stub) -> DreamHandlerCalls {
    DreamHandlerCalls {
        proposer: stub.calls(DreamChildRole::Proposer),
        dreamer: stub.calls(DreamChildRole::Dreamer),
        guidance: stub.calls(DreamChildRole::Guidance),
    }
}

fn the<'a>(spans: &'a [SpanRecord], name: &str) -> &'a SpanRecord {
    named(spans, name)
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("no {name} span"))
}

#[test]
fn the_whole_loop_never_regresses_and_runs_under_a_detached_root_carrying_the_trigger() {
    let stub = Stub::new()
        .proposer(|_| Answer::ok(ARTIFACT, 100))
        .dreamer(|_| Answer::ok(&revised(), 200));
    let task = sum_difference();
    let dir = tempfile::tempdir().unwrap();
    let (result, spans) = capture(|| {
        let turn = tracing::info_span!("test.turn");
        let _turn = turn.enter();
        run_dream_loop_with_agent(DreamLoopWithAgentOptions {
            trigger_trace_id: Some("0af7651916cd43dd8448eb211c80319c".to_string()),
            ..options(&stub, task.as_ref(), dir.path())
        })
        .expect("loop")
    });
    assert_eq!(result.tree_ids.len(), 3);
    assert!(result.final_policy_score >= result.initial_policy_score);
    assert_eq!(result.tokens, stub.total_tokens());
    assert!(result.tokens > 0);
    let run = the(&spans, "dream.run");
    assert_eq!(run.parent, None, "dream.run is a detached root");
    assert_eq!(
        run.attr("trigger.trace_id"),
        Some(&json!("0af7651916cd43dd8448eb211c80319c"))
    );
    assert_eq!(run.attr("dream.mode"), Some(&json!("llm")));
    // Every other span descends from dream.run.
    let ancestor = |span: &SpanRecord| {
        let mut parent = span.parent;
        while let Some(id) = parent {
            if id == run.id {
                return true;
            }
            parent = spans
                .iter()
                .find(|other| other.id == id)
                .and_then(|other| other.parent);
        }
        false
    };
    for span in spans
        .iter()
        .filter(|span| !matches!(span.name, "dream.run" | "test.turn"))
    {
        assert!(ancestor(span), "{} must descend from dream.run", span.name);
    }
    assert!(!named(&spans, "dream.llm_propose").is_empty());
    assert!(!named(&spans, "dream.llm_dream").is_empty());
}

#[test]
fn each_toggle_runs_independently_and_deterministically() {
    let task = sum_difference();
    let proposer_only = || {
        let stub = Stub::new().proposer(|_| Answer::ok(ARTIFACT, 100));
        let dir = tempfile::tempdir().unwrap();
        run_dream_loop_with_agent(DreamLoopWithAgentOptions {
            use_llm_dreamer: false,
            ..options(&stub, task.as_ref(), dir.path())
        })
        .unwrap()
    };
    let (a, b) = (proposer_only(), proposer_only());
    assert_eq!(
        (&b.final_policy_id, &b.tree_ids, b.tokens),
        (&a.final_policy_id, &a.tree_ids, a.tokens)
    );
    assert!(a.tokens > 0);
    let dreamer_only = || {
        let stub = Stub::new().dreamer(|_| Answer::ok(&revised(), 200));
        let dir = tempfile::tempdir().unwrap();
        run_dream_loop_with_agent(DreamLoopWithAgentOptions {
            use_llm_proposer: false,
            ..options(&stub, task.as_ref(), dir.path())
        })
        .unwrap()
    };
    let (c, d) = (dreamer_only(), dreamer_only());
    assert_eq!(
        (&d.final_policy_id, &d.tree_ids, d.tokens),
        (&c.final_policy_id, &c.tree_ids, c.tokens)
    );
    assert_eq!(c.tokens, 200 * u64::from(c.iterations));
}

fn local_sync(
    dir: &Path,
    priming: Vec<ExplorationPolicy>,
    clock: &dyn Fn() -> u64,
) -> DreamLoopResult {
    let task = sum_difference();
    run_dream_loop(DreamLoopOptions {
        task: task.as_ref(),
        task_id: "sum-difference".to_string(),
        n: None,
        seed: Seed::Number(7),
        clock,
        workers: 3,
        k1: 5,
        k2: 10,
        dreams: 4,
        iterations: 2,
        dir,
        objective: DEFAULT_OBJECTIVE,
        rng: None,
        initial_policy: DEFAULT_POLICY,
        fixed_policy: false,
        candidates: None,
        run_label: None,
        dreams_log_context: DreamsLogContext::default(),
        priming_policies: priming,
    })
    .unwrap()
}

#[test]
fn the_local_path_is_the_local_loop_byte_for_byte_with_priming_charged_to_round_1() {
    let sync_dir = tempfile::tempdir().unwrap();
    let expected = local_sync(sync_dir.path(), PRIMING_DIVERSE.to_vec(), &fixed_clock);
    let stub = Stub::new();
    let task = sum_difference();
    let dir = tempfile::tempdir().unwrap();
    let (result, spans) = capture(|| {
        run_dream_loop_with_agent(DreamLoopWithAgentOptions {
            use_llm_proposer: false,
            use_llm_dreamer: false,
            priming_policies: PRIMING_DIVERSE.to_vec(),
            ..options(&stub, task.as_ref(), dir.path())
        })
        .unwrap()
    });
    assert_eq!(stub.total_calls(), 0);
    assert_eq!(result.run_id, expected.run_id);
    assert_eq!(result.tree_ids, expected.tree_ids);
    assert_eq!(
        json::stringify(&result.rounds),
        json::stringify(&expected.rounds)
    );
    assert_eq!(result.stopped_early, expected.stopped_early);
    assert_eq!(
        json::stringify(&result.final_selection),
        json::stringify(&expected.final_selection)
    );
    for tree_id in &expected.tree_ids {
        assert_eq!(
            tree_files(dir.path(), tree_id),
            tree_files(sync_dir.path(), tree_id)
        );
    }
    let priming_ids: Vec<String> = (0..PRIMING_DIVERSE.len())
        .map(|index| priming_tree_id("sum-difference", &Seed::Number(7), index, FIXED_CLOCK))
        .collect();
    assert_eq!(
        result.rounds[0].priming_tree_ids.as_ref(),
        Some(&priming_ids)
    );
    for tree_id in &priming_ids {
        assert_eq!(
            tree_files(dir.path(), tree_id),
            tree_files(sync_dir.path(), tree_id)
        );
    }
    assert_eq!(list_trees(dir.path()).len(), 3 + PRIMING_DIVERSE.len());
    assert_eq!(result.rounds[1].pool_size, 1 + PRIMING_DIVERSE.len());
    assert_eq!(
        the(&spans, "dream.run").attr("dream.priming_policies"),
        Some(&json!(2))
    );
    assert!(
        result
            .rounds
            .iter()
            .all(|round| round.tokens == DreamRoundTokens::default()
                && round.handler_calls == DreamHandlerCalls::default()
                && round.proposals == ProposalTally::default()
                && round.agent_generated_calls == 0)
    );
    assert!(!dir.path().join("rejections").exists());
    // The fixed control charges the same priming without freezing a pool.
    let fixed_dir = tempfile::tempdir().unwrap();
    let fixed = run_dream_loop_with_agent(DreamLoopWithAgentOptions {
        use_llm_proposer: false,
        use_llm_dreamer: false,
        fixed_policy: true,
        priming_policies: PRIMING_DIVERSE.to_vec(),
        ..options(&stub, task.as_ref(), fixed_dir.path())
    })
    .unwrap();
    assert_eq!(fixed.rounds[0], result.rounds[0]);
    assert_eq!(fixed.rounds[1].pool_size, 1 + PRIMING_DIVERSE.len());
}

#[test]
fn two_clocks_change_only_the_clock_bearing_ids_on_the_local_path() {
    let stub = Stub::new();
    let task = sum_difference();
    let run = |clock: &'static (dyn Fn() -> u64 + Sync)| {
        let dir = tempfile::tempdir().unwrap();
        run_dream_loop_with_agent(DreamLoopWithAgentOptions {
            clock,
            use_llm_proposer: false,
            use_llm_dreamer: false,
            ..options(&stub, task.as_ref(), dir.path())
        })
        .unwrap()
    };
    let a = run(&|| 1_789_842_143_996);
    let b = run(&|| 1);
    assert_ne!(a.tree_ids, b.tree_ids);
    let clock_free = |result: &DreamLoopResult| {
        result
            .rounds
            .iter()
            .map(|round| DreamRoundRecord {
                tree_id: String::new(),
                ..round.clone()
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(clock_free(&a), clock_free(&b));
    assert_eq!(
        (&b.final_policy_id, b.final_policy_score, b.best_node_score),
        (&a.final_policy_id, a.final_policy_score, a.best_node_score)
    );
}

/// The TS scripted task: trees 0, 1 and 3+ find 1.0 in round 1 and 0.5 later;
/// tree 2 (the probation rollout) finds 0.2 in round 1, then 0.9.
struct Scripted {
    trees: AtomicUsize,
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

    fn propose(&self, _: Option<&f64>, _: &ProposeParams, _: &mut SeededRng, round: u32) -> f64 {
        let probation = self.trees.load(Ordering::SeqCst) == 3;
        match (probation, round == 1) {
            (true, true) => 0.2,
            (true, false) => 0.9,
            (false, true) => 1.0,
            (false, false) => 0.5,
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

#[test]
fn a_dreamed_winner_on_probation_is_reverted_and_the_next_dreamer_is_told() {
    let collapse = support::policy(|p| {
        p.stop_rule = StopRule::FixedRounds;
        p.beta = 1;
    });
    let output = json::stringify(&json!([collapse]));
    let stub = Stub::new().dreamer(move |_| Answer::ok(&output, 50));
    let task = Scripted {
        trees: AtomicUsize::new(0),
    };
    let dir = tempfile::tempdir().unwrap();
    let (result, spans) = capture(|| {
        run_dream_loop_with_agent(DreamLoopWithAgentOptions {
            workers: 3,
            k1: 4,
            k2: 8,
            dreams: 1,
            iterations: 3,
            use_llm_proposer: false,
            ..options(&stub, &task, dir.path())
        })
        .unwrap()
    });
    let initial = policy_id(&DEFAULT_POLICY);
    let collapse_id = policy_id(&collapse);
    assert_eq!(
        result
            .rounds
            .iter()
            .map(|round| round.policy_id.clone())
            .collect::<Vec<_>>(),
        vec![
            initial.clone(),
            initial.clone(),
            collapse_id.clone(),
            initial.clone()
        ]
    );
    let verdict = |index: usize| {
        result.rounds[index]
            .dreaming
            .as_ref()
            .unwrap()
            .candidate_verdicts[0]
            .reason
    };
    assert_eq!(
        (verdict(1), verdict(2), verdict(3)),
        (
            CandidateReason::Worse,
            CandidateReason::Winner,
            CandidateReason::Revoked
        )
    );
    let probation = result.rounds[2]
        .dreaming
        .as_ref()
        .unwrap()
        .probation
        .clone()
        .unwrap();
    assert_eq!(
        (
            probation.policy_id.as_str(),
            probation.incumbent_policy_id.as_str(),
            probation.round_best,
            probation.floor,
            probation.charged_probes,
            probation.charged_rounds,
            probation.incumbent_charged_probes,
            probation.incumbent_charged_rounds,
            probation.evidence_trees,
            probation.reverted
        ),
        (
            collapse_id.as_str(),
            initial.as_str(),
            0.2,
            1.0,
            1.0,
            1.0,
            6.0,
            4.0,
            1,
            true
        )
    );
    assert!(
        result.rounds[3]
            .dreaming
            .as_ref()
            .unwrap()
            .probation
            .is_none()
    );
    assert_eq!(result.probation_reverts, 1);
    assert_eq!(result.final_policy_id, initial);
    assert_eq!(
        result
            .final_selection
            .iter()
            .map(|verdict| verdict.reason)
            .collect::<Vec<_>>(),
        vec![
            CandidateReason::Identical,
            CandidateReason::Revoked,
            CandidateReason::Identical
        ]
    );
    let prompts = stub.prompts(DreamChildRole::Dreamer);
    assert_eq!(prompts.len(), 3);
    assert!(prompts[2].contains(&format!(
        "iteration 2: {collapse_id} (llm; changed stopRule, beta)"
    )));
    let lines: Vec<&str> = prompts[2].lines().collect();
    let at = lines
        .iter()
        .position(|line| line.contains(&format!("iteration 2: {collapse_id} ")))
        .unwrap();
    assert!(lines[at].ends_with(": winner"), "{}", lines[at]);
    assert!(lines[at + 1].contains(&format!("iteration 2: {collapse_id} ")));
    assert!(lines[at + 1].ends_with(": revoked"), "{}", lines[at + 1]);
    assert!(!prompts[1].contains("revoked\n"));
    let probations: Vec<_> = read_dreams_log(&dreams_path(dir.path(), &result.run_id))
        .unwrap()
        .into_iter()
        .filter_map(|line| match line {
            DreamsLogLine::Probation { iteration, line } => Some((iteration, line)),
            _ => None,
        })
        .collect();
    assert_eq!(probations.len(), 1);
    assert_eq!(
        (
            probations[0].0,
            &probations[0].1["policyId"],
            &probations[0].1["reverted"]
        ),
        (2, &json!(collapse_id), &json!(true))
    );
    let redeploys = named(&spans, "dream.redeploy");
    assert_eq!(
        redeploys
            .iter()
            .map(|span| span.attr("dream.probation").cloned())
            .collect::<Vec<_>>(),
        vec![Some(json!(false)), Some(json!(true)), Some(json!(false))]
    );
    assert_eq!(redeploys[1].attr("dream.reverted"), Some(&json!(true)));
    assert_eq!(
        redeploys[1].attr("dream.probation_floor"),
        Some(&json!(1.0))
    );
}

#[test]
fn an_aborted_child_stops_the_run_and_marks_the_root_aborted() {
    let stub = Stub::new().proposer(|_| Answer::status(RunAgentStatus::Aborted, 10));
    let task = sum_difference();
    let dir = tempfile::tempdir().unwrap();
    let (result, spans) =
        capture(|| run_dream_loop_with_agent(options(&stub, task.as_ref(), dir.path())));
    assert!(result.err().is_some_and(|error| error.is_abort()));
    assert_eq!(
        the(&spans, "dream.run").attr("dream.stopped"),
        Some(&json!("aborted"))
    );
    assert!(!named(&spans, "dream.llm_propose").is_empty());
}

#[test]
fn a_pre_cancelled_run_stops_before_any_rollout_even_on_the_local_path() {
    let stub = Stub::new();
    let task = sum_difference();
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let (result, spans) = capture(|| {
        run_dream_loop_with_agent(DreamLoopWithAgentOptions {
            use_llm_proposer: false,
            use_llm_dreamer: false,
            cancel,
            ..options(&stub, task.as_ref(), dir.path())
        })
    });
    assert!(result.err().is_some_and(|error| error.is_abort()));
    assert_eq!(
        the(&spans, "dream.run").attr("dream.stopped"),
        Some(&json!("aborted"))
    );
    assert!(!dir.path().join("trees").exists());
}

#[test]
fn the_fixed_control_never_dreams_and_shares_iteration_0_with_a_dreaming_run() {
    let stub = Stub::new()
        .proposer(|_| Answer::ok(ARTIFACT, 10))
        .dreamer(|_| Answer::ok(&revised(), 100));
    let task = sum_difference();
    let fixed_dir = tempfile::tempdir().unwrap();
    let events = Mutex::new(Vec::new());
    let mut push = |event: DreamProgressEvent| events.lock().unwrap().push(event);
    let (fixed, spans) = capture(|| {
        run_dream_loop_with_agent(DreamLoopWithAgentOptions {
            fixed_policy: true,
            on_progress: Some(&mut push),
            ..options(&stub, task.as_ref(), fixed_dir.path())
        })
        .unwrap()
    });
    assert!(fixed.fixed_policy);
    assert_eq!((fixed.rounds.len(), fixed.tree_ids.len()), (3, 3));
    assert_eq!(fixed.final_policy_id, policy_id(&DEFAULT_POLICY));
    assert!(!fixed.improved);
    assert_eq!(fixed.final_policy_score, fixed.initial_policy_score);
    assert!(fixed.rounds.iter().all(|round| round.dreaming.is_none()
        && round.policy_id == fixed.initial_policy_id
        && round.handler_calls.dreamer == 0
        && round.tokens.dreamer == 0));
    assert_eq!(
        fixed
            .rounds
            .iter()
            .map(|round| (round.iteration, round.pool_size))
            .collect::<Vec<_>>(),
        vec![(0, 0), (1, 1), (2, 2)]
    );
    assert_eq!(stub.calls(DreamChildRole::Dreamer), 0);
    let events = events.into_inner().unwrap();
    assert!(!events.iter().any(|event| matches!(
        event,
        DreamProgressEvent::Phase {
            phase: DreamPhase::Dreaming,
            ..
        }
    )));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                DreamProgressEvent::Phase {
                    phase: DreamPhase::Redeploying,
                    ..
                }
            ))
            .count(),
        2
    );
    assert_eq!(
        the(&spans, "dream.run").attr("dream.fixed_policy"),
        Some(&json!(true))
    );
    for name in ["dream.llm_dream", "dream.dream", "dream.replay"] {
        assert!(named(&spans, name).is_empty(), "{name}");
    }
    let redeploys = named(&spans, "dream.redeploy");
    assert_eq!(redeploys.len(), 2);
    assert!(
        redeploys
            .iter()
            .all(|span| span.attr("dream.fixed_policy") == Some(&json!(true))
                && span.attr("dream.policy_id") == Some(&json!(fixed.initial_policy_id)))
    );

    let dreaming_stub = Stub::new()
        .proposer(|_| Answer::ok(ARTIFACT, 10))
        .dreamer(|_| Answer::ok(&revised(), 100));
    let dreaming_dir = tempfile::tempdir().unwrap();
    let dreaming =
        run_dream_loop_with_agent(options(&dreaming_stub, task.as_ref(), dreaming_dir.path()))
            .unwrap();
    assert_eq!(dreaming.tree_ids, fixed.tree_ids);
    assert_eq!(
        tree_files(dreaming_dir.path(), &dreaming.tree_ids[0]),
        tree_files(fixed_dir.path(), &fixed.tree_ids[0])
    );
    assert_eq!(dreaming.rounds[0], fixed.rounds[0]);
    assert!(dreaming.rounds[1].dreaming.is_some());
    assert_eq!(dreaming.rounds[1].handler_calls.dreamer, 1);
    assert_eq!(dreaming.rounds[1].tokens.dreamer, 100);
    assert_eq!(fixed.rounds[1].handler_calls.dreamer, 0);
}

#[test]
fn per_role_tokens_and_calls_sum_to_the_run_totals_and_the_initial_policy_heads_every_tree() {
    let custom = support::policy(|p| {
        p.batch_size = 2;
        p.beta = 3;
        p.stop_rule = StopRule::FixedRounds;
    });
    let stub = Stub::new()
        .proposer(|_| Answer::ok(ARTIFACT, 7))
        .dreamer(|_| Answer::ok(&revised(), 300));
    let task = sum_difference();
    let dir = tempfile::tempdir().unwrap();
    let result = run_dream_loop_with_agent(DreamLoopWithAgentOptions {
        initial_policy: custom,
        ..options(&stub, task.as_ref(), dir.path())
    })
    .unwrap();
    assert_eq!(result.initial_policy_id, policy_id(&custom));
    assert_eq!(result.rounds[0].policy_id, policy_id(&custom));
    for tree in list_trees(dir.path()) {
        let round = result
            .rounds
            .iter()
            .find(|round| round.tree_id == tree.tree_id)
            .unwrap();
        assert_eq!(round.probes as usize, tree.node_count - 1);
        assert_eq!(round.round_best, tree.best_score);
        assert_eq!(round.policy_id, tree.policy_id);
    }
    let (tokens, calls) = sum_rounds(&result.rounds);
    assert_eq!(tokens, result.tokens);
    assert_eq!(result.tokens, stub.total_tokens());
    assert_eq!(calls, role_calls(&stub));
    assert_eq!(
        result
            .rounds
            .iter()
            .map(|round| round.tokens.dreamer)
            .collect::<Vec<_>>(),
        vec![0, 300, 300]
    );
    assert!(
        result
            .rounds
            .iter()
            .all(|round| round.tokens.rollout == u64::from(round.probes) * 7
                && round.tokens.guidance == 0
                && round.handler_calls.guidance == 0)
    );
}

#[test]
fn every_child_result_is_counted_per_round_and_rejections_land_under_the_run_key() {
    let good = r#"{"n": 4, "weights": [3, 1, 1, 3]}"#;
    let bad = r#"{"n": 4, "weights": [1, 2]}"#;
    let stub =
        Stub::new().proposer(move |call| Answer::ok(if call % 3 == 1 { good } else { bad }, 10));
    let task: Arc<dyn DynTask> = Arc::new(Autocorrelation::new(4));
    let dir = tempfile::tempdir().unwrap();
    let result = run_dream_loop_with_agent(DreamLoopWithAgentOptions {
        task_id: "autocorrelation".to_string(),
        n: Some(4),
        use_llm_dreamer: false,
        proposer_prompt_context: task_prompt_context(DreamTaskId::Autocorrelation, Some(4)),
        ..options(&stub, task.as_ref(), dir.path())
    })
    .unwrap();
    assert_eq!(result.rounds.len(), 3);
    let mut rejected_total = 0;
    for round in &result.rounds {
        let tally = round.proposals;
        let rejected = tally.llm_rejected.total();
        assert_eq!(u64::from(round.agent_generated_calls), tally.llm_accepted);
        assert_eq!(tally.llm_proposals, tally.llm_accepted + rejected);
        assert_eq!(
            u64::from(round.probes),
            tally.llm_accepted + tally.local_fallbacks
        );
        assert_eq!(tally.llm_proposals, round.handler_calls.proposer);
        assert_eq!(
            tally.llm_rejected.get(ProposalRejectReason::Shape),
            rejected
        );
        assert!(tally.local_fallbacks > 0 && tally.llm_accepted > 0);
        rejected_total += rejected;
        let tree = read_tree(&round.tree_id, dir.path()).unwrap();
        let count = |origin: &str| {
            tree.nodes
                .iter()
                .filter(|node| node.origin.as_deref() == Some(origin))
                .count() as u64
        };
        assert_eq!(count("llm"), tally.llm_accepted);
        assert_eq!(count("local"), tally.local_fallbacks);
        assert_eq!(count("root"), 1);
    }
    let logged = read_rejections(&rejections_path(
        dir.path(),
        &format!("autocorrelation-s7-r{FIXED_CLOCK}"),
    ))
    .unwrap();
    assert_eq!(logged.len() as u64, rejected_total);
    assert_eq!(
        logged.iter().filter(|line| line.input.fell_back).count() as u64,
        result
            .rounds
            .iter()
            .map(|round| round.proposals.local_fallbacks)
            .sum::<u64>()
    );
    assert!(
        logged
            .iter()
            .all(|line| line.input.reason == ProposalRejectReason::Shape
                && line.input.excerpt == bad)
    );
    let mut iterations: Vec<u32> = logged.iter().map(|line| line.input.iteration).collect();
    iterations.sort_unstable();
    iterations.dedup();
    assert_eq!(iterations, vec![0, 1, 2]);
}

#[test]
fn a_retried_handler_invocation_is_two_proposer_calls_for_one_probe() {
    let stub = Stub::new().proposer(|call| {
        if call % 2 == 1 {
            Answer::status(RunAgentStatus::Error, 3)
        } else {
            Answer::ok(ARTIFACT, 5)
        }
    });
    let task = sum_difference();
    let dir = tempfile::tempdir().unwrap();
    let result = run_dream_loop_with_agent(DreamLoopWithAgentOptions {
        iterations: 0,
        k1: 1,
        workers: 1,
        use_llm_dreamer: false,
        ..options(&stub, task.as_ref(), dir.path())
    })
    .unwrap();
    assert_eq!(result.rounds.len(), 1);
    assert_eq!(result.rounds[0].probes, 1);
    assert_eq!(
        result.rounds[0].handler_calls,
        DreamHandlerCalls {
            proposer: 2,
            dreamer: 0,
            guidance: 0
        }
    );
    assert_eq!((result.rounds[0].tokens.rollout, result.tokens), (8, 8));
}

#[test]
fn a_shared_initial_rollout_is_adopted_as_round_1_and_a_missing_one_is_refused() {
    let task = sum_difference();
    let dir = tempfile::tempdir().unwrap();
    let seed_stub = Stub::new().proposer(|_| Answer::ok(ARTIFACT, 10));
    let scope = ChildRuntimeScope::default();
    let cancel = CancellationToken::new();
    let mut proposer = LlmProposer::new(
        &seed_stub,
        task.as_ref(),
        LlmProposerOptions {
            scope: &scope,
            cancel: &cancel,
            token_budget: 1,
            prompt_context: None,
            guidance: None,
            rejections: None,
            iteration: 0,
        },
    );
    let shared = run_online_exploration(ExploreOptions {
        task: task.as_ref(),
        task_id: "sum-difference".to_string(),
        n: None,
        seed: Seed::Number(7),
        rng: SeededRng::new(&Seed::Number(7)).fork("iter:0"),
        clock: &fixed_clock,
        workers: 3,
        k1: 5,
        dir: dir.path(),
        policy: DEFAULT_POLICY,
        iteration: 0,
        proposer: Some(&mut proposer),
        tree_id: None,
        cancel: None,
    })
    .unwrap();
    let initial = DreamInitialRollout {
        proposals: proposer.tally,
        handler_calls: DreamHandlerCalls {
            proposer: seed_stub.calls(DreamChildRole::Proposer),
            dreamer: 0,
            guidance: 0,
        },
        ..merge_primed_rollouts(&shared, &[])
    };
    assert_eq!(shared.agent_generated_count, shared.revealed_count);
    assert_eq!(
        proposer.tally.llm_accepted,
        u64::from(shared.revealed_count)
    );
    let stub = Stub::new()
        .proposer(|_| Answer::ok(ARTIFACT, 10))
        .dreamer(|_| Answer::ok(&revised(), 100));
    let mut events = Vec::new();
    let mut push = |event: DreamProgressEvent| events.push(event);
    let result = run_dream_loop_with_agent(DreamLoopWithAgentOptions {
        initial_rollout: Some(initial.clone()),
        on_progress: Some(&mut push),
        ..options(&stub, task.as_ref(), dir.path())
    })
    .unwrap();
    assert_eq!(result.tree_ids[0], shared.tree_id);
    assert_eq!(result.tree_ids.len(), 3);
    assert_eq!(
        result.rounds[0],
        DreamRoundRecord {
            iteration: 0,
            tree_id: shared.tree_id.clone(),
            policy_id: policy_id(&DEFAULT_POLICY),
            round_best: shared.best_score,
            probes: shared.revealed_count,
            agent_generated_calls: shared.revealed_count,
            proposals: proposer.tally,
            decision_rounds: shared.rounds,
            pool_size: 0,
            tokens: DreamRoundTokens {
                rollout: shared.tokens,
                dreamer: 0,
                guidance: 0
            },
            handler_calls: initial.handler_calls,
            dreaming: None,
            probes_to_round_best: shared.probes_to_best,
            improvements: shared.improvements.clone(),
            priming_tree_ids: None,
            priming_probes: None,
        }
    );
    assert_eq!(
        result.stopped_early as usize,
        result
            .rounds
            .iter()
            .filter(|round| round.decision_rounds < 5)
            .count()
    );
    assert_eq!(
        result.rounds[1].proposals.llm_accepted,
        result.rounds[1].handler_calls.proposer
    );
    assert_eq!(
        result.rounds[1].agent_generated_calls,
        result.rounds[1].probes
    );
    assert_eq!(
        stub.calls(DreamChildRole::Proposer),
        result.rounds[1].handler_calls.proposer + result.rounds[2].handler_calls.proposer
    );
    assert_eq!(result.tokens, shared.tokens + stub.total_tokens());
    assert!(matches!(
        events.first(),
        Some(DreamProgressEvent::Phase { phase: DreamPhase::Rollout, tree_id: Some(id), .. }) if *id == shared.tree_id
    ));
    assert_eq!(result.rounds[1].pool_size, 1);
    assert_eq!(list_trees(dir.path()).len(), 3);

    let missing = Stub::new().proposer(|_| Answer::ok(ARTIFACT, 10));
    let empty = tempfile::tempdir().unwrap();
    let refused = run_dream_loop_with_agent(DreamLoopWithAgentOptions {
        initial_rollout: Some(DreamInitialRollout {
            tree_id: "nope".to_string(),
            ..initial
        }),
        ..options(&missing, task.as_ref(), empty.path())
    });
    assert!(refused.err().is_some_and(|error| {
        error
            .to_string()
            .contains("shared initial rollout nope is not in the store")
    }));
    assert_eq!(missing.total_calls(), 0);
}

#[test]
fn the_dreams_log_verdicts_and_spans_carry_every_step_and_the_dreamer_sees_its_history() {
    let stub = Stub::new()
        .proposer(|_| Answer::ok(ARTIFACT, 10))
        .dreamer(|_| Answer::ok(&revised(), 20));
    let task = sum_difference();
    let dir = tempfile::tempdir().unwrap();
    let (result, spans) =
        capture(|| run_dream_loop_with_agent(options(&stub, task.as_ref(), dir.path())).unwrap());
    assert_eq!(result.run_id, format!("sum-difference-s7-r{FIXED_CLOCK}"));
    assert_eq!(result.final_selection.len(), result.iterations as usize);
    for round in &result.rounds[1..] {
        let dreaming = round.dreaming.as_ref().unwrap();
        assert_eq!(dreaming.candidates, 4);
        assert_eq!(
            dreaming
                .candidate_verdicts
                .iter()
                .map(|verdict| verdict.origin)
                .collect::<Vec<_>>(),
            vec![
                CandidateOrigin::Llm,
                CandidateOrigin::Local,
                CandidateOrigin::Local,
                CandidateOrigin::Local
            ]
        );
        assert_eq!(
            dreaming.candidate_verdicts[0].policy_id,
            policy_id(&never())
        );
        assert_eq!(dreaming.dreamer, DreamerKind::Mixed);
        assert!(dreaming.lever_scan.as_ref().unwrap().policies > 1);
        assert!(round.probes_to_round_best <= round.probes);
        assert_eq!(round.improvements.last().unwrap().score, round.round_best);
    }
    let lines = read_dreams_log(&dreams_path(dir.path(), &result.run_id)).unwrap();
    let steps: Vec<(i64, Value, Value)> = lines
        .iter()
        .filter_map(|line| match line {
            DreamsLogLine::Step { iteration, line } => Some((
                *iteration,
                line["dreamer"].clone(),
                line["poolSize"].clone(),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        steps,
        vec![
            (1, json!("mixed"), json!(1)),
            (2, json!("mixed"), json!(2)),
            (-1, json!("local"), json!(3))
        ]
    );
    let run = the(&spans, "dream.run");
    assert_eq!(run.attr("dream.run_id"), Some(&json!(result.run_id)));
    assert_eq!(run.attr("dream.priming_policies"), Some(&json!(0)));
    let iterations = |name: &str| {
        named(&spans, name)
            .iter()
            .map(|span| span.attr("dream.iteration").cloned())
            .collect::<Vec<_>>()
    };
    for name in ["dream.llm_dream", "dream.dream", "dream.replay"] {
        assert_eq!(
            iterations(name),
            vec![Some(json!(1)), Some(json!(2))],
            "{name}"
        );
    }
    let dream_ids: Vec<u64> = named(&spans, "dream.dream")
        .iter()
        .map(|span| span.id)
        .collect();
    let candidates = named(&spans, "dream.candidate");
    assert_eq!(candidates.len(), 8);
    assert!(
        candidates
            .iter()
            .all(|span| span.parent.is_some_and(|id| dream_ids.contains(&id)))
    );
    assert_eq!(
        candidates
            .iter()
            .map(|span| span.attr("dream.origin").cloned().unwrap())
            .collect::<Vec<_>>(),
        [
            "llm", "local", "local", "local", "llm", "local", "local", "local"
        ]
        .map(|origin| json!(origin))
    );
    let prompts = stub.prompts(DreamChildRole::Dreamer);
    assert_eq!(prompts.len(), 2);
    assert_ne!(prompts[0], prompts[1]);
    assert!(!prompts[0].contains("Earlier candidates"));
    assert!(prompts[1].contains("Earlier candidates"));
    assert!(prompts[1].contains(&format!("- iteration 1: {} (llm;", policy_id(&never()))));
    assert!(prompts[0].contains(&format!("- {}: N ", result.tree_ids[0])));
    assert!(prompts[1].contains(&format!("- {}: N ", result.tree_ids[1])));
}

#[test]
fn a_run_label_keys_the_logs_and_the_child_scope_lands_on_the_root() {
    let bad = r#"{"n": 4, "weights": [1, 2]}"#;
    let stub = Stub::new()
        .proposer(move |call| {
            Answer::ok(
                if call % 2 == 1 {
                    r#"{"n": 4, "weights": [3, 1, 1, 3]}"#
                } else {
                    bad
                },
                1,
            )
        })
        .dreamer(|_| Answer::ok("no array here", 2));
    let task: Arc<dyn DynTask> = Arc::new(Autocorrelation::new(4));
    let dir = tempfile::tempdir().unwrap();
    let (result, spans) = capture(|| {
        run_dream_loop_with_agent(DreamLoopWithAgentOptions {
            task_id: "autocorrelation".to_string(),
            n: Some(4),
            iterations: 1,
            run_label: Some("exp 1/dream".to_string()),
            dreams_log_context: DreamsLogContext {
                experiment_id: Some("exp 1".to_string()),
                arm: Some("dream".to_string()),
            },
            scope: ChildRuntimeScope {
                model: Some("faux/child".to_string()),
                thinking_level: Some("off".to_string()),
                max_output_tokens: Some(4096),
                max_turns: Some(2),
                token_budget: Some(500_000),
            },
            ..options(&stub, task.as_ref(), dir.path())
        })
        .unwrap()
    });
    let run_id = dream_run_id(
        "autocorrelation",
        &Seed::Number(7),
        FIXED_CLOCK,
        Some("exp 1/dream"),
    );
    assert_eq!(
        run_id,
        format!("autocorrelation-s7-r{FIXED_CLOCK}-exp_1_dream")
    );
    assert_eq!(result.run_id, run_id);
    let rejections = read_rejections(&rejections_path(dir.path(), &run_id)).unwrap();
    assert!(
        rejections
            .iter()
            .any(|line| line.input.role.is_none()
                && line.input.reason == ProposalRejectReason::Shape)
    );
    assert_eq!(
        rejections
            .iter()
            .filter(|line| line.input.role == Some(RejectionRole::Dreamer))
            .map(|line| (
                line.input.iteration,
                line.input.attempt,
                line.input.reason,
                line.input.fell_back
            ))
            .collect::<Vec<_>>(),
        vec![
            (1, 1, ProposalRejectReason::Parse, false),
            (1, 2, ProposalRejectReason::Parse, true)
        ]
    );
    let dreaming = result.rounds[1].dreaming.as_ref().unwrap();
    assert_eq!(dreaming.dreamer, DreamerKind::Local);
    assert!(
        dreaming
            .candidate_verdicts
            .iter()
            .all(|verdict| verdict.origin == CandidateOrigin::Local)
    );
    let lines = read_dreams_log(&dreams_path(dir.path(), &run_id)).unwrap();
    assert!(!lines.is_empty());
    let text = std::fs::read_to_string(dreams_path(dir.path(), &run_id)).unwrap();
    assert!(
        text.lines()
            .all(|line| line.contains(r#""experimentId":"exp 1","arm":"dream""#))
    );
    let run = the(&spans, "dream.run");
    for (key, value) in [
        ("dream.run_id", json!(run_id)),
        ("dream.child_model", json!("faux/child")),
        ("dream.child_thinking", json!("off")),
        ("dream.child_max_output_tokens", json!(4096)),
    ] {
        assert_eq!(run.attr(key), Some(&value), "{key}");
    }
    let dream = the(&spans, "dream.llm_dream");
    for (key, value) in [
        ("dream.llm_fallback", json!(true)),
        ("dream.llm_reject_reason", json!("parse")),
        ("dream.llm_status", json!("completed")),
        ("dream.llm_reject_excerpt", json!("no array here")),
    ] {
        assert_eq!(dream.attr(key), Some(&value), "{key}");
    }
}

#[test]
fn merging_primed_rollouts_sums_probes_tokens_and_agent_calls_and_merges_the_curve() {
    let task = sum_difference();
    let dir = tempfile::tempdir().unwrap();
    let stub = Stub::new().proposer(|_| Answer::ok(ARTIFACT, 5));
    let scope = ChildRuntimeScope::default();
    let cancel = CancellationToken::new();
    let explore = |policy: ExplorationPolicy, fork: &str, tree_id: Option<&str>| {
        let mut proposer = LlmProposer::new(
            &stub,
            task.as_ref(),
            LlmProposerOptions {
                scope: &scope,
                cancel: &cancel,
                token_budget: 1,
                prompt_context: None,
                guidance: None,
                rejections: None,
                iteration: 0,
            },
        );
        run_online_exploration(ExploreOptions {
            task: task.as_ref(),
            task_id: "sum-difference".to_string(),
            n: None,
            seed: Seed::Number(3),
            rng: SeededRng::new(&Seed::Number(3)).fork(fork),
            clock: &fixed_clock,
            workers: 3,
            k1: 4,
            dir: dir.path(),
            policy,
            iteration: 0,
            proposer: Some(&mut proposer),
            tree_id: tree_id.map(str::to_string),
            cancel: None,
        })
        .unwrap()
    };
    let initial = explore(DEFAULT_POLICY, "iter:0", None);
    let primed = vec![
        explore(PRIMING_DIVERSE[0], "prime:0", Some("p0")),
        explore(PRIMING_DIVERSE[1], "prime:1", Some("p1")),
    ];
    let merged = merge_primed_rollouts(&initial, &primed);
    let priming_probes = primed[0].revealed_count + primed[1].revealed_count;
    assert_eq!(merged.tree_id, initial.tree_id);
    assert_eq!(merged.revealed_count, initial.revealed_count);
    assert_eq!(
        merged.priming_tree_ids,
        Some(vec!["p0".to_string(), "p1".to_string()])
    );
    assert_eq!(merged.priming_probes, Some(priming_probes));
    assert_eq!(
        merged.tokens,
        initial.tokens + primed[0].tokens + primed[1].tokens
    );
    assert_eq!(
        merged.agent_generated_count,
        initial.revealed_count + priming_probes
    );
    assert_eq!(
        merged.best_score,
        initial
            .best_score
            .max(primed[0].best_score)
            .max(primed[1].best_score)
    );
    assert_eq!(merged.rounds, initial.rounds);
    assert_eq!(merged.improvements.last().unwrap().score, merged.best_score);
    assert_eq!(
        merged.probes_to_best,
        merged.improvements.last().unwrap().probe
    );
    assert!(merged.probes_to_best <= initial.revealed_count + priming_probes);
    let alone = merge_primed_rollouts(&initial, &[]);
    assert_eq!(alone.improvements, initial.improvements);
    assert_eq!(alone.probes_to_best, initial.probes_to_best);
    assert_eq!((alone.priming_tree_ids, alone.priming_probes), (None, None));
}

#[test]
fn semantic_guidance_without_the_llm_proposer_is_refused_before_anything_runs() {
    let stub = Stub::new();
    let task = sum_difference();
    let dir = tempfile::tempdir().unwrap();
    let (result, spans) = capture(|| {
        run_dream_loop_with_agent(DreamLoopWithAgentOptions {
            use_llm_proposer: false,
            semantic_guidance: Some(SemanticGuidance::Child),
            ..options(&stub, task.as_ref(), dir.path())
        })
    });
    assert_eq!(
        result.err().map(|error| error.to_string()),
        Some("semanticGuidance requires useLlmProposer".to_string())
    );
    assert_eq!(stub.total_calls(), 0);
    assert!(named(&spans, "dream.run").is_empty());
    assert!(!dir.path().join("trees").exists());
}

#[test]
fn injected_insights_reach_every_later_prompt_after_the_header_and_leave_iteration_0_untouched() {
    let task = sum_difference();
    let plain = Stub::new()
        .proposer(|_| Answer::ok(ARTIFACT, 10))
        .dreamer(|_| Answer::ok(&revised(), 100));
    let plain_dir = tempfile::tempdir().unwrap();
    let plain_run =
        run_dream_loop_with_agent(options(&plain, task.as_ref(), plain_dir.path())).unwrap();
    let inputs = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&inputs);
    let guided = Stub::new()
        .proposer(|_| Answer::ok(ARTIFACT, 10))
        .dreamer(|_| Answer::ok(&revised(), 100));
    let guided_dir = tempfile::tempdir().unwrap();
    let guided_run = run_dream_loop_with_agent(DreamLoopWithAgentOptions {
        semantic_guidance: Some(SemanticGuidance::Injected(Box::new(move |input| {
            seen.lock()
                .unwrap()
                .push((input.iteration, input.pool_size, input.task_id.clone()));
            GuidanceInsights {
                text: format!("Iteration {}: {DEFAULT_INSIGHTS}", input.iteration),
                tokens: 0,
            }
        }))),
        ..options(&guided, task.as_ref(), guided_dir.path())
    })
    .unwrap();
    let first = usize::try_from(plain_run.rounds[0].handler_calls.proposer).unwrap();
    assert!(first > 0);
    assert_eq!(
        guided_run.rounds[0].handler_calls.proposer,
        plain_run.rounds[0].handler_calls.proposer
    );
    let guided_prompts = guided.prompts(DreamChildRole::Proposer);
    let plain_prompts = plain.prompts(DreamChildRole::Proposer);
    assert_eq!(guided_prompts[..first], plain_prompts[..first]);
    assert!(
        !plain_prompts
            .iter()
            .any(|prompt| prompt.contains(GUIDANCE_PREFIX))
    );
    let later = &guided_prompts[first..];
    assert!(!later.is_empty());
    for prompt in later {
        let guidance = prompt.find(GUIDANCE_PREFIX).unwrap();
        let body = prompt.find("Improve the candidate below").unwrap();
        assert!(prompt.starts_with(PROPOSER_PROMPT_HEADER) && guidance < body);
        assert!(prompt.contains(DEFAULT_INSIGHTS));
    }
    assert!(later.iter().any(|prompt| prompt.contains("Iteration 1:")));
    assert!(later.iter().any(|prompt| prompt.contains("Iteration 2:")));
    assert_eq!(guided.calls(DreamChildRole::Guidance), 0);
    assert!(
        guided_run
            .rounds
            .iter()
            .all(|round| round.tokens.guidance == 0 && round.handler_calls.guidance == 0)
    );
    assert_eq!(
        *inputs.lock().unwrap(),
        vec![
            (1, 1, "sum-difference".to_string()),
            (2, 2, "sum-difference".to_string())
        ]
    );
    assert_eq!(guided_run.tree_ids[0], plain_run.tree_ids[0]);
}

#[test]
fn one_guidance_child_per_iteration_is_spanned_and_accounted() {
    let insights = json::stringify(&json!({ "insights": DEFAULT_INSIGHTS }));
    let stub = Stub::new()
        .proposer(|_| Answer::ok(ARTIFACT, 10))
        .dreamer(|_| Answer::ok(&revised(), 20))
        .guidance(move |_| Answer::ok(&insights, 77));
    let task = sum_difference();
    let dir = tempfile::tempdir().unwrap();
    let (result, spans) = capture(|| {
        run_dream_loop_with_agent(DreamLoopWithAgentOptions {
            semantic_guidance: Some(SemanticGuidance::Child),
            ..options(&stub, task.as_ref(), dir.path())
        })
        .unwrap()
    });
    assert_eq!(stub.calls(DreamChildRole::Guidance), 2);
    let prompts = stub.prompts(DreamChildRole::Guidance);
    assert!(
        prompts.iter().all(
            |prompt| prompt.starts_with(GUIDANCE_PROMPT_HEADER) && prompt.contains("\"trees\"")
        )
    );
    assert_eq!(
        result
            .rounds
            .iter()
            .map(|round| (round.handler_calls.guidance, round.tokens.guidance))
            .collect::<Vec<_>>(),
        vec![(0, 0), (1, 77), (1, 77)]
    );
    assert_eq!(result.tokens, stub.total_tokens());
    let guidance = named(&spans, "dream.llm_guidance");
    assert_eq!(
        guidance
            .iter()
            .map(|span| (
                span.attr("dream.iteration").cloned(),
                span.attr("dream.pool_size").cloned(),
                span.attr("dream.llm_fallback").cloned(),
                span.attr("dream.tokens").cloned()
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                Some(json!(1)),
                Some(json!(1)),
                Some(json!(false)),
                Some(json!(77))
            ),
            (
                Some(json!(2)),
                Some(json!(2)),
                Some(json!(false)),
                Some(json!(77))
            ),
        ]
    );
    let run = the(&spans, "dream.run");
    assert!(guidance.iter().all(|span| span.parent == Some(run.id)));
    let first = usize::try_from(result.rounds[0].handler_calls.proposer).unwrap();
    let proposer = stub.prompts(DreamChildRole::Proposer);
    assert!(
        proposer[first..]
            .iter()
            .all(|prompt| prompt.contains(DEFAULT_INSIGHTS))
    );
    assert!(
        !proposer[..first]
            .iter()
            .any(|prompt| prompt.contains(GUIDANCE_PREFIX))
    );
}

#[test]
fn a_failing_writer_falls_back_to_empty_guidance_and_an_aborted_one_aborts_the_run() {
    let failing = Stub::new()
        .proposer(|_| Answer::ok(ARTIFACT, 10))
        .dreamer(|_| Answer::ok(&revised(), 100))
        .guidance(|_| Answer::status(RunAgentStatus::Error, 4));
    let task = sum_difference();
    let dir = tempfile::tempdir().unwrap();
    let (result, spans) = capture(|| {
        run_dream_loop_with_agent(DreamLoopWithAgentOptions {
            semantic_guidance: Some(SemanticGuidance::Child),
            ..options(&failing, task.as_ref(), dir.path())
        })
        .unwrap()
    });
    assert_eq!(
        result
            .rounds
            .iter()
            .map(|round| (round.handler_calls.guidance, round.tokens.guidance))
            .collect::<Vec<_>>(),
        vec![(0, 0), (2, 8), (2, 8)]
    );
    assert!(
        !failing
            .prompts(DreamChildRole::Proposer)
            .iter()
            .any(|prompt| prompt.contains(GUIDANCE_PREFIX))
    );
    let guidance = named(&spans, "dream.llm_guidance");
    assert_eq!(guidance.len(), 2);
    assert!(
        guidance
            .iter()
            .all(|span| span.attr("dream.llm_fallback") == Some(&json!(true))
                && span.attr("dream.tokens") == Some(&json!(8)))
    );

    let aborting = Stub::new()
        .proposer(|_| Answer::ok(ARTIFACT, 10))
        .guidance(|_| Answer::status(RunAgentStatus::Aborted, 1));
    let abort_dir = tempfile::tempdir().unwrap();
    let (aborted, abort_spans) = capture(|| {
        run_dream_loop_with_agent(DreamLoopWithAgentOptions {
            semantic_guidance: Some(SemanticGuidance::Child),
            ..options(&aborting, task.as_ref(), abort_dir.path())
        })
    });
    assert!(aborted.err().is_some_and(|error| error.is_abort()));
    assert_eq!(aborting.calls(DreamChildRole::Guidance), 1);
    assert_eq!(aborting.calls(DreamChildRole::Dreamer), 0);
    assert_eq!(
        the(&abort_spans, "dream.run").attr("dream.stopped"),
        Some(&json!("aborted"))
    );
}

#[test]
fn every_child_prompt_starts_with_its_role_header() {
    let stub = Stub::new()
        .proposer(|_| Answer::ok(ARTIFACT, 1))
        .dreamer(|_| Answer::ok(&revised(), 1));
    let task = sum_difference();
    let dir = tempfile::tempdir().unwrap();
    run_dream_loop_with_agent(DreamLoopWithAgentOptions {
        iterations: 1,
        semantic_guidance: Some(SemanticGuidance::Child),
        ..options(&stub, task.as_ref(), dir.path())
    })
    .unwrap();
    assert!(!stub.prompts(DreamChildRole::Proposer).is_empty());
    assert_eq!(stub.prompts(DreamChildRole::Dreamer).len(), 1);
    assert_eq!(stub.prompts(DreamChildRole::Guidance).len(), 1);
    assert!(stub.prompts(DreamChildRole::Proposer).iter().all(|prompt| {
        prompt
            .lines()
            .next()
            .unwrap()
            .starts_with(PROPOSER_PROMPT_HEADER)
    }));
    assert_eq!(
        stub.prompts(DreamChildRole::Dreamer)[0].lines().next(),
        Some(DREAMER_PROMPT_HEADER)
    );
    assert!(stub.prompts(DreamChildRole::Guidance)[0].starts_with(GUIDANCE_PROMPT_HEADER));
    let headers = [
        PROPOSER_PROMPT_HEADER,
        DREAMER_PROMPT_HEADER,
        GUIDANCE_PROMPT_HEADER,
    ];
    for a in headers {
        for b in headers {
            assert!(a == b || !a.starts_with(b));
        }
    }
}
