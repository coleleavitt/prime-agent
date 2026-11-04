//! Host requests for `rlm.memory.*`: seven thin wrappers over the
//! [`LocalStore`]. The Python side exposes each as a function; the kernel
//! sends `memory.<op>` and the handler returns the op's payload. A
//! `MemoryError` comes back as `{"error": <message>, "code": <stable>}`
//! for the kernel to raise as `ValueError`; an I/O panic stays an
//! infrastructure error (`RuntimeError`).

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};

use super::local::ListEntry;
use super::{LocalStore, MemoryError, MemoryPath, SearchHit};
use crate::kernel::shared::{HostRequestHandlers, HostRequestPayload, host_handler};

/// Every host-request name this module answers. Kept in a list so the
/// prompt-guards test sees them without enumerating one by one.
pub const MEMORY_HOST_REQUESTS: &[&str] = &[
    "memory.read",
    "memory.write",
    "memory.append",
    "memory.str_replace",
    "memory.delete",
    "memory.list",
    "memory.search",
];

const DEFAULT_SEARCH_LIMIT: usize = 100;
const MAX_SEARCH_LIMIT: usize = 500;

/// Register the `memory.*` handlers against `store` (one per session).
pub fn register_host_handlers(handlers: &mut HostRequestHandlers, root: PathBuf) {
    let store = Arc::new(LocalStore::new(root));
    for &name in MEMORY_HOST_REQUESTS {
        let store = store.clone();
        handlers.register(
            name,
            host_handler(move |payload| {
                let store = store.clone();
                async move { dispatch(name, &store, &payload) }
            }),
        );
    }
}

fn dispatch(
    request: &str,
    store: &LocalStore,
    payload: &HostRequestPayload,
) -> anyhow::Result<Value> {
    let outcome = match request {
        "memory.read" => read(store, &payload.data),
        "memory.write" => write(store, &payload.data),
        "memory.append" => append(store, &payload.data),
        "memory.str_replace" => str_replace(store, &payload.data),
        "memory.delete" => delete(store, &payload.data),
        "memory.list" => list(store, &payload.data),
        "memory.search" => search(store, &payload.data),
        other => {
            return Err(anyhow::anyhow!(
                "memory host request: unknown operation `{other}`"
            ));
        }
    };
    match outcome {
        Ok(value) => Ok(value),
        Err(error) => Ok(json!({
            "error": error.to_string(),
            "code": error.code(),
        })),
    }
}

fn read(store: &LocalStore, data: &Value) -> Result<Value, MemoryError> {
    let path = memory_path(data)?;
    let content = store.read(&path)?;
    Ok(json!({ "content": content }))
}

fn write(store: &LocalStore, data: &Value) -> Result<Value, MemoryError> {
    let path = memory_path(data)?;
    let content = content_field(data)?;
    store.write(&path, content)?;
    Ok(json!({ "bytes": content.len() }))
}

fn append(store: &LocalStore, data: &Value) -> Result<Value, MemoryError> {
    let path = memory_path(data)?;
    let content = content_field(data)?;
    store.append(&path, content)?;
    Ok(json!({ "bytes": content.len() }))
}

fn str_replace(store: &LocalStore, data: &Value) -> Result<Value, MemoryError> {
    let path = memory_path(data)?;
    let old = string_field(data, "old")?;
    let new = string_field(data, "new")?;
    store.str_replace(&path, old, new)?;
    Ok(json!({ "replaced": true }))
}

fn delete(store: &LocalStore, data: &Value) -> Result<Value, MemoryError> {
    let path = memory_path(data)?;
    store.delete(&path)?;
    Ok(json!({ "deleted": true }))
}

fn list(store: &LocalStore, data: &Value) -> Result<Value, MemoryError> {
    let prefix = match data.get("prefix").and_then(Value::as_str) {
        Some(prefix) if !prefix.is_empty() => Some(MemoryPath::parse(prefix)?),
        _ => None,
    };
    let entries = store.list(prefix.as_ref())?;
    Ok(json!({ "entries": entries.iter().map(list_entry_json).collect::<Vec<_>>() }))
}

fn search(store: &LocalStore, data: &Value) -> Result<Value, MemoryError> {
    let query = string_field(data, "query")?;
    let limit = data
        .get("limit")
        .and_then(Value::as_u64)
        .map_or(DEFAULT_SEARCH_LIMIT, |value| {
            value.min(MAX_SEARCH_LIMIT as u64) as usize
        });
    let hits = store.search(query, limit)?;
    Ok(json!({ "hits": hits.iter().map(search_hit_json).collect::<Vec<_>>() }))
}

fn memory_path(data: &Value) -> Result<MemoryPath, MemoryError> {
    let raw = string_field(data, "path")?;
    Ok(MemoryPath::parse(raw)?)
}

fn string_field<'a>(data: &'a Value, key: &str) -> Result<&'a str, MemoryError> {
    data.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| MemoryError::Io(std::io::Error::other(format!("missing `{key}` string"))))
}

fn content_field(data: &Value) -> Result<&str, MemoryError> {
    data.get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| MemoryError::Io(std::io::Error::other("missing `content` string")))
}

fn list_entry_json(entry: &ListEntry) -> Value {
    let mut map = json!({
        "path": entry.path,
        "size": entry.size,
    });
    if let Some(modified) = entry.modified_unix {
        map["modified_unix"] = json!(modified);
    }
    map
}

fn search_hit_json(hit: &SearchHit) -> Value {
    json!({
        "path": hit.path,
        "line_number": hit.line_number,
        "line": hit.line,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> (tempfile::TempDir, LocalStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = LocalStore::new(dir.path().to_path_buf());
        (dir, store)
    }

    #[test]
    fn write_then_read_through_the_json_surface() {
        let (_root, store) = env();
        let reply = write(&store, &json!({"path": "hello.md", "content": "hi"})).unwrap();
        assert_eq!(reply["bytes"], json!(2));
        let reply = read(&store, &json!({"path": "hello.md"})).unwrap();
        assert_eq!(reply["content"], json!("hi"));
    }

    #[test]
    fn invalid_path_surfaces_the_stable_code() {
        let (_root, store) = env();
        let outcome = dispatch(
            "memory.write",
            &store,
            &HostRequestPayload {
                data: json!({"path": "../escape", "content": "x"}),
            },
        )
        .unwrap();
        assert_eq!(outcome["code"], json!("invalid_path"));
    }

    #[test]
    fn unknown_op_is_an_infra_error() {
        let (_root, store) = env();
        let outcome = dispatch(
            "memory.unknown",
            &store,
            &HostRequestPayload { data: json!({}) },
        );
        assert!(outcome.is_err());
    }

    #[test]
    fn list_returns_entries_with_sizes() {
        let (_root, store) = env();
        store
            .write(&MemoryPath::parse("one.md").unwrap(), "hello")
            .unwrap();
        let reply = list(&store, &json!({})).unwrap();
        let entries = reply["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["path"], json!("one.md"));
        assert_eq!(entries[0]["size"], json!(5));
    }

    #[test]
    fn search_returns_line_hits() {
        let (_root, store) = env();
        store
            .write(
                &MemoryPath::parse("t.md").unwrap(),
                "line a\nline b\nline a\n",
            )
            .unwrap();
        let reply = search(&store, &json!({"query": "line a"})).unwrap();
        let hits = reply["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 2);
    }
}
