//! The TS `dream-experiment-llm.test.ts` behaviour: the agent-backed arm
//! runner with a shared round 1, per-arm accounting, the four arms, the
//! detached experiment root, cancellation and priming.

#![allow(clippy::float_cmp)]
#![allow(clippy::too_many_lines)] // the ported TS cases stay whole

mod support;

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use pa_dream::child::{
    ChildRuntimeScope, RunAgent, RunAgentOptions, RunAgentRequest, RunAgentResult, RunAgentStatus,
};
use pa_dream::dream_loop::{dream_run_id, priming_tree_id, DreamHandlerCalls};
use pa_dream::dreams::{dreams_path, read_dreams_log, DreamsLogLine};
use pa_dream::experiment::{
    is_experiment_result, plan_experiment, read_experiment_result, run_experiment, ExperimentArm,
    ExperimentArmMode, ExperimentArmResult, ExperimentArmRunner, ExperimentBudget, ExperimentError,
    ExperimentProgressEvent, ExperimentResult, ExperimentRunOptions, ExperimentSpec,
    EXPERIMENT_ARMS,
};
use pa_dream::experiment_llm::{run_experiment_with_agent, AgentArmRunner, AgentArmRunnerOptions};
use pa_dream::improve::CandidateOrigin;
use pa_dream::json;
use pa_dream::llm::DreamChildRole;
use pa_dream::policy::{policy_id, SelectionRule, StopRule, DEFAULT_POLICY, PRIMING_DIVERSE};
use pa_dream::proposer::{ProposalRejectReason, RejectCounts};
use pa_dream::rejections::{read_rejections, rejections_path};
use pa_dream::rng::Seed;
use pa_dream::store::{experiment_arm_dir, experiment_result_path, list_trees, read_tree};
use pa_dream::tasks::DreamTaskId;
use serde_json::json;
use support::spans::{capture, named, SpanRecord};
use support::stub::Answer;
use tokio_util::sync::CancellationToken;

const FIXED_CLOCK: u64 = 1_700_000_000_000;
const ARTIFACT: &str = r#"{"set":[0,1,2,4,9]}"#;
const INSIGHTS: &str =
    "Wider gaps between set members raised the score; consecutive runs lowered it.";
const GUIDANCE_PREFIX: &str = "Directional insights from prior trajectories";
const SHARED: &str = "shared";

fn fixed_clock() -> u64 {
    FIXED_CLOCK
}

fn revised() -> String {
    json::stringify(&json!([support::policy(|p| p.stop_rule = StopRule::Never)]))
}

fn spec(arms: &[ExperimentArm]) -> ExperimentSpec {
    ExperimentSpec::new(
        DreamTaskId::SumDifference,
        Seed::Number(7),
        2,
        ExperimentBudget {
            workers: 3,
            k1: 5,
            k2: 10,
            dreams: 4,
        },
        arms.to_vec(),
    )
}

fn run_options(dir: &Path) -> ExperimentRunOptions<'_> {
    ExperimentRunOptions {
        dir,
        clock: &fixed_clock,
        notes: Vec::new(),
        overwrite: false,
    }
}

fn scope() -> ChildRuntimeScope {
    ChildRuntimeScope {
        max_turns: Some(2),
        token_budget: Some(500_000),
        ..ChildRuntimeScope::default()
    }
}

fn agent(proposer: bool, dreamer: bool) -> AgentArmRunnerOptions {
    AgentArmRunnerOptions {
        use_llm_proposer: proposer,
        use_llm_dreamer: dreamer,
        ..AgentArmRunnerOptions::new(scope(), CancellationToken::new())
    }
}

/// What the stub saw while one arm (or the shared round) ran.
#[derive(Debug, Clone, Default)]
struct Tally {
    calls: DreamHandlerCalls,
    tokens: u64,
    prompts: HashMap<&'static str, Vec<String>>,
}

type Script = Box<dyn Fn(u64) -> Answer + Send + Sync>;

/// TS `makeArmStub`: attributes every call to the arm currently running (the
/// shared round 1 runs before any arm starts and is tagged `shared`).
struct ArmStub {
    proposer: Script,
    dreamer: Script,
    guidance: Script,
    current: Mutex<String>,
    tallies: Mutex<HashMap<String, Tally>>,
    total: Mutex<(u64, u64)>,
    events: Mutex<Vec<ExperimentProgressEvent>>,
}

impl ArmStub {
    fn new(proposer: Script, dreamer: Script, guidance: Script) -> Self {
        Self {
            proposer,
            dreamer,
            guidance,
            current: Mutex::new(SHARED.to_string()),
            tallies: Mutex::new(HashMap::new()),
            total: Mutex::new((0, 0)),
            events: Mutex::new(Vec::new()),
        }
    }

    fn llm() -> Self {
        let insights = json::stringify(&json!({ "insights": INSIGHTS }));
        Self::new(
            Box::new(|_| Answer::ok(ARTIFACT, 10)),
            Box::new(|_| Answer::ok(&revised(), 200)),
            Box::new(move |_| Answer::ok(&insights, 70)),
        )
    }

