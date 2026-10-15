//! WebAssembly plugins: sandboxed components (`wit/plugin.wit`) loaded from
//! `<agent dir>/plugins/` and, in a trusted workspace, the project's
//! `.prime/agent/plugins/`. Each plugin reviews the session's tool calls and
//! kernel host requests, may add hidden prompt context, publishes status
//! (the agents-view line and the panel under the chat editor), and answers
//! the kernel's `wasm_plugins.call(...)`. See `README.md` and
//! `docs/hooks-and-plugins.md`.

mod engine;
mod instance;
pub mod manifest;
mod state;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use pa_agent::types::ToolResultContent;
use pa_core::features::{
    FeatureFuture,
    FeatureVerdict,
    HostRequestObservation,
    PromptDecision,
    SessionFeature,
    SessionFeatureContext,
    ToolCallObservation,
    ToolResultObservation,
};
use pa_core::kernel::shared::{HostRequestHandlers, host_handler};
use serde_json::{Value, json};

pub use crate::instance::SessionPlugin;
pub use crate::state::status_feature;

/// The generated bindings of the `plugin` world.
pub(crate) mod bindings {
    wasmtime::component::bindgen!({
        world: "plugin",
        path: "wit",
        additional_derives: [PartialEq, Eq],
    });
}

use bindings::prime_agent::plugin::types as wit;

/// The feature's name (`SessionFeature::name`).
pub const FEATURE_NAME: &str = "plugins";
/// The bundled kernel skill (`skills/.features/wasm-plugins`, Python import
/// `wasm_plugins`: a name no project package is likely to shadow).
pub const PLUGINS_SKILL: &str = "wasm-plugins";
/// The plugin directory under the agent dir and under a project's config dir.
pub const PLUGINS_DIR: &str = "plugins";
/// Host request: call a plugin's `call` export (`{plugin, method, data}`).
pub const CALL_REQUEST: &str = "plugin.call";
/// Host request: the session's loaded plugins.
pub const LIST_REQUEST: &str = "plugin.list";

/// The plugin files a session loads, in order: the user's (agent dir), then
/// the project's when its workspace is trusted. Each directory is read in
/// name order; only `*.wasm` files count.
#[must_use]
pub fn plugin_files(agent_dir: &Path, cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![agent_dir.join(PLUGINS_DIR)];
    let project = cwd
        .join(pa_core::settings::CONFIG_DIR_NAME)
        .join(PLUGINS_DIR);
    if project != dirs[0]
        && project.is_dir()
        && pa_core::workspace_trust::evaluate(cwd, agent_dir).is_trusted()
    {
        dirs.push(project);
    }
    dirs.iter()
        .flat_map(|dir| {
            let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .filter(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "wasm"))
                .collect();
            files.sort();
            files
        })
        .collect()
}

fn session_info(context: &SessionFeatureContext) -> wit::SessionInfo {
    wit::SessionInfo {
        session_id: context.session_id.clone(),
        cwd: context.cwd.to_string_lossy().into_owned(),
        rlm_depth: context.rlm_depth,
        model: format!("{}/{}", context.model.provider, context.model.id),
    }
}

/// Load every plugin for one session. Blocking (compiles and instantiates).
/// A plugin that fails to load is reported and skipped; a later plugin with
/// an earlier one's name is skipped.
fn load_session(context: &SessionFeatureContext) -> Vec<Arc<SessionPlugin>> {
    let mut loaded: Vec<Arc<SessionPlugin>> = Vec::new();
    for path in plugin_files(&context.agent_dir, &context.cwd) {
        let plugin = engine::prepare(&path)
            .and_then(|pre| SessionPlugin::load(&path, pre, session_info(context)));
        match plugin {
            Ok(plugin) if loaded.iter().any(|other| other.name() == plugin.name()) => {
                tracing::warn!(
                    target: "pa_plugins",
                    "skipping {}: a plugin named {} is already loaded",
                    path.display(),
                    plugin.name()
                );
            }
            Ok(plugin) => loaded.push(Arc::new(plugin)),
            Err(error) => {
                tracing::error!(target: "pa_plugins", "cannot load {}: {error}", path.display());
                pa_core::features::publish_feature_status(
                    &context.session_id,
                    pa_core::features::FeatureStatus {
                        feature: status_feature(
                            &path.file_stem().unwrap_or_default().to_string_lossy(),
                        ),
                        line: Some(format!("failed to load: {error}")),
                        status: json!({ "error": error }),
                    },
                );
            }
        }
    }
    loaded
}

type SessionPlugins = Arc<Vec<Arc<SessionPlugin>>>;
type SessionCell = Arc<tokio::sync::OnceCell<SessionPlugins>>;

/// The loaded plugins of every session in this process.
#[derive(Default)]
struct Registry {
    sessions: Mutex<HashMap<String, SessionCell>>,
}

impl Registry {
    fn cell(&self, session_id: &str) -> SessionCell {
        Arc::clone(
            self.sessions
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(session_id.to_string())
                .or_default(),
        )
    }

