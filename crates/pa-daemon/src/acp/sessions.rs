//! Session admission over the daemon-attached transport: `session/new`
//! (the connection's current daemon session), `session/list` (the saved
//! catalog as ACP `SessionInfo`), and `session/load` (a saved session bound
//! and its transcript replayed before the answer; upstream #1116, #1600,
//! #2804). One session per connection: both admissions share the slot.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pa_types::daemon::DaemonCommand;
use serde_json::{json, Map, Value};
use tokio::sync::Mutex;

use super::daemon::{
    clear_connection_servers, fetch_rlm_children, release_session_input_pause,
    replace_connection_servers, DaemonAcpOptions, DaemonAcpState, DaemonBinding, DaemonLink,
    HostedSession,
};
use super::jsonrpc;
use super::meta::{self, PrimeAgentEventPhase, PrimeAgentSessionMeta};
use super::producer::{self, UpdateProducer};
use super::types;
use super::wire_config::{
    fetch_available_models, fetch_connection_state, picker_options_from_state,
    state_context_window, HostedConfig,
};
use super::wire_events::{self, WireMappingState};

/// What one admission binds: `session/new` hosts the connection's current
/// daemon session; `session/load` hosts a saved one whose transcript replays
/// before the response (ACP requires the history first).
enum Admission {
    New,
    Load {
        acp_session_id: String,
        messages: Vec<Value>,
    },
}

/// The one-session slot: a second `session/new` or `session/load` while a
/// session is hosted, admitted, or closing is refused.
async fn reserve_session_slot(state: &Arc<Mutex<DaemonAcpState>>) -> bool {
    let mut guard = state.lock().await;
    if guard.session.is_some() || guard.session_new_in_flight || guard.session_close_done.is_some()
    {
        return false;
    }
    guard.session_new_in_flight = true;
    true
}

const ONE_SESSION_PER_CONNECTION: &str = "prime-agent ACP mode hosts one session per connection; start another prime-agent process for a second session";

/// The daemon session the connection currently hosts: the startup one until
/// a `session/load` binds another.
pub(super) async fn bound_daemon_session(
    state: &Arc<Mutex<DaemonAcpState>>,
    binding: &DaemonBinding,
) -> String {
    state
        .lock()
        .await
        .bound_daemon_session_id
        .clone()
        .unwrap_or_else(|| binding.active_session_id.clone())
}

/// Admit one session over the connection's current daemon session.
pub(super) async fn handle_session_new(
    id: Value,
    params: Value,
    link: &Arc<DaemonLink>,
    state: &Arc<Mutex<DaemonAcpState>>,
    options: &DaemonAcpOptions,
    binding: &DaemonBinding,
    tx: producer::FrameSink,
) {
    if !reserve_session_slot(state).await {
        let _ = tx.send(super::internal_error(&id, ONE_SESSION_PER_CONNECTION));
        return;
    }
    let params = types::NewSessionParams::parse(&params);
    let daemon_session_id = bound_daemon_session(state, binding).await;
    admit_session(
        AdmissionRequest {
            id,
            params,
            daemon_session_id,
            actual_cwd: options.actual_cwd.clone(),
            admission: Admission::New,
        },
        link,
        state,
        binding,
        tx,
    )
    .await;
}

