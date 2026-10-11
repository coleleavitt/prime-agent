//! The ledger's home on disk: the `failures` key of a harness state file
//! (`<dir>/harness_state.json`), local per session (`<sessionArtifactDir>/harness`)
//! and global per machine (`<agentDir>/harness`).
//!
//! The document is read and written as raw JSON: this crate replaces the
//! keys it is asked to and carries every other key through byte for byte
//! (key order kept), so it never rewrites another producer's data. Writes
//! are `JSON.stringify(state, null, 2) + "\n"` to a temp file renamed over
//! the target, keeping the target's mode (0600 for a new file). The global
//! read-modify-write runs under the file's one lock
//! (`harness_state.json.lock`, [`pa_core::refinement::lock`]: owned,
//! heartbeated, 10 s stale, a 15 s wait per holder), shared with the
//! kernel's harness store, refine and the RAVO commit, so concurrent
//! writers serialize and none loses another's records.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::ledger::{FailureLedger, normalize_failure_ledger};

/// The harness state directory's name under an agent or session artifact dir.
pub const HARNESS_STATE_DIR_NAME: &str = "harness";
/// The harness state file's name.
pub const HARNESS_STATE_FILE_NAME: &str = "harness_state.json";
/// The key the failure ledger lives under.
pub const FAILURES_KEY: &str = "failures";
/// `PRIME_AGENT_GLOBAL_LEDGER=0|off|false|no` keeps the ledger per session.
pub const GLOBAL_FAILURE_LEDGER_ENV: &str = "PRIME_AGENT_GLOBAL_LEDGER";

/// The top-level keys the TS `loadHarnessState` emits, in its order; a key
/// this crate adds lands where a TS save would put it.
const CANONICAL_KEY_ORDER: [&str; 6] = [
    "schema",
    "entries",
    "refinements",
    "ravo",
    FAILURES_KEY,
    "trustWindows",
];

/// Why a harness state write did not land.
#[derive(Debug, thiserror::Error)]
pub enum HarnessStateError {
    /// One holder kept the harness state lock past the wait.
    #[error("could not lock harness state: {0}")]
    Locked(PathBuf),
    /// Reading, serializing, or writing failed.
    #[error("harness state I/O failed at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Whether the failure ledger is kept in the global harness state too
/// (on unless the variable says `0`, `off`, `false` or `no`).
#[must_use]
pub fn global_failure_ledger_enabled(value: Option<&str>) -> bool {
    let value = value.map(|value| crate::js::js_trim(value).to_lowercase());
    !matches!(value.as_deref(), Some("0" | "off" | "false" | "no"))
}

/// [`global_failure_ledger_enabled`] read from the process environment.
#[must_use]
pub fn global_failure_ledger_enabled_from_env() -> bool {
    global_failure_ledger_enabled(std::env::var(GLOBAL_FAILURE_LEDGER_ENV).ok().as_deref())
}

/// `<agentDir>/harness`.
#[must_use]
pub fn global_harness_state_dir(agent_dir: &Path) -> PathBuf {
    agent_dir.join(HARNESS_STATE_DIR_NAME)
}

/// `<sessionArtifactDir>/harness`.
#[must_use]
pub fn local_harness_state_dir(session_artifact_dir: &Path) -> PathBuf {
    session_artifact_dir.join(HARNESS_STATE_DIR_NAME)
}

/// `<dir>/harness_state.json`.
#[must_use]
pub fn harness_state_path(harness_state_dir: &Path) -> PathBuf {
    harness_state_dir.join(HARNESS_STATE_FILE_NAME)
}

/// One harness state file as raw JSON.
#[derive(Debug, Clone, PartialEq)]
pub struct HarnessDocument {
    map: Map<String, Value>,
}

impl HarnessDocument {
    /// The TS empty state (`emptyHarnessState`): what a missing or corrupt
    /// file loads as, and so what the first write builds on.
    #[must_use]
    pub fn empty() -> Self {
        let Value::Object(map) = serde_json::json!({
            "schema": 1,
            "entries": { "prompt": {}, "memory": {}, "skill": {}, "subagent": {} },
            "refinements": [],
        }) else {
            unreachable!("a JSON object literal")
        };
        Self { map }
    }

    /// Load `<dir>/harness_state.json`; missing, unreadable, or not an
    /// object reads as [`Self::empty`].
    #[must_use]
    pub fn load(harness_state_dir: &Path) -> Self {
        let path = harness_state_path(harness_state_dir);
        let parsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok());
        match parsed {
            Some(Value::Object(map)) => Self { map },
            Some(_) => {
                tracing::warn!(path = %path.display(), reason = "not-an-object", "harness.state.corrupt");
                Self::empty()
            }
            None => {
                if path.exists() {
                    tracing::warn!(path = %path.display(), reason = "unreadable", "harness.state.corrupt");
                }
                Self::empty()
            }
        }
    }

