//! Replays the TS goldens (`tests/fixtures/golden/*.json`, written by
//! `generate.ts` from the TS sources) and compares the Rust results as
//! serialized JSON text, so key order counts as well as values.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use pa_core::refinement::executor::RefinerFn;
use pa_core::refinement::planner::normalize_refinement_proposal;
use pa_ledger::{FailureRecord, ReplayCase};
use pa_ravo::{
    authorize_assisted_ravo, canonical_json, normalize_assisted_ravo_state, parse_judge_verdict,
    ravo_artifact_digest, ravo_evaluate_proposal, ravo_fast_screen, ravo_mark_provisional,
    ravo_observe_champion, ravo_step, sha256_hex, AssistedRavoObservation, AuthorityInput,
    GateEvaluation, GateStatus, RavoConfig, RavoEvaluation, RavoProposal, RavoState,
    RavoWindowClock, RefereeVerdict, RefineKind, ReplayEnvironment, ReplayOutcome, ReplayRunner,
    UnclaimedCommitPolicy, WindowSpan, DEFAULT_RAVO_OBSERVATION_WINDOW_TURNS, RAVO_DEFAULT_CONFIG,
};
use pa_types::ai::{AssistantContentBlock, AssistantMessage, Model, StopReason, TextContent};
use serde::de::DeserializeOwned;
use serde_json::{json, Value};

fn fixture(name: &str) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/golden")
        .join(name);
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

fn parse<T: DeserializeOwned>(value: &Value) -> T {
    serde_json::from_value(value.clone()).unwrap_or_else(|error| panic!("{error}: {value}"))
}

/// The same JSON text, key order included.
fn assert_json(actual: &impl serde::Serialize, expected: &Value, context: &str) {
    assert_eq!(
        serde_json::to_string_pretty(actual).unwrap(),
        serde_json::to_string_pretty(expected).unwrap(),
        "{context}"
    );
}

#[test]
fn the_reducer_steps_like_the_ts_reducer() {
    let golden = fixture("reducer.json");
    for step in golden["steps"].as_array().unwrap() {
        let name = step["name"].as_str().unwrap();
        let state: RavoState = parse(&step["state"]);
        let proposal: RavoProposal = parse(&step["proposal"]);
        let evaluation: RavoEvaluation = parse(&step["evaluation"]);
        let config: RavoConfig = parse(&step["config"]);
        let result = ravo_step(&state, &proposal, &evaluation, &config);
        assert_json(&result, &step["result"], name);
    }
    let provisional = &golden["provisional"];
    let last = golden["steps"].as_array().unwrap().last().unwrap();
    let state: RavoState = parse(&last["result"]["state"]);
    let claimed: Vec<String> = ["b2", "a1", "b2", "", "A1"]
        .into_iter()
        .map(str::to_string)
        .collect();
    let marked = ravo_mark_provisional(
        &state,
        "p1",
        &claimed,
        Some(WindowSpan {
            committed_turn: 40,
            until_turn: 60,
            clock: Some(RavoWindowClock::Ordinal),
        }),
    );
    assert_json(&marked, &provisional["marked"], "marked");
    let observe = |ids: &[&str], turn: u64| {
        let ids: Vec<String> = ids.iter().map(|id| (*id).to_string()).collect();
        let (state, regression) = ravo_observe_champion(&marked, "p1", &ids, turn);
        json!({ "state": state, "regression": regression })
    };
    assert_json(
        &observe(&["a1", "zz"], 47),
        &provisional["inside"],
        "inside",
    );
    assert_json(&observe(&["a1"], 61), &provisional["outside"], "outside");
    assert_json(
        &observe(&["zz"], 47),
        &provisional["unclaimed"],
        "unclaimed",
    );
    assert_json(
        &ravo_mark_provisional(&state, "nope", &["x".to_string()], None),
        &provisional["unknownChampion"],
        "unknown champion",
    );
    assert_json(
        &ravo_mark_provisional(
            &state,
            "p8",
            &["x".to_string()],
            Some(WindowSpan {
                committed_turn: 9,
                until_turn: 3,
                clock: None,
            }),
        ),
        &provisional["invalidWindow"],
        "invalid window",
    );
}

