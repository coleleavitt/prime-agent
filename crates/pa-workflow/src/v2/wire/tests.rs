//! The TS `workflow-v2-wire.test.ts` battery and the runtime's
//! `test_workflow_v2.py` contract cases, against the Rust codec.

use serde_json::{Value, json};

use super::*;

const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn definition() -> Value {
    json!({
        "protocol": "prime.workflow.definition/v2",
        "nodes": [{
            "nodeId": "n1", "kind": "agent", "prompt": "hi", "dependsOn": [],
            "model": "model", "maxTurns": 1, "tools": "none", "maxTokens": 10
        }],
        "outputs": ["n1"],
        "budget": { "maxConcurrentAttempts": 1, "maxTotalTokens": 10, "semantics": "soft_admission" }
    })
}

fn with(mut value: Value, pointer: &str, field: Value) -> Value {
    *value.pointer_mut(pointer).unwrap() = field;
    value
}

fn base(action: &str) -> Value {
    json!({ "protocol": REQUEST_PROTOCOL, "requestId": "r1", "action": action })
}

fn extend(mut value: Value, fields: Value) -> Value {
    let (Value::Object(object), Value::Object(fields)) = (&mut value, fields) else {
        panic!("extend takes two objects");
    };
    object.extend(fields);
    value
}

fn command(action: &str) -> Value {
    extend(
        base(action),
        json!({
            "runId": "run", "commandId": "cmd", "expectedRevision": 0,
            "expectedControllerEpoch": 0, "expectedCancelEpoch": 0
        }),
    )
}

fn public_requests() -> Vec<Value> {
    vec![
        extend(base("validate"), json!({ "definition": definition() })),
        extend(base("create"), json!({ "definition": definition() })),
        command("start"),
        extend(command("cancel"), json!({ "reason": "stop" })),
        extend(
            command("retry"),
            json!({ "nodeId": "n1", "fromAttemptId": "a1", "reason": "again" }),
        ),
        extend(
            base("status"),
            json!({ "runId": "run", "include": ["nodes", "attempts"] }),
        ),
        extend(
            base("events"),
            json!({ "runId": "run", "after": "e1", "limit": 500 }),
        ),
    ]
}

fn retained_requests() -> Vec<Value> {
    let base = json!({ "protocol": "prime.workflow.retained-request/v2", "requestId": "r1", "nodeId": "n1" });
    let turn = json!({ "prompt": "hi", "model": "model", "maxTurns": 1, "tools": "none" });
    vec![
        extend(
            base.clone(),
            json!({ "operation": "child.admit", "attemptId": "a1", "workflowChildId": "w1", "turn": turn, "requestDigest": DIGEST }),
        ),
        extend(
            base.clone(),
            json!({ "operation": "child.send", "attemptId": "a1", "rlmChildId": "c1", "turn": turn, "requestDigest": DIGEST }),
        ),
        extend(
            base.clone(),
            json!({ "operation": "child.get", "rlmChildId": "c1" }),
        ),
        extend(
            base.clone(),
            json!({ "operation": "child.list", "limit": 200 }),
        ),
        extend(
            base.clone(),
            json!({ "operation": "child.events", "limit": 500 }),
        ),
        extend(
            base.clone(),
            json!({ "operation": "child.wait", "rlmChildId": "c1", "turnId": "t1", "timeoutMs": 30000 }),
        ),
        extend(
            base.clone(),
            json!({ "operation": "child.cancel", "rlmChildId": "c1", "turnId": "t1", "reason": "stop", "requestDigest": DIGEST }),
        ),
        extend(
            base,
            json!({ "operation": "child.delete", "rlmChildId": "c1", "requestDigest": DIGEST }),
        ),
    ]
}

fn retained_results() -> Vec<Value> {
    let receipt = json!({
        "protocol": "prime.workflow.retained-result/v2", "requestId": "r1",
        "hostCursor": "h1", "receiptDigest": DIGEST
    });
    vec![
        extend(
            receipt.clone(),
            json!({ "operation": "child.admit", "disposition": "admitted", "rlmChildId": "c1", "turnId": "t1" }),
        ),
        extend(
            receipt.clone(),
            json!({ "operation": "child.send", "disposition": "replayed", "rlmChildId": "c1", "turnId": "t1" }),
        ),
        extend(
            receipt.clone(),
            json!({ "operation": "child.get", "disposition": "snapshot", "child": { "rlmChildId": "c1", "turnId": "t1", "lifecycle": "running" } }),
        ),
        extend(
            receipt.clone(),
            json!({ "operation": "child.list", "disposition": "page", "children": [], "nextCursor": "h1", "caughtUp": true }),
        ),
        extend(
            receipt.clone(),
            json!({ "operation": "child.events", "disposition": "page", "events": [], "nextCursor": "h1", "caughtUp": true }),
        ),
        extend(
            receipt.clone(),
            json!({ "operation": "child.wait", "disposition": "pending", "rlmChildId": "c1", "turnId": "t1", "projection": { "phase": "running", "intent": "execute", "outcome": null, "conditions": ["result_absent"] } }),
        ),
        extend(
            receipt.clone(),
            json!({ "operation": "child.cancel", "disposition": "requested", "rlmChildId": "c1", "turnId": "t1", "actuation": "requested" }),
        ),
        extend(
            receipt,
            json!({ "operation": "child.delete", "disposition": "tombstoned", "rlmChildId": "c1" }),
        ),
    ]
}

