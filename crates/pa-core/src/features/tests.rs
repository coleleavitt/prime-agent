use super::*;

/// A status published before the session's sink registered reaches the
/// sink when it registers, latest per feature; later ones go straight
/// through.
#[test]
fn a_status_published_before_the_sink_registers_is_delivered_to_it() {
    let status = |feature: &str, line: &str| FeatureStatus {
        feature: feature.to_string(),
        line: Some(line.to_string()),
        status: serde_json::Value::Null,
    };
    let session = "held-status-session";
    assert!(!publish_feature_status(session, status("a", "first")));
    assert!(!publish_feature_status(session, status("a", "second")));
    assert!(!publish_feature_status(session, status("b", "only")));
    let seen: Arc<std::sync::Mutex<Vec<FeatureStatus>>> = Arc::default();
    let sink: FeatureStatusSink = {
        let seen = Arc::clone(&seen);
        Arc::new(move |status| seen.lock().unwrap().push(status))
    };
    register_feature_status_sink(session, &sink);
    assert!(publish_feature_status(session, status("a", "live")));
    assert_eq!(
        *seen.lock().unwrap(),
        vec![
            status("a", "second"),
            status("b", "only"),
            status("a", "live")
        ]
    );
}

/// Named through a const: `prompt_guards` reads every request-type
/// literal registered in `src/` as model-facing surface.
const STUB_REQUEST: &str = "stub.ping";

struct Stub;

/// A context for session `session_id` (agent dir `/agent`, cwd `/work`).
pub(super) fn context(session_id: &str) -> SessionFeatureContext {
    SessionFeatureContext {
        agent_dir: PathBuf::from("/agent"),
        cwd: PathBuf::from("/work"),
        session_id: session_id.to_string(),
        python_skill_import_names: Vec::new(),
        model: stub_model(),
        telemetry: None,
        rlm_depth: 0,
        session_artifact_dir: None,
    }
}

fn stub_model() -> pa_agent::types::Model {
    serde_json::from_value(serde_json::json!({
        "id": "m1", "name": "M1", "api": "test", "provider": "p1",
        "baseUrl": "http://localhost", "reasoning": false,
        "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
        "contextWindow": 1000, "maxTokens": 100
    }))
    .unwrap()
}

impl SessionFeature for Stub {
    fn name(&self) -> &'static str {
        "stub"
    }

    fn register_host_handlers(
        &self,
        context: &SessionFeatureContext,
        handlers: &mut HostRequestHandlers,
    ) {
        let cwd = context.cwd.display().to_string();
        // No dot: prompt_guards scans the sources for dotted request
        // literals and requires the core prompt to document each one.
        handlers.register(
            STUB_REQUEST,
            crate::kernel::shared::host_handler(move |_| {
                let cwd = cwd.clone();
                async move { Ok(serde_json::json!({ "cwd": cwd })) }
            }),
        );
    }
}

/// A feature's default methods are no-ops, and a registered feature's
/// handlers reach the registry the engine hands it.
#[test]
fn a_feature_registers_handlers_through_the_seam() {
    let context = context("s1");
    let mut handlers = HostRequestHandlers::default();
    Stub.register_host_handlers(&context, &mut handlers);
    assert!(handlers.get(STUB_REQUEST).is_some());
}

/// A harness-render hook a stub feature offers reaches the session's
/// digest hooks, after the features that offer none.
#[test]
fn a_feature_offers_a_harness_prompt_hook_through_the_seam() {
    struct Ranked;
    impl HarnessPromptHook for Ranked {
        fn adjust(
            &self,
            _state: &crate::refinement::HarnessState,
        ) -> crate::refinement::prompt_hook::HarnessPromptAdjustment {
            crate::refinement::prompt_hook::HarnessPromptAdjustment {
                entry_rank: [("a".to_string(), -1)].into_iter().collect(),
                ..Default::default()
            }
        }
    }
    struct Hooked;
    impl SessionFeature for Hooked {
        fn name(&self) -> &'static str {
            "hooked"
        }
        fn harness_prompt_hook(
            &self,
            _context: &Arc<SessionFeatureContext>,
        ) -> Option<Arc<dyn HarnessPromptHook>> {
            Some(Arc::new(Ranked))
        }
    }
    let context = Arc::new(context("s1"));
    assert!(session_harness_prompt_hooks(&[], &context).0.is_empty());
    let features: Vec<Arc<dyn SessionFeature>> = vec![Arc::new(Stub), Arc::new(Hooked)];
    let hooks = session_harness_prompt_hooks(&features, &context);
    assert_eq!(hooks.0.len(), 1);
    let adjustment = hooks
        .adjust(&crate::refinement::empty_harness_state())
        .expect("a ranking adjustment");
    assert_eq!(adjustment.rank("a"), -1);
}

