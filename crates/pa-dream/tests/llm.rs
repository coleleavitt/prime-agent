//! The TS `dream-llm.test.ts` behaviour for the LLM proposer, the LLM dreamer,
//! their prompts and the guidance digest, with a scripted child runner.

// Exact float equality is the claim: replay and the objective are deterministic.
#![allow(clippy::float_cmp)]
// The ported TS cases stay whole, one scenario per test.
#![allow(clippy::too_many_lines)]

mod support;

use std::sync::Arc;

use pa_dream::child::{ChildRuntimeScope, RunAgentStatus};
use pa_dream::improve::{
    CandidateInput,
    CandidateOrigin,
    CandidateReason,
    CandidateSource,
    DreamerKind,
    DreamingOptions,
    propose_policies,
    run_dreaming,
};
use pa_dream::interpreter::project_propose_params;
use pa_dream::json;
use pa_dream::llm::{
    DREAMER_PROMPT_HEADER,
    DreamChildRole,
    DreamerContext,
    DroppedCandidate,
    LlmDreamerOptions,
    LlmProposer,
    LlmProposerOptions,
    PROPOSER_JSON_ONLY,
    PROPOSER_PROMPT_HEADER,
    build_dream_prompt,
    build_dreamer_input,
    build_guidance_input,
    history_of,
    parse_candidate_array,
    propose_policies_with_agent,
};
use pa_dream::objective::ReplayObjectiveConfig;
use pa_dream::policy::{
    DEFAULT_POLICY,
    ExplorationPolicy,
    REPLAY_DEAD_FIELDS,
    SelectionRule,
    StopRule,
    policy_id,
};
use pa_dream::proposer::{
    LocalProposer,
    ProposalRejectReason,
    ProposalTally,
    ProposeOutcome,
    Proposer,
};
use pa_dream::records::{NodeRecord, TreeHeaderRecord};
use pa_dream::rejections::{
    ProposalRejection,
    REJECTION_EXCERPT_CHARS,
    RejectionInput,
    RejectionLog,
    RejectionRole,
    excerpt_of,
    read_rejections,
    rejections_path,
};
use pa_dream::rng::{Seed, SeededRng};
use pa_dream::rollout::{ExploreOptions, run_online_exploration};
use pa_dream::store::{DreamStoreError, RecordedTree, list_trees, read_tree};
use pa_dream::task::DynTask;
use pa_dream::tasks::autocorrelation::Autocorrelation;
use pa_dream::tasks::{DreamTaskId, resolve_task, task_prompt_context};
use serde_json::{Value, json};
use support::spans::{capture, named};
use support::stub::{Answer, Stub};
use tokio_util::sync::CancellationToken;

const FIXED_CLOCK: u64 = 1_700_000_000_000;

fn scope() -> ChildRuntimeScope {
    ChildRuntimeScope {
        model: None,
        max_turns: Some(2),
        token_budget: Some(500_000),
        thinking_level: None,
        max_output_tokens: None,
    }
}

fn sum_difference() -> Arc<dyn DynTask> {
    resolve_task(DreamTaskId::SumDifference, None).expect("task")
}

fn n4_task() -> Arc<dyn DynTask> {
    Arc::new(Autocorrelation::new(4))
}

fn n4_parent(task: &dyn DynTask) -> pa_dream::task::Artifact {
    task.deserialize(&json!({"n": 4, "weights": [2, 2, 2, 2]}))
        .expect("parent")
}

fn rng(seed: i64) -> SeededRng {
    SeededRng::new(&Seed::Number(seed))
}

fn policy(over: impl FnOnce(&mut ExplorationPolicy)) -> ExplorationPolicy {
    support::policy(over)
}

struct Proposed {
    artifact: Value,
    tokens: u64,
    origin: Option<CandidateOrigin>,
}

#[allow(clippy::needless_pass_by_value)] // consumes the outcome the proposer returned
fn outcome(task: &dyn DynTask, outcome: ProposeOutcome) -> Proposed {
    Proposed {
        artifact: task.serialize(&outcome.artifact),
        tokens: outcome.tokens,
        origin: outcome.origin,
    }
}

fn options<'a>(
    scope: &'a ChildRuntimeScope,
    cancel: &'a CancellationToken,
) -> LlmProposerOptions<'a> {
    LlmProposerOptions {
        scope,
        cancel,
        token_budget: 200_000,
        prompt_context: None,
        guidance: None,
        rejections: None,
        iteration: 0,
    }
}

fn local_propose(
    task: &dyn DynTask,
    parent: Option<&pa_dream::task::Artifact>,
    seed: i64,
    round: u32,
) -> Value {
    let params = project_propose_params(&DEFAULT_POLICY);
    let outcome = LocalProposer::new(task)
        .propose(parent, &params, &mut rng(seed), round)
        .expect("local");
    task.serialize(&outcome.artifact)
}

