//! Offline prune of a saved session file: replace oversized entry content
//! (pasted images, giant text blocks) with a short stub so a bloated or wedged
//! session opens and compacts again -- reversibly.
//!
//! A session that has grown past every model's context window (for example a
//! few multi-megabyte pasted images) can no longer be summarized in one call,
//! so auto-compaction fails and the session is stuck. This operates on the
//! session's `.jsonl` file directly (so it works even when the session cannot
//! be opened), keeping each entry's id/parent/type intact and only stubbing the
//! heavy content. The original content of every pruned entry is written to a
//! sidecar (`<session>.jsonl.pruned.json`) so [`restore`] can put it back, and
//! the whole file is backed up first.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use std::{fs, io};

use serde_json::{Map, Value, json};

/// A text block longer than this (characters) is stubbed on an explicit prune.
pub const LARGE_TEXT_CHARS: usize = 50_000;

/// One prunable (message) entry, for the `--list` view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryInfo {
    /// The entry's line index in the file (0-based; the header is line 0).
    pub index: usize,
    pub id: Option<String>,
    /// On-disk size of the entry's JSON line, in bytes.
    pub bytes: usize,
    /// `user` / `assistant` / `toolResult` / ... (the message role).
    pub role: String,
    /// A short human summary, e.g. `4 image(s), ~16.8 MB` or `text 1234 chars`.
    pub descriptor: String,
    /// Whether this entry is currently pruned (its original is in the sidecar).
    pub pruned: bool,
}

/// What a [`prune`] call did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PruneReport {
    pub pruned_indices: Vec<usize>,
    pub removed_images: usize,
    pub removed_bytes: usize,
    pub backup: PathBuf,
    pub before_bytes: u64,
    pub after_bytes: u64,
}

/// What a [`restore`] call did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreReport {
    pub restored_ids: Vec<String>,
    pub backup: PathBuf,
}

/// The sidecar that holds pruned entries' original messages, keyed by entry id.
#[must_use]
pub fn sidecar_path(session_path: &Path) -> PathBuf {
    let mut raw: OsString = session_path.as_os_str().to_owned();
    raw.push(".pruned.json");
    PathBuf::from(raw)
}

fn backup_path(session_path: &Path) -> PathBuf {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let mut raw: OsString = session_path.as_os_str().to_owned();
    raw.push(format!(".bak-prune-{ts}"));
    PathBuf::from(raw)
}

fn read_lines(path: &Path) -> io::Result<(bool, Vec<String>)> {
    let raw = fs::read_to_string(path)?;
    let trailing = raw.ends_with('\n');
    let mut lines: Vec<String> = raw.split('\n').map(str::to_string).collect();
    if trailing {
        lines.pop();
    }
    Ok((trailing, lines))
}

fn write_lines(path: &Path, trailing: bool, lines: &[String]) -> io::Result<()> {
    let mut out = lines.join("\n");
    if trailing {
        out.push('\n');
    }
    let mut tmp: OsString = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    fs::write(&tmp, out)?;
    fs::rename(&tmp, path)
}

