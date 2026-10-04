//! Workspace capture: HEAD, a digest of the index, a digest per dirty
//! path, and the skip-worktree paths absent from disk. Nothing here keeps
//! file content; every value that leaves this module is a digest or a path.
//!
//! The digest is sha256 truncated to 128 bits, and mark files name it
//! `sha256-128` (the TS product's algorithm; byte-compatible).

use std::collections::{HashMap, HashSet};
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

use serde_json::{json, Map, Value};
use sha2::{Digest as _, Sha256};

use crate::git::{run_git, GitFailure};

pub const RECALL_DIGEST_ALGORITHM: &str = "sha256-128";
pub const RECALL_UNVERIFIABLE: &str = "unverifiable";
pub const RECALL_MAX_DIRTY_PATHS: usize = 200;
pub const RECALL_MAX_FILE_BYTES: u64 = 8 * 1024 * 1024;
pub const RECALL_MAX_HASHED_BYTES: u64 = 64 * 1024 * 1024;
/// More skip-worktree entries than this are not checked for presence on
/// disk, so none of them is hashed.
pub const RECALL_MAX_SKIP_WORKTREE_CHECKS: usize = 1000;
/// More skip-worktree and assume-unchanged paths than this (core.ignoreStat,
/// sparse checkout) are counted, never hashed.
pub const RECALL_MAX_TAGGED_PATHS: usize = 100;

const BLOB_COMPARE_BATCH_SIZE: usize = 64;

/// Lowercase hex of `bytes`.
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut text, byte| {
            let _ = write!(text, "{byte:02x}");
            text
        })
}

/// sha256 truncated to 128 bits, as 32 lowercase hex digits.
#[must_use]
pub fn recall_digest(data: &[u8]) -> String {
    hex(&Sha256::digest(data)[..16])
}

/// Digest of "nothing at this path", so a deletion recorded at mark time
/// compares equal to the same deletion later.
pub static RECALL_ABSENT_DIGEST: LazyLock<String> =
    LazyLock::new(|| recall_digest(b"\0prime-agent-recall:absent"));

/// Order two strings by UTF-16 code units, the TS product's `<`.
pub(crate) fn compare_utf16(a: &str, b: &str) -> std::cmp::Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

pub(crate) fn sort_utf16(paths: &mut [String]) {
    paths.sort_by(|a, b| compare_utf16(a, b));
}

/// Skip-worktree entries missing on disk, such as a sparse checkout's
/// excluded paths. Names only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbsentSkipWorktree {
    pub count: usize,
    /// [`absent_skip_worktree_digest`] of every name, listed or not.
    pub digest: String,
    /// Sorted, while there are at most [`RECALL_MAX_TAGGED_PATHS`]; past
    /// that they are counted in `dirty_overflow` instead.
    pub paths: Option<Vec<String>>,
}

impl AbsentSkipWorktree {
    pub(crate) fn to_json(&self) -> Value {
        let mut value = json!({ "count": self.count, "digest": self.digest });
        if let Some(paths) = &self.paths {
            value["paths"] = json!(paths);
        }
        value
    }
}

/// What a mark knows about absent skip-worktree entries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AbsentPresence {
    /// A mark written before presence was recorded (the field is missing).
    NotRecorded,
    /// Presence was not checked: more than
    /// [`RECALL_MAX_SKIP_WORKTREE_CHECKS`] entries, all counted in
    /// `dirty_overflow` (`null` on disk).
    Unchecked,
    Checked(AbsentSkipWorktree),
}

impl AbsentPresence {
    #[must_use]
    pub fn checked(&self) -> Option<&AbsentSkipWorktree> {
        match self {
            AbsentPresence::Checked(absent) => Some(absent),
            AbsentPresence::NotRecorded | AbsentPresence::Unchecked => None,
        }
    }

    /// The absent paths this presence names (none unless checked and listed).
    #[must_use]
    pub fn listed_paths(&self) -> &[String] {
        self.checked()
            .and_then(|absent| absent.paths.as_deref())
            .unwrap_or(&[])
    }
}

/// The recorded part of a workspace: what a mark persists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceState {
    pub head: Option<String>,
    pub tracked_tree_digest: String,
    /// Repo-relative path to a content digest, [`RECALL_ABSENT_DIGEST`], or
    /// [`RECALL_UNVERIFIABLE`], in recording order.
    pub dirty: Vec<(String, String)>,
    /// Paths that needed a digest but were neither hashed nor recorded.
    pub dirty_overflow: usize,
    pub absent: AbsentPresence,
}

