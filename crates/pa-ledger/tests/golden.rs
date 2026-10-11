//! Parity with the TS product: every scenario in `fixtures/golden/` was run
//! through the fork's TS sources under node (`fixtures/golden/generate.ts`),
//! and each is replayed here with whole-value (and exact-byte) comparisons.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use pa_agent::types::AgentMessage;
use pa_ledger::{
    DEFAULT_PROMPT_LIMIT,
    FailureFingerprint,
    FailureKind,
    FailureLedger,
    FailureObservation,
    FailureRecord,
    FileResolutionStore,
    HarnessDocument,
    ProvisionalRegression,
    ReplayVerification,
    ResolutionCell,
    ResolutionIndex,
    ResolutionIndexOptions,
    ResolutionStore,
    apply_replay_verifications,
    derive_replay_case,
    extract_failures,
    find_provisional_regressions,
    fingerprint_failure,
    fingerprint_tool_result_text,
    format_failure_ledger_for_prompt,
    format_recurrence_refine_instructions,
    format_regression_refine_instructions,
    merge_failure_observations,
    normalize_failure_ledger,
    normalize_failure_message,
    normalize_replay_cases,
    observation_ordinal,
    parse_python_traceback,
    record_provisional_regressions,
    recurring_failures,
    replay_probe_of,
    resolution_store_path,
    update_failure_ledger,
};
use serde_json::{Value, json};

fn fixture(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/golden")
        .join(name);
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap()
}

/// `JSON.stringify(value, null, 2) + "\n"`.
fn text(value: &impl serde::Serialize) -> String {
    format!("{}\n", serde_json::to_string_pretty(value).unwrap())
}

fn str_of(value: &Value) -> Option<&str> {
    value.as_str()
}

fn kind(value: &Value) -> FailureKind {
    FailureKind::from_wire(value.as_str().unwrap()).unwrap()
}

#[test]
fn fingerprints_and_normalized_messages_match_the_ts_ids() {
    for case in fixture("fingerprints.json").as_array().unwrap() {
        let raw = case["raw"].as_str().unwrap();
        assert_eq!(
            normalize_failure_message(raw),
            case["normalized"].as_str().unwrap(),
            "{raw:?}"
        );
        let expected: FailureFingerprint =
            serde_json::from_value(case["fingerprint"].clone()).unwrap();
        let actual = fingerprint_failure(
            kind(&case["kind"]),
            str_of(&case["source"]),
            str_of(&case["exceptionClass"]),
            raw,
        );
        assert_eq!(actual, expected, "{raw:?}");
        assert_eq!(serde_json::to_value(&actual).unwrap(), case["fingerprint"]);
    }
}

#[test]
fn tracebacks_parse_like_the_ts_parser() {
    for case in fixture("tracebacks.json").as_array().unwrap() {
        let input = case["text"].as_str().unwrap();
        let parsed = parse_python_traceback(input).map(|parsed| {
            let mut value = json!({
                "exceptionClass": parsed.exception_class,
                "message": parsed.message,
                "excerpt": parsed.excerpt,
            });
            if let Some(skill) = parsed.skill_name {
                value["skillName"] = Value::from(skill);
            }
            value
        });
        assert_eq!(parsed.unwrap_or(Value::Null), case["parsed"], "{input:?}");
        let as_result = fingerprint_tool_result_text(Some("ipython"), input, false);
        assert_eq!(
            serde_json::to_value(as_result).unwrap(),
            case["asToolResult"]
        );
        let as_error = fingerprint_tool_result_text(Some("bash"), input, true);
        assert_eq!(serde_json::to_value(as_error).unwrap(), case["asError"]);
    }
}

