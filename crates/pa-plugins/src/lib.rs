//! WebAssembly plugins: sandboxed components (`wit/plugin.wit`) loaded from
//! `<agent dir>/plugins/` and, in a trusted workspace, the project's
//! `.prime/agent/plugins/`. Each plugin reviews the session's tool calls and
//! kernel host requests (refusing them or putting them to the user),
//! observes what the kernel ran, may add hidden prompt context, publishes
//! status (the agents-view line and the panel under the chat editor),
//! offers the model tools (`wasm_plugins.call(...)` in the kernel) and the
//! user slash commands (`/wasm <plugin> <command>`). See `README.md` and
//! `docs/hooks-and-plugins.md`.

mod declared;
mod engine;
mod instance;
pub mod manifest;
mod state;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use pa_agent::types::ToolResultContent;
use pa_core::features::{
    FeatureCommandOutcome,
    FeatureFuture,
    FeatureVerdict,
    HostRequestObservation,
    HostRequestOutcome,
    PromptDecision,
    SessionFeature,
    SessionFeatureContext,
    ToolCallObservation,
    ToolResultObservation,
};
use pa_core::kernel::shared::{HostRequestHandlers, host_handler};
use serde_json::{Value, json};

pub use crate::declared::{PluginCommand, PluginTool, signature};
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
/// Host request: the session's loaded plugins, with their tools and
/// commands.
pub const LIST_REQUEST: &str = "plugin.list";
/// The slash command that lists and runs the plugins' commands.
pub const COMMAND: &str = "wasm";

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
    /// Sessions whose model was told about the plugins' tools.
    announced: Mutex<HashSet<String>>,
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

/// The loaded plugin named `name`.
fn find_plugin(plugins: &SessionPlugins, name: &str) -> Result<Arc<SessionPlugin>, String> {
    plugins
        .iter()
        .find(|plugin| plugin.name() == name)
        .cloned()
        .ok_or_else(|| format!("no plugin named {name} is loaded; /wasm lists them"))
}

fn plugin_name(plugins: &SessionPlugins, index: usize) -> String {
    plugins
        .get(index)
        .map(|plugin| plugin.name().to_string())
        .unwrap_or_default()
}

/// The plugins' policy answers as one verdict: the first refusal (a failed
/// plugin refuses: a policy fails closed), else the questions they ask,
/// joined, else allowed.
fn verdict_of(results: Vec<(String, Result<wit::Verdict, String>)>) -> FeatureVerdict {
    let mut asks = Vec::new();
    for (name, result) in results {
        match result {
            Ok(wit::Verdict::Allow) => {}
            Ok(wit::Verdict::Ask(reason)) => asks.push(format!("plugin {name}: {reason}")),
            Ok(wit::Verdict::Block(reason)) => {
                return FeatureVerdict::Block(format!("plugin {name}: {reason}"));
            }
            Err(error) => return FeatureVerdict::Block(error),
        }
    }
    if asks.is_empty() {
        FeatureVerdict::Allow
    } else {
        FeatureVerdict::Ask(asks.join("; "))
    }
}

