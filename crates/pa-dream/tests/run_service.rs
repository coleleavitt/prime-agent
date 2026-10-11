//! The TS `dream-run-service.test.ts` behaviour: the single-slot background
//! service, its status stream, cancellation, the experiment seeds, and the
//! child knobs, against runners that refuse or record every call.

#![allow(clippy::float_cmp)]
#![allow(clippy::too_many_lines)] // the ported TS cases stay whole

mod support;

use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use pa_dream::child::{
    ChildRuntimeScope,
    RunAgent,
    RunAgentOptions,
    RunAgentRequest,
    RunAgentResult,
    RunAgentStatus,
};
use pa_dream::experiment::{ExperimentArm, read_experiment_result};
use pa_dream::llm::{
    DREAMER_PROMPT_HEADER,
    DreamChildRole,
    GUIDANCE_PROMPT_HEADER,
    PROPOSER_PROMPT_HEADER,
};
use pa_dream::rng::{Seed, SeededRng};
use pa_dream::run_service::{
    DREAM_MAX_SEEDS,
    DreamChildOptions,
    DreamExperimentRequest,
    DreamRunKind,
    DreamRunPhase,
    DreamRunRequest,
    DreamRunService,
    DreamRunServiceDeps,
    DreamRunStatus,
    DreamStopReason,
    RoleCappedRunner,
};
use pa_dream::tasks::DreamTaskId;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// A runner that must never be called: the local path spends no token.
struct Refusing;

impl RunAgent for Refusing {
    fn run(&self, _: &RunAgentRequest, _: &RunAgentOptions) -> RunAgentResult {
        panic!("the runner must not be called on the local Dream-RSI path")
    }
}

/// A recording runner whose every answer is rejected (not JSON), so each
/// child call falls back locally.
/// One recorded call: the request, its output cap and its turn cap.
type Call = (RunAgentRequest, Option<u64>, Option<u32>);

#[derive(Default)]
struct Recording {
    calls: Mutex<Vec<Call>>,
}

impl RunAgent for Recording {
    fn run(&self, request: &RunAgentRequest, options: &RunAgentOptions) -> RunAgentResult {
        self.calls.lock().unwrap().push((
            request.clone(),
            options.max_output_tokens,
            options.max_turns,
        ));
        RunAgentResult {
            status: RunAgentStatus::Completed,
            output: "no json here".to_string(),
            stop_reason: None,
            total_tokens: 5,
            output_tokens: 2,
            error: None,
        }
    }
}

type Updates = Arc<Mutex<Vec<DreamRunStatus>>>;

fn service(
    runner: Arc<dyn RunAgent>,
    dir: &Path,
    rng: Option<i64>,
    updates: &Updates,
) -> DreamRunService {
    let sink = Arc::clone(updates);
    DreamRunService::new(DreamRunServiceDeps {
        runner,
        session_model: None,
        dir: dir.to_path_buf(),
        now: Arc::new(|| 1000),
        rng: rng.map(|seed| SeededRng::new(&Seed::Number(seed))),
        llm_experiments: true,
        on_update: Arc::new(move |status| sink.lock().unwrap().push(status.clone())),
    })
}

fn small_run() -> DreamRunRequest {
    DreamRunRequest {
        iterations: Some(1),
        workers: Some(1),
        k1: Some(2),
        k2: Some(4),
        dreams: Some(1),
        seed: Some(5),
        ..DreamRunRequest::new(DreamTaskId::CirclePacking)
    }
}

fn finish(started: pa_dream::run_service::StartedRun) -> Result<DreamRunStatus, String> {
    started.completion.join().expect("the run thread")
}

#[test]
fn the_local_path_never_calls_the_runner_and_emits_ordered_phases() {
    let dir = tempfile::tempdir().unwrap();
    let updates = Updates::default();
    let service = service(Arc::new(Refusing), dir.path(), Some(5), &updates);
    let status = finish(service.start(small_run(), None).unwrap()).unwrap();
    assert_eq!(status.stop_reason, Some(DreamStopReason::Completed));
    assert!(matches!(
        status.phase,
        DreamRunPhase::Accepted | DreamRunPhase::Stopped
    ));
    assert_eq!(status.task, DreamTaskId::CirclePacking);
    assert!(status.final_policy_score.is_some() && status.improved.is_some());
    assert!(!service.running());
    let updates = updates.lock().unwrap();
    let phases: Vec<DreamRunPhase> = updates.iter().map(|update| update.phase).collect();
    assert_eq!(phases[0], DreamRunPhase::Rollout);
    assert!(phases.contains(&DreamRunPhase::Dreaming));
    assert!(phases.contains(&DreamRunPhase::Redeploying));
    assert_eq!(
        updates.last().unwrap().stop_reason,
        Some(DreamStopReason::Completed)
    );
    assert!(updates.iter().all(|update| update.run_id == status.run_id));
    assert_eq!(service.status(), Some(status));
}

