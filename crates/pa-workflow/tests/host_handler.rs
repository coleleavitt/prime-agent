//! The `workflow.run_agent` host-handler battery (TS
//! `workflow-v1-host-handler.test.ts`): the handler a session registers
//! through the feature seam, driven against a scripted faux provider that
//! a temp agent dir's models.json registers — the real registry, auth
//! preflight, and provider transport, no network.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use pa_ai::faux::{
    faux_assistant_message, faux_text, register_faux_provider, FauxAssistantMessageOptions,
    FauxProviderRegistration, FauxResponseStep, RegisterFauxProviderOptions,
};
use pa_core::features::{FeatureTelemetry, SessionFeature, SessionFeatureContext};
use pa_core::kernel::shared::{
    with_host_request_cancellation, HostRequestHandlers, HostRequestPayload,
};
use pa_core::session_engine::telemetry::TelemetryWiring;
use pa_telemetry::{MockSink, TelemetryClient, TelemetryClientConfig, TelemetrySink};
use pa_workflow::WorkflowFeature;
use serde_json::{json, Value};

const MODEL_ID: &str = "faux-1";

/// What the provider received: the API key, the headers, the session id.
type SeenRequest = (
    Option<String>,
    Option<BTreeMap<String, String>>,
    Option<String>,
);

/// One test's world: a faux provider under a unique api/provider name, an
/// agent dir whose models.json registers it with `provider_config`, and the
/// handlers the feature registers for a session on that model.
struct Harness {
    dir: tempfile::TempDir,
    faux: FauxProviderRegistration,
    provider: String,
    handlers: HostRequestHandlers,
    telemetry: Option<(TelemetryClient, Arc<MockSink>)>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.faux.unregister();
    }
}

fn harness(name: &str, provider_config: &Value) -> Harness {
    build(name, provider_config, false)
}

