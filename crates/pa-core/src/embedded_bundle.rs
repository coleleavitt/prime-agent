//! The kernel runtime and bundled skills this binary was built from, carried
//! in the binary (see `build.rs`) and extracted on demand to a
//! content-addressed directory beside the kernel venv that installs from it:
//! `~/.prime/agent/runtime/<bundle id>/{prime-agent-runtime,skills}` by
//! default (`<kernel venv parent>/runtime/...`).
//!
//! A binary with a packaged exe-adjacent layout (or an explicit override)
//! uses that; every other binary (`cargo install`, `cargo run`) uses its
//! embedded copy, never the live source checkout, so the runtime and skills
//! always match the host that serves them. A new binary extracts a new
//! bundle id and its runtime identity differs, so it boots its own keyed
//! kernel venv; going back to an older binary finds that binary's venv
//! still in place.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use sha2::{Digest, Sha256};

include!(concat!(env!("OUT_DIR"), "/embedded_bundle.rs"));

/// The runtime tree inside a bundle.
pub(crate) const RUNTIME_DIR: &str = "prime-agent-runtime";
/// The bundled skills tree inside a bundle.
pub(crate) const SKILLS_DIR: &str = "skills";
/// The bundles directory beside the kernel venv.
const BUNDLES_DIR: &str = "runtime";
/// Touched on every use: pruning keeps recently used bundles.
const LAST_USED_FILE: &str = ".last-used";
/// Hex digits of the content hash that name a bundle.
const BUNDLE_ID_LEN: usize = 20;
/// Pruning keeps this many most recently used bundles besides the current one.
const KEEP_NEWEST: usize = 3;
/// Pruning never removes a bundle used within this window (another binary
/// that is still running touches its bundle on use).
const KEEP_RECENT: Duration = Duration::from_hours(7 * 24);
/// Abandoned staging and trash directories older than this are removed.
const STALE_SCRATCH: Duration = Duration::from_hours(24);
/// How often a long-running process refreshes its bundle's last-used mark.
const TOUCH_INTERVAL: Duration = Duration::from_hours(1);

/// One embedded file: its `/`-separated path inside the bundle, and contents.
pub(crate) type BundleFile = (&'static str, &'static [u8]);

/// The content address of a file set: a hash over every path and content.
pub(crate) fn bundle_id(files: &[BundleFile]) -> String {
    let mut sorted: Vec<&BundleFile> = files.iter().collect();
    sorted.sort_by_key(|(path, _)| *path);
    let mut hasher = Sha256::new();
    for (path, bytes) in sorted {
        hasher.update(path.as_bytes());
        hasher.update([0]);
        hasher.update(bytes);
        hasher.update([0]);
    }
    let digest = format!("{:x}", hasher.finalize());
    digest[..BUNDLE_ID_LEN].to_string()
}

fn is_bundle_id(name: &str) -> bool {
    name.len() == BUNDLE_ID_LEN && name.bytes().all(|b| b.is_ascii_hexdigit())
}

fn bundle_file_path(dir: &Path, relative: &str) -> PathBuf {
    relative
        .split('/')
        .fold(dir.to_path_buf(), |path, part| path.join(part))
}

/// True when `dir` holds every file of the set with identical contents.
fn bundle_matches(dir: &Path, files: &[BundleFile]) -> bool {
    files.iter().all(|(relative, bytes)| {
        std::fs::read(bundle_file_path(dir, relative)).is_ok_and(|disk| disk == *bytes)
    })
}

fn touch_last_used(dir: &Path) {
    let marker = dir.join(LAST_USED_FILE);
    let touched = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&marker)
        .and_then(|file| file.set_modified(SystemTime::now()));
    if let Err(error) = touched {
        tracing::debug!(marker = %marker.display(), %error, "bundle last-used mark failed");
    }
}

