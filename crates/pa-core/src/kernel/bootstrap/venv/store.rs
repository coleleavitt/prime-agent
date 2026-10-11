//! The content-addressed venv store: one kernel venv per runtime identity,
//! `<agent dir>/kernel-venvs/<key>/`, so binaries carrying different
//! runtimes (an installed release and a dev build, a default and a
//! `--no-default-features` build) each keep their own venv instead of
//! rebuilding one shared venv on every switch.
//!
//! The key hashes everything a venv's base install is judged by (the
//! bootstrap schema, the runtime identity, the snapshot engine, the default
//! extras, the Python version), so a venv under a key never needs a base
//! rebuild: only skill syncs, which stay per venv.
//!
//! `<agent dir>/kernel-venv`, the old single venv, becomes a symlink to the
//! venv most recently booted (Unix): tools, docs and tests that name that
//! path keep working, and an old binary that rebuilds it replaces only the
//! link (`remove_dir_all` does not follow it). An old venv there is adopted
//! once, by rename, when its recorded identity is the current one.
//!
//! Pruning follows the runtime bundles' policy: the current venv, the
//! [`KEEP_NEWEST`] most recently used others, any used within
//! [`KEEP_RECENT`], the link's target, and any a live process holds (a
//! lease written by every process that resolves the venv, or, on Linux, a
//! process running from it) are kept.

use std::collections::HashSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use sha2::Digest;

use super::version::{BOOTSTRAP_SCHEMA, STATE_SNAPSHOT_REQUIREMENT};
use super::{PYTHON_VERSION, default_rlm_extra_uv_args};
use crate::kernel::bootstrap::dir_lock::{
    is_owner_alive,
    owner_content,
    parse_owner,
    try_bootstrap_lock,
};

/// The keyed venvs directory under the agent dir.
pub(crate) const VENVS_DIR: &str = "kernel-venvs";
/// The pre-store single venv, now the link to the most recently booted one.
pub(crate) const LEGACY_VENV_DIR: &str = "kernel-venv";
/// Hex digits of the hash that name a venv.
const KEY_LEN: usize = 20;
/// Pruning keeps this many most recently used venvs besides the current one
/// (fewer than the bundles' three: a venv is hundreds of megabytes).
pub(crate) const KEEP_NEWEST: usize = 2;
/// Pruning never removes a venv used within this window.
pub(crate) const KEEP_RECENT: Duration = Duration::from_hours(7 * 24);
/// Abandoned trash and link scratch older than this are removed.
const STALE_SCRATCH: Duration = Duration::from_hours(24);
/// Touched on every resolution: pruning keeps recently used venvs.
const LAST_USED_FILE: &str = ".last-used";
/// One file per process that resolved the venv: `<pid>`, holding the
/// owner identity the bootstrap lock records.
const LEASES_DIR: &str = ".prime-agent-leases";

