//! The auto-compaction context-token cap (`compaction.maxContextTokens`,
//! upstream #2100) at the session layer: the `/context-limit` session
//! override, persisted as a model-invisible `context_limit_state` custom
//! entry and restored from the branch on resume and tree navigation; the
//! precedence session > project > global (the settings merge orders the
//! latter two); the once-per-session clamp notice; and the status text
//! `/context-limit` answers with. The cap itself is a native compaction
//! setting ([`compaction::resolve_context_cap`]).

use pa_types::session::{CustomMessage, FileEntry};
use pa_types::sync::MutexExt;

use super::AgentSession;
use super::compaction::{self, CompactionSettings};
use super::messages::{CONTEXT_CAP_CLAMP_NOTICE_CUSTOM_TYPE, create_context_cap_clamp_notice};

/// The durable session-override entry (`{ "maxContextTokens": n | null }`;
/// `null` records an explicit `/context-limit off`).
pub const CONTEXT_LIMIT_STATE_CUSTOM_TYPE: &str = "context_limit_state";

/// Where the cap in force comes from (TS `ContextLimitSource`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContextLimitSource {
    /// The session's own `/context-limit` override.
    Chat,
    Project,
    Global,
    #[default]
    None,
}

impl ContextLimitSource {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Project => "project",
            Self::Global => "global",
            Self::None => "none",
        }
    }
}

/// The session's cap layer: the override, which settings layer supplied the
/// configured cap, and whether the clamp notice already showed.
#[derive(Debug, Default)]
pub(crate) struct ContextLimitState {
    session_override: Option<u64>,
    settings_source: ContextLimitSource,
    clamp_noticed: bool,
}

/// The override the latest valid `context_limit_state` entry on a branch
/// leaves in force: `None` without one, or after an explicit `off` (`null`).
fn persisted_override<'a>(branch: impl DoubleEndedIterator<Item = &'a FileEntry>) -> Option<u64> {
    branch
        .rev()
        .find_map(|entry| {
            let FileEntry::Custom { payload, .. } = entry else {
                return None;
            };
            if payload.custom_type != CONTEXT_LIMIT_STATE_CUSTOM_TYPE {
                return None;
            }
            match payload.data.as_ref()?.get("maxContextTokens")? {
                serde_json::Value::Null => Some(None),
                value => value.as_u64().filter(|tokens| *tokens > 0).map(Some),
            }
        })
        .flatten()
}

/// One `/context-limit` status snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextLimitStatus {
    /// The model's context window; 0 when unknown.
    pub context_window: u64,
    pub reserve_tokens: u64,
    /// The configured cap before the clamp.
    pub max_context_tokens: Option<u64>,
    pub source: ContextLimitSource,
    pub resolved: Option<compaction::ResolvedContextCap>,
    /// The context-token point auto-compaction fires at; `None` without a
    /// known window.
    pub compact_at: Option<u64>,
    pub enabled: bool,
}

impl ContextLimitStatus {
    /// The status lines `/context-limit` answers with (TS
    /// `showContextLimitStatus`).
    #[must_use]
    pub fn render(&self, header: Option<&str>) -> String {
        let mut lines: Vec<String> = header.map(str::to_string).into_iter().collect();
        lines.push(if self.context_window > 0 {
            format!("Model context window: {} tokens", self.context_window)
        } else {
            "Model context window: unknown tokens".to_string()
        });
        lines.push(format!("Reserve tokens: {}", self.reserve_tokens));
        match self.max_context_tokens {
            None => lines.push("Configured cap: none".to_string()),
            Some(tokens) => {
                lines.push(format!(
                    "Configured cap: {tokens} tokens ({})",
                    self.source.as_str()
                ));
                if let Some(resolved) = self.resolved.filter(|resolved| resolved.clamped) {
                    lines.push(format!(
                        "Cap raised to {} tokens (below keepRecentTokens + reserveTokens + {})",
                        resolved.cap,
                        compaction::CONTEXT_CAP_FLOOR_MARGIN
                    ));
                }
            }
        }
        lines.push(match self.compact_at {
            None => "Auto-compacts at: unknown (no context window)".to_string(),
            Some(tokens) if self.enabled => format!("Auto-compacts at: {tokens} tokens"),
            Some(tokens) => {
                format!("Auto-compacts at: {tokens} tokens (auto-compaction disabled)")
            }
        });
        lines.join("\n")
    }
}