impl WorkspaceState {
    #[must_use]
    pub fn dirty_digest(&self, path: &str) -> Option<&str> {
        self.dirty
            .iter()
            .find(|(dirty, _)| dirty == path)
            .map(|(_, digest)| digest.as_str())
    }
}

/// A live capture: the recorded state plus what is counted, never persisted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSnapshot {
    pub repo_root: String,
    pub state: WorkspaceState,
    /// Paths in the index now.
    pub tracked_paths: HashSet<String>,
    /// Dirty paths beyond [`RECALL_MAX_DIRTY_PATHS`], unverifiable by construction.
    pub overflow_paths: Vec<String>,
    /// Skip-worktree and assume-unchanged paths that were hashed.
    pub tagged_paths: HashSet<String>,
    /// Tagged paths left unhashed, and absent skip-worktree entries left
    /// unlisted, because there were too many; counted in `dirty_overflow`.
    pub unhashed_tagged_paths: Vec<String>,
    /// Repo-relative agent dir left out of the capture, so recall's own
    /// writes are never workspace changes.
    pub excluded_path: Option<String>,
}

/// Why a capture produced no snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum CaptureFailure {
    #[error("git_unavailable")]
    GitUnavailable,
    #[error("git_timeout")]
    GitTimeout,
    #[error("not_repo")]
    NotRepo,
}

impl From<GitFailure> for CaptureFailure {
    fn from(failure: GitFailure) -> Self {
        match failure {
            GitFailure::Unavailable => CaptureFailure::GitUnavailable,
            GitFailure::Timeout => CaptureFailure::GitTimeout,
        }
    }
}

#[must_use]
pub fn absent_skip_worktree_digest(paths: &[String]) -> String {
    recall_digest(json!(paths).to_string().as_bytes())
}

/// One digest over everything a claim depends on; equal digests of fully
/// verifiable states mean an identical tracked and non-ignored tree,
/// skip-worktree and assume-unchanged entries included, with the same
/// skip-worktree entries absent from disk. Byte-compatible with the TS
/// product's `JSON.stringify` input.
#[must_use]
pub fn workspace_digest(state: &WorkspaceState) -> String {
    let mut dirty: Vec<&(String, String)> = state.dirty.iter().collect();
    dirty.sort_by(|(a, _), (b, _)| compare_utf16(a, b));
    let mut object = Map::new();
    object.insert("head".into(), json!(state.head));
    object.insert("trackedTreeDigest".into(), json!(state.tracked_tree_digest));
    object.insert(
        "dirty".into(),
        Value::Array(
            dirty
                .into_iter()
                .map(|(path, digest)| json!([path, digest]))
                .collect(),
        ),
    );
    object.insert("dirtyOverflow".into(), json!(state.dirty_overflow));
    match &state.absent {
        // Left out for a mark from before presence was recorded, so it keeps
        // the digest its claims were made with.
        AbsentPresence::NotRecorded => {}
        AbsentPresence::Unchecked => {
            object.insert("absentSkipWorktree".into(), Value::Null);
        }
        AbsentPresence::Checked(absent) => {
            object.insert(
                "absentSkipWorktree".into(),
                json!({ "count": absent.count, "digest": absent.digest }),
            );
        }
    }
    recall_digest(Value::Object(object).to_string().as_bytes())
}

/// True when the state's digests cover every path they claim to: nothing
/// unverifiable, nothing past the limit, and every absent skip-worktree
/// entry named.
#[must_use]
pub fn is_fully_verifiable(state: &WorkspaceState) -> bool {
    state.dirty_overflow == 0
        && state
            .absent
            .checked()
            .is_some_and(|absent| absent.paths.is_some())
        && state
            .dirty
            .iter()
            .all(|(_, digest)| digest != RECALL_UNVERIFIABLE)
}

/// One `git status --porcelain=v1 -z` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEntry {
    pub xy: String,
    pub path: String,
    pub orig_path: Option<String>,
}

