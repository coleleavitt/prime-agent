//! The kernel's write reports: `file_history.before_write` and
//! `file_history.after_write`, sent by the bundled `edit` skill around each
//! write. Bookkeeping, not policy: the write itself stays in the kernel
//! (under its sandbox and plan guard), and the skill writes even when a
//! report fails.

use std::path::PathBuf;

use serde_json::{Value, json};

use super::FileHistory;
use crate::kernel::shared::{HostRequestHandlers, HostRequestPayload, host_handler};

/// The request the kernel sends before a write replaces a file.
pub const BEFORE_WRITE_REQUEST: &str = "file_history.before_write";

/// The request the kernel sends after the write landed.
pub const AFTER_WRITE_REQUEST: &str = "file_history.after_write";

/// Register the write reports for a session whose history is `history`.
pub fn register_host_handlers(handlers: &mut HostRequestHandlers, history: &FileHistory) {
    let before = history.clone();
    handlers.register(
        "file_history.before_write",
        host_handler(move |payload| {
            let history = before.clone();
            record(payload, move |path| {
                history.before_write(&path, super::now_ms())
            })
        }),
    );
    let after = history.clone();
    handlers.register(
        "file_history.after_write",
        host_handler(move |payload| {
            let history = after.clone();
            record(payload, move |path| {
                history.after_write(&path, super::now_ms())
            })
        }),
    );
}

async fn record(
    payload: HostRequestPayload,
    write: impl FnOnce(PathBuf) -> std::io::Result<()> + Send + 'static,
) -> anyhow::Result<Value> {
    let path = payload
        .data
        .get("path")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("file_history: `path` must be a non-empty string"))?;
    tokio::task::spawn_blocking(move || write(path))
        .await
        .map_err(|join| anyhow::anyhow!("file_history: the record task failed: {join}"))??;
    Ok(json!({ "recorded": true }))
}
