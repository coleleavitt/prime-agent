//! The trajectory store (`<agentDir>/learning/trajectory.json`) and the
//! backfill reader. Writes are temp file + fsync + rename under the TS
//! `proper-lockfile` lock (`trajectory.json.lock`); reads go through a
//! `(mtime, size)` stat cache, so a digest render re-parses only a changed
//! file. Every failure degrades to "no trajectory".

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, SystemTime};

use serde_json::Value;

use crate::index::read_learning_index;
use crate::json::stringify_pretty;
use crate::trajectory::{CorpusDay, TrajectoryLabel, TrajectoryLabelKind, TrajectoryStoreFile};

const STORE_LOCK_ATTEMPTS: usize = 40;
const STORE_LOCK_RETRY: Duration = Duration::from_millis(5);
const STORE_LOCK_STALE: Duration = Duration::from_secs(10);
/// A sealed diff is tiny; anything larger is treated as absent.
const MAX_STORE_BYTES: u64 = 8 * 1024 * 1024;
const LOG_TARGET: &str = "pa_learning::trajectory";

/// `<agentDir>/learning`.
#[must_use]
pub fn learning_dir(agent_dir: &Path) -> PathBuf {
    agent_dir.join("learning")
}

/// `<agentDir>/learning/days`: the sealed day roll-ups.
#[must_use]
pub fn learning_index_dir(agent_dir: &Path) -> PathBuf {
    learning_dir(agent_dir).join("days")
}

/// `<agentDir>/learning/trajectory.json`.
#[must_use]
pub fn trajectory_index_path(agent_dir: &Path) -> PathBuf {
    learning_dir(agent_dir).join("trajectory.json")
}

/// `<agentDir>/learning/backfill`: one corpus per subdirectory.
#[must_use]
pub fn trajectory_backfill_dir(agent_dir: &Path) -> PathBuf {
    learning_dir(agent_dir).join("backfill")
}

/// `<agentDir>/logs/agent.jsonl`.
#[must_use]
pub fn agent_log_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join("logs").join("agent.jsonl")
}

#[derive(Clone)]
struct Cached {
    modified: Option<SystemTime>,
    size: u64,
    file: Option<TrajectoryStoreFile>,
}

static CACHE: LazyLock<Mutex<HashMap<PathBuf, Cached>>> = LazyLock::new(Mutex::default);

fn cache() -> std::sync::MutexGuard<'static, HashMap<PathBuf, Cached>> {
    CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The sealed trajectory of `agent_dir`, through the stat cache; `None`
/// when it is absent, unreadable, oversized or malformed. Opens no span and
/// scans no day: this is the digest-render read.
#[must_use]
pub fn read_trajectory_index(agent_dir: &Path) -> Option<TrajectoryStoreFile> {
    let path = trajectory_index_path(agent_dir);
    let metadata = match std::fs::metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            tracing::warn!(target: LOG_TARGET, path = %path.display(), error = %error, "trajectory store cannot be stat'd");
            return None;
        }
    };
    let (modified, size) = (metadata.modified().ok(), metadata.len());
    if let Some(cached) = cache().get(&path) {
        if cached.modified.is_some() && cached.modified == modified && cached.size == size {
            return cached.file.clone();
        }
    }
    let file = if size > MAX_STORE_BYTES {
        tracing::warn!(target: LOG_TARGET, path = %path.display(), error = %format!("{size} bytes"), "trajectory store is larger than expected; ignoring it");
        None
    } else {
        read_store(&path)
    };
    cache().insert(
        path,
        Cached {
            modified,
            size,
            file: file.clone(),
        },
    );
    file
}

fn read_store(path: &Path) -> Option<TrajectoryStoreFile> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(target: LOG_TARGET, path = %path.display(), error = %error, "trajectory store is unreadable; ignoring it");
            }
            return None;
        }
    };
    let parsed = match serde_json::from_str::<Value>(&text) {
        Ok(parsed) => parsed,
        Err(error) => {
            tracing::warn!(target: LOG_TARGET, path = %path.display(), error = %error, "trajectory store is unreadable; ignoring it");
            return None;
        }
    };
    let file = parse_store(&parsed);
    if file.is_none() {
        tracing::warn!(target: LOG_TARGET, path = %path.display(), "trajectory store has an unexpected shape; ignoring it");
    }
    file
}

