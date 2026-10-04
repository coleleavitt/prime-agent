//! RAVO through the session seams (TS `agent-session.ts`'s ravo wiring):
//! `/ravo` (the started row, the durable terminal row), the run's live
//! status published as the session's `ravo` feature status with the TS
//! agents-view line, the bundled `ravo` skill, and the session gate — all
//! against a scripted faux provider a temp agent dir's models.json
//! registers: the real registry, credential and provider transport, no
//! network.

use std::future::Future;
use std::pin::Pin;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use pa_ai::faux::{
    faux_assistant_message, faux_text, register_faux_provider, FauxAssistantMessageOptions,
    FauxProviderRegistration, FauxResponseStep, RegisterFauxProviderOptions,
};
use pa_core::features::{
    register_feature_status_sink, FeatureStatus, FeatureStatusSink, FeatureTelemetry,
    SessionFeature, SessionFeatureContext,
};
use pa_core::kernel::shared::{HostRequestHandlers, HostRequestPayload};
use pa_ledger::ReplayCase;
use pa_ravo::{RavoFeature, RavoOptions, ReplayEnvironment, ReplayOutcome, ReplayRunner};
use serde_json::{json, Value};

const MODEL_ID: &str = "faux-ravo-1";

struct NeverRuns;

impl ReplayRunner for NeverRuns {
    fn run<'a>(
        &'a self,
        _case: &'a ReplayCase,
        _environment: ReplayEnvironment,
        _sys_path: &'a [String],
    ) -> Pin<Box<dyn Future<Output = ReplayOutcome> + Send + 'a>> {
        panic!("no failure recurs in these runs");
    }
}

/// The reply a RAVO child gets, by its prompt's heading; the judge passes.
fn child_reply(heading: &str) -> String {
    match heading {
        "# RAVO inspect" => json!({ "summary": "nothing yet", "facts": ["no memory"] }).to_string(),
        "# RAVO plan" => json!({ "steps": ["create memory:tactic"] }).to_string(),
        "# RAVO implement" | "# RAVO repair" => json!({
            "summary": "note Tactic", "rationale": "seen twice", "expectedOutcome": "recall",
            "addressedFingerprints": [],
            "edits": [{ "action": "create", "kind": "memory", "id": "tactic", "title": "Tactic", "content": "Use tactic A", "reason": "evidence" }]
        })
        .to_string(),
        "# RAVO judge" => {
            r#"{"verdict":"pass","score":80,"failedCriteria":[],"rationale":"fine"}"#.to_string()
        }
        other => panic!("unexpected child {other}"),
    }
}

fn prompt_heading(context: &pa_types::ai::Context) -> String {
    let text = context
        .messages
        .iter()
        .find_map(|message| match message {
            pa_types::ai::Message::User(user) => match &user.content {
                pa_types::ai::UserContent::Text(text) => Some(text.clone()),
                pa_types::ai::UserContent::Blocks(_) => None,
            },
            _ => None,
        })
        .unwrap_or_default();
    text.lines().next().unwrap_or_default().to_string()
}

struct Harness {
    _dir: tempfile::TempDir,
    faux: FauxProviderRegistration,
    context: Arc<SessionFeatureContext>,
    events: Arc<Mutex<Vec<(String, Value)>>>,
    statuses: Arc<Mutex<Vec<FeatureStatus>>>,
    _sink: FeatureStatusSink,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.faux.unregister();
    }
}

fn harness(name: &str, depth: u32, artifacts: bool) -> Harness {
    let provider = format!("ravo-session-{name}");
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
            "apiKey": "ravo-key", "baseUrl": "http://127.0.0.1:9", "api": faux.api,
            "models": [{ "id": MODEL_ID, "name": "Faux", "contextWindow": 128_000, "maxTokens": 16_384 }]
        } } }))
        .unwrap(),
    )
    .unwrap();
    let events: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
    let recorder = Arc::clone(&events);
    let session_id = format!("ravo-session-{name}");
    let context = SessionFeatureContext {
        agent_dir,
        cwd: dir.path().to_path_buf(),
        session_id: session_id.clone(),
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
        session_artifact_dir: artifacts.then(|| dir.path().join("artifacts")),
    };
    let statuses: Arc<Mutex<Vec<FeatureStatus>>> = Arc::default();
    let seen = Arc::clone(&statuses);
    let sink: FeatureStatusSink = Arc::new(move |status| seen.lock().unwrap().push(status));
    register_feature_status_sink(&session_id, &sink);
    Harness {
        _dir: dir,
        faux,
        context: Arc::new(context),
        events,
        statuses,
        _sink: sink,
    }
}

fn feature() -> RavoFeature {
    RavoFeature::new(RavoOptions {
        enabled: Some(true),
        runner: Arc::new(NeverRuns),
        replay_sys_path: Vec::new(),
    })
}

