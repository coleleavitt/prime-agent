//! The `brain.*` host requests the bundled `brain` skill sends. File and
//! index work runs on the blocking pool; synthesis is the one async call.

use std::path::{Path, PathBuf};
use std::sync::PoisonError;

use anyhow::{Context as _, bail};
use pa_core::features::SessionFeatureContext;
use pa_core::kernel::shared::{HostRequestHandlers, host_handler};
use pa_core::session_engine::side_model::{SideBill, SideModel, SideRequest};
use serde_json::{Value, json};

use super::controls::{self, ScopeSummary};
use super::project::{Project, project_of};
use super::search::{DEFAULT_LIMIT, Scope, SearchContext, SearchRequest, shared_brain};
use super::synth::{self, RunLock, SynthScope};
use crate::handle::{RerankerHandle, shared_handle_for};

/// The longest query accepted.
const MAX_QUERY_CHARS: usize = 2_000;

/// What every handler of one session shares.
#[derive(Clone)]
pub(crate) struct BrainSession {
    pub(crate) agent_dir: PathBuf,
    pub(crate) cwd: PathBuf,
    pub(crate) session_id: String,
    /// The side model synthesis uses, and its selector.
    pub(crate) model: Option<(SideModel, String)>,
}

impl BrainSession {
    /// The session of `context`; synthesis uses `auxiliaryModel` when set,
    /// else the session's own model.
    pub(crate) fn of(context: &SessionFeatureContext) -> Self {
        let selector = pa_core::settings::SettingsManager::create(&context.cwd, &context.agent_dir)
            .get_auxiliary_model()
            .map(str::to_string)
            .unwrap_or_else(|| format!("{}/{}", context.model.provider, context.model.id));
        Self {
            agent_dir: context.agent_dir.clone(),
            cwd: context.cwd.clone(),
            session_id: context.session_id.clone(),
            model: Some((
                SideModel::Selector {
                    selector: selector.clone(),
                    agent_dir: context.agent_dir.clone(),
                    cwd: context.cwd.clone(),
                },
                selector,
            )),
        }
    }

    fn project(&self) -> Project {
        project_of(&self.cwd)
    }
}

fn learned(handle: Option<&RerankerHandle>) -> impl Fn(&str) -> f32 + '_ {
    move |id: &str| handle.map_or(0.0, |handle| handle.learned_prior(id))
}

fn reranker(agent_dir: &Path) -> Option<RerankerHandle> {
    shared_handle_for(agent_dir).ok()
}

fn string<'a>(data: &'a Value, field: &str) -> Option<&'a str> {
    data.get(field).and_then(Value::as_str).map(str::trim)
}

fn required<'a>(data: &'a Value, field: &str, request: &str) -> anyhow::Result<&'a str> {
    string(data, field)
        .filter(|value| !value.is_empty())
        .with_context(|| format!("{request} needs a non-empty string `{field}`"))
}

/// `brain.search`.
pub(crate) fn search(session: &BrainSession, data: &Value) -> anyhow::Result<Value> {
    let query = required(data, "query", "brain.search")?;
    if query.chars().count() > MAX_QUERY_CHARS {
        bail!("brain.search query must be at most {MAX_QUERY_CHARS} characters");
    }
    let scope_text = string(data, "scope").unwrap_or("all");
    let scope = Scope::parse(scope_text).with_context(|| {
        format!(
            "brain.search scope must be one of all, project, global, session, files, \
             magic-context, history (got {scope_text:?})"
        )
    })?;
    let limit = data
        .get("limit")
        .and_then(Value::as_u64)
        .map_or(DEFAULT_LIMIT, |limit| {
            usize::try_from(limit).unwrap_or(usize::MAX)
        });
    let include_superseded = data
        .get("includeSuperseded")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let state = controls::read(&session.agent_dir);
    let project = session.project();
    let handle = reranker(&session.agent_dir);
    let learned = learned(handle.as_ref());
    let brain = shared_brain(&session.agent_dir);
    let mut brain = brain.lock().unwrap_or_else(PoisonError::into_inner);
    brain.refresh(&state.controls.superseded);
    let hits = brain.search(
        &SearchRequest {
            query: query.to_string(),
            scope,
            limit,
            include_superseded,
        },
        &SearchContext {
            project: &project,
            session_id: &session.session_id,
            controls: &state.controls,
            learned: &learned,
        },
    );
    Ok(json!({ "results": hits, "project": project.identity }))
}