/// `git status --porcelain=v1 -z`: `XY path\0`, with the source path as an
/// extra field for renames and copies.
#[must_use]
pub fn parse_porcelain_status(output: &[u8]) -> Vec<StatusEntry> {
    let text = String::from_utf8_lossy(output);
    let fields: Vec<&str> = text.split('\0').collect();
    let mut entries = Vec::new();
    let mut index = 0;
    while index < fields.len() {
        let field = fields[index];
        index += 1;
        if field.encode_utf16().count() < 4 || field.as_bytes().get(2) != Some(&b' ') {
            continue;
        }
        let xy = field[..2].to_string();
        let mut entry = StatusEntry {
            path: field[3..].to_string(),
            orig_path: None,
            xy,
        };
        if entry.xy.contains(['R', 'C']) {
            if let Some(orig) = fields.get(index).filter(|orig| !orig.is_empty()) {
                entry.orig_path = Some((*orig).to_string());
            }
            index += 1;
        }
        entries.push(entry);
    }
    entries
}

/// The index as `git ls-files -s -v -z` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexListing {
    /// Paths in the index; unmerged stages collapse to one path.
    pub paths: HashSet<String>,
    /// Digest of the listing with its tags removed: byte for byte the
    /// digest of `git ls-files -s -z`.
    pub digest: String,
    /// Tag `S`: git status never looks at these paths.
    pub skip_worktree: Vec<String>,
    /// Lowercase tag: git status trusts the index for these paths.
    pub assume_unchanged: Vec<String>,
}

/// `git ls-files -s -v -z`: `T mode object stage\tpath\0`, where T is the
/// one-letter tag.
#[must_use]
pub fn parse_index_listing(output: &[u8]) -> IndexListing {
    let mut hash = Sha256::new();
    let mut listing = IndexListing {
        paths: HashSet::new(),
        digest: String::new(),
        skip_worktree: Vec::new(),
        assume_unchanged: Vec::new(),
    };
    let mut start = 0;
    while start < output.len() {
        let nul = output[start..]
            .iter()
            .position(|&byte| byte == 0)
            .map(|offset| start + offset);
        let end = nul.unwrap_or(output.len());
        let record = &output[start..end];
        let tagged = record.len() >= 2 && record[1] == b' ';
        hash.update(if tagged { &record[2..] } else { record });
        if nul.is_some() {
            hash.update([0u8]);
        }
        let text = String::from_utf8_lossy(record);
        if let Some(tab) = text.find('\t') {
            let path = &text[tab + 1..];
            if !path.is_empty() && listing.paths.insert(path.to_string()) {
                let tag = if tagged { record[0] } else { 0 };
                if tag.is_ascii_lowercase() {
                    listing.assume_unchanged.push(path.to_string());
                } else if tag == b'S' {
                    listing.skip_worktree.push(path.to_string());
                }
            }
        }
        start = end + 1;
    }
    let digest = hash.finalize();
    listing.digest = hex(&digest[..16]);
    listing
}

fn is_missing(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

fn open_for_digest(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    // O_NONBLOCK keeps a path swapped for a FIFO after the lstat from
    // blocking the open; O_NOFOLLOW refuses a path swapped for a symlink.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW);
    }
    options.open(path)
}

/// Digest one repo-relative path. `absent_is_state` says a missing file is
/// the expected state (a deletion) rather than a path that vanished between
/// the listing and the read. Directories, special files, oversized or
/// unreadable files, and anything past the byte budget are unverifiable:
/// the digest is either of real bytes or it is not recorded at all.
#[must_use]
pub fn digest_recall_path(
    repo_root: &Path,
    relative_path: &str,
    absent_is_state: bool,
    budget: &mut u64,
) -> String {
    let full_path = repo_root.join(relative_path);
    let metadata = match std::fs::symlink_metadata(&full_path) {
        Ok(metadata) => metadata,
        Err(error) if is_missing(&error) && absent_is_state => return RECALL_ABSENT_DIGEST.clone(),
        Err(_) => return RECALL_UNVERIFIABLE.to_string(),
    };
    if metadata.file_type().is_symlink() {
        return match std::fs::read_link(&full_path) {
            Ok(target) => {
                recall_digest(format!("\0symlink:{}", target.to_string_lossy()).as_bytes())
            }
            Err(_) => RECALL_UNVERIFIABLE.to_string(),
        };
    }
    let within_limits = |size: u64| size <= RECALL_MAX_FILE_BYTES && size <= *budget;
    if !metadata.is_file() || !within_limits(metadata.len()) {
        return RECALL_UNVERIFIABLE.to_string();
    }
    let Ok(file) = open_for_digest(&full_path) else {
        return RECALL_UNVERIFIABLE.to_string();
    };
    match file.metadata() {
        Ok(opened) if opened.is_file() && within_limits(opened.len()) => {}
        Ok(_) | Err(_) => return RECALL_UNVERIFIABLE.to_string(),
    }
    let mut bytes = Vec::new();
    if file
        .take(RECALL_MAX_FILE_BYTES.min(*budget) + 1)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return RECALL_UNVERIFIABLE.to_string();
    }
    let read = bytes.len() as u64;
    if !within_limits(read) {
        return RECALL_UNVERIFIABLE.to_string();
    }
    *budget -= read;
    recall_digest(&bytes)
}