/// The store key of the venv a runtime installs into.
pub(crate) fn venv_key(runtime_identity: &str) -> String {
    let mut hasher = sha2::Sha256::new();
    for part in [
        BOOTSTRAP_SCHEMA.to_string().as_str(),
        runtime_identity,
        STATE_SNAPSHOT_REQUIREMENT,
        PYTHON_VERSION,
    ]
    .into_iter()
    .chain(default_rlm_extra_uv_args())
    {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    let digest = format!("{:x}", hasher.finalize());
    digest[..KEY_LEN].to_string()
}

fn is_venv_key(name: &str) -> bool {
    name.len() == KEY_LEN && name.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A process-unique scratch name.
fn scratch_name(kind: &str, id: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!(".{kind}-{id}-{}-{nanos}", std::process::id())
}

/// What [`VenvStore::ensure`] needs from a venv's runtime: the readiness
/// checks, the skill sync and the build (the real ones probe the
/// interpreter and run uv).
pub(crate) trait VenvOps {
    /// Base install and every requested skill are current.
    fn ready(&self, venv: &Path) -> bool;
    /// The base install is current (skills may need a sync).
    fn base_ready(&self, venv: &Path) -> bool;
    /// The venv's manifest records this runtime's base install (no probe).
    fn records_this_runtime(&self, venv: &Path) -> bool;
    /// Bring the requested skills of a base-ready venv up to date.
    fn sync(&self, venv: &Path) -> impl Future<Output = anyhow::Result<()>> + Send;
    /// Build a venv at `venv`, which does not exist.
    fn build(&self, venv: &Path) -> impl Future<Output = anyhow::Result<()>> + Send;
    fn report(&self, message: &str);
}

/// The venv store under one agent dir (the managed one, or its XDG
/// fallback).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct VenvStore {
    base: PathBuf,
}

impl VenvStore {
    pub(crate) fn new(base: PathBuf) -> Self {
        Self { base }
    }

    /// `<base>/kernel-venvs`.
    pub(crate) fn root(&self) -> PathBuf {
        self.base.join(VENVS_DIR)
    }

    /// `<base>/kernel-venv`: the link (or an old venv not yet adopted).
    pub(crate) fn legacy(&self) -> PathBuf {
        self.base.join(LEGACY_VENV_DIR)
    }

    pub(crate) fn venv(&self, key: &str) -> PathBuf {
        self.root().join(key)
    }

    /// Every keyed venv directory in the store.
    pub(crate) fn venvs(&self) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(self.root()) else {
            return Vec::new();
        };
        let mut venvs: Vec<PathBuf> = entries
            .flatten()
            .filter(|entry| is_venv_key(&entry.file_name().to_string_lossy()))
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        venvs.sort();
        venvs
    }

    /// The venv under `key`, ready: reused as is, else (under its bootstrap
    /// lock, so concurrent first boots build once) synced, adopted from the
    /// legacy venv, or built. A venv under another key is never touched.
    /// Afterwards the legacy link names it.
    pub(crate) async fn ensure(&self, key: &str, ops: &impl VenvOps) -> anyhow::Result<PathBuf> {
        let venv = self.venv(key);
        // The lease goes down before the readiness check, so a concurrent
        // prune that has not looked yet sees this process's use.
        if venv.is_dir() {
            Self::mark_in_use(&venv);
        }
        if !ops.ready(&venv) {
            let lock = crate::kernel::bootstrap::dir_lock::acquire_bootstrap_lock(&venv).await;
            let built = async {
                if ops.ready(&venv) {
                    return Ok(());
                }
                if !ops.base_ready(&venv)
                    && self
                        .adopt_legacy(&venv, |legacy| ops.records_this_runtime(legacy))
                        .await
                {
                    ops.report("moved the kernel venv into the per-runtime store");
                }
                if ops.base_ready(&venv) {
                    return ops.sync(&venv).await;
                }
                ops.report("› setting up python kernel (one-time, ~30s)…");
                if venv.exists() {
                    // Half built (an interrupted first boot) or damaged:
                    // never this runtime's working venv, which would have
                    // been ready.
                    self.discard(&venv).map_err(|error| {
                        anyhow::anyhow!("moving aside {}: {error}", venv.display())
                    })?;
                }
                ops.build(&venv).await
            }
            .await;
            drop(lock);
            ops.report("✓ ready");
            built?;
        }
        Self::mark_in_use(&venv);
        self.point_legacy_at(&venv);
        Ok(venv)
    }

    /// Record that this process uses `venv`: refresh its last-used mark and
    /// (once per process) write this process's lease. Called before the
    /// readiness check, so a concurrent prune sees the use.
    pub(crate) fn mark_in_use(venv: &Path) {
        static LEASED: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);
        let leases = venv.join(LEASES_DIR);
        if std::fs::create_dir_all(&leases).is_err() {
            return;
        }
        let marker = venv.join(LAST_USED_FILE);
        let touched = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&marker)
            .and_then(|file| file.set_modified(SystemTime::now()));
        if let Err(error) = touched {
            tracing::debug!(marker = %marker.display(), %error, "venv last-used mark failed");
        }
        let mut guard = LEASED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let written = guard.get_or_insert_with(HashSet::new);
        let lease = leases.join(std::process::id().to_string());
        if written.contains(venv) && lease.is_file() {
            return;
        }
        match std::fs::write(&lease, owner_content()) {
            Ok(()) => {
                written.insert(venv.to_path_buf());
            }
            Err(error) => {
                tracing::debug!(lease = %lease.display(), %error, "venv lease write failed");
            }
        }
    }

    /// Point the legacy path at `venv` (Unix). Only a missing path or a link
    /// is replaced, atomically (a new link renamed over the old); a real
    /// directory there (an old venv, or one an old binary rebuilt) is left
    /// alone. A link some live process runs a Python through is not moved
    /// either: its later imports would load another runtime. True when the
    /// link names `venv` afterwards.
    #[cfg(unix)]
    pub(crate) fn point_legacy_at(&self, venv: &Path) -> bool {
        let Some(name) = venv.file_name() else {
            return false;
        };
        let target = Path::new(VENVS_DIR).join(name);
        let legacy = self.legacy();
        match std::fs::symlink_metadata(&legacy) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                if std::fs::read_link(&legacy).is_ok_and(|current| current == target) {
                    return true;
                }
                if path_in_use(&legacy) {
                    return false;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Ok(_) | Err(_) => return false,
        }
        let scratch = self.base.join(scratch_name("link", LEGACY_VENV_DIR));
        if std::os::unix::fs::symlink(&target, &scratch).is_err() {
            return false;
        }
        if std::fs::rename(&scratch, &legacy).is_ok() {
            return true;
        }
        let _ = std::fs::remove_file(&scratch);
        false
    }

    // Same signature as the Unix link writer; there is no link to write here.
    #[cfg(not(unix))]
    #[allow(clippy::unused_self)]
    pub(crate) fn point_legacy_at(&self, _venv: &Path) -> bool {
        false
    }

    /// Adopt the old single venv as `target` (its key's slot): only a real
    /// directory whose recorded base install is `matches`, only when the
    /// slot is empty, and only when no process runs a Python from it. The
    /// rename is followed at once by the link, so paths into the old
    /// location (its scripts' shebangs) keep resolving. The caller holds
    /// `target`'s bootstrap lock; the legacy venv's own lock (the one older
    /// binaries take to rebuild it) is held across the move. True when it
    /// moved.
    pub(crate) async fn adopt_legacy(
        &self,
        target: &Path,
        matches: impl Fn(&Path) -> bool,
    ) -> bool {
        let legacy = self.legacy();
        let is_real_dir = |path: &Path| {
            std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_dir())
        };
        if !is_real_dir(&legacy) || target.exists() || !matches(&legacy) {
            return false;
        }
        let Ok(_legacy_lock) =
            crate::kernel::bootstrap::dir_lock::acquire_bootstrap_lock(&legacy).await
        else {
            return false;
        };
        // Re-judge under the lock: an older binary may have rebuilt it.
        if !is_real_dir(&legacy) || target.exists() || !matches(&legacy) || path_in_use(&legacy) {
            return false;
        }
        if std::fs::rename(&legacy, target).is_err() {
            return false;
        }
        self.point_legacy_at(target);
        true
    }

    /// Move a venv that cannot be used (a half-built or damaged one) out of
    /// its slot so the slot can be rebuilt. The move is a rename, so no
    /// reader sees it half-deleted; it is deleted at once unless a live
    /// process holds it, else by a later prune.
    pub(crate) fn discard(&self, venv: &Path) -> std::io::Result<()> {
        let name = venv
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        let in_use = path_in_use(venv);
        let trash = self.root().join(scratch_name("trash", &name));
        std::fs::rename(venv, &trash)?;
        if !in_use && !live_lease(&trash, std::process::id()) {
            let _ = std::fs::remove_dir_all(&trash);
        }
        Ok(())
    }

    /// Remove old venvs, conservatively (see the module docs). Returns the
    /// removed keys, sorted.
    pub(crate) fn prune(&self, current: &str, now: SystemTime) -> Vec<String> {
        let root = self.root();
        let Ok(entries) = std::fs::read_dir(&root) else {
            return Vec::new();
        };
        let age = |time: SystemTime| now.duration_since(time).unwrap_or_default();
        // The legacy link's target: processes may run through the link.
        let linked = std::fs::symlink_metadata(self.legacy())
            .ok()
            .filter(|metadata| metadata.file_type().is_symlink())
            .and_then(|_| std::fs::read_link(self.legacy()).ok())
            .map(|target| self.base.join(target));
        let mut venvs = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let path = entry.path();
            if name.starts_with(".trash-") {
                let modified = entry
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .unwrap_or(now);
                if age(modified) > STALE_SCRATCH && !held_by_live_process(&path, 0) {
                    let _ = std::fs::remove_dir_all(&path);
                }
                continue;
            }
            if name == current || !is_venv_key(&name) || !path.is_dir() {
                continue;
            }
            if linked.as_deref() == Some(path.as_path()) {
                continue;
            }
            venvs.push((last_used(&path), name, path));
        }
        venvs.sort_by_key(|(used, _, _)| std::cmp::Reverse(*used));
        let mut removed = Vec::new();
        for (used, name, path) in venvs.into_iter().skip(KEEP_NEWEST) {
            if age(used) < KEEP_RECENT {
                continue;
            }
            // A process booting this venv takes its bootstrap lock to build
            // or sync it; one that only resolves it writes its lease first.
            let Some(_lock) = try_bootstrap_lock(&path) else {
                continue;
            };
            if age(last_used(&path)) < KEEP_RECENT || held_by_live_process(&path, 0) {
                continue;
            }
            let trash = root.join(scratch_name("trash", &name));
            if std::fs::rename(&path, &trash).is_ok() {
                let _ = std::fs::remove_dir_all(&trash);
                removed.push(name);
            }
        }
        // Old link scratch a crashed writer left beside the link.
        if let Ok(entries) = std::fs::read_dir(&self.base) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                let modified = entry
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .unwrap_or(now);
                if name.starts_with(&format!(".link-{LEGACY_VENV_DIR}-"))
                    && age(modified) > STALE_SCRATCH
                {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
        removed.sort();
        removed
    }
}