fn build(name: &str, provider_config: &Value, with_telemetry: bool) -> Harness {
    let provider = format!("workflow-host-{name}");
    let faux = register_faux_provider(RegisterFauxProviderOptions {
        api: Some(format!("{provider}-api")),
        provider: Some(provider.clone()),
        ..RegisterFauxProviderOptions::default()
    });
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    // A live session's agent dir already holds its auth store (opening one
    // creates it), so the isolation snapshot starts from that state.
    std::fs::write(agent_dir.join("auth.json"), "{}").unwrap();
    let mut config = provider_config.clone();
    config["baseUrl"] = json!("http://127.0.0.1:9");
    config["api"] = json!(faux.api);
    config["models"] = json!([{ "id": MODEL_ID, "name": "Faux", "contextWindow": 128_000 }]);
    std::fs::write(
        agent_dir.join("models.json"),
        serde_json::to_string(&json!({ "providers": { provider.clone(): config } })).unwrap(),
    )
    .unwrap();
    let telemetry = with_telemetry.then(|| {
        let mock = Arc::new(MockSink::new());
        let mut config = TelemetryClientConfig::new("install-1");
        config.batch_size = 1;
        config.sinks = vec![Arc::clone(&mock) as Arc<dyn TelemetrySink>];
        (TelemetryClient::spawn(config).unwrap(), mock)
    });
    let context = SessionFeatureContext {
        agent_dir,
        cwd: dir.path().to_path_buf(),
        session_id: "session-1".to_string(),
        python_skill_import_names: Vec::new(),
        model: serde_json::from_value(json!({
            "id": MODEL_ID, "name": "Faux", "api": faux.api, "provider": provider,
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
        rlm_depth: 0,
    };
    let mut handlers = HostRequestHandlers::new();
    WorkflowFeature.register_host_handlers(&context, &mut handlers);
    Harness {
        dir,
        faux,
        provider,
        handlers,
        telemetry,
    }
}

fn request() -> Value {
    json!({
        "protocol": "prime.workflow.run-agent/v1",
        "requestId": "req-1",
        "nodeId": "node-1",
        "prompt": "one turn",
        "model": null,
        "maxTurns": 1,
        "maxResultUtf8Bytes": 1024,
        "drainTimeoutMs": 1000,
        "tools": "none",
    })
}

fn text_response(text: &str) -> FauxResponseStep {
    FauxResponseStep::Message(faux_assistant_message(
        vec![faux_text(text)],
        FauxAssistantMessageOptions::default(),
    ))
}

impl Harness {
    async fn call(&self, request: Value) -> anyhow::Result<Value> {
        let handler = self.handlers.get("workflow.run_agent").unwrap().clone();
        handler(HostRequestPayload {
            data: json!({ "type": "workflow.run_agent", "request": request, "cellSourceCode": "x" }),
            cell_source_code: None,
        })
        .await
    }

    fn selector(&self) -> String {
        format!("{}/{MODEL_ID}", self.provider)
    }
}

/// The reply's classification, without its timing.
fn verdict(reply: &Value) -> Value {
    json!({
        "resolvedModel": reply["resolvedModel"],
        "turnsStarted": reply["turnsStarted"],
        "outcome": reply["outcome"],
        "stopReason": reply["stopReason"],
        "result": reply["result"],
        "errorCode": reply["error"]["code"],
    })
}

fn refused() -> Value {
    json!({
        "resolvedModel": null,
        "turnsStarted": 0,
        "outcome": "failed",
        "stopReason": "model_resolution_failed",
        "result": null,
        "errorCode": "MODEL_RESOLUTION_FAILED",
    })
}

fn completed(selector: &str, text: &str) -> Value {
    json!({
        "resolvedModel": selector,
        "turnsStarted": 1,
        "outcome": "completed",
        "stopReason": "completed",
        "result": pa_workflow::v1::wire::ResultText::new(text.to_string()),
        "errorCode": null,
    })
}

#[test]
fn the_feature_registers_exactly_the_run_agent_route() {
    let harness = harness("route", &json!({ "apiKey": "workflow-key" }));
    assert_eq!(harness.handlers.len(), 1);
    assert!(harness.handlers.get("workflow.run_agent").is_some());
}

#[tokio::test]
async fn old_or_mixed_protocols_fail_closed_before_provider_io() {
    let harness = harness("wire", &json!({ "apiKey": "workflow-key" }));
    harness
        .faux
        .set_responses(vec![text_response("must remain")]);
    let mut old = request();
    old["protocol"] = json!("prime.workflow.run-agent/v0");
    let mut mixed = request();
    mixed["legacyProtocol"] = json!("prime.workflow.run-agent/v0");
    for invalid in [old, mixed] {
        let error = harness.call(invalid).await.unwrap_err();
        assert!(
            error.to_string().starts_with("unsupported protocol")
                || error.to_string() == "request has missing or unknown fields",
            "{error}"
        );
    }
    assert_eq!(harness.faux.get_pending_response_count(), 1);
}

#[tokio::test]
async fn the_session_model_runs_and_returns_the_closed_result() {
    let harness = harness("current", &json!({ "apiKey": "workflow-key" }));
    harness
        .faux
        .set_responses(vec![FauxResponseStep::Message(faux_assistant_message(
            vec![faux_text("hé"), faux_text("llo")],
            FauxAssistantMessageOptions::default(),
        ))]);
    let reply = harness.call(request()).await.unwrap();
    assert_eq!(verdict(&reply), completed(&harness.selector(), "héllo"));
    assert_eq!(
        (&reply["protocol"], &reply["requestId"], &reply["nodeId"]),
        (
            &json!("prime.workflow.run-agent-result/v1"),
            &json!("req-1"),
            &json!("node-1")
        )
    );
    assert_eq!(harness.faux.get_pending_response_count(), 0);
}

#[tokio::test]
async fn an_unavailable_exact_model_fails_closed_before_provider_io() {
    let harness = harness("missing", &json!({ "apiKey": "workflow-key" }));
    harness
        .faux
        .set_responses(vec![text_response("must remain")]);
    let mut missing = request();
    missing["model"] = json!("missing/model");
    let reply = harness.call(missing).await.unwrap();
    assert_eq!(verdict(&reply), refused());
    assert_eq!(harness.faux.get_pending_response_count(), 1);
}

/// Absent or non-credential auth refuses the turn before provider I/O: no
/// resolvable key, metadata-only headers, or an `authHeader` without a key.
#[cfg(unix)]
#[tokio::test]
async fn absent_or_non_credential_auth_fails_closed_before_provider_io() {
    let cases = [
        ("absent-auth", json!({ "apiKey": "!false" })),
        (
            "user-agent-only",
            json!({ "apiKey": "!false", "headers": { "User-Agent": "workflow-client" } }),
        ),
        (
            "metadata-header",
            json!({ "apiKey": "!false", "headers": { "X-Workflow-Metadata": "not-a-secret" } }),
        ),
        (
            "auth-header-missing",
            json!({ "apiKey": "!false", "authHeader": true }),
        ),
    ];
    for (name, config) in cases {
        let harness = harness(name, &config);
        harness
            .faux
            .set_responses(vec![text_response("must remain")]);
        let reply = harness.call(request()).await.unwrap();
        assert_eq!(verdict(&reply), refused(), "{name}");
        assert_eq!(harness.faux.get_pending_response_count(), 1, "{name}");
    }
}

/// A key, a configured credential-bearing header, or an explicit
/// `authHeader: false` no-auth policy each clears the preflight.
#[cfg(unix)]
#[tokio::test]
async fn usable_credentials_or_an_explicit_no_auth_policy_clear_the_preflight() {
    let cases = [
        (
            "no-auth-policy",
            json!({ "apiKey": "!false", "authHeader": false }),
        ),
        (
            "literal-key",
            json!({ "apiKey": "workflow-key", "authHeader": true }),
        ),
        (
            "command-key",
            json!({ "apiKey": "!printf workflow-command-key", "authHeader": true }),
        ),
        (
            "authorization-header",
            json!({ "apiKey": "!false", "headers": { "Authorization": "Bearer workflow-token" } }),
        ),
        (
            "x-api-key-header",
            json!({ "apiKey": "!false", "headers": { "X-API-Key": "workflow-header-key" } }),
        ),
    ];
    for (name, config) in cases {
        let harness = harness(name, &config);
        harness
            .faux
            .set_responses(vec![text_response("configured")]);
        let reply = harness.call(request()).await.unwrap();
        assert_eq!(
            verdict(&reply),
            completed(&harness.selector(), "configured"),
            "{name}"
        );
    }
}

fn filesystem_snapshot(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut entries = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let key = path.strip_prefix(root).unwrap().display().to_string();
            if path.is_dir() {
                entries.push((format!("{key}/"), Vec::new()));
                stack.push(path);
            } else {
                entries.push((key, std::fs::read(&path).unwrap()));
            }
        }
    }
    entries.sort();
    entries
}

/// The turn carries only the resolved credential and auth headers, and
/// leaves the agent dir and working tree byte-identical.
#[tokio::test]
async fn the_turn_is_isolated_from_session_state_and_inherited_headers() {
    let harness = harness(
        "isolated",
        &json!({
            "apiKey": "workflow-key",
            "headers": { "X-Workflow-Auth": "workflow-secret" },
            "authHeader": true
        }),
    );
    let seen: Arc<Mutex<Vec<SeenRequest>>> = Arc::default();
    let record = Arc::clone(&seen);
    harness
        .faux
        .set_responses(vec![FauxResponseStep::Factory(Arc::new(
            move |context, options, _call, _model| {
                assert_eq!(context.tools.as_deref().map(<[_]>::len), Some(0));
                assert_eq!(context.system_prompt, None);
                record.lock().unwrap().push((
                    options.and_then(|options| options.api_key.clone()),
                    options
                        .and_then(|options| options.headers.clone())
                        .map(|headers| headers.into_iter().collect()),
                    options.and_then(|options| options.session_id.clone()),
                ));
                Ok(faux_assistant_message(
                    vec![faux_text("isolated")],
                    FauxAssistantMessageOptions::default(),
                ))
            },
        ))]);
    let before = filesystem_snapshot(harness.dir.path());
    let reply = harness.call(request()).await.unwrap();
    assert_eq!(verdict(&reply), completed(&harness.selector(), "isolated"));
    assert_eq!(filesystem_snapshot(harness.dir.path()), before);
    assert_eq!(
        *seen.lock().unwrap(),
        vec![(
            Some("workflow-key".to_string()),
            Some(BTreeMap::from([
                (
                    "Authorization".to_string(),
                    "Bearer workflow-key".to_string()
                ),
                ("X-Workflow-Auth".to_string(), "workflow-secret".to_string()),
            ])),
            None,
        )]
    );
    assert_eq!(harness.faux.get_pending_response_count(), 0);
}

/// A request whose host cancellation already fired settles `cancelled`
/// without provider I/O, and the settle ships its adoption event.
#[tokio::test]
async fn a_cancelled_request_settles_without_provider_io_and_is_recorded() {
    let harness = build("cancelled", &json!({ "apiKey": "workflow-key" }), true);
    harness
        .faux
        .set_responses(vec![text_response("must remain")]);
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();
    let reply = with_host_request_cancellation(cancel, harness.call(request()))
        .await
        .unwrap();
    assert_eq!(
        verdict(&reply),
        json!({
            "resolvedModel": harness.selector(),
            "turnsStarted": 0,
            "outcome": "cancelled",
            "stopReason": "caller_aborted",
            "result": null,
            "errorCode": null,
        })
    );
    assert_eq!(harness.faux.get_pending_response_count(), 1);

    let (client, mock) = harness.telemetry.as_ref().unwrap();
    client.flush().await.unwrap();
    let events = mock.events();
    assert_eq!(events.len(), 1);
    let event = &events[0];
    let properties: Value = [
        "outcome",
        "stop_reason",
        "turns_started",
        "total_tokens",
        "budget_exhausted",
    ]
    .iter()
    .map(|key| {
        (
            (*key).to_string(),
            event.properties.get(key).cloned().unwrap_or(Value::Null),
        )
    })
    .collect::<serde_json::Map<_, _>>()
    .into();
    assert_eq!(
        (event.name.as_str(), properties),
        (
            "workflow_run_agent",
            json!({
                "outcome": "cancelled",
                "stop_reason": "caller_aborted",
                "turns_started": 0,
                "total_tokens": 0,
                "budget_exhausted": false
            })
        )
    );
    assert!(event
        .properties
        .get("duration_ms")
        .is_some_and(Value::is_u64));
}
