//! The harness store's on-disk document: one lenient reader and one
//! durable writer, shared by every producer of `harness_state.json` (the
//! system-prompt digest, `/refine`, `/harness`, and the kernel's
//! `rlm.harness`).
//!
//! Reading never fails: a missing, unreadable, corrupt, or non-object file
//! reads as the empty state (prompt builds run on every turn), and the
//! reason is reported beside it. Entries are read leniently, the way the
//! kernel always read them: a malformed optional field takes its default
//! instead of dropping the whole entry, and the id and kind come from the
//! entry's position in the document, not from fields inside it.
//!
//! Writing replaces the file atomically (temp file + rename) on the file a
//! symlink names, keeps the destination's permission bits (a new file is
//! created owner-only, within the umask), and first copies aside a file
//! that does not parse, so a corrupt store is never silently wiped.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use super::pyfmt;
use crate::refinement::{
    empty_harness_state, HarnessEntry, HarnessRefinementEvent, HarnessScope, HarnessState,
    RefinementKind, REFINEMENT_KINDS,
};

/// Entry keys the store models; every other key on an entry is carried in
/// [`HarnessEntry::extensions`].
const MODELLED_ENTRY_KEYS: [&str; 13] = [
    "id",
    "kind",
    "title",
    "content",
    "path",
    "scope",
    "reference",
    "arguments",
    "metadata",
    "source",
    "created_at",
    "updated_at",
    "version",
];

/// The provenance an entry written before the kernel/refine split carries:
/// relabelling such rows would invent provenance.
pub const LEGACY_ENTRY_SOURCE: &str = "agent";

/// A state read from disk, with why the file did not parse when it did not.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedHarnessState {
    pub state: HarnessState,
    /// `None` for a missing or well-formed file.
    pub load_error: Option<String>,
}

pub(crate) fn kind_of(name: &str) -> Option<RefinementKind> {
    Some(match name {
        "prompt" => RefinementKind::Prompt,
        "memory" => RefinementKind::Memory,
        "skill" => RefinementKind::Skill,
        "subagent" => RefinementKind::Subagent,
        "factory" => RefinementKind::Factory,
        _ => return None,
    })
}

fn scope_of(value: Option<&Value>) -> Option<HarnessScope> {
    match value.and_then(Value::as_str) {
        Some("local") => Some(HarnessScope::Local),
        Some("global") => Some(HarnessScope::Global),
        _ => None,
    }
}

