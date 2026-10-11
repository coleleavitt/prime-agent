//! The kernel-facing contract: the `toolforge.publish` handler the feature
//! registers through the session seam, its response shape (what
//! `rlm.toolforge.publish` reads) and its adoption event.
#![allow(clippy::too_many_lines)]

mod common;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use common::{SLUGIFY_DOC, SLUGIFY_EXIT_TEST, SLUGIFY_SOURCE, gate_python, recording_installer};
use pa_core::features::{FeatureTelemetry, SessionFeature, SessionFeatureContext};
use pa_core::kernel::shared::{HostRequestHandlers, HostRequestPayload};
use pa_telemetry::Properties;
use pa_toolforge::{PUBLISH_REQUEST, ToolforgeFeature, ToolforgeOverrides};
use serde_json::{Value, json};

type Tracked = Arc<Mutex<Vec<(String, Properties)>>>;

fn context(agent_dir: PathBuf, loaded: &[&str], tracked: &Tracked) -> SessionFeatureContext {
    let recorder = Arc::clone(tracked);
    SessionFeatureContext {
        cwd: agent_dir.clone(),
        agent_dir,
        session_id: "s-handler".to_string(),
        python_skill_import_names: loaded.iter().map(|name| (*name).to_string()).collect(),
        model: serde_json::from_value(serde_json::json!({
            "id": "m1", "name": "M1", "api": "test", "provider": "p1",
            "baseUrl": "http://localhost", "reasoning": false,
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
            "contextWindow": 1000, "maxTokens": 100
        }))
        .unwrap(),
        rlm_depth: 0,
        session_artifact_dir: None,
        telemetry: Some(FeatureTelemetry::new(move |name, properties| {
            recorder
                .lock()
                .unwrap()
                .push((name.to_string(), properties));
        })),
    }
}

async fn call(handlers: &HostRequestHandlers, data: Value) -> Value {
    let handler = handlers.get(PUBLISH_REQUEST).expect("registered").clone();
    handler(HostRequestPayload {
        data,
        cell_source_code: None,
    })
    .await
    .expect("the handler answers rejections as values")
}

fn properties(pairs: &[(&str, Value)]) -> Properties {
    let mut properties = Properties::new();
    for (key, value) in pairs {
        properties.set(key, value.clone());
    }
    properties
}

/// Durations vary per run; everything else is compared whole.
fn without_durations(mut response: Value) -> Value {
    if let Some(gate) = response.get_mut("gate").and_then(Value::as_array_mut) {
        for run in gate {
            run["duration_ms"] = json!(0);
            run["detail"] = json!("");
        }
    }
    response
}