fn status(value: &Value) -> GateStatus {
    parse(value)
}

fn strings(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn the_authority_decides_like_the_ts_authority() {
    for case in fixture("authority.json").as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let input = &case["input"];
        let observation = &input["observation"];
        let state: Option<RavoState> = input.get("state").map(parse);
        let failure_opponents = strings(&input["failureOpponents"]);
        let verdicts: Vec<RefereeVerdict> =
            input.get("refereeVerdicts").map(parse).unwrap_or_default();
        let unclaimed_commit = match input["unclaimedCommit"].as_str() {
            Some("unmeasured") => UnclaimedCommitPolicy::Unmeasured,
            Some("reject") => UnclaimedCommitPolicy::Reject,
            _ => UnclaimedCommitPolicy::Measured,
        };
        let result = authorize_assisted_ravo(&AuthorityInput {
            proposal_id: input["proposalId"].as_str().unwrap(),
            artifact: &input["artifact"],
            baseline: &input["baseline"],
            fast_score: input["fastScore"].as_u64().unwrap(),
            observation: AssistedRavoObservation {
                status: status(&observation["status"]),
                score: observation["score"].as_u64(),
                detail: observation["detail"].as_str().map(str::to_string),
                failed_criteria: observation.get("failedCriteria").map(strings),
                addressed_fingerprints: strings(&observation["addressedFingerprints"]),
            },
            state: state.as_ref(),
            config: RavoConfig {
                screen_threshold: input["screenThreshold"].as_u64().unwrap_or(50),
                epsilon: input["epsilon"].as_u64().unwrap_or(1),
                deep_tolerance: input["deepTolerance"].as_u64().unwrap_or(0),
            },
            failure_opponents: &failure_opponents,
            referee_verdicts: &verdicts,
            turn: input["turn"].as_u64(),
            turn_clock: input.get("turnClock").map(parse),
            observation_window_turns: input["observationWindowTurns"]
                .as_u64()
                .unwrap_or(DEFAULT_RAVO_OBSERVATION_WINDOW_TURNS),
            unclaimed_commit,
        });
        assert_json(&result, &case["result"], name);
    }
}

#[test]
fn stored_states_normalize_like_the_ts_authority() {
    for (index, case) in fixture("normalize.json")
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        let input = (!case["input"].is_null()).then_some(&case["input"]);
        assert_json(
            &normalize_assisted_ravo_state(input),
            &case["output"],
            &format!("case {index}"),
        );
    }
}

#[test]
fn digests_match_the_ts_canonical_json() {
    for case in fixture("digests.json").as_array().unwrap() {
        let value = &case["value"];
        assert_eq!(canonical_json(value), case["canonical"].as_str().unwrap());
        assert_eq!(
            sha256_hex(&canonical_json(value)),
            case["sha256"].as_str().unwrap()
        );
        assert_eq!(
            ravo_artifact_digest(value),
            case["artifactDigest"].as_str().unwrap()
        );
    }
}

#[test]
fn judge_tokens_and_screens_match_the_ts_gate() {
    let golden = fixture("judge.json");
    for case in golden["verdicts"].as_array().unwrap() {
        assert_eq!(
            parse_judge_verdict(Some(&case["value"])),
            status(&case["verdict"]),
            "{}",
            case["value"]
        );
    }
    for case in golden["screens"].as_array().unwrap() {
        let edits = usize::try_from(case["edits"].as_u64().unwrap()).unwrap();
        let proposal = normalize_refinement_proposal(&json!({ "edits": vec![json!({}); edits] }));
        let valid = usize::try_from(case["valid"].as_u64().unwrap()).unwrap();
        assert_eq!(
            ravo_fast_screen(&proposal, valid),
            case["score"].as_u64().unwrap(),
            "{case}"
        );
    }
}