fn record(value: Option<&Value>) -> Map<String, Value> {
    value
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

/// Python `int(value)` for the version field, falling back to 1.
fn version_of(value: Option<&Value>) -> u64 {
    match value {
        Some(Value::Bool(flag)) => u64::from(*flag),
        Some(Value::Number(number)) => number.as_u64().unwrap_or(1),
        Some(Value::String(text)) => pyfmt::strip(text).parse::<u64>().unwrap_or(1),
        None | Some(Value::Null | Value::Array(_) | Value::Object(_)) => 1,
    }
}

fn text_or(value: Option<&Value>, fallback: &str) -> String {
    value
        .and_then(Value::as_str)
        .map_or_else(|| fallback.to_string(), str::to_string)
}

fn parse_entry(
    id: &str,
    kind: RefinementKind,
    raw: &Map<String, Value>,
    scope: HarnessScope,
) -> Option<HarnessEntry> {
    let title = raw.get("title")?.as_str()?.to_string();
    let content = raw.get("content")?.as_str()?.to_string();
    Some(HarnessEntry {
        id: id.to_string(),
        kind,
        title,
        content,
        path: text_or(raw.get("path"), "general"),
        scope: Some(scope_of(raw.get("scope")).unwrap_or(scope)),
        reference: record(raw.get("reference")),
        arguments: record(raw.get("arguments")),
        metadata: record(raw.get("metadata")),
        source: text_or(raw.get("source"), LEGACY_ENTRY_SOURCE),
        created_at: text_or(raw.get("created_at"), ""),
        updated_at: text_or(raw.get("updated_at"), ""),
        version: version_of(raw.get("version")),
        extensions: raw
            .iter()
            .filter(|(key, _)| !MODELLED_ENTRY_KEYS.contains(&key.as_str()))
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
    })
}

fn parse_event(raw: &Map<String, Value>) -> Option<HarnessRefinementEvent> {
    let id = raw.get("id")?.as_str()?.to_string();
    let trigger = raw.get("trigger")?.as_str()?.to_string();
    let changes = match raw.get("changes")? {
        Value::String(change) => vec![change.clone()],
        Value::Array(changes) => changes.iter().map(pyfmt::str_of).collect(),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Object(_) => return None,
    };
    let text = |key: &str| raw.get(key).map(pyfmt::str_of).unwrap_or_default();
    Some(HarnessRefinementEvent {
        id,
        trigger,
        changes,
        evidence: text("evidence"),
        outcome: text("outcome"),
        created_at: text("created_at"),
        reason: raw
            .get("reason")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// Read a parsed harness document leniently (see the module docs); a
/// value that is not a JSON object reads as the empty state.
///
/// # Panics
///
/// Never: the empty state pre-populates every kind map.
#[must_use]
pub fn parse_harness_document(document: &Value, scope: HarnessScope) -> HarnessState {
    let mut state = empty_harness_state();
    let Some(document) = document.as_object() else {
        return state;
    };
    state.schema = document.get("schema").and_then(Value::as_u64).unwrap_or(1);
    if let Some(entries) = document.get("entries").and_then(Value::as_object) {
        for name in REFINEMENT_KINDS {
            let Some(kind) = kind_of(name) else { continue };
            let Some(records) = entries.get(name).and_then(Value::as_object) else {
                continue;
            };
            let bucket = state.entries.get_mut(&kind).expect("every kind is present");
            for (id, raw) in records {
                if let Some(entry) = raw
                    .as_object()
                    .and_then(|raw| parse_entry(id, kind, raw, scope))
                {
                    bucket.insert(id.clone(), entry);
                }
            }
        }
    }
    if let Some(events) = document.get("refinements").and_then(Value::as_array) {
        state.refinements = events
            .iter()
            .filter_map(Value::as_object)
            .filter_map(parse_event)
            .collect();
    }
    state.extensions = document
        .iter()
        .filter(|(key, _)| !matches!(key.as_str(), "schema" | "entries" | "refinements"))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    state
}

/// The Python `_type_name` of a top-level JSON value.
fn top_level_type(value: &Value) -> &'static str {
    match value {
        Value::Array(_) => "a list",
        Value::String(text) if text.is_empty() => "an empty string",
        other => pyfmt::type_name(other),
    }
}

/// Why `raw` is not a harness document, or `None` when it is one.
fn document_error(raw: &str) -> (Value, Option<String>) {
    match serde_json::from_str::<Value>(raw) {
        Ok(value) if value.is_object() => (value, None),
        Ok(value) => {
            let error = format!(
                "top-level JSON is {}, not an object",
                top_level_type(&value)
            );
            (Value::Null, Some(error))
        }
        Err(error) => (Value::Null, Some(format!("JSONDecodeError: {error}"))),
    }
}

/// Read the state file at `path` (see the module docs).
#[must_use]
pub fn read_harness_state_file(path: &Path, scope: HarnessScope) -> LoadedHarnessState {
    let raw = match std::fs::read(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return LoadedHarnessState {
                state: empty_harness_state(),
                load_error: None,
            };
        }
        Err(error) => {
            return LoadedHarnessState {
                state: empty_harness_state(),
                load_error: Some(format!("OSError: {error}")),
            };
        }
    };
    let (document, load_error) = match String::from_utf8(raw) {
        Ok(raw) => document_error(&raw),
        Err(error) => (Value::Null, Some(format!("UnicodeDecodeError: {error}"))),
    };
    LoadedHarnessState {
        state: parse_harness_document(&document, scope),
        load_error,
    }
}

/// Whether a write syncs the new file to disk before the rename.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteDurability {
    /// The TS default (`writeFileAtomicSync` without `fsync`): a hard crash
    /// leaves the previous file or the new one.
    NoSync,
    /// fsync before the rename: the kernel writer's durability.
    Sync,
}

/// The file a write replaces: the target of a symlinked state file, so an
/// alias keeps pointing at the store.
fn write_target(path: &Path) -> PathBuf {
    if let Ok(real) = std::fs::canonicalize(path) {
        return real;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => std::fs::canonicalize(parent)
            .map_or_else(|_| path.to_path_buf(), |parent| parent.join(name)),
        _ => path.to_path_buf(),
    }
}

/// Copy a state file that does not parse aside, so the write that follows
/// never destroys the only copy of its entries.
fn back_up_unparsed(path: &Path) -> std::io::Result<()> {
    let raw = match std::fs::read(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let parses = String::from_utf8(raw).is_ok_and(|raw| document_error(&raw).1.is_none());
    if parses {
        return Ok(());
    }
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let backup = path.with_file_name(format!(
        "{name}.corrupt-{}-{}",
        pyfmt::compact_stamp_now(),
        std::process::id()
    ));
    std::fs::copy(path, backup).map(|_| ())
}

#[cfg(unix)]
fn existing_mode(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .ok()
        .map(|metadata| metadata.permissions().mode() & 0o7777)
}

fn write_temp(
    temp: &Path,
    target: &Path,
    content: &str,
    durability: WriteDurability,
) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut open = std::fs::OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    let mode = existing_mode(target);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // Never looser than the destination; a new file is owner-only
        // (within the umask).
        open.mode(mode.unwrap_or(0o600));
    }
    #[cfg(not(unix))]
    let _ = target;
    let mut file = open.open(temp)?;
    file.write_all(content.as_bytes())?;
    if durability == WriteDurability::Sync {
        file.sync_all()?;
    }
    #[cfg(unix)]
    if let Some(mode) = mode {
        // The umask narrowed the create; the destination's bits win.
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(temp, std::fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

/// Write `state` to the state file at `path` (see the module docs).
///
/// # Errors
///
/// The directory cannot be created, an unparsed file cannot be backed up,
/// or the temp write or the rename fails (the previous file is then
/// untouched and no temp file is left behind).
pub(crate) fn write_harness_state_file(
    path: &Path,
    state: &HarnessState,
    durability: WriteDurability,
) -> anyhow::Result<PathBuf> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    back_up_unparsed(path)?;
    let content = format!("{}\n", serde_json::to_string_pretty(state)?);
    let target = write_target(path);
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temp = target.with_file_name(format!(
        "{name}.{}.{}.tmp",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let written = write_temp(&temp, &target, &content, durability)
        .and_then(|()| crate::platform::rename_onto(&temp, &target));
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    written?;
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_store() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("harness_state.json");
        (dir, path)
    }

    /// The kernel's lenient read (`test_load_ignores_unknown_json_keys`):
    /// malformed optional fields default, the key is the id, the bucket is
    /// the kind, an entry without text content and an event without changes
    /// are skipped, and unknown entry keys ride along.
    #[test]
    fn reads_malformed_fields_leniently() {
        let document = json!({
            "schema": 1,
            "entries": {"memory": {
                "known": {
                    "id": "mismatched", "kind": "skill", "title": "Known memory",
                    "content": "Loaded despite extra keys.", "path": 123, "source": null,
                    "version": "2", "metadata": "not a dict", "unexpected": true
                },
                "missing_content": {"title": "Missing content"}
            }},
            "refinements": [
                {"id": "refine_extra", "trigger": "extra keys", "changes": [1, "loaded"], "ignored": "value"},
                {"id": "refine_missing_changes", "trigger": "missing changes"}
            ]
        });
        let state = parse_harness_document(&document, HarnessScope::Local);
        let memories = &state.entries[&RefinementKind::Memory];
        assert_eq!(
            memories.keys().collect::<Vec<_>>(),
            vec!["known"],
            "the entry without content is skipped"
        );
        let mut extensions = Map::new();
        extensions.insert("unexpected".to_string(), json!(true));
        assert_eq!(
            memories["known"],
            HarnessEntry {
                id: "known".to_string(),
                kind: RefinementKind::Memory,
                title: "Known memory".to_string(),
                content: "Loaded despite extra keys.".to_string(),
                path: "general".to_string(),
                scope: Some(HarnessScope::Local),
                reference: Map::new(),
                arguments: Map::new(),
                metadata: Map::new(),
                source: LEGACY_ENTRY_SOURCE.to_string(),
                created_at: String::new(),
                updated_at: String::new(),
                version: 2,
                extensions,
            }
        );
        assert_eq!(
            state.refinements,
            vec![HarnessRefinementEvent {
                id: "refine_extra".to_string(),
                trigger: "extra keys".to_string(),
                changes: vec!["1".to_string(), "loaded".to_string()],
                evidence: String::new(),
                outcome: String::new(),
                created_at: String::new(),
                reason: None,
            }]
        );
    }

    #[test]
    fn non_object_and_corrupt_files_read_empty_with_a_reason() {
        let (_dir, path) = temp_store();
        for (payload, reason) in [
            ("null", "top-level JSON is NoneType, not an object"),
            ("[]", "top-level JSON is a list, not an object"),
            ("\"\"", "top-level JSON is an empty string, not an object"),
            ("123", "top-level JSON is int, not an object"),
        ] {
            std::fs::write(&path, payload).unwrap();
            assert_eq!(
                read_harness_state_file(&path, HarnessScope::Local),
                LoadedHarnessState {
                    state: empty_harness_state(),
                    load_error: Some(reason.to_string()),
                }
            );
        }
        std::fs::write(&path, "not json").unwrap();
        let loaded = read_harness_state_file(&path, HarnessScope::Local);
        assert_eq!(loaded.state, empty_harness_state());
        assert!(loaded.load_error.unwrap().starts_with("JSONDecodeError: "));
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            read_harness_state_file(&path, HarnessScope::Local),
            LoadedHarnessState {
                state: empty_harness_state(),
                load_error: None,
            }
        );
    }

    /// Ported from the kernel's `test_save_failure_preserves_previous_state_on_disk`
    /// and `test_failed_write_leaves_original_intact` (which tore Python's
    /// `json.dump`): a write that cannot land leaves the previous file
    /// byte-identical and no temp file behind.
    #[cfg(unix)]
    #[test]
    fn a_failed_write_leaves_the_previous_file_and_no_temp() {
        use std::os::unix::fs::PermissionsExt as _;
        let (dir, path) = temp_store();
        std::fs::write(&path, "{\"schema\": 1}").unwrap();
        // A read-only directory refuses the temp file.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        let result = write_harness_state_file(&path, &empty_harness_state(), WriteDurability::Sync);
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        if nix::unistd::geteuid().is_root() {
            // root ignores the directory mode; nothing to observe.
            return;
        }
        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"schema\": 1}");
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["harness_state.json".to_string()]);
    }

    /// Ported from the kernel's `test_save_temp_file_is_never_looser_than_the_destination`
    /// (which observed Python's `os.open`): the replacement keeps the
    /// destination's mode, and a new file is owner-only.
    #[cfg(unix)]
    #[test]
    fn writes_keep_the_destination_mode() {
        use std::os::unix::fs::PermissionsExt as _;
        let (_dir, path) = temp_store();
        write_harness_state_file(&path, &empty_harness_state(), WriteDurability::NoSync).unwrap();
        assert_eq!(existing_mode(&path), Some(0o600));
        for mode in [0o640, 0o666] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
            write_harness_state_file(&path, &empty_harness_state(), WriteDurability::Sync).unwrap();
            assert_eq!(existing_mode(&path), Some(mode));
        }
    }

    #[cfg(unix)]
    #[test]
    fn writes_land_in_the_file_a_symlink_names() {
        let (dir, path) = temp_store();
        let real = dir.path().join("real_state.json");
        std::fs::write(&real, "{}").unwrap();
        std::os::unix::fs::symlink(&real, &path).unwrap();
        write_harness_state_file(&path, &empty_harness_state(), WriteDurability::NoSync).unwrap();
        assert!(std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            std::fs::read_to_string(&real).unwrap(),
            format!(
                "{}\n",
                serde_json::to_string_pretty(&empty_harness_state()).unwrap()
            )
        );
    }

    /// #961: a corrupt file is copied aside once, before the first write
    /// replaces it; a write over a valid file makes no backup.
    #[test]
    fn a_corrupt_file_is_backed_up_before_it_is_replaced() {
        let (dir, path) = temp_store();
        let corrupt = r#"{"schema": 1, "entries": {"memory": {"kept": {"title": "T""#;
        std::fs::write(&path, corrupt).unwrap();
        write_harness_state_file(&path, &empty_harness_state(), WriteDurability::NoSync).unwrap();
        write_harness_state_file(&path, &empty_harness_state(), WriteDurability::NoSync).unwrap();
        let backups: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|entry| {
                entry
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("harness_state.json.corrupt-")
            })
            .map(|entry| std::fs::read_to_string(entry).unwrap())
            .collect();
        assert_eq!(backups, vec![corrupt.to_string()]);
    }
}