#[test]
fn the_public_request_family_decodes() {
    let decoded: Vec<(Action, Option<String>, bool)> = public_requests()
        .iter()
        .map(|request| {
            let request = decode_public_request(request).unwrap();
            assert_eq!(request.request_id, "r1");
            (request.action, request.run_id, request.definition.is_some())
        })
        .collect();
    let run = Some("run".to_string());
    assert_eq!(
        decoded,
        vec![
            (Action::Validate, None, true),
            (Action::Create, None, true),
            (Action::Start, run.clone(), false),
            (Action::Cancel, run.clone(), false),
            (Action::Retry, run.clone(), false),
            (Action::Status, run.clone(), false),
            (Action::Events, run, false),
        ]
    );
}

#[test]
fn all_eight_retained_requests_and_results_decode() {
    for (index, request) in retained_requests().iter().enumerate() {
        assert_eq!(decode_retained_request(request), Ok(()), "request {index}");
    }
    for (index, result) in retained_results().iter().enumerate() {
        assert_eq!(
            decode_as(result, Def::RetainedResult),
            Ok(()),
            "result {index}"
        );
    }
}

#[test]
fn discriminator_key_integer_identifier_digest_enum_and_utf8_mutations_fail() {
    let start = command("start");
    for (bad, reason) in [
        (
            with(
                start.clone(),
                "/protocol",
                json!("prime.workflow.request/v1"),
            ),
            "$.protocol has the wrong constant",
        ),
        (
            extend(start.clone(), json!({ "extra": true })),
            "$.extra is unknown",
        ),
        (
            with(start.clone(), "/expectedRevision", json!(true)),
            "$.expectedRevision must be a safe integer",
        ),
        (
            with(
                start.clone(),
                "/expectedRevision",
                json!(9_007_199_254_740_992_u64),
            ),
            "$.expectedRevision must be a safe integer",
        ),
        (
            with(start.clone(), "/expectedRevision", json!(1.0)),
            "$.expectedRevision must be a safe integer",
        ),
        (
            with(start.clone(), "/expectedRevision", json!(-1)),
            "$.expectedRevision is below its minimum",
        ),
        (
            with(start, "/runId", json!(" bad")),
            "$.runId has invalid syntax",
        ),
    ] {
        assert_eq!(
            decode_public_request(&bad),
            Err(RequestError::Request(WireError::new(
                reason.split_once(' ').unwrap().0,
                reason.split_once(' ').unwrap().1
            ))),
            "{bad}"
        );
    }
    let admit = with(
        retained_requests()[0].clone(),
        "/requestDigest",
        json!(format!("sha256:{}", "A".repeat(64))),
    );
    assert_eq!(
        decode_retained_request(&admit).unwrap_err().to_string(),
        "$.requestDigest has invalid syntax"
    );
    let wait = with(retained_requests()[5].clone(), "/timeoutMs", json!(30_001));
    assert_eq!(
        decode_retained_request(&wait).unwrap_err().to_string(),
        "$.timeoutMs is above its maximum"
    );
    // 129 four-byte characters: within the 512 code-point prebound, over
    // the 512-byte UTF-8 bound.
    let cancel = extend(command("cancel"), json!({ "reason": "💣".repeat(129) }));
    assert_eq!(
        decode_public_request(&cancel).unwrap_err().to_string(),
        "$.reason exceeds its UTF-8 byte bound"
    );
    let unknown = with(command("start"), "/action", json!("pause"));
    assert_eq!(
        decode_public_request(&unknown),
        Err(RequestError::Request(WireError::new(
            "$.action",
            "is outside the closed enum"
        )))
    );
}

#[test]
fn strict_json_rejects_duplicates_trailing_bytes_invalid_utf8_depth_and_size() {
    assert_eq!(
        decode_public_request_json(
            br#"{"protocol":"prime.workflow.request/v2","requestId":"a","requestId":"b","action":"status","runId":"r"}"#
        )
        .unwrap_err()
        .to_string(),
        "$ contains a duplicate object key"
    );
    assert_eq!(
        decode_public_request_json(
            br#"{"protocol":"prime.workflow.request/v2","requestId":"a","action":"status","runId":"r"}"#
        )
        .unwrap()
        .action,
        Action::Status
    );
}