#[test]
fn the_proposer_returns_the_deserialized_child_artifact_and_its_tokens() {
    let task = sum_difference();
    let stub = Stub::new().proposer(|_| Answer::ok(r#"{"set":[0,1,2,4,9,15]}"#, 1234));
    let (scope, cancel) = (scope(), CancellationToken::new());
    let mut proposer = LlmProposer::new(&stub, task.as_ref(), options(&scope, &cancel));
    let params = project_propose_params(&DEFAULT_POLICY);
    let result = outcome(
        task.as_ref(),
        proposer
            .propose(None, &params, &mut rng(1), 1)
            .expect("propose"),
    );
    assert_eq!(result.tokens, 1234);
    assert_eq!(result.artifact, json!({"set": [0, 1, 2, 4, 9, 15]}));
    assert_eq!(result.origin, Some(CandidateOrigin::Llm));
    assert_eq!(stub.total_tokens(), 1234);
}

#[test]
fn an_invalid_child_output_is_retried_once_then_the_local_proposer_stands_in() {
    let task = sum_difference();
    let stub = Stub::new().proposer(|_| Answer::ok(r#"{"notASet":true}"#, 40));
    let (scope, cancel) = (scope(), CancellationToken::new());
    let mut proposer = LlmProposer::new(&stub, task.as_ref(), options(&scope, &cancel));
    let parent = task.deserialize(&json!({"set": [0, 1, 2, 3]})).unwrap();
    let params = project_propose_params(&DEFAULT_POLICY);
    let result = outcome(
        task.as_ref(),
        proposer
            .propose(Some(&parent), &params, &mut rng(7), 2)
            .expect("propose"),
    );
    assert_eq!(result.tokens, 80);
    assert_eq!(result.origin, Some(CandidateOrigin::Local));
    assert_eq!(
        result.artifact,
        local_propose(task.as_ref(), Some(&parent), 7, 2)
    );
}

#[test]
fn an_aborted_child_aborts_the_attempt_without_a_fallback() {
    let task = sum_difference();
    let stub = Stub::new().proposer(|_| Answer::status(RunAgentStatus::Aborted, 10));
    let (scope, cancel) = (scope(), CancellationToken::new());
    let mut proposer = LlmProposer::new(&stub, task.as_ref(), options(&scope, &cancel));
    let params = project_propose_params(&DEFAULT_POLICY);
    let error = proposer
        .propose(None, &params, &mut rng(1), 1)
        .err()
        .expect("aborted");
    assert!(error.is_abort(), "{error}");
    let mut expected = ProposalTally::default();
    expected.reject(ProposalRejectReason::Aborted, false);
    assert_eq!(proposer.tally, expected);
    assert_eq!(proposer.tally.local_fallbacks, 0);
}

#[test]
fn an_object_wrapped_in_prose_and_fences_is_accepted_as_the_agents_work() {
    let task = n4_task();
    let wrapped = "Here is my improved candidate. I moved mass from the middle bins [1, 2] toward the edges:\n```json\n{\"n\": 4, \"weights\": [3, 1, 1, 3]}\n```\nThis lowers the central peak {see the hint about a flat top}.";
    let stub = Stub::new().proposer(move |_| Answer {
        output_tokens: 120,
        ..Answer::ok(wrapped, 300)
    });
    let (scope, cancel) = (scope(), CancellationToken::new());
    let mut proposer = LlmProposer::new(
        &stub,
        task.as_ref(),
        LlmProposerOptions {
            prompt_context: task_prompt_context(DreamTaskId::Autocorrelation, Some(4)),
            ..options(&scope, &cancel)
        },
    );
    let params = project_propose_params(&DEFAULT_POLICY);
    let parent = n4_parent(task.as_ref());
    let (result, spans) = capture(|| {
        outcome(
            task.as_ref(),
            proposer
                .propose(Some(&parent), &params, &mut rng(1), 1)
                .unwrap(),
        )
    });
    assert_eq!(result.origin, Some(CandidateOrigin::Llm));
    assert_eq!(result.tokens, 300);
    assert_eq!(json::stringify(&result.artifact["weights"]), "[3,1,1,3]");
    assert_eq!(stub.total_calls(), 1);
    let mut expected = ProposalTally::default();
    expected.accept();
    assert_eq!(proposer.tally, expected);
    let span = named(&spans, "dream.llm_propose")[0];
    assert_eq!(span.attr("dream.llm_fallback"), Some(&json!(false)));
    assert_eq!(span.attr("dream.origin"), Some(&json!("llm")));
    assert_eq!(span.attr("dream.llm_attempts"), Some(&json!(1)));
    assert_eq!(span.attr("dream.llm_output_tokens"), Some(&json!(120)));
    assert_eq!(span.attr("dream.llm_reject_reason"), None);
}

#[test]
fn a_wrong_shape_is_retried_once_then_falls_back_and_both_rejections_are_logged() {
    let short = r#"{"n": 4, "weights": [1, 2, 3]}"#;
    let task = n4_task();
    let stub = Stub::new().proposer(move |_| Answer {
        output_tokens: 15,
        ..Answer::ok(short, 40)
    });
    let dir = tempfile::tempdir().unwrap();
    let path = rejections_path(dir.path(), "unit");
    let clock = || FIXED_CLOCK;
    let log = RejectionLog::new(path.clone(), &clock);
    let (scope, cancel) = (scope(), CancellationToken::new());
    let mut proposer = LlmProposer::new(
        &stub,
        task.as_ref(),
        LlmProposerOptions {
            prompt_context: task_prompt_context(DreamTaskId::Autocorrelation, Some(4)),
            rejections: Some(&log),
            iteration: 3,
            ..options(&scope, &cancel)
        },
    );
    let parent = n4_parent(task.as_ref());
    let params = project_propose_params(&DEFAULT_POLICY);
    let (result, spans) = capture(|| {
        outcome(
            task.as_ref(),
            proposer
                .propose(Some(&parent), &params, &mut rng(7), 2)
                .unwrap(),
        )
    });
    assert_eq!(stub.total_calls(), 2);
    assert_eq!(result.origin, Some(CandidateOrigin::Local));
    assert_eq!(result.tokens, 80);
    assert_eq!(
        result.artifact,
        local_propose(task.as_ref(), Some(&parent), 7, 2)
    );
    let mut expected = ProposalTally::default();
    expected.reject(ProposalRejectReason::Shape, false);
    expected.reject(ProposalRejectReason::Shape, true);
    assert_eq!(proposer.tally, expected);
    let span = named(&spans, "dream.llm_propose")[0];
    for (key, value) in [
        ("dream.llm_fallback", json!(true)),
        ("dream.origin", json!("local")),
        ("dream.llm_reject_reason", json!("shape")),
        ("dream.llm_status", json!("completed")),
        ("dream.llm_attempts", json!(2)),
        ("dream.llm_output_tokens", json!(30)),
        ("dream.llm_reject_excerpt", json!(short)),
    ] {
        assert_eq!(span.attr(key), Some(&value), "{key}");
    }
    let line = |attempt, fell_back| ProposalRejection {
        ts: FIXED_CLOCK,
        input: RejectionInput {
            role: None,
            iteration: 3,
            round: 2,
            attempt,
            reason: ProposalRejectReason::Shape,
            status: RunAgentStatus::Completed,
            fell_back,
            tokens: 40,
            output_tokens: 15,
            stop_reason: None,
            error: Some(
                "autocorrelation artifact must have a weights array of length 4".to_string(),
            ),
            excerpt: short.to_string(),
        },
    };
    assert_eq!(
        read_rejections(&path).unwrap(),
        vec![line(1, false), line(2, true)]
    );
}

#[test]
fn an_output_cut_at_the_cap_is_a_length_rejection_and_is_not_retried() {
    let runaway = format!(
        "Let me reason about this carefully. {}{{\"n\": 4, \"weights\": [1, 2,",
        "The peak is the central knot. ".repeat(40)
    );
    let task = n4_task();
    let output = runaway.clone();
    let stub = Stub::new().proposer(move |_| Answer {
        output_tokens: 32_000,
        stop_reason: Some("length"),
        ..Answer::ok(&output, 32_500)
    });
    let (scope, cancel) = (scope(), CancellationToken::new());
    let mut proposer = LlmProposer::new(&stub, task.as_ref(), options(&scope, &cancel));
    let parent = n4_parent(task.as_ref());
    let params = project_propose_params(&DEFAULT_POLICY);
    let (result, spans) = capture(|| {
        outcome(
            task.as_ref(),
            proposer
                .propose(Some(&parent), &params, &mut rng(3), 1)
                .unwrap(),
        )
    });
    assert_eq!(stub.total_calls(), 1);
    assert_eq!(result.origin, Some(CandidateOrigin::Local));
    assert_eq!(result.tokens, 32_500);
    let mut expected = ProposalTally::default();
    expected.reject(ProposalRejectReason::Length, true);
    assert_eq!(proposer.tally, expected);
    let span = named(&spans, "dream.llm_propose")[0];
    assert_eq!(span.attr("dream.llm_reject_reason"), Some(&json!("length")));
    assert_eq!(span.attr("dream.llm_output_tokens"), Some(&json!(32_000)));
    let excerpt = span
        .attr("dream.llm_reject_excerpt")
        .unwrap()
        .as_str()
        .unwrap();
    assert_eq!(excerpt, excerpt_of(&runaway, REJECTION_EXCERPT_CHARS));
    assert!(excerpt.chars().count() <= 240);
    assert!(excerpt.starts_with("Let me reason") && excerpt.ends_with("[1, 2,"));
    assert!(excerpt.contains(" ... "));
}

#[test]
fn the_largest_object_wins_and_prose_without_one_is_a_parse_rejection() {
    let task = n4_task();
    let output = "Compared with {\"n\": 4} the shape below is flatter:\n{\"n\": 4, \"weights\": [2.5, 1.5, 1.5, 2.5]}\nDone.";
    let stub = Stub::new().proposer(move |_| Answer::ok(output, 50));
    let (scope, cancel) = (scope(), CancellationToken::new());
    let parent = n4_parent(task.as_ref());
    let params = project_propose_params(&DEFAULT_POLICY);
    let mut proposer = LlmProposer::new(&stub, task.as_ref(), options(&scope, &cancel));
    let result = outcome(
        task.as_ref(),
        proposer
            .propose(Some(&parent), &params, &mut rng(3), 1)
            .unwrap(),
    );
    assert_eq!(result.origin, Some(CandidateOrigin::Llm));
    assert_eq!(
        json::stringify(&result.artifact["weights"]),
        "[2.5,1.5,1.5,2.5]"
    );

    let prose = Stub::new().proposer(|_| Answer::ok("I cannot improve on the uniform density.", 5));
    let mut proposer = LlmProposer::new(&prose, task.as_ref(), options(&scope, &cancel));
    let fell = outcome(
        task.as_ref(),
        proposer
            .propose(Some(&parent), &params, &mut rng(3), 1)
            .unwrap(),
    );
    assert_eq!(fell.origin, Some(CandidateOrigin::Local));
    assert_eq!(prose.total_calls(), 2);
    assert_eq!(
        proposer.tally.llm_rejected.get(ProposalRejectReason::Parse),
        2
    );
    assert_eq!(proposer.tally.local_fallbacks, 1);
}

#[test]
fn a_child_error_turn_limit_and_budget_map_to_their_reasons_and_only_the_error_is_retried() {
    let task = n4_task();
    let parent = n4_parent(task.as_ref());
    let params = project_propose_params(&DEFAULT_POLICY);
    let (scope, cancel) = (scope(), CancellationToken::new());
    for (status, reason, calls) in [
        (RunAgentStatus::Error, ProposalRejectReason::Error, 2),
        (
            RunAgentStatus::TurnLimit,
            ProposalRejectReason::TurnLimit,
            1,
        ),
        (
            RunAgentStatus::BudgetExceeded,
            ProposalRejectReason::Budget,
            1,
        ),
    ] {
        let stub = Stub::new().proposer(move |_| Answer::status(status, 3));
        let mut proposer = LlmProposer::new(&stub, task.as_ref(), options(&scope, &cancel));
        let result = proposer
            .propose(Some(&parent), &params, &mut rng(1), 1)
            .unwrap();
        assert_eq!(result.origin, Some(CandidateOrigin::Local), "{status:?}");
        assert_eq!(stub.total_calls(), calls, "{status:?}");
        assert_eq!(proposer.tally.llm_rejected.get(reason), calls, "{status:?}");
        assert_eq!(proposer.tally.local_fallbacks, 1, "{status:?}");
        assert_eq!(proposer.tally.llm_proposals, calls, "{status:?}");
    }
}

#[test]
fn the_prompt_carries_the_exact_n_the_shape_example_and_the_json_only_line_last() {
    let task = n4_task();
    let stub = Stub::new().proposer(|_| Answer::ok(r#"{"n": 4, "weights": [1, 1, 1, 1]}"#, 1));
    let (scope, cancel) = (scope(), CancellationToken::new());
    let mut proposer = LlmProposer::new(
        &stub,
        task.as_ref(),
        LlmProposerOptions {
            prompt_context: task_prompt_context(DreamTaskId::Autocorrelation, Some(4)),
            ..options(&scope, &cancel)
        },
    );
    let parent = n4_parent(task.as_ref());
    let params = project_propose_params(&DEFAULT_POLICY);
    proposer
        .propose(Some(&parent), &params, &mut rng(1), 1)
        .unwrap();
    let prompt = &stub.prompts(DreamChildRole::Proposer)[0];
    assert!(prompt.starts_with(PROPOSER_PROMPT_HEADER));
    for needle in [
        "\"n\": 4",
        "exactly 4 weights",
        "exactly 4 entries",
        r#"{"n": 4, "weights": [1.5, 2.5, 2.5, 1.5]}"#,
    ] {
        assert!(prompt.contains(needle), "{needle}");
    }
    assert!(!prompt.contains("exactly n entries"));
    assert_eq!(prompt.lines().last(), Some(PROPOSER_JSON_ONLY));
    let candidate = prompt.find("Current candidate (JSON)").unwrap();
    let contract = prompt.find("Contract:").unwrap();
    let output = prompt.find("Output contract:").unwrap();
    let last = prompt.rfind(PROPOSER_JSON_ONLY).unwrap();
    assert!(0 < candidate && candidate < contract && contract < output && output < last);
    assert_eq!(prompt.find(PROPOSER_JSON_ONLY), Some(last));
}

fn fixed_clock() -> u64 {
    FIXED_CLOCK
}

fn explore<'a>(
    task: &'a dyn DynTask,
    dir: &'a std::path::Path,
    proposer: Option<&'a mut (dyn Proposer + 'a)>,
) -> pa_dream::rollout::ExploreResult {
    let clock = &fixed_clock;
    run_online_exploration(ExploreOptions {
        task,
        task_id: "sum-difference".to_string(),
        n: None,
        seed: Seed::Number(3),
        rng: rng(3),
        clock,
        workers: 3,
        k1: 6,
        dir,
        policy: DEFAULT_POLICY,
        iteration: 0,
        proposer,
        tree_id: None,
        cancel: None,
    })
    .expect("rollout")
}

#[test]
fn a_rollout_with_the_llm_proposer_spends_the_stubs_tokens_per_probe() {
    let task = sum_difference();
    let stub = Stub::new().proposer(|_| Answer::ok(r#"{"set":[0,1,3,7,12]}"#, 250));
    let (scope, cancel) = (scope(), CancellationToken::new());
    let mut proposer = LlmProposer::new(&stub, task.as_ref(), options(&scope, &cancel));
    let dir = tempfile::tempdir().unwrap();
    let result = explore(task.as_ref(), dir.path(), Some(&mut proposer));
    assert!(result.revealed_count > 0);
    assert_eq!(result.tokens, stub.total_tokens());
    assert_eq!(result.tokens, u64::from(result.revealed_count) * 250);
    assert_eq!(result.agent_generated_count, result.revealed_count);
}

#[test]
fn a_cancelled_token_stops_a_rollout_before_its_next_round() {
    let task = sum_difference();
    let dir = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let clock = || FIXED_CLOCK;
    let result = run_online_exploration(ExploreOptions {
        task: task.as_ref(),
        task_id: "sum-difference".to_string(),
        n: None,
        seed: Seed::Number(3),
        rng: rng(3),
        clock: &clock,
        workers: 3,
        k1: 6,
        dir: dir.path(),
        policy: DEFAULT_POLICY,
        iteration: 0,
        proposer: None,
        tree_id: None,
        cancel: Some(&cancel),
    })
    .expect("rollout");
    assert_eq!((result.rounds, result.revealed_count), (0, 0));
}

fn dreamer_options<'a>(
    scope: &'a ChildRuntimeScope,
    cancel: &'a CancellationToken,
    fallback: i64,
) -> LlmDreamerOptions<'a> {
    LlmDreamerOptions {
        scope,
        cancel,
        token_budget: 200_000,
        local_fallback_rng: rng(fallback),
        context: DreamerContext::default(),
        rejections: None,
    }
}

fn ids(candidates: &[CandidateInput]) -> Vec<String> {
    candidates
        .iter()
        .map(|candidate| policy_id(&candidate.policy))
        .collect()
}

fn local_ids(current: &ExplorationPolicy, m: usize, seed: i64) -> Vec<String> {
    propose_policies(current, m, &rng(seed))
        .iter()
        .map(policy_id)
        .collect()
}

#[test]
fn the_dreamer_keeps_in_bounds_candidates_names_every_drop_and_tops_up_locally() {
    let mut sneaky = DEFAULT_POLICY.to_value();
    sneaky["sneaky"] = json!("code");
    let mut wide = DEFAULT_POLICY.to_value();
    wide["branchWidth"] = json!(999);
    let mut bad_rule = DEFAULT_POLICY.to_value();
    bad_rule["selectionRule"] = json!("nope");
    let never = policy(|p| p.stop_rule = StopRule::Never);
    let output = json::stringify(&json!([never.to_value(), wide, sneaky, bad_rule]));
    let stub = Stub::new().dreamer(move |_| Answer::ok(&output, 500));
    let (scope, cancel) = (scope(), CancellationToken::new());
    let options = LlmDreamerOptions {
        context: DreamerContext {
            iteration: 2,
            ..DreamerContext::default()
        },
        ..dreamer_options(&scope, &cancel, 1)
    };
    let (dreamed, spans) =
        capture(|| propose_policies_with_agent(&stub, &DEFAULT_POLICY, 4, &options).unwrap());
    assert_eq!(stub.total_calls(), 1);
    assert_eq!(
        (
            dreamed.tokens,
            dreamed.returned,
            dreamed.kept,
            dreamed.truncated,
            dreamed.local
        ),
        (500, 4, 1, 0, 3)
    );
    assert_eq!(
        dreamed
            .dropped
            .iter()
            .map(|entry| entry.index)
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert!(
        dreamed.dropped[0]
            .reason
            .contains("branchWidth must be within"),
        "{}",
        dreamed.dropped[0].reason
    );
    assert!(
        dreamed.dropped[1]
            .reason
            .contains("unknown policy field: sneaky")
    );
    assert!(
        dreamed.dropped[2]
            .reason
            .contains("selectionRule must be one of"),
        "{}",
        dreamed.dropped[2].reason
    );
    assert!(!dreamed.llm_fallback);
    assert_eq!(dreamed.dreamer, DreamerKind::Mixed);
    assert_eq!(
        dreamed.candidates[0],
        CandidateInput {
            policy: never,
            origin: CandidateOrigin::Llm
        }
    );
    assert!(
        dreamed.candidates[1..]
            .iter()
            .all(|candidate| candidate.origin == CandidateOrigin::Local)
    );
    assert_eq!(
        ids(&dreamed.candidates[1..]),
        local_ids(&DEFAULT_POLICY, 3, 1)
    );
    let span = named(&spans, "dream.llm_dream")[0];
    for (key, value) in [
        ("dream.iteration", json!(2)),
        ("dream.candidates_requested", json!(4)),
        ("dream.candidates_returned", json!(4)),
        ("dream.candidates_dropped", json!(3)),
        ("dream.candidates_kept", json!(1)),
        ("dream.candidates_truncated", json!(0)),
        ("dream.candidates_local", json!(3)),
        ("dream.candidates", json!(4)),
        ("dream.dreamer", json!("mixed")),
        ("dream.llm_fallback", json!(false)),
        ("dream.llm_status", json!("completed")),
        ("dream.llm_attempts", json!(1)),
        ("dream.tokens", json!(500)),
    ] {
        assert_eq!(span.attr(key), Some(&value), "{key}");
    }
    assert_eq!(span.attr("dream.llm_reject_reason"), None);
}

#[test]
fn the_dreamer_falls_back_locally_when_everything_is_dropped_or_the_call_fails() {
    let mut sneaky = DEFAULT_POLICY.to_value();
    sneaky["sneaky"] = json!(1);
    let mut negative = DEFAULT_POLICY.to_value();
    negative["beta"] = json!(-5);
    let junk = json::stringify(&json!([sneaky, negative]));
    let output = junk.clone();
    let stub = Stub::new().dreamer(move |_| Answer::ok(&output, 300));
    let dir = tempfile::tempdir().unwrap();
    let path = rejections_path(dir.path(), "dreamer-unit");
    let clock = || FIXED_CLOCK;
    let log = RejectionLog::new(path.clone(), &clock);
    let (scope, cancel) = (scope(), CancellationToken::new());
    let options = LlmDreamerOptions {
        context: DreamerContext {
            iteration: 3,
            ..DreamerContext::default()
        },
        rejections: Some(&log),
        ..dreamer_options(&scope, &cancel, 1)
    };
    let (dropped, spans) =
        capture(|| propose_policies_with_agent(&stub, &DEFAULT_POLICY, 4, &options).unwrap());
    assert_eq!(stub.total_calls(), 2);
    assert_eq!(
        (
            dropped.tokens,
            dropped.returned,
            dropped.kept,
            dropped.local
        ),
        (600, 2, 0, 4)
    );
    assert!(
        dropped.dropped[0]
            .reason
            .contains("unknown policy field: sneaky")
    );
    assert!(
        dropped.dropped[1].reason.contains("beta must be within"),
        "{}",
        dropped.dropped[1].reason
    );
    assert!(dropped.llm_fallback);
    assert_eq!(dropped.dreamer, DreamerKind::Local);
    assert_eq!(ids(&dropped.candidates), local_ids(&DEFAULT_POLICY, 4, 1));
    let span = named(&spans, "dream.llm_dream")[0];
    for (key, value) in [
        ("dream.iteration", json!(3)),
        ("dream.llm_fallback", json!(true)),
        ("dream.llm_status", json!("completed")),
        ("dream.llm_reject_reason", json!("shape")),
        (
            "dream.llm_reject_excerpt",
            json!(excerpt_of(&junk, REJECTION_EXCERPT_CHARS)),
        ),
        ("dream.llm_attempts", json!(2)),
        ("dream.candidates_returned", json!(2)),
        ("dream.candidates_dropped", json!(2)),
        ("dream.candidates_kept", json!(0)),
        ("dream.candidates_local", json!(4)),
        ("dream.dreamer", json!("local")),
    ] {
        assert_eq!(span.attr(key), Some(&value), "{key}");
    }
    let logged = read_rejections(&path).unwrap();
    assert_eq!(
        logged
            .iter()
            .map(|line| (
                line.input.role,
                line.input.iteration,
                line.input.round,
                line.input.attempt,
                line.input.fell_back
            ))
            .collect::<Vec<_>>(),
        vec![
            (Some(RejectionRole::Dreamer), 3, 0, 1, false),
            (Some(RejectionRole::Dreamer), 3, 0, 2, true),
        ]
    );
    assert!(
        logged
            .iter()
            .all(|line| line.input.reason == ProposalRejectReason::Shape
                && line.input.status == RunAgentStatus::Completed)
    );
    let error = logged[0].input.error.as_deref().unwrap();
    assert!(
        error.starts_with(
            "every entry was dropped: [0] unknown policy field: sneaky; [1] beta must be within"
        ),
        "{error}"
    );

    let failing = Stub::new().dreamer(|_| Answer::status(RunAgentStatus::Error, 20));
    let failed = propose_policies_with_agent(
        &failing,
        &DEFAULT_POLICY,
        3,
        &dreamer_options(&scope, &cancel, 2),
    )
    .unwrap();
    assert_eq!((failed.tokens, failed.returned), (40, 0));
    assert!(failed.llm_fallback);
    assert_eq!(ids(&failed.candidates), local_ids(&DEFAULT_POLICY, 3, 2));

    let limited = Stub::new().dreamer(|_| Answer::status(RunAgentStatus::TurnLimit, 5));
    let capped = propose_policies_with_agent(
        &limited,
        &DEFAULT_POLICY,
        2,
        &dreamer_options(&scope, &cancel, 2),
    )
    .unwrap();
    assert_eq!(limited.total_calls(), 1);
    assert_eq!(capped.local, 2);
}

/// TS `SYNTH`: revealing more of the root's children raises the best score.
fn synth_pool() -> Vec<RecordedTree> {
    vec![support::tree(
        "synth",
        2,
        &[
            (0, None, 0, 0, 0.3),
            (1, Some(0), 0, 0, 0.5),
            (2, Some(0), 0, 1, 0.4),
            (3, Some(0), 0, 2, 0.9),
        ],
    )]
}

const CFG_OBJECTIVE: ReplayObjectiveConfig = ReplayObjectiveConfig {
    beta1: 0.05,
    beta2: 0.05,
    beta3: 0.0,
};

struct Resolved(Vec<CandidateInput>);

impl CandidateSource for Resolved {
    fn propose(&mut self, _: &ExplorationPolicy, _: usize, _: &SeededRng) -> Vec<CandidateInput> {
        self.0.clone()
    }
}

fn dream_with(
    current: ExplorationPolicy,
    candidates: Vec<CandidateInput>,
    dreams: usize,
    objective: ReplayObjectiveConfig,
) -> pa_dream::improve::DreamResult {
    let mut source = Resolved(candidates);
    let revoked = std::collections::HashSet::new();
    run_dreaming(DreamingOptions {
        current,
        pool: &synth_pool(),
        dreams,
        k1: 5,
        k2: 10,
        rng: rng(1),
        objective,
        quality_eps: 0.0,
        iteration: 1,
        candidates: Some(&mut source),
        lever_scan: true,
        revoked: &revoked,
    })
}

#[test]
fn only_distinct_new_policies_count_toward_m_and_the_rest_ride_along_or_are_truncated() {
    let a = policy(|p| p.stop_rule = StopRule::Never);
    let b = policy(|p| p.selection_rule = SelectionRule::RoundRobin);
    let c = policy(|p| p.batch_size = 1);
    let output = json::stringify(&json!([a, DEFAULT_POLICY, a, b, c]));
    let stub = Stub::new().dreamer(move |_| Answer::ok(&output, 9));
    let (scope, cancel) = (scope(), CancellationToken::new());
    let dreamed = propose_policies_with_agent(
        &stub,
        &DEFAULT_POLICY,
        2,
        &dreamer_options(&scope, &cancel, 1),
    )
    .unwrap();
    assert_eq!(dreamed.returned, 5);
    assert_eq!(dreamed.dropped, Vec::<DroppedCandidate>::new());
    assert_eq!((dreamed.kept, dreamed.truncated, dreamed.local), (2, 1, 0));
    assert_eq!(dreamed.dreamer, DreamerKind::Llm);
    assert_eq!(
        ids(&dreamed.candidates),
        vec![
            policy_id(&a),
            policy_id(&DEFAULT_POLICY),
            policy_id(&a),
            policy_id(&b)
        ]
    );
    let selection = dream_with(DEFAULT_POLICY, dreamed.candidates, 2, CFG_OBJECTIVE);
    let reasons: Vec<CandidateReason> = selection
        .candidates
        .iter()
        .map(|verdict| verdict.reason)
        .collect();
    assert!(!matches!(
        reasons[0],
        CandidateReason::Identical | CandidateReason::Duplicate
    ));
    assert_eq!(
        &reasons[1..3],
        &[CandidateReason::Identical, CandidateReason::Duplicate]
    );
    assert!(!matches!(
        reasons[3],
        CandidateReason::Identical | CandidateReason::Duplicate
    ));
    assert_eq!(selection.candidates[2].duplicate_of, Some(0));
    assert_eq!(selection.scored_count, 3);
    assert_eq!(selection.dreamer, DreamerKind::Llm);
}

#[test]
fn m_zero_makes_no_call_and_an_aborted_dreamer_aborts() {
    let idle = Stub::new().dreamer(|_| Answer::ok("[]", 1));
    let (scope, cancel) = (scope(), CancellationToken::new());
    let none = propose_policies_with_agent(
        &idle,
        &DEFAULT_POLICY,
        0,
        &dreamer_options(&scope, &cancel, 1),
    )
    .unwrap();
    assert_eq!(idle.total_calls(), 0);
    assert_eq!(none.candidates, Vec::new());
    assert_eq!(none.tokens, 0);
    assert!(!none.llm_fallback);

    let aborting = Stub::new().dreamer(|_| Answer::status(RunAgentStatus::Aborted, 2));
    let dir = tempfile::tempdir().unwrap();
    let path = rejections_path(dir.path(), "dreamer-abort");
    let clock = || FIXED_CLOCK;
    let log = RejectionLog::new(path.clone(), &clock);
    let options = LlmDreamerOptions {
        rejections: Some(&log),
        ..dreamer_options(&scope, &cancel, 1)
    };
    let error = propose_policies_with_agent(&aborting, &DEFAULT_POLICY, 2, &options)
        .err()
        .unwrap();
    assert!(matches!(error, DreamStoreError::Aborted(_)));
    assert_eq!(aborting.total_calls(), 1);
    let logged = read_rejections(&path).unwrap();
    assert_eq!(logged.len(), 1);
    assert_eq!(
        (
            logged[0].input.role,
            logged[0].input.reason,
            logged[0].input.status,
            logged[0].input.fell_back
        ),
        (
            Some(RejectionRole::Dreamer),
            ProposalRejectReason::Aborted,
            RunAgentStatus::Aborted,
            false
        )
    );
}

#[test]
fn parse_candidate_array_keeps_accepted_entries_and_names_every_drop() {
    assert_eq!(parse_candidate_array(&json!("nope")).returned, 0);
    let mut fractional = DEFAULT_POLICY.to_value();
    fractional["beta"] = json!(1.5);
    let parsed = parse_candidate_array(&json!([DEFAULT_POLICY, fractional, 7]));
    assert_eq!(parsed.returned, 3);
    assert_eq!(parsed.kept, vec![DEFAULT_POLICY]);
    assert_eq!(
        parsed
            .dropped
            .iter()
            .map(|entry| entry.index)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    assert!(
        parsed.dropped[0].reason.contains("beta must be an integer"),
        "{}",
        parsed.dropped[0].reason
    );
    assert!(
        parsed.dropped[1]
            .reason
            .contains("policy must be a JSON object")
    );
}

fn grow_pool(dir: &std::path::Path, seed: i64) -> Vec<RecordedTree> {
    let task = sum_difference();
    let clock = || FIXED_CLOCK;
    for iteration in [0, 1] {
        run_online_exploration(ExploreOptions {
            task: task.as_ref(),
            task_id: "sum-difference".to_string(),
            n: None,
            seed: Seed::Number(seed),
            rng: rng(seed).fork(&format!("iter:{iteration}")),
            clock: &clock,
            workers: 3,
            k1: 5,
            dir,
            policy: DEFAULT_POLICY,
            iteration,
            proposer: None,
            tree_id: None,
            cancel: None,
        })
        .expect("rollout");
    }
    list_trees(dir)
        .iter()
        .map(|summary| read_tree(&summary.tree_id, dir).expect("tree"))
        .collect()
}

#[test]
fn the_dreamer_digest_is_the_current_policys_replay_under_the_selections_scoring() {
    let dir = tempfile::tempdir().unwrap();
    let pool = grow_pool(dir.path(), 7);
    let input = build_dreamer_input(
        &DEFAULT_POLICY,
        4,
        &DreamerContext {
            iteration: 2,
            pool: &pool,
            objective: Some(CFG_OBJECTIVE),
            workers: Some(3),
            k1: Some(5),
            k2: Some(10),
            history: &[],
        },
    );
    assert_eq!((input.m, input.iteration), (4, 2));
    assert_eq!(
        (input.budget.workers, input.budget.k1, input.budget.k2),
        (3, 5, 10)
    );
    let mut sorted: Vec<String> = pool
        .iter()
        .map(|tree| tree.header.tree_id.clone())
        .collect();
    sorted.sort();
    assert_eq!(
        input
            .pool
            .iter()
            .map(|tree| tree.tree_id.clone())
            .collect::<Vec<_>>(),
        sorted
    );
    let cfg = pa_dream::improve::DreamingScoreConfig {
        k1: 5,
        k2: 10,
        objective: CFG_OBJECTIVE,
        quality_eps: 0.0,
    };
    let score = pa_dream::improve::score_policy_on_pool(
        &DEFAULT_POLICY,
        &pool,
        &cfg,
        pa_dream::improve::SpendCharge::Raw,
    );
    #[allow(clippy::cast_precision_loss)]
    let trees = input.pool.len() as f64;
    let mean_v = input.pool.iter().map(|tree| tree.value).sum::<f64>() / trees;
    assert!(
        (mean_v - score.value).abs() < 5e-13,
        "{mean_v} vs {}",
        score.value
    );
    let mean_n = input.pool.iter().map(|tree| f64::from(tree.n)).sum::<f64>() / trees;
    assert!((mean_n - score.n).abs() < 5e-13);
    assert!(input.history.is_empty());
    let bare = build_dreamer_input(&DEFAULT_POLICY, 2, &DreamerContext::default());
    assert!(bare.pool.is_empty());
    assert_eq!(bare.budget.workers, 1);
    assert_eq!((bare.scale.score_min, bare.scale.score_max), (0.0, 0.0));
}

#[test]
fn the_dreamer_prompt_states_the_rules_the_pool_and_the_history_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let pool = grow_pool(dir.path(), 7);
    let earlier = policy(|p| p.selection_rule = SelectionRule::Weighted);
    let verdict: pa_dream::improve::CandidateVerdict = serde_json::from_value(json!({
        "index": 0, "policyId": policy_id(&earlier), "policy": earlier, "origin": "llm",
        "changed": ["selectionRule"], "duplicateOf": null, "value": 0.5, "quality": 1,
        "anytime": 0.9, "cost": 0.5, "roundsSaved": 0, "N": 10, "rounds": 5,
        "outOfSupportCells": 0, "inSupportMean": 1, "inSupportMin": 1, "chargedProbes": 10,
        "chargedRounds": 5, "evidenceTrees": 1, "eligible": true, "reason": "tie"
    }))
    .unwrap();
    let history = history_of(1, &[verdict]);
    let input = build_dreamer_input(
        &DEFAULT_POLICY,
        4,
        &DreamerContext {
            iteration: 2,
            pool: &pool,
            objective: Some(ReplayObjectiveConfig {
                beta1: 0.05,
                beta2: 0.1,
                beta3: 0.25,
            }),
            workers: Some(3),
            k1: Some(5),
            k2: Some(10),
            history: &history,
        },
    );
    let prompt = build_dream_prompt(&input);
    assert_eq!(prompt.lines().next(), Some(DREAMER_PROMPT_HEADER));
    let dead: Vec<&str> = REPLAY_DEAD_FIELDS
        .iter()
        .map(|field| field.as_str())
        .collect();
    for needle in [
        "Propose up to 4 revised exploration policies".to_string(),
        "dreaming step 2".to_string(),
        "V = (1 - beta3) * q + beta3 * anytime - beta1 * S / (W * k1) + beta2 * (1 - rounds / k1)".to_string(),
        "beta1 = 0.05, beta2 = 0.1, beta3 = 0.25".to_string(),
        "W = 3 cells per round, online round cap k1 = 5, replay round cap k2 = 10".to_string(),
        "the current policy wins every tie, so only a STRICTLY higher mean V".to_string(),
        "q is below the current policy's on ANY of these trees is excluded".to_string(),
        "stop-early credit (fewer charged probes, fewer rounds) is charged at the latest probe and round at which the same candidate was still improving on the OTHER recorded trees, so on a single tree there is none".to_string(),
        "the current policy is charged exactly what it spent".to_string(),
        "no-worse quality must hold on every tree".to_string(),
        "Probation: an adopted policy's first online rollout must reach at least the current policy's lowest recorded best on these trees, or the adoption is reverted and the policy is revoked for the rest of the run".to_string(),
        "Replay mechanics: a candidate re-walks each recorded tree".to_string(),
        "out of support: it reveals nothing but is charged as a probe".to_string(),
        format!("{} are never read by replay", dead.join(", ")),
        "capped at W = 3 at runtime".to_string(),
        "beta is read only under patience and fixed-rounds".to_string(),
        "patience stops after beta consecutive rounds without improving".to_string(),
        "mean V ".to_string(),
        "is the value to beat".to_string(),
        "Earlier candidates and their verdicts".to_string(),
        format!("- iteration 1: {} (llm; changed selectionRule) -> V 0.500000, q 1: tie", policy_id(&earlier)),
        "Return at most 4 policy objects that are pairwise distinct".to_string(),
    ] {
        assert!(prompt.contains(&needle), "missing: {needle}");
    }
    for tree in &pool {
        assert!(prompt.contains(&format!("- {}: N ", tree.header.tree_id)));
    }
    assert!(
        prompt
            .lines()
            .last()
            .unwrap()
            .contains("Return exactly one JSON array and nothing else")
    );
    let at = |needle: &str| prompt.find(needle).unwrap();
    let order = [
        at("Named-rule fields"),
        at("How a candidate is judged"),
        at("Field semantics"),
        at("Current policy:"),
        at("Current policy on the pool"),
        at("Earlier candidates"),
    ];
    assert!(
        order[0] > 0 && order.windows(2).all(|pair| pair[0] < pair[1]),
        "{order:?}"
    );
    let empty = build_dream_prompt(&build_dreamer_input(
        &DEFAULT_POLICY,
        1,
        &DreamerContext::default(),
    ));
    assert!(!empty.contains("Earlier candidates"));
    assert!(empty.contains("no recorded trees yet"));
    assert_eq!(empty.lines().next(), Some(DREAMER_PROMPT_HEADER));
}

#[test]
fn a_strictly_worse_or_collapsing_or_malformed_policy_is_never_deployed() {
    let current = policy(|p| {
        p.selection_rule = SelectionRule::ExploreRoot;
        p.stop_rule = StopRule::Patience;
        p.beta = 1;
        p.batch_size = 1;
    });
    let worse = policy(|p| {
        p.selection_rule = SelectionRule::BestFirst;
        p.stop_rule = StopRule::Never;
        p.batch_size = 1;
    });
    let (scope, cancel) = (scope(), CancellationToken::new());
    let output = json::stringify(&json!([worse]));
    let stub = Stub::new().dreamer(move |_| Answer::ok(&output, 111));
    let dreamed =
        propose_policies_with_agent(&stub, &current, 1, &dreamer_options(&scope, &cancel, 1))
            .unwrap();
    assert_eq!(ids(&dreamed.candidates), vec![policy_id(&worse)]);
    assert_eq!(dreamed.dreamer, DreamerKind::Llm);
    let selection = dream_with(current, dreamed.candidates, 1, CFG_OBJECTIVE);
    assert!(!selection.improved);
    assert_eq!(selection.chosen_policy_id, policy_id(&current));
    assert!(selection.chosen_score >= selection.current_score);
    assert!(selection.chosen_quality >= selection.current_quality);

    let collapsing = policy(|p| {
        p.selection_rule = SelectionRule::ExploreRoot;
        p.stop_rule = StopRule::FixedRounds;
        p.beta = 1;
        p.batch_size = 1;
    });
    let exploring = policy(|p| {
        p.selection_rule = SelectionRule::ExploreRoot;
        p.stop_rule = StopRule::Never;
        p.batch_size = 1;
    });
    let output = json::stringify(&json!([collapsing]));
    let stub = Stub::new().dreamer(move |_| Answer::ok(&output, 5));
    let dreamed =
        propose_policies_with_agent(&stub, &exploring, 1, &dreamer_options(&scope, &cancel, 1))
            .unwrap();
    assert_eq!(ids(&dreamed.candidates), vec![policy_id(&collapsing)]);
    let selection = dream_with(
        exploring,
        dreamed.candidates,
        1,
        ReplayObjectiveConfig {
            beta1: 5.0,
            beta2: 0.0,
            beta3: 0.0,
        },
    );
    assert_eq!(selection.chosen_policy_id, policy_id(&exploring));
    assert!(!selection.improved);
    assert_eq!(selection.quality_rejected, 1);
    assert_eq!(selection.current_quality, 1.0);

    let mut exfiltrate = current.to_value();
    exfiltrate["exfiltrate"] = json!("rm -rf");
    let output = json::stringify(&json!([exfiltrate]));
    let stub = Stub::new().dreamer(move |_| Answer::ok(&output, 9));
    let dreamed =
        propose_policies_with_agent(&stub, &current, 1, &dreamer_options(&scope, &cancel, 3))
            .unwrap();
    assert_eq!(ids(&dreamed.candidates), local_ids(&current, 1, 3));
    assert_eq!(dreamed.dreamer, DreamerKind::Local);
    assert!(
        dreamed.dropped[0]
            .reason
            .contains("unknown policy field: exfiltrate")
    );
}

fn node(id: &str, parent: Option<&str>, seq: u32, score: f64) -> NodeRecord {
    support::node(id, parent, seq, score)
}

#[test]
fn the_guidance_digest_is_deterministic_bounded_and_scalar() {
    let dir = tempfile::tempdir().unwrap();
    let blobs = [
        json!({"set": [0, 1]}),
        json!({"set": [0, 1, 3]}),
        json!({"set": [0, 2, 3]}),
        json!({"set": [0, 1, 3, 7, 12, 20]}),
    ];
    let write = |tree_id: &str, policy: &str| {
        let writer = pa_dream::store::TreeWriter::new(tree_id, dir.path());
        writer
            .write_header(&TreeHeaderRecord {
                policy_id: policy.to_string(),
                ..support::header(tree_id, 2)
            })
            .unwrap();
        let id = |seq: u32| format!("{tree_id}-n{seq}");
        let rows = [
            (0, None, 0.3),
            (1, Some(0), 0.5),
            (2, Some(0), 0.4),
            (3, Some(0), 0.9),
        ];
        for (seq, parent, score) in rows {
            let mut record = node(&id(seq), parent.map(id).as_deref(), seq, score);
            record.branch = seq.saturating_sub(1);
            writer.append_node(&record).unwrap();
            writer.write_blob(seq, &blobs[seq as usize]).unwrap();
        }
        let mut failed = node(&id(4), Some(&id(1)), 4, 0.0);
        failed.round = 2;
        failed.valid = false;
        failed.fail_class = Some("degenerate".to_string());
        writer.append_node(&failed).unwrap();
        read_tree(tree_id, dir.path()).unwrap()
    };
    let pool = vec![write("synth", "p"), write("alpha", "q")];
    let a = build_guidance_input(&pool, "sum-difference", 3, 2, 16);
    let reversed: Vec<RecordedTree> = pool.iter().rev().cloned().collect();
    let b = build_guidance_input(&reversed, "sum-difference", 3, 2, 16);
    assert_eq!(json::stringify(&a), json::stringify(&b));
    assert_eq!(
        (a.task_id.as_str(), a.iteration, a.pool_size),
        ("sum-difference", 3, 2)
    );
    assert_eq!(
        a.trees
            .iter()
            .map(|tree| tree.tree_id.as_str())
            .collect::<Vec<_>>(),
        vec!["alpha", "synth"]
    );
    let synth = &a.trees[1];
    assert_eq!(synth.policy_id, "p");
    assert_eq!(synth.best_score, 0.9);
    assert_eq!((synth.attempts, synth.rounds), (4, 2));
    assert_eq!(synth.fail_classes, vec!["degenerate".to_string()]);
    assert_eq!(
        synth
            .top_nodes
            .iter()
            .map(|entry| entry.score)
            .collect::<Vec<_>>(),
        vec![0.9, 0.5]
    );
    let full = json::stringify(&blobs[3]);
    assert_eq!(
        synth.top_nodes[0].artifact_json,
        format!("{}...", &full[..16])
    );
    assert_eq!(synth.top_nodes[1].artifact_json, json::stringify(&blobs[1]));
    assert!(
        build_guidance_input(&pool, "sum-difference", 3, 0, 16)
            .trees
            .iter()
            .all(|tree| tree.top_nodes.is_empty())
    );

    // An in-memory tree has no blob loader: the artifact is an empty string.
    let digest = build_guidance_input(&synth_pool(), "sum-difference", 1, 3, 2000);
    assert_eq!(
        digest.trees[0]
            .top_nodes
            .iter()
            .map(|entry| (entry.artifact_json.as_str(), entry.score))
            .collect::<Vec<_>>(),
        vec![("", 0.9), ("", 0.5), ("", 0.4)]
    );
}