impl AgentSession {
    /// Apply the session override over the settings-resolved compaction
    /// settings (the `compaction_settings()` read path).
    pub(super) fn apply_context_limit(&self, settings: CompactionSettings) -> CompactionSettings {
        match self.context_limit.lock_or_recover().session_override {
            Some(tokens) => CompactionSettings {
                max_context_tokens: Some(tokens),
                ..settings
            },
            None => settings,
        }
    }

    /// Record which settings layer supplied `compaction.maxContextTokens`
    /// (the engine build reads the layers; the merge already picked the
    /// value).
    pub fn set_context_limit_settings_source(&self, source: ContextLimitSource) {
        self.context_limit.lock_or_recover().settings_source = source;
    }

    /// The session's `/context-limit` override, if any.
    #[must_use]
    pub fn session_context_limit(&self) -> Option<u64> {
        self.context_limit.lock_or_recover().session_override
    }

    /// Restore the override and the clamp-notice flag from the current
    /// branch (resume and tree navigation): the latest valid
    /// `context_limit_state` wins, and a branch already carrying the clamp
    /// notice does not show it again.
    pub async fn restore_context_limit_from_branch(&self) {
        let (restored, noticed) = {
            let session = self.session.lock().await;
            let branch = session.get_branch(None);
            let noticed = branch.iter().any(|entry| {
                matches!(
                    entry,
                    FileEntry::CustomMessage { payload, .. }
                        if payload.custom_type == CONTEXT_CAP_CLAMP_NOTICE_CUSTOM_TYPE
                )
            });
            (persisted_override(branch.into_iter()), noticed)
        };
        let mut state = self.context_limit.lock_or_recover();
        state.session_override = restored;
        state.clamp_noticed |= noticed;
    }

    /// Set (`Some`) or clear (`None`) the session override, persisting it
    /// first as a model-invisible custom entry so a resumed session keeps
    /// it.
    ///
    /// # Errors
    ///
    /// The session append's I/O error; the live override is then unchanged.
    pub async fn set_session_context_limit(&self, tokens: Option<u64>) -> std::io::Result<()> {
        self.session.lock().await.append_custom_entry(
            CONTEXT_LIMIT_STATE_CUSTOM_TYPE,
            Some(serde_json::json!({ "maxContextTokens": tokens })),
        )?;
        self.context_limit.lock_or_recover().session_override = tokens;
        Ok(())
    }

    /// The status snapshot against `model` (its window and the request's
    /// output budget decide the compact-at point).
    #[must_use]
    pub fn context_limit_status(
        &self,
        model: &pa_types::ai::Model,
        max_output_tokens: u64,
    ) -> ContextLimitStatus {
        let settings = self.compaction_settings();
        let source = {
            let state = self.context_limit.lock_or_recover();
            if state.session_override.is_some() {
                ContextLimitSource::Chat
            } else if settings.max_context_tokens.is_some() {
                state.settings_source
            } else {
                ContextLimitSource::None
            }
        };
        ContextLimitStatus {
            context_window: model.context_window,
            reserve_tokens: settings.reserve_tokens,
            max_context_tokens: settings.max_context_tokens,
            source,
            resolved: compaction::resolve_context_cap(&settings),
            compact_at: (model.context_window > 0).then(|| {
                compaction::compaction_threshold(model.context_window, max_output_tokens, &settings)
            }),
            enabled: settings.enabled,
        }
    }

