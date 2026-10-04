//! The RAVO run archive (TS `ravo/archive.ts`): an append-only,
//! hash-chained event log (`events.jsonl`, each record the canonical JSON
//! of `{seq, type, timestamp, payload, prevDigest, digest}`), its derived
//! state (`state.json`: revision, champion digest, event count, head
//! digest), and the compare-and-set an accepted champion commits through.
//! Every operation re-reads and verifies the log under the directory's
//! `.archive.lock` (the TS `proper-lockfile` protocol).

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Map, Value};

use crate::js::{canonical_json, sha256_hex};

/// The event types the log admits.
pub const RAVO_EVENT_TYPES: [&str; 7] = [
    "run",
    "proposal",
    "evaluation",
    "pressure",
    "accept",
    "reject",
    "stop",
];

const LOCK_STALE: Duration = Duration::from_secs(30);
const LOCK_ATTEMPTS: u32 = 100;

/// Why an archive operation failed.
#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("RAVO archive I/O at {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("RAVO archive is corrupt: {0}")]
    Corrupt(String),
    #[error("events.jsonl has an incomplete final record")]
    TruncatedTail,
    #[error("Champion changed since evaluation")]
    StaleCommit,
    #[error("Sensitive field is not allowed in RAVO archive: {0}")]
    Sensitive(String),
    #[error("could not lock the RAVO archive at {0}")]
    Locked(PathBuf),
}

/// The log's derived state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ArchiveState {
    pub revision: u64,
    pub champion_digest: Option<String>,
    pub event_count: u64,
    pub head_digest: Option<String>,
}

/// The champion a commit must still find.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChampionCas {
    pub revision: u64,
    pub champion_digest: Option<String>,
}

impl ArchiveState {
    fn to_value(&self) -> Value {
        json!({
            "revision": self.revision,
            "championDigest": self.champion_digest,
            "eventCount": self.event_count,
            "headDigest": self.head_digest,
        })
    }

    /// The compare-and-set this state stands for.
    #[must_use]
    pub fn cas(&self) -> ChampionCas {
        ChampionCas {
            revision: self.revision,
            champion_digest: self.champion_digest.clone(),
        }
    }
}

/// One archive directory.
pub struct RavoArchive {
    directory: PathBuf,
    now: Box<dyn Fn() -> String + Send + Sync>,
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> ArchiveError + '_ {
    move |source| ArchiveError::Io {
        path: path.to_path_buf(),
        source,
    }
}

fn is_digest(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// A key naming a secret (TS `SENSITIVE_KEY`:
/// `(?:^|_)(?:api[_-]?key|authorization|cookie|credential|password|private[_-]?key|secret|token)(?:$|_)`,
/// case-insensitive).
fn is_sensitive_key(key: &str) -> bool {
    const WORDS: [&str; 12] = [
        "apikey",
        "api_key",
        "api-key",
        "authorization",
        "cookie",
        "credential",
        "password",
        "privatekey",
        "private_key",
        "private-key",
        "secret",
        "token",
    ];
    let lower = key.to_ascii_lowercase();
    let starts = std::iter::once(0).chain(lower.match_indices('_').map(|(index, _)| index + 1));
    starts.into_iter().any(|start| {
        WORDS.iter().any(|word| {
            lower[start..].starts_with(word)
                && matches!(lower.as_bytes().get(start + word.len()), None | Some(b'_'))
        })
    })
}

fn assert_no_secrets(value: &Value, path: &mut Vec<String>) -> Result<(), ArchiveError> {
    match value {
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                path.push(index.to_string());
                assert_no_secrets(item, path)?;
                path.pop();
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                path.push(key.clone());
                if is_sensitive_key(key) {
                    return Err(ArchiveError::Sensitive(path.join(".")));
                }
                assert_no_secrets(item, path)?;
                path.pop();
            }
        }
        _ => {}
    }
    Ok(())
}

impl RavoArchive {
    /// The archive at `<artifact_root>/<archive_path>`.
    #[must_use]
    pub fn new(artifact_root: &Path, archive_path: &str) -> Self {
        Self {
            directory: artifact_root.join(archive_path),
            now: Box::new(pa_ledger::now_iso),
        }
    }

    /// Stamp events with `now` (tests).
    #[cfg(test)]
    #[must_use]
    pub fn with_clock(mut self, now: impl Fn() -> String + Send + Sync + 'static) -> Self {
        self.now = Box::new(now);
        self
    }

    fn events_path(&self) -> PathBuf {
        self.directory.join("events.jsonl")
    }