/// `session/list` (ACP `ListSessionsRequest`): the saved top-level sessions,
/// newest first, optionally only those of one cwd, in pages of
/// [`SESSION_LIST_PAGE_SIZE`] behind an opaque offset cursor.
pub(super) async fn handle_session_list(
    id: Value,
    params: Value,
    link: &Arc<DaemonLink>,
    state: &Arc<Mutex<DaemonAcpState>>,
    binding: &DaemonBinding,
    tx: producer::FrameSink,
) {
    let invalid = |reason: &str| {
        jsonrpc::error_response(
            &id,
            jsonrpc::INVALID_PARAMS,
            "Invalid params",
            Some(&json!({ "reason": reason })),
        )
    };
    let offset = match params.get("cursor") {
        None | Some(Value::Null) => 0,
        Some(cursor) => {
            let Some(offset) = cursor
                .as_str()
                .and_then(|cursor| cursor.parse::<usize>().ok())
            else {
                let _ = tx.send(invalid("Invalid session/list cursor"));
                return;
            };
            offset
        }
    };
    let cwd = params
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|cwd| !cwd.is_empty());
    let daemon_session_id = bound_daemon_session(state, binding).await;
    let rows = match saved_session_rows(link, &daemon_session_id).await {
        Ok(rows) => rows,
        Err(error) => {
            let _ = tx.send(super::internal_error(&id, &error.to_string()));
            return;
        }
    };
    let matching: Vec<&Value> = rows
        .iter()
        .filter(|row| {
            cwd.is_none_or(|cwd| {
                row.get("cwd")
                    .and_then(Value::as_str)
                    .is_some_and(|row_cwd| super::same_cwd(Path::new(cwd), Path::new(row_cwd)))
            })
        })
        .collect();
    let page: Vec<Value> = matching
        .iter()
        .skip(offset)
        .take(SESSION_LIST_PAGE_SIZE)
        .map(|row| acp_session_info(row))
        .collect();
    let next_offset = offset.saturating_add(page.len());
    let mut result = json!({ "sessions": page });
    if next_offset < matching.len() {
        result["nextCursor"] = json!(next_offset.to_string());
    }
    let _ = tx.send(jsonrpc::response(&id, &result));
}

/// The `session/list` page size (upstream #1116's).
const SESSION_LIST_PAGE_SIZE: usize = 50;

/// One saved-catalog row as ACP `SessionInfo`: the title is the session
/// name, else its first message; `updatedAt` the last activity.
fn acp_session_info(row: &Value) -> Value {
    let text = |key: &str| {
        row.get(key)
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty())
    };
    let mut info = json!({
        "sessionId": text("id").unwrap_or_default(),
        "cwd": text("cwd").unwrap_or_default(),
    });
    if let Some(title) = text("name").or_else(|| text("firstMessage")) {
        info["title"] = json!(title);
    }
    if let Some(updated_at) = text("modified") {
        info["updatedAt"] = json!(updated_at);
    }
    info
}