/// `brain.get`.
pub(crate) fn get(session: &BrainSession, data: &Value) -> anyhow::Result<Value> {
    let key = required(data, "key", "brain.get")?;
    let state = controls::read(&session.agent_dir);
    let brain = shared_brain(&session.agent_dir);
    let mut brain = brain.lock().unwrap_or_else(PoisonError::into_inner);
    brain.refresh(&state.controls.superseded);
    let doc = brain
        .get(key)
        .with_context(|| format!("brain.get: no memory {key:?}"))?;
    let mut memory = serde_json::to_value(doc)?;
    memory["pinned"] = json!(state.controls.pinned.contains(key));
    memory["excluded"] = json!(state.controls.excluded.contains(key));
    if let Some(superseded) = brain.superseded().get(key) {
        memory["superseded"] = serde_json::to_value(superseded)?;
    }
    Ok(json!({ "memory": memory }))
}

/// The control operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ControlOp {
    Pin,
    Unpin,
    Exclude,
    Include,
    Supersede,
}

impl ControlOp {
    fn request(self) -> &'static str {
        match self {
            Self::Pin => PIN_REQUEST,
            Self::Unpin => UNPIN_REQUEST,
            Self::Exclude => EXCLUDE_REQUEST,
            Self::Include => INCLUDE_REQUEST,
            Self::Supersede => SUPERSEDE_REQUEST,
        }
    }
}

/// `brain.pin` / `unpin` / `exclude` / `include` / `supersede`.
pub(crate) fn control(
    session: &BrainSession,
    op: ControlOp,
    data: &Value,
) -> anyhow::Result<Value> {
    let request = op.request();
    let key = required(data, "key", request)?.to_string();
    let by = match op {
        ControlOp::Supersede => string(data, "by")
            .filter(|by| !by.is_empty())
            .map(str::to_string),
        _ => None,
    };
    {
        let state = controls::read(&session.agent_dir);
        let brain = shared_brain(&session.agent_dir);
        let mut brain = brain.lock().unwrap_or_else(PoisonError::into_inner);
        brain.refresh(&state.controls.superseded);
        let known = |key: &str| brain.get(key).is_some();
        let removing = matches!(op, ControlOp::Unpin | ControlOp::Include)
            || (op == ControlOp::Supersede && by.is_none());
        if !removing && !known(&key) {
            bail!("{request}: no memory {key:?} (brain.search lists keys)");
        }
        if let Some(by) = by.as_deref() {
            if !known(by) {
                bail!("{request}: no memory {by:?} to supersede with");
            }
            if by == key {
                bail!("{request}: a memory cannot supersede itself");
            }
        }
    }
    let controls = controls::update(&session.agent_dir, |brain| {
        let controls = &mut brain.controls;
        match op {
            ControlOp::Pin => {
                controls.excluded.remove(&key);
                controls.pinned.insert(key.clone());
            }
            ControlOp::Unpin => {
                controls.pinned.remove(&key);
            }
            ControlOp::Exclude => {
                controls.pinned.remove(&key);
                controls.excluded.insert(key.clone());
            }
            ControlOp::Include => {
                controls.excluded.remove(&key);
            }
            ControlOp::Supersede => match by.clone() {
                Some(by) => {
                    controls.superseded.insert(key.clone(), by);
                }
                None => {
                    controls.superseded.remove(&key);
                }
            },
        }
        controls.updated_at = Some(pa_ledger::now_iso());
        controls.clone()
    })?;
    Ok(json!({ "controls": controls }))
}