fn load_sidecar(path: &Path) -> Map<String, Value> {
    fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

/// The role (message entries) or entry type (others), for display.
fn entry_role(entry: &Value) -> String {
    entry
        .get("message")
        .and_then(|message| message.get("role"))
        .and_then(Value::as_str)
        .or_else(|| entry.get("type").and_then(Value::as_str))
        .unwrap_or("?")
        .to_string()
}

/// Summarize an entry's heavy content for the list view.
fn describe(entry: &Value) -> String {
    let Some(content) = entry
        .get("message")
        .and_then(|message| message.get("content"))
    else {
        return "(no content)".to_string();
    };
    match content {
        Value::Array(blocks) => {
            let mut images = 0usize;
            let mut image_bytes = 0usize;
            let mut text_chars = 0usize;
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("image") => {
                        images += 1;
                        image_bytes += block
                            .get("data")
                            .and_then(Value::as_str)
                            .map_or(0, str::len);
                    }
                    Some("text") => {
                        text_chars += block
                            .get("text")
                            .and_then(Value::as_str)
                            .map_or(0, str::len);
                    }
                    _ => {}
                }
            }
            let mut parts = Vec::new();
            if images > 0 {
                parts.push(format!(
                    "{images} image(s), ~{:.1} MB",
                    image_bytes as f64 / 1e6
                ));
            }
            if text_chars > 0 {
                parts.push(format!("text {text_chars} chars"));
            }
            if parts.is_empty() {
                format!("{} block(s)", blocks.len())
            } else {
                parts.join(", ")
            }
        }
        Value::String(text) => format!("text {} chars", text.len()),
        _ => "(empty)".to_string(),
    }
}

/// Stub an entry's heavy content in place; returns `(images_removed, bytes_removed)`.
fn stub_content(message: &mut Value) -> (usize, usize) {
    let Some(content) = message.get_mut("content") else {
        return (0, 0);
    };
    match content {
        Value::Array(blocks) => {
            let mut images = 0usize;
            let mut bytes = 0usize;
            let mut stubbed: Vec<Value> = Vec::with_capacity(blocks.len());
            for block in blocks.iter() {
                let kind = block.get("type").and_then(Value::as_str).unwrap_or("");
                if kind == "image" {
                    images += 1;
                    bytes += block
                        .get("data")
                        .and_then(Value::as_str)
                        .map_or(0, str::len);
                    stubbed.push(json!({ "type": "text", "text": "[pruned image]" }));
                } else if kind == "text" {
                    let text = block.get("text").and_then(Value::as_str).unwrap_or("");
                    if text.len() > LARGE_TEXT_CHARS {
                        bytes += text.len();
                        stubbed.push(
                            json!({ "type": "text", "text": format!("[pruned text: {} chars]", text.len()) }),
                        );
                    } else {
                        stubbed.push(block.clone());
                    }
                } else {
                    let size = serde_json::to_string(block).map_or(0, |s| s.len());
                    if size > LARGE_TEXT_CHARS {
                        bytes += size;
                        stubbed.push(
                            json!({ "type": "text", "text": format!("[pruned {kind} block: {size} bytes]") }),
                        );
                    } else {
                        stubbed.push(block.clone());
                    }
                }
            }
            *blocks = stubbed;
            (images, bytes)
        }
        Value::String(text) if text.len() > LARGE_TEXT_CHARS => {
            let removed = text.len();
            *content = json!(format!("[pruned text: {removed} chars]"));
            (0, removed)
        }
        _ => (0, 0),
    }
}

/// List the message entries of a session, largest first.
///
/// # Errors
/// Returns an error if the file cannot be read.
pub fn list(session_path: &Path) -> io::Result<Vec<EntryInfo>> {
    let (_, lines) = read_lines(session_path)?;
    let sidecar = load_sidecar(&sidecar_path(session_path));
    let mut infos: Vec<EntryInfo> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if entry.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let id = entry.get("id").and_then(Value::as_str).map(str::to_string);
        let pruned = id.as_deref().is_some_and(|id| sidecar.contains_key(id));
        infos.push(EntryInfo {
            index,
            bytes: line.len(),
            role: entry_role(&entry),
            descriptor: describe(&entry),
            pruned,
            id,
        });
    }
    infos.sort_by_key(|info| std::cmp::Reverse(info.bytes));
    Ok(infos)
}

