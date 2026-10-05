//! The trust store: `<agentDir>/trusted-workspaces.json`, owner-only,
//! written atomically under the settings lock protocol.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

use crate::platform::LockDir;

/// The store's file name inside the agent dir.
pub const TRUST_STORE_FILE: &str = "trusted-workspaces.json";

const STORE_VERSION: u32 = 1;
const STALE_AFTER: Duration = Duration::from_secs(10);
const LOCK_ATTEMPTS: u32 = 10;

/// The user's answer for a workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TrustDecision {
    Trusted,
    Denied,
}

/// One workspace's recorded decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustRecord {
    pub decision: TrustDecision,
    /// `sha256:<hex>` over the gated content the decision covered.
    pub content_hash: String,
    pub decided_at_ms: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct TrustStoreDocument {
    version: u32,
    #[serde(default)]
    workspaces: BTreeMap<String, TrustRecord>,
}

pub(super) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

fn parse(content: &str) -> Result<BTreeMap<String, TrustRecord>> {
    let document: TrustStoreDocument =
        serde_json::from_str(content).context("parse the workspace trust store")?;
    if document.version != STORE_VERSION {
        return Err(anyhow!(
            "unsupported workspace trust store version {}",
            document.version
        ));
    }
    Ok(document.workspaces)
}

/// Every record; an absent store is empty.
pub(super) fn read_all(agent_dir: &Path) -> Result<BTreeMap<String, TrustRecord>> {
    let path = super::store_path(agent_dir);
    match std::fs::read_to_string(&path) {
        Ok(content) => parse(&content),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

/// One workspace's record; any read or parse failure reads as no record,
/// so a damaged store leaves every workspace untrusted.
pub(super) fn read_record(agent_dir: &Path, workspace: &Path) -> Option<TrustRecord> {
    read_all(agent_dir)
        .ok()?
        .remove(&workspace.display().to_string())
}

fn acquire_lock(path: &Path) -> Result<LockDir> {
    let mut attempts = 0;
    loop {
        attempts += 1;
        match LockDir::acquire(path, STALE_AFTER) {
            Ok(guard) => return Ok(guard),
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock && attempts < LOCK_ATTEMPTS =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                return Err(error).with_context(|| format!("lock {}", path.display()));
            }
        }
    }
}

/// Read-modify-write under the store lock. `change` returns whether it
/// changed anything; an unchanged store is not rewritten. A store that
/// fails to parse is never overwritten.
pub(super) fn update(
    agent_dir: &Path,
    change: impl FnOnce(&mut BTreeMap<String, TrustRecord>) -> bool,
) -> Result<()> {
    std::fs::create_dir_all(agent_dir)
        .with_context(|| format!("create {}", agent_dir.display()))?;
    let path = super::store_path(agent_dir);
    let _guard = acquire_lock(&path)?;
    let mut records = read_all(agent_dir).with_context(|| {
        format!(
            "{} is unreadable; fix or remove it before changing workspace trust",
            path.display()
        )
    })?;
    if !change(&mut records) {
        return Ok(());
    }
    let document = TrustStoreDocument {
        version: STORE_VERSION,
        workspaces: records,
    };
    let content = serde_json::to_string_pretty(&document)?;
    let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let written = write_private(&temp, &content)
        .and_then(|()| crate::platform::rename_onto(&temp, &path))
        .with_context(|| format!("replace {}", path.display()));
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    written
}

/// Write the whole document to a new owner-only file and sync it.
fn write_private(path: &Path, content: &str) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    crate::platform::set_private_mode(&mut options);
    let mut file = options.open(path)?;
    file.write_all(content.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()
}