impl Harness {
    fn handlers(&self, feature: &RavoFeature) -> HostRequestHandlers {
        let mut handlers = HostRequestHandlers::new();
        feature.register_host_handlers(&self.context, &mut handlers);
        handlers
    }

    /// Answer every child from [`child_reply`].
    fn script(&self) {
        self.faux
            .set_responses(vec![FauxResponseStep::Factory(Arc::new(
                |context, _, _, _| {
                    Ok(faux_assistant_message(
                        vec![faux_text(&child_reply(&prompt_heading(context)))],
                        FauxAssistantMessageOptions::default(),
                    ))
                },
            ))]);
        self.faux.set_repeat_last_response(true);
    }
}

async fn call(handlers: &HostRequestHandlers, request: &str, payload: Value) -> Value {
    let handler = handlers.get(request).expect("registered").clone();
    handler(HostRequestPayload {
        data: payload,
        cell_source_code: None,
    })
    .await
    .unwrap()
}

async fn ravo(
    feature: &RavoFeature,
    context: &Arc<SessionFeatureContext>,
    args: &str,
) -> Result<pa_core::features::FeatureCommandOutcome, String> {
    feature
        .execute_slash_command(context, "ravo", args)
        .expect("ravo owns /ravo")
        .await
}

/// `/ravo` starts a run, answers the TS started row at once, and its
/// completion is the TS terminal row; every status update is the session's
/// `ravo` feature status with the TS agents-view line; the run's adoption
/// event fires once.
#[tokio::test(flavor = "multi_thread")]
async fn a_ravo_command_runs_reports_its_end_and_publishes_its_status() {
    let harness = harness("command", 0, true);
    harness.script();
    let feature = feature();
    let outcome = ravo(&feature, &harness.context, "--rounds 2 note  the tactic")
        .await
        .unwrap();
    let run_id = outcome.text.split(' ').nth(2).unwrap().to_string();
    assert!(run_id.starts_with("ravo_"), "{}", outcome.text);
    assert_eq!(
        outcome.text,
        format!("RAVO run {run_id} started: note the tactic")
    );
    let completion = tokio::time::timeout(
        Duration::from_secs(60),
        outcome.completion.expect("a run reports its end"),
    )
    .await
    .expect("the run settles");
    assert_eq!(completion, Ok(format!("RAVO run {run_id} accepted")));
    assert_eq!(
        harness.faux.call_count(),
        4,
        "inspect, plan, implement, judge"
    );

    let statuses = harness.statuses.lock().unwrap().clone();
    assert!(!statuses.is_empty());
    for status in &statuses {
        assert_eq!(status.feature, "ravo");
        assert_eq!(status.status["runId"], json!(run_id));
        assert_eq!(
            status.line.as_deref(),
            Some(pa_ravo::ravo_status_line(&status.status).as_str())
        );
    }
    let lines: Vec<&str> = statuses
        .iter()
        .filter_map(|status| status.line.as_deref())
        .collect();
    assert!(lines.contains(&"ravo inspect r1/0"), "{lines:?}");
    assert_eq!(lines.last(), Some(&"ravo accepted"));
    assert_eq!(statuses.last().unwrap().status["phase"], json!("accepted"));

    let handlers = harness.handlers(&feature);
    let status = call(&handlers, "ravo.status", json!({})).await;
    assert_eq!(status, statuses.last().unwrap().status);

    let events = harness.events.lock().unwrap().clone();
    let runs: Vec<&Value> = events
        .iter()
        .filter(|(name, _)| name == "ravo_run")
        .map(|(_, properties)| properties)
        .collect();
    assert_eq!(runs.len(), 1);
    for (key, value) in [
        ("outcome", json!("accepted")),
        ("scope", json!("local")),
        ("rounds", json!(1)),
        ("repairs", json!(0)),
    ] {
        assert_eq!(runs[0][key], value, "{key}");
    }
}