/// The saved catalog's top-level sessions (`list_saved_sessions` on the
/// session's own session dir), newest first. RLM children (rows with a
/// parent) are the parent's internals, never an ACP session.
async fn saved_session_rows(
    link: &Arc<DaemonLink>,
    daemon_session_id: &str,
) -> anyhow::Result<Vec<Value>> {
    let response = link
        .request(DaemonCommand::ListSavedSessions {
            id: None,
            active_session_id: Some(daemon_session_id.to_string()),
            cwd: None,
            session_dir: None,
            scope: json!("all"),
            rest: Map::default(),
        })
        .await?;
    if !response.success {
        anyhow::bail!(response
            .error
            .unwrap_or_else(|| "unknown error".to_string()));
    }
    let mut rows: Vec<Value> = response
        .data
        .and_then(|data| data.get("sessions").and_then(Value::as_array).cloned())
        .unwrap_or_default()
        .into_iter()
        .filter(|row| row.get("parentSessionPath").is_none_or(Value::is_null))
        .collect();
    let modified = |row: &Value| {
        row.get("modified")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    rows.sort_by_key(|row| std::cmp::Reverse(modified(row)));
    Ok(rows)
}

/// `session/load`: bind the saved session the id names and replay its
/// transcript before answering. A session a live worker serves is attached
/// (one file, one worker); a dormant file opens in the connection's own
/// worker (`switch_session`, with the client's cwd). The cwd rules of
/// `session/new` apply to the loaded session's real cwd.
pub(super) async fn handle_session_load(
    id: Value,
    params: Value,
    link: &Arc<DaemonLink>,
    state: &Arc<Mutex<DaemonAcpState>>,
    options: &DaemonAcpOptions,
    binding: &DaemonBinding,
    tx: producer::FrameSink,
) {
    if !reserve_session_slot(state).await {
        let _ = tx.send(super::internal_error(&id, ONE_SESSION_PER_CONNECTION));
        return;
    }
    let params = types::LoadSessionParams::parse(&params);
    match bind_saved_session(&params, link, state, binding).await {
        Ok((daemon_session_id, messages, actual_cwd)) => {
            admit_session(
                AdmissionRequest {
                    id,
                    params: params.admission(),
                    daemon_session_id,
                    actual_cwd: actual_cwd.unwrap_or_else(|| options.actual_cwd.clone()),
                    admission: Admission::Load {
                        acp_session_id: params.session_id,
                        messages,
                    },
                },
                link,
                state,
                binding,
                tx,
            )
            .await;
        }
        Err(error) => {
            state.lock().await.session_new_in_flight = false;
            let _ = tx.send(match error {
                LoadError::UnknownSession => jsonrpc::error_response(
                    &id,
                    jsonrpc::INVALID_PARAMS,
                    "Invalid params",
                    Some(&json!({
                        "reason": format!("Unknown ACP session: {}", params.session_id)
                    })),
                ),
                LoadError::Daemon(error) => super::internal_error(&id, &format!("{error:#}")),
            });
        }
    }
}

/// Why a `session/load` could not bind its session.
enum LoadError {
    /// No saved top-level session carries the id.
    UnknownSession,
    Daemon(anyhow::Error),
}

impl From<anyhow::Error> for LoadError {
    fn from(error: anyhow::Error) -> Self {
        LoadError::Daemon(error)
    }
}

/// Resolve the saved session, make the connection host it, and read its
/// transcript: the bound daemon session, the messages, and its live cwd.
async fn bind_saved_session(
    params: &types::LoadSessionParams,
    link: &Arc<DaemonLink>,
    state: &Arc<Mutex<DaemonAcpState>>,
    binding: &DaemonBinding,
) -> Result<(String, Vec<Value>, Option<PathBuf>), LoadError> {
    if params.session_id.is_empty() {
        return Err(LoadError::UnknownSession);
    }
    let current = bound_daemon_session(state, binding).await;
    let rows = saved_session_rows(link, &current).await?;
    let Some(path) = rows
        .iter()
        .find(|row| row.get("id").and_then(Value::as_str) == Some(params.session_id.as_str()))
        .and_then(|row| row.get("path").and_then(Value::as_str))
        .map(PathBuf::from)
    else {
        return Err(LoadError::UnknownSession);
    };
    let live = live_session_for_file(link, &path).await?;
    let daemon_session_id = match live {
        Some(live) if live == current => current,
        Some(live) => {
            // The connection moves to another worker: what it held on the
            // previous one (a close's input pause, its MCP servers) goes.
            let closed_pause = {
                let mut guard = state.lock().await;
                guard.closed_input_pause_key = None;
                guard.closed_input_pause_id.take()
            };
            if let Some(pause_id) = closed_pause {
                release_session_input_pause(link, &current, &pause_id).await?;
            }
            if !state.lock().await.mcp_server_names.is_empty() {
                clear_connection_servers(link, &current, &binding.mcp_owner_id, state).await?;
            }
            link.request_ok(DaemonCommand::Attach {
                id: None,
                active_session_id: live.clone(),
                client_id: None,
                capabilities: None,
                resume_cursor: None,
                telemetry_disabled: None,
                recovery_config: None,
                env: None,
                launch_env: None,
                rest: Map::default(),
            })
            .await?;
            state.lock().await.bound_daemon_session_id = Some(live.clone());
            live
        }
        None => {
            link.request_ok(DaemonCommand::SwitchSession {
                id: None,
                active_session_id: current.clone(),
                session_path: path.to_string_lossy().to_string(),
                cwd_override: params.cwd.clone().filter(|cwd| !cwd.is_empty()),
                rest: Map::default(),
            })
            .await?;
            current
        }
    };
    let messages = fetch_messages(link, &daemon_session_id).await?;
    let actual_cwd = fetch_connection_state(link, &daemon_session_id)
        .await
        .and_then(|state| state.get("cwd").and_then(Value::as_str).map(PathBuf::from));
    Ok((daemon_session_id, messages, actual_cwd))
}

/// The live daemon session serving `path`, if any (the supervisor's `list`,
/// owned sessions included, matched on the canonical session file).
async fn live_session_for_file(
    link: &Arc<DaemonLink>,
    path: &Path,
) -> anyhow::Result<Option<String>> {
    let response = link
        .request(DaemonCommand::List {
            id: None,
            all: None,
            cwd: None,
            session_dir: None,
            include_client_owned: Some(true),
            include_remote_mesh: None,
            rest: Map::default(),
        })
        .await?;
    if !response.success {
        anyhow::bail!(response
            .error
            .unwrap_or_else(|| "unknown error".to_string()));
    }
    let canonical =
        |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let target = canonical(path);
    Ok(response
        .data
        .and_then(|data| data.get("sessions").and_then(Value::as_array).cloned())
        .unwrap_or_default()
        .iter()
        .find(|row| {
            row.get("sessionFile")
                .and_then(Value::as_str)
                .is_some_and(|file| canonical(Path::new(file)) == target)
        })
        .and_then(|row| row.get("activeSessionId").and_then(Value::as_str))
        .map(str::to_string))
}

/// The session's transcript (`get_messages`).
async fn fetch_messages(
    link: &Arc<DaemonLink>,
    daemon_session_id: &str,
) -> anyhow::Result<Vec<Value>> {
    let response = link
        .request(DaemonCommand::GetMessages {
            id: None,
            active_session_id: daemon_session_id.to_string(),
            rest: Map::default(),
        })
        .await?;
    if !response.success {
        anyhow::bail!(response
            .error
            .unwrap_or_else(|| "unknown error".to_string()));
    }
    Ok(response
        .data
        .and_then(|data| data.get("messages").and_then(Value::as_array).cloned())
        .unwrap_or_default())
}

struct AdmissionRequest {
    id: Value,
    params: types::NewSessionParams,
    daemon_session_id: String,
    actual_cwd: PathBuf,
    admission: Admission,
}

/// Host one daemon session on the reserved slot: admit its MCP servers,
/// publish the pickers and the live children, then answer. A `session/new`
/// answers first and its held updates flow after; a `session/load` streams
/// the replayed transcript first and answers after it.
async fn admit_session(
    request: AdmissionRequest,
    link: &Arc<DaemonLink>,
    state: &Arc<Mutex<DaemonAcpState>>,
    binding: &DaemonBinding,
    tx: producer::FrameSink,
) {
    let AdmissionRequest {
        id,
        params,
        daemon_session_id,
        actual_cwd,
        admission,
    } = request;
    if !state.lock().await.mcp_server_names.is_empty() {
        if let Err(error) =
            clear_connection_servers(link, &daemon_session_id, &binding.mcp_owner_id, state).await
        {
            state.lock().await.session_new_in_flight = false;
            let _ = tx.send(super::internal_error(&id, &error.to_string()));
            return;
        }
    }
    // MCP admission runs after the pending-clear retry: a rejected list
    // fails the request with the same error payloads.
    let resolved = match super::mcp::resolve_acp_mcp_servers(&params.mcp_servers, &actual_cwd) {
        Ok(resolved) => resolved,
        Err(reason) => {
            state.lock().await.session_new_in_flight = false;
            let _ = tx.send(jsonrpc::error_response(
                &id,
                jsonrpc::INVALID_PARAMS,
                "Invalid params",
                Some(&json!({ "reason": reason })),
            ));
            return;
        }
    };
    if let Err(details) = super::mcp::acp_mcp_tool_names(&resolved) {
        state.lock().await.session_new_in_flight = false;
        let _ = tx.send(super::internal_error(&id, &details));
        return;
    }

    // Neither picker fetch may fail the admission: discovery failures
    // catch to an empty list, a state fetch failure degrades to no
    // options.
    let (state_value, models) = (
        fetch_connection_state(link, &daemon_session_id).await,
        fetch_available_models(link, &daemon_session_id)
            .await
            .unwrap_or_default(),
    );
    // ACP needs the id `session/new` answers to be loadable later: a
    // persisted session answers its saved id (upstream #2804); a session
    // with no file (`--no-session`) a fresh one.
    let (acp_session_id, replay) = match admission {
        Admission::New => (
            state_value
                .as_ref()
                .filter(|state| {
                    state
                        .get("sessionFile")
                        .and_then(Value::as_str)
                        .is_some_and(|file| !file.is_empty())
                })
                .and_then(|state| state.get("sessionId").and_then(Value::as_str))
                .filter(|session_id| !session_id.is_empty())
                .map_or_else(|| uuid::Uuid::new_v4().to_string(), str::to_string),
            None,
        ),
        Admission::Load {
            acp_session_id,
            messages,
        } => (acp_session_id, Some(messages)),
    };
    let producer = UpdateProducer::new(acp_session_id.clone(), tx.clone());
    let published = picker_options_from_state(state_value.as_ref(), &models);
    let context_window = state_context_window(state_value.as_ref(), &models);
    let config = Arc::new(HostedConfig {
        queue: tokio::sync::Mutex::new(()),
        published: tokio::sync::Mutex::new(published),
        models: tokio::sync::Mutex::new(models),
        context_window: std::sync::atomic::AtomicU64::new(context_window),
    });
    let mut hosted = HostedSession {
        acp_session_id: acp_session_id.clone(),
        daemon_active_session_id: daemon_session_id.clone(),
        producer,
        config,
        cancelling: false,
        stop_failure: None,
        input_pause_key: None,
        input_pause_id: None,
        cancel_task: None,
        turn: None,
        assistant_stop_reason: None,
        mapping: WireMappingState::default(),
        observed_children: std::collections::HashSet::new(),
    };
    // The ACP MCP servers ride the wire command, not a local manager.
    let replace_skipped = resolved.is_empty() && state.lock().await.mcp_server_names.is_empty();
    if !replace_skipped {
        if let Err(error) =
            replace_connection_servers(link, &daemon_session_id, &binding.mcp_owner_id, &resolved)
                .await
        {
            // The worker may have applied the list before this failed (a
            // lost acknowledgement); the clear is best-effort, like TS.
            let _ =
                clear_connection_servers(link, &daemon_session_id, &binding.mcp_owner_id, state)
                    .await;
            state.lock().await.session_new_in_flight = false;
            let _ = tx.send(super::internal_error(&id, &error.to_string()));
            return;
        }
        let names = resolved
            .iter()
            .map(pa_core::mcp::AcpMcpServerConfig::name)
            .map(str::to_string)
            .collect();
        state.lock().await.mcp_server_names = names;
    }

    let mut result = json!({ "configOptions": *hosted.config.published.lock().await });
    if replay.is_none() {
        result["sessionId"] = json!(acp_session_id);
    }
    if let Some(requested) = params.cwd.as_deref().filter(|cwd| !cwd.is_empty()) {
        let actual = actual_cwd.display().to_string();
        if !super::same_cwd(Path::new(requested), &actual_cwd) {
            result["_meta"] = meta::prime_agent_meta(&PrimeAgentSessionMeta {
                cwd: Some(meta::PrimeAgentCwdMeta {
                    requested: requested.to_string(),
                    actual,
                }),
                ..Default::default()
            });
        }
    }
    // Install the hosted session before the admission response leaves
    // (an immediate `session/set_config_option` must resolve against
    // it). The producer gate opens only after the response is queued,
    // so no held update can precede it.
    let producer = Arc::clone(&hosted.producer);
    let inherited_pause = {
        let mut guard = state.lock().await;
        guard.session_new_in_flight = false;
        if let Some(pause_id) = guard.closed_input_pause_id.clone() {
            hosted.input_pause_id = Some(pause_id);
            hosted.input_pause_key = guard.closed_input_pause_key.clone();
        }
        guard.session = Some(hosted);
        guard
            .closed_input_pause_id
            .clone()
            .zip(guard.closed_input_pause_key.clone())
    };
    let children = match fetch_rlm_children(link, &daemon_session_id).await {
        Ok(children) => children,
        Err(error) => {
            let _ =
                clear_connection_servers(link, &daemon_session_id, &binding.mcp_owner_id, state)
                    .await;
            let mut guard = state.lock().await;
            guard.session = None;
            guard.session_new_in_flight = false;
            drop(guard);
            let _ = tx.send(super::internal_error(&id, &error.to_string()));
            return;
        }
    };
    {
        let mut guard = state.lock().await;
        if let Some(current) = guard.session.as_mut() {
            for child in children {
                let id = child.get("id").and_then(Value::as_str).unwrap_or_default();
                if !current.observed_children.insert(id.to_string()) {
                    continue;
                }
                let event = json!({ "type": "rlm_child_update", "child": child });
                for update in wire_events::wire_updates(&event, &mut current.mapping) {
                    let _ = current
                        .producer
                        .publish(&update, 0, PrimeAgentEventPhase::Event, None)
                        .await;
                }
            }
        }
    }
    if let Some(messages) = replay {
        // The history precedes the `session/load` response: the gate opens
        // first and the replay publishes in transcript order.
        producer.commit_session_new_response().await;
        for update in wire_events::transcript_updates(&messages) {
            if !producer
                .publish(&update, 0, PrimeAgentEventPhase::Event, None)
                .await
            {
                break;
            }
        }
        if let Some((pause_id, lease_key)) = inherited_pause {
            release_inherited_pause(link, state, &daemon_session_id, &pause_id, &lease_key).await;
        }
        let _ = tx.send(jsonrpc::response(&id, &result));
        return;
    }
    let _ = tx.send(jsonrpc::response(&id, &result));
    if let Some((pause_id, lease_key)) = inherited_pause {
        release_inherited_pause(link, state, &daemon_session_id, &pause_id, &lease_key).await;
    }
    producer.commit_session_new_response().await;
}

/// Release the input pause a close left for the next admission; a failed
/// release becomes the hosted session's stop failure.
async fn release_inherited_pause(
    link: &Arc<DaemonLink>,
    state: &Arc<Mutex<DaemonAcpState>>,
    daemon_session_id: &str,
    pause_id: &str,
    lease_key: &str,
) {
    match release_session_input_pause(link, daemon_session_id, pause_id).await {
        Ok(()) => {
            let mut guard = state.lock().await;
            if guard.closed_input_pause_id.as_deref() == Some(pause_id) {
                guard.closed_input_pause_id = None;
                guard.closed_input_pause_key = None;
            }
            if let Some(hosted) = guard
                .session
                .as_mut()
                .filter(|hosted| hosted.input_pause_id.as_deref() == Some(pause_id))
            {
                hosted.input_pause_id = None;
                if hosted.input_pause_key.as_deref() == Some(lease_key) {
                    hosted.input_pause_key = None;
                }
            }
        }
        Err(error) => {
            if let Some(hosted) = state
                .lock()
                .await
                .session
                .as_mut()
                .filter(|hosted| hosted.input_pause_id.as_deref() == Some(pause_id))
            {
                hosted.stop_failure = Some(error.to_string());
            }
        }
    }
}
