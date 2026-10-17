//! The durable tasks file: Claude Code's `scheduled_tasks.json` shape
//! (`{"tasks": [{id, cron, prompt, createdAt, lastFiredAt?, recurring?}]}`),
//! kept in the session's artifact partition. Malformed entries are skipped
//! on read, like Claude Code's loader.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The file name in the session's artifact partition.
pub const SCHEDULED_TASKS_FILENAME: &str = "scheduled_tasks.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DurableTask {
    pub id: String,
    pub cron: String,
    pub prompt: String,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fired_at: Option<i64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub recurring: bool,
}

#[derive(Serialize)]
struct FileShape<'a> {
    tasks: &'a [DurableTask],
}

/// Read the durable tasks; a missing or unreadable file is empty.
#[must_use]
pub fn read_durable_tasks(path: &Path) -> Vec<DurableTask> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        tracing::warn!(path = %path.display(), "scheduled tasks file is not JSON; ignoring it");
        return Vec::new();
    };
    let Some(tasks) = value.get("tasks").and_then(Value::as_array) else {
        return Vec::new();
    };
    tasks
        .iter()
        .filter_map(|task| {
            let parsed = serde_json::from_value::<DurableTask>(task.clone()).ok();
            if parsed.is_none() {
                tracing::warn!("skipping a malformed scheduled task");
            }
            parsed
        })
        .collect()
}

/// Write the durable tasks (atomically); an empty set removes the file.
///
/// # Errors
///
/// The directory or the file cannot be written.
pub fn write_durable_tasks(path: &Path, tasks: &[DurableTask]) -> anyhow::Result<()> {
    if tasks.is_empty() {
        match std::fs::remove_file(path) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut text = serde_json::to_string_pretty(&FileShape { tasks })?;
    text.push('\n');
    crate::settings::storage::atomic_write(path, &text)
}
