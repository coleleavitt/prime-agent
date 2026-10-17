//! # pa-lsp
//!
//! Claude Code's LSP integration for a kernel harness: language servers
//! configured in the `lspServers` setting or a trusted project's
//! `.lsp.json` ([`config`]), each started on first use for a file of its
//! language and shut down with its session ([`server`], [`manager`]), over
//! a JSON-RPC stdio client ([`rpc`]).
//!
//! - The kernel's `rlm.lsp.*` functions send the `lsp.*` host requests
//!   ([`requests`]): definition, implementation, references, hover,
//!   document and workspace symbols, call hierarchy, a file's diagnostics,
//!   and the server list. Answers carry Claude Code's LSP tool text
//!   ([`format`]) and the items as data.
//! - After a cell writes files (the `python_repl` result's `fileWrites`
//!   host fact, or the native `edit` tool), their servers get `didChange` /
//!   `didSave`; at the run's next boundary (bounded by
//!   `lspDiagnosticsWaitMs`) the new diagnostics they published reach the
//!   model as Claude Code's hidden `<new-diagnostics>` reminder
//!   ([`diagnostics`]); ones published later ride the next boundary or the
//!   next prompt.
//!
//! The crate plugs into sessions through
//! [`pa_core::features::SessionFeature`]; see `README.md`.

pub mod config;
pub mod diagnostics;
pub mod format;
pub mod manager;
pub mod protocol;
pub mod requests;
pub mod rpc;
pub mod server;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use pa_core::features::{
    FeatureFuture,
    FeatureTelemetry,
    PromptDecision,
    SessionFeature,
    SessionFeatureContext,
    ToolBatchObservation,
    ToolResultObservation,
};
use pa_core::kernel::shared::{HostRequestHandlers, HostRequestPayload, host_handler};
use serde_json::Value;

use crate::config::LspConfig;
use crate::manager::{SessionServers, Synced};

/// The adoption event, sent once per session that used a server.
pub const SESSION_EVENT: &str = "lsp_session";

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// One session's usage counters (the adoption event's properties).
#[derive(Debug, Default)]
struct Stats {
    requests: AtomicU64,
    request_errors: AtomicU64,
    deliveries: AtomicU64,
    diagnostics: AtomicU64,
}

/// Where a session's servers come from: its configuration (`cwd`,
/// `agent_dir`), read on the first use off the async runtime (tests
/// substitute a fixed one).
pub type ConfigLoader = Arc<dyn Fn(&Path, &Path) -> LspConfig + Send + Sync>;

/// One session's LSP state.
struct SessionLsp {
    cwd: PathBuf,
    agent_dir: PathBuf,
    loader: ConfigLoader,
    servers: tokio::sync::OnceCell<Arc<SessionServers>>,
    /// Syncs of written files still running (the boundary waits on them).
    syncs: Mutex<Vec<tokio::task::JoinHandle<Option<Synced>>>>,
    stats: Stats,
    telemetry: Option<FeatureTelemetry>,
    /// The runtime the session's servers run on (their shutdown is spawned
    /// there when the session ends outside any runtime).
    runtime: Option<tokio::runtime::Handle>,
}

impl SessionLsp {
    async fn servers(&self) -> Arc<SessionServers> {
        Arc::clone(
            self.servers
                .get_or_init(|| async {
                    let cwd = self.cwd.clone();
                    let agent_dir = self.agent_dir.clone();
                    let loader = Arc::clone(&self.loader);
                    let config = tokio::task::spawn_blocking(move || loader(&cwd, &agent_dir))
                        .await
                        .unwrap_or_default();
                    for error in &config.errors {
                        tracing::warn!(target: "pa_lsp", %error, "LSP server configuration");
                    }
                    Arc::new(SessionServers::new(config, &self.cwd))
                })
                .await,
        )
    }

    fn loaded(&self) -> Option<Arc<SessionServers>> {
        self.servers.get().cloned()
    }

    async fn serve(&self, request: &str, data: &Value) -> anyhow::Result<Value> {
        let servers = self.servers().await;
        self.stats.requests.fetch_add(1, Ordering::Relaxed);
        let outcome = match request {
            requests::DEFINITION | requests::IMPLEMENTATION | requests::REFERENCES => {
                requests::locations(&servers, request, data).await
            }
            requests::HOVER => requests::hover(&servers, data).await,
            requests::DOCUMENT_SYMBOLS => requests::document_symbols(&servers, data).await,
            requests::WORKSPACE_SYMBOLS => requests::workspace_symbols(&servers, data).await,
            requests::CALL_HIERARCHY => requests::call_hierarchy(&servers, data).await,
            requests::DIAGNOSTICS => requests::diagnostics(&servers, data).await,
            requests::SERVERS => Ok(requests::server_list(&servers).await),
            other => Err(anyhow::anyhow!("unknown LSP request {other}")),
        };
        if outcome.is_err() {
            self.stats.request_errors.fetch_add(1, Ordering::Relaxed);
        }
        outcome
    }