#[test]
fn a_cancel_mid_run_settles_cancelled_without_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let cell: Arc<OnceLock<DreamRunService>> = Arc::new(OnceLock::new());
    let canceller = Arc::clone(&cell);
    let service = DreamRunService::new(DreamRunServiceDeps {
        runner: Arc::new(Refusing),
        session_model: None,
        dir: dir.path().to_path_buf(),
        now: Arc::new(|| 1000),
        rng: Some(SeededRng::new(&Seed::Number(5))),
        llm_experiments: true,
        on_update: Arc::new(move |status| {
            if status.phase == DreamRunPhase::Rollout {
                if let Some(service) = canceller.get() {
                    assert!(service.cancel());
                }
            }
        }),
    });
    let _ = cell.set(service.clone());
    let status = finish(
        service
            .start(
                DreamRunRequest {
                    iterations: Some(5),
                    ..small_run()
                },
                None,
            )
            .unwrap(),
    )
    .unwrap();
    assert_eq!(status.stop_reason, Some(DreamStopReason::Cancelled));
    assert_eq!(status.phase, DreamRunPhase::Stopped);
    assert_eq!(status.error, None);
    assert!(!service.running());
    assert!(!service.cancel(), "nothing left to cancel");
}

#[test]
fn a_failure_stops_the_run_with_its_error() {
    let dir = tempfile::tempdir().unwrap();
    let updates = Updates::default();
    let service = service(Arc::new(Refusing), dir.path(), None, &updates);
    let error = finish(
        service
            .start(
                DreamRunRequest {
                    n: Some(1),
                    ..DreamRunRequest::new(DreamTaskId::CirclePacking)
                },
                None,
            )
            .unwrap(),
    )
    .unwrap_err();
    assert!(error.contains("n >= 2"), "{error}");
    let status = service.status().unwrap();
    assert_eq!(status.phase, DreamRunPhase::Stopped);
    assert_eq!(status.error.as_deref(), Some(error.as_str()));
    assert!(!service.running());
}

#[test]
fn the_same_seed_and_clock_give_the_same_scores() {
    let run = || {
        let dir = tempfile::tempdir().unwrap();
        let service = service(Arc::new(Refusing), dir.path(), Some(9), &Updates::default());
        finish(
            service
                .start(
                    DreamRunRequest {
                        iterations: Some(2),
                        workers: Some(2),
                        k1: Some(3),
                        k2: Some(6),
                        dreams: Some(2),
                        seed: Some(9),
                        ..DreamRunRequest::new(DreamTaskId::CirclePacking)
                    },
                    None,
                )
                .unwrap(),
        )
        .unwrap()
    };
    let (first, second) = (run(), run());
    assert_eq!(
        (
            second.final_policy_score,
            second.best_node_score,
            second.improved
        ),
        (
            first.final_policy_score,
            first.best_node_score,
            first.improved
        )
    );
}

fn experiment() -> DreamExperimentRequest {
    DreamExperimentRequest {
        rounds: Some(2),
        arms: Some(vec![ExperimentArm::Dream, ExperimentArm::Fixed]),
        workers: Some(2),
        k1: Some(3),
        k2: Some(6),
        dreams: Some(2),
        seed: Some(5),
        ..DreamExperimentRequest::new(DreamTaskId::SumDifference)
    }
}