/// Digest `targets` in order on the blocking pool, one path per task, so a
/// dropped capture (a missed deadline) stops between paths.
async fn digest_paths(repo_root: &Path, targets: Vec<(String, bool)>) -> Vec<(String, String)> {
    let mut budget = RECALL_MAX_HASHED_BYTES;
    let mut digests = Vec::with_capacity(targets.len());
    for (path, absent_is_state) in targets {
        let root = repo_root.to_path_buf();
        let task = tokio::task::spawn_blocking(move || {
            let digest = digest_recall_path(&root, &path, absent_is_state, &mut budget);
            (path, digest, budget)
        });
        match task.await {
            Ok((path, digest, remaining)) => {
                budget = remaining;
                digests.push((path, digest));
            }
            Err(_) => return digests,
        }
    }
    digests
}

fn is_deletion(xy: &str) -> bool {
    let bytes = xy.as_bytes();
    bytes.get(1) == Some(&b'D') || (bytes.first() == Some(&b'D') && bytes.get(1) == Some(&b' '))
}

/// The deepest existing ancestor resolved through symlinks, with the
/// missing tail appended.
fn realpath_or_resolve(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    let mut current = absolute.clone();
    loop {
        if let Ok(resolved) = std::fs::canonicalize(&current) {
            return missing
                .iter()
                .rev()
                .fold(resolved, |path, part| path.join(part));
        }
        match (current.parent(), current.file_name()) {
            (Some(parent), Some(name)) => {
                missing.push(name.to_os_string());
                current = parent.to_path_buf();
            }
            _ => return absolute,
        }
    }
}

/// The agent dir relative to `repo_root`, with `/` separators, when it
/// lives inside the worktree: `""` when it is the worktree itself, `None`
/// when it is outside.
#[must_use]
pub fn recall_excluded_path(repo_root: &Path, agent_dir: Option<&Path>) -> Option<String> {
    let agent_dir = agent_dir?;
    let root = realpath_or_resolve(repo_root);
    let dir = realpath_or_resolve(agent_dir);
    let relative = dir.strip_prefix(&root).ok()?;
    let parts: Vec<String> = relative
        .components()
        .map(|component| match component {
            Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
            Component::Prefix(_)
            | Component::RootDir
            | Component::CurDir
            | Component::ParentDir => None,
        })
        .collect::<Option<_>>()?;
    Some(parts.join("/"))
}