    /// A top-level value.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.map.get(key)
    }

    /// Replace a top-level value in place, or insert it where a TS save
    /// would put it.
    pub fn set(&mut self, key: &str, value: Value) {
        if let Some(slot) = self.map.get_mut(key) {
            *slot = value;
            return;
        }
        let rank = CANONICAL_KEY_ORDER.iter().position(|known| *known == key);
        let before = rank.and_then(|rank| {
            self.map.keys().position(|existing| {
                CANONICAL_KEY_ORDER
                    .iter()
                    .position(|known| known == existing)
                    .is_some_and(|existing_rank| existing_rank > rank)
            })
        });
        match before {
            Some(index) => {
                self.map.shift_insert(index, key.to_string(), value);
            }
            None => {
                self.map.insert(key.to_string(), value);
            }
        }
    }

    /// The failure ledger the document holds (empty when absent).
    #[must_use]
    pub fn failures(&self) -> FailureLedger {
        self.map
            .get(FAILURES_KEY)
            .map(normalize_failure_ledger)
            .unwrap_or_default()
    }

    /// Store a failure ledger.
    pub fn set_failures(&mut self, ledger: &FailureLedger) {
        self.set(FAILURES_KEY, ledger.to_value());
    }

    /// The document as the JSON value it serializes.
    #[must_use]
    pub fn as_map(&self) -> &Map<String, Value> {
        &self.map
    }

    /// Write `<dir>/harness_state.json` atomically.
    ///
    /// # Errors
    ///
    /// [`HarnessStateError::Io`] when the directory, the temp file, or the
    /// rename fails.
    pub fn save(&self, harness_state_dir: &Path) -> Result<PathBuf, HarnessStateError> {
        let path = harness_state_path(harness_state_dir);
        let io = |source| HarnessStateError::Io {
            path: path.clone(),
            source,
        };
        std::fs::create_dir_all(harness_state_dir).map_err(io)?;
        let text = serde_json::to_string_pretty(&self.map)
            .map_err(|error| io(std::io::Error::other(error)))?;
        write_atomically(&path, &format!("{text}\n")).map_err(io)?;
        Ok(path)
    }
}

/// Temp file + rename over the (symlink-resolved) target, keeping its mode.
fn write_atomically(path: &Path, text: &str) -> std::io::Result<()> {
    let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mode = pa_core::platform::file_mode(&target);
    let mut suffix = [0u8; 4];
    getrandom::fill(&mut suffix).map_err(std::io::Error::other)?;
    let mut temp = target.as_os_str().to_os_string();
    temp.push(format!(
        ".{}.{}.tmp",
        std::process::id(),
        crate::fingerprint::hex(&suffix)
    ));
    let temp = PathBuf::from(temp);
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        pa_core::platform::set_private_mode(&mut options);
        let mut file = options.open(&temp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        pa_core::platform::rename_onto(&temp, &target)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result?;
    match mode {
        Some(mode) => set_mode(&target, mode),
        None => pa_core::platform::restrict_file(&target),
    }
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o777))
}

/// No mode bits to carry over off unix: the renamed file keeps its ACL;
/// only confirm it landed.
#[cfg(not(unix))]
fn set_mode(path: &Path, _mode: u32) -> std::io::Result<()> {
    std::fs::metadata(path).map(|_| ())
}

/// Acquire the harness state lock of `harness_state_dir` (the file's one
/// lock, [`pa_core::refinement::lock::lock_harness_state_file`]); dropping
/// the guard releases it. Blocking; call it off the async runtime.
///
/// # Errors
///
/// [`HarnessStateError::Locked`] when one holder kept the lock past the
/// wait, and [`HarnessStateError::Io`] when the directory or the lock
/// cannot be made.
pub fn acquire_harness_state_lock(
    harness_state_dir: &Path,
) -> Result<pa_core::platform::HeartbeatLock, HarnessStateError> {
    let path = harness_state_path(harness_state_dir);
    pa_core::refinement::lock::lock_harness_state_file(
        &path,
        pa_core::refinement::lock::HARNESS_STATE_LOCK,
    )
    .map_err(|source| {
        if source.kind() == std::io::ErrorKind::TimedOut {
            HarnessStateError::Locked(path.clone())
        } else {
            HarnessStateError::Io {
                path: path.clone(),
                source,
            }
        }
    })
}