    /// The model wrote `paths`: sync each one a server handles, in the
    /// background (a cold server start can take a while).
    async fn files_written(&self, paths: Vec<PathBuf>) {
        let servers = self.servers().await;
        if servers.is_empty() {
            return;
        }
        let mut syncs = lock(&self.syncs);
        for path in paths {
            if servers.server_for(&path).is_none() {
                continue;
            }
            let servers = Arc::clone(&servers);
            syncs.push(tokio::spawn(async move {
                match servers.file_written(&path).await {
                    Ok(synced) => synced,
                    Err(error) => {
                        tracing::warn!(target: "pa_lsp", %error, path = %path.display(), "could not sync a written file");
                        None
                    }
                }
            }));
        }
    }

    /// The reminder for whatever is pending, after waiting (bounded) for
    /// the written files' publishes.
    async fn boundary_reminder(&self) -> Option<String> {
        let servers = self.loaded()?;
        let syncs: Vec<_> = std::mem::take(&mut *lock(&self.syncs));
        if !syncs.is_empty() {
            let deadline = tokio::time::Instant::now() + servers.diagnostics_wait();
            let mut synced = Vec::new();
            let mut unfinished = Vec::new();
            for mut sync in syncs {
                match tokio::time::timeout_at(deadline, &mut sync).await {
                    Ok(Ok(Some(done))) => synced.push(done),
                    Ok(Ok(None) | Err(_)) => {}
                    // Still starting: its diagnostics ride a later delivery.
                    Err(_) => unfinished.push(sync),
                }
            }
            drop(unfinished);
            servers.wait_for_publishes(&synced, deadline).await;
        }
        self.take_reminder(&servers)
    }

    fn take_reminder(&self, servers: &SessionServers) -> Option<String> {
        let delivery = servers.take_delivery()?;
        self.stats.deliveries.fetch_add(1, Ordering::Relaxed);
        self.stats
            .diagnostics
            .fetch_add(delivery.count() as u64, Ordering::Relaxed);
        Some(diagnostics::reminder(&delivery))
    }

    async fn report(&self, servers: &SessionServers) {
        let Some(telemetry) = &self.telemetry else {
            return;
        };
        let reports = servers.reports().await;
        let started = reports.iter().filter(|report| report.starts > 0).count();
        let requests = self.stats.requests.load(Ordering::Relaxed);
        let deliveries = self.stats.deliveries.load(Ordering::Relaxed);
        if started == 0 && requests == 0 && deliveries == 0 {
            return;
        }
        let mut properties = pa_telemetry::Properties::new();
        properties.set("servers_configured", reports.len().into());
        properties.set("servers_started", started.into());
        properties.set("requests", requests.into());
        properties.set(
            "request_errors",
            self.stats.request_errors.load(Ordering::Relaxed).into(),
        );
        properties.set("diagnostic_deliveries", deliveries.into());
        properties.set(
            "diagnostics_delivered",
            self.stats.diagnostics.load(Ordering::Relaxed).into(),
        );
        properties.set("server_failures", servers.failures().await.into());
        telemetry.track(SESSION_EVENT, &properties);
    }
}

/// The paths a finished tool call wrote: a `python_repl` cell's
/// `fileWrites` host fact, or the native `edit` tool's `path`.
fn written_paths(result: &ToolResultObservation, cwd: &Path) -> Vec<PathBuf> {
    if result.is_error && result.tool_name == "edit" {
        return Vec::new();
    }
    if pa_types::ai::is_python_tool_name(&result.tool_name) {
        return result
            .host_facts
            .get("fileWrites")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(PathBuf::from)
            .collect();
    }
    if result.tool_name == "edit" {
        if let Some(path) = result.args.get("path").and_then(Value::as_str) {
            let path = Path::new(path);
            return vec![if path.is_absolute() {
                path.to_path_buf()
            } else {
                cwd.join(path)
            }];
        }
    }
    Vec::new()
}

/// The LSP session feature: one [`SessionServers`] per session.
pub struct LspFeature {
    sessions: Mutex<HashMap<String, Arc<SessionLsp>>>,
    loader: ConfigLoader,
}

impl std::fmt::Debug for LspFeature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspFeature").finish_non_exhaustive()
    }
}

impl Default for LspFeature {
    fn default() -> Self {
        Self::new()
    }
}

impl LspFeature {
    /// The product's feature: configuration from the session's settings
    /// and a trusted project's `.lsp.json`.
    #[must_use]
    pub fn new() -> Self {
        Self::with_loader(Arc::new(LspConfig::load))
    }