/// `brain.summary`.
pub(crate) fn summary(session: &BrainSession) -> Value {
    let state = controls::read(&session.agent_dir);
    let project = session.project();
    let key = project.summary_key();
    json!({
        "projectKey": key,
        "project": state.synthesis.scopes.get(&key),
        "global": state.synthesis.scopes.get(controls::GLOBAL_SCOPE),
    })
}

/// `brain.status`.
pub(crate) fn status(session: &BrainSession) -> Value {
    let state = controls::read(&session.agent_dir);
    let project = session.project();
    let brain = shared_brain(&session.agent_dir);
    let mut brain = brain.lock().unwrap_or_else(PoisonError::into_inner);
    brain.refresh(&state.controls.superseded);
    let mut by_source = serde_json::Map::new();
    let mut in_project = 0_usize;
    for doc in brain.docs() {
        let count = by_source
            .entry(doc.source.as_str())
            .or_insert_with(|| json!(0));
        *count = json!(count.as_u64().unwrap_or(0) + 1);
        if project.contains(doc.project.as_deref(), doc.slug.as_deref()) {
            in_project += 1;
        }
    }
    json!({
        "memories": by_source,
        "projectMemories": in_project,
        "superseded": brain.superseded().len(),
        "pinned": state.controls.pinned.len(),
        "excluded": state.controls.excluded.len(),
        "project": project.identity,
        "projectSlug": project.slug,
        "summaries": state.synthesis.scopes.keys().collect::<Vec<_>>(),
    })
}

/// Synthesize `scope` now: the side-model call, then the stored summary
/// (or the failure count). `manual` resets the failure count first.
pub(crate) async fn synthesize(
    session: BrainSession,
    scope: SynthScope,
    manual: bool,
) -> anyhow::Result<ScopeSummary> {
    let (model, selector) = session
        .model
        .clone()
        .context("brain.synthesize: no model to synthesize with")?;
    let Some(lock) = RunLock::acquire(&session.agent_dir) else {
        bail!("brain.synthesize: another synthesis is running");
    };
    let key = scope.key();
    let prepared = {
        let session = session.clone();
        let scope = scope.clone();
        tokio::task::spawn_blocking(move || {
            let state = controls::read(&session.agent_dir);
            let brain = shared_brain(&session.agent_dir);
            let mut brain = brain.lock().unwrap_or_else(PoisonError::into_inner);
            brain.refresh(&state.controls.superseded);
            let docs = synth::select(brain.docs(), &scope, &state.controls, brain.superseded());
            let previous = state
                .synthesis
                .scopes
                .get(&scope.key())
                .map(|summary| summary.lines.clone())
                .unwrap_or_default();
            synth::prompt(&scope, &previous, &docs)
        })
        .await?
    };
    let (prompt, used) = prepared;
    if manual {
        controls::update(&session.agent_dir, |brain| {
            if let Some(summary) = brain.synthesis.scopes.get_mut(&key) {
                summary.failures = 0;
            }
        })?;
    }
    let now = pa_ledger::now_iso();
    let reply = model
        .complete(
            SideRequest::single(synth::SYNTHESIS_SYSTEM_PROMPT, prompt),
            SideBill {
                session_id: &session.session_id,
                source: "brain",
            },
        )
        .await;
    let result = match reply {
        Ok(reply) => {
            let summary = ScopeSummary {
                lines: synth::parse(&reply),
                synthesized: true,
                model: selector,
                updated_at: now.clone(),
                source_count: used,
                failures: 0,
                last_attempt_at: Some(now),
            };
            controls::update(&session.agent_dir, |brain| {
                brain.synthesis.scopes.insert(key.clone(), summary.clone());
            })?;
            Ok(summary)
        }
        Err(error) => {
            controls::update(&session.agent_dir, |brain| {
                let summary = brain.synthesis.scopes.entry(key.clone()).or_default();
                summary.failures += 1;
                summary.last_attempt_at = Some(now.clone());
            })?;
            Err(error.context(format!("brain.synthesize ({selector}) failed")))
        }
    };
    drop(lock);
    result
}