#[must_use]
pub fn is_recall_excluded(path: &str, excluded_path: Option<&str>) -> bool {
    excluded_path.is_some_and(|excluded| {
        path == excluded
            || path
                .strip_prefix(excluded)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

fn exclude_pathspec(excluded_path: Option<&str>) -> Vec<String> {
    excluded_path.map_or_else(Vec::new, |excluded| {
        vec![
            "--".to_string(),
            ".".to_string(),
            format!(":(exclude,literal){excluded}"),
        ]
    })
}

/// Skip-worktree paths split by presence on disk: present ones are hashed,
/// absent ones are recorded by name.
async fn split_skip_worktree_by_presence(
    repo_root: &Path,
    paths: Vec<String>,
) -> (Vec<String>, Vec<String>) {
    let root = repo_root.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let mut present = Vec::new();
        let mut absent = Vec::new();
        for path in paths {
            let exists = match std::fs::symlink_metadata(root.join(&path)) {
                Ok(_) => true,
                Err(error) => !is_missing(&error),
            };
            if exists {
                present.push(path);
            } else {
                absent.push(path);
            }
        }
        (present, absent)
    })
    .await
    .unwrap_or_default()
}

/// Snapshot the workspace at `repo_root`. Fails whenever git cannot answer
/// or disagrees with discovery about the toplevel: no mark and no block is
/// always the safe outcome, never a partial snapshot. Dropping the future
/// kills outstanding git children and stops hashing.
///
/// # Errors
///
/// The [`CaptureFailure`] that stopped the capture.
#[allow(clippy::too_many_lines)] // one pass over the TS capture, kept together for review against it
pub async fn capture_workspace(
    repo_root: &str,
    agent_dir: Option<&Path>,
) -> Result<WorkspaceSnapshot, CaptureFailure> {
    let root = Path::new(repo_root);
    let toplevel = run_git(root, &["rev-parse", "--show-toplevel"]).await?;
    let toplevel = String::from_utf8_lossy(&toplevel).trim().to_string();
    let same_root = !toplevel.is_empty()
        && matches!(
            (std::fs::canonicalize(&toplevel), std::fs::canonicalize(root)),
            (Ok(a), Ok(b)) if a == b
        );
    if !same_root {
        tracing::debug!(
            repo_root,
            toplevel,
            "git toplevel differs from the recall repo; recall skipped"
        );
        return Err(CaptureFailure::NotRepo);
    }
    let excluded_path = recall_excluded_path(root, agent_dir);
    if excluded_path.as_deref() == Some("") {
        tracing::debug!(repo_root, "the agent dir is the repo root; recall skipped");
        return Err(CaptureFailure::NotRepo);
    }
    let pathspec = exclude_pathspec(excluded_path.as_deref());
    let pathspec: Vec<&str> = pathspec.iter().map(String::as_str).collect();
    let index_args = [&["ls-files", "-s", "-v", "-z"][..], &pathspec].concat();
    let status_args = [
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"][..],
        &pathspec,
    ]
    .concat();
    let (head_output, index, status) = tokio::join!(
        run_git(root, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]),
        run_git(root, &index_args),
        run_git(root, &status_args),
    );
    let index = index?;
    let status = status?;
    // An unborn HEAD makes rev-parse exit non-zero; only a timeout fails the capture.
    let head = match head_output {
        Ok(output) => Some(String::from_utf8_lossy(&output).trim().to_string())
            .filter(|head| !head.is_empty()),
        Err(GitFailure::Unavailable) => None,
        Err(GitFailure::Timeout) => return Err(CaptureFailure::GitTimeout),
    };
    let listing = parse_index_listing(&index);

    let mut targets: Vec<(String, bool)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut add_target = |path: &str, absent_is_state: bool, targets: &mut Vec<(String, bool)>| {
        if seen.insert(path.to_string()) {
            targets.push((path.to_string(), absent_is_state));
        }
    };
    for entry in parse_porcelain_status(&status) {
        add_target(&entry.path, is_deletion(&entry.xy), &mut targets);
        if let Some(orig) = &entry.orig_path {
            add_target(orig, true, &mut targets);
        }
    }
    // git status reports no edit to either kind of entry, so hash them like
    // dirty paths while there are few of them.
    let skip_worktree_unchecked = listing.skip_worktree.len() > RECALL_MAX_SKIP_WORKTREE_CHECKS;
    let (present, absent) = if skip_worktree_unchecked {
        (listing.skip_worktree.clone(), None)
    } else {
        let (present, absent) =
            split_skip_worktree_by_presence(root, listing.skip_worktree.clone()).await;
        (present, Some(absent))
    };
    let tagged: Vec<String> = listing
        .assume_unchanged
        .iter()
        .chain(&present)
        .cloned()
        .collect();
    let in_targets = |path: &String, targets: &[(String, bool)]| {
        targets.iter().any(|(target, _)| target == path)
    };
    let mut tagged_paths = HashSet::new();
    let mut unhashed_tagged_paths: Vec<String> = tagged
        .iter()
        .filter(|path| !in_targets(path, &targets))
        .cloned()
        .collect();
    if skip_worktree_unchecked || unhashed_tagged_paths.len() > RECALL_MAX_TAGGED_PATHS {
        tracing::debug!(
            count = unhashed_tagged_paths.len(),
            "too many skip-worktree or assume-unchanged paths to hash; counting them unverifiable"
        );
    } else {
        unhashed_tagged_paths.clear();
        for path in &tagged {
            tagged_paths.insert(path.clone());
            add_target(path, true, &mut targets);
        }
    }
    // Which entries a sparse checkout left off disk is part of the
    // workspace: set, add or disable moves it with no status change.
    let absent = match absent {
        None => AbsentPresence::Unchecked,
        Some(mut absent) => {
            sort_utf16(&mut absent);
            let mut recorded = AbsentSkipWorktree {
                count: absent.len(),
                digest: absent_skip_worktree_digest(&absent),
                paths: None,
            };
            if absent.len() <= RECALL_MAX_TAGGED_PATHS {
                recorded.paths = Some(absent);
            } else {
                tracing::debug!(
                    count = absent.len(),
                    "too many absent skip-worktree paths to list; counting them unverifiable"
                );
                unhashed_tagged_paths.extend(
                    absent
                        .into_iter()
                        .filter(|path| !in_targets(path, &targets)),
                );
            }
            AbsentPresence::Checked(recorded)
        }
    };

    let overflow_paths: Vec<String> = targets
        .iter()
        .skip(RECALL_MAX_DIRTY_PATHS)
        .map(|(path, _)| path.clone())
        .collect();
    targets.truncate(RECALL_MAX_DIRTY_PATHS);
    let dirty = digest_paths(root, targets).await;
    Ok(WorkspaceSnapshot {
        repo_root: repo_root.to_string(),
        state: WorkspaceState {
            head,
            tracked_tree_digest: listing.digest,
            dirty,
            dirty_overflow: overflow_paths.len() + unhashed_tagged_paths.len(),
            absent,
        },
        tracked_paths: listing.paths,
        overflow_paths,
        tagged_paths,
        unhashed_tagged_paths,
        excluded_path,
    })
}