#[test]
fn a_local_experiment_reports_arm_and_round_progress_and_its_result() {
    let dir = tempfile::tempdir().unwrap();
    let updates = Updates::default();
    let service = service(Arc::new(Refusing), dir.path(), None, &updates);
    let started = service.start_experiment(experiment(), None).unwrap();
    let initial = service.status().unwrap();
    assert!(initial.run_id.starts_with("dream_"));
    assert_eq!(initial.kind, DreamRunKind::Experiment);
    assert_eq!((initial.rounds, initial.arm_count), (Some(2), Some(2)));
    let status = finish(started).unwrap();
    assert_eq!(status.kind, DreamRunKind::Experiment);
    assert_eq!(status.phase, DreamRunPhase::Stopped);
    assert_eq!(status.stop_reason, Some(DreamStopReason::Completed));
    assert_eq!(status.error, None);
    assert_eq!(
        status.experiment_id.as_deref(),
        Some("sum-difference-s5-n2-1000")
    );
    let result_path = dir
        .path()
        .join("experiments/sum-difference-s5-n2-1000/result.json");
    assert_eq!(
        status.result_path.as_deref(),
        Some(result_path.to_str().unwrap())
    );
    assert!(result_path.exists());
    assert!(!service.running());
    let updates = updates.lock().unwrap();
    let arms: Vec<_> = updates
        .iter()
        .filter(|update| update.round == Some(0) && update.arm.is_some())
        .map(|update| (update.arm, update.arm_index, update.arm_count))
        .collect();
    assert_eq!(
        arms,
        vec![
            (Some(ExperimentArm::Dream), Some(0), Some(2)),
            (Some(ExperimentArm::Fixed), Some(1), Some(2))
        ]
    );
    let rounds: Vec<u32> = updates
        .iter()
        .filter(|update| update.arm == Some(ExperimentArm::Fixed) && update.round.unwrap_or(0) > 0)
        .filter_map(|update| update.round)
        .collect();
    assert!(rounds.contains(&1) && rounds.contains(&2));
    assert!(
        updates
            .iter()
            .all(|update| update.kind == DreamRunKind::Experiment)
    );
    assert_eq!(updates.last().unwrap().result_path, status.result_path);
    assert!(
        updates
            .windows(2)
            .all(|pair| pair[1].best_node_score >= pair[0].best_node_score)
    );
    assert!(!dir.path().join("trees").exists());
    let result = read_experiment_result(dir.path(), "sum-difference-s5-n2-1000").unwrap();
    assert_eq!(
        result["arms"]
            .as_array()
            .unwrap()
            .iter()
            .map(|arm| arm["arm"].clone())
            .collect::<Vec<_>>(),
        vec![json!("dream"), json!("fixed")]
    );
    assert!(
        result["arms"]
            .as_array()
            .unwrap()
            .iter()
            .all(|arm| arm["totals"]["tokens"] == json!(0))
    );
}

#[test]
fn runs_and_experiments_share_one_slot() {
    let dir = tempfile::tempdir().unwrap();
    let service = service(Arc::new(Refusing), dir.path(), None, &Updates::default());
    let started = service.start_experiment(experiment(), None).unwrap();
    let refused = service
        .start(DreamRunRequest::new(DreamTaskId::CirclePacking), None)
        .err()
        .unwrap();
    assert_eq!(
        refused,
        format!("Dream-RSI run {} is already in progress", started.run_id)
    );
    finish(started).unwrap();
    let run = finish(
        service
            .start(
                DreamRunRequest {
                    iterations: Some(0),
                    ..small_run()
                },
                None,
            )
            .unwrap(),
    )
    .unwrap();
    assert_eq!(run.kind, DreamRunKind::Run);
    assert_eq!(run.experiment_id, None);
}

#[test]
fn guided_arms_need_the_llm_proposer_and_llm_arms_need_an_llm_runner() {
    let dir = tempfile::tempdir().unwrap();
    let service = service(Arc::new(Refusing), dir.path(), None, &Updates::default());
    let error = finish(
        service
            .start_experiment(
                DreamExperimentRequest {
                    arms: Some(vec![ExperimentArm::DreamGuided]),
                    ..experiment()
                },
                None,
            )
            .unwrap(),
    )
    .unwrap_err();
    assert_eq!(error, "dream-guided/fixed-guided require llmProposer");
    assert_eq!(service.status().unwrap().phase, DreamRunPhase::Stopped);
    assert_eq!(
        service.status().unwrap().error.as_deref(),
        Some(error.as_str())
    );

    let without = DreamRunService::new(DreamRunServiceDeps {
        runner: Arc::new(Refusing),
        session_model: None,
        dir: dir.path().to_path_buf(),
        now: Arc::new(|| 1000),
        rng: None,
        llm_experiments: false,
        on_update: Arc::new(|_| {}),
    });
    let error = finish(
        without
            .start_experiment(
                DreamExperimentRequest {
                    llm_proposer: true,
                    ..experiment()
                },
                None,
            )
            .unwrap(),
    )
    .unwrap_err();
    assert!(error.contains("LLM experiment arms need an in-session LLM arm runner"));
    assert!(!dir.path().join("experiments").exists());
    assert!(!without.running());
}