/// `brain.search`.
pub(crate) const SEARCH_REQUEST: &str = "brain.search";
/// `brain.get`.
pub(crate) const GET_REQUEST: &str = "brain.get";
/// `brain.pin`.
pub(crate) const PIN_REQUEST: &str = "brain.pin";
/// `brain.unpin`.
pub(crate) const UNPIN_REQUEST: &str = "brain.unpin";
/// `brain.exclude`.
pub(crate) const EXCLUDE_REQUEST: &str = "brain.exclude";
/// `brain.include`.
pub(crate) const INCLUDE_REQUEST: &str = "brain.include";
/// `brain.supersede`.
pub(crate) const SUPERSEDE_REQUEST: &str = "brain.supersede";
/// `brain.summary`.
pub(crate) const SUMMARY_REQUEST: &str = "brain.summary";
/// `brain.synthesize`.
pub(crate) const SYNTHESIZE_REQUEST: &str = "brain.synthesize";
/// `brain.status`.
pub(crate) const STATUS_REQUEST: &str = "brain.status";

fn blocking(
    handlers: &mut HostRequestHandlers,
    request: &'static str,
    session: &BrainSession,
    run: fn(&BrainSession, &Value) -> anyhow::Result<Value>,
) {
    let session = session.clone();
    handlers.register(
        request,
        host_handler(move |payload| {
            let session = session.clone();
            async move {
                tokio::task::spawn_blocking(move || run(&session, &payload.data))
                    .await
                    .with_context(|| format!("{request} did not finish"))?
            }
        }),
    );
}

