//! One-shot backfill of `HarnessEntry::targets_fingerprints` for
//! memories admitted before the RAVO failure-fingerprint plumbing
//! landed on this account.
//!
//! The fix the refinement overhaul shipped on 2026-11-04 made the
//! planner cite `failure:<fp>` ids in each edit's own
//! `targetsFingerprints`. Memories admitted BEFORE that fix (94 of 96
//! in this user's live state, measured 2026-11-07) carry no
//! fingerprints, so `FeedbackTap`'s cert-stream extractor has nothing
//! to cross-reference: the extractor emits Helped signals for
//! memories whose `targetsFingerprints` contains a fingerprint the
//! rejection cites, and an empty tfs field means the memory is
//! invisible to the learning loop.
//!
//! This module reconstructs the join from
//! `refinement_history.jsonl` and `local_refinement_history.jsonl`:
//! for each successfully applied memory create/update edit, union
//! the parent record's `ravo.missedCriteria` failure fingerprints
//! into that memory's `targetsFingerprints`. The operation is
//! idempotent (set-union) and guarded by a sentinel file so it only
//! runs once per account; the sentinel carries the date for audit.
//!
//! Call `backfill_once` from the daemon's cold-boot path. It is
//! best-effort: every failure mode logs a warning and returns Ok(),
//! so a missing file, malformed record, or permission error never
//! blocks startup.

use std::fs;
use std::path::Path;

use serde::Deserialize;
use serde_json::{Map, Value};

/// Mirror of `pa_ledger::HARNESS_STATE_FILE_NAME` — kept inline so
/// this module stays in pa-core (pa-ledger depends on pa-core, not
/// the other way around).
const HARNESS_STATE_FILE_NAME: &str = "harness_state.json";

/// The sentinel file name. Its presence (regardless of contents)
/// disables the backfill. Writing the current iso-8601 date in the
/// file lets a human re-run the backfill by deleting it.
const SENTINEL_NAME: &str = "targets_fingerprints_backfill.done";