    /// The clamp notice, once per session: `Some` only while compaction is
    /// enabled, the cap in force was raised to the anti-thrash floor, and
    /// neither this process nor the branch has shown it. The row is
    /// display-only (`convert_to_llm` drops it); the caller persists and
    /// broadcasts it.
    pub fn take_context_cap_clamp_notice(&self) -> Option<CustomMessage> {
        let settings = self.compaction_settings();
        if !settings.enabled {
            return None;
        }
        let resolved = compaction::resolve_context_cap(&settings)?;
        let configured = settings.max_context_tokens?;
        if !resolved.clamped {
            return None;
        }
        let mut state = self.context_limit.lock_or_recover();
        if state.clamp_noticed {
            return None;
        }
        state.clamp_noticed = true;
        Some(create_context_cap_clamp_notice(
            configured,
            settings.keep_recent_tokens,
            settings.reserve_tokens,
            resolved.cap,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn custom(data: &serde_json::Value) -> FileEntry {
        serde_json::from_value(serde_json::json!({
            "type": "custom",
            "id": "c1",
            "parentId": null,
            "timestamp": "2026-10-05T00:00:00.000Z",
            "customType": CONTEXT_LIMIT_STATE_CUSTOM_TYPE,
            "data": data,
        }))
        .unwrap()
    }

    #[test]
    fn the_latest_valid_override_entry_wins() {
        let entries = [
            custom(&serde_json::json!({ "maxContextTokens": 100_000 })),
            custom(&serde_json::json!({ "maxContextTokens": 0 })),
            custom(&serde_json::json!({ "maxContextTokens": "lots" })),
        ];
        assert_eq!(persisted_override(entries.iter()), Some(100_000));
        let cleared = [
            custom(&serde_json::json!({ "maxContextTokens": 100_000 })),
            custom(&serde_json::json!({ "maxContextTokens": null })),
        ];
        assert_eq!(persisted_override(cleared.iter()), None);
        assert_eq!(persisted_override([].iter()), None);
    }

    /// One engine over a persisted session in `root`; `session_file`
    /// reopens an existing one (resume).
    async fn engine(
        root: &std::path::Path,
        session_file: Option<&std::path::Path>,
    ) -> crate::session_engine::engine::SessionEngine {
        let agent_dir = root.join("agent");
        let sessions_dir = agent_dir.join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let session_manager = if let Some(file) = session_file {
            crate::session::manager::SessionManager::open(root, &sessions_dir, file)
        } else {
            let mut manager =
                crate::session::manager::SessionManager::persisted(root, &sessions_dir);
            manager.new_session(&crate::session::manager::NewSessionOptions {
                id: None,
                parent_session: None,
                rlm_depth: Some(0),
            });
            manager
        };
        crate::session_engine::engine::create_session(
            crate::session_engine::engine::SessionEngineConfig {
                cwd: root.to_path_buf(),
                agent_dir,
                model: Some(serde_json::from_value(long_window_model_json()).unwrap()),
                stream_fn: Some(
                    std::sync::Arc::new(pa_agent::scripted::ScriptedProvider::new(
                        serde_json::from_value(long_window_model_json()).unwrap(),
                    ))
                    .stream_fn(),
                ),
                session_manager: Some(session_manager),
                rlm_depth: Some(0),
                ..Default::default()
            },
        )
        .await
        .unwrap()
    }

    fn long_window_model_json() -> serde_json::Value {
        serde_json::json!({
            "id": "long", "name": "long", "api": "openai-completions", "provider": "test",
            "baseUrl": "http://localhost", "reasoning": false, "input": ["text"],
            "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 },
            "contextWindow": 1_048_576, "maxTokens": 32_000
        })
    }

    /// Run one `/context-limit` through the session-command executor and
    /// return its result row text.
    async fn context_limit(
        engine: &crate::session_engine::engine::SessionEngine,
        args: &str,
    ) -> String {
        let model: pa_types::ai::Model = serde_json::from_value(long_window_model_json()).unwrap();
        let mut autonomous = crate::autonomous::create_autonomous_runtime_state(None, None);
        let text = if args.is_empty() {
            "/context-limit".to_string()
        } else {
            format!("/context-limit {args}")
        };
        let command = crate::session_engine::slash_commands::parse_session_command(
            &crate::session_engine::slash_commands::SlashCommandRegistry::builtin(),
            &text,
        )
        .expect("a session command");
        let execution = crate::session_engine::session_commands::execute_session_command(
            engine,
            &mut crate::session_engine::session_commands::SessionCommandParams {
                model: &model,
                api_key: None,
                global_harness_dir: crate::refinement::get_global_harness_state_dir(
                    &engine.feature_context.agent_dir,
                ),
                autonomous: &mut autonomous,
            },
            &command,
        )
        .await;
        assert_eq!(execution.error, None);
        execution
            .messages
            .last()
            .expect("the result row")
            .content
            .text()
    }

    async fn session_file(
        engine: &crate::session_engine::engine::SessionEngine,
    ) -> std::path::PathBuf {
        engine
            .session
            .shared_persistence()
            .lock()
            .await
            .get_session_file()
            .expect("a persisted session")
            .to_path_buf()
    }

    fn write_settings(path: &std::path::Path, max_context_tokens: u64) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            serde_json::json!({ "compaction": { "maxContextTokens": max_context_tokens } })
                .to_string(),
        )
        .unwrap();
    }

    /// #2100: precedence is session > project > global, and the session
    /// override round-trips through the transcript on resume (`off` too).
    #[tokio::test]
    async fn the_session_override_wins_and_survives_resume() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        write_settings(&root.join("agent/settings.json"), 400_000);
        write_settings(&root.join(".prime/agent/settings.json"), 300_000);

        let first = engine(root, None).await;
        let cap = |engine: &crate::session_engine::engine::SessionEngine| {
            engine.session.compaction_settings().max_context_tokens
        };
        assert_eq!(cap(&first), Some(300_000), "project beats global");
        assert_eq!(
            context_limit(&first, "").await,
            "Model context window: 1048576 tokens\nReserve tokens: 16384\n\
             Configured cap: 300000 tokens (project)\nAuto-compacts at: 300000 tokens"
        );
        assert_eq!(
            context_limit(&first, "200000").await,
            "Session context limit set\nModel context window: 1048576 tokens\n\
             Reserve tokens: 16384\nConfigured cap: 200000 tokens (chat)\n\
             Auto-compacts at: 200000 tokens"
        );
        let file = session_file(&first).await;
        drop(first);

        let resumed = engine(root, Some(&file)).await;
        assert_eq!(cap(&resumed), Some(200_000), "the override restores");
        assert_eq!(resumed.session.session_context_limit(), Some(200_000));
        context_limit(&resumed, "off").await;
        assert_eq!(cap(&resumed), Some(300_000), "off falls back to settings");
        drop(resumed);

        let cleared = engine(root, Some(&file)).await;
        assert_eq!(
            (cap(&cleared), cleared.session.session_context_limit()),
            (Some(300_000), None)
        );
    }

