//! The `workflow.v2.request` host-handler battery: the handler a session
//! registers through the feature seam, driven with the runtime's envelope
//! (`{"type": "workflow.v2.request", "request": …}`). No provider is ever
//! involved: `validate` is pure and every durable action is unavailable.

use std::sync::Arc;

use pa_core::features::{FeatureTelemetry, SessionFeature, SessionFeatureContext};
use pa_core::kernel::shared::{HostRequestHandlers, HostRequestPayload};
use pa_core::session_engine::telemetry::TelemetryWiring;
use pa_telemetry::{MockSink, TelemetryClient, TelemetryClientConfig, TelemetrySink};
use pa_workflow::WorkflowFeature;
use pa_workflow::v2::wire::{Def, decode_as, decode_public_result};
use serde_json::{Value, json};

/// The runtime unittest's definition; its digest is sha256 over Python's
/// `json.dumps(sort_keys=True, separators=(",", ":"), ensure_ascii=False)`.
const DEFINITION_DIGEST: &str =
    "sha256:ac3260986589ff84071f3f1cadd684ac7fa684344bba88f470f549730a551794";

fn definition() -> Value {
    json!({
        "protocol": "prime.workflow.definition/v2",
        "nodes": [{
            "nodeId": "n", "kind": "agent", "prompt": "hi", "dependsOn": [],
            "model": "m", "maxTurns": 1, "tools": "none", "maxTokens": 10
        }],
        "outputs": ["n"],
        "budget": { "maxConcurrentAttempts": 1, "maxTotalTokens": 10, "semantics": "soft_admission" }
    })
}

fn request(action: &str, fields: &Value) -> Value {
    let mut request = json!({
        "protocol": "prime.workflow.request/v2", "requestId": "r", "action": action
    });
    for (key, value) in fields.as_object().unwrap() {
        request[key] = value.clone();
    }
    request
}

fn command(action: &str) -> Value {
    request(
        action,
        &json!({
            "runId": "run", "commandId": "cmd", "expectedRevision": 0,
            "expectedControllerEpoch": 0, "expectedCancelEpoch": 0
        }),
    )
}

fn unavailable(action: &str) -> Value {
    json!({
        "protocol": "prime.workflow.error/v2",
        "requestId": "r",
        "code": "CAPABILITY_UNAVAILABLE",
        "message": format!("Workflow V2 {action} is unavailable: this host has no durable workflow controller"),
        "retryable": false,
        "currentRevision": null
    })
}

struct Harness {
    handlers: HostRequestHandlers,
    telemetry: Option<(TelemetryClient, Arc<MockSink>)>,
    _dir: tempfile::TempDir,
}

fn harness(with_telemetry: bool) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let telemetry = with_telemetry.then(|| {
        let mock = Arc::new(MockSink::new());
        let mut config = TelemetryClientConfig::new("install-1");
        config.batch_size = 1;
        config.sinks = vec![Arc::clone(&mock) as Arc<dyn TelemetrySink>];
        (TelemetryClient::spawn(config).unwrap(), mock)
    });
    let context = SessionFeatureContext {
        agent_dir: dir.path().join("agent"),
        cwd: dir.path().to_path_buf(),
        session_id: "session-1".to_string(),
        python_skill_import_names: Vec::new(),
        model: serde_json::from_value(json!({
            "id": "m", "name": "M", "api": "workflow-v2-none", "provider": "none",
            "baseUrl": "http://127.0.0.1:9", "reasoning": false,
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
            "contextWindow": 128_000, "maxTokens": 16_384
        }))
        .unwrap(),
        telemetry: telemetry.as_ref().map(|(client, _)| {
            FeatureTelemetry::from_wiring(&TelemetryWiring {
                client: client.clone(),
                execution_mode: None,
                now: None,
                telemetry_enabled: None,
            })
        }),
        session_artifact_dir: None,
        rlm_depth: 0,
    };
    let mut handlers = HostRequestHandlers::new();
    WorkflowFeature.register_host_handlers(&context, &mut handlers);
    Harness {
        handlers,
        telemetry,
        _dir: dir,
    }
}

impl Harness {
    async fn call(&self, request: Value) -> anyhow::Result<Value> {
        let handler = self.handlers.get("workflow.v2.request").unwrap().clone();
        handler(HostRequestPayload {
            data: json!({ "type": "workflow.v2.request", "request": request }),
            cell_source_code: None,
        })
        .await
    }
}

#[tokio::test]
async fn validate_answers_the_closed_result_with_the_canonical_digest() {
    let reply = harness(false)
        .call(request("validate", &json!({ "definition": definition() })))
        .await
        .unwrap();
    assert_eq!(
        reply,
        json!({
            "protocol": "prime.workflow.result/v2",
            "requestId": "r",
            "action": "validate",
            "valid": true,
            "definitionDigest": DEFINITION_DIGEST,
            "errors": [],
            "warnings": []
        })
    );
    assert_eq!(decode_public_result(&reply), Ok(()));
}