/// A `RefinementResult`-shaped record we project just enough of to
/// extract (edit_id, kind, action, applied) tuples plus the parent
/// `ravo.missedCriteria`. Every other field is ignored (serde
/// `default`), so a schema drift elsewhere in the record doesn't
/// block the backfill.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRecord {
    #[serde(default)]
    ravo: RawRavo,
    #[serde(default)]
    applied_edits: Vec<RawEdit>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRavo {
    #[serde(default)]
    missed_criteria: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawEdit {
    #[serde(default)]
    id: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    applied: bool,
}

/// Run the backfill once. `harness_dir` is the shared harness dir
/// (`~/.prime/agent/harness/`); `state_path` is the canonical
/// `harness_state.json` inside it. Idempotent: subsequent calls after
/// the sentinel exists are no-ops.
///
/// # Errors
///
/// Returns an error only when the sentinel-write at the end fails —
/// every other failure is logged and the function continues as a
/// best-effort pass. The caller may ignore the error.
pub fn backfill_once(harness_dir: &Path) -> anyhow::Result<()> {
    let sentinel = harness_dir.join(SENTINEL_NAME);
    if sentinel.exists() {
        return Ok(());
    }
    let state_path = harness_dir.join(HARNESS_STATE_FILE_NAME);
    let joins = collect_fingerprints(harness_dir);
    if joins.is_empty() {
        // Nothing to join — write the sentinel so we don't re-walk
        // every startup on an empty account.
        write_sentinel(&sentinel)?;
        return Ok(());
    }
    let Ok(bytes) = fs::read(&state_path) else {
        tracing::warn!(
            target: "pa_core::refinement::backfill",
            path = %state_path.display(),
            "harness_state.json unreadable; skipping backfill"
        );
        return Ok(());
    };
    let Ok(mut root) = serde_json::from_slice::<Value>(&bytes) else {
        tracing::warn!(
            target: "pa_core::refinement::backfill",
            "harness_state.json unparsable; skipping backfill"
        );
        return Ok(());
    };
    let Some(memories) = root
        .get_mut("entries")
        .and_then(|e| e.get_mut("memory"))
        .and_then(Value::as_object_mut)
    else {
        write_sentinel(&sentinel)?;
        return Ok(());
    };

    let mut updated = 0usize;
    let mut fps_added = 0usize;
    for (id, entry) in memories.iter_mut() {
        let Some(wanted) = joins.get(id.as_str()) else {
            continue;
        };
        if wanted.is_empty() {
            continue;
        }
        let Some(entry) = entry.as_object_mut() else {
            continue;
        };
        let current: Vec<String> = entry
            .get("targetsFingerprints")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let mut merged = current.clone();
        let before = merged.len();
        for fp in wanted {
            if !merged.contains(fp) {
                merged.push(fp.clone());
            }
        }
        if merged.len() == before {
            continue;
        }
        fps_added += merged.len() - before;
        entry.insert(
            "targetsFingerprints".to_string(),
            Value::Array(merged.into_iter().map(Value::String).collect()),
        );
        updated += 1;
    }

    if updated == 0 {
        write_sentinel(&sentinel)?;
        return Ok(());
    }

    // Atomic rewrite of harness_state.json: write to .tmp, rename.
    let tmp = state_path.with_extension("json.backfill-tmp");
    let encoded = serde_json::to_vec_pretty(&root)?;
    fs::write(&tmp, &encoded)?;
    fs::rename(&tmp, &state_path)?;

    tracing::info!(
        target: "pa_core::refinement::backfill",
        memories_updated = updated,
        fingerprints_added = fps_added,
        "backfilled targetsFingerprints on historical memories"
    );
    write_sentinel(&sentinel)
}

/// Build `memory_id -> {fingerprint ...}` from both history files.
/// Non-applied edits contribute nothing; non-memory edits contribute
/// nothing. A memory_id that appears in multiple records gets the
/// union of every parent record's failure fingerprints.
fn collect_fingerprints(harness_dir: &Path) -> std::collections::HashMap<String, Vec<String>> {
    let mut joins: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for file_name in ["refinement_history.jsonl", "local_refinement_history.jsonl"] {
        let path = harness_dir.join(file_name);
        let Ok(body) = fs::read(&path) else {
            continue;
        };
        for line in body.split(|&b| b == b'\n') {
            if line.is_empty() {
                continue;
            }
            let Ok(rec) = serde_json::from_slice::<RawRecord>(line) else {
                continue;
            };
            let fps: Vec<String> = rec
                .ravo
                .missed_criteria
                .iter()
                .filter_map(|c| c.strip_prefix("failure:").map(str::to_string))
                .collect();
            if fps.is_empty() {
                continue;
            }
            for edit in &rec.applied_edits {
                if !edit.applied {
                    continue;
                }
                if edit.kind.as_deref() != Some("memory") {
                    continue;
                }
                if !matches!(edit.action.as_deref(), Some("create" | "update")) {
                    continue;
                }
                let entry = joins.entry(edit.id.clone()).or_default();
                for fp in &fps {
                    if !entry.contains(fp) {
                        entry.push(fp.clone());
                    }
                }
            }
        }
    }
    joins
}

fn write_sentinel(sentinel: &Path) -> anyhow::Result<()> {
    if let Some(parent) = sentinel.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut body = Map::new();
    body.insert(
        "completedAt".to_string(),
        Value::String(now_iso_best_effort()),
    );
    body.insert(
        "note".to_string(),
        Value::String(
            "targets_fingerprints backfill done. Delete to re-run on the next cold boot."
                .to_string(),
        ),
    );
    fs::write(sentinel, serde_json::to_vec_pretty(&body)?)?;
    Ok(())
}

/// Format current UTC as an iso-8601 millisecond string. Uses
/// `std::time::SystemTime` + the `jiff` wall-clock conversion
/// `chrono` would do, written directly so pa-core doesn't take a
/// new dependency: a sentinel timestamp is cosmetic (it's only read
/// by humans checking "when did the backfill last run"), so a
/// seconds-precision formatting is enough.
fn now_iso_best_effort() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let millis = now.subsec_millis();
    // Convert unix seconds to YYYY-MM-DDTHH:MM:SS.mmmZ via the
    // civil-time arithmetic every ISO formatter embeds; we do it by
    // hand here to avoid pulling `chrono`/`jiff` for one cosmetic
    // line. Leap-second correctness isn't a goal for a sentinel.
    let days = secs / 86_400;
    let secs_in_day = secs % 86_400;
    let hour = secs_in_day / 3600;
    let min = (secs_in_day % 3600) / 60;
    let sec = secs_in_day % 60;
    let (year, month, day) = civil_from_days(days as i64);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}.{millis:03}Z")
}

