//! The durable half of Workspace Recall: one small JSON mark per repo under
//! `<agentDir>/recall/`, holding HEAD, digests, and at most
//! [`RECALL_MAX_CLAIMS`] build claims. It stores the invalidation, never the
//! answer: no file content, no command output. Writes are read-merge-write
//! under the `proper-lockfile`-compatible `<mark>.lock` directory, then temp
//! file + rename. File names, JSON text and the lock are byte-compatible
//! with the TS product, so both binaries share marks.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Map, Value};
use sha2::{Digest as _, Sha256};

use crate::claims::{
    is_build_claim_command, merge_recall_claims, utf16_prefix, RecallClaim,
    RECALL_MAX_CLAIM_COMMAND_CHARS,
};
use crate::mark::{
    absent_skip_worktree_digest, capture_workspace, hex, workspace_digest, AbsentPresence,
    AbsentSkipWorktree, CaptureFailure, WorkspaceSnapshot, WorkspaceState, RECALL_DIGEST_ALGORITHM,
    RECALL_MAX_DIRTY_PATHS, RECALL_MAX_TAGGED_PATHS,
};
use crate::time::{format_iso, parse_iso_millis};

pub const RECALL_MARK_SCHEMA: u64 = 1;
/// How long every process leaves a repo alone after one of its git calls
/// timed out.
pub const RECALL_SKIP_TTL_MS: i64 = 10 * 60 * 1000;

const MARK_LOCK_STALE: Duration = Duration::from_secs(10);
/// `proper-lockfile`'s retry schedule as the TS product configures it:
/// 12 retries, factor 1.5, 25 ms to 500 ms.
const MARK_LOCK_RETRIES: u32 = 12;
const MARK_LOCK_MIN_WAIT_MS: u64 = 25;
const MARK_LOCK_MAX_WAIT_MS: u64 = 500;
const SKIP_SCHEMA: u64 = 1;

/// One repo's mark file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecallMarkFile {
    pub repo_root: String,
    pub state: WorkspaceState,
    pub claims: Vec<RecallClaim>,
    pub written_at: String,
}

impl RecallMarkFile {
    /// The file's JSON, keys in the TS product's order.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut object = Map::new();
        object.insert("schema".into(), json!(RECALL_MARK_SCHEMA));
        object.insert("digestAlgorithm".into(), json!(RECALL_DIGEST_ALGORITHM));
        object.insert("repoRoot".into(), json!(self.repo_root));
        object.insert("head".into(), json!(self.state.head));
        object.insert(
            "trackedTreeDigest".into(),
            json!(self.state.tracked_tree_digest),
        );
        object.insert("dirty".into(), js_object_from_entries(&self.state.dirty));
        object.insert("dirtyOverflow".into(), json!(self.state.dirty_overflow));
        match &self.state.absent {
            AbsentPresence::NotRecorded => {}
            AbsentPresence::Unchecked => {
                object.insert("absentSkipWorktree".into(), Value::Null);
            }
            AbsentPresence::Checked(absent) => {
                object.insert("absentSkipWorktree".into(), absent.to_json());
            }
        }
        object.insert(
            "claims".into(),
            Value::Array(self.claims.iter().map(RecallClaim::to_json).collect()),
        );
        object.insert("writtenAt".into(), json!(self.written_at));
        Value::Object(object)
    }

    /// A mark read from disk; `None` when it cannot be trusted as one (TS
    /// `isRecallMarkFile`). Malformed claims are dropped.
    #[must_use]
    pub fn from_json(value: &Value) -> Option<Self> {
        let object = value.as_object()?;
        if object.get("schema")?.as_u64()? != RECALL_MARK_SCHEMA
            || object.get("digestAlgorithm")?.as_str()? != RECALL_DIGEST_ALGORITHM
        {
            return None;
        }
        let head = match object.get("head")? {
            Value::Null => None,
            Value::String(head) => Some(head.clone()),
            _ => return None,
        };
        let dirty = object
            .get("dirty")?
            .as_object()?
            .iter()
            .map(|(path, digest)| Some((path.clone(), digest.as_str()?.to_string())))
            .collect::<Option<Vec<_>>>()?;
        let absent = match object.get("absentSkipWorktree") {
            None => AbsentPresence::NotRecorded,
            Some(Value::Null) => AbsentPresence::Unchecked,
            Some(value) => AbsentPresence::Checked(parse_absent(value)?),
        };
        Some(Self {
            repo_root: object.get("repoRoot")?.as_str()?.to_string(),
            state: WorkspaceState {
                head,
                tracked_tree_digest: object.get("trackedTreeDigest")?.as_str()?.to_string(),
                dirty,
                dirty_overflow: usize::try_from(object.get("dirtyOverflow")?.as_u64()?).ok()?,
                absent,
            },
            claims: object
                .get("claims")?
                .as_array()?
                .iter()
                .filter_map(RecallClaim::from_json)
                .collect(),
            written_at: object.get("writtenAt")?.as_str()?.to_string(),
        })
    }
}