    fn progress(&self, event: ExperimentProgressEvent) {
        if let ExperimentProgressEvent::ArmStart { arm, .. } = &event {
            *self.current.lock().unwrap() = arm.as_str().to_string();
        }
        self.events.lock().unwrap().push(event);
    }

    fn tally(&self, name: &str) -> Option<Tally> {
        self.tallies.lock().unwrap().get(name).cloned()
    }

    fn calls(&self) -> u64 {
        self.total.lock().unwrap().0
    }

    fn tokens(&self) -> u64 {
        self.total.lock().unwrap().1
    }

    fn sum_role(&self, role: fn(&DreamHandlerCalls) -> u64) -> u64 {
        self.tallies
            .lock()
            .unwrap()
            .values()
            .map(|tally| role(&tally.calls))
            .sum()
    }
}

impl RunAgent for ArmStub {
    fn run(&self, request: &RunAgentRequest, _options: &RunAgentOptions) -> RunAgentResult {
        let role = DreamChildRole::of_prompt(&request.prompt).expect("a dream prompt");
        let current = self.current.lock().unwrap().clone();
        let (call, key) = {
            let mut tallies = self.tallies.lock().unwrap_or_else(PoisonError::into_inner);
            let tally = tallies.entry(current).or_default();
            let (slot, key) = match role {
                DreamChildRole::Proposer => (&mut tally.calls.proposer, "proposer"),
                DreamChildRole::Dreamer => (&mut tally.calls.dreamer, "dreamer"),
                DreamChildRole::Guidance => (&mut tally.calls.guidance, "guidance"),
            };
            *slot += 1;
            let call = *slot;
            tally
                .prompts
                .entry(key)
                .or_default()
                .push(request.prompt.clone());
            (call, key)
        };
        let answer = match key {
            "proposer" => (self.proposer)(call),
            "dreamer" => (self.dreamer)(call),
            _ => (self.guidance)(call),
        };
        {
            let current = self.current.lock().unwrap().clone();
            let mut tallies = self.tallies.lock().unwrap();
            tallies.get_mut(&current).unwrap().tokens += answer.tokens;
            let mut total = self.total.lock().unwrap();
            total.0 += 1;
            total.1 += answer.tokens;
        }
        RunAgentResult {
            status: answer.status,
            output: answer.output,
            stop_reason: None,
            total_tokens: answer.tokens,
            output_tokens: 0,
            error: None,
        }
    }
}

fn run_with(
    stub: &ArmStub,
    spec: &ExperimentSpec,
    dir: &Path,
    agent: AgentArmRunnerOptions,
) -> Result<ExperimentResult, ExperimentError> {
    let mut progress = |event| stub.progress(event);
    run_experiment_with_agent(
        spec,
        &run_options(dir),
        stub,
        agent,
        Some(&mut progress),
        None,
    )
}