#[test]
fn a_cancel_at_the_first_arm_settles_cancelled_and_writes_no_result() {
    let dir = tempfile::tempdir().unwrap();
    let cell: Arc<OnceLock<DreamRunService>> = Arc::new(OnceLock::new());
    let canceller = Arc::clone(&cell);
    let service = DreamRunService::new(DreamRunServiceDeps {
        runner: Arc::new(Refusing),
        session_model: None,
        dir: dir.path().to_path_buf(),
        now: Arc::new(|| 1000),
        rng: None,
        llm_experiments: true,
        on_update: Arc::new(move |status| {
            if status.arm.is_some() {
                if let Some(service) = canceller.get() {
                    service.cancel();
                }
            }
        }),
    });
    let _ = cell.set(service.clone());
    let status = finish(
        service
            .start_experiment(
                DreamExperimentRequest {
                    rounds: Some(4),
                    ..experiment()
                },
                None,
            )
            .unwrap(),
    )
    .unwrap();
    assert_eq!(status.stop_reason, Some(DreamStopReason::Cancelled));
    assert_eq!(status.phase, DreamRunPhase::Stopped);
    assert_eq!((status.error, status.result_path), (None, None));
    assert!(!service.running());
    assert!(
        !dir.path()
            .join("experiments/sum-difference-s5-n4-1000/result.json")
            .exists()
    );
}

#[test]
fn the_role_capped_runner_applies_each_roles_default_cap_per_call() {
    let recording = Arc::new(Recording::default());
    let capped = RoleCappedRunner::new(
        Arc::clone(&recording) as Arc<dyn RunAgent>,
        DreamTaskId::PythonSpeedup,
        &DreamChildOptions::default(),
    );
    let options = RunAgentOptions {
        max_turns: Some(8),
        token_budget: 1000,
        max_output_tokens: Some(8192),
        cancel: CancellationToken::new(),
    };
    for prompt in [
        format!("{PROPOSER_PROMPT_HEADER}\np"),
        format!("{DREAMER_PROMPT_HEADER}\nd"),
        format!("{GUIDANCE_PROMPT_HEADER}\ng"),
        "unrelated".to_string(),
    ] {
        capped.run(
            &RunAgentRequest {
                prompt,
                model: None,
                thinking_level: None,
            },
            &options,
        );
    }
    assert_eq!(
        recording
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|call| call.1)
            .collect::<Vec<_>>(),
        vec![Some(8192), Some(4096), Some(2048), Some(8192)]
    );
    assert_eq!(DreamChildRole::of_prompt("# something else"), None);
    // An explicit cap is on the scope for every role: nothing is rewritten.
    let explicit = Arc::new(Recording::default());
    let passthrough = RoleCappedRunner::new(
        Arc::clone(&explicit) as Arc<dyn RunAgent>,
        DreamTaskId::PythonSpeedup,
        &DreamChildOptions {
            max_output_tokens: Some(300),
            ..DreamChildOptions::default()
        },
    );
    passthrough.run(
        &RunAgentRequest {
            prompt: format!("{DREAMER_PROMPT_HEADER}\nd"),
            model: None,
            thinking_level: None,
        },
        &RunAgentOptions {
            max_output_tokens: Some(300),
            ..options
        },
    );
    assert_eq!(explicit.calls.lock().unwrap()[0].1, Some(300));
}

fn llm_experiment(child: DreamChildOptions, dreamer: bool) -> DreamExperimentRequest {
    DreamExperimentRequest {
        rounds: Some(2),
        arms: Some(vec![if dreamer {
            ExperimentArm::DreamGuided
        } else {
            ExperimentArm::Dream
        }]),
        workers: Some(1),
        k1: Some(2),
        k2: Some(4),
        dreams: Some(1),
        seed: Some(3),
        llm_proposer: true,
        llm_dreamer: dreamer,
        child,
        ..DreamExperimentRequest::new(DreamTaskId::SumDifference)
    }
}