/// A runner with no interpreter, as the TS generator ran.
struct NoInterpreter;

impl ReplayRunner for NoInterpreter {
    fn run<'a>(
        &'a self,
        _case: &'a ReplayCase,
        _environment: ReplayEnvironment,
        _sys_path: &'a [String],
    ) -> Pin<Box<dyn Future<Output = ReplayOutcome> + Send + 'a>> {
        Box::pin(async {
            ReplayOutcome::Unrunnable {
                detail: "no kernel python: the replay case could not be executed".to_string(),
            }
        })
    }
}

fn model() -> Model {
    serde_json::from_value(json!({
        "id": "judge", "name": "Judge", "api": "test", "provider": "test",
        "baseUrl": "http://localhost", "reasoning": false, "input": ["text"],
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 100_000, "maxTokens": 8000
    }))
    .unwrap()
}

type Requests = Arc<Mutex<Vec<Value>>>;

fn scripted_judge(reply: &Value, requests: &Requests) -> RefinerFn {
    let reply = reply.clone();
    let requests = Arc::clone(requests);
    Box::new(move |model, system, prompt| {
        requests.lock().unwrap().push(json!({
            "systemPrompt": system,
            "prompt": prompt,
            "maxTokens": model.max_tokens,
        }));
        Box::pin(async move {
            let (stop_reason, error_message, text) = match reply.get("error") {
                Some(error) => (StopReason::Error, error.as_str().map(str::to_string), None),
                None => (
                    StopReason::Stop,
                    None,
                    reply["text"].as_str().map(str::to_string),
                ),
            };
            Ok(AssistantMessage {
                content: text
                    .map(|text| {
                        vec![AssistantContentBlock::Text(TextContent {
                            text,
                            text_signature: None,
                            rest: serde_json::Map::default(),
                        })]
                    })
                    .unwrap_or_default(),
                api: "test".to_string(),
                provider: "test".to_string(),
                model: "judge".to_string(),
                response_model: None,
                response_id: None,
                diagnostics: None,
                usage: pa_types::ai::Usage::default(),
                stop_reason,
                stop_reason_raw: None,
                error_message,
                timestamp: 0,
                rest: serde_json::Map::default(),
            })
        })
    })
}

#[tokio::test]
async fn the_gate_reports_like_the_ts_gate() {
    for case in fixture("gate.json").as_array().unwrap() {
        let name = case["name"].as_str().unwrap();
        let options = &case["options"];
        let proposal = normalize_refinement_proposal(&case["proposal"]);
        let state: RavoState = parse(&options["state"]);
        let recurring: Vec<FailureRecord> = options
            .get("recurringFailures")
            .map(parse)
            .unwrap_or_default();
        let refine_kind = match options["refineKind"].as_str() {
            Some("failure") => RefineKind::Failure,
            Some("checkpoint") => RefineKind::Checkpoint,
            _ => RefineKind::Directed,
        };
        let requests: Requests = Arc::default();
        let report = ravo_evaluate_proposal(
            GateEvaluation {
                proposal: &proposal,
                proposal_id: options["proposalId"].as_str().unwrap(),
                state: &state,
                config: RAVO_DEFAULT_CONFIG,
                conversation_text: options["conversationText"].as_str().unwrap().to_string(),
                harness_overview: options["harnessOverview"].as_str().unwrap().to_string(),
                baseline: options["baseline"].clone(),
                recurring_failures: &recurring,
                turn: options["turn"].as_u64(),
                turn_clock: options.get("turnClock").map(parse),
                refine_kind,
                model: model(),
                runner: &NoInterpreter,
                sys_path: &[],
            },
            scripted_judge(&case["reply"], &requests),
        )
        .await;
        assert_json(&report, &case["report"], name);
        assert_json(&*requests.lock().unwrap(), &case["requests"], name);
    }
}
