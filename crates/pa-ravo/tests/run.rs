//! `ravo.run` end to end through the host requests the bundled `ravo`
//! skill sends: the run starts in the background and answers at once, one
//! run per session; the controller inspects, plans, implements, is judged,
//! repairs a rejected proposal, and commits the accepted one into the
//! session's harness store with its lineage and a hash-chained archive;
//! `ravo.cancel` stops a run. The children's model is scripted.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use pa_core::features::{SessionFeature, SessionFeatureContext};
use pa_core::kernel::shared::{HostRequestHandlers, HostRequestPayload};
use pa_core::refinement::{load_harness_state, HarnessScope, RefinementKind};
use pa_ledger::ReplayCase;
use pa_ravo::{
    ModelFailure, ModelReply, RavoFeature, RavoModel, RavoOptions, ReplayEnvironment,
    ReplayOutcome, ReplayRunner,
};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

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

/// Answers each child by its prompt's heading; the judge answers from a
/// script, in order. With `hang`, every call waits for cancellation.
#[derive(Default)]
struct Scripted {
    judge: Mutex<Vec<&'static str>>,
    asked: Mutex<Vec<String>>,
    hang: bool,
}

fn memory_proposal(title: &str) -> String {
    json!({
        "summary": format!("note {title}"),
        "rationale": "seen twice",
        "expectedOutcome": "recall",
        "addressedFingerprints": [],
        "edits": [{ "action": "create", "kind": "memory", "id": "tactic", "title": title, "content": "Use tactic A", "reason": "evidence" }]
    })
    .to_string()
}

impl RavoModel for Scripted {
    fn complete(
        &self,
        prompt: String,
        _token_budget: u64,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = Result<ModelReply, ModelFailure>> + Send>> {
        let heading = prompt.lines().next().unwrap_or_default().to_string();
        self.asked.lock().unwrap().push(heading.clone());
        if self.hang {
            return Box::pin(async move {
                cancel.cancelled().await;
                Err(ModelFailure {
                    status: "aborted".to_string(),
                    tokens: 0,
                    error: None,
                })
            });
        }
        let text = match heading.as_str() {
            "# RAVO inspect" => {
                json!({ "summary": "nothing yet", "facts": ["no memory"] }).to_string()
            }
            "# RAVO plan" => json!({ "steps": ["create memory:tactic"] }).to_string(),
            "# RAVO implement" => memory_proposal("Tactic"),
            "# RAVO repair" => memory_proposal("Tactic, repaired"),
            "# RAVO judge" => self.judge.lock().unwrap().remove(0).to_string(),
            other => panic!("unexpected child {other}"),
        };
        Box::pin(async move { Ok(ModelReply { text, tokens: 100 }) })
    }
}

struct Session {
    _root: tempfile::TempDir,
    context: SessionFeatureContext,
    handlers: HostRequestHandlers,
    model: Arc<Scripted>,
}

fn session(model: Scripted, artifacts: bool) -> Session {
    session_at_depth(model, artifacts, 0)
}

fn session_at_depth(model: Scripted, artifacts: bool, rlm_depth: u32) -> Session {
    let root = tempfile::tempdir().unwrap();
    let context = SessionFeatureContext {
        agent_dir: root.path().join("agent"),
        cwd: root.path().join("work"),
        session_id: "s1".to_string(),
        python_skill_import_names: Vec::new(),
        model: serde_json::from_value(json!({
            "id": "m1", "name": "M1", "api": "test", "provider": "p1",
            "baseUrl": "http://localhost", "reasoning": false,
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
            "contextWindow": 1000, "maxTokens": 100
        }))
        .unwrap(),
        telemetry: None,
        rlm_depth,
        session_artifact_dir: artifacts.then(|| root.path().join("artifacts")),
    };
    let model = Arc::new(model);
    let shared = Arc::clone(&model);
    let feature = RavoFeature::new(RavoOptions {
        enabled: Some(true),
        runner: Arc::new(NeverRuns),
        replay_sys_path: Vec::new(),
    })
    .with_run_model(Arc::new(move |_context: &SessionFeatureContext| {
        Arc::clone(&shared) as Arc<dyn RavoModel>
    }));
    let mut handlers = HostRequestHandlers::new();
    feature.register_host_handlers(&context, &mut handlers);
    Session {
        _root: root,
        context,
        handlers,
        model,
    }
}

impl Session {
    async fn call(&self, kind: &str, data: Value) -> anyhow::Result<Value> {
        let handler = self.handlers.get(kind).expect("registered").clone();
        handler(HostRequestPayload {
            data,
            cell_source_code: None,
        })
        .await
    }