    fn locked<T>(
        &self,
        operation: impl FnOnce() -> Result<T, ArchiveError>,
    ) -> Result<T, ArchiveError> {
        std::fs::create_dir_all(self.directory.join("blobs")).map_err(io(&self.directory))?;
        let lock_path = self.directory.join(".archive.lock");
        let mut delay = Duration::from_millis(10);
        for attempt in 0..LOCK_ATTEMPTS {
            match pa_core::platform::LockDir::acquire_at(&lock_path, LOCK_STALE) {
                Ok(lock) => {
                    let result = operation();
                    drop(lock);
                    return result;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if attempt + 1 < LOCK_ATTEMPTS {
                        std::thread::sleep(delay);
                        delay = (delay * 6 / 5).min(Duration::from_millis(250));
                    }
                }
                Err(source) => {
                    return Err(ArchiveError::Io {
                        path: lock_path,
                        source,
                    })
                }
            }
        }
        Err(ArchiveError::Locked(lock_path))
    }

    /// Create the log if missing, verify it, and answer its state.
    ///
    /// # Errors
    ///
    /// A corrupt or truncated log, or the I/O failing.
    pub fn initialize(&self) -> Result<ArchiveState, ArchiveError> {
        self.locked(|| self.recover_unlocked())
    }

    /// Verify the log and answer its state.
    ///
    /// # Errors
    ///
    /// A corrupt or truncated log, or the I/O failing.
    pub fn recover(&self) -> Result<ArchiveState, ArchiveError> {
        self.locked(|| self.recover_unlocked())
    }