/// Run `update` while holding the harness state lock of `harness_state_dir`
/// ([`acquire_harness_state_lock`]). The guard is lent to `update` so it
/// can confirm the lock is still its own right before it saves. Blocking;
/// call it off the async runtime.
///
/// # Errors
///
/// The errors of [`acquire_harness_state_lock`].
pub fn with_harness_state_lock<T>(
    harness_state_dir: &Path,
    update: impl FnOnce(&pa_core::platform::HeartbeatLock) -> T,
) -> Result<T, HarnessStateError> {
    let lock = acquire_harness_state_lock(harness_state_dir)?;
    let result = update(&lock);
    drop(lock);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_flag_turns_the_global_ledger_off_only_on_explicit_no() {
        assert!(global_failure_ledger_enabled(None));
        for value in ["1", "on", "true", "yes", "", "  ", "anything"] {
            assert!(global_failure_ledger_enabled(Some(value)), "{value:?}");
        }
        for value in ["0", "off", "false", "no", " OFF ", "No", "FALSE"] {
            assert!(!global_failure_ledger_enabled(Some(value)), "{value:?}");
        }
    }

    #[test]
    fn a_new_key_lands_where_a_ts_save_puts_it_and_others_stay_put() {
        let Value::Object(map) = serde_json::json!({
            "schema": 1, "entries": {}, "refinements": [], "ravo": {"x": 1}, "trustWindows": {}, "zzz": true
        }) else {
            unreachable!()
        };
        let mut document = HarnessDocument { map };
        document.set_failures(&FailureLedger::default());
        let keys: Vec<&String> = document.as_map().keys().collect();
        assert_eq!(
            keys,
            [
                "schema",
                "entries",
                "refinements",
                "ravo",
                "failures",
                "trustWindows",
                "zzz"
            ]
        );
        document.set("ravo", Value::Null);
        let keys: Vec<&String> = document.as_map().keys().collect();
        assert_eq!(
            keys,
            [
                "schema",
                "entries",
                "refinements",
                "ravo",
                "failures",
                "trustWindows",
                "zzz"
            ]
        );
    }

    #[test]
    fn a_save_keeps_unknown_keys_and_the_files_mode() {
        let dir = tempfile::tempdir().unwrap();
        let harness = dir.path().join("harness");
        std::fs::create_dir_all(&harness).unwrap();
        let path = harness_state_path(&harness);
        std::fs::write(
            &path,
            "{\n  \"schema\": 1,\n  \"other\": {\"keep\": [1, 2]}\n}\n",
        )
        .unwrap();
        #[cfg(unix)]
        set_mode(&path, 0o640).unwrap();
        let mut document = HarnessDocument::load(&harness);
        document.set_failures(&FailureLedger::default());
        document.save(&harness).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\n  \"schema\": 1,\n  \"other\": {\n    \"keep\": [\n      1,\n      2\n    ]\n  },\n  \"failures\": {\n    \"schema\": 1,\n    \"failures\": {},\n    \"lastScannedEntryIndex\": 0\n  }\n}\n"
        );
        #[cfg(unix)]
        assert_eq!(
            pa_core::platform::file_mode(&path).map(|mode| mode & 0o777),
            Some(0o640)
        );
    }

    #[test]
    fn a_missing_or_corrupt_file_loads_as_the_ts_empty_state() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(HarnessDocument::load(dir.path()), HarnessDocument::empty());
        std::fs::write(harness_state_path(dir.path()), "{ nope").unwrap();
        assert_eq!(HarnessDocument::load(dir.path()), HarnessDocument::empty());
        std::fs::write(harness_state_path(dir.path()), "[1]").unwrap();
        assert_eq!(HarnessDocument::load(dir.path()), HarnessDocument::empty());
    }

    /// A held lock makes the update wait for the holder instead of failing
    /// fast: the update runs after the release and reads what the holder
    /// saved, so neither write is lost.
    #[test]
    fn a_held_lock_makes_the_update_wait_for_the_holder() {
        let dir = tempfile::tempdir().unwrap();
        let path = harness_state_path(dir.path());
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let holder = {
            let dir = dir.path().to_path_buf();
            std::thread::spawn(move || {
                let held = acquire_harness_state_lock(&dir).unwrap();
                held_tx.send(()).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(200));
                std::fs::write(harness_state_path(&dir), "{\"holder\":true}\n").unwrap();
                drop(held);
            })
        };
        held_rx.recv().unwrap();
        let seen = with_harness_state_lock(dir.path(), |_| {
            std::fs::read_to_string(harness_state_path(dir.path())).unwrap_or_default()
        })
        .unwrap();
        holder.join().unwrap();
        assert!(
            seen.contains("holder"),
            "the update ran after the holder's save: {seen:?}"
        );
        assert!(!pa_core::platform::LockDir::path_for(&path).exists());
    }
}