    /// The session's plugins, loading them on first use (off the runtime).
    async fn plugins(&self, context: &SessionFeatureContext) -> SessionPlugins {
        let cell = self.cell(&context.session_id);
        let context = context.clone();
        Arc::clone(
            cell.get_or_init(|| async move {
                let loaded = tokio::task::spawn_blocking(move || load_session(&context)).await;
                Arc::new(loaded.unwrap_or_default())
            })
            .await,
        )
    }
}

/// Run one export of every plugin, in load order, off the runtime.
async fn each<R: Send + 'static>(
    plugins: &SessionPlugins,
    call: impl Fn(&SessionPlugin) -> Result<R, String> + Send + Sync + Clone + 'static,
) -> Vec<(String, Result<R, String>)> {
    let mut results = Vec::with_capacity(plugins.len());
    for plugin in plugins.iter() {
        let plugin = Arc::clone(plugin);
        let call = call.clone();
        let name = plugin.name().to_string();
        let result = tokio::task::spawn_blocking(move || call(&plugin))
            .await
            .unwrap_or_else(|error| Err(format!("plugin {name} panicked: {error}")));
        results.push((plugin_name(plugins, results.len()), result));
    }
    results
}

fn plugin_name(plugins: &SessionPlugins, index: usize) -> String {
    plugins
        .get(index)
        .map(|plugin| plugin.name().to_string())
        .unwrap_or_default()
}

/// The first refusal among policy answers; a failed plugin refuses (a
/// policy fails closed).
fn first_block(results: Vec<(String, Result<wit::Verdict, String>)>) -> FeatureVerdict {
    for (name, result) in results {
        match result {
            Ok(wit::Verdict::Allow) => {}
            Ok(wit::Verdict::Block(reason)) => {
                return FeatureVerdict::Block(format!("plugin {name}: {reason}"));
            }
            Err(error) => return FeatureVerdict::Block(error),
        }
    }
    FeatureVerdict::Allow
}

