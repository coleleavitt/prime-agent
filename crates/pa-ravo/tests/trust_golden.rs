//! Replays the TS trust goldens (`tests/fixtures/golden/trust.json`,
//! written by `trust-generate.ts` from `refinement/harness-trust.ts`) and
//! compares the Rust results as JSON text, key order included.

use indexmap::IndexMap;
use pa_ravo::{
    TrustAdjustment,
    TrustClaim,
    TrustWindowEvidence,
    TrustWindows,
    WindowSettlement,
    normalize_entry_trust,
    normalize_trust_windows,
    open_trust_window,
    record_trust_window_evidence,
    settle_harness_trust,
    trust_windows_value,
};
use serde_json::{Map, Value, json};

const AT: &str = "2026-09-14T08:00:00.000Z";

fn fixture() -> Value {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/golden/trust.json");
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

fn assert_json(actual: &Value, expected: &Value, context: &str) {
    assert_eq!(
        serde_json::to_string_pretty(actual).unwrap(),
        serde_json::to_string_pretty(expected).unwrap(),
        "{context}"
    );
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

fn claim(value: &Value) -> TrustClaim {
    TrustClaim {
        proposal_id: value["proposalId"].as_str().unwrap().to_string(),
        touched: strings(&value["touched"]),
        claimed_fingerprints: strings(&value["claimedFingerprints"]),
        committed_turn: value["committedTurn"].as_u64().unwrap(),
        until_turn: value["untilTurn"].as_u64().unwrap(),
        skill_imports: value["skillImports"]
            .as_object()
            .map(|imports| {
                imports
                    .iter()
                    .map(|(entry, list)| (entry.clone(), strings(list)))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

fn evidence(value: &Value) -> TrustWindowEvidence {
    let text = |key: &str| value[key].as_str().unwrap().to_string();
    let ordinal = value["ordinal"].as_u64().unwrap();
    match value["type"].as_str().unwrap() {
        "recurrence" => TrustWindowEvidence::Recurrence {
            proposal_id: text("proposalId"),
            fingerprint_id: text("fingerprintId"),
            ordinal,
        },
        _ => TrustWindowEvidence::Adjudication {
            proposal_id: text("proposalId"),
            entry: text("entry"),
            fingerprint_id: text("fingerprintId"),
            status: serde_json::from_value(value["status"].clone()).unwrap(),
            ordinal,
            at: text("at"),
        },
    }
}

fn adjustment_value(adjustment: &TrustAdjustment) -> Value {
    let mut map = json!({
        "kind": adjustment.kind,
        "id": adjustment.id,
        "reason": adjustment.reason.as_str(),
        "delta": adjustment.delta,
        "before": adjustment.before,
        "after": adjustment.after,
        "trust": adjustment.trust,
        "proposalId": adjustment.proposal_id,
    });
    if let Some(fingerprint) = &adjustment.fingerprint_id {
        map["fingerprintId"] = json!(fingerprint);
    }
    map
}

fn settled_value(window: &WindowSettlement) -> Value {
    json!({
        "proposalId": window.proposal_id,
        "from": window.from.as_str(),
        "outcome": window.outcome.as_str(),
        "turn": window.turn,
        "fingerprints": window.fingerprints,
    })
}

#[test]
fn trust_records_and_windows_normalize_like_the_ts_loader() {
    let golden = fixture();
    for case in golden["normalizeTrust"].as_array().unwrap() {
        let output = normalize_entry_trust(case.get("input"));
        assert_json(
            &serde_json::to_value(output).unwrap(),
            &case["output"],
            &case.to_string(),
        );
    }
    for case in golden["normalizeWindows"].as_array().unwrap() {
        let output = normalize_trust_windows(case.get("input"));
        assert_json(
            &output.as_ref().map_or(Value::Null, trust_windows_value),
            &case["output"],
            &case["input"].to_string(),
        );
    }
}

#[test]
fn windows_open_record_and_settle_like_the_ts_windows() {
    let golden = fixture();
    let imports: IndexMap<&str, Vec<String>> = [
        ("skill:probe", vec!["absent_module".to_string()]),
        ("skill:other", vec!["other_module".to_string()]),
    ]
    .into_iter()
    .collect();
    let current = |entry: &str| imports.get(entry).cloned();
    let mut entries: Map<String, Value> = json!({
        "skill": {
            "probe": { "reference": { "import": "absent_module" } },
            "other": { "reference": { "import": "other_module" }, "trust": { "score": 22, "updated_at": "old", "events": [] } }
        },
        "memory": { "note": {} },
        "prompt": { "policy": { "trust": { "score": 98, "updated_at": "old", "events": [] } } }
    })
    .as_object()
    .unwrap()
    .clone();
    let mut windows: Option<TrustWindows> = None;
    for (index, traced) in golden["trace"].as_array().unwrap().iter().enumerate() {
        let step = &traced["step"];
        let context = format!("step {index}: {step}");
        if let Some(open) = step.get("open") {
            windows = Some(open_trust_window(windows.as_ref(), &claim(open)));
        } else if let Some(record) = step.get("record") {
            let items: Vec<TrustWindowEvidence> =
                record.as_array().unwrap().iter().map(evidence).collect();
            windows = record_trust_window_evidence(windows.as_ref(), &items, Some(&current));
        } else {
            let turn = step["settle"].as_u64().unwrap();
            let (settled_windows, settlement) =
                settle_harness_trust(windows.as_ref().unwrap(), &mut entries, turn, AT);
            windows = Some(settled_windows);
            let adjustments: Vec<Value> = settlement
                .adjustments
                .iter()
                .map(adjustment_value)
                .collect();
            assert_json(&json!(adjustments), &traced["adjustments"], &context);
            let settled: Vec<Value> = settlement.settled.iter().map(settled_value).collect();
            assert_json(&json!(settled), &traced["settled"], &context);
            assert_json(
                &Value::Object(entries.clone()),
                &traced["entries"],
                &context,
            );
        }
        assert_json(
            &trust_windows_value(windows.as_ref().unwrap()),
            &traced["windows"],
            &context,
        );
    }
}

#[test]
fn settled_windows_are_pruned_oldest_first_and_open_ones_never() {
    let golden = fixture();
    let open = |windows: Option<&TrustWindows>, id: &str, claimed: &[&str]| {
        open_trust_window(
            windows,
            &TrustClaim {
                proposal_id: id.to_string(),
                touched: if id == "open" {
                    vec!["memory:note".to_string()]
                } else {
                    Vec::new()
                },
                claimed_fingerprints: claimed.iter().map(|id| (*id).to_string()).collect(),
                committed_turn: 0,
                until_turn: u64::from(id == "open"),
                skill_imports: IndexMap::new(),
            },
        )
    };
    let mut windows = open(None, "open", &["fa"]);
    for index in 0..102u64 {
        let id = format!("s{}{index}", index % 7);
        windows = open(Some(&windows), &id, &[]);
        // Settled the way TS does it in the generator: a stored window
        // read back with its outcome and settled turn.
        let mut raw = trust_windows_value(&windows);
        raw[&id]["outcome"] = json!("clean");
        raw[&id]["settledTurn"] = json!(index % 5);
        windows = normalize_trust_windows(Some(&raw)).unwrap();
    }
    windows = open(Some(&windows), "last", &["fa"]);
    let keys: Vec<&String> = windows.keys().collect();
    assert_json(&json!(keys), &golden["pruned"], "pruned");
}
