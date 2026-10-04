//! The dream store: append-only JSONL trees plus artifact blobs (TS `store.ts`).
//!
//! Each tree is `<dir>/trees/<treeId>.jsonl` (header, node and reveal lines);
//! its artifacts are `<dir>/trees/<treeId>/blobs/<seq>.json`, so node lines stay
//! scalar-only. Experiments live beside the pool, never in it:
//! `<dir>/experiments/<id>/<arm>` is a complete store of its own. Directories
//! are created 0700 and files 0600, as the TS product created them.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::json;
use crate::policy::sha256_hex;
use crate::records::{NodeOrigin, NodeRecord, RevealRecord, TreeHeaderRecord, TreeRecord};
use crate::rng::Seed;

/// `PRIME_AGENT_DREAM_DIR`: the dream store override (tilde expanded).
pub const ENV_DREAM_DIR: &str = "PRIME_AGENT_DREAM_DIR";

/// A store failure.
#[derive(Debug, thiserror::Error)]
pub enum DreamStoreError {
    #[error("{0}")]
    Message(String),
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The run was cancelled (TS `DreamAbortError`). Only the in-session
    /// path, which holds a cancellation token, ever produces it.
    #[error("{0}")]
    Aborted(String),
}

impl DreamStoreError {
    /// Whether this is a cancellation rather than a failure.
    #[must_use]
    pub fn is_abort(&self) -> bool {
        matches!(self, Self::Aborted(_))
    }
}

impl DreamStoreError {
    pub(crate) fn io(path: &Path, source: io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            source,
        }
    }
}

/// The dream store directory: `$PRIME_AGENT_DREAM_DIR`, else `<agent-dir>/dream`.
#[must_use]
pub fn dream_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os(ENV_DREAM_DIR).filter(|dir| !dir.is_empty()) {
        return expand_tilde(&dir.to_string_lossy());
    }
    pa_types::platform::dirs::agent_dir()
        .unwrap_or_else(|| PathBuf::from(pa_types::platform::dirs::CONFIG_DIR_NAME))
        .join("dream")
}

fn expand_tilde(path: &str) -> PathBuf {
    let home = pa_types::platform::dirs::home_dir;
    if path == "~" {
        return home().unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home().map_or_else(|| PathBuf::from(path), |home| home.join(rest));
    }
    PathBuf::from(path)
}

/// `<dir>/trees`.
#[must_use]
pub fn trees_dir(dir: &Path) -> PathBuf {
    dir.join("trees")
}

/// `<dir>/trees/<treeId>.jsonl`.
#[must_use]
pub fn tree_path(tree_id: &str, dir: &Path) -> PathBuf {
    trees_dir(dir).join(format!("{tree_id}.jsonl"))
}

fn blob_dir(tree_id: &str, dir: &Path) -> PathBuf {
    trees_dir(dir).join(tree_id).join("blobs")
}

fn blob_path(tree_id: &str, seq: u32, dir: &Path) -> PathBuf {
    blob_dir(tree_id, dir).join(format!("{seq}.json"))
}

/// `<dir>/experiments`.
#[must_use]
pub fn experiments_dir(dir: &Path) -> PathBuf {
    dir.join("experiments")
}

/// `<dir>/experiments/<experimentId>`.
#[must_use]
pub fn experiment_dir(dir: &Path, experiment_id: &str) -> PathBuf {
    experiments_dir(dir).join(experiment_id)
}

/// `<dir>/experiments/<experimentId>/<arm>`: a complete store of its own.
#[must_use]
pub fn experiment_arm_dir(dir: &Path, experiment_id: &str, arm: &str) -> PathBuf {
    experiment_dir(dir, experiment_id).join(arm)
}

/// `<dir>/experiments/<experimentId>/result.json`.
#[must_use]
pub fn experiment_result_path(dir: &Path, experiment_id: &str) -> PathBuf {
    experiment_dir(dir, experiment_id).join("result.json")
}

/// Create `path` and its parents with mode 0700 (on the directories it creates).
pub(crate) fn create_dir_private(path: &Path) -> Result<(), DreamStoreError> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .map_err(|error| DreamStoreError::io(path, error))
}