    fn recover_unlocked(&self) -> Result<ArchiveState, ArchiveError> {
        let path = self.events_path();
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::write(&path, b"").map_err(io(&path))?;
                Vec::new()
            }
            Err(error) => return Err(io(&path)(error)),
        };
        if bytes.last().is_some_and(|last| *last != b'\n') {
            return Err(ArchiveError::TruncatedTail);
        }
        let text = String::from_utf8(bytes)
            .map_err(|_| ArchiveError::Corrupt("events.jsonl is not UTF-8".to_string()))?;
        let mut state = ArchiveState::default();
        for (index, line) in text.lines().enumerate() {
            let event: Value = serde_json::from_str(line).map_err(|_| {
                ArchiveError::Corrupt(format!("Invalid JSON at event {}", index + 1))
            })?;
            state = apply_verified(&state, &event)?;
        }
        self.write_state(&state)?;
        Ok(state)
    }

    /// Append a non-`accept` event.
    ///
    /// # Errors
    ///
    /// A payload naming a secret, a corrupt log, or the I/O failing.
    pub fn append(&self, kind: &str, payload: Map<String, Value>) -> Result<Value, ArchiveError> {
        assert_no_secrets(&Value::Object(payload.clone()), &mut Vec::new())?;
        self.locked(|| {
            let state = self.recover_unlocked()?;
            self.append_unlocked(kind, payload, &state)
        })
    }

    /// Append the `accept` of a champion, when the archive still stands
    /// where `expected` saw it.
    ///
    /// # Errors
    ///
    /// [`ArchiveError::StaleCommit`] when another champion was accepted
    /// meanwhile; the errors of [`Self::append`].
    pub fn accept(
        &self,
        payload: Map<String, Value>,
        expected: &ChampionCas,
        champion_digest: &str,
    ) -> Result<Value, ArchiveError> {
        if !is_digest(champion_digest) {
            return Err(ArchiveError::Corrupt(
                "championDigest must be a SHA-256 digest".to_string(),
            ));
        }
        let mut payload = payload;
        assert_no_secrets(&Value::Object(payload.clone()), &mut Vec::new())?;
        payload.insert("championDigest".to_string(), json!(champion_digest));
        self.locked(|| {
            let state = self.recover_unlocked()?;
            if state.cas() != *expected {
                return Err(ArchiveError::StaleCommit);
            }
            self.append_unlocked("accept", payload, &state)
        })
    }

    fn append_unlocked(
        &self,
        kind: &str,
        payload: Map<String, Value>,
        state: &ArchiveState,
    ) -> Result<Value, ArchiveError> {
        let unsigned = json!({
            "seq": state.event_count + 1,
            "type": kind,
            "timestamp": (self.now)(),
            "payload": Value::Object(payload),
            "prevDigest": state.head_digest,
        });
        let digest = sha256_hex(&canonical_json(&unsigned));
        let mut event = unsigned;
        event["digest"] = json!(digest);
        let path = self.events_path();
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        pa_core::platform::set_private_mode(&mut options);
        let mut file = options.open(&path).map_err(io(&path))?;
        file.write_all(format!("{}\n", canonical_json(&event)).as_bytes())
            .map_err(io(&path))?;
        file.sync_all().map_err(io(&path))?;
        drop(file);
        let next = apply_verified(state, &event)?;
        self.write_state(&next)?;
        Ok(event)
    }

    fn write_state(&self, state: &ArchiveState) -> Result<(), ArchiveError> {
        let path = self.directory.join("state.json");
        let temp = self.directory.join(format!(
            "state.json.{}.{}.tmp",
            std::process::id(),
            sha256_hex(&format!("{:?}", std::time::SystemTime::now()))
                .get(..8)
                .unwrap_or_default()
        ));
        let text = format!("{}\n", canonical_json(&state.to_value()));
        let result = (|| {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            pa_core::platform::set_private_mode(&mut options);
            let mut file = options.open(&temp)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            drop(file);
            pa_core::platform::rename_onto(&temp, &path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result.map_err(io(&path))
    }
}

/// `state` after the verified `event` (TS `validateEvent` + `applyEvent`).
fn apply_verified(state: &ArchiveState, event: &Value) -> Result<ArchiveState, ArchiveError> {
    let Some(record) = event.as_object() else {
        return Err(ArchiveError::Corrupt(
            "Unknown or malformed event".to_string(),
        ));
    };
    let kind = record
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !RAVO_EVENT_TYPES.contains(&kind) {
        return Err(ArchiveError::Corrupt(
            "Unknown or malformed event".to_string(),
        ));
    }
    let seq = record.get("seq").and_then(Value::as_u64);
    let prev = record.get("prevDigest").and_then(Value::as_str);
    if seq != Some(state.event_count + 1) || prev != state.head_digest.as_deref() {
        return Err(ArchiveError::Corrupt(format!(
            "Broken hash chain at sequence {}",
            seq.unwrap_or_default()
        )));
    }
    let digest = record
        .get("digest")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let mut unsigned = record.clone();
    unsigned.remove("digest");
    if digest != sha256_hex(&canonical_json(&Value::Object(unsigned))) {
        return Err(ArchiveError::Corrupt(format!(
            "Digest mismatch at sequence {}",
            seq.unwrap_or_default()
        )));
    }
    let champion = record
        .get("payload")
        .and_then(|payload| payload.get("championDigest"))
        .and_then(Value::as_str);
    if kind == "accept" && !champion.is_some_and(is_digest) {
        return Err(ArchiveError::Corrupt(format!(
            "Invalid champion at sequence {}",
            seq.unwrap_or_default()
        )));
    }
    Ok(ArchiveState {
        revision: state.revision + u64::from(kind == "accept"),
        champion_digest: if kind == "accept" {
            champion.map(str::to_string)
        } else {
            state.champion_digest.clone()
        },
        event_count: state.event_count + 1,
        head_digest: Some(digest),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive(root: &Path) -> RavoArchive {
        RavoArchive::new(root, "ravo/archive").with_clock(|| "2026-01-01T00:00:00.000Z".to_string())
    }

    /// The chain verifies across reopenings; an accept is a CAS on the
    /// champion; a tampered record and a torn tail are refused; a payload
    /// naming a secret never lands.
    #[test]
    fn the_log_is_a_verified_hash_chain_with_a_champion_cas() {
        let root = tempfile::tempdir().unwrap();
        let log = archive(root.path());
        assert_eq!(log.initialize().unwrap(), ArchiveState::default());
        let mut payload = Map::new();
        payload.insert("runId".into(), json!("r1"));
        let first = log.append("run", payload.clone()).unwrap();
        assert_eq!(first["seq"], json!(1));
        assert_eq!(first["prevDigest"], Value::Null);
        let baseline = log.recover().unwrap().cas();
        let digest = "a".repeat(64);
        log.accept(payload.clone(), &baseline, &digest).unwrap();
        let state = archive(root.path()).recover().unwrap();
        assert_eq!(
            (
                state.revision,
                state.champion_digest.as_deref(),
                state.event_count
            ),
            (1, Some(digest.as_str()), 2)
        );
        assert!(matches!(
            log.accept(payload.clone(), &baseline, &digest),
            Err(ArchiveError::StaleCommit)
        ));
        let mut secret = Map::new();
        secret.insert("nested".into(), json!({"api_key": "x"}));
        assert!(matches!(
            log.append("run", secret),
            Err(ArchiveError::Sensitive(path)) if path == "nested.api_key"
        ));
        let events = root.path().join("ravo/archive/events.jsonl");
        let text = std::fs::read_to_string(&events).unwrap();
        std::fs::write(&events, text.replace("\"r1\"", "\"r2\"")).unwrap();
        assert!(matches!(log.recover(), Err(ArchiveError::Corrupt(_))));
        std::fs::write(&events, format!("{text}{{\"seq\":")).unwrap();
        assert!(matches!(log.recover(), Err(ArchiveError::TruncatedTail)));
    }
}