/// Prune the entries at `indices` (line indices into the file): back the file
/// up, stub each entry's heavy content, and record the originals in the sidecar.
///
/// # Errors
/// Returns an error string if the file cannot be read or written.
pub fn prune(session_path: &Path, indices: &[usize]) -> Result<PruneReport, String> {
    let before_bytes = fs::metadata(session_path).map_or(0, |m| m.len());
    let (trailing, mut lines) = read_lines(session_path).map_err(|e| e.to_string())?;
    let sidecar_file = sidecar_path(session_path);
    let mut sidecar = load_sidecar(&sidecar_file);
    let backup = backup_path(session_path);
    fs::copy(session_path, &backup).map_err(|e| format!("backup failed: {e}"))?;

    let mut pruned_indices = Vec::new();
    let mut removed_images = 0usize;
    let mut removed_bytes = 0usize;
    for &index in indices {
        let Some(line) = lines.get(index) else {
            return Err(format!("entry index {index} is out of range"));
        };
        let mut entry: Value =
            serde_json::from_str(line).map_err(|e| format!("entry {index} is not JSON: {e}"))?;
        if entry.get("type").and_then(Value::as_str) != Some("message") {
            return Err(format!("entry {index} is not a message (not prunable)"));
        }
        let Some(message) = entry.get("message").cloned() else {
            continue;
        };
        let mut new_message = message.clone();
        let (images, bytes) = stub_content(&mut new_message);
        if bytes == 0 {
            continue; // nothing heavy to prune
        }
        if let Some(id) = entry.get("id").and_then(Value::as_str) {
            // Keep the first original if the entry was pruned before.
            sidecar.entry(id.to_string()).or_insert(message);
        }
        entry["message"] = new_message;
        lines[index] =
            serde_json::to_string(&entry).map_err(|e| format!("re-encode {index}: {e}"))?;
        pruned_indices.push(index);
        removed_images += images;
        removed_bytes += bytes;
    }

    if pruned_indices.is_empty() {
        let _ = fs::remove_file(&backup);
        return Err("nothing to prune in the given entries".to_string());
    }
    fs::write(
        &sidecar_file,
        serde_json::to_string(&Value::Object(sidecar)).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("sidecar write failed: {e}"))?;
    write_lines(session_path, trailing, &lines).map_err(|e| e.to_string())?;
    let after_bytes = fs::metadata(session_path).map_or(0, |m| m.len());
    Ok(PruneReport {
        pruned_indices,
        removed_images,
        removed_bytes,
        backup,
        before_bytes,
        after_bytes,
    })
}

/// Prune the `n` largest prunable entries.
///
/// # Errors
/// Returns an error string if the file cannot be read or written.
pub fn prune_biggest(session_path: &Path, n: usize) -> Result<PruneReport, String> {
    let infos = list(session_path).map_err(|e| e.to_string())?;
    let indices: Vec<usize> = infos
        .iter()
        .filter(|info| !info.pruned)
        .take(n)
        .map(|info| info.index)
        .collect();
    if indices.is_empty() {
        return Err("no prunable entries found".to_string());
    }
    prune(session_path, &indices)
}