#[test]
fn extraction_matches_extract_failures() {
    let golden = fixture("extract.json");
    let messages: Vec<AgentMessage> = serde_json::from_value(golden["messages"].clone()).unwrap();
    for run in golden["runs"].as_array().unwrap() {
        let tick = AtomicU64::new(0);
        let now = || {
            format!(
                "2026-01-01T00:00:0{}.000Z",
                tick.fetch_add(1, Ordering::SeqCst) % 10
            )
        };
        let observations = extract_failures(
            &messages,
            run["fromEntryIndex"].as_u64().unwrap(),
            run["turn"].as_u64().unwrap(),
            &now,
        );
        assert_eq!(
            serde_json::to_value(&observations).unwrap(),
            run["observations"]
        );
    }
}

#[test]
fn ledger_updates_write_the_ts_bytes() {
    let mut ledger = FailureLedger::default();
    for update in fixture("ledger-updates.json").as_array().unwrap() {
        let step = &update["step"];
        let observations: Vec<FailureObservation> =
            serde_json::from_value(step["observations"].clone()).unwrap();
        let threshold = step["threshold"].as_u64();
        let result = match step["mode"].as_str().unwrap() {
            "update" => update_failure_ledger(
                &ledger,
                &observations,
                threshold,
                step["scannedThroughEntryIndex"].as_u64(),
            ),
            _ => merge_failure_observations(&ledger, &observations, threshold),
        };
        ledger = result.ledger;
        assert_eq!(
            text(&ledger),
            update["ledgerText"].as_str().unwrap(),
            "{step}"
        );
        let ids = |records: &[FailureRecord]| -> Value {
            records
                .iter()
                .map(|record| record.fingerprint.id.clone())
                .collect()
        };
        assert_eq!(
            ids(&result.newly_recurring),
            update["newlyRecurring"],
            "{step}"
        );
        assert_eq!(ids(&recurring_failures(&ledger, None)), update["recurring"]);
        assert_eq!(
            ids(&recurring_failures(&ledger, Some(1))),
            update["recurringAt1"]
        );
        assert_eq!(
            observation_ordinal(Some(&ledger)),
            update["ordinal"].as_u64().unwrap()
        );
        let actionable: serde_json::Map<String, Value> = ledger
            .failures
            .iter()
            .map(|(id, record)| (id.clone(), Value::from(record.is_actionable())))
            .collect();
        assert_eq!(Value::Object(actionable), update["actionable"]);
    }
}

#[test]
fn stored_ledgers_normalize_like_the_ts_loader() {
    for case in fixture("normalize.json").as_array().unwrap() {
        let ledger = normalize_failure_ledger(&case["raw"]);
        assert_eq!(
            text(&ledger),
            case["ledgerText"].as_str().unwrap(),
            "{}",
            case["raw"]
        );
    }
}

#[test]
fn prompt_blocks_and_refine_instructions_match() {
    let golden = fixture("prompt.json");
    let updates = fixture("ledger-updates.json");
    let last = updates.as_array().unwrap().last().unwrap();
    let ledger = normalize_failure_ledger(
        &serde_json::from_str::<Value>(last["ledgerText"].as_str().unwrap()).unwrap(),
    );
    let verified = apply_replay_verifications(
        &ledger,
        &[ReplayVerification {
            fingerprint_id: "d".to_string(),
            source: "import numpy".to_string(),
            verified_at: "v1".to_string(),
        }],
    );
    assert_eq!(text(&verified), golden["ledgerText"].as_str().unwrap());
    let all: Vec<FailureRecord> = verified.failures.values().cloned().collect();
    let expect = |key: &str| golden[key].as_str().unwrap().to_string();
    assert_eq!(
        format_failure_ledger_for_prompt(&all, DEFAULT_PROMPT_LIMIT),
        expect("limit12")
    );
    assert_eq!(format_failure_ledger_for_prompt(&all, 2), expect("limit2"));
    assert_eq!(format_failure_ledger_for_prompt(&all, 0), expect("limit0"));
    assert_eq!(
        format_failure_ledger_for_prompt(&[], DEFAULT_PROMPT_LIMIT),
        expect("empty")
    );
    assert_eq!(
        format_recurrence_refine_instructions(&all),
        expect("recurrence")
    );
    let regressions: Vec<ProvisionalRegression> =
        serde_json::from_value(golden["regressions"].clone()).unwrap();
    assert_eq!(
        format_regression_refine_instructions(&regressions, &all),
        expect("regression")
    );
    let mut long = all[0].clone();
    long.excerpt = format!("  {}  ", "word\n\t".repeat(80));
    long.fingerprint.source = Some(String::new());
    long.fingerprint.exception_class = Some(String::new());
    assert_eq!(
        format_failure_ledger_for_prompt(&[long], DEFAULT_PROMPT_LIMIT),
        expect("longExcerpt")
    );
}