fn result_text(content: &[ToolResultContent]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ToolResultContent::Text(text) => Some(text.text.as_str()),
            ToolResultContent::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The plugins feature, installed by the composition root.
#[derive(Default)]
pub struct PluginsFeature {
    registry: Arc<Registry>,
}

impl PluginsFeature {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl SessionFeature for PluginsFeature {
    fn name(&self) -> &'static str {
        FEATURE_NAME
    }

    fn register_host_handlers(
        &self,
        context: &SessionFeatureContext,
        handlers: &mut HostRequestHandlers,
    ) {
        let registry = Arc::clone(&self.registry);
        let session = Arc::new(context.clone());
        handlers.register(
            LIST_REQUEST,
            host_handler(move |_payload| {
                let registry = Arc::clone(&registry);
                let session = Arc::clone(&session);
                async move {
                    let plugins = registry.plugins(&session).await;
                    Ok(Value::Array(
                        plugins
                            .iter()
                            .map(|plugin| {
                                json!({
                                    "name": plugin.name(),
                                    "description": plugin.description(),
                                    "path": plugin.path().to_string_lossy(),
                                })
                            })
                            .collect(),
                    ))
                }
            }),
        );
        let registry = Arc::clone(&self.registry);
        let session = Arc::new(context.clone());
        handlers.register(
            CALL_REQUEST,
            host_handler(move |payload| {
                let registry = Arc::clone(&registry);
                let session = Arc::clone(&session);
                async move {
                    let field = |name: &str| {
                        payload
                            .data
                            .get(name)
                            .and_then(Value::as_str)
                            .filter(|value| !value.is_empty())
                            .map(str::to_string)
                            .ok_or_else(|| {
                                anyhow::anyhow!("{CALL_REQUEST} requires a string {name}")
                            })
                    };
                    let target = field("plugin")?;
                    let method = field("method")?;
                    let data = payload.data.get("data").cloned().unwrap_or(Value::Null);
                    let plugins = registry.plugins(&session).await;
                    let plugin = plugins
                        .iter()
                        .find(|plugin| plugin.name() == target)
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("no plugin named {target} is loaded"))?;
                    let data_json = data.to_string();
                    let answer = tokio::task::spawn_blocking(move || {
                        let session_id = plugin.session_id().to_string();
                        plugin.call(|instance, store| {
                            instance.call_call(store, &session_id, &method, &data_json)
                        })
                    })
                    .await
                    .map_err(|error| anyhow::anyhow!("plugin call panicked: {error}"))?
                    .map_err(|error| anyhow::anyhow!(error))?;
                    match answer {
                        Ok(text) => serde_json::from_str(&text).map_err(|error| {
                            anyhow::anyhow!("plugin {target} answered invalid JSON: {error}")
                        }),
                        Err(message) => Err(anyhow::anyhow!("plugin {target}: {message}")),
                    }
                }
            }),
        );
    }

    fn review_tool_call(
        &self,
        context: &Arc<SessionFeatureContext>,
        call: &ToolCallObservation,
    ) -> FeatureFuture<FeatureVerdict> {
        let registry = Arc::clone(&self.registry);
        let context = Arc::clone(context);
        let call = wit::ToolCall {
            tool_call_id: call.tool_call_id.clone(),
            tool_name: call.tool_name.clone(),
            args_json: call.args.to_string(),
        };
        Box::pin(async move {
            let plugins = registry.plugins(&context).await;
            let session_id = context.session_id.clone();
            first_block(
                each(&plugins, move |plugin| {
                    plugin.call(|instance, store| {
                        instance.call_review_tool_call(store, &session_id, &call)
                    })
                })
                .await,
            )
        })
    }

    fn after_tool_call(
        &self,
        context: &Arc<SessionFeatureContext>,
        result: &ToolResultObservation,
    ) -> FeatureFuture<Option<String>> {
        let registry = Arc::clone(&self.registry);
        let context = Arc::clone(context);
        let outcome = wit::ToolResult {
            tool_call_id: result.tool_call_id.clone(),
            tool_name: result.tool_name.clone(),
            args_json: result.args.to_string(),
            is_error: result.is_error,
            text: result_text(&result.content),
        };
        Box::pin(async move {
            let plugins = registry.plugins(&context).await;
            let session_id = context.session_id.clone();
            let notes: Vec<String> = each(&plugins, move |plugin| {
                plugin.call(|instance, store| {
                    instance.call_after_tool_call(store, &session_id, &outcome)
                })
            })
            .await
            .into_iter()
            .filter_map(|(name, result)| match result {
                Ok(note) => note,
                Err(error) => {
                    tracing::warn!(target: "pa_plugins", plugin = name.as_str(), "{error}");
                    None
                }
            })
            .collect();
            (!notes.is_empty()).then(|| notes.join("\n"))
        })
    }

    fn review_host_request(
        &self,
        context: &Arc<SessionFeatureContext>,
        request: &HostRequestObservation,
    ) -> FeatureFuture<FeatureVerdict> {
        let registry = Arc::clone(&self.registry);
        let context = Arc::clone(context);
        let request = wit::HostRequest {
            request_type: request.request_type.clone(),
            data_json: request.data.to_string(),
        };
        Box::pin(async move {
            let plugins = registry.plugins(&context).await;
            let session_id = context.session_id.clone();
            first_block(
                each(&plugins, move |plugin| {
                    plugin.call(|instance, store| {
                        instance.call_review_host_request(store, &session_id, &request)
                    })
                })
                .await,
            )
        })
    }

    fn prompt_context(
        &self,
        context: &Arc<SessionFeatureContext>,
        prompt: &str,
    ) -> FeatureFuture<PromptDecision> {
        let registry = Arc::clone(&self.registry);
        let context = Arc::clone(context);
        let prompt = prompt.to_string();
        Box::pin(async move {
            let plugins = registry.plugins(&context).await;
            let session_id = context.session_id.clone();
            let answers = each(&plugins, move |plugin| {
                plugin.call(|instance, store| {
                    instance.call_prompt_context(store, &session_id, &prompt)
                })
            })
            .await;
            let mut contexts = Vec::new();
            for (name, answer) in answers {
                match answer {
                    Ok(wit::PromptDecision::Context(Some(text))) if !text.trim().is_empty() => {
                        contexts.push(text);
                    }
                    Ok(wit::PromptDecision::Context(_)) => {}
                    Ok(wit::PromptDecision::Block(reason)) => {
                        return PromptDecision::Block(format!("plugin {name}: {reason}"));
                    }
                    // Context is advisory: a failed plugin adds none.
                    Err(error) => tracing::warn!(target: "pa_plugins", "{error}"),
                }
            }
            PromptDecision::Continue {
                context: (!contexts.is_empty()).then(|| contexts.join("\n\n")),
            }
        })
    }

    fn on_session_start(
        &self,
        context: &Arc<SessionFeatureContext>,
        _history: &[pa_agent::types::AgentMessage],
    ) {
        // Load (compile, instantiate, `on-session-start`) off the creation
        // path; the first hook awaits the same load.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let registry = Arc::clone(&self.registry);
            let context = Arc::clone(context);
            runtime.spawn(async move {
                registry.plugins(&context).await;
            });
        }
    }

    fn on_agent_end(&self, context: &Arc<SessionFeatureContext>) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let registry = Arc::clone(&self.registry);
        let context = Arc::clone(context);
        runtime.spawn(async move {
            let plugins = registry.plugins(&context).await;
            let session_id = context.session_id.clone();
            for (name, result) in each(&plugins, move |plugin| {
                plugin.call(|instance, store| instance.call_on_agent_end(store, &session_id))
            })
            .await
            {
                if let Err(error) = result {
                    tracing::warn!(target: "pa_plugins", plugin = name.as_str(), "{error}");
                }
            }
        });
    }

    fn bundled_skills(&self) -> Vec<&'static str> {
        vec![PLUGINS_SKILL]
    }

    /// The `wasm-plugins` skill only where a plugin file exists to call.
    fn session_skill_visible(&self, context: &SessionFeatureContext, skill_name: &str) -> bool {
        skill_name != PLUGINS_SKILL || !plugin_files(&context.agent_dir, &context.cwd).is_empty()
    }
}

#[cfg(test)]
mod tests;