fn parse_absent(value: &Value) -> Option<AbsentSkipWorktree> {
    let object = value.as_object()?;
    let count = usize::try_from(object.get("count")?.as_u64()?).ok()?;
    let digest = object.get("digest")?.as_str()?.to_string();
    let paths = match object.get("paths") {
        None if count > RECALL_MAX_TAGGED_PATHS => None,
        None => return None,
        Some(paths) => {
            let paths = paths
                .as_array()?
                .iter()
                .map(|path| path.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()?;
            if paths.len() != count || absent_skip_worktree_digest(&paths) != digest {
                return None;
            }
            Some(paths)
        }
    };
    Some(AbsentSkipWorktree {
        count,
        digest,
        paths,
    })
}

/// `Object.fromEntries` order: array-index keys first in ascending numeric
/// order, then the rest in insertion order.
fn js_object_from_entries(entries: &[(String, String)]) -> Value {
    let index_key = |key: &str| -> Option<u32> {
        let canonical = key == "0" || (!key.starts_with('0') && !key.is_empty());
        if !canonical || !key.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        key.parse::<u32>().ok().filter(|index| *index != u32::MAX)
    };
    let mut indexed: Vec<(u32, &(String, String))> = entries
        .iter()
        .filter_map(|entry| index_key(&entry.0).map(|index| (index, entry)))
        .collect();
    indexed.sort_by_key(|(index, _)| *index);
    let mut object = Map::new();
    for (_, (key, digest)) in indexed {
        object.insert(key.clone(), json!(digest));
    }
    for (key, digest) in entries {
        if index_key(key).is_none() {
            object.insert(key.clone(), json!(digest));
        }
    }
    Value::Object(object)
}

/// `<agentDir>/recall/<basename>.<sha256(repoRoot)[:16]>.json`.
#[must_use]
pub fn recall_mark_path(repo_root: &str, agent_dir: &Path) -> PathBuf {
    let hash = Sha256::digest(repo_root.as_bytes());
    let hash = hex(&hash[..8]);
    let basename = repo_root
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("");
    // The TS replace runs per UTF-16 unit: an astral character becomes "__".
    let mut name = String::new();
    for ch in basename.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            name.push(ch);
        } else {
            name.push_str(&"_".repeat(ch.len_utf16()));
        }
    }
    if name.is_empty() {
        name.push_str("repo");
    }
    agent_dir.join("recall").join(format!("{name}.{hash}.json"))
}

/// `<basename>.<16 hex>`: the mark file name without its extension.
#[must_use]
pub fn recall_repo_key(repo_root: &str, agent_dir: &Path) -> String {
    let path = recall_mark_path(repo_root, agent_dir);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    name.strip_suffix(".json").unwrap_or(&name).to_string()
}

#[must_use]
pub fn recall_skip_path(repo_root: &str, agent_dir: &Path) -> PathBuf {
    recall_mark_path(repo_root, agent_dir).with_extension("skip.json")
}

/// The mark for `repo_root`, or `None` when there is none or it cannot be
/// trusted as one.
#[must_use]
pub fn read_recall_mark(repo_root: &str, agent_dir: &Path) -> Option<RecallMarkFile> {
    let mark_path = recall_mark_path(repo_root, agent_dir);
    let text = match std::fs::read_to_string(&mark_path) {
        Ok(text) => text,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!(path = %mark_path.display(), %error, "recall mark is unreadable; ignoring it");
            }
            return None;
        }
    };
    let mark = serde_json::from_str::<Value>(&text)
        .ok()
        .as_ref()
        .and_then(RecallMarkFile::from_json)
        .filter(|mark| mark.repo_root == repo_root);
    if mark.is_none() {
        tracing::debug!(path = %mark_path.display(), "recall mark has an unexpected shape; ignoring it");
    }
    mark
}