    /// Read `ravo.status` until the run settled.
    async fn settled(&self) -> Value {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let status = self.call("ravo.status", json!({})).await.unwrap();
                if matches!(status["phase"].as_str(), Some("accepted" | "stopped")) {
                    return status;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the run settles")
    }

    fn harness_dir(&self) -> std::path::PathBuf {
        self.context
            .session_artifact_dir
            .as_ref()
            .unwrap()
            .join("harness")
    }
}

/// The judge rejects the first proposal and accepts its repair: the run
/// commits the repair into the session store, records the champion and the
/// consumed evaluation, and leaves a verified archive and no checkpoint.
#[tokio::test]
async fn a_run_repairs_a_rejected_proposal_and_commits_the_repair() {
    let session = session(
        Scripted {
            judge: Mutex::new(vec![
                r#"{"verdict":"fail","score":40,"failedCriteria":["evidence"],"rationale":"thin"}"#,
                r#"{"verdict":"pass","score":80,"failedCriteria":[],"rationale":"fine"}"#,
            ]),
            ..Scripted::default()
        },
        true,
    );
    assert_eq!(
        session.call("ravo.status", json!({})).await.unwrap(),
        json!({ "phase": "idle" })
    );
    let started = session
        .call(
            "ravo.run",
            json!({ "task": "note the tactic", "max_rounds": 3 }),
        )
        .await
        .unwrap();
    assert_eq!(started["started"], json!(true));
    let run_id = started["runId"].as_str().unwrap().to_string();
    assert!(run_id.starts_with("ravo_"));
    // One run per session.
    let refused = session
        .call("ravo.run", json!({ "task": "another" }))
        .await
        .unwrap();
    assert_eq!(
        refused,
        json!({ "started": false, "reason": format!("RAVO run {run_id} is already in progress") })
    );

    let status = session.settled().await;
    assert_eq!(
        (
            &status["phase"],
            &status["stopReason"],
            &status["round"],
            &status["repairs"],
            &status["candidateId"],
            &status["lastCertificate"]["status"],
        ),
        (
            &json!("accepted"),
            &json!("accepted"),
            &json!(2),
            &json!(1),
            &json!(format!("{run_id}-p2")),
            &json!("commit"),
        )
    );
    let state = load_harness_state(&session.harness_dir(), HarnessScope::Local);
    let entry = &state.entries[&RefinementKind::Memory]["tactic"];
    assert_eq!(entry.title, "Tactic, repaired");
    assert_eq!(entry.extensions["trust"]["score"], json!(50));
    let ravo = &state.extensions["ravo"];
    assert_eq!(ravo["championId"], json!(format!("{run_id}-p2")));
    assert_eq!(
        ravo["evaluatedProposalIds"],
        json!([format!("{run_id}-p1"), format!("{run_id}-p2")])
    );
    let archive = session.harness_dir().join("ravo/archive");
    let events: Vec<Value> = std::fs::read_to_string(archive.join("events.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let kinds: Vec<&str> = events
        .iter()
        .map(|event| event["type"].as_str().unwrap())
        .collect();
    assert_eq!(kinds.first(), Some(&"run"));
    assert_eq!(kinds.last(), Some(&"accept"));
    assert!(kinds.contains(&"reject"));
    for pair in events.windows(2) {
        assert_eq!(pair[1]["prevDigest"], pair[0]["digest"]);
    }
    assert!(!session
        .harness_dir()
        .join(format!("ravo/runs/{run_id}.json"))
        .exists());
    assert_eq!(
        *session.model.asked.lock().unwrap(),
        [
            "# RAVO inspect",
            "# RAVO plan",
            "# RAVO implement",
            "# RAVO judge",
            "# RAVO plan",
            "# RAVO repair",
            "# RAVO judge",
        ]
    );
}

/// `ravo.cancel` stops the running run; with nothing running it answers
/// false.
#[tokio::test]
async fn a_run_stops_when_cancelled() {
    let session = session(
        Scripted {
            hang: true,
            ..Scripted::default()
        },
        true,
    );
    assert_eq!(
        session.call("ravo.cancel", json!({})).await.unwrap(),
        json!({ "cancelled": false })
    );
    let started = session
        .call("ravo.run", json!({ "task": "note the tactic" }))
        .await
        .unwrap();
    assert_eq!(started["started"], json!(true));
    // Let the run reach its first child.
    tokio::time::timeout(Duration::from_secs(30), async {
        while session.model.asked.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        session.call("ravo.cancel", json!({})).await.unwrap(),
        json!({ "cancelled": true })
    );
    let status = session.settled().await;
    assert_eq!(
        (&status["phase"], &status["stopReason"]),
        (&json!("stopped"), &json!("cancelled"))
    );
}

/// Malformed requests fail the host request with the TS messages; the
/// ARC-AGI evaluator is not part of the product.
#[tokio::test]
async fn requests_are_validated_and_runs_need_a_session_store() {
    let session = session(Scripted::default(), true);
    for (payload, message) in [
        (json!({}), "ravo.run task must be a non-empty string"),
        (
            json!({ "task": "x", "global": "yes" }),
            "ravo.run global must be a boolean when provided",
        ),
        (
            json!({ "task": "x", "max_rounds": 0 }),
            "ravo.run max_rounds must be a positive integer when provided",
        ),
        (
            json!({ "task": "x", "arc_agi": { "repo_dir": "/r", "game": "ls20" } }),
            "ravo.run arc_agi is not available: the ARC-AGI evaluator is not part of this build",
        ),
    ] {
        assert_eq!(
            session
                .call("ravo.run", payload)
                .await
                .unwrap_err()
                .to_string(),
            message
        );
    }
}

/// Like TS (`_autoRefineAllowedForSession`), the `ravo.*` requests are
/// registered only where refine is: a top-level session with a local
/// harness store. Elsewhere the kernel's calls fail as unregistered.
#[test]
fn only_a_top_level_session_with_a_store_gets_the_requests() {
    let names = |session: &Session| {
        let mut names: Vec<String> = ["ravo.run", "ravo.status", "ravo.cancel"]
            .into_iter()
            .filter(|kind| session.handlers.get(kind).is_some())
            .map(str::to_string)
            .collect();
        names.sort();
        names
    };
    let top = session(Scripted::default(), true);
    assert_eq!(names(&top), ["ravo.cancel", "ravo.run", "ravo.status"]);
    let storeless = session(Scripted::default(), false);
    assert_eq!(names(&storeless), Vec::<String>::new());
    let child = session_at_depth(Scripted::default(), true, 1);
    assert_eq!(names(&child), Vec::<String>::new());
}