/// One run per session: a second `/ravo` while one runs is refused with
/// the TS reason; a cancelled run's terminal row says so.
#[tokio::test(flavor = "multi_thread")]
async fn a_second_command_is_refused_while_a_run_is_in_progress() {
    let harness = harness("busy", 0, true);
    let (entered_tx, entered_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let entered = Mutex::new(entered_tx);
    let release = Mutex::new(release_rx);
    harness
        .faux
        .set_responses(vec![FauxResponseStep::Factory(Arc::new(
            move |context, _, _, _| {
                let _ = entered.lock().unwrap().send(());
                let _ = release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(60));
                Ok(faux_assistant_message(
                    vec![faux_text(&child_reply(&prompt_heading(context)))],
                    FauxAssistantMessageOptions::default(),
                ))
            },
        ))]);
    harness.faux.set_repeat_last_response(true);
    let feature = feature();
    let first = ravo(&feature, &harness.context, "note the tactic")
        .await
        .unwrap();
    let run_id = first.text.split(' ').nth(2).unwrap().to_string();
    tokio::task::spawn_blocking(move || entered_rx.recv_timeout(Duration::from_secs(60)))
        .await
        .unwrap()
        .expect("the run reaches its first child");
    let refused = ravo(&feature, &harness.context, "another task").await.err();
    assert_eq!(
        refused,
        Some(format!("RAVO run {run_id} is already in progress"))
    );
    let handlers = harness.handlers(&feature);
    assert_eq!(
        call(&handlers, "ravo.cancel", json!({})).await,
        json!({ "cancelled": true })
    );
    drop(release_tx);
    let completion = tokio::time::timeout(Duration::from_secs(60), first.completion.unwrap())
        .await
        .expect("the run settles");
    assert_eq!(completion, Ok(format!("RAVO run {run_id} cancelled")));
    assert_eq!(
        harness
            .statuses
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .line
            .as_deref(),
        Some("ravo cancelled")
    );
}

/// Like TS, RAVO is offered only where refine is: a child session or one
/// without a local harness store gets no `ravo.*` handlers and `/ravo`
/// refuses (after the usage check, which comes first as in TS), touching
/// nothing on disk and no provider.
#[tokio::test(flavor = "multi_thread")]
async fn sessions_without_refine_get_no_ravo_surface() {
    for (name, depth, artifacts) in [("child", 1, true), ("storeless", 0, false)] {
        let harness = harness(name, depth, artifacts);
        harness.script();
        let feature = feature();
        assert!(harness.handlers(&feature).is_empty(), "{name}");
        assert_eq!(
            ravo(&feature, &harness.context, "note it")
                .await
                .err()
                .as_deref(),
            Some("RAVO is not available in this session"),
            "{name}"
        );
        assert_eq!(
            ravo(&feature, &harness.context, "").await.err().as_deref(),
            Some(pa_ravo::RAVO_USAGE),
            "{name}"
        );
        assert_eq!(harness.faux.call_count(), 0);
        assert!(harness.statuses.lock().unwrap().is_empty());
        assert!(!harness.context.agent_dir.join("harness").exists());
    }
}

/// Usage errors carry the TS usage; the ARC-AGI evaluator is not part of
/// this build; a name the feature does not own is not its command.
#[tokio::test(flavor = "multi_thread")]
async fn malformed_commands_are_refused_before_a_run_starts() {
    let harness = harness("usage", 0, true);
    let feature = feature();
    assert_eq!(
        ravo(&feature, &harness.context, "x --rounds 0").await.err(),
        Some(format!(
            "{} (--rounds expects a positive integer)",
            pa_ravo::RAVO_USAGE
        ))
    );
    assert_eq!(
        ravo(
            &feature,
            &harness.context,
            "play --arc-repo /r --arc-game ls20"
        )
        .await
        .err()
        .as_deref(),
        Some("RAVO's ARC-AGI evaluator (--arc-repo/--arc-game) is not part of this build")
    );
    assert!(feature
        .execute_slash_command(&harness.context, "dream", "")
        .is_none());
    assert_eq!(harness.faux.call_count(), 0);
    assert!(harness.statuses.lock().unwrap().is_empty());
}

/// The feature contributes `/ravo` (the TS registry entry) and the bundled
/// `ravo` skill, which ships verbatim from the TS branch under
/// `skills/.features/`.
#[test]
fn the_feature_contributes_its_command_and_skill() {
    let feature = feature();
    let commands = feature.slash_commands();
    assert_eq!(commands.len(), 1);
    let command = &commands[0];
    assert_eq!(
        (
            command.name,
            command.description,
            command.argument_hint,
            command.takes_argument,
            command.execution,
        ),
        (
            "ravo",
            "Run the full RAVO loop (inspect, plan, implement, evaluate, diagnose, repair) over a continual harness mutation for a task",
            Some("[--global] [--rounds N] [--repairs N] [--arc-repo DIR --arc-game ID] <task>"),
            true,
            pa_types::slash_commands::SlashCommandExecution::Session,
        )
    );
    assert_eq!(feature.bundled_skills(), vec![pa_ravo::RAVO_SKILL]);
    let skill_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../skills")
        .join(pa_core::packages::FEATURE_SKILLS_DIR)
        .join("ravo");
    let text =
        std::fs::read_to_string(skill_dir.join("SKILL.md")).expect("the bundled skill ships");
    assert!(text.starts_with("---\nname: ravo\n"));
    assert!(skill_dir.join("pyproject.toml").is_file());
    assert!(skill_dir.join("src/ravo/__init__.py").is_file());
}
