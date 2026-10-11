//! One-time move of a global harness store the Rust daemon misplaced.
//!
//! The canonical global store is `<agentDir>/harness/` (TS v0.9.8
//! `getGlobalHarnessStateDir()`, the kernel's `rlm.harness`, print mode, and
//! the system-prompt digest). Daemon builds before this fix passed the agent
//! dir itself as the global harness dir, so a daemon-side global `/refine`,
//! `/harness` or `refine.preview` read and wrote `<agentDir>/harness_state.json`
//! and `<agentDir>/refinement_history.jsonl` instead. Those files are merged into
//! the canonical store and then moved aside (never deleted).

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde_json::{Map, Value};

use super::entries::lock_harness_state;
use super::{
    empty_harness_state,
    get_global_harness_state_dir,
    get_harness_state_path,
    get_refinement_history_path,
};

/// What one migration pass moved into the canonical store.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MisplacedHarnessMigration {
    /// Entries copied into the canonical store.
    pub entries: usize,
    /// Of those, entries whose id the canonical store already held with other
    /// content, kept under a new id: `(kind, old id, new id)`.
    pub renamed: Vec<(String, String, String)>,
    /// Refinement events copied into the canonical `refinements` list.
    pub refinements: usize,
    /// Refinement-history records appended to the canonical history.
    pub history_records: usize,
    /// Where each misplaced file was moved aside, in migration order.
    pub backups: Vec<PathBuf>,
}

/// Merge a misplaced `<agentDir>/harness_state.json` and
/// `<agentDir>/refinement_history.jsonl` into `<agentDir>/harness/`, once.
///
/// # Errors
///
/// A store is unreadable or not a JSON object, a lock stayed held, or a write failed.
pub fn migrate_misplaced_global_harness_state(
    agent_dir: &Path,
) -> anyhow::Result<MisplacedHarnessMigration> {
    let misplaced_state = get_harness_state_path(agent_dir);
    let misplaced_history = get_refinement_history_path(agent_dir);
    if !misplaced_state.exists() && !misplaced_history.exists() {
        return Ok(MisplacedHarnessMigration::default());
    }
    let canonical = get_global_harness_state_dir(agent_dir);
    std::fs::create_dir_all(&canonical)
        .with_context(|| format!("creating {}", canonical.display()))?;
    // Both stores' writers serialize on their own `harness_state.json.lock`.
    let _misplaced_lock = lock_harness_state(agent_dir)?;
    let _canonical_lock = lock_harness_state(&canonical)?;
    let mut migration = MisplacedHarnessMigration::default();
    if misplaced_state.exists() {
        merge_state(
            &misplaced_state,
            &get_harness_state_path(&canonical),
            &mut migration,
        )?;
        migration.backups.push(move_aside(&misplaced_state)?);
    }
    if misplaced_history.exists() {
        migration.history_records =
            merge_history(&misplaced_history, &get_refinement_history_path(&canonical))?;
        migration.backups.push(move_aside(&misplaced_history)?);
    }
    Ok(migration)
}

fn read_object(path: &Path) -> anyhow::Result<Map<String, Value>> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    match serde_json::from_str::<Value>(&raw) {
        Ok(Value::Object(object)) => Ok(object),
        Ok(_) => anyhow::bail!("{} is not a JSON object", path.display()),
        Err(error) => anyhow::bail!("{} is not valid JSON: {error}", path.display()),
    }
}

fn object_field<'a>(
    object: &'a mut Map<String, Value>,
    key: &str,
    path: &Path,
) -> anyhow::Result<&'a mut Map<String, Value>> {
    object
        .entry(key)
        .or_insert_with(|| Value::Object(Map::new()))
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("{}: `{key}` is not an object", path.display()))
}

/// The first `<id>-migrated[-n]` free in both the canonical and the
/// misplaced records of one kind.
fn free_id(id: &str, taken: &Map<String, Value>, incoming: &Map<String, Value>) -> String {
    let base = format!("{id}-migrated");
    let mut candidate = base.clone();
    let mut n = 2;
    while taken.contains_key(&candidate) || incoming.contains_key(&candidate) {
        candidate = format!("{base}-{n}");
        n += 1;
    }
    candidate
}