fn last_used(venv: &Path) -> SystemTime {
    std::fs::metadata(venv.join(LAST_USED_FILE))
        .or_else(|_| std::fs::metadata(venv))
        .and_then(|metadata| metadata.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Whether a live process other than `except` holds `venv`: a lease whose
/// owner is still that process, or a process running from it.
fn held_by_live_process(venv: &Path, except: u32) -> bool {
    live_lease(venv, except) || path_in_use(venv)
}

/// Whether a lease in `venv` names a live process other than `except`.
/// Dead leases are removed on the way.
fn live_lease(venv: &Path, except: u32) -> bool {
    let mut held = false;
    if let Ok(entries) = std::fs::read_dir(venv.join(LEASES_DIR)) {
        for entry in entries.flatten() {
            let owner = std::fs::read_to_string(entry.path())
                .ok()
                .and_then(|raw| parse_owner(&raw));
            match owner {
                Some(owner) if owner.pid == except => {}
                Some(owner) if is_owner_alive(&owner) => held = true,
                _ => {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
        }
    }
    held
}

/// Whether a live process was started from a path under `dir` (a kernel
/// runs `<venv>/bin/python`): Linux reads every process's command line.
/// Elsewhere there is no cheap portable answer, and the leases decide.
#[cfg(target_os = "linux")]
pub(crate) fn path_in_use(dir: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let mut prefix = dir.as_os_str().as_bytes().to_vec();
    if prefix.last() != Some(&b'/') {
        prefix.push(b'/');
    }
    let Ok(processes) = std::fs::read_dir("/proc") else {
        return false;
    };
    processes.flatten().any(|process| {
        let name = process.file_name();
        if !name.as_bytes().iter().all(u8::is_ascii_digit) {
            return false;
        }
        std::fs::read(process.path().join("cmdline")).is_ok_and(|cmdline| {
            cmdline
                .split(|byte| *byte == 0)
                .any(|arg| arg.starts_with(&prefix))
        })
    })
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn path_in_use(_dir: &Path) -> bool {
    false
}

#[cfg(test)]
mod tests;