#[test]
fn actionability_classifies_like_the_ts_rules() {
    for case in fixture("actionability.json").as_array().unwrap() {
        let excerpt = case["excerpt"].as_str().unwrap();
        let fingerprint = match case["exceptionClass"].as_str() {
            Some(class) => fingerprint_failure(
                FailureKind::PythonException,
                Some("ipython"),
                Some(class),
                "x",
            ),
            None => fingerprint_failure(kind(&case["kind"]), Some("src"), None, "unrelated"),
        };
        let observation = FailureObservation {
            fingerprint,
            excerpt: excerpt.to_string(),
            entry_index: 0,
            turn: 0,
            at: String::new(),
            replay_case: None,
        };
        assert_eq!(
            Value::from(observation.is_actionable()),
            case["actionable"],
            "{case}"
        );
    }
}

#[test]
fn provisional_regressions_are_found_and_recorded_like_ts() {
    let golden = fixture("regressions.json");
    let ravo = &golden["ravo"];
    let ids = |list: &[&str]| list.iter().map(|id| (*id).to_string()).collect::<Vec<_>>();
    let found = find_provisional_regressions(Some(ravo), &ids(&["a", "b"]), 10, "ordinal");
    assert_eq!(serde_json::to_value(&found).unwrap(), golden["found"]);
    let local = find_provisional_regressions(Some(ravo), &ids(&["a"]), 10, "local-ordinal");
    assert_eq!(serde_json::to_value(&local).unwrap(), golden["foundLocal"]);
    assert_eq!(
        serde_json::to_value(find_provisional_regressions(Some(ravo), &[], 10, "ordinal")).unwrap(),
        golden["none"]
    );
    assert_eq!(
        serde_json::to_value(find_provisional_regressions(
            Some(ravo),
            &ids(&["a"]),
            26,
            "ordinal"
        ))
        .unwrap(),
        golden["outside"]
    );
    assert_eq!(
        text(&record_provisional_regressions(ravo, &found, 12)),
        golden["recordedText"].as_str().unwrap()
    );
}

#[test]
fn replay_cases_derive_and_validate_like_the_referee() {
    let golden = fixture("replay.json");
    for case in golden["derived"].as_array().unwrap() {
        let excerpt = case["excerpt"].as_str().unwrap();
        let fingerprint = fingerprint_failure(
            FailureKind::PythonException,
            Some("ipython"),
            case["exceptionClass"].as_str(),
            excerpt,
        );
        let derived = derive_replay_case(&fingerprint, excerpt);
        assert_eq!(
            serde_json::to_value(derived).unwrap(),
            case["replayCase"],
            "{excerpt:?}"
        );
    }
    for case in golden["probes"].as_array().unwrap() {
        let source = case["source"].as_str().unwrap();
        assert_eq!(
            Value::from(replay_probe_of(source).is_some()),
            case["valid"],
            "{source:?}"
        );
    }
    let normalized = normalize_replay_cases(
        Some(&json!([
            { "language": "python", "source": "import b", "sysPath": ["/p"] },
            { "language": "python", "source": "import b", "verifiedAt": "v" },
            { "language": "python", "source": "import c" },
        ])),
        Some(&json!({ "language": "python", "source": "import b", "verifiedAt": "legacy" })),
    );
    assert_eq!(
        serde_json::to_value(normalized).unwrap(),
        golden["normalized"]
    );
}