/// Merge at the raw-JSON level, so keys and entries this build does not model
/// survive: a new id is copied, an identical entry is already there, a
/// differing entry under a taken id is kept under a free id. Refinement events
/// append unless an identical event is present; other top-level keys fill in
/// where the canonical store has none (the backup keeps the rest).
fn merge_state(
    misplaced_path: &Path,
    canonical_path: &Path,
    migration: &mut MisplacedHarnessMigration,
) -> anyhow::Result<()> {
    let misplaced = read_object(misplaced_path)?;
    let mut target = if canonical_path.exists() {
        read_object(canonical_path)?
    } else {
        match serde_json::to_value(empty_harness_state())? {
            Value::Object(object) => object,
            _ => unreachable!("a harness state serializes to an object"),
        }
    };
    let schema =
        |object: &Map<String, Value>| object.get("schema").and_then(Value::as_u64).unwrap_or(1);
    let merged_schema = schema(&target).max(schema(&misplaced));
    target.insert("schema".to_string(), Value::from(merged_schema));

    if let Some(Value::Object(kinds)) = misplaced.get("entries") {
        let target_entries = object_field(&mut target, "entries", canonical_path)?;
        for (kind, records) in kinds {
            let Value::Object(records) = records else {
                if !target_entries.contains_key(kind) {
                    target_entries.insert(kind.clone(), records.clone());
                }
                continue;
            };
            let target_kind = object_field(target_entries, kind, canonical_path)?;
            for (id, record) in records {
                match target_kind.get(id) {
                    None => {
                        target_kind.insert(id.clone(), record.clone());
                    }
                    Some(existing) if existing == record => continue,
                    Some(_) => {
                        let new_id = free_id(id, target_kind, records);
                        let mut record = record.clone();
                        if let Some(object) = record.as_object_mut() {
                            object.insert("id".to_string(), Value::String(new_id.clone()));
                        }
                        target_kind.insert(new_id.clone(), record);
                        migration.renamed.push((kind.clone(), id.clone(), new_id));
                    }
                }
                migration.entries += 1;
            }
        }
    }

    if let Some(Value::Array(events)) = misplaced.get("refinements") {
        let target_events = target
            .entry("refinements")
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .ok_or_else(|| {
                anyhow::anyhow!("{}: `refinements` is not a list", canonical_path.display())
            })?;
        for event in events {
            if !target_events.contains(event) {
                target_events.push(event.clone());
                migration.refinements += 1;
            }
        }
    }

    for (key, value) in &misplaced {
        if !matches!(key.as_str(), "schema" | "entries" | "refinements")
            && !target.contains_key(key)
        {
            target.insert(key.clone(), value.clone());
        }
    }

    let content = format!(
        "{}\n",
        serde_json::to_string_pretty(&Value::Object(target))?
    );
    crate::settings::storage::atomic_write(canonical_path, &content)
        .with_context(|| format!("writing {}", canonical_path.display()))?;
    Ok(())
}

/// Append every misplaced history line the canonical history does not already
/// hold verbatim. Appends (the history's own writers append unlocked), so a
/// concurrent record is never lost to a rewrite.
fn merge_history(misplaced_path: &Path, canonical_path: &Path) -> anyhow::Result<usize> {
    use std::io::Write as _;
    let misplaced = std::fs::read_to_string(misplaced_path)
        .with_context(|| format!("reading {}", misplaced_path.display()))?;
    let existing = match std::fs::read_to_string(canonical_path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", canonical_path.display()));
        }
    };
    let mut seen: std::collections::HashSet<&str> = existing.lines().collect();
    let mut appended = String::new();
    let mut count = 0;
    for line in misplaced.lines() {
        if line.trim().is_empty() || !seen.insert(line) {
            continue;
        }
        appended.push_str(line);
        appended.push('\n');
        count += 1;
    }
    if count == 0 {
        return Ok(0);
    }
    if !existing.is_empty() && !existing.ends_with('\n') {
        appended.insert(0, '\n');
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(canonical_path)
        .with_context(|| format!("opening {}", canonical_path.display()))?;
    file.write_all(appended.as_bytes())
        .with_context(|| format!("appending to {}", canonical_path.display()))?;
    Ok(count)
}

