//! The per-session skill-visibility seam end to end: a stub feature
//! installed the way the composition root installs one hides a loaded
//! skill from the sessions it gates, in both the system prompt's skill list
//! and the kernel's bound skills, and leaves `/skill:` expansion its full
//! inventory. Its own test binary, because installation is process-global.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pa_agent::scripted::ScriptedProvider;
use pa_agent::types::AgentMessage;
use pa_core::features::{SessionFeature, SessionFeatureContext};
use pa_core::session_engine::engine::{create_session, SessionEngine, SessionEngineConfig};

/// The bundled Python skill the stub gates (import name `edit`).
const GATED: &str = "edit";

/// What a session told the stub at start.
#[derive(Debug, Clone, PartialEq)]
struct Started {
    rlm_depth: u32,
    artifact_dir: Option<PathBuf>,
    python_skill_import_names: Vec<String>,
}

#[derive(Default)]
struct Stub {
    started: Mutex<Vec<Started>>,
}

impl SessionFeature for Stub {
    fn name(&self) -> &'static str {
        "stub"
    }

    fn session_skill_visible(&self, context: &SessionFeatureContext, skill_name: &str) -> bool {
        skill_name != GATED || (context.rlm_depth == 0 && context.session_artifact_dir.is_some())
    }

    fn on_session_start(&self, context: &Arc<SessionFeatureContext>, _history: &[AgentMessage]) {
        self.started.lock().unwrap().push(Started {
            rlm_depth: context.rlm_depth,
            artifact_dir: context.session_artifact_dir.clone(),
            python_skill_import_names: context.python_skill_import_names.clone(),
        });
    }
}

fn model() -> pa_agent::types::Model {
    pa_agent::types::Model {
        id: "m".into(),
        name: "m".into(),
        api: "test".into(),
        provider: "test".into(),
        base_url: "http://localhost".into(),
        reasoning: false,
        cost: pa_agent::types::UsageCost::default(),
        context_window: 1_000,
        max_tokens: 100,
    }
}

async fn session(root: &std::path::Path, depth: u32, log: Option<PathBuf>) -> SessionEngine {
    let cwd = root.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let provider = Arc::new(ScriptedProvider::new(model()));
    Box::pin(create_session(SessionEngineConfig {
        cwd,
        agent_dir: root.join("agent"),
        model: Some(model()),
        stream_fn: Some(provider.stream_fn()),
        conversation_log_path: log,
        rlm_depth: Some(depth),
        ..SessionEngineConfig::default()
    }))
    .await
    .unwrap()
}

fn listed(engine: &SessionEngine, name: &str) -> bool {
    engine
        .system_prompt
        .contains(&format!("<name>{name}</name>"))
}

#[tokio::test]
async fn a_feature_hides_a_skill_from_the_sessions_it_gates() {
    let stub = Arc::new(Stub::default());
    assert!(pa_core::features::install(vec![
        Arc::clone(&stub) as Arc<dyn SessionFeature>
    ]));
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("sessions").join("project").join("s1.jsonl");

    let top = session(tmp.path(), 0, Some(log.clone())).await;
    let child = session(tmp.path(), 1, Some(log)).await;
    let storeless = session(tmp.path(), 0, None).await;

    assert!(listed(&top, GATED));
    assert!(!listed(&child, GATED));
    assert!(!listed(&storeless, GATED));
    // Other skills stay listed, and `/skill:` expansion keeps the full
    // inventory (TS reads the resource loader there).
    for engine in [&child, &storeless] {
        assert!(listed(engine, "goal"));
        assert!(engine.skills.iter().any(|skill| skill.name == GATED));
    }

    let started = stub.started.lock().unwrap().clone();
    let top_names = started[0].python_skill_import_names.clone();
    assert!(top_names.iter().any(|name| name == GATED));
    let gated_names: Vec<String> = top_names
        .iter()
        .filter(|name| *name != GATED)
        .cloned()
        .collect();
    assert_eq!(
        started,
        vec![
            Started {
                rlm_depth: 0,
                artifact_dir: Some(
                    tmp.path()
                        .join("sessions")
                        .join("session-artifacts")
                        .join("s1")
                ),
                python_skill_import_names: top_names,
            },
            Started {
                rlm_depth: 1,
                artifact_dir: Some(
                    tmp.path()
                        .join("sessions")
                        .join("session-artifacts")
                        .join("s1")
                ),
                python_skill_import_names: gated_names.clone(),
            },
            Started {
                rlm_depth: 0,
                artifact_dir: None,
                python_skill_import_names: gated_names,
            },
        ]
    );
}