/// The civil calendar conversion from Howard Hinnant's "date"
/// algorithms — unix-epoch days to (year, month, day). Correct for
/// all dates after 1970.
#[allow(clippy::many_single_char_names)]
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 {
        z / 146_097
    } else {
        (z - 146_096) / 146_097
    };
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = y + i64::from(m <= 2);
    (year, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn write(path: &Path, body: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, body).unwrap();
    }

    fn state_with_memories(memories: &[(&str, Vec<&str>)]) -> Value {
        let entries: Map<String, Value> = memories
            .iter()
            .map(|(id, tfs)| {
                (
                    (*id).to_string(),
                    json!({
                        "id": id,
                        "kind": "memory",
                        "title": "t",
                        "content": "c",
                        "path": "",
                        "reference": {},
                        "arguments": {},
                        "metadata": {},
                        "source": "refine",
                        "created_at": "2026-01-01T00:00:00.000Z",
                        "updated_at": "2026-01-01T00:00:00.000Z",
                        "version": 1,
                        "targetsFingerprints": tfs,
                    }),
                )
            })
            .collect();
        json!({ "schema": 3, "entries": { "memory": entries } })
    }

    #[test]
    fn backfill_merges_fingerprints_from_global_history() {
        let dir = tempfile::tempdir().unwrap();
        let harness = dir.path();
        fs::write(
            harness.join(HARNESS_STATE_FILE_NAME),
            serde_json::to_vec(&state_with_memories(&[("mem_a", vec![])])).unwrap(),
        )
        .unwrap();
        write(
            &harness.join("refinement_history.jsonl"),
            r#"{"id":"r1","ravo":{"missedCriteria":["failure:abc","failure:def","scope"]},"appliedEdits":[{"id":"mem_a","kind":"memory","action":"create","applied":true}]}
"#,
        );
        backfill_once(harness).unwrap();
        let state: Value =
            serde_json::from_slice(&fs::read(harness.join(HARNESS_STATE_FILE_NAME)).unwrap())
                .unwrap();
        let tfs = state["entries"]["memory"]["mem_a"]["targetsFingerprints"]
            .as_array()
            .unwrap();
        let fps: Vec<&str> = tfs.iter().filter_map(Value::as_str).collect();
        assert_eq!(fps, vec!["abc", "def"], "non-failure causes stripped");
        assert!(
            harness.join(SENTINEL_NAME).exists(),
            "sentinel written after a successful backfill"
        );
    }

    #[test]
    fn backfill_merges_from_local_history_too() {
        let dir = tempfile::tempdir().unwrap();
        let harness = dir.path();
        fs::write(
            harness.join(HARNESS_STATE_FILE_NAME),
            serde_json::to_vec(&state_with_memories(&[("mem_b", vec!["already"])])).unwrap(),
        )
        .unwrap();
        write(
            &harness.join("local_refinement_history.jsonl"),
            r#"{"id":"r1","ravo":{"missedCriteria":["failure:cafe"]},"appliedEdits":[{"id":"mem_b","kind":"memory","action":"update","applied":true}]}
"#,
        );
        backfill_once(harness).unwrap();
        let state: Value =
            serde_json::from_slice(&fs::read(harness.join(HARNESS_STATE_FILE_NAME)).unwrap())
                .unwrap();
        let fps: Vec<&str> = state["entries"]["memory"]["mem_b"]["targetsFingerprints"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert!(fps.contains(&"already"), "existing fp kept");
        assert!(fps.contains(&"cafe"), "local fp merged");
    }

    #[test]
    fn backfill_is_idempotent_after_sentinel_lands() {
        let dir = tempfile::tempdir().unwrap();
        let harness = dir.path();
        let initial = state_with_memories(&[("mem_c", vec!["x"])]);
        fs::write(
            harness.join(HARNESS_STATE_FILE_NAME),
            serde_json::to_vec(&initial).unwrap(),
        )
        .unwrap();
        write(
            &harness.join("refinement_history.jsonl"),
            r#"{"id":"r1","ravo":{"missedCriteria":["failure:y"]},"appliedEdits":[{"id":"mem_c","kind":"memory","action":"create","applied":true}]}
"#,
        );
        backfill_once(harness).unwrap();
        // Delete one fp and re-run: since the sentinel exists, the
        // backfill must be a no-op and the state stays as the test
        // left it.
        let after_first: Value =
            serde_json::from_slice(&fs::read(harness.join(HARNESS_STATE_FILE_NAME)).unwrap())
                .unwrap();
        // Hand-edit the state to drop a fp.
        let mut tampered = after_first.clone();
        tampered["entries"]["memory"]["mem_c"]["targetsFingerprints"] = json!(["only_one"]);
        fs::write(
            harness.join(HARNESS_STATE_FILE_NAME),
            serde_json::to_vec(&tampered).unwrap(),
        )
        .unwrap();
        backfill_once(harness).unwrap();
        let after_second: Value =
            serde_json::from_slice(&fs::read(harness.join(HARNESS_STATE_FILE_NAME)).unwrap())
                .unwrap();
        let fps: Vec<&str> = after_second["entries"]["memory"]["mem_c"]["targetsFingerprints"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_str)
            .collect();
        assert_eq!(fps, vec!["only_one"], "second run must not touch state");
    }

    #[test]
    fn backfill_skips_unapplied_and_non_memory_edits() {
        let dir = tempfile::tempdir().unwrap();
        let harness = dir.path();
        fs::write(
            harness.join(HARNESS_STATE_FILE_NAME),
            serde_json::to_vec(&state_with_memories(&[("mem_d", vec![])])).unwrap(),
        )
        .unwrap();
        write(
            &harness.join("refinement_history.jsonl"),
            r#"{"id":"r1","ravo":{"missedCriteria":["failure:z"]},"appliedEdits":[{"id":"mem_d","kind":"memory","action":"create","applied":false}]}
{"id":"r2","ravo":{"missedCriteria":["failure:q"]},"appliedEdits":[{"id":"mem_d","kind":"skill","action":"create","applied":true}]}
"#,
        );
        backfill_once(harness).unwrap();
        let state: Value =
            serde_json::from_slice(&fs::read(harness.join(HARNESS_STATE_FILE_NAME)).unwrap())
                .unwrap();
        let tfs = state["entries"]["memory"]["mem_d"]["targetsFingerprints"]
            .as_array()
            .unwrap();
        assert!(tfs.is_empty(), "unapplied + non-memory edits both skipped");
    }
}