/// A build claim offered to a mark write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecallClaimInput {
    pub command: String,
    pub exit_code: i64,
    /// `workspace_digest` of the workspace the command ran against;
    /// defaults to the workspace captured by this write.
    pub digest_at_claim: Option<String>,
    /// ISO timestamp; defaults to the time of this write.
    pub at: Option<String>,
}

/// Why no mark was written.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MarkSkipReason {
    #[error("git_unavailable")]
    GitUnavailable,
    #[error("git_timeout")]
    GitTimeout,
    #[error("lock_busy")]
    LockBusy,
    #[error("write_failed: {0}")]
    WriteFailed(String),
    #[error("not_repo")]
    NotRepo,
}

impl From<CaptureFailure> for MarkSkipReason {
    fn from(failure: CaptureFailure) -> Self {
        match failure {
            CaptureFailure::GitUnavailable => MarkSkipReason::GitUnavailable,
            CaptureFailure::GitTimeout => MarkSkipReason::GitTimeout,
            CaptureFailure::NotRepo => MarkSkipReason::NotRepo,
        }
    }
}

/// A written mark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrittenMark {
    pub mark_path: PathBuf,
    pub repo_key: String,
    pub mark: RecallMarkFile,
    /// The mark this write replaced, if there was a readable one.
    pub previous: Option<RecallMarkFile>,
    pub snapshot: WorkspaceSnapshot,
}

fn claim_timestamp(at: Option<&str>, fallback: i64) -> String {
    match at {
        Some(at) if parse_iso_millis(at).is_some() => at.to_string(),
        Some(_) | None => format_iso(fallback),
    }
}

async fn acquire_mark_lock(mark_path: &Path) -> Result<pa_core::platform::LockDir, MarkSkipReason> {
    let mut wait_ms = MARK_LOCK_MIN_WAIT_MS;
    for attempt in 0..=MARK_LOCK_RETRIES {
        match pa_core::platform::LockDir::acquire(mark_path, MARK_LOCK_STALE) {
            Ok(lock) => return Ok(lock),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if attempt == MARK_LOCK_RETRIES {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(wait_ms)).await;
                wait_ms = (wait_ms * 3 / 2).min(MARK_LOCK_MAX_WAIT_MS);
            }
            Err(error) => return Err(MarkSkipReason::WriteFailed(error.to_string())),
        }
    }
    Err(MarkSkipReason::LockBusy)
}

/// Capture the workspace and write it as the repo's mark, carrying the
/// previous mark's claims forward and adding `claims` (only build commands
/// that exited 0). A previous mark from before absent skip-worktree paths
/// were recorded carries none: its claims were digested without that field
/// and can never be CURRENT again.
///
/// # Errors
///
/// Every reason no mark was written, as a [`MarkSkipReason`].
pub async fn write_recall_mark(
    repo_root: &str,
    agent_dir: &Path,
    claims: &[RecallClaimInput],
    now: i64,
) -> Result<WrittenMark, MarkSkipReason> {
    let mark_path = recall_mark_path(repo_root, agent_dir);
    let mark_dir = mark_path.parent().unwrap_or(agent_dir).to_path_buf();
    if let Err(error) =
        std::fs::create_dir_all(&mark_dir).and_then(|()| pa_core::platform::restrict_dir(&mark_dir))
    {
        tracing::debug!(path = %mark_path.display(), %error, "recall mark directory is not writable; mark not written");
        return Err(MarkSkipReason::WriteFailed(error.to_string()));
    }
    let _lock = acquire_mark_lock(&mark_path).await?;
    // Captured under the lock so two sessions can never land an older
    // snapshot over a newer one.
    let snapshot = capture_workspace(repo_root, Some(agent_dir)).await?;
    let previous = read_recall_mark(repo_root, agent_dir);
    let digest = workspace_digest(&snapshot.state);
    let added: Vec<RecallClaim> = claims
        .iter()
        .filter(|claim| claim.exit_code == 0 && is_build_claim_command(&claim.command))
        .map(|claim| RecallClaim {
            command: utf16_prefix(claim.command.trim(), RECALL_MAX_CLAIM_COMMAND_CHARS).to_string(),
            exit_code: claim.exit_code,
            at: claim_timestamp(claim.at.as_deref(), now),
            digest_at_claim: claim
                .digest_at_claim
                .clone()
                .unwrap_or_else(|| digest.clone()),
        })
        .collect();
    let carried = match &previous {
        Some(previous) if previous.state.absent != AbsentPresence::NotRecorded => {
            previous.claims.as_slice()
        }
        Some(_) | None => &[],
    };
    let mut state = snapshot.state.clone();
    state.dirty.truncate(RECALL_MAX_DIRTY_PATHS);
    let mark = RecallMarkFile {
        repo_root: repo_root.to_string(),
        state,
        claims: merge_recall_claims(carried, &added),
        written_at: format_iso(now),
    };
    if let Err(error) = write_json_atomically(&mark_path, &mark.to_json()) {
        tracing::debug!(path = %mark_path.display(), %error, "recall mark write failed");
        return Err(MarkSkipReason::WriteFailed(error.to_string()));
    }
    Ok(WrittenMark {
        repo_key: recall_repo_key(repo_root, agent_dir),
        mark_path,
        mark,
        previous,
        snapshot,
    })
}

