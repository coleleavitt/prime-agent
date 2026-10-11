//! The in-session Dream feature end to end (TS `agent-session-dream.test.ts`):
//! the `dream.*` host requests and `/dream` a session gets through the
//! feature seam, against a scripted faux provider a temp agent dir's
//! models.json registers — the real registry, credential and provider
//! transport, no network.

use std::sync::{Arc, Mutex};

use pa_ai::faux::{
    FauxAssistantMessageOptions,
    FauxProviderRegistration,
    FauxResponseStep,
    RegisterFauxProviderOptions,
    faux_assistant_message,
    faux_text,
    register_faux_provider,
};
use pa_core::features::{
    FeatureStatus,
    FeatureStatusSink,
    FeatureTelemetry,
    SessionFeature,
    SessionFeatureContext,
    register_feature_status_sink,
};
use pa_core::kernel::shared::{HostRequestHandlers, HostRequestPayload};
use pa_dream::session::{DREAM_REQUEST_TYPES, DreamFeature};
use pa_dream::store::{list_trees, read_tree};
use serde_json::{Value, json};

const MODEL_ID: &str = "faux-dream-1";
const SMALL: &str =
    "--task sum-difference --seed 3 --iterations 1 --workers 1 --k1 2 --k2 4 --dreams 1";

struct Harness {
    dir: tempfile::TempDir,
    faux: FauxProviderRegistration,
    context: Arc<SessionFeatureContext>,
    events: Arc<Mutex<Vec<(String, Value)>>>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.faux.unregister();
    }
}

fn harness(name: &str, depth: u32) -> Harness {
    let provider = format!("dream-session-{name}");
    let faux = register_faux_provider(RegisterFauxProviderOptions {
        api: Some(format!("{provider}-api")),
        provider: Some(provider.clone()),
        ..RegisterFauxProviderOptions::default()
    });
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path().join("agent");
    std::fs::create_dir_all(&agent_dir).unwrap();
    std::fs::write(agent_dir.join("auth.json"), "{}").unwrap();
    std::fs::write(
        agent_dir.join("models.json"),
        serde_json::to_string(&json!({ "providers": { provider.clone(): {
            "apiKey": "dream-key", "baseUrl": "http://127.0.0.1:9", "api": faux.api,
            "models": [{ "id": MODEL_ID, "name": "Faux", "contextWindow": 128_000, "maxTokens": 16_384 }]
        } } }))
        .unwrap(),
    )
    .unwrap();
    let events: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
    let recorder = Arc::clone(&events);
    let context = SessionFeatureContext {
        agent_dir,
        cwd: dir.path().to_path_buf(),
        session_id: format!("dream-session-{name}"),
        python_skill_import_names: Vec::new(),
        model: serde_json::from_value(json!({
            "id": MODEL_ID, "name": "Faux", "api": faux.api, "provider": provider,
            "baseUrl": "http://127.0.0.1:9", "reasoning": false,
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
            "contextWindow": 128_000, "maxTokens": 16_384
        }))
        .unwrap(),
        telemetry: Some(FeatureTelemetry::new(move |name, properties| {
            recorder
                .lock()
                .unwrap()
                .push((name.to_string(), serde_json::to_value(&properties).unwrap()));
        })),
        rlm_depth: depth,
        session_artifact_dir: Some(dir.path().join("artifacts")),
    };
    Harness {
        dir,
        faux,
        context: Arc::new(context),
        events,
    }
}

impl Harness {
    fn handlers(&self, feature: &DreamFeature) -> HostRequestHandlers {
        let mut handlers = HostRequestHandlers::new();
        feature.register_host_handlers(&self.context, &mut handlers);
        handlers
    }

    fn dream_dir(&self) -> std::path::PathBuf {
        self.context.agent_dir.join("dream")
    }
}

async fn call(
    handlers: &HostRequestHandlers,
    request: &str,
    payload: Value,
) -> anyhow::Result<Value> {
    let handler = handlers.get(request).unwrap().clone();
    handler(HostRequestPayload {
        data: payload,
        cell_source_code: None,
    })
    .await
}