/// Validate-on-read (TS `isTrajectoryStoreFile`): the version, the
/// counters, every window's key fields, every label's fingerprint, kind and
/// non-empty confounds. Fields the TS check leaves open read as their
/// defaults.
fn parse_store(value: &Value) -> Option<TrajectoryStoreFile> {
    let file = value.as_object()?;
    if file.get("version").and_then(Value::as_f64) != Some(1.0) {
        return None;
    }
    let sealed_at = file.get("sealedAt")?.as_str()?.to_string();
    let windows_observed = file.get("windowsObserved")?.as_f64()?;
    let min_windows = file.get("minWindows")?.as_f64()?;
    let windows = file.get("windows")?.as_array()?;
    let labels = file.get("labels")?.as_array()?;
    file.get("rate")?.as_array()?;
    for window in windows {
        let window = window.as_object()?;
        window.get("window")?.as_str()?;
        window.get("turns")?.as_f64()?;
        window.get("corpus")?.as_str()?;
        window.get("days")?.as_array()?;
        window.get("fingerprints")?.as_array()?;
    }
    let labels = labels.iter().map(parse_label).collect::<Option<Vec<_>>>()?;
    let windows = windows.iter().filter_map(parse_window).collect();
    let rate = file
        .get("rate")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(parse_rate)
        .collect();
    Some(TrajectoryStoreFile {
        sealed_at,
        windows_observed: as_count(windows_observed),
        min_windows: as_count(min_windows),
        windows,
        labels,
        rate,
    })
}

fn as_count(value: f64) -> u64 {
    if value.is_nan() || value <= 0.0 {
        return 0;
    }
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a non-negative counter; `as` saturates"
    )]
    let count = value.trunc() as u64;
    count
}