/// The unexpired git-timeout entry for `repo_root`, shared by every process
/// using this agent dir: its `until` in epoch milliseconds.
#[must_use]
pub fn read_recall_skip(repo_root: &str, agent_dir: &Path, now: i64) -> Option<i64> {
    let text = std::fs::read_to_string(recall_skip_path(repo_root, agent_dir)).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    if value.get("schema")?.as_u64()? != SKIP_SCHEMA
        || value.get("reason")?.as_str()? != "git_timeout"
    {
        return None;
    }
    let until = parse_iso_millis(value.get("until")?.as_str()?)?;
    // An entry reaching further out than one TTL did not come from this clock.
    (until > now && until <= now + RECALL_SKIP_TTL_MS).then_some(until)
}

/// Leave `repo_root` alone for [`RECALL_SKIP_TTL_MS`] after a git timeout.
/// Best effort: the entry's `until` is returned even if it was not stored.
pub fn write_recall_skip(repo_root: &str, agent_dir: &Path, now: i64) -> i64 {
    let until = now + RECALL_SKIP_TTL_MS;
    let skip_path = recall_skip_path(repo_root, agent_dir);
    let written = skip_path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| {
            write_json_atomically(
                &skip_path,
                &json!({ "schema": SKIP_SCHEMA, "reason": "git_timeout", "until": format_iso(until) }),
            )
        });
    if let Err(error) = written {
        tracing::debug!(path = %skip_path.display(), %error, "recall skip entry write failed");
    }
    until
}

/// `JSON.stringify(value, null, 2) + "\n"` to a `0600` temp file beside
/// `path`, synced, then renamed over it.
fn write_json_atomically(path: &Path, value: &Value) -> std::io::Result<()> {
    let mut suffix = [0u8; 4];
    getrandom::fill(&mut suffix).map_err(std::io::Error::other)?;
    let suffix = hex(&suffix);
    let mut temp = path.as_os_str().to_os_string();
    temp.push(format!(".{}.{suffix}.tmp", std::process::id()));
    let temp = PathBuf::from(temp);
    let text = format!(
        "{}\n",
        serde_json::to_string_pretty(value).map_err(std::io::Error::other)?
    );
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        pa_core::platform::set_private_mode(&mut options);
        let mut file = options.open(&temp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        pa_core::platform::rename_onto(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result?;
    // The create mode is masked by the umask, so pin it after the rename.
    pa_core::platform::restrict_file(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mark_path_is_the_readable_basename_and_a_hash_of_the_root() {
        let agent = Path::new("/agent");
        // sha256("/work/my repo")[:16], as the TS product names it.
        let hash = hex(&Sha256::digest(b"/work/my repo")[..8]);
        assert_eq!(
            recall_mark_path("/work/my repo", agent),
            PathBuf::from(format!("/agent/recall/my_repo.{hash}.json"))
        );
        assert_eq!(
            recall_skip_path("/work/my repo", agent),
            PathBuf::from(format!("/agent/recall/my_repo.{hash}.skip.json"))
        );
        assert_eq!(
            recall_repo_key("/work/my repo", agent),
            format!("my_repo.{hash}")
        );
        assert!(recall_mark_path("/", agent)
            .to_string_lossy()
            .starts_with("/agent/recall/repo."));
    }

    #[test]
    fn dirty_keys_serialize_in_object_from_entries_order() {
        let entries: Vec<(String, String)> = ["b", "10", "a", "2", "02"]
            .into_iter()
            .map(|key| (key.to_string(), "d".to_string()))
            .collect();
        let keys: Vec<String> = js_object_from_entries(&entries)
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(keys, ["2", "10", "b", "a", "02"]);
    }
}
