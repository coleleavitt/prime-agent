//! One session's language servers: which server owns a file (by its
//! extension; the first configured server claims it), which documents are
//! open on which server and at which version, and the diagnostics they
//! publish.
//!
//! Documents follow Claude Code's manager: a file is opened
//! (`textDocument/didOpen`) the first time a request or an edit touches it,
//! changed (`didChange`, full text) and saved (`didSave`) after the model
//! writes it, and closed (`didClose`) when more than 50 are open (the least
//! recently used first). Unlike Claude Code, a request also re-sends a
//! document whose content changed on disk since it was sent (a `bash()`
//! command can rewrite files the kernel never reports).

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::config::{LspConfig, normalize_extension};
use crate::diagnostics::{Delivery, DiagnosticsRegistry};
use crate::protocol::{Diagnostic, PublishDiagnostics, path_to_uri};
use crate::server::{LspServer, ServerEvents, ServerStatus};

/// Claude Code's open-document cap.
pub const MAX_OPEN_DOCUMENTS: usize = 50;
/// Claude Code's largest analyzable file.
pub const MAX_FILE_BYTES: u64 = 10_000_000;

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// One open document.
#[derive(Debug, Clone)]
struct Document {
    server: String,
    /// The text last sent.
    text: Arc<str>,
}

/// The open documents, least recently used first.
#[derive(Debug, Default)]
struct Documents {
    open: HashMap<String, Document>,
    order: VecDeque<String>,
    /// Every document's last version, kept across closes (versions only grow).
    versions: HashMap<String, i64>,
}

impl Documents {
    fn touch(&mut self, uri: &str) {
        self.order.retain(|known| known != uri);
        self.order.push_back(uri.to_string());
    }

    fn next_version(&mut self, uri: &str) -> i64 {
        let version = self.versions.entry(uri.to_string()).or_default();
        *version += 1;
        *version
    }

    /// Forget every document `server` had open (it restarted).
    fn drop_server(&mut self, server: &str) {
        self.open.retain(|_, document| document.server != server);
        let open = &self.open;
        self.order.retain(|uri| open.contains_key(uri));
    }

    /// The documents past the cap, least recently used first.
    fn evict(&mut self) -> Vec<(String, String)> {
        let mut evicted = Vec::new();
        while self.open.len() > MAX_OPEN_DOCUMENTS {
            let Some(uri) = self.order.pop_front() else {
                break;
            };
            if let Some(document) = self.open.remove(&uri) {
                evicted.push((uri, document.server));
            }
        }
        evicted
    }
}

/// The session's view of server events: documents and diagnostics.
struct Events {
    documents: Arc<Mutex<Documents>>,
    diagnostics: Arc<Mutex<DiagnosticsRegistry>>,
    published: Arc<Notify>,
    /// Servers whose diagnostics stay out of the reminders.
    quiet: Vec<String>,
}

impl ServerEvents for Events {
    fn notification(&self, server: &str, method: &str, params: Value) {
        match method {
            "textDocument/publishDiagnostics" => {
                let Ok(publish) = serde_json::from_value::<PublishDiagnostics>(params) else {
                    tracing::debug!(target: "pa_lsp::server", %server, "malformed publishDiagnostics");
                    return;
                };
                if let Some(version) = publish.version {
                    let current = lock(&self.documents).versions.get(&publish.uri).copied();
                    if current.is_some_and(|current| version < current) {
                        return;
                    }
                }
                let inject = !self.quiet.iter().any(|quiet| quiet == server);
                lock(&self.diagnostics).publish(server, &publish.uri, publish.parsed(), inject);
                self.published.notify_waiters();
            }
            "window/logMessage" | "window/showMessage" => {
                let message = params
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                tracing::debug!(target: "pa_lsp::server", %server, %message, "server message");
            }
            _ => {}
        }
    }

    fn started(&self, server: &str) {
        lock(&self.documents).drop_server(server);
    }
}

/// Why a file has no server answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unserved {
    /// No configured server handles the extension.
    NoServer(String),
    /// The file is over the 10 MB limit.
    TooLarge(u64),
}

impl Unserved {
    /// Claude Code's texts for both.
    #[must_use]
    pub fn text(&self) -> String {
        match self {
            Self::NoServer(extension) => {
                format!("No LSP server available for file type: {extension}")
            }
            Self::TooLarge(bytes) => format!(
                "File too large for LSP analysis ({}MB exceeds 10MB limit)",
                bytes.div_ceil(1_000_000)
            ),
        }
    }
}

/// A synced file: its URI, and the publish count before the sync (the
/// boundary waits for it to move).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Synced {
    pub uri: String,
    pub server: String,
    pub publishes_before: u64,
    /// Whether anything was sent (a new or changed document).
    pub sent: bool,
}

/// One server's status line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerReport {
    pub name: String,
    pub command: String,
    pub source: &'static str,
    pub extensions: Vec<String>,
    pub status: ServerStatus,
    pub starts: u32,
    pub last_error: Option<String>,
}