fn cells(value: &Value) -> Vec<(String, String, bool)> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|cell| {
            (
                cell["code"].as_str().unwrap().to_string(),
                cell["output"].as_str().unwrap().to_string(),
                cell["isError"].as_bool().unwrap(),
            )
        })
        .collect()
}

fn index_over(
    store: &Arc<FileResolutionStore>,
    clock: &Arc<AtomicU64>,
    step: u64,
) -> ResolutionIndex {
    struct Shared(Arc<FileResolutionStore>);
    impl ResolutionStore for Shared {
        fn load(&self) -> Vec<pa_ledger::ResolutionRecord> {
            self.0.load()
        }
        fn save(&self, record: &pa_ledger::ResolutionRecord) {
            self.0.save(record);
        }
        fn forget(&self, fingerprint_id: &str) {
            self.0.forget(fingerprint_id);
        }
    }
    let clock = Arc::clone(clock);
    ResolutionIndex::new(ResolutionIndexOptions {
        store: Some(Box::new(Shared(Arc::clone(store)))),
        ..ResolutionIndexOptions::default()
    })
    .with_clock(move || clock.fetch_add(step, Ordering::SeqCst) + step)
}

#[test]
fn the_resolution_index_and_its_store_match_the_ts_hints_and_bytes() {
    let golden = fixture("resolution.json");
    let agent = tempfile::tempdir().unwrap();
    let repo = golden["repo"].as_str().unwrap();
    let path: PathBuf = resolution_store_path(repo, agent.path());
    assert_eq!(
        path.strip_prefix(agent.path()).unwrap(),
        Path::new(golden["storeName"].as_str().unwrap())
    );
    let store = Arc::new(FileResolutionStore::new(path.clone(), repo.to_string()));
    let clock = Arc::new(AtomicU64::new(golden["clockStart"].as_u64().unwrap()));
    let step = golden["clockStep"].as_u64().unwrap();

    let mut session_a = index_over(&store, &clock, step);
    let hints: Vec<Value> = cells(&golden["cells"])
        .iter()
        .map(|(code, output, is_error)| {
            let hint = session_a.observe(ResolutionCell {
                code,
                output,
                is_error: *is_error,
            });
            hint.map_or(Value::Null, |hint| Value::from(hint.text))
        })
        .collect();
    assert_eq!(Value::from(hints), golden["hints"]);
    assert_eq!(
        serde_json::to_value(session_a.records()).unwrap(),
        golden["sessionRecords"]
    );
    assert_eq!(Value::from(session_a.unresolved()), golden["unresolved"]);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        golden["storeAfterA"].as_str().unwrap()
    );

    let mut session_b = index_over(&store, &clock, step);
    let hints: Vec<Value> = cells(&golden["cellsB"])
        .iter()
        .map(|(code, output, is_error)| {
            session_b
                .observe(ResolutionCell {
                    code,
                    output,
                    is_error: *is_error,
                })
                .map_or(Value::Null, |hint| Value::from(hint.text))
        })
        .collect();
    assert_eq!(Value::from(hints), golden["hintsB"]);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        golden["storeAfterB"].as_str().unwrap()
    );

    for case in golden["storeNames"].as_array().unwrap() {
        let name = resolution_store_path(case["repo"].as_str().unwrap(), Path::new("/agent"));
        assert_eq!(
            name.strip_prefix("/agent").unwrap(),
            Path::new(case["name"].as_str().unwrap())
        );
    }
}

#[test]
fn a_ledger_in_an_empty_harness_state_writes_the_ts_bytes() {
    let golden = fixture("harness.json");
    let prompt = fixture("prompt.json");
    let ledger = normalize_failure_ledger(
        &serde_json::from_str::<Value>(prompt["ledgerText"].as_str().unwrap()).unwrap(),
    );
    let dir = tempfile::tempdir().unwrap();
    let mut document = HarnessDocument::load(dir.path());
    document.set_failures(&ledger);
    let path = document.save(dir.path()).unwrap();
    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        golden["emptyWithFailuresText"].as_str().unwrap()
    );
}