fn last_used(dir: &Path) -> SystemTime {
    std::fs::metadata(dir.join(LAST_USED_FILE))
        .or_else(|_| std::fs::metadata(dir))
        .and_then(|metadata| metadata.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// A process-unique scratch name under `root`.
fn scratch_name(kind: &str, id: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!(".{kind}-{id}-{}-{nanos}", std::process::id())
}

fn write_bundle(dir: &Path, files: &[BundleFile]) -> io::Result<()> {
    for (relative, bytes) in files {
        let path = bundle_file_path(dir, relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, bytes)?;
    }
    Ok(())
}

/// Put the file set at `<root>/<bundle id>` and return that directory.
///
/// A complete bundle is reused as is. Otherwise the set is written to a
/// private staging directory and renamed into place, so a reader never sees
/// a partial bundle and concurrent first boots converge on one directory (a
/// loser of the rename race verifies the winner's copy and discards its
/// own). A damaged bundle (files edited or removed) is moved aside and
/// replaced.
///
/// # Errors
///
/// Returns the I/O error when the bundle cannot be written or installed.
pub(crate) fn materialize(root: &Path, files: &[BundleFile]) -> io::Result<PathBuf> {
    let id = bundle_id(files);
    let target = root.join(&id);
    if bundle_matches(&target, files) {
        touch_last_used(&target);
        return Ok(target);
    }
    std::fs::create_dir_all(root)?;
    let staging = root.join(scratch_name("tmp", &id));
    if let Err(error) = write_bundle(&staging, files) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }
    for _ in 0..3 {
        match std::fs::rename(&staging, &target) {
            Ok(()) => {
                touch_last_used(&target);
                return Ok(target);
            }
            Err(_) if target.exists() => {
                if bundle_matches(&target, files) {
                    // Another process installed the same bundle first.
                    let _ = std::fs::remove_dir_all(&staging);
                    touch_last_used(&target);
                    return Ok(target);
                }
                // Damaged: move it aside (a rename, so no reader sees it
                // half-deleted) and retry the install.
                let trash = root.join(scratch_name("trash", &id));
                if std::fs::rename(&target, &trash).is_ok() {
                    let _ = std::fs::remove_dir_all(&trash);
                }
            }
            Err(error) => {
                let _ = std::fs::remove_dir_all(&staging);
                return Err(error);
            }
        }
    }
    let _ = std::fs::remove_dir_all(&staging);
    Err(io::Error::other(format!(
        "could not install the embedded runtime bundle at {}",
        target.display()
    )))
}

/// Remove old bundles from `root`, conservatively: `current`, the
/// [`KEEP_NEWEST`] most recently used others, every bundle used within
/// [`KEEP_RECENT`], and every bundle a path in `protected` lives under (the
/// kernel venv's editable skill installs) are kept. Abandoned scratch
/// directories older than [`STALE_SCRATCH`] go too. Returns the removed
/// bundle ids, sorted.
pub(crate) fn prune(
    root: &Path,
    current: &str,
    protected: &[PathBuf],
    now: SystemTime,
) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let age = |time: SystemTime| now.duration_since(time).unwrap_or_default();
    let mut bundles = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();
        if name.starts_with(".tmp-") || name.starts_with(".trash-") {
            let modified = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .unwrap_or(now);
            if age(modified) > STALE_SCRATCH {
                let _ = std::fs::remove_dir_all(&path);
            }
            continue;
        }
        if name == current || !is_bundle_id(&name) || !path.is_dir() {
            continue;
        }
        bundles.push((last_used(&path), name, path));
    }
    bundles.sort_by_key(|(used, _, _)| std::cmp::Reverse(*used));
    let mut removed = Vec::new();
    for (used, name, path) in bundles.into_iter().skip(KEEP_NEWEST) {
        if age(used) < KEEP_RECENT || protected.iter().any(|kept| kept.starts_with(&path)) {
            continue;
        }
        let trash = root.join(scratch_name("trash", &name));
        if std::fs::rename(&path, &trash).is_ok() {
            let _ = std::fs::remove_dir_all(&trash);
            removed.push(name);
        }
    }
    removed.sort();
    removed
}

/// The bundles directory: `runtime/` beside the kernel venv
/// (`~/.prime/agent/runtime` by default). It follows the venv, not the agent
/// dir: the venv's editable skill installs point into a bundle, and every
/// session sharing that venv (whatever its agent dir) must share the bundle
/// too, or one session's bundle can vanish under another's kernel.
fn bundles_root() -> Option<PathBuf> {
    crate::kernel::bootstrap::kernel_venv_dir()
        .parent()
        .map(|parent| parent.join(BUNDLES_DIR))
}

/// Per bundles root: the extracted bundle and when it was last marked used.
static MATERIALIZED: Mutex<Option<HashMap<PathBuf, (PathBuf, Instant)>>> = Mutex::new(None);

/// The extracted embedded bundle, extracting (and then pruning old bundles)
/// on the first call per bundles root in this process. `None` when this
/// binary embeds nothing or the kernel venv dir has no parent. On an
/// extraction failure the would-be directory is returned (it lacks the
/// files, so callers report it as searched) and the error is logged.
pub(crate) fn embedded_bundle_dir() -> Option<PathBuf> {
    if EMBEDDED_FILES.is_empty() {
        return None;
    }
    let root = bundles_root()?;
    let mut guard = MATERIALIZED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let memo = guard.get_or_insert_with(HashMap::new);
    if let Some((dir, marked)) = memo.get_mut(&root) {
        if dir.is_dir() {
            if marked.elapsed() >= TOUCH_INTERVAL {
                touch_last_used(dir);
                *marked = Instant::now();
            }
            return Some(dir.clone());
        }
    }
    match materialize(&root, EMBEDDED_FILES) {
        Ok(dir) => {
            memo.insert(root.clone(), (dir.clone(), Instant::now()));
            drop(guard);
            let current = bundle_id(EMBEDDED_FILES);
            let protected = crate::kernel::bootstrap::recorded_kernel_skill_paths();
            let removed = prune(&root, &current, &protected, SystemTime::now());
            if !removed.is_empty() {
                tracing::info!(root = %root.display(), removed = ?removed, "pruned old runtime bundles");
            }
            Some(dir)
        }
        Err(error) => {
            tracing::warn!(root = %root.display(), %error, "embedded runtime extraction failed");
            Some(root.join(bundle_id(EMBEDDED_FILES)))
        }
    }
}

#[cfg(test)]
mod tests;