/// Register every `brain.*` request for one session.
pub(crate) fn register(session: &BrainSession, handlers: &mut HostRequestHandlers) {
    blocking(handlers, SEARCH_REQUEST, session, search);
    blocking(handlers, GET_REQUEST, session, get);
    blocking(handlers, PIN_REQUEST, session, |session, data| {
        control(session, ControlOp::Pin, data)
    });
    blocking(handlers, UNPIN_REQUEST, session, |session, data| {
        control(session, ControlOp::Unpin, data)
    });
    blocking(handlers, EXCLUDE_REQUEST, session, |session, data| {
        control(session, ControlOp::Exclude, data)
    });
    blocking(handlers, INCLUDE_REQUEST, session, |session, data| {
        control(session, ControlOp::Include, data)
    });
    blocking(handlers, SUPERSEDE_REQUEST, session, |session, data| {
        control(session, ControlOp::Supersede, data)
    });
    blocking(handlers, SUMMARY_REQUEST, session, |session, _| {
        Ok(summary(session))
    });
    blocking(handlers, STATUS_REQUEST, session, |session, _| {
        Ok(status(session))
    });
    let synth_session = session.clone();
    handlers.register(
        SYNTHESIZE_REQUEST,
        host_handler(move |payload| {
            let session = synth_session.clone();
            async move {
                let scope = match string(&payload.data, "scope").unwrap_or("project") {
                    "project" => SynthScope::Project(
                        tokio::task::spawn_blocking({
                            let cwd = session.cwd.clone();
                            move || project_of(&cwd)
                        })
                        .await?,
                    ),
                    "global" => SynthScope::Global,
                    other => {
                        bail!("brain.synthesize scope must be project or global (got {other:?})")
                    }
                };
                let summary = synthesize(session, scope, true).await?;
                Ok(json!({ "summary": summary }))
            }
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brain::corpus::tests::write_harness;

    fn session(agent: &Path) -> BrainSession {
        BrainSession {
            agent_dir: agent.to_path_buf(),
            cwd: agent.to_path_buf(),
            session_id: "s1".to_string(),
            model: None,
        }
    }

    #[test]
    fn controls_validate_keys_and_persist() {
        let agent = tempfile::tempdir().unwrap();
        write_harness(
            &agent.path().join("harness"),
            &[
                ("a", "A", "alpha memory", json!({})),
                ("b", "B", "beta memory", json!({})),
            ],
        );
        let session = session(agent.path());
        let error = control(&session, ControlOp::Pin, &json!({"key": "global:zzz"})).unwrap_err();
        assert!(error.to_string().contains("no memory"));
        control(&session, ControlOp::Pin, &json!({"key": "global:a"})).unwrap();
        let reply = control(&session, ControlOp::Exclude, &json!({"key": "global:a"})).unwrap();
        assert_eq!(reply["controls"]["excluded"], json!(["global:a"]));
        assert!(reply["controls"].get("pinned").is_none(), "exclude unpins");
        assert!(
            control(
                &session,
                ControlOp::Supersede,
                &json!({"key": "global:a", "by": "global:a"})
            )
            .is_err()
        );
        control(
            &session,
            ControlOp::Supersede,
            &json!({"key": "global:a", "by": "global:b"}),
        )
        .unwrap();
        let fetched = get(&session, &json!({"key": "global:a"})).unwrap();
        assert_eq!(fetched["memory"]["superseded"]["by"], "global:b");
        assert_eq!(fetched["memory"]["excluded"], true);
        control(&session, ControlOp::Supersede, &json!({"key": "global:a"})).unwrap();
        assert!(controls::read(agent.path()).controls.superseded.is_empty());
    }

    #[test]
    fn search_validates_and_returns_hits() {
        let agent = tempfile::tempdir().unwrap();
        write_harness(
            &agent.path().join("harness"),
            &[("a", "Toolchain", "build with rustc-rdr-main", json!({}))],
        );
        let session = session(agent.path());
        assert!(search(&session, &json!({})).is_err());
        assert!(search(&session, &json!({"query": "x", "scope": "everywhere"})).is_err());
        let reply = search(&session, &json!({"query": "rustc toolchain"})).unwrap();
        assert_eq!(reply["results"][0]["key"], "global:a");
        assert_eq!(reply["results"][0]["source"], "global");
        let status = status(&session);
        assert_eq!(status["memories"]["global"], 1);
    }

    fn scripted_session(
        agent: &Path,
        provider: &std::sync::Arc<pa_agent::scripted::ScriptedProvider>,
        model: pa_agent::types::Model,
    ) -> BrainSession {
        BrainSession {
            model: Some((
                SideModel::Session {
                    stream_fn: provider.stream_fn(),
                    model,
                },
                "test/m".to_string(),
            )),
            ..session(agent)
        }
    }

    fn test_model() -> pa_agent::types::Model {
        pa_agent::types::Model {
            id: "m".into(),
            name: "m".into(),
            api: "test".into(),
            provider: "test".into(),
            base_url: "http://localhost".into(),
            reasoning: false,
            cost: pa_agent::types::UsageCost::default(),
            context_window: 100_000,
            max_tokens: 1_000,
            max_tokens_explicit: false,
        }
    }

    /// Synthesis end to end: the scope's memories reach the model, the
    /// guarded reply is stored, the digest hook renders it, a failure counts.
    #[tokio::test]
    async fn synthesis_stores_a_guarded_summary_the_digest_renders() {
        let agent = tempfile::tempdir().unwrap();
        write_harness(
            &agent.path().join("harness"),
            &[
                (
                    "tool",
                    "Toolchain",
                    "build with rustc-rdr-main and mold",
                    json!({}),
                ),
                ("old_part_1", "Old", "first handoff", json!({})),
                ("old_part_2", "New", "second handoff", json!({})),
            ],
        );
        let provider = std::sync::Arc::new(pa_agent::scripted::ScriptedProvider::new(test_model()));
        provider
            .push_text_turn("- Builds use rustc-rdr-main and mold.\n- token = hunter2hunter2\n");
        let session = scripted_session(agent.path(), &provider, test_model());

        let summary = synthesize(session.clone(), SynthScope::Global, true)
            .await
            .unwrap();
        assert_eq!(summary.lines, ["Builds use rustc-rdr-main and mold."]);
        assert!(summary.synthesized);
        assert_eq!(summary.model, "test/m");
        assert_eq!(summary.source_count, 2, "the superseded part is not read");
        let calls = provider.calls();
        let sent = format!("{:?}", calls[0].messages);
        assert!(sent.contains("global:tool") && sent.contains("global:old_part_2"));
        assert!(!sent.contains("global:old_part_1"));

        let stored = controls::read(agent.path());
        assert_eq!(stored.synthesis.scopes[controls::GLOBAL_SCOPE], summary);
        let hook = crate::brain::BrainPromptHook {
            agent_dir: agent.path().to_path_buf(),
            session_id: "s1".to_string(),
            project: project_of(agent.path()),
            state: std::sync::Arc::default(),
        };
        let merged = pa_core::refinement::merge_harness_states(
            &pa_core::refinement::load_harness_state(
                &agent.path().join("harness"),
                pa_core::refinement::HarnessScope::Global,
            ),
            None,
        );
        let adjustment =
            pa_core::refinement::prompt_hook::HarnessPromptHook::adjust(&hook, &merged);
        assert_eq!(adjustment.sections.len(), 1);
        assert_eq!(adjustment.sections[0].lines, summary.lines);
        assert_eq!(adjustment.withheld[0].label, "superseded");

        provider.push_fail_start_turn("provider down");
        let error = synthesize(session, SynthScope::Global, false)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("brain.synthesize"));
        let after = controls::read(agent.path()).synthesis.scopes[controls::GLOBAL_SCOPE].clone();
        assert_eq!(after.failures, 1);
        assert_eq!(
            after.lines, summary.lines,
            "a failure keeps the last summary"
        );
    }

    /// The feature registers every request, and they answer through the
    /// handler table the kernel bridge dispatches from.
    #[tokio::test]
    async fn the_feature_serves_the_brain_requests() {
        use pa_core::features::SessionFeature as _;
        let agent = tempfile::tempdir().unwrap();
        write_harness(
            &agent.path().join("harness"),
            &[("tool", "Toolchain", "build with rustc-rdr-main", json!({}))],
        );
        let context = SessionFeatureContext {
            agent_dir: agent.path().to_path_buf(),
            cwd: agent.path().to_path_buf(),
            session_id: "s1".to_string(),
            python_skill_import_names: Vec::new(),
            model: test_model(),
            telemetry: None,
            rlm_depth: 0,
            session_artifact_dir: None,
            daemonless: true,
        };
        let mut handlers = HostRequestHandlers::new();
        crate::RetrievalFeature::new().register_host_handlers(&context, &mut handlers);
        for request in [
            SEARCH_REQUEST,
            GET_REQUEST,
            PIN_REQUEST,
            UNPIN_REQUEST,
            EXCLUDE_REQUEST,
            INCLUDE_REQUEST,
            SUPERSEDE_REQUEST,
            SUMMARY_REQUEST,
            SYNTHESIZE_REQUEST,
            STATUS_REQUEST,
        ] {
            assert!(handlers.get(request).is_some(), "{request} is registered");
        }
        let search = handlers.get(SEARCH_REQUEST).unwrap().clone();
        let reply = search(pa_core::kernel::shared::HostRequestPayload {
            data: json!({"query": "toolchain"}),
        })
        .await
        .unwrap();
        assert_eq!(reply["results"][0]["key"], "global:tool");
        let pin = handlers.get(PIN_REQUEST).unwrap().clone();
        pin(pa_core::kernel::shared::HostRequestPayload {
            data: json!({"key": "global:tool"}),
        })
        .await
        .unwrap();
        assert!(
            controls::read(agent.path())
                .controls
                .pinned
                .contains("global:tool")
        );
    }

    #[tokio::test]
    async fn synthesis_without_a_model_is_refused() {
        let agent = tempfile::tempdir().unwrap();
        let error = synthesize(session(agent.path()), SynthScope::Global, true)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("no model"));
    }
}