#[tokio::test]
async fn the_feature_serves_publish_and_reports_adoption() {
    let Some(python) = gate_python() else { return };
    let dir = tempfile::tempdir().unwrap();
    let agent_dir = dir.path().join("agent");
    let installs = Arc::new(Mutex::new(Vec::new()));
    let feature = ToolforgeFeature::with_overrides(ToolforgeOverrides {
        python: Some(python),
        installer: Some(recording_installer(&installs)),
        ..ToolforgeOverrides::default()
    });
    let tracked: Tracked = Arc::new(Mutex::new(Vec::new()));
    let mut handlers = HostRequestHandlers::default();
    feature.register_host_handlers(
        &context(agent_dir.clone(), &["edit"], &tracked),
        &mut handlers,
    );
    assert_eq!(feature.name(), "toolforge");

    let published = call(
        &handlers,
        json!({
            "type": PUBLISH_REQUEST,
            "cellSourceCode": "await rlm.toolforge.publish(...)",
            "name": "slugify",
            "source": SLUGIFY_SOURCE,
            "doc": SLUGIFY_DOC,
            "exit_test": SLUGIFY_EXIT_TEST,
        }),
    )
    .await;
    let package = agent_dir.join("skills").join("slugify");
    assert_eq!(
        without_durations(published),
        json!({
            "status": "published",
            "name": "slugify",
            "import_name": "slugify",
            "package_path": package.display().to_string(),
            "src_path": package.join("src").display().to_string(),
            "version": 1,
            "installed": false,
            "gate": [
                { "phase": "negative", "outcome": "raised", "detail": "", "duration_ms": 0, "ok": true },
                { "phase": "positive", "outcome": "clean", "detail": "", "duration_ms": 0, "ok": true },
            ],
            "install_detail": "test installer: promoted without installing",
        })
    );

    // A loaded skill's name and a non-string field are refusals, not errors.
    let shadowing = call(
        &handlers,
        json!({ "name": "edit", "source": 1, "doc": "d", "exit_test": "t" }),
    )
    .await;
    assert_eq!(
        shadowing,
        json!({
            "status": "rejected",
            "name": "edit",
            "import_name": "",
            "package_path": "",
            "src_path": "",
            "version": 0,
            "installed": false,
            "gate": [],
            "reason": "toolforge name \"edit\" collides with the loaded skill edit",
        })
    );
    let shapeless = call(&handlers, json!({ "name": "fresh", "source": 1 })).await;
    assert_eq!(
        shapeless["reason"],
        json!("toolforge source must be a non-empty string")
    );

    // One adoption event per attempt: categories and counts only.
    let events: Vec<(String, Properties)> = tracked
        .lock()
        .unwrap()
        .iter()
        .map(|(name, props)| {
            let mut props = props.clone();
            props.set("duration_ms", json!(0));
            (name.clone(), props)
        })
        .collect();
    assert_eq!(
        events,
        vec![
            (
                "toolforge publish".to_string(),
                properties(&[
                    ("status", json!("published")),
                    ("rejection", Value::Null),
                    ("installed", json!(false)),
                    ("gate_run_count", json!(2)),
                    ("version", json!(1)),
                    ("duration_ms", json!(0)),
                ])
            ),
            (
                "toolforge publish".to_string(),
                properties(&[
                    ("status", json!("rejected")),
                    ("rejection", json!("name")),
                    ("installed", json!(false)),
                    ("gate_run_count", json!(0)),
                    ("version", json!(0)),
                    ("duration_ms", json!(0)),
                ])
            ),
            (
                "toolforge publish".to_string(),
                properties(&[
                    ("status", json!("rejected")),
                    ("rejection", json!("shape")),
                    ("installed", json!(false)),
                    ("gate_run_count", json!(0)),
                    ("version", json!(0)),
                    ("duration_ms", json!(0)),
                ])
            ),
        ]
    );
}

/// Concurrent publishes from one session run one at a time: both land, and
/// the versions are consecutive rather than racing to the same number.
#[tokio::test]
async fn concurrent_publishes_are_serialized() {
    let Some(python) = gate_python() else { return };
    let dir = tempfile::tempdir().unwrap();
    let installs = Arc::new(Mutex::new(Vec::new()));
    let feature = ToolforgeFeature::with_overrides(ToolforgeOverrides {
        python: Some(python),
        installer: Some(recording_installer(&installs)),
        ..ToolforgeOverrides::default()
    });
    let tracked: Tracked = Arc::new(Mutex::new(Vec::new()));
    let mut handlers = HostRequestHandlers::default();
    feature.register_host_handlers(
        &context(dir.path().join("agent"), &[], &tracked),
        &mut handlers,
    );
    let request = json!({
        "name": "slugify",
        "source": SLUGIFY_SOURCE,
        "doc": SLUGIFY_DOC,
        "exit_test": SLUGIFY_EXIT_TEST,
    });
    let (first, second) = tokio::join!(
        call(&handlers, request.clone()),
        call(&handlers, request.clone())
    );
    let mut versions = vec![first["version"].clone(), second["version"].clone()];
    versions.sort_by_key(Value::as_u64);
    assert_eq!(versions, vec![json!(1), json!(2)]);
}