fn open_private(path: &Path, append: bool) -> Result<fs::File, DreamStoreError> {
    let mut options = OpenOptions::new();
    options.create(true);
    if append {
        options.append(true);
    } else {
        options.write(true).truncate(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .map_err(|error| DreamStoreError::io(path, error))
}

/// Write `content` to `path` (created 0600, truncated if present).
pub(crate) fn write_private(path: &Path, content: &str) -> Result<(), DreamStoreError> {
    open_private(path, /*append*/ false)?
        .write_all(content.as_bytes())
        .map_err(|error| DreamStoreError::io(path, error))
}

/// Append `content` to `path` (created 0600 when absent).
pub(crate) fn append_private(path: &Path, content: &str) -> Result<(), DreamStoreError> {
    open_private(path, /*append*/ true)?
        .write_all(content.as_bytes())
        .map_err(|error| DreamStoreError::io(path, error))
}

#[cfg(unix)]
fn chmod_private(path: &Path) -> Result<(), DreamStoreError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| DreamStoreError::io(path, error))
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps)] // the unix variant can fail
fn chmod_private(_path: &Path) -> Result<(), DreamStoreError> {
    Ok(())
}

/// Experiment ids (directory names under `<dir>/experiments`), sorted; empty when none.
#[must_use]
pub fn list_experiment_ids(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(experiments_dir(dir)) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    ids.sort_by(|a, b| crate::collate::locale_compare(a, b));
    ids
}

/// Copy one recorded tree (its JSONL file and every blob) between stores,
/// byte for byte, with the store's modes.
///
/// # Errors
///
/// [`DreamStoreError`] when the tree is missing or a copy fails.
pub fn copy_tree(tree_id: &str, from_dir: &Path, to_dir: &Path) -> Result<(), DreamStoreError> {
    let source = tree_path(tree_id, from_dir);
    if !source.exists() {
        return Err(DreamStoreError::Message(format!(
            "no tree {tree_id} in {}",
            from_dir.display()
        )));
    }
    let target = tree_path(tree_id, to_dir);
    create_dir_private(&trees_dir(to_dir))?;
    fs::copy(&source, &target).map_err(|error| DreamStoreError::io(&target, error))?;
    chmod_private(&target)?;
    let Ok(entries) = fs::read_dir(blob_dir(tree_id, from_dir)) else {
        return Ok(());
    };
    let target_blobs = blob_dir(tree_id, to_dir);
    create_dir_private(&target_blobs)?;
    for entry in entries.filter_map(Result::ok) {
        let target_blob = target_blobs.join(entry.file_name());
        fs::copy(entry.path(), &target_blob)
            .map_err(|error| DreamStoreError::io(&target_blob, error))?;
        chmod_private(&target_blob)?;
    }
    Ok(())
}

/// Append-only writer for one tree file plus its blobs.
#[derive(Debug, Clone)]
pub struct TreeWriter {
    tree_id: String,
    dir: PathBuf,
}

impl TreeWriter {
    #[must_use]
    pub fn new(tree_id: &str, dir: &Path) -> Self {
        Self {
            tree_id: tree_id.to_string(),
            dir: dir.to_path_buf(),
        }
    }

    /// Create or truncate the tree file with its header line (a re-run is idempotent).
    ///
    /// # Errors
    ///
    /// [`DreamStoreError`] on a filesystem failure.
    pub fn write_header(&self, header: &TreeHeaderRecord) -> Result<(), DreamStoreError> {
        create_dir_private(&trees_dir(&self.dir))?;
        write_private(
            &tree_path(&self.tree_id, &self.dir),
            &format!("{}\n", json::stringify(header)),
        )
    }

    /// Append one node line.
    ///
    /// # Errors
    ///
    /// [`DreamStoreError`] on a filesystem failure.
    pub fn append_node(&self, node: &NodeRecord) -> Result<(), DreamStoreError> {
        append_private(
            &tree_path(&self.tree_id, &self.dir),
            &format!("{}\n", json::stringify(node)),
        )
    }

    /// Append one reveal line.
    ///
    /// # Errors
    ///
    /// [`DreamStoreError`] on a filesystem failure.
    pub fn append_reveal(&self, reveal: &RevealRecord) -> Result<(), DreamStoreError> {
        append_private(
            &tree_path(&self.tree_id, &self.dir),
            &format!("{}\n", json::stringify(reveal)),
        )
    }