/// One session's servers.
pub struct SessionServers {
    cwd: PathBuf,
    servers: Vec<(Arc<LspServer>, &'static str)>,
    by_extension: HashMap<String, usize>,
    documents: Arc<Mutex<Documents>>,
    diagnostics: Arc<Mutex<DiagnosticsRegistry>>,
    published: Arc<Notify>,
    config_errors: Vec<String>,
    diagnostics_wait: Duration,
}

impl std::fmt::Debug for SessionServers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionServers")
            .field("servers", &self.servers.len())
            .finish_non_exhaustive()
    }
}

impl SessionServers {
    /// The servers `config` names, none started.
    #[must_use]
    pub fn new(config: LspConfig, cwd: &Path) -> Self {
        let documents = Arc::new(Mutex::new(Documents::default()));
        let diagnostics = Arc::new(Mutex::new(DiagnosticsRegistry::default()));
        let published = Arc::new(Notify::new());
        let events: Arc<dyn ServerEvents> = Arc::new(Events {
            documents: Arc::clone(&documents),
            diagnostics: Arc::clone(&diagnostics),
            published: Arc::clone(&published),
            quiet: config
                .servers
                .iter()
                .filter(|server| !server.config.injects_diagnostics())
                .map(|server| server.name.clone())
                .collect(),
        });
        let mut servers = Vec::new();
        let mut by_extension = HashMap::new();
        let mut config_errors = config.errors;
        for configured in config.servers {
            let index = servers.len();
            for (extension, _) in configured.config.extensions() {
                if let Some(owner) = by_extension.get(&extension) {
                    let (owner, _): &(Arc<LspServer>, &str) = &servers[*owner];
                    config_errors.push(format!(
                        "extension {extension} is already handled by '{}'; '{}' is not used for it",
                        owner.name(),
                        configured.name
                    ));
                    continue;
                }
                by_extension.insert(extension, index);
            }
            servers.push((
                Arc::new(LspServer::new(
                    configured.name,
                    configured.config,
                    cwd,
                    Arc::clone(&events),
                )),
                configured.source.as_str(),
            ));
        }
        Self {
            cwd: cwd.to_path_buf(),
            servers,
            by_extension,
            documents,
            diagnostics,
            published,
            config_errors,
            diagnostics_wait: config.diagnostics_wait,
        }
    }

    #[must_use]
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    #[must_use]
    pub fn server_count(&self) -> usize {
        self.servers.len()
    }

    #[must_use]
    pub fn diagnostics_wait(&self) -> Duration {
        self.diagnostics_wait
    }

    /// The server that handles `path`'s extension.
    #[must_use]
    pub fn server_for(&self, path: &Path) -> Option<&Arc<LspServer>> {
        let extension = path.extension()?.to_string_lossy();
        let index = self.by_extension.get(&normalize_extension(&extension))?;
        Some(&self.servers[*index].0)
    }

    fn server_named(&self, name: &str) -> Option<&Arc<LspServer>> {
        self.servers
            .iter()
            .map(|(server, _)| server)
            .find(|server| server.name() == name)
    }

    fn language_id(server: &LspServer, path: &Path) -> String {
        let extension = path
            .extension()
            .map(|extension| normalize_extension(&extension.to_string_lossy()))
            .unwrap_or_default();
        server
            .config()
            .extensions()
            .into_iter()
            .find(|(known, _)| *known == extension)
            .map_or_else(|| "plaintext".to_string(), |(_, language)| language)
    }

    /// Open or re-send `path` on its server (starting it when needed) from
    /// its content on disk, and `didSave` it when `save`.
    ///
    /// # Errors
    ///
    /// The file could not be read, or the server could not run.
    pub async fn sync(&self, path: &Path, save: bool) -> Result<Result<Synced, Unserved>, String> {
        let Some(server) = self.server_for(path) else {
            return Ok(Err(Unserved::NoServer(extension_label(path))));
        };
        let Some(uri) = path_to_uri(path) else {
            return Err(format!("not an absolute path: {}", path.display()));
        };
        let size = tokio::fs::metadata(path)
            .await
            .map_err(|error| format!("Cannot access file: {}. {error}", path.display()))?
            .len();
        if size > MAX_FILE_BYTES {
            return Ok(Err(Unserved::TooLarge(size)));
        }
        let text: Arc<str> = tokio::fs::read_to_string(path)
            .await
            .map_err(|error| format!("Cannot read file: {}. {error}", path.display()))?
            .into();
        // Start (or restart) first: a fresh start forgets the open documents.
        server.ensure_running().await?;
        let publishes_before = lock(&self.diagnostics).publish_count(&uri);
        let (message, evicted) = {
            let mut documents = lock(&self.documents);
            let current = documents
                .open
                .get(&uri)
                .filter(|document| document.server == server.name())
                .cloned();
            let message = match current {
                Some(document) if *document.text == *text => None,
                Some(_) => {
                    let version = documents.next_version(&uri);
                    Some((
                        "textDocument/didChange",
                        json!({
                            "textDocument": {"uri": uri, "version": version},
                            "contentChanges": [{"text": &*text}],
                        }),
                    ))
                }
                None => {
                    let version = documents.next_version(&uri);
                    Some((
                        "textDocument/didOpen",
                        json!({
                            "textDocument": {
                                "uri": uri,
                                "languageId": Self::language_id(server, path),
                                "version": version,
                                "text": &*text,
                            },
                        }),
                    ))
                }
            };
            if message.is_some() {
                documents.open.insert(
                    uri.clone(),
                    Document {
                        server: server.name().to_string(),
                        text: Arc::clone(&text),
                    },
                );
            }
            documents.touch(&uri);
            (message, documents.evict())
        };
        let sent = message.is_some();
        if let Some((method, params)) = message {
            server.notify(method, params).await?;
        }
        if save {
            server
                .notify(
                    "textDocument/didSave",
                    json!({"textDocument": {"uri": uri}}),
                )
                .await?;
        }
        for (evicted_uri, owner) in evicted {
            if let Some(owner) = self.server_named(&owner) {
                if owner.is_running().await {
                    let _ = owner
                        .notify(
                            "textDocument/didClose",
                            json!({"textDocument": {"uri": evicted_uri}}),
                        )
                        .await;
                }
            }
        }
        Ok(Ok(Synced {
            uri,
            server: server.name().to_string(),
            publishes_before,
            sent,
        }))
    }