#[test]
fn definition_bounds_and_closed_nested_keys_hold() {
    let decoded = decode_definition(&definition()).unwrap();
    assert_eq!(
        decoded,
        Definition {
            nodes: vec![NodeDefinition {
                node_id: "n1".to_string(),
                prompt: "hi".to_string(),
                depends_on: Vec::new(),
                model: "model".to_string(),
                max_tokens: 10,
            }],
            outputs: vec!["n1".to_string()],
            budget: Budget {
                max_concurrent_attempts: 1,
                max_total_tokens: 10,
            },
            digest: request_digest(&definition()).unwrap(),
        }
    );
    let mut oops = definition();
    oops["nodes"][0]["oops"] = json!(true);
    assert_eq!(
        decode_definition(&oops).unwrap_err().to_string(),
        "$.nodes[0].oops is unknown"
    );
    let bomb = with(definition(), "/nodes/0/prompt", json!("💣".repeat(16_385)));
    assert_eq!(
        decode_definition(&bomb).unwrap_err().to_string(),
        "$.nodes[0].prompt exceeds its UTF-8 byte bound"
    );
}

#[test]
fn every_invalid_graph_and_budget_fails_through_every_definition_entry_point() {
    let node = definition()["nodes"][0].clone();
    let second = with(node.clone(), "/nodeId", json!("n2"));
    let depends = |value: &Value, on: &str| {
        with(
            value.clone(),
            "/dependsOn",
            json!([{ "nodeId": on, "require": "accepted" }]),
        )
    };
    let cases = [
        (
            with(definition(), "/nodes", json!([node, node])),
            ("$.nodes", "nodeId values must be unique"),
        ),
        (
            with(definition(), "/nodes", json!([depends(&node, "n1")])),
            ("$.nodes[0].dependsOn", "contains a self dependency"),
        ),
        (
            with(definition(), "/nodes", json!([depends(&node, "missing")])),
            (
                "$.nodes[0].dependsOn",
                "references unknown node \"missing\"",
            ),
        ),
        (
            with(
                definition(),
                "/nodes",
                json!([depends(&node, "n2"), depends(&second, "n1")]),
            ),
            ("$.nodes", "dependency graph must be acyclic"),
        ),
        (
            with(definition(), "/outputs", json!(["missing"])),
            ("$.outputs", "references unknown node \"missing\""),
        ),
        (
            with(definition(), "/budget/maxTotalTokens", json!(9)),
            (
                "$.budget.maxTotalTokens",
                "must be at least maxTokens for node \"n1\"",
            ),
        ),
    ];
    for (value, (path, reason)) in cases {
        assert_eq!(
            decode_definition(&value),
            Err(WireError::new(path, reason)),
            "{value}"
        );
        for action in ["validate", "create"] {
            let request = extend(base(action), json!({ "definition": value }));
            assert_eq!(
                decode_public_request(&request),
                Err(RequestError::Definition(WireError::new(
                    path.replacen('$', "$.definition", 1),
                    reason
                ))),
                "{action} {value}"
            );
        }
    }
    // A duplicate dependency is refused by the schema's uniqueItems first.
    let doubled = with(
        definition(),
        "/nodes",
        json!([
            second,
            with(
                node,
                "/dependsOn",
                json!([
                    { "nodeId": "n2", "require": "accepted" },
                    { "nodeId": "n2", "require": "accepted" }
                ])
            )
        ]),
    );
    assert_eq!(
        decode_definition(&doubled).unwrap_err().to_string(),
        "$.nodes[1].dependsOn has duplicate items"
    );
}

#[test]
fn a_long_dependency_chain_is_acyclic_without_recursion() {
    let nodes: Vec<Value> = (0..128)
        .map(|index| {
            let depends: Vec<Value> = if index == 0 {
                Vec::new()
            } else {
                vec![json!({ "nodeId": format!("n{}", index - 1), "require": "accepted" })]
            };
            json!({
                "nodeId": format!("n{index}"), "kind": "agent", "prompt": "p",
                "dependsOn": depends, "model": "m", "maxTurns": 1, "tools": "none", "maxTokens": 1
            })
        })
        .collect();
    let chain = with(definition(), "/nodes", json!(nodes));
    let chain = with(chain, "/outputs", json!(["n127"]));
    assert_eq!(decode_definition(&chain).unwrap().nodes.len(), 128);
    let mut cyclic = chain;
    cyclic["nodes"][0]["dependsOn"] = json!([{ "nodeId": "n127", "require": "accepted" }]);
    assert_eq!(
        decode_definition(&cyclic).unwrap_err().to_string(),
        "$.nodes dependency graph must be acyclic"
    );
}