    /// Persist an artifact as a canonical-JSON blob keyed by seq; returns its digest.
    ///
    /// # Errors
    ///
    /// [`DreamStoreError`] on a filesystem failure.
    pub fn write_blob(&self, seq: u32, content: &Value) -> Result<String, DreamStoreError> {
        let path = blob_path(&self.tree_id, seq, &self.dir);
        create_dir_private(&blob_dir(&self.tree_id, &self.dir))?;
        let canonical = json::canonical_json(content);
        write_private(&path, &canonical)?;
        Ok(sha256_hex(canonical.as_bytes()))
    }
}

/// A frozen, read-only tree: the replay simulator's only input.
#[derive(Debug, Clone)]
pub struct RecordedTree {
    pub header: TreeHeaderRecord,
    pub root_id: String,
    /// Every node record in seq order, the root first.
    pub nodes: Vec<NodeRecord>,
    /// Online reveal lines (informational).
    pub reveals: Vec<RevealRecord>,
    pub(crate) root_index: usize,
    by_id: HashMap<String, usize>,
    /// Child indices per node index, in seq order.
    pub(crate) children: Vec<Vec<usize>>,
    blob_dir: Option<PathBuf>,
}

impl RecordedTree {
    /// Build a frozen tree from records in memory.
    ///
    /// # Errors
    ///
    /// [`DreamStoreError`] without a header or without exactly one root.
    pub fn from_records(records: Vec<TreeRecord>) -> Result<Self, DreamStoreError> {
        Self::build(records, None)
    }