/// Restore pruned entries from the sidecar (`ids = None` restores all), backing
/// the file up first.
///
/// # Errors
/// Returns an error string if nothing can be restored or on an IO failure.
pub fn restore(session_path: &Path, ids: Option<&[String]>) -> Result<RestoreReport, String> {
    let sidecar_file = sidecar_path(session_path);
    let mut sidecar = load_sidecar(&sidecar_file);
    if sidecar.is_empty() {
        return Err("no pruned entries to restore (no sidecar)".to_string());
    }
    let (trailing, mut lines) = read_lines(session_path).map_err(|e| e.to_string())?;
    let backup = backup_path(session_path);
    fs::copy(session_path, &backup).map_err(|e| format!("backup failed: {e}"))?;

    let wanted: Option<std::collections::HashSet<&str>> =
        ids.map(|ids| ids.iter().map(String::as_str).collect());
    let mut restored = Vec::new();
    for line in &mut lines {
        let Ok(mut entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(id) = entry.get("id").and_then(Value::as_str).map(str::to_string) else {
            continue;
        };
        if wanted
            .as_ref()
            .is_some_and(|set| !set.contains(id.as_str()))
        {
            continue;
        }
        if let Some(original) = sidecar.remove(&id) {
            entry["message"] = original;
            *line = serde_json::to_string(&entry).map_err(|e| e.to_string())?;
            restored.push(id);
        }
    }
    if restored.is_empty() {
        let _ = fs::remove_file(&backup);
        return Err("no matching pruned entries to restore".to_string());
    }
    if sidecar.is_empty() {
        let _ = fs::remove_file(&sidecar_file);
    } else {
        fs::write(
            &sidecar_file,
            serde_json::to_string(&Value::Object(sidecar)).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
    }
    write_lines(session_path, trailing, &lines).map_err(|e| e.to_string())?;
    Ok(RestoreReport {
        restored_ids: restored,
        backup,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_session(dir: &Path) -> PathBuf {
        let big_text = "x".repeat(LARGE_TEXT_CHARS + 10);
        let image = "A".repeat(2_000_000); // ~2 MB of base64
        let lines = [
            json!({"type":"session","id":"s","version":3,"cwd":"/tmp"}).to_string(),
            json!({"type":"message","id":"e1","parentId":null,
                "message":{"role":"user","content":[{"type":"text","text":"hi"}]}})
            .to_string(),
            json!({"type":"message","id":"e2","parentId":"e1",
                "message":{"role":"user","content":[
                    {"type":"text","text":"look"},
                    {"type":"image","mimeType":"image/png","data":image},
                    {"type":"image","mimeType":"image/png","data":"B".repeat(1_000_000)}]}})
            .to_string(),
            json!({"type":"message","id":"e3","parentId":"e2",
                "message":{"role":"user","content":[{"type":"text","text":big_text}]}})
            .to_string(),
        ];
        let path = dir.join("s.jsonl");
        fs::write(&path, lines.join("\n") + "\n").unwrap();
        path
    }

    #[test]
    fn list_orders_entries_largest_first_with_descriptors() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_session(dir.path());
        let infos = list(&path).unwrap();
        assert_eq!(infos.len(), 3, "three message entries");
        assert_eq!(infos[0].index, 2, "the two-image entry is largest");
        assert!(infos[0].descriptor.contains("image(s)"));
        assert!(infos.iter().all(|info| !info.pruned));
    }

    #[test]
    fn prune_stubs_shrinks_and_is_restorable() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_session(dir.path());
        let before = fs::metadata(&path).unwrap().len();
        let report = prune(&path, &[2, 3]).unwrap();
        assert_eq!(report.removed_images, 2);
        assert!(report.after_bytes < before / 2, "file shrank a lot");
        assert!(report.backup.exists(), "a backup was written");
        assert!(sidecar_path(&path).exists(), "originals saved");
        // The pruned entries now read as pruned, with stubs in place.
        let infos = list(&path).unwrap();
        let e2 = infos
            .iter()
            .find(|info| info.id.as_deref() == Some("e2"))
            .unwrap();
        assert!(e2.pruned);
        // Restore brings the originals back and clears the sidecar.
        let restored = restore(&path, None).unwrap();
        assert_eq!(restored.restored_ids.len(), 2);
        assert!(!sidecar_path(&path).exists());
        assert_eq!(
            fs::metadata(&path).unwrap().len(),
            before,
            "byte-identical restore"
        );
    }

    #[test]
    fn prune_biggest_takes_the_largest() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_session(dir.path());
        let report = prune_biggest(&path, 1).unwrap();
        assert_eq!(report.pruned_indices, vec![2]);
    }

    #[test]
    fn pruning_a_non_message_entry_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_session(dir.path());
        assert!(
            prune(&path, &[0]).is_err(),
            "the session header is not prunable"
        );
    }

    #[test]
    fn restoring_with_no_sidecar_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_session(dir.path());
        assert!(restore(&path, None).is_err());
    }
}