fn tree_files(dir: &Path, tree_id: &str) -> Vec<(String, String)> {
    let mut files = vec![(
        "tree".to_string(),
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

fn arm(result: &ExperimentResult, name: ExperimentArm) -> &ExperimentArmResult {
    result
        .arms
        .iter()
        .find(|candidate| candidate.arm == name)
        .unwrap()
}

fn arm_dir(dir: &Path, result: &ExperimentResult, name: ExperimentArm) -> std::path::PathBuf {
    experiment_arm_dir(dir, &result.experiment_id, name.as_str())
}

#[test]
fn round_1_is_shared_across_every_arm_and_each_arm_is_charged_its_own_calls_and_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let stub = ArmStub::llm();
    let result = run_with(&stub, &spec(EXPERIMENT_ARMS), dir.path(), agent(true, true)).unwrap();
    let written = read_experiment_result(dir.path(), &result.experiment_id).unwrap();
    assert!(is_experiment_result(&written));
    assert_eq!(json::stringify(&written), json::stringify(&result));
    assert!(result.shared_initial_rollout);
    assert_eq!(
        result.arms.iter().map(|a| a.arm).collect::<Vec<_>>(),
        EXPERIMENT_ARMS.to_vec()
    );
    assert!(result
        .arms
        .iter()
        .all(|a| a.rounds.len() == 2 && a.mode.proposer == "llm" && a.mode.dreamer == "llm"));
    assert_eq!(result.headline.as_ref().unwrap().reference, "fixed");
    assert!(!dir.path().join("trees").exists());

    let first = &result.arms[0].rounds[0];
    let first_files = tree_files(
        &arm_dir(dir.path(), &result, ExperimentArm::Dream),
        &first.tree_id,
    );
    assert!(first_files.len() > 1);
    assert!(first.probes_to_round_best <= first.probes);
    assert_eq!(first.improvements.last().unwrap().score, first.round_best);
    assert_eq!(first.priming_tree_ids, None);
    for a in &result.arms {
        assert_eq!(&a.rounds[0], first);
        let store = arm_dir(dir.path(), &result, a.arm);
        assert_eq!(tree_files(&store, &first.tree_id), first_files);
        assert_eq!(list_trees(&store).len(), 2);
    }
    let run_ids: std::collections::HashSet<&str> =
        result.arms.iter().map(|a| a.run_id.as_str()).collect();
    assert_eq!(run_ids.len(), result.arms.len());
    assert!(result.arms.iter().all(|a| a.run_id.ends_with(&format!(
        "-{}_{}",
        result.experiment_id,
        a.arm.as_str()
    ))));
    for a in &result.arms {
        assert!(
            result.headline.as_ref().unwrap().probes_to_target_exact[a.arm.as_str()].is_number()
        );
    }
    let shared = stub.tally(SHARED).unwrap();
    assert_eq!(
        shared.calls,
        DreamHandlerCalls {
            proposer: first.handler_calls.proposer,
            dreamer: 0,
            guidance: 0
        }
    );
    assert_eq!(shared.calls.proposer, u64::from(first.probes));
    assert_eq!(shared.tokens, first.tokens);
    assert_eq!(first.agent_generated_calls, first.probes);
    assert_eq!(
        (
            first.llm_proposals,
            first.llm_accepted,
            first.local_fallbacks
        ),
        (u64::from(first.probes), u64::from(first.probes), 0)
    );
    assert_eq!(first.llm_rejected, RejectCounts::default());
    assert!(!rejections_path(
        &arm_dir(dir.path(), &result, ExperimentArm::Dream),
        &format!("{}-shared", result.experiment_id)
    )
    .exists());
    for a in &result.arms {
        assert_eq!(a.totals.agent_generated_calls, a.totals.probes);
        assert_eq!(a.totals.local_fallbacks, 0);
        assert!(a
            .rounds
            .iter()
            .all(|row| u64::from(row.agent_generated_calls) == row.llm_accepted));
    }
    let arm_proposer_calls: u64 = EXPERIMENT_ARMS
        .iter()
        .map(|a| stub.tally(a.as_str()).unwrap().calls.proposer)
        .sum();
    assert_eq!(
        stub.calls(),
        shared.calls.proposer
            + arm_proposer_calls
            + stub.sum_role(|calls| calls.dreamer)
            + stub.sum_role(|calls| calls.guidance)
    );
    for a in &result.arms {
        let tally = stub.tally(a.arm.as_str()).unwrap();
        assert_eq!(a.rounds[1].handler_calls, tally.calls);
        assert_eq!(a.rounds[1].tokens, tally.tokens);
        assert_eq!(a.totals.tokens, shared.tokens + tally.tokens);
        assert_eq!(
            a.totals.handler_calls,
            shared.calls.proposer
                + tally.calls.proposer
                + tally.calls.dreamer
                + tally.calls.guidance
        );
    }
    for name in [ExperimentArm::Fixed, ExperimentArm::FixedGuided] {
        let fixed = arm(&result, name);
        assert!(fixed.fixed_policy);
        assert!(fixed.rounds.iter().all(|row| row.dreaming.is_none()));
        assert_eq!(fixed.final_policy_id, fixed.initial_policy_id);
        assert_eq!(fixed.policy_changes, 0);
        assert_eq!(stub.tally(name.as_str()).unwrap().calls.dreamer, 0);
    }
    for name in [ExperimentArm::Dream, ExperimentArm::DreamGuided] {
        let dreaming = arm(&result, name);
        assert!(!dreaming.fixed_policy);
        assert!(dreaming.rounds[1].dreaming.as_ref().unwrap().candidates > 0);
        assert_eq!(stub.tally(name.as_str()).unwrap().calls.dreamer, 1);
    }
    for a in &result.arms {
        let tally = stub.tally(a.arm.as_str()).unwrap();
        assert_eq!(a.guided, a.arm.as_str().ends_with("-guided"));
        assert_eq!(tally.calls.guidance, u64::from(a.guided));
        assert_eq!(a.rounds[1].handler_calls.guidance, u64::from(a.guided));
        let prompts = &tally.prompts["proposer"];
        assert!(!prompts.is_empty());
        assert!(prompts
            .iter()
            .all(|prompt| prompt.contains(GUIDANCE_PREFIX) == a.guided));
        assert!(prompts
            .iter()
            .all(|prompt| prompt.contains(INSIGHTS) == a.guided));
    }
    assert!(!shared.prompts["proposer"]
        .iter()
        .any(|prompt| prompt.contains(GUIDANCE_PREFIX)));
    let every: Vec<String> = stub
        .tallies
        .lock()
        .unwrap()
        .values()
        .flat_map(|tally| tally.prompts.get("proposer").cloned().unwrap_or_default())
        .collect();
    assert!(!every.is_empty());
    assert!(!every.iter().any(|prompt| prompt.contains("Contract:")));
    let events = stub.events.lock().unwrap();
    let starts: Vec<ExperimentArm> = events
        .iter()
        .filter_map(|event| match event {
            ExperimentProgressEvent::ArmStart { arm, .. } => Some(*arm),
            _ => None,
        })
        .collect();
    assert_eq!(starts, EXPERIMENT_ARMS.to_vec());
    assert!(matches!(
        events.last(),
        Some(ExperimentProgressEvent::Completed { .. })
    ));
}

#[test]
fn rejected_child_results_are_reported_apart_from_the_agents_work() {
    let dir = tempfile::tempdir().unwrap();
    let calls = Arc::new(Mutex::new(0_u64));
    let counter = Arc::clone(&calls);
    let stub = ArmStub::new(
        Box::new(move |_| {
            let mut calls = counter.lock().unwrap();
            *calls += 1;
            if *calls % 3 == 1 {
                Answer::ok(ARTIFACT, 10)
            } else {
                Answer::ok(r#"{"notASet":true}"#, 10)
            }
        }),
        Box::new(|_| Answer::ok(&revised(), 200)),
        Box::new(|_| Answer::ok("{}", 70)),
    );
    let result = run_with(
        &stub,
        &spec(&[ExperimentArm::Fixed, ExperimentArm::Dream]),
        dir.path(),
        agent(true, false),
    )
    .unwrap();
    let first = &result.arms[0].rounds[0];
    let rejected = |row: &pa_dream::experiment::ExperimentRoundRow| row.llm_rejected.total();
    assert_eq!(first.llm_proposals, first.llm_accepted + rejected(first));
    assert_eq!(
        u64::from(first.probes),
        first.llm_accepted + first.local_fallbacks
    );
    assert_eq!(u64::from(first.agent_generated_calls), first.llm_accepted);
    assert!(first.local_fallbacks > 0);
    assert_eq!(
        first.llm_rejected.get(ProposalRejectReason::Shape),
        rejected(first)
    );
    assert_eq!(first.handler_calls.proposer, first.llm_proposals);
    let first_dir = arm_dir(dir.path(), &result, ExperimentArm::Fixed);
    let shared_log = read_rejections(&rejections_path(
        &first_dir,
        &format!("{}-shared", result.experiment_id),
    ))
    .unwrap();
    assert_eq!(shared_log.len() as u64, rejected(first));
    assert_eq!(
        shared_log
            .iter()
            .filter(|line| line.input.fell_back)
            .count() as u64,
        first.local_fallbacks
    );
    assert!(shared_log
        .iter()
        .all(|line| line.input.iteration == 0 && line.input.reason == ProposalRejectReason::Shape));
    assert!(!rejections_path(
        &arm_dir(dir.path(), &result, ExperimentArm::Dream),
        &format!("{}-shared", result.experiment_id)
    )
    .exists());
    for a in &result.arms {
        let second = &a.rounds[1];
        assert_eq!(second.llm_proposals, second.llm_accepted + rejected(second));
        assert_eq!(
            u64::from(second.probes),
            second.llm_accepted + second.local_fallbacks
        );
        assert_eq!(u64::from(second.agent_generated_calls), second.llm_accepted);
        assert_eq!(
            second.cumulative_agent_generated_calls,
            first.agent_generated_calls + second.agent_generated_calls
        );
        assert_eq!(
            a.totals.local_fallbacks,
            first.local_fallbacks + second.local_fallbacks
        );
        assert_eq!(
            a.totals.llm_proposals,
            first.llm_proposals + second.llm_proposals
        );
        assert!(a.totals.agent_generated_calls < a.totals.probes);
        assert_eq!(
            a.run_id,
            dream_run_id(
                "sum-difference",
                &Seed::Number(7),
                FIXED_CLOCK,
                Some(&format!("{}/{}", result.experiment_id, a.arm.as_str()))
            )
        );
        let store = arm_dir(dir.path(), &result, a.arm);
        let arm_log = read_rejections(&rejections_path(&store, &a.run_id)).unwrap();
        assert_eq!(arm_log.len() as u64, rejected(second));
        assert!(arm_log.iter().all(|line| line.input.iteration == 1));
        let tree = read_tree(&second.tree_id, &store).unwrap();
        let count = |origin: &str| {
            tree.nodes
                .iter()
                .filter(|node| node.origin.as_deref() == Some(origin))
                .count() as u64
        };
        assert_eq!(count("llm"), u64::from(second.agent_generated_calls));
        assert_eq!(count("local"), second.local_fallbacks);
    }
}

#[test]
fn the_local_toggles_never_call_the_runner_and_match_the_standalone_runner_arm_for_arm() {
    let failing = |_: &RunAgentRequest, _: &RunAgentOptions| -> RunAgentResult {
        panic!("the runner must not be called on the local path")
    };
    let local_spec = ExperimentSpec {
        rounds: 3,
        ..spec(&[ExperimentArm::Fixed, ExperimentArm::Dream])
    };
    let sync_dir = tempfile::tempdir().unwrap();
    let expected = run_experiment(&local_spec, &run_options(sync_dir.path())).unwrap();
    let agent_dir = tempfile::tempdir().unwrap();
    let (result, spans) = capture(|| {
        run_experiment_with_agent(
            &local_spec,
            &run_options(agent_dir.path()),
            &failing,
            agent(false, false),
            None,
            None,
        )
        .unwrap()
    });
    assert!(result.shared_initial_rollout);
    assert_eq!(
        json::stringify(&result.arms),
        json::stringify(&expected.arms)
    );
    assert_eq!(
        json::stringify(&result.headline),
        json::stringify(&expected.headline)
    );
    for name in &local_spec.arms {
        let tree_id = &arm(&result, *name).rounds[0].tree_id;
        assert_eq!(
            tree_files(&arm_dir(agent_dir.path(), &result, *name), tree_id),
            tree_files(&arm_dir(sync_dir.path(), &expected, *name), tree_id)
        );
    }
    assert_eq!(
        named(&spans, "dream.experiment")[0].attr("dream.mode"),
        Some(&json!("local"))
    );
    let own_dir = tempfile::tempdir().unwrap();
    let own = run_experiment_with_agent(
        &local_spec,
        &run_options(own_dir.path()),
        &failing,
        AgentArmRunnerOptions {
            share_initial_rollout: false,
            ..agent(false, false)
        },
        None,
        None,
    )
    .unwrap();
    assert!(!own.shared_initial_rollout);
    assert_eq!(json::stringify(&own.arms), json::stringify(&expected.arms));
}

#[test]
fn a_guided_arm_without_the_llm_proposer_is_refused_before_anything_is_created() {
    let dir = tempfile::tempdir().unwrap();
    let stub = ArmStub::llm();
    let refused = run_with(
        &stub,
        &spec(&[ExperimentArm::Fixed, ExperimentArm::DreamGuided]),
        dir.path(),
        agent(false, true),
    );
    assert!(matches!(refused, Err(ExperimentError::ArmUnavailable(_))));
    assert_eq!(stub.calls(), 0);
    assert!(!dir.path().join("experiments").exists());

    let mut runner = AgentArmRunner::new(&stub, agent(false, false));
    let plan = plan_experiment(
        &spec(&[ExperimentArm::FixedGuided]),
        &run_options(dir.path()),
        EXPERIMENT_ARMS,
    )
    .unwrap();
    let prepared = runner.prepare(&plan, &fixed_clock);
    assert!(matches!(
        prepared,
        Err(ExperimentError::ArmUnavailable(message)) if message.starts_with("fixed-guided require useLlmProposer")
    ));
    assert_eq!(stub.calls(), 0);
    assert_eq!(runner.mode(&plan.arms[0]), ExperimentArmMode::local());
}

#[test]
fn the_prompt_context_and_the_child_model_reach_every_proposer_call() {
    let dir = tempfile::tempdir().unwrap();
    let stub = ArmStub::llm();
    let result = run_with(
        &stub,
        &spec(&[ExperimentArm::Fixed]),
        dir.path(),
        AgentArmRunnerOptions {
            scope: ChildRuntimeScope {
                model: Some("faux/stub-model".to_string()),
                ..scope()
            },
            proposer_prompt_context: Some("Contract: public examples only.".to_string()),
            ..agent(true, false)
        },
    )
    .unwrap();
    assert_eq!(
        result.arms[0].mode,
        ExperimentArmMode {
            proposer: "llm",
            dreamer: "local",
            model: Some("faux/stub-model".to_string()),
            thinking: None,
            max_output_tokens: None,
        }
    );
    let prompts: Vec<String> = stub
        .tallies
        .lock()
        .unwrap()
        .values()
        .flat_map(|tally| tally.prompts.get("proposer").cloned().unwrap_or_default())
        .collect();
    assert_eq!(prompts.len(), result.arms[0].totals.probes as usize);
    assert!(prompts
        .iter()
        .all(|prompt| prompt.contains("Contract: public examples only.")));
    assert_eq!(result.arms[0].totals.tokens, stub.tokens());
}

fn descends(spans: &[SpanRecord], span: &SpanRecord, root: u64) -> bool {
    let mut parent = span.parent;
    while let Some(id) = parent {
        if id == root {
            return true;
        }
        parent = spans
            .iter()
            .find(|other| other.id == id)
            .and_then(|other| other.parent);
    }
    false
}

#[test]
fn the_experiment_and_every_arm_run_are_detached_roots_linked_through_their_triggers() {
    let dir = tempfile::tempdir().unwrap();
    let stub = ArmStub::llm();
    let (result, spans) = capture(|| {
        let turn = tracing::info_span!("test.turn");
        let _turn = turn.enter();
        let mut progress = |event| stub.progress(event);
        run_experiment_with_agent(
            &spec(&[ExperimentArm::Fixed, ExperimentArm::DreamGuided]),
            &run_options(dir.path()),
            &stub,
            agent(true, true),
            Some(&mut progress),
            Some("4bf92f3577b34da6a3ce929d0e0e4736".to_string()),
        )
        .unwrap()
    });
    let experiment = named(&spans, "dream.experiment")[0];
    assert_eq!(experiment.parent, None);
    for (key, value) in [
        (
            "trigger.trace_id",
            json!("4bf92f3577b34da6a3ce929d0e0e4736"),
        ),
        ("dream.mode", json!("llm")),
        ("dream.arms", json!("fixed,dream-guided")),
    ] {
        assert_eq!(experiment.attr(key), Some(&value), "{key}");
    }
    assert_eq!(experiment.attr("dream.stopped"), None);
    let arms = named(&spans, "dream.experiment_arm");
    assert_eq!(arms.len(), 2);
    assert!(arms.iter().all(|span| span.parent == Some(experiment.id)));
    assert_eq!(
        arms.iter()
            .map(|span| (
                span.attr("dream.fixed_policy").cloned(),
                span.attr("dream.guided").cloned()
            ))
            .collect::<Vec<_>>(),
        vec![
            (Some(json!(true)), Some(json!(false))),
            (Some(json!(false)), Some(json!(true)))
        ]
    );
    assert!(arms.iter().all(|span| span
        .attr("dream.run_id")
        .is_some_and(serde_json::Value::is_string)));
    let explores: Vec<&SpanRecord> = named(&spans, "dream.explore");
    let shared: Vec<&&SpanRecord> = explores
        .iter()
        .filter(|span| span.parent == Some(experiment.id))
        .collect();
    assert_eq!(shared.len(), 1);
    assert_eq!(
        shared[0].attr("dream.tree_id"),
        Some(&json!(result.arms[0].rounds[0].tree_id))
    );
    let runs = named(&spans, "dream.run");
    assert_eq!(runs.len(), 2);
    for run in &runs {
        assert_eq!(run.parent, None);
        let inside: Vec<&SpanRecord> = spans
            .iter()
            .filter(|span| descends(&spans, span, run.id))
            .collect();
        let redeploys: Vec<_> = inside
            .iter()
            .filter(|span| span.name == "dream.redeploy")
            .collect();
        let explores: Vec<_> = inside
            .iter()
            .filter(|span| span.name == "dream.explore")
            .collect();
        assert_eq!((redeploys.len(), explores.len()), (1, 1));
        assert_eq!(explores[0].parent, Some(redeploys[0].id));
        assert_eq!(explores[0].attr("dream.iteration"), Some(&json!(1)));
    }
    let count = |run: &SpanRecord, name: &str| {
        spans
            .iter()
            .filter(|span| span.name == name && descends(&spans, span, run.id))
            .count()
    };
    let fixed = runs
        .iter()
        .find(|run| run.attr("dream.fixed_policy") == Some(&json!(true)))
        .unwrap();
    let guided = runs
        .iter()
        .find(|run| run.attr("dream.fixed_policy") == Some(&json!(false)))
        .unwrap();
    assert_eq!(count(fixed, "dream.llm_guidance"), 0);
    assert_eq!(count(guided, "dream.llm_guidance"), 1);
    assert_eq!(count(guided, "dream.llm_dream"), 1);
}

#[test]
fn a_cancel_before_or_during_the_experiment_writes_no_result_and_marks_it_aborted() {
    let dir = tempfile::tempdir().unwrap();
    let stub = ArmStub::llm();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let (caught, spans) = capture(|| {
        run_with(
            &stub,
            &spec(EXPERIMENT_ARMS),
            dir.path(),
            AgentArmRunnerOptions {
                cancel,
                ..agent(true, true)
            },
        )
    });
    assert!(matches!(caught, Err(ExperimentError::Store(error)) if error.is_abort()));
    assert_eq!(stub.calls(), 0);
    assert_eq!(
        named(&spans, "dream.experiment")[0].attr("dream.stopped"),
        Some(&json!("aborted"))
    );
    assert!(
        !experiment_result_path(dir.path(), &format!("sum-difference-s7-n2-{FIXED_CLOCK}"))
            .exists()
    );

    // A child that reports `aborted` in the second arm stops the whole experiment.
    let mid = tempfile::tempdir().unwrap();
    let started = Arc::new(Mutex::new(0_u32));
    let seen = Arc::clone(&started);
    let aborting = ArmStub::new(
        Box::new(move |_| {
            if *seen.lock().unwrap() >= 2 {
                Answer::status(RunAgentStatus::Aborted, 1)
            } else {
                Answer::ok(ARTIFACT, 10)
            }
        }),
        Box::new(|_| Answer::ok(&revised(), 200)),
        Box::new(|_| Answer::ok("{}", 70)),
    );
    let (mid_caught, mid_spans) = capture(|| {
        let mut progress = |event: ExperimentProgressEvent| {
            if matches!(event, ExperimentProgressEvent::ArmStart { .. }) {
                *started.lock().unwrap() += 1;
            }
            aborting.progress(event);
        };
        run_experiment_with_agent(
            &spec(EXPERIMENT_ARMS),
            &run_options(mid.path()),
            &aborting,
            agent(true, true),
            Some(&mut progress),
            None,
        )
    });
    assert!(matches!(mid_caught, Err(ExperimentError::Store(error)) if error.is_abort()));
    assert!(aborting.tally("dream").is_some());
    assert!(aborting.tally("dream-guided").is_none());
    let experiment = named(&mid_spans, "dream.experiment")[0];
    assert_eq!(experiment.attr("dream.stopped"), Some(&json!("aborted")));
    assert_eq!(experiment.attr("error"), None);
    let runs = named(&mid_spans, "dream.run");
    assert_eq!(
        runs.iter()
            .map(|run| run.attr("dream.stopped").cloned())
            .collect::<Vec<_>>(),
        vec![None, Some(json!("aborted"))]
    );
    assert!(
        !experiment_result_path(mid.path(), &format!("sum-difference-s7-n2-{FIXED_CLOCK}"))
            .exists()
    );
}

#[test]
fn priming_rollouts_are_shared_with_round_1_of_every_arm() {
    let plain_dir = tempfile::tempdir().unwrap();
    let arms = [ExperimentArm::Fixed, ExperimentArm::Dream];
    let plain_stub = ArmStub::llm();
    let plain = run_with(
        &plain_stub,
        &spec(&arms),
        plain_dir.path(),
        agent(true, true),
    )
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let stub = ArmStub::llm();
    let result = run_with(
        &stub,
        &ExperimentSpec {
            priming_policies: PRIMING_DIVERSE.to_vec(),
            ..spec(&arms)
        },
        dir.path(),
        agent(true, true),
    )
    .unwrap();
    let first = &result.arms[0].rounds[0];
    let plain_first = &plain.arms[0].rounds[0];
    let priming_ids: Vec<String> = (0..PRIMING_DIVERSE.len())
        .map(|index| priming_tree_id("sum-difference", &Seed::Number(7), index, FIXED_CLOCK))
        .collect();
    assert_eq!(first.tree_id, plain_first.tree_id);
    assert_eq!(first.priming_tree_ids.as_ref(), Some(&priming_ids));
    let priming_probes = first.priming_probes.unwrap();
    assert!(priming_probes > 0);
    assert_eq!(first.probes, plain_first.probes + priming_probes);
    let shared = stub.tally(SHARED).unwrap();
    assert_eq!(shared.calls.proposer, u64::from(first.probes));
    assert_eq!(first.handler_calls.proposer, u64::from(first.probes));
    assert_eq!(first.agent_generated_calls, first.probes);
    assert_eq!(first.llm_accepted, u64::from(first.probes));
    assert_eq!(first.tokens, u64::from(first.probes) * 10);
    assert_eq!(first.improvements.last().unwrap().score, first.round_best);
    assert!(first.round_best >= plain_first.round_best);
    for a in &result.arms {
        assert_eq!(&a.rounds[0], first);
        let store = arm_dir(dir.path(), &result, a.arm);
        let mut trees: Vec<String> = list_trees(&store)
            .into_iter()
            .map(|tree| tree.tree_id)
            .collect();
        trees.sort();
        let mut expected: Vec<String> = priming_ids.clone();
        expected.push(first.tree_id.clone());
        expected.push(a.rounds[1].tree_id.clone());
        expected.sort();
        assert_eq!(trees, expected);
        for tree_id in &priming_ids {
            assert_eq!(
                tree_files(&store, tree_id),
                tree_files(&arm_dir(dir.path(), &result, ExperimentArm::Fixed), tree_id)
            );
        }
        assert_eq!(a.rounds[1].pool_size, 1 + PRIMING_DIVERSE.len());
        assert_eq!(a.totals.probes, first.probes + a.rounds[1].probes);
    }
    assert_eq!(stub.tally("dream").unwrap().calls.dreamer, 1);
}

#[test]
fn autocorrelation_n32_records_mixed_verdicts_the_final_selection_and_the_dreams_log() {
    let dir = tempfile::tempdir().unwrap();
    let weights: Vec<f64> = (0..32_i32)
        .map(|index| 1.0 + 0.05 * f64::from((16 - index).abs()))
        .collect();
    let artifact = json::stringify(&json!({ "n": 32, "weights": weights }));
    let dreamer_output = json::stringify(&json!([
        support::policy(|p| p.stop_rule = StopRule::Never),
        support::policy(|p| p.selection_rule = SelectionRule::ExploreRoot),
    ]));
    let stub = ArmStub::new(
        Box::new(move |_| Answer::ok(&artifact, 12)),
        Box::new(move |_| Answer::ok(&dreamer_output, 300)),
        Box::new(|_| Answer::ok("{}", 70)),
    );
    let spec = ExperimentSpec {
        n: Some(32),
        ..ExperimentSpec::new(
            DreamTaskId::Autocorrelation,
            Seed::Number(7),
            2,
            ExperimentBudget {
                workers: 3,
                k1: 6,
                k2: 12,
                dreams: 4,
            },
            vec![ExperimentArm::Fixed, ExperimentArm::Dream],
        )
    };
    let result = run_with(&stub, &spec, dir.path(), agent(true, true)).unwrap();
    assert_eq!(
        (result.task.0, result.n),
        (DreamTaskId::Autocorrelation, Some(32))
    );
    assert!(result
        .notes
        .iter()
        .any(|note| note.starts_with("k1 6 <= initialPolicy.beta 6")));
    assert!(is_experiment_result(
        &read_experiment_result(dir.path(), &result.experiment_id).unwrap()
    ));
    let prompts: Vec<String> = stub
        .tallies
        .lock()
        .unwrap()
        .values()
        .flat_map(|tally| tally.prompts.get("proposer").cloned().unwrap_or_default())
        .collect();
    assert!(prompts
        .iter()
        .all(|prompt| prompt.contains("exactly 32 weights")));
    assert!(result.arms.iter().all(|a| a.totals.local_fallbacks == 0));
    let dream = arm(&result, ExperimentArm::Dream);
    let step = dream.rounds[1].dreaming.as_ref().unwrap();
    assert_eq!(step.candidates, 4);
    assert_eq!(step.dreamer, pa_dream::improve::DreamerKind::Mixed);
    assert_eq!(
        step.candidate_verdicts
            .iter()
            .map(|verdict| (verdict.index, verdict.origin))
            .collect::<Vec<_>>(),
        vec![
            (0, CandidateOrigin::Llm),
            (1, CandidateOrigin::Llm),
            (2, CandidateOrigin::Local),
            (3, CandidateOrigin::Local)
        ]
    );
    assert!(step
        .candidate_verdicts
        .iter()
        .all(|verdict| !verdict.changed.is_empty()));
    assert_eq!(
        step.candidate_verdicts
            .iter()
            .filter(|verdict| verdict.reason == pa_dream::improve::CandidateReason::Winner)
            .count(),
        usize::from(step.improved)
    );
    assert!(step.lever_scan.as_ref().unwrap().gap >= 0.0);
    assert_eq!(dream.final_selection.len(), 1);
    assert_eq!(
        dream.final_selection[0].policy_id,
        dream.rounds[1].policy_id
    );
    assert_eq!(dream.stopped_early, 0);
    assert!(arm(&result, ExperimentArm::Fixed)
        .final_selection
        .is_empty());
    let lines = read_dreams_log(&dreams_path(
        &arm_dir(dir.path(), &result, ExperimentArm::Dream),
        &dream.run_id,
    ))
    .unwrap();
    let shape: Vec<(&str, i64)> = lines
        .iter()
        .map(|line| match line {
            DreamsLogLine::Candidate { iteration, .. } => ("candidate", *iteration),
            DreamsLogLine::Step { iteration, .. } => ("step", *iteration),
            DreamsLogLine::Probation { iteration, .. } => ("probation", *iteration),
        })
        .collect();
    assert_eq!(
        shape,
        vec![
            ("candidate", 1),
            ("candidate", 1),
            ("candidate", 1),
            ("candidate", 1),
            ("step", 1),
            ("candidate", -1),
            ("step", -1)
        ]
    );
    let fixed_lines = read_dreams_log(&dreams_path(
        &arm_dir(dir.path(), &result, ExperimentArm::Fixed),
        &arm(&result, ExperimentArm::Fixed).run_id,
    ))
    .unwrap();
    assert_eq!(fixed_lines.len(), 1);
    assert!(matches!(
        fixed_lines[0],
        DreamsLogLine::Step { iteration: -1, .. }
    ));
}

#[test]
fn the_runner_reports_its_modes_prepares_round_1_and_runs_an_arm_on_it() {
    let dir = tempfile::tempdir().unwrap();
    let stub = ArmStub::llm();
    let mut runner = AgentArmRunner::new(&stub, agent(true, false));
    let plan = plan_experiment(
        &spec(EXPERIMENT_ARMS),
        &run_options(dir.path()),
        EXPERIMENT_ARMS,
    )
    .unwrap();
    assert!(plan.arms.iter().all(|a| runner.mode(a)
        == ExperimentArmMode {
            proposer: "llm",
            dreamer: "local",
            ..ExperimentArmMode::local()
        }));
    let shared = runner.prepare(&plan, &fixed_clock).unwrap().unwrap();
    assert_eq!(
        shared.tree_id,
        format!("sum-difference-s7-i0-{FIXED_CLOCK}")
    );
    assert_eq!(
        shared.handler_calls,
        DreamHandlerCalls {
            proposer: u64::from(shared.revealed_count),
            dreamer: 0,
            guidance: 0
        }
    );
    assert_eq!(shared.tokens, u64::from(shared.revealed_count) * 10);
    for a in &plan.arms {
        assert_eq!(
            list_trees(&a.dir)
                .into_iter()
                .map(|tree| tree.tree_id)
                .collect::<Vec<_>>(),
            vec![shared.tree_id.clone()]
        );
    }
    let run = runner
        .run(
            &plan,
            &plan.arms[1],
            &fixed_clock,
            Some(&shared),
            &mut |_| {},
        )
        .unwrap();
    assert!(run.fixed_policy);
    assert_eq!(run.initial_policy_id, policy_id(&DEFAULT_POLICY));
    assert_eq!(run.tree_ids[0], shared.tree_id);
    assert_eq!(run.rounds[0].handler_calls, shared.handler_calls);
}