async fn command(
    feature: &DreamFeature,
    harness: &Harness,
    args: &str,
) -> Result<(String, Result<String, String>), String> {
    let outcome = feature
        .execute_slash_command(&harness.context, "dream", args)
        .expect("dream owns /dream")
        .await?;
    let completion = outcome.completion.expect("a run reports its end").await;
    Ok((outcome.text, completion))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_local_dream_command_completes_at_zero_tokens_and_reports_its_end() {
    let harness = harness("local", 0);
    let feature = DreamFeature::new();
    let handlers = harness.handlers(&feature);
    assert_eq!(handlers.len(), DREAM_REQUEST_TYPES.len());
    assert_eq!(
        call(&handlers, "dream.status", json!({})).await.unwrap(),
        json!({ "phase": "idle" })
    );
    assert_eq!(
        call(&handlers, "dream.cancel", json!({})).await.unwrap(),
        json!({ "cancelled": false })
    );
    let statuses: Arc<Mutex<Vec<FeatureStatus>>> = Arc::default();
    let seen = Arc::clone(&statuses);
    let sink: FeatureStatusSink = Arc::new(move |status| seen.lock().unwrap().push(status));
    register_feature_status_sink(&harness.context.session_id, &sink);

    let (started, completion) = command(&feature, &harness, SMALL).await.unwrap();
    assert!(
        started.starts_with("Dream-RSI run dream_")
            && started.ends_with(" started: sum-difference"),
        "{started}"
    );
    let run_id = started.split(' ').nth(2).unwrap().to_string();
    assert_eq!(completion, Ok(format!("Dream-RSI run {run_id} completed")));
    assert_eq!(
        harness.faux.call_count(),
        0,
        "the local path spends no token"
    );
    let status = call(&handlers, "dream.status", json!({})).await.unwrap();
    assert_eq!(
        (status["runId"].clone(), status["stopReason"].clone()),
        (json!(run_id), json!("completed"))
    );
    assert_eq!(list_trees(&harness.dream_dir()).len(), 2);

    let statuses = statuses.lock().unwrap();
    assert!(statuses.iter().all(|status| status.feature == "dream"));
    assert_eq!(
        statuses
            .first()
            .and_then(|status| status.line.clone())
            .as_deref()
            .map(|line| line.starts_with("dream rollout it0 best ")),
        Some(true)
    );
    assert!(
        statuses
            .last()
            .unwrap()
            .line
            .as_deref()
            .unwrap()
            .starts_with("dream ")
    );
    assert_eq!(
        statuses.last().unwrap().status["stopReason"],
        json!("completed")
    );

    let events = harness.events.lock().unwrap();
    let (name, properties) = events
        .iter()
        .find(|(name, _)| name == "dream_session_run")
        .expect("adoption event");
    assert_eq!(name, "dream_session_run");
    for (key, value) in [
        ("kind", json!("run")),
        ("surface", json!("command")),
        ("task", json!("sum-difference")),
        ("outcome", json!("completed")),
        ("llm_proposer", json!(false)),
        ("llm_dreamer", json!(false)),
        ("seeds", json!(1)),
    ] {
        assert_eq!(properties[key], value, "{key}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_llm_proposer_reaches_the_provider_with_thinking_off_and_the_proposer_cap() {
    let harness = harness("llm", 0);
    let caps: Arc<Mutex<Vec<Option<u64>>>> = Arc::default();
    let seen = Arc::clone(&caps);
    harness
        .faux
        .set_responses(vec![FauxResponseStep::Factory(Arc::new(
            move |_, options, _, _| {
                seen.lock()
                    .unwrap()
                    .push(options.and_then(|options| options.max_tokens));
                Ok(faux_assistant_message(
                    vec![faux_text(r#"{"set":[0,1,3,7,12]}"#)],
                    FauxAssistantMessageOptions::default(),
                ))
            },
        ))]);
    harness.faux.set_repeat_last_response(true);
    let feature = DreamFeature::new();
    let (_, completion) = command(&feature, &harness, &format!("{SMALL} --llm-proposer"))
        .await
        .unwrap();
    assert!(completion.unwrap().ends_with(" completed"));
    let caps = caps.lock().unwrap();
    assert!(!caps.is_empty());
    assert!(caps.iter().all(|cap| *cap == Some(4096)), "{caps:?}");
    // Every probe the provider answered entered a tree as the agent's work.
    let llm_nodes: usize = list_trees(&harness.dream_dir())
        .iter()
        .map(|summary| {
            read_tree(&summary.tree_id, &harness.dream_dir())
                .unwrap()
                .nodes
                .iter()
                .filter(|node| node.origin.as_deref() == Some("llm"))
                .count()
        })
        .sum();
    assert_eq!(llm_nodes as u64, harness.faux.call_count());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_kernel_requests_start_report_and_refuse_like_the_ts_session() {
    let harness = harness("kernel", 0);
    let feature = DreamFeature::new();
    let handlers = harness.handlers(&feature);
    for (request, payload, message) in [
        (
            "dream.run",
            json!({ "task": "nope" }),
            "dream.run task must be one of circle-packing, sum-difference, python-speedup, autocorrelation",
        ),
        (
            "dream.experiment",
            json!({ "task": "sum-difference", "seed": 1, "seeds": [2] }),
            "dream.experiment takes either seed or seeds, not both",
        ),
    ] {
        let error = call(&handlers, request, payload).await.unwrap_err();
        assert_eq!(error.to_string(), message);
    }
    let started = call(
        &handlers,
        "dream.experiment",
        json!({ "task": "sum-difference", "seeds": [5, 6], "rounds": 2, "workers": 1, "k1": 2, "k2": 4, "dreams": 1 }),
    )
    .await
    .unwrap();
    assert_eq!(started["started"], json!(true));
    assert_eq!(started["seeds"], json!(2));
    assert!(
        started["note"]
            .as_str()
            .unwrap()
            .contains("(2 seeds, run sequentially)")
    );
    let status = call(&handlers, "dream.status", json!({})).await.unwrap();
    assert_eq!(status["runId"], started["runId"]);
    assert_eq!(status["kind"], json!("experiment"));
    // A cancel stops it; the status settles cancelled or (if it already
    // finished) completed.
    call(&handlers, "dream.cancel", json!({})).await.unwrap();
    let settled = loop {
        let status = call(&handlers, "dream.status", json!({})).await.unwrap();
        if status.get("stopReason").is_some() {
            break status;
        }
        tokio::task::yield_now().await;
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    };
    assert!(
        matches!(
            settled["stopReason"].as_str(),
            Some("cancelled" | "completed")
        ),
        "{settled}"
    );
    assert_eq!(harness.faux.call_count(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_child_session_gets_no_dream_surface() {
    let harness = harness("child", 1);
    let feature = DreamFeature::new();
    assert!(harness.handlers(&feature).is_empty());
    // The bundled skill is hidden where its requests are not registered;
    // other skills are not this feature's to hide.
    assert!(!feature.session_skill_visible(&harness.context, "dream"));
    assert!(feature.session_skill_visible(&harness.context, "refine"));
    let storeless = SessionFeatureContext {
        rlm_depth: 0,
        session_artifact_dir: None,
        ..(*harness.context).clone()
    };
    assert!(!feature.session_skill_visible(&storeless, "dream"));
    assert!(feature.session_skill_visible(&harness_with_depth0(&harness), "dream"));
    let refused = feature
        .execute_slash_command(&harness.context, "dream", "")
        .unwrap()
        .await
        .err();
    assert_eq!(
        refused.as_deref(),
        Some("Dream-RSI is not available in this session")
    );
    assert!(
        feature
            .execute_slash_command(&harness.context, "other", "")
            .is_none()
    );
    assert!(!harness.dir.path().join("agent/dream").exists());
    let usage = feature
        .execute_slash_command(&harness_with_depth0(&harness), "dream", "--bogus")
        .unwrap()
        .await
        .err()
        .unwrap();
    assert!(usage.starts_with("Usage: /dream [experiment]"));
}

fn harness_with_depth0(harness: &Harness) -> Arc<SessionFeatureContext> {
    Arc::new(SessionFeatureContext {
        rlm_depth: 0,
        session_id: format!("{}-top", harness.context.session_id),
        ..(*harness.context).clone()
    })
}

#[test]
fn the_feature_contributes_its_command_and_skill() {
    let feature = DreamFeature::new();
    let commands = feature.slash_commands();
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].name, "dream");
    assert_eq!(feature.bundled_skills(), vec!["dream"]);
    let skill = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../skills")
        .join(pa_core::packages::FEATURE_SKILLS_DIR)
        .join("dream/SKILL.md");
    let text = std::fs::read_to_string(skill).expect("the bundled skill ships");
    assert!(text.starts_with("---\nname: dream\n"));
}