/// Rename `path` to `<path>.migrated` (then `.migrated.1`, ...): an earlier
/// backup is never overwritten.
fn move_aside(path: &Path) -> anyhow::Result<PathBuf> {
    let base = format!("{}.migrated", path.display());
    let mut backup = PathBuf::from(&base);
    let mut n = 1;
    while backup.exists() {
        backup = PathBuf::from(format!("{base}.{n}"));
        n += 1;
    }
    std::fs::rename(path, &backup)
        .with_context(|| format!("moving {} to {}", path.display(), backup.display()))?;
    Ok(backup)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn entry(id: &str, content: &str) -> Value {
        json!({
            "id": id, "kind": "prompt", "title": id, "content": content, "path": "p",
            "scope": "global", "reference": {}, "arguments": {}, "metadata": {},
            "source": "refine", "created_at": "t0", "updated_at": "t0", "version": 1
        })
    }

    fn write_json(path: &Path, value: &Value) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            format!("{}\n", serde_json::to_string_pretty(value).unwrap()),
        )
        .unwrap();
    }

    fn read_json(path: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn nothing_misplaced_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let migration = migrate_misplaced_global_harness_state(dir.path()).unwrap();
        assert_eq!(migration, MisplacedHarnessMigration::default());
        assert!(!get_global_harness_state_dir(dir.path()).exists());
    }

    #[test]
    fn misplaced_state_and_history_merge_into_the_canonical_store_without_loss() {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path();
        let canonical = get_global_harness_state_dir(agent);
        write_json(
            &get_harness_state_path(&canonical),
            &json!({
                "schema": 1,
                "entries": {
                    "prompt": { "style": entry("style", "terse"), "shared": entry("shared", "same") },
                    "memory": {}, "skill": {}, "subagent": {}, "factory": {}
                },
                "refinements": [ { "id": "r1", "trigger": "t", "changes": [], "evidence": "", "outcome": "ok", "created_at": "t1" } ],
                "failures": { "kept": true }
            }),
        );
        std::fs::write(
            get_refinement_history_path(&canonical),
            "{\"id\":\"h1\",\"summary\":\"canonical\"}\n",
        )
        .unwrap();
        let misplaced_state = json!({
            "schema": 2,
            "entries": {
                "prompt": {
                    "style": entry("style", "verbose"),
                    "shared": entry("shared", "same"),
                    "fresh": entry("fresh", "new")
                },
                "factory": { "f": entry("f", "factory") }
            },
            "refinements": [
                { "id": "r1", "trigger": "t", "changes": [], "evidence": "", "outcome": "ok", "created_at": "t1" },
                { "id": "r2", "trigger": "t", "changes": [], "evidence": "", "outcome": "ok", "created_at": "t2" }
            ],
            "failures": { "kept": false },
            "ravo": { "from": "daemon" }
        });
        write_json(&get_harness_state_path(agent), &misplaced_state);
        let misplaced_state_bytes = std::fs::read(get_harness_state_path(agent)).unwrap();
        // An exact copy of a canonical record is the only line not appended.
        let misplaced_history = "{\"id\":\"h1\",\"summary\":\"canonical\"}\n{\"id\":\"h1\",\"summary\":\"daemon edit\"}\nnot json\n";
        std::fs::write(get_refinement_history_path(agent), misplaced_history).unwrap();

        let migration = migrate_misplaced_global_harness_state(agent).unwrap();

        assert_eq!(
            migration,
            MisplacedHarnessMigration {
                entries: 3,
                renamed: vec![(
                    "prompt".to_string(),
                    "style".to_string(),
                    "style-migrated".to_string()
                )],
                refinements: 1,
                history_records: 2,
                backups: vec![
                    agent.join("harness_state.json.migrated"),
                    agent.join("refinement_history.jsonl.migrated"),
                ],
            }
        );
        let mut renamed = entry("style", "verbose");
        renamed["id"] = json!("style-migrated");
        assert_eq!(
            read_json(&get_harness_state_path(&canonical)),
            json!({
                "schema": 2,
                "entries": {
                    "prompt": {
                        "style": entry("style", "terse"),
                        "shared": entry("shared", "same"),
                        "style-migrated": renamed,
                        "fresh": entry("fresh", "new")
                    },
                    "memory": {}, "skill": {}, "subagent": {},
                    "factory": { "f": entry("f", "factory") }
                },
                "refinements": [
                    { "id": "r1", "trigger": "t", "changes": [], "evidence": "", "outcome": "ok", "created_at": "t1" },
                    { "id": "r2", "trigger": "t", "changes": [], "evidence": "", "outcome": "ok", "created_at": "t2" }
                ],
                "failures": { "kept": true },
                "ravo": { "from": "daemon" }
            })
        );
        assert_eq!(
            std::fs::read_to_string(get_refinement_history_path(&canonical)).unwrap(),
            "{\"id\":\"h1\",\"summary\":\"canonical\"}\n{\"id\":\"h1\",\"summary\":\"daemon edit\"}\nnot json\n"
        );
        // The misplaced files are moved aside byte for byte, never deleted.
        assert!(!get_harness_state_path(agent).exists());
        assert!(!get_refinement_history_path(agent).exists());
        assert_eq!(
            std::fs::read(agent.join("harness_state.json.migrated")).unwrap(),
            misplaced_state_bytes
        );
        assert_eq!(
            std::fs::read_to_string(agent.join("refinement_history.jsonl.migrated")).unwrap(),
            misplaced_history
        );

        // Once: a second pass finds nothing to move.
        assert_eq!(
            migrate_misplaced_global_harness_state(agent).unwrap(),
            MisplacedHarnessMigration::default()
        );
    }

    #[test]
    fn a_misplaced_store_without_a_canonical_one_moves_whole() {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path();
        let state = json!({
            "schema": 1,
            "entries": { "subagent": { "a": entry("a", "x") } },
            "refinements": []
        });
        write_json(&get_harness_state_path(agent), &state);

        let migration = migrate_misplaced_global_harness_state(agent).unwrap();

        assert_eq!(
            migration,
            MisplacedHarnessMigration {
                entries: 1,
                renamed: Vec::new(),
                refinements: 0,
                history_records: 0,
                backups: vec![agent.join("harness_state.json.migrated")],
            }
        );
        assert_eq!(
            read_json(&get_harness_state_path(&get_global_harness_state_dir(
                agent
            ))),
            json!({
                "schema": 1,
                "entries": {
                    "prompt": {}, "memory": {}, "skill": {},
                    "subagent": { "a": entry("a", "x") },
                    "factory": {}
                },
                "refinements": []
            })
        );
    }

    #[test]
    fn an_unreadable_misplaced_store_is_left_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path();
        std::fs::write(get_harness_state_path(agent), "{ not json").unwrap();

        let error = migrate_misplaced_global_harness_state(agent).unwrap_err();

        assert!(
            error.to_string().contains("harness_state.json"),
            "the error names the file: {error:#}"
        );
        assert_eq!(
            std::fs::read_to_string(get_harness_state_path(agent)).unwrap(),
            "{ not json"
        );
        assert!(!get_harness_state_path(&get_global_harness_state_dir(agent)).exists());
    }

    #[test]
    fn an_earlier_backup_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let agent = dir.path();
        std::fs::write(agent.join("refinement_history.jsonl.migrated"), "older\n").unwrap();
        std::fs::write(get_refinement_history_path(agent), "{\"id\":\"h9\"}\n").unwrap();

        let migration = migrate_misplaced_global_harness_state(agent).unwrap();

        assert_eq!(
            migration.backups,
            vec![agent.join("refinement_history.jsonl.migrated.1")]
        );
        assert_eq!(
            std::fs::read_to_string(agent.join("refinement_history.jsonl.migrated")).unwrap(),
            "older\n"
        );
    }
}