    /// A feature whose sessions read their configuration from `loader`
    /// (`cwd`, `agent_dir`) instead (tests, embedders).
    #[must_use]
    pub fn with_loader(loader: ConfigLoader) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            loader,
        }
    }

    fn session(&self, context: &SessionFeatureContext) -> Option<Arc<SessionLsp>> {
        lock(&self.sessions).get(&context.session_id).cloned()
    }

    fn session_or_create(&self, context: &SessionFeatureContext) -> Arc<SessionLsp> {
        Arc::clone(
            lock(&self.sessions)
                .entry(context.session_id.clone())
                .or_insert_with(|| {
                    Arc::new(SessionLsp {
                        cwd: context.cwd.clone(),
                        agent_dir: context.agent_dir.clone(),
                        loader: Arc::clone(&self.loader),
                        servers: tokio::sync::OnceCell::new(),
                        syncs: Mutex::new(Vec::new()),
                        stats: Stats::default(),
                        telemetry: context.telemetry.clone(),
                        runtime: tokio::runtime::Handle::try_current().ok(),
                    })
                }),
        )
    }
}

impl SessionFeature for LspFeature {
    fn name(&self) -> &'static str {
        "lsp"
    }

    fn register_host_handlers(
        &self,
        context: &SessionFeatureContext,
        handlers: &mut HostRequestHandlers,
    ) {
        let session = self.session_or_create(context);
        for &request in requests::REQUEST_TYPES {
            let session = Arc::clone(&session);
            handlers.register(
                request,
                host_handler(move |payload: HostRequestPayload| {
                    let session = Arc::clone(&session);
                    async move { session.serve(request, &payload.data).await }
                }),
            );
        }
    }

    fn after_tool_call(
        &self,
        context: &Arc<SessionFeatureContext>,
        result: &ToolResultObservation,
    ) -> FeatureFuture<Option<String>> {
        let paths = written_paths(result, &context.cwd);
        if paths.is_empty() {
            return Box::pin(async { None });
        }
        let session = self.session_or_create(context);
        Box::pin(async move {
            session.files_written(paths).await;
            None
        })
    }

    fn after_tool_batch(
        &self,
        context: &Arc<SessionFeatureContext>,
        _batch: &ToolBatchObservation,
    ) -> FeatureFuture<Option<String>> {
        let Some(session) = self
            .session(context)
            .filter(|session| session.loaded().is_some())
        else {
            return Box::pin(async { None });
        };
        Box::pin(async move { session.boundary_reminder().await })
    }

    fn prompt_context(
        &self,
        context: &Arc<SessionFeatureContext>,
        _prompt: &str,
    ) -> FeatureFuture<PromptDecision> {
        let reminder = self
            .session(context)
            .and_then(|session| {
                let servers = session.loaded()?;
                servers
                    .has_pending()
                    .then(|| session.take_reminder(&servers))
            })
            .flatten();
        Box::pin(async move { PromptDecision::Continue { context: reminder } })
    }

    fn on_session_end(&self, context: &Arc<SessionFeatureContext>, _reason: &str) {
        let Some(session) = lock(&self.sessions).remove(&context.session_id) else {
            return;
        };
        let Some(servers) = session.loaded() else {
            return;
        };
        for sync in std::mem::take(&mut *lock(&session.syncs)) {
            sync.abort();
        }
        let runtime = tokio::runtime::Handle::try_current()
            .ok()
            .or_else(|| session.runtime.clone());
        let Some(runtime) = runtime else {
            // No runtime to shut down on: dropping the clients kills the
            // processes (`kill_on_drop`).
            return;
        };
        runtime.spawn(async move {
            session.report(&servers).await;
            servers.shutdown().await;
        });
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn observation(tool: &str, args: Value, host_facts: Value) -> ToolResultObservation {
        ToolResultObservation {
            tool_call_id: "t".to_string(),
            tool_name: tool.to_string(),
            args,
            is_error: false,
            content: Vec::new(),
            details: Value::Null,
            host_facts,
            earlier_results_of_tool: 0,
        }
    }

    #[test]
    fn written_paths_come_from_the_cell_facts_and_the_edit_tool() {
        let cwd = Path::new("/repo");
        assert_eq!(
            written_paths(
                &observation(
                    "python_repl",
                    json!({"code": "..."}),
                    json!({"fileWrites": ["/repo/a.rs", 3]})
                ),
                cwd
            ),
            vec![PathBuf::from("/repo/a.rs")]
        );
        assert_eq!(
            written_paths(
                &observation("edit", json!({"path": "src/b.rs"}), Value::Null),
                cwd
            ),
            vec![PathBuf::from("/repo/src/b.rs")]
        );
        assert_eq!(
            written_paths(
                &observation("bash", json!({"command": "ls"}), Value::Null),
                cwd
            ),
            Vec::<PathBuf>::new()
        );
    }
}