/// Paths changed between two commits, or `None` when git cannot list them
/// (unknown or pruned commit, shallow clone).
pub async fn committed_changes_between(
    repo_root: &Path,
    from_head: &str,
    to_head: &str,
    excluded_path: Option<&str>,
) -> Option<HashSet<String>> {
    let mut args = vec![
        "diff",
        "--name-only",
        "--no-renames",
        "-z",
        from_head,
        to_head,
        "--",
    ];
    let exclude = excluded_path.map(|excluded| format!(":(exclude,literal){excluded}"));
    if let Some(exclude) = &exclude {
        args.push(".");
        args.push(exclude);
    }
    let output = run_git(repo_root, &args).await.ok()?;
    Some(
        String::from_utf8_lossy(&output)
            .split('\0')
            .filter(|path| !path.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

const REGULAR_FILE_MODES: [&str; 2] = ["100644", "100755"];

/// The paths among `paths` that are regular files whose working content,
/// as `git hash-object` cleans it, is exactly the blob `commit` holds for
/// them. Anything git cannot answer for is left out.
pub async fn paths_matching_commit(
    repo_root: &Path,
    commit: &str,
    paths: Vec<String>,
) -> HashSet<String> {
    let root = repo_root.to_path_buf();
    let regular: Vec<String> = tokio::task::spawn_blocking(move || {
        paths
            .into_iter()
            .filter(|path| {
                std::fs::symlink_metadata(root.join(path)).is_ok_and(|metadata| metadata.is_file())
            })
            .collect()
    })
    .await
    .unwrap_or_default();
    let mut matching = HashSet::new();
    for batch in regular.chunks(BLOB_COMPARE_BATCH_SIZE) {
        let batch_args: Vec<&str> = batch.iter().map(String::as_str).collect();
        let tree_args = [
            &["--literal-pathspecs", "ls-tree", "-z", commit, "--"][..],
            &batch_args,
        ]
        .concat();
        let hash_args = [&["hash-object", "--"][..], &batch_args].concat();
        let (tree, object_ids) = tokio::join!(
            run_git(repo_root, &tree_args),
            run_git(repo_root, &hash_args)
        );
        let (Ok(tree), Ok(object_ids)) = (tree, object_ids) else {
            continue;
        };
        let hash_text = String::from_utf8_lossy(&object_ids);
        let hashes: Vec<&str> = hash_text.split('\n').take(batch.len()).collect();
        if hashes.len() != batch.len() {
            continue;
        }
        let tree = String::from_utf8_lossy(&tree);
        let mut blobs: HashMap<&str, &str> = HashMap::new();
        for record in tree.split('\0') {
            let Some((meta, path)) = record.split_once('\t') else {
                continue;
            };
            let mut fields = meta.split(' ');
            if let (Some(mode), Some("blob"), Some(object)) =
                (fields.next(), fields.next(), fields.next())
            {
                if REGULAR_FILE_MODES.contains(&mode) && !object.is_empty() {
                    blobs.insert(path, object);
                }
            }
        }
        for (path, hash) in batch.iter().zip(hashes) {
            if blobs.get(path.as_str()) == Some(&hash.trim()) {
                matching.insert(path.clone());
            }
        }
    }
    matching
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_index_digest_ignores_tags_and_tags_sort_paths() {
        let tagged =
            b"H 100644 aaa 0\tplain.txt\0S 100644 bbb 0\tskip.txt\0h 100644 ccc 0\tassume.txt\0";
        let untagged =
            b"100644 aaa 0\tplain.txt\x00100644 bbb 0\tskip.txt\x00100644 ccc 0\tassume.txt\x00";
        let listing = parse_index_listing(tagged);
        assert_eq!(
            listing,
            IndexListing {
                paths: ["plain.txt", "skip.txt", "assume.txt"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
                digest: recall_digest(untagged),
                skip_worktree: vec!["skip.txt".to_string()],
                assume_unchanged: vec!["assume.txt".to_string()],
            }
        );
    }

    #[test]
    fn porcelain_status_pairs_renames_with_their_source() {
        assert_eq!(
            parse_porcelain_status(b"R  new.txt\0old.txt\0?? u.txt\0 D gone.txt\0"),
            vec![
                StatusEntry {
                    xy: "R ".to_string(),
                    path: "new.txt".to_string(),
                    orig_path: Some("old.txt".to_string()),
                },
                StatusEntry {
                    xy: "??".to_string(),
                    path: "u.txt".to_string(),
                    orig_path: None,
                },
                StatusEntry {
                    xy: " D".to_string(),
                    path: "gone.txt".to_string(),
                    orig_path: None,
                },
            ]
        );
    }

    /// The workspace digest hashes the TS product's exact JSON text: the
    /// key order, the sorted dirty pairs, and an `absentSkipWorktree` key
    /// that is null when unchecked and missing for a legacy mark.
    #[test]
    fn the_workspace_digest_hashes_the_ts_json_text() {
        let mut state = WorkspaceState {
            head: None,
            tracked_tree_digest: "t".to_string(),
            dirty: vec![
                ("b".to_string(), "2".to_string()),
                ("a".to_string(), "1".to_string()),
            ],
            dirty_overflow: 0,
            absent: AbsentPresence::Unchecked,
        };
        assert_eq!(
            workspace_digest(&state),
            recall_digest(
                br#"{"head":null,"trackedTreeDigest":"t","dirty":[["a","1"],["b","2"]],"dirtyOverflow":0,"absentSkipWorktree":null}"#
            )
        );
        state.absent = AbsentPresence::NotRecorded;
        assert_eq!(
            workspace_digest(&state),
            recall_digest(
                br#"{"head":null,"trackedTreeDigest":"t","dirty":[["a","1"],["b","2"]],"dirtyOverflow":0}"#
            )
        );
        state.absent = AbsentPresence::Checked(AbsentSkipWorktree {
            count: 1,
            digest: "d".to_string(),
            paths: Some(vec!["x".to_string()]),
        });
        assert_eq!(
            workspace_digest(&state),
            recall_digest(
                br#"{"head":null,"trackedTreeDigest":"t","dirty":[["a","1"],["b","2"]],"dirtyOverflow":0,"absentSkipWorktree":{"count":1,"digest":"d"}}"#
            )
        );
    }

    #[test]
    fn an_excluded_path_covers_itself_and_what_is_under_it() {
        assert!(is_recall_excluded(".prime/agent", Some(".prime/agent")));
        assert!(is_recall_excluded(".prime/agent/x", Some(".prime/agent")));
        assert!(!is_recall_excluded(".prime/agents", Some(".prime/agent")));
        assert!(!is_recall_excluded("a", None));
    }
}