#[test]
fn every_child_role_runs_with_thinking_off_and_its_own_cap_through_the_llm_runner() {
    let dir = tempfile::tempdir().unwrap();
    let recording = Arc::new(Recording::default());
    let service = service(
        Arc::clone(&recording) as Arc<dyn RunAgent>,
        dir.path(),
        None,
        &Updates::default(),
    );
    let status = finish(
        service
            .start_experiment(llm_experiment(DreamChildOptions::default(), true), None)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(status.stop_reason, Some(DreamStopReason::Completed));
    assert!(status.tokens.is_some());
    let calls = recording.calls.lock().unwrap();
    let mut roles: Vec<&str> = calls
        .iter()
        .map(|call| match DreamChildRole::of_prompt(&call.0.prompt) {
            Some(DreamChildRole::Proposer) => "proposer",
            Some(DreamChildRole::Dreamer) => "dreamer",
            Some(DreamChildRole::Guidance) => "guidance",
            None => "unknown",
        })
        .collect();
    for (role, (request, cap, turns)) in roles.iter().zip(calls.iter()) {
        assert_eq!(request.thinking_level.as_deref(), Some("off"));
        assert_eq!(*turns, Some(8));
        let expected = match *role {
            "guidance" => 2048,
            _ => 4096,
        };
        assert_eq!(*cap, Some(expected), "{role}");
    }
    roles.sort_unstable();
    roles.dedup();
    assert_eq!(roles, vec!["dreamer", "guidance", "proposer"]);
}

#[test]
fn an_explicit_model_thinking_and_cap_reach_every_role_and_the_arm_mode() {
    let dir = tempfile::tempdir().unwrap();
    let recording = Arc::new(Recording::default());
    let service = service(
        Arc::clone(&recording) as Arc<dyn RunAgent>,
        dir.path(),
        None,
        &Updates::default(),
    );
    let status = finish(
        service
            .start_experiment(
                llm_experiment(
                    DreamChildOptions {
                        model: Some("faux/child".to_string()),
                        thinking: Some("low".to_string()),
                        max_output_tokens: Some(777),
                    },
                    false,
                ),
                None,
            )
            .unwrap(),
    )
    .unwrap();
    let calls = recording.calls.lock().unwrap();
    assert!(!calls.is_empty());
    for (request, cap, _) in calls.iter() {
        assert_eq!(request.model.as_deref(), Some("faux/child"));
        assert_eq!(request.thinking_level.as_deref(), Some("low"));
        assert_eq!(*cap, Some(777));
    }
    let result =
        read_experiment_result(dir.path(), status.experiment_id.as_deref().unwrap()).unwrap();
    assert_eq!(
        result["arms"][0]["mode"],
        json!({"proposer": "llm", "dreamer": "local", "model": "faux/child", "thinking": "low", "maxOutputTokens": 777})
    );
}

fn seeds_request(seeds: Vec<u64>) -> DreamExperimentRequest {
    DreamExperimentRequest {
        seed: None,
        seeds: Some(seeds),
        ..experiment()
    }
}

#[test]
fn several_seeds_run_under_one_run_id_with_one_result_each() {
    assert_eq!(DREAM_MAX_SEEDS, 16);
    let dir = tempfile::tempdir().unwrap();
    let updates = Updates::default();
    let service = service(Arc::new(Refusing), dir.path(), None, &updates);
    assert_eq!(
        service
            .start_experiment(
                DreamExperimentRequest {
                    seed: Some(1),
                    seeds: Some(vec![5, 6]),
                    ..experiment()
                },
                None
            )
            .err(),
        Some("dream experiment takes either seed or seeds, not both".to_string())
    );
    let started = service
        .start_experiment(seeds_request(vec![5, 6, 7]), None)
        .unwrap();
    let initial = service.status().unwrap();
    assert_eq!(
        (initial.seed, initial.seed_index, initial.seed_count),
        (Some(5), Some(0), Some(3))
    );
    let status = finish(started).unwrap();
    assert_eq!(status.stop_reason, Some(DreamStopReason::Completed));
    assert_eq!(status.run_id, initial.run_id);
    let updates = updates.lock().unwrap();
    assert!(updates.iter().all(|update| update.run_id == initial.run_id));
    let expected: Vec<String> = [5, 6, 7]
        .iter()
        .map(|seed| {
            dir.path()
                .join(format!(
                    "experiments/sum-difference-s{seed}-n2-1000/result.json"
                ))
                .display()
                .to_string()
        })
        .collect();
    assert_eq!(status.result_paths.as_ref(), Some(&expected));
    assert_eq!(status.result_path.as_ref(), Some(&expected[2]));
    assert_eq!(
        status.experiment_id.as_deref(),
        Some("sum-difference-s7-n2-1000")
    );
    assert_eq!(
        (status.seed, status.seed_index, status.seed_count),
        (Some(7), Some(2), Some(3))
    );
    assert_eq!(status.tokens, None);
    assert!(expected.iter().all(|path| Path::new(path).exists()));
    let indexes: Vec<usize> = updates
        .iter()
        .filter_map(|update| update.seed_index)
        .collect();
    assert!(indexes.windows(2).all(|pair| pair[1] >= pair[0]));
    let mut distinct = indexes;
    distinct.dedup();
    assert_eq!(distinct, vec![0, 1, 2]);
    let mut counts: Vec<usize> = updates
        .iter()
        .filter_map(|update| update.result_paths.as_ref().map(Vec::len))
        .collect();
    counts.dedup();
    assert_eq!(counts, vec![1, 2, 3]);
    let trees: std::collections::HashSet<Value> = [5, 6, 7]
        .iter()
        .map(|seed| {
            read_experiment_result(dir.path(), &format!("sum-difference-s{seed}-n2-1000")).unwrap()
                ["arms"][0]["rounds"][0]["treeId"]
                .clone()
        })
        .collect();
    assert_eq!(trees.len(), 3);
}

#[test]
fn a_cancel_after_the_first_seed_keeps_its_result_and_stops() {
    let dir = tempfile::tempdir().unwrap();
    let cell: Arc<OnceLock<DreamRunService>> = Arc::new(OnceLock::new());
    let canceller = Arc::clone(&cell);
    let service = DreamRunService::new(DreamRunServiceDeps {
        runner: Arc::new(Refusing),
        session_model: None,
        dir: dir.path().to_path_buf(),
        now: Arc::new(|| 1000),
        rng: None,
        llm_experiments: true,
        on_update: Arc::new(move |status| {
            if status
                .result_paths
                .as_ref()
                .is_some_and(|paths| paths.len() == 1)
            {
                if let Some(service) = canceller.get() {
                    service.cancel();
                }
            }
        }),
    });
    let _ = cell.set(service.clone());
    let status = finish(
        service
            .start_experiment(seeds_request(vec![5, 6, 7]), None)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(status.stop_reason, Some(DreamStopReason::Cancelled));
    assert_eq!(status.phase, DreamRunPhase::Stopped);
    let first = dir
        .path()
        .join("experiments/sum-difference-s5-n2-1000/result.json");
    assert_eq!(status.result_paths, Some(vec![first.display().to_string()]));
    assert!(first.exists());
    assert!(
        !dir.path()
            .join("experiments/sum-difference-s6-n2-1000/result.json")
            .exists()
    );
    assert!(
        !dir.path()
            .join("experiments/sum-difference-s7-n2-1000")
            .exists()
    );
}

#[test]
fn the_status_serializes_with_the_ts_keys_and_omits_what_is_unset() {
    let dir = tempfile::tempdir().unwrap();
    let service = service(Arc::new(Refusing), dir.path(), Some(1), &Updates::default());
    let status = finish(
        service
            .start(
                DreamRunRequest {
                    iterations: Some(0),
                    ..small_run()
                },
                None,
            )
            .unwrap(),
    )
    .unwrap();
    let value = serde_json::to_value(&status).unwrap();
    let keys: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        vec![
            "runId",
            "phase",
            "task",
            "iteration",
            "bestNodeScore",
            "finalPolicyScore",
            "improved",
            "stopReason",
            "startedAt",
            "updatedAt",
            "kind"
        ]
    );
    assert_eq!(value["task"], json!("circle-packing"));
    assert_eq!(value["kind"], json!("run"));
    assert_eq!(value["stopReason"], json!("completed"));
    let _ = ChildRuntimeScope::default();
}
