//! The native `refine` skill is model-visible only where refine can run
//! (TS `_modelVisibleSkills`, v0.9.8 and the fork): a top-level session
//! with a local harness store. RLM children and storeless sessions neither
//! list it in the system prompt nor bind it in the kernel, because their
//! `refine.*` host requests are not registered; `/skill:` expansion keeps
//! it. Its own test binary, because the observer is installed process-wide.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pa_agent::scripted::ScriptedProvider;
use pa_core::features::{SessionFeature, SessionFeatureContext};
use pa_core::session_engine::engine::{create_session, SessionEngine, SessionEngineConfig};

/// Records the kernel's bound Python skills; gates nothing.
#[derive(Default)]
struct Observer {
    bound: Mutex<Vec<Vec<String>>>,
}

impl SessionFeature for Observer {
    fn name(&self) -> &'static str {
        "observer"
    }

    fn on_session_start(
        &self,
        context: &Arc<SessionFeatureContext>,
        _history: &[pa_agent::types::AgentMessage],
    ) {
        self.bound
            .lock()
            .unwrap()
            .push(context.python_skill_import_names.clone());
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

/// (listed in the system prompt, bound in the kernel, expandable by `/skill:`).
fn refine_surfaces(engine: &SessionEngine, bound: &[String]) -> (bool, bool, bool) {
    (
        engine.system_prompt.contains("<name>refine</name>"),
        bound.iter().any(|name| name == "refine"),
        engine.skills.iter().any(|skill| skill.name == "refine"),
    )
}

#[tokio::test]
async fn refine_is_model_visible_only_where_refine_can_run() {
    let observer = Arc::new(Observer::default());
    assert!(pa_core::features::install(vec![
        Arc::clone(&observer) as Arc<dyn SessionFeature>
    ]));
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("sessions").join("project").join("s1.jsonl");

    let top = session(tmp.path(), 0, Some(log.clone())).await;
    let child = session(tmp.path(), 1, Some(log)).await;
    let storeless = session(tmp.path(), 0, None).await;

    let bound = observer.bound.lock().unwrap().clone();
    assert_eq!(bound.len(), 3);
    assert_eq!(
        [
            refine_surfaces(&top, &bound[0]),
            refine_surfaces(&child, &bound[1]),
            refine_surfaces(&storeless, &bound[2]),
        ],
        [
            (true, true, true),
            (false, false, true),
            (false, false, true)
        ]
    );
    // Only refine is withheld: the remaining bound skills match.
    let without_refine: Vec<String> = bound[0]
        .iter()
        .filter(|name| *name != "refine")
        .cloned()
        .collect();
    assert_eq!([&bound[1], &bound[2]], [&without_refine, &without_refine]);
}