    #[tokio::test]
    async fn context_limit_rejects_a_non_positive_or_non_numeric_cap() {
        let dir = tempfile::TempDir::new().unwrap();
        let engine = engine(dir.path(), None).await;
        let model: pa_types::ai::Model = serde_json::from_value(long_window_model_json()).unwrap();
        let mut autonomous = crate::autonomous::create_autonomous_runtime_state(None, None);
        for args in ["0", "lots", "-5"] {
            let command = crate::session_engine::slash_commands::parse_session_command(
                &crate::session_engine::slash_commands::SlashCommandRegistry::builtin(),
                &format!("/context-limit {args}"),
            )
            .unwrap();
            let execution = crate::session_engine::session_commands::execute_session_command(
                &engine,
                &mut crate::session_engine::session_commands::SessionCommandParams {
                    model: &model,
                    api_key: None,
                    global_harness_dir: dir.path().to_path_buf(),
                    autonomous: &mut autonomous,
                },
                &command,
            )
            .await;
            assert_eq!(
                execution.error.as_deref(),
                Some("Usage: /context-limit [tokens|off]")
            );
        }
        assert_eq!(engine.session.session_context_limit(), None);
    }

    /// #2100: a sub-floor cap is clamped with one model-invisible notice per
    /// session — a resumed branch that already shows it does not repeat it,
    /// and a disabled compaction never notices.
    #[tokio::test]
    async fn the_clamp_notice_shows_once_per_session() {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();
        let first = engine(root, None).await;
        assert_eq!(first.session.take_context_cap_clamp_notice(), None);
        let status = context_limit(&first, "1000").await;
        assert!(status.contains("Cap raised to 44576 tokens"), "{status}");
        let notice = first
            .session
            .take_context_cap_clamp_notice()
            .expect("the first clamped check notices");
        assert_eq!(
            (notice.custom_type.as_str(), notice.content.text()),
            (
                CONTEXT_CAP_CLAMP_NOTICE_CUSTOM_TYPE,
                "Configured context limit of 1000 tokens is below keepRecentTokens (20000) + \
                 reserveTokens (16384) + 8192, which would make compaction thrash. Using 44576 \
                 tokens instead."
                    .to_string()
            )
        );
        assert!(
            crate::session_engine::messages::convert_to_llm(&[
                pa_types::session::AgentMessage::Custom(notice.clone())
            ])
            .is_empty(),
            "the notice never reaches the model"
        );
        assert_eq!(first.session.take_context_cap_clamp_notice(), None);
        {
            let persistence = first.session.shared_persistence();
            let mut session = persistence.lock().await;
            session
                .append_custom_message(&notice.custom_type, notice.content, true, None)
                .unwrap();
            session.flush_now().unwrap();
        }
        let file = session_file(&first).await;
        drop(first);
        let resumed = engine(root, Some(&file)).await;
        assert_eq!(resumed.session.take_context_cap_clamp_notice(), None);

        let fresh_dir = tempfile::TempDir::new().unwrap();
        let disabled = engine(fresh_dir.path(), None).await;
        context_limit(&disabled, "1000").await;
        disabled.session.set_auto_compaction_enabled(false);
        assert_eq!(disabled.session.take_context_cap_clamp_notice(), None);
    }
}