    fn build(records: Vec<TreeRecord>, blob_dir: Option<PathBuf>) -> Result<Self, DreamStoreError> {
        let mut header = None;
        let mut nodes = Vec::new();
        let mut reveals = Vec::new();
        for record in records {
            match record {
                TreeRecord::Header(found) => {
                    if header.is_none() {
                        header = Some(found);
                    }
                }
                TreeRecord::Node(node) => nodes.push(node),
                TreeRecord::Reveal(reveal) => reveals.push(reveal),
            }
        }
        let header = header
            .ok_or_else(|| DreamStoreError::Message("records have no tree header".to_string()))?;
        nodes.sort_by_key(|node| node.seq);
        let roots: Vec<usize> = nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| node.parent_id.is_none())
            .map(|(index, _)| index)
            .collect();
        if roots.len() != 1 {
            return Err(DreamStoreError::Message(format!(
                "a recorded tree needs exactly one root, found {}",
                roots.len()
            )));
        }
        let root_index = roots[0];
        let mut by_id = HashMap::with_capacity(nodes.len());
        for (index, node) in nodes.iter().enumerate() {
            by_id.insert(node.id.clone(), index);
        }
        let mut children = vec![Vec::new(); nodes.len()];
        for (index, node) in nodes.iter().enumerate() {
            if let Some(parent) = node.parent_id.as_ref().and_then(|id| by_id.get(id)) {
                children[*parent].push(index);
            }
        }
        Ok(Self {
            root_id: nodes[root_index].id.clone(),
            header,
            nodes,
            reveals,
            root_index,
            by_id,
            children,
            blob_dir,
        })
    }

    /// The node record with `id`.
    #[must_use]
    pub fn node_by_id(&self, id: &str) -> Option<&NodeRecord> {
        self.by_id.get(id).map(|index| &self.nodes[*index])
    }

    pub(crate) fn index_of(&self, id: &str) -> Option<usize> {
        self.by_id.get(id).copied()
    }

    /// Recorded children of a node, in seq order.
    #[must_use]
    pub fn children_of(&self, id: &str) -> Vec<&NodeRecord> {
        self.index_of(id)
            .map(|index| {
                self.children[index]
                    .iter()
                    .map(|child| &self.nodes[*child])
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Load the full artifact of a node from its blob.
    ///
    /// # Errors
    ///
    /// [`DreamStoreError`] for an in-memory tree, a missing blob or bad JSON.
    pub fn load_blob(&self, node: &NodeRecord) -> Result<Value, DreamStoreError> {
        let Some(dir) = &self.blob_dir else {
            return Err(DreamStoreError::Message(format!(
                "no blob loader configured for node {}",
                node.id
            )));
        };
        let path = dir.join(format!("{}.json", node.seq));
        let text = fs::read_to_string(&path).map_err(|error| DreamStoreError::io(&path, error))?;
        json::parse(&text)
            .map_err(|error| DreamStoreError::Message(format!("{}: {error}", path.display())))
    }
}

/// Parse a tree file's lines; lines of other types are skipped.
///
/// # Errors
///
/// [`DreamStoreError`] on an unparsable line.
pub fn parse_records(content: &str) -> Result<Vec<TreeRecord>, DreamStoreError> {
    let unparsable = || DreamStoreError::Message("tree file has an unparsable line".to_string());
    let mut records = Vec::new();
    for line in content.split('\n') {
        if line.is_empty() {
            continue;
        }
        let value = json::parse(line).map_err(|_| unparsable())?;
        let record = match value.get("type").and_then(Value::as_str) {
            Some("tree") => {
                TreeRecord::Header(serde_json::from_value(value).map_err(|_| unparsable())?)
            }
            Some("node") => {
                TreeRecord::Node(serde_json::from_value(value).map_err(|_| unparsable())?)
            }
            Some("reveal") => {
                TreeRecord::Reveal(serde_json::from_value(value).map_err(|_| unparsable())?)
            }
            _ => continue,
        };
        records.push(record);
    }
    Ok(records)
}

/// Read a persisted tree with a blob loader over `<treeId>/blobs/<seq>.json`.
///
/// # Errors
///
/// [`DreamStoreError`] when the tree is missing or malformed.
pub fn read_tree(tree_id: &str, dir: &Path) -> Result<RecordedTree, DreamStoreError> {
    let path = tree_path(tree_id, dir);
    if !path.exists() {
        return Err(DreamStoreError::Message(format!(
            "no tree {tree_id} in the dream store"
        )));
    }
    let content = fs::read_to_string(&path).map_err(|error| DreamStoreError::io(&path, error))?;
    RecordedTree::build(parse_records(&content)?, Some(blob_dir(tree_id, dir)))
}

/// One tree's summary for `status` and the pool freeze.
#[derive(Debug, Clone, PartialEq)]
pub struct TreeSummary {
    pub tree_id: String,
    pub task_id: String,
    pub w: u32,
    pub seed: Seed,
    pub policy_id: String,
    pub iteration: u32,
    pub created_ts: u64,
    pub node_count: usize,
    /// Nodes a child agent generated (`origin: "llm"`).
    pub agent_generated_count: usize,
    pub best_score: f64,
}

/// Summaries of every tree in the store, sorted by tree id; unreadable files are skipped.
#[must_use]
pub fn list_trees(dir: &Path) -> Vec<TreeSummary> {
    let Ok(entries) = fs::read_dir(trees_dir(dir)) else {
        return Vec::new();
    };
    let mut summaries = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if Path::new(&name)
            .extension()
            .is_none_or(|extension| extension != "jsonl")
        {
            continue;
        }
        let Ok(content) = fs::read_to_string(entry.path()) else {
            continue;
        };
        let Ok(records) = parse_records(&content) else {
            continue;
        };
        let mut header = None;
        let mut node_count = 0;
        let mut agent_generated_count = 0;
        let mut best: Option<f64> = None;
        for record in &records {
            match record {
                TreeRecord::Header(found) => {
                    if header.is_none() {
                        header = Some(found);
                    }
                }
                TreeRecord::Node(node) => {
                    node_count += 1;
                    if node.origin() == NodeOrigin::Llm {
                        agent_generated_count += 1;
                    }
                    if node.valid
                        && node.score.is_finite()
                        && best.is_none_or(|current| node.score > current)
                    {
                        best = Some(node.score);
                    }
                }
                TreeRecord::Reveal(_) => {}
            }
        }
        let Some(header) = header else {
            continue;
        };
        summaries.push(TreeSummary {
            tree_id: header.tree_id.clone(),
            task_id: header.task_id.clone(),
            w: header.w,
            seed: header.seed.clone(),
            policy_id: header.policy_id.clone(),
            iteration: header.iteration,
            created_ts: header.created_ts,
            node_count,
            agent_generated_count,
            best_score: best.unwrap_or(0.0),
        });
    }
    summaries.sort_by(|a, b| crate::collate::locale_compare(&a.tree_id, &b.tree_id));
    summaries
}