/// One plugin as `plugin.list` reports it.
fn listed(plugin: &SessionPlugin) -> Value {
    json!({
        "name": plugin.name(),
        "description": plugin.description(),
        "path": plugin.path().to_string_lossy(),
        "tools": plugin.tools().iter().map(|tool| json!({
            "name": tool.name,
            "description": tool.description,
            "signature": signature(tool),
            "parameters": tool.parameters,
        })).collect::<Vec<_>>(),
        "commands": plugin.commands().iter().map(|command| json!({
            "name": command.name,
            "description": command.description,
            "argumentHint": command.argument_hint,
        })).collect::<Vec<_>>(),
    })
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
                        plugins.iter().map(|plugin| listed(plugin)).collect(),
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
                    // A declared tool's arguments are checked against its
                    // schema before the plugin sees them.
                    let data = match plugin.tools().iter().find(|tool| tool.name == method) {
                        Some(tool) => pa_agent::validation::validate_tool_arguments(
                            &format!("{target}.{method}"),
                            &tool.parameters,
                            &data,
                        )
                        .map_err(|error| anyhow::anyhow!(error))?,
                        None => data,
                    };
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
            verdict_of(
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
            verdict_of(
                each(&plugins, move |plugin| {
                    plugin.call(|instance, store| {
                        instance.call_review_host_request(store, &session_id, &request)
                    })
                })
                .await,
            )
        })
    }

    fn after_host_request(
        &self,
        context: &Arc<SessionFeatureContext>,
        request: &HostRequestObservation,
        outcome: HostRequestOutcome<'_>,
    ) -> FeatureFuture<Option<String>> {
        let registry = Arc::clone(&self.registry);
        let context = Arc::clone(context);
        let request = wit::HostRequest {
            request_type: request.request_type.clone(),
            data_json: request.data.to_string(),
        };
        let outcome = outcome.map(Value::to_string).map_err(str::to_string);
        Box::pin(async move {
            let plugins = registry.plugins(&context).await;
            if plugins.is_empty() {
                return None;
            }
            let session_id = context.session_id.clone();
            let notes: Vec<String> = each(&plugins, move |plugin| {
                plugin.call(|instance, store| {
                    let outcome = outcome.as_deref().map_err(String::as_str);
                    instance.call_after_host_request(store, &session_id, &request, outcome)
                })
            })
            .await
            .into_iter()
            .filter_map(|(name, result)| match result {
                Ok(note) => note.map(|note| format!("plugin {name}: {note}")),
                Err(error) => {
                    tracing::warn!(target: "pa_plugins", plugin = name.as_str(), "{error}");
                    None
                }
            })
            .collect();
            (!notes.is_empty()).then(|| notes.join("\n"))
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
            // The session's first admitted prompt tells the model about the
            // plugins' tools.
            let first = registry
                .announced
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(context.session_id.clone());
            if let Some(tools) = first.then(|| declared::tools_context(&plugins)).flatten() {
                contexts.push(tools);
            }
            for (name, answer) in answers {
                match answer {
                    Ok(wit::PromptDecision::Context(Some(text))) if !text.trim().is_empty() => {
                        contexts.push(text);
                    }
                    Ok(wit::PromptDecision::Context(_)) => {}
                    Ok(wit::PromptDecision::Block(reason)) => {
                        // A refused prompt announced nothing: the next
                        // admitted one carries the tools.
                        registry
                            .announced
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .remove(&context.session_id);
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

    /// `on-agent-end` means the agent finished: a run a queued steer ended
    /// (and the agent goes on with it) does not count.
    fn on_agent_stop(&self, context: &Arc<SessionFeatureContext>) {
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

    fn slash_commands(&self) -> Vec<pa_types::slash_commands::BuiltinSlashCommand> {
        vec![pa_types::slash_commands::BuiltinSlashCommand {
            name: COMMAND,
            description: "List the WebAssembly plugins' commands, or run one",
            execution: pa_types::slash_commands::SlashCommandExecution::Session,
            argument_hint: Some("[<plugin> <command> [args]]"),
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
        if name != COMMAND {
            return None;
        }
        let registry = Arc::clone(&self.registry);
        let context = Arc::clone(context);
        let args = args.to_string();
        Some(Box::pin(async move {
            let plugins = registry.plugins(&context).await;
            let text = match declared::parse_wasm_command(&args) {
                declared::WasmCommand::List => declared::listing(plugins.iter()),
                declared::WasmCommand::Plugin(target) => {
                    let plugin = find_plugin(&plugins, target)?;
                    declared::listing(std::iter::once(&plugin))
                }
                declared::WasmCommand::Run {
                    plugin: target,
                    command,
                    args,
                } => {
                    let plugin = find_plugin(&plugins, target)?;
                    if !plugin.commands().iter().any(|known| known.name == command) {
                        return Err(format!(
                            "plugin {target} has no command {command}; /wasm {target} lists them"
                        ));
                    }
                    let (command, args) = (command.to_string(), args.to_string());
                    let session_id = context.session_id.clone();
                    tokio::task::spawn_blocking(move || {
                        plugin.call(|instance, store| {
                            instance.call_run_command(store, &session_id, &command, &args)
                        })
                    })
                    .await
                    .map_err(|error| format!("plugin {target} panicked: {error}"))??
                    .map_err(|error| format!("plugin {target}: {error}"))?
                }
            };
            Ok(FeatureCommandOutcome {
                text,
                completion: None,
            })
        }))
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