#[tokio::test]
async fn an_invalid_definition_validates_false_without_a_digest() {
    let harness = harness(false);
    let mut cyclic = definition();
    cyclic["nodes"][0]["dependsOn"] = json!([{ "nodeId": "n", "require": "accepted" }]);
    let mut oversized = definition();
    oversized["nodes"][0]["prompt"] = json!("💣".repeat(16_385));
    let mut over_budget = definition();
    over_budget["budget"]["maxTotalTokens"] = json!(9);
    for (definition, error) in [
        (
            cyclic,
            "$.definition.nodes[0].dependsOn contains a self dependency",
        ),
        (
            oversized,
            "$.definition.nodes[0].prompt exceeds its UTF-8 byte bound",
        ),
        (
            over_budget,
            "$.definition.budget.maxTotalTokens must be at least maxTokens for node \"n\"",
        ),
    ] {
        let reply = harness
            .call(request("validate", &json!({ "definition": definition })))
            .await
            .unwrap();
        assert_eq!(
            reply,
            json!({
                "protocol": "prime.workflow.result/v2",
                "requestId": "r",
                "action": "validate",
                "valid": false,
                "definitionDigest": null,
                "errors": [error],
                "warnings": []
            })
        );
        assert_eq!(decode_public_result(&reply), Ok(()));
    }
}

#[tokio::test]
async fn every_durable_action_is_capability_unavailable_never_an_empty_answer() {
    let harness = harness(false);
    let requests = [
        (
            "create",
            request("create", &json!({ "definition": definition() })),
        ),
        ("start", command("start")),
        ("cancel", {
            let mut cancel = command("cancel");
            cancel["reason"] = json!("stop");
            cancel
        }),
        ("retry", {
            let mut retry = command("retry");
            retry["nodeId"] = json!("n");
            retry["fromAttemptId"] = json!("a1");
            retry["reason"] = json!("again");
            retry
        }),
        ("status", request("status", &json!({ "runId": "run" }))),
        (
            "events",
            request("events", &json!({ "runId": "run", "limit": 10 })),
        ),
    ];
    for (action, request) in requests {
        let reply = harness.call(request).await.unwrap();
        assert_eq!(reply, unavailable(action));
        assert_eq!(decode_as(&reply, Def::PublicError), Ok(()));
    }
}

#[tokio::test]
async fn a_create_with_an_invalid_definition_is_invalid_definition() {
    let mut definition = definition();
    definition["outputs"] = json!(["missing"]);
    let reply = harness(false)
        .call(request("create", &json!({ "definition": definition })))
        .await
        .unwrap();
    assert_eq!(
        reply,
        json!({
            "protocol": "prime.workflow.error/v2",
            "requestId": "r",
            "code": "INVALID_DEFINITION",
            "message": "$.definition.outputs references unknown node \"missing\"",
            "retryable": false,
            "currentRevision": null
        })
    );
}

#[tokio::test]
async fn an_envelope_outside_the_closed_family_is_invalid_request() {
    let harness = harness(false);
    let mut extra = request("status", &json!({ "runId": "run" }));
    extra["extra"] = json!(true);
    let mut unknown_action = request("status", &json!({ "runId": "run" }));
    unknown_action["action"] = json!("pause");
    let mut float_revision = command("start");
    float_revision["expectedRevision"] = json!(1.0);
    for (request, message) in [
        (extra, "$.extra is unknown"),
        (unknown_action, "$.action is outside the closed enum"),
        (float_revision, "$.expectedRevision must be a safe integer"),
    ] {
        let reply = harness.call(request).await.unwrap();
        assert_eq!(
            reply,
            json!({
                "protocol": "prime.workflow.error/v2",
                "requestId": "r",
                "code": "INVALID_REQUEST",
                "message": message,
                "retryable": false,
                "currentRevision": null
            })
        );
    }
}

#[tokio::test]
async fn an_uncorrelatable_envelope_fails_the_host_request() {
    let harness = harness(false);
    let mut no_id = request("status", &json!({ "runId": "run" }));
    no_id.as_object_mut().unwrap().remove("requestId");
    let mut bad_id = request("status", &json!({ "runId": "run" }));
    bad_id["requestId"] = json!(" r");
    for request in [no_id, bad_id, json!("status"), Value::Null] {
        let error = harness.call(request).await.unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("workflow.v2.request is outside the closed protocol: "),
            "{error}"
        );
    }
}

#[tokio::test]
async fn each_answer_records_its_action_and_outcome_only() {
    let harness = harness(true);
    harness
        .call(request("validate", &json!({ "definition": definition() })))
        .await
        .unwrap();
    harness
        .call(request("status", &json!({ "runId": "run" })))
        .await
        .unwrap();
    harness.call(Value::Null).await.unwrap_err();
    let (client, mock) = harness.telemetry.as_ref().unwrap();
    client.flush().await.unwrap();
    let recorded: Vec<(String, Value)> = mock
        .events()
        .iter()
        .map(|event| {
            // The client's common context rides every event; the feature's
            // own properties are exactly these two, and no id leaks.
            assert!(
                event
                    .properties
                    .iter()
                    .all(|(_, value)| value != "r" && value != "run"),
                "{:?}",
                event.properties
            );
            let own: serde_json::Map<String, Value> = ["action", "outcome"]
                .iter()
                .map(|key| {
                    (
                        (*key).to_string(),
                        event.properties.get(key).cloned().unwrap_or(Value::Null),
                    )
                })
                .collect();
            (event.name.clone(), Value::Object(own))
        })
        .collect();
    assert_eq!(
        recorded,
        vec![
            (
                "workflow_durable_request".to_string(),
                json!({ "action": "validate", "outcome": "valid" })
            ),
            (
                "workflow_durable_request".to_string(),
                json!({ "action": "status", "outcome": "capability_unavailable" })
            ),
            (
                "workflow_durable_request".to_string(),
                json!({ "action": "unknown", "outcome": "invalid_request" })
            ),
        ]
    );
}