/// A feature's telemetry handle tracks catalogued events through the
/// session's client with the base properties stamped, and refuses
/// uncatalogued names and events while the opt-out switch is off.
#[tokio::test]
async fn feature_telemetry_tracks_catalogued_events_behind_the_switch() {
    let mock = Arc::new(pa_telemetry::MockSink::new());
    let mut config = pa_telemetry::TelemetryClientConfig::new("install-1");
    config.batch_size = 1;
    config.sinks = vec![Arc::clone(&mock) as Arc<dyn pa_telemetry::TelemetrySink>];
    let client = TelemetryClient::spawn(config).unwrap();
    let on = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let probe = Arc::clone(&on);
    let telemetry = FeatureTelemetry::from_wiring(&TelemetryWiring {
        client: client.clone(),
        execution_mode: Some("print".to_string()),
        now: None,
        telemetry_enabled: Some(crate::session_engine::telemetry::RecordingSwitch::test(
            Arc::new(move || probe.load(std::sync::atomic::Ordering::SeqCst)),
        )),
    });
    let mut properties = Properties::new();
    properties.set("platform", serde_json::json!("linux"));

    telemetry.track("computer_use_session_started", &properties);
    telemetry.track("not a catalogued event", &properties);
    on.store(false, std::sync::atomic::Ordering::SeqCst);
    telemetry.track("computer_use_session_started", &properties);
    client.flush().await.unwrap();

    let events = mock.events();
    let names: Vec<&str> = events.iter().map(|event| event.name.as_str()).collect();
    assert_eq!(names, ["computer_use_session_started"]);
    let mut expected = base_properties("print");
    expected.merge(&properties);
    let delivered: Vec<(&String, &serde_json::Value)> = events[0]
        .properties
        .iter()
        .filter(|(key, _)| expected.get(key).is_some())
        .collect();
    assert_eq!(delivered, expected.iter().collect::<Vec<_>>());
}

/// With no feature installed the agent loop gets no tool hooks at all:
/// the native product's loop is untouched.
#[test]
fn no_features_install_no_loop_hooks() {
    let context = Arc::new(context("s1"));
    let (before, after) = tool_call_hooks(&[], &context, &FeatureNotes::default());
    assert!(before.is_none() && after.is_none());
}

struct GateStub;

impl crate::refinement::gate::RefinementGate for GateStub {
    fn evaluate(
        &self,
        _request: crate::refinement::gate::RefinementGateRequest,
    ) -> FeatureFuture<
        anyhow::Result<Option<Box<dyn crate::refinement::gate::RefinementGateVerdict>>>,
    > {
        Box::pin(async { Ok(None) })
    }
}

struct Gating;

impl SessionFeature for Gating {
    fn name(&self) -> &'static str {
        "gating"
    }

    fn refinement_gate(
        &self,
        _context: &Arc<SessionFeatureContext>,
    ) -> Option<Arc<dyn RefinementGate>> {
        Some(Arc::new(GateStub))
    }
}

/// No feature offers a gate in the native product; with features
/// installed, the first one that offers a gate is the session's.
#[test]
fn the_first_offered_refinement_gate_is_the_sessions() {
    let context = Arc::new(context("s1"));
    assert!(session_refinement_gate(&[], &context).is_none());
    let features: Vec<Arc<dyn SessionFeature>> = vec![Arc::new(Stub), Arc::new(Gating)];
    assert!(session_refinement_gate(&features, &context).is_some());
}

struct CommandStub;

impl SessionFeature for CommandStub {
    fn name(&self) -> &'static str {
        "command-stub"
    }

    fn slash_commands(&self) -> Vec<pa_types::slash_commands::BuiltinSlashCommand> {
        vec![pa_types::slash_commands::BuiltinSlashCommand {
            name: "stub-command",
            description: "A stub command",
            execution: pa_types::slash_commands::SlashCommandExecution::Session,
            argument_hint: None,
            aliases: &[],
            takes_argument: true,
        }]
    }

    fn execute_slash_command(
        &self,
        context: &Arc<SessionFeatureContext>,
        name: &str,
        args: &str,
    ) -> Option<FeatureFuture<Result<FeatureCommandOutcome, String>>> {
        if name != "stub-command" {
            return None;
        }
        let text = format!("{} ran {args}", context.session_id);
        let fail = args == "fail";
        Some(Box::pin(async move {
            if fail {
                return Err("stub refused".to_string());
            }
            Ok(FeatureCommandOutcome {
                text,
                completion: Some(Box::pin(async { Ok("stub finished".to_string()) })),
            })
        }))
    }
}

/// The owning feature runs a slash command; nobody owns an unknown one.
#[tokio::test]
async fn a_feature_runs_its_own_slash_command() {
    let context = Arc::new(context("s1"));
    let features: Vec<Arc<dyn SessionFeature>> = vec![Arc::new(Stub), Arc::new(CommandStub)];
    assert!(execute_feature_slash_command(&features, &context, "nope", "").is_none());
    let outcome = execute_feature_slash_command(&features, &context, "stub-command", "x")
        .expect("owned")
        .await
        .expect("ran");
    assert_eq!(outcome.text, "s1 ran x");
    assert_eq!(
        outcome.completion.expect("background work").await,
        Ok("stub finished".to_string())
    );
    let refused = execute_feature_slash_command(&features, &context, "stub-command", "fail")
        .expect("owned")
        .await;
    assert_eq!(refused.err(), Some("stub refused".to_string()));
    assert_eq!(CommandStub.slash_commands()[0].name, "stub-command");
    assert!(Stub.slash_commands().is_empty());
}

/// A session's statuses reach the sink registered for it while the
/// sink lives; none reach a dropped or unregistered one.
#[test]
fn feature_statuses_reach_the_sessions_live_sink() {
    let seen: Arc<std::sync::Mutex<Vec<FeatureStatus>>> = Arc::default();
    let recorder = Arc::clone(&seen);
    let sink: FeatureStatusSink = Arc::new(move |status| recorder.lock().unwrap().push(status));
    register_feature_status_sink("status-session-1", &sink);
    let status = FeatureStatus {
        feature: "stub".to_string(),
        line: Some("stub: running".to_string()),
        status: serde_json::json!({ "phase": "running" }),
    };
    assert!(publish_feature_status("status-session-1", status.clone()));
    assert!(!publish_feature_status("status-session-2", status.clone()));
    drop(sink);
    assert!(!publish_feature_status("status-session-1", status.clone()));
    assert_eq!(*seen.lock().unwrap(), vec![status]);
}