#[test]
fn public_errors_capability_and_canonical_digests() {
    let error = json!({
        "protocol": ERROR_PROTOCOL, "requestId": "r", "code": "CAPABILITY_UNAVAILABLE",
        "message": "off", "retryable": false, "currentRevision": null
    });
    assert_eq!(decode_as(&error, Def::PublicError), Ok(()));
    assert_eq!(
        public_error("r", ErrorCode::CapabilityUnavailable, "off"),
        error
    );
    assert_eq!(
        request_digest(&json!({ "b": 1, "a": 2 })),
        request_digest(&json!({ "a": 2, "b": 1 }))
    );
}

#[test]
fn canonical_json_is_rfc_8785_for_v2_values() {
    // Python `json.dumps(sort_keys=True, separators=(",", ":"),
    // ensure_ascii=False)` agrees for these BMP keys.
    assert_eq!(
        canonical_json(&json!({ "b": 1, "a": 2, "é": [{ "z": "\n\u{1}\"", "y": null }] })).unwrap(),
        r#"{"a":2,"b":1,"é":[{"y":null,"z":"\n\u0001\""}]}"#
    );
    // UTF-16 code-unit order: a supplementary-plane key (a surrogate pair,
    // 0xD800…) sorts before U+FF61, unlike its UTF-8 bytes.
    assert_eq!(
        canonical_json(&json!({ "\u{ff61}": 1, "\u{10000}": 2 })).unwrap(),
        "{\"\u{10000}\":2,\"\u{ff61}\":1}"
    );
    assert_eq!(
        canonical_json(&json!({ "x": 0.5 }))
            .unwrap_err()
            .to_string(),
        "$ has a non-integer canonical number"
    );
    // Independently computed: sha256 of Python's canonical dump of the
    // runtime test's definition.
    let runtime_definition = json!({
        "protocol": "prime.workflow.definition/v2",
        "nodes": [{
            "nodeId": "n", "kind": "agent", "prompt": "hi", "dependsOn": [],
            "model": "m", "maxTurns": 1, "tools": "none", "maxTokens": 10
        }],
        "outputs": ["n"],
        "budget": { "maxConcurrentAttempts": 1, "maxTotalTokens": 10, "semantics": "soft_admission" }
    });
    assert_eq!(
        decode_definition(&runtime_definition).unwrap().digest,
        "sha256:ac3260986589ff84071f3f1cadd684ac7fa684344bba88f470f549730a551794"
    );
}

#[test]
fn result_bytes_and_digests_must_bind() {
    let text = "héllo";
    let settlement_text = json!({
        "kind": "text", "text": text, "utf8Bytes": 6,
        "sha256": "sha256:3c48591d8d098a4538f5e013dfcf406e948eac4d3277b10bf614e295d6068179"
    });
    assert_eq!(validate_digest_bindings(&settlement_text, "$"), Ok(()));
    assert_eq!(
        validate_digest_bindings(&with(settlement_text.clone(), "/utf8Bytes", json!(5)), "$"),
        Err(WireError::new("$", "has a result byte/digest mismatch"))
    );
    assert_eq!(
        validate_digest_bindings(
            &json!({ "r": [with(settlement_text, "/sha256", json!(DIGEST))] }),
            "$"
        ),
        Err(WireError::new(
            "$.r[0]",
            "has a result byte/digest mismatch"
        ))
    );
}

#[test]
fn validate_replies_are_closed_results() {
    let definition = decode_definition(&definition()).unwrap();
    let valid = validate_result("r", Ok(&definition));
    assert_eq!(
        valid,
        json!({
            "protocol": REPLY_PROTOCOL, "requestId": "r", "action": "validate", "valid": true,
            "definitionDigest": definition.digest, "errors": [], "warnings": []
        })
    );
    assert_eq!(decode_public_result(&valid), Ok(()));
    let long = WireError::new("$", "é".repeat(400));
    let invalid = validate_result("r", Err(&long));
    assert_eq!(invalid["valid"], json!(false));
    assert_eq!(invalid["definitionDigest"], Value::Null);
    assert_eq!(invalid["errors"][0].as_str().unwrap().len(), 512);
    assert_eq!(decode_public_result(&invalid), Ok(()));
    let mut extra = valid;
    extra["extra"] = json!(1);
    assert_eq!(
        decode_public_result(&extra).unwrap_err().to_string(),
        "$.extra is unknown"
    );
}

#[test]
fn bounded_text_cuts_on_character_boundaries() {
    assert_eq!(bounded_text("abc"), "abc");
    assert_eq!(bounded_text(&"a".repeat(600)).len(), 512);
    // Three-byte characters: 170 fit in 510 bytes, the 171st would not.
    assert_eq!(bounded_text(&"€".repeat(200)).chars().count(), 170);
}
