//! The feature build's bundled `dream` and `ravo` skills reach only the
//! sessions whose `dream.*` / `ravo.*` host requests register (TS
//! `_modelVisibleSkills` with `_autoRefineAllowedForSession`): a top-level
//! session with a session store lists and binds both; an RLM child and a
//! storeless session see neither, in the system prompt or the kernel. Its
//! own test binary, because installation is process-global.
#![cfg(all(feature = "dream", feature = "ravo"))]

use std::sync::{Arc, Mutex};

use pa_agent::scripted::ScriptedProvider;
use pa_agent::types::AgentMessage;
use pa_core::features::{SessionFeature, SessionFeatureContext};
use pa_core::session_engine::engine::{create_session, SessionEngine, SessionEngineConfig};

const FEATURE_SKILLS: [&str; 2] = ["dream", "ravo"];

/// Records the kernel's bound skills each session starts with.
#[derive(Default)]
struct Recorder {
    import_names: Mutex<Vec<Vec<String>>>,
}

impl SessionFeature for Recorder {
    fn name(&self) -> &'static str {
        "recorder"
    }

    fn on_session_start(&self, context: &Arc<SessionFeatureContext>, _history: &[AgentMessage]) {
        self.import_names
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

async fn session(root: &std::path::Path, depth: u32, stored: bool) -> SessionEngine {
    let cwd = root.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let provider = Arc::new(ScriptedProvider::new(model()));
    Box::pin(create_session(SessionEngineConfig {
        cwd,
        agent_dir: root.join("agent"),
        model: Some(model()),
        stream_fn: Some(provider.stream_fn()),
        conversation_log_path: stored
            .then(|| root.join("sessions").join("project").join("s1.jsonl")),
        rlm_depth: Some(depth),
        ..SessionEngineConfig::default()
    }))
    .await
    .unwrap()
}

/// The feature skills `engine`'s system prompt lists.
fn listed(engine: &SessionEngine) -> Vec<&'static str> {
    FEATURE_SKILLS
        .into_iter()
        .filter(|name| {
            engine
                .system_prompt
                .contains(&format!("<name>{name}</name>"))
        })
        .collect()
}

/// The feature skills among a session's kernel import names.
fn bound(import_names: &[String]) -> Vec<&'static str> {
    FEATURE_SKILLS
        .into_iter()
        .filter(|name| import_names.iter().any(|import| import == name))
        .collect()
}

#[tokio::test]
async fn feature_skills_reach_only_sessions_that_can_run_them() {
    let recorder = Arc::new(Recorder::default());
    let mut features = pa_cli::features::enabled_features();
    features.push(Arc::clone(&recorder) as Arc<dyn SessionFeature>);
    assert!(pa_core::features::install(features));
    let tmp = tempfile::tempdir().unwrap();

    let top = session(tmp.path(), 0, true).await;
    let child = session(tmp.path(), 1, true).await;
    let storeless = session(tmp.path(), 0, false).await;

    assert_eq!(
        [listed(&top), listed(&child), listed(&storeless)],
        [vec!["dream", "ravo"], vec![], vec![]]
    );
    let import_names = recorder.import_names.lock().unwrap().clone();
    assert_eq!(
        import_names
            .iter()
            .map(|names| bound(names))
            .collect::<Vec<_>>(),
        vec![vec!["dream", "ravo"], vec![], vec![]]
    );
    // Only the feature skills and the native `refine` (withheld by the same
    // rule, pa-core session_refine_visibility) differ: the other native
    // skills stay bound.
    let native = |names: &[String]| -> Vec<String> {
        names
            .iter()
            .filter(|name| !FEATURE_SKILLS.contains(&name.as_str()) && *name != "refine")
            .cloned()
            .collect()
    };
    assert_eq!(native(&import_names[0]), import_names[1]);
    assert_eq!(import_names[1], import_names[2]);
}