    /// The model wrote `path`: forget what was delivered for it, then
    /// change and save it on its server. `None` when no server handles it.
    ///
    /// # Errors
    ///
    /// As [`Self::sync`].
    pub async fn file_written(&self, path: &Path) -> Result<Option<Synced>, String> {
        if self.server_for(path).is_none() {
            return Ok(None);
        }
        if let Some(uri) = path_to_uri(path) {
            lock(&self.diagnostics).forget_file(&uri);
        }
        Ok(self.sync(path, true).await?.ok())
    }

    /// Wait until each synced file got a publish after its sync, or the
    /// deadline passes.
    pub async fn wait_for_publishes(&self, synced: &[Synced], deadline: tokio::time::Instant) {
        loop {
            let notified = self.published.notified();
            let done = {
                let diagnostics = lock(&self.diagnostics);
                synced.iter().all(|file| {
                    !file.sent || diagnostics.publish_count(&file.uri) > file.publishes_before
                })
            };
            if done {
                return;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return;
            }
        }
    }

    /// Take what is pending as one delivery.
    #[must_use]
    pub fn take_delivery(&self) -> Option<Delivery> {
        lock(&self.diagnostics).take_pending()
    }

    #[must_use]
    pub fn has_pending(&self) -> bool {
        lock(&self.diagnostics).has_pending()
    }

    /// The file's latest published diagnostics, marked delivered.
    #[must_use]
    pub fn read_diagnostics(&self, uri: &str) -> Option<Vec<Diagnostic>> {
        let mut diagnostics = lock(&self.diagnostics);
        let latest = diagnostics.latest(uri)?.to_vec();
        diagnostics.mark_delivered(uri, &latest);
        Some(latest)
    }

    /// The servers that are running now.
    pub async fn running_servers(&self) -> Vec<Arc<LspServer>> {
        let mut running = Vec::new();
        for (server, _) in &self.servers {
            if server.is_running().await {
                running.push(Arc::clone(server));
            }
        }
        running
    }

    /// Every server's status.
    pub async fn reports(&self) -> Vec<ServerReport> {
        let mut reports = Vec::new();
        for (server, source) in &self.servers {
            let (status, starts, last_error) = server.status().await;
            let mut extensions: Vec<String> = self
                .by_extension
                .iter()
                .filter(|(_, index)| Arc::ptr_eq(&self.servers[**index].0, server))
                .map(|(extension, _)| extension.clone())
                .collect();
            extensions.sort();
            reports.push(ServerReport {
                name: server.name().to_string(),
                command: server.config().command.clone(),
                source,
                extensions,
                status,
                starts,
                last_error,
            });
        }
        reports
    }

    #[must_use]
    pub fn config_errors(&self) -> &[String] {
        &self.config_errors
    }

    /// Crashes and failed starts across the servers.
    pub async fn failures(&self) -> u32 {
        let mut total = 0;
        for (server, _) in &self.servers {
            total += server.failures().await;
        }
        total
    }

    /// Shut every server down (each bounded by its shutdown timeout).
    pub async fn shutdown(&self) {
        let servers: Vec<_> = self
            .servers
            .iter()
            .map(|(server, _)| Arc::clone(server))
            .collect();
        let mut tasks = tokio::task::JoinSet::new();
        for server in servers {
            tasks.spawn(async move { server.shutdown().await });
        }
        while tasks.join_next().await.is_some() {}
    }
}

/// The extension Claude Code names in its no-server text (with its dot;
/// empty for none).
#[must_use]
pub fn extension_label(path: &Path) -> String {
    path.extension()
        .map(|extension| format!(".{}", extension.to_string_lossy()))
        .unwrap_or_default()
}