fn text(object: &serde_json::Map<String, Value>, key: &str) -> String {
    object
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn parse_label(value: &Value) -> Option<TrajectoryLabel> {
    let label = value.as_object()?;
    let fingerprint = label.get("fingerprint")?.as_str()?.to_string();
    let kind = match label.get("label")? {
        Value::Null => None,
        Value::String(kind) => Some(TrajectoryLabelKind::parse(kind)?),
        _ => return None,
    };
    let confounds: Vec<String> = label
        .get("confounds")?
        .as_array()?
        .iter()
        .map(|confound| match confound {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
        .collect();
    if confounds.is_empty() {
        return None;
    }
    let count = |key: &str| label.get(key).and_then(Value::as_f64).map_or(0, as_count);
    let flag = |key: &str| label.get(key) == Some(&Value::Bool(true));
    Some(TrajectoryLabel {
        fingerprint,
        name: text(label, "name"),
        message: text(label, "message"),
        corpus: text(label, "corpus"),
        label: kind,
        withheld: label
            .get("withheld")
            .and_then(Value::as_str)
            .map(str::to_string),
        since_window: text(label, "sinceWindow"),
        last_window: text(label, "lastWindow"),
        windows_present: count("windowsPresent"),
        windows_recurring: count("windowsRecurring"),
        claimed_by_refinement: flag("claimedByRefinement"),
        domain_active: flag("domainActive"),
        security_class: flag("securityClass"),
        confounds,
    })
}

fn parse_window(value: &Value) -> Option<crate::trajectory::TrajectoryWindow> {
    let window = value.as_object()?;
    let fingerprints = window
        .get("fingerprints")?
        .as_array()?
        .iter()
        .filter_map(|item| {
            let item = item.as_object()?;
            Some(crate::trajectory::TrajectoryFingerprintWindow {
                fingerprint: item.get("fingerprint")?.as_str()?.to_string(),
                name: text(item, "name"),
                message: text(item, "message"),
                count: item
                    .get("count")
                    .and_then(Value::as_f64)
                    .map_or(0, as_count),
                cumulative_ordinal: item
                    .get("cumulativeOrdinal")
                    .and_then(Value::as_f64)
                    .map_or(0, as_count),
            })
        })
        .collect();
    Some(crate::trajectory::TrajectoryWindow {
        window: text(window, "window"),
        sealed_at: text(window, "sealedAt"),
        days: window
            .get("days")?
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        turns: window
            .get("turns")
            .and_then(Value::as_f64)
            .map_or(0, as_count),
        corpus: text(window, "corpus"),
        fingerprints,
    })
}

fn parse_rate(value: &Value) -> Option<crate::trajectory::TrajectoryRateWindow> {
    let step = value.as_object()?;
    let optional = |key: &str| step.get(key).and_then(Value::as_f64);
    Some(crate::trajectory::TrajectoryRateWindow {
        window: text(step, "window"),
        appeared: optional("appeared").map_or(0, as_count),
        retired: optional("retired").map(as_count),
        #[expect(clippy::cast_possible_truncation, reason = "a small window delta")]
        new_minus_retired: optional("newMinusRetired").map(|value| value as i64),
    })
}

/// Keep `trajectory.json` and `backfill/` out of any checkout the agent dir
/// happens to sit in. Best effort.
fn write_store_gitignore(learning_dir: &Path) {
    let path = learning_dir.join(".gitignore");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let lines: std::collections::HashSet<&str> = existing.split('\n').map(str::trim).collect();
    let missing: Vec<&str> = ["trajectory.json", "backfill/"]
        .into_iter()
        .filter(|entry| !lines.contains(entry))
        .collect();
    if missing.is_empty() {
        return;
    }
    let mut next = existing.clone();
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(&missing.join("\n"));
    next.push('\n');
    let _ = crate::fs::write_private_file(&path, next.as_bytes());
}

fn acquire_store_lock(path: &Path) -> std::io::Result<Option<pa_core::platform::LockDir>> {
    for attempt in 0..STORE_LOCK_ATTEMPTS {
        match pa_core::platform::LockDir::acquire(path, STORE_LOCK_STALE) {
            Ok(lock) => return Ok(Some(lock)),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if attempt + 1 < STORE_LOCK_ATTEMPTS {
                    std::thread::sleep(STORE_LOCK_RETRY);
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

/// Persist a sealed file for `agent_dir`: prime only and bounded, whatever
/// the caller passed. A failure is logged and leaves the store as it was.
/// Blocking; off the turn path.
pub fn write_trajectory_index(file: &TrajectoryStoreFile, agent_dir: &Path) {
    let path = trajectory_index_path(agent_dir);
    if let Err(error) = save(&path, file) {
        tracing::warn!(target: LOG_TARGET, path = %path.display(), error = %error, "trajectory store write failed; index stays absent");
    }
}

fn save(path: &Path, file: &TrajectoryStoreFile) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    crate::fs::create_private_dir(dir)?;
    crate::fs::set_mode(dir, 0o700)?;
    write_store_gitignore(dir);
    let Some(_lock) = acquire_store_lock(path)? else {
        tracing::warn!(target: LOG_TARGET, path = %path.display(), "trajectory store is locked by another process; index not written");
        return Ok(());
    };
    let nonce = temp_nonce();
    let temp = dir.join(format!(
        "{}.{}.{}.tmp",
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        std::process::id(),
        nonce.iter().fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
    ));
    let content = format!("{}\n", stringify_pretty(&file.bounded().to_json()));
    let written = crate::fs::write_private_file_synced(&temp, content.as_bytes(), true, true)
        .and_then(|()| std::fs::rename(&temp, path));
    if let Err(error) = written {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    crate::fs::set_mode(path, 0o600)?;
    cache().remove(path);
    Ok(())
}

/// Four bytes that keep two writers' temp names apart (the TS
/// `randomBytes(4)`; the lock already serializes writers): the clock's
/// nanoseconds mixed with the thread id.
fn temp_nonce() -> [u8; 4] {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    let thread = format!("{:?}", std::thread::current().id());
    thread
        .bytes()
        .fold(nanos, |acc, byte| acc.rotate_left(5) ^ u32::from(byte))
        .to_le_bytes()
}

/// The pre-generated cross-tool backfill days (one corpus per subdirectory
/// of `backfill_dir`, each in the day schema), tagged `backfill:<name>`.
/// CLI only: never written to the store or the prompt.
#[must_use]
pub fn read_backfill_days(backfill_dir: &Path) -> Vec<CorpusDay> {
    let Ok(entries) = std::fs::read_dir(backfill_dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
        .into_iter()
        .filter(|name| backfill_dir.join(name).is_dir())
        .flat_map(|name| {
            let corpus = format!("backfill:{name}");
            read_learning_index(&backfill_dir.join(&name))
                .into_iter()
                .map(move |day| CorpusDay {
                    corpus: corpus.clone(),
                    day,
                })
        })
        .collect()
}
