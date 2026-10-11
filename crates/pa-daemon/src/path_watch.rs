//! Session-owned filesystem watch subscriptions (upstream #2351, TS
//! `core/path-watches.ts`): the registry behind `rlm.watch.path` and the
//! notice formatters.
//!
//! The SESSION owns the subscriptions (the engine holds the registry, not
//! the kernel or a Python cell), so kernel restarts never drop them, and a
//! session replacement or close releases every one. Raw filesystem events
//! from ONE platform watcher per session (`notify`: inotify, `FSEvents`,
//! kqueue, `ReadDirectoryChangesW`) route to the watches that cover their
//! paths and debounce over [`PATH_WATCH_DEBOUNCE`] into one batch of
//! changed paths; observed removal of the watched path or a watcher
//! failure stops the subscription with a failure event, and recreating the
//! path needs a new registration. Quiet like the agent and job watches:
//! notices carry paths only, never file contents.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use pa_types::sync::MutexExt;
use serde_json::{Value, json};

/// TS `PATH_WATCH_DEBOUNCE_MS`: one batch window for filesystem events.
pub const PATH_WATCH_DEBOUNCE: Duration = Duration::from_millis(200);
/// TS `MAX_ACTIVE_PATH_WATCHES`: concurrent active watches per session.
pub const PATH_WATCH_MAX_ACTIVE: usize = 64;
/// TS `MAX_TOTAL_PATH_WATCHES`: registrations per session, finished ones
/// included.
pub const PATH_WATCH_MAX_TOTAL: usize = 1_024;
/// TS `PATH_WATCH_PATHS_MAX_BYTES`: the encoded cap of one notice's
/// changed-path list.
pub const PATH_WATCH_PATHS_MAX_BYTES: usize = 32 * 1024;
/// The distinct changed paths one batch keeps while it debounces (a burst
/// past it reports `truncated`); bounds memory before the byte cap applies.
const PATH_WATCH_PENDING_MAX: usize = 4_096;

/// A watch's lifecycle state (the TS `RlmPathWatchStatus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathWatchStatus {
    Active,
    Completed,
    Failed,
}

impl PathWatchStatus {
    /// The wire token.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }
}

/// One registered watch (TS `RlmPathWatchInfo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathWatchInfo {
    pub watch_id: String,
    /// The watched path, resolved against the session cwd by the caller.
    pub path: String,
    /// Recursive only for a directory registered recursively.
    pub recursive: bool,
    pub status: PathWatchStatus,
    pub created_at: String,
    pub error: Option<String>,
}

impl PathWatchInfo {
    /// The kernel wire shape (`snake_case`, like the other `rlm.*` payloads).
    #[must_use]
    pub fn host_response(&self) -> Value {
        let mut row = json!({
            "watch_id": self.watch_id,
            "path": self.path,
            "recursive": self.recursive,
            "status": self.status.as_str(),
            "created_at": self.created_at,
        });
        if let Some(error) = &self.error {
            row["error"] = json!(error);
        }
        row
    }
}

/// One debounced batch of changed paths under an active watch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathWatchChange {
    pub watch_id: String,
    pub path: String,
    pub recursive: bool,
    pub paths: Vec<String>,
    pub truncated: bool,
}

/// The watch stopped: the path was removed or the watcher failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathWatchFailure {
    pub watch_id: String,
    pub path: String,
    pub recursive: bool,
    pub error: String,
}

/// What a watch reports to its session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathWatchEvent {
    Changed(PathWatchChange),
    Failed(PathWatchFailure),
}

/// The worker-installed routing of one path-watch event into the session
/// (the digest-aware notice pipeline).
pub type PathWatchSink = Arc<dyn Fn(PathWatchEvent) + Send + Sync>;

/// TS `createWatchPathChangedMessage`'s content.
#[must_use]
pub fn format_path_watch_changed(change: &PathWatchChange) -> String {
    let header = pa_core::session_engine::agent_messaging::sanitize_message_header_value;
    let changed = change
        .paths
        .iter()
        .map(|path| header(path))
        .collect::<Vec<_>>()
        .join("\n- ");
    let truncated = if change.truncated {
        "\n(more paths changed; the list is truncated)"
    } else {
        ""
    };
    format!(
        "[watch-path id:{} path:{}]\n\nChanged paths:\n- {changed}{truncated}",
        header(&change.watch_id),
        header(&change.path)
    )
}

/// TS `createWatchPathFailedMessage`'s content.
#[must_use]
pub fn format_path_watch_failed(failure: &PathWatchFailure) -> String {
    let header = pa_core::session_engine::agent_messaging::sanitize_message_header_value;
    format!(
        "[watch-path-failed id:{} path:{}]\n\nError: {}",
        header(&failure.watch_id),
        header(&failure.path),
        failure.error
    )
}

/// TS `capPathList`: keep paths (in order) while their encoded size (one
/// separator byte each) fits [`PATH_WATCH_PATHS_MAX_BYTES`].
#[must_use]
pub fn cap_path_list(paths: &[String]) -> (Vec<String>, bool) {
    let mut kept = Vec::new();
    let mut bytes = 0usize;
    for path in paths {
        let size = path.len() + 1;
        if bytes + size > PATH_WATCH_PATHS_MAX_BYTES {
            return (kept, true);
        }
        kept.push(path.clone());
        bytes += size;
    }
    (kept, false)
}

/// TS `resolveWatchPath`: absolute stays, relative joins the session cwd.
#[must_use]
pub fn resolve_watch_path(path: &str, cwd: &Path) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// One watch's raw input: the changed paths of one platform event, or the
/// watcher's failure.
type RawEvent = Result<Vec<PathBuf>, String>;

struct Entry {
    info: PathWatchInfo,
    /// The path handed to the platform watcher (canonical when the path
    /// resolves): event paths arrive under it and map back under
    /// [`PathWatchInfo::path`].
    root: PathBuf,
    is_dir: bool,
    /// The watch task's input; dropping it ends the task.
    events: Option<tokio::sync::mpsc::UnboundedSender<RawEvent>>,
}

impl Entry {
    fn active(&self) -> bool {
        self.info.status == PathWatchStatus::Active
    }

    /// Whether a platform event at `path` belongs to this watch: the
    /// watched path itself, a direct child of a watched directory, or any
    /// descendant of a recursively watched one.
    fn covers(&self, path: &Path) -> bool {
        path == self.root
            || (self.is_dir
                && if self.info.recursive {
                    path.starts_with(&self.root)
                } else {
                    path.parent() == Some(self.root.as_path())
                })
    }

    /// An event path under the watch's display path (the platform reports
    /// the canonical form).
    fn display_path(&self, path: &Path) -> String {
        match path.strip_prefix(&self.root) {
            Ok(rest) if rest.as_os_str().is_empty() => self.info.path.clone(),
            Ok(rest) => Path::new(&self.info.path).join(rest).display().to_string(),
            Err(_) => path.display().to_string(),
        }
    }
}

#[derive(Default)]
struct State {
    /// Registration order (TS `Map` order).
    entries: Vec<Entry>,
    total: usize,
    /// ONE platform watcher per session (one inotify instance and one
    /// backend thread however many watches are active), created at the
    /// first registration. Its callback only forwards events to the
    /// dispatcher, never touching this state: the backend's `watch` call
    /// waits on its event thread, so a callback taking this lock while a
    /// registration holds it would deadlock.
    watcher: Option<notify::RecommendedWatcher>,
}

impl State {
    fn entry_mut(&mut self, watch_id: &str) -> Option<&mut Entry> {
        self.entries
            .iter_mut()
            .find(|entry| entry.info.watch_id == watch_id)
    }

    /// The mode one root needs across its active watches (`None`: unwatch).
    fn root_mode(&self, root: &Path) -> Option<notify::RecursiveMode> {
        let mut needed = None;
        for entry in self
            .entries
            .iter()
            .filter(|entry| entry.active() && entry.root == root)
        {
            needed = Some(if entry.info.recursive {
                notify::RecursiveMode::Recursive
            } else {
                needed.unwrap_or(notify::RecursiveMode::NonRecursive)
            });
        }
        needed
    }

    /// Re-apply one root's platform subscription after its watch set
    /// changed (several watches may share a root).
    fn sync_root(&mut self, root: &Path) -> notify::Result<()> {
        use notify::Watcher as _;
        let mode = self.root_mode(root);
        let Some(watcher) = self.watcher.as_mut() else {
            return Ok(());
        };
        if let Some(mode) = mode {
            watcher.watch(root, mode)
        } else {
            // A root the platform already dropped (removed) errors;
            // nothing is left to release.
            let _ = watcher.unwatch(root);
            Ok(())
        }
    }

    /// Stop one active watch with `status`; `true` when it was active.
    fn stop(&mut self, watch_id: &str, status: PathWatchStatus, error: Option<String>) -> bool {
        let Some(entry) = self.entry_mut(watch_id) else {
            return false;
        };
        if !entry.active() {
            return false;
        }
        entry.info.status = status;
        entry.info.error = error;
        entry.events = None;
        let root = entry.root.clone();
        let _ = self.sync_root(&root);
        true
    }
}

/// The session's path-watch registry (TS `RlmPathWatchRegistry`).
#[derive(Default)]
pub struct PathWatchRegistry {
    state: Arc<Mutex<State>>,
}

impl PathWatchRegistry {
    /// Register one watch on an existing file or directory. The watch's
    /// task runs on the current tokio runtime and reports through `sink`.
    ///
    /// # Errors
    ///
    /// Returns an error at the active or total limit, for a missing path,
    /// or when the platform watcher cannot watch it.
    pub fn register(
        &self,
        path: &Path,
        recursive: bool,
        sink: PathWatchSink,
    ) -> anyhow::Result<PathWatchInfo> {
        let mut state = self.state.lock_or_recover();
        let active = state.entries.iter().filter(|entry| entry.active()).count();
        if active >= PATH_WATCH_MAX_ACTIVE {
            anyhow::bail!("Too many active path watches: limit is {PATH_WATCH_MAX_ACTIVE}");
        }
        if state.total >= PATH_WATCH_MAX_TOTAL {
            anyhow::bail!("Too many path watch registrations: limit is {PATH_WATCH_MAX_TOTAL}");
        }
        let display = path.display().to_string();
        let Ok(metadata) = std::fs::metadata(path) else {
            anyhow::bail!("Watched path does not exist: {display}");
        };
        if state.watcher.is_none() {
            state.watcher = Some(self.spawn_watcher(&display, recursive)?);
        }
        let root = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
        let info = PathWatchInfo {
            watch_id: format!("watch_{}", uuid::Uuid::new_v4().simple()),
            path: display.clone(),
            recursive: recursive && metadata.is_dir(),
            status: PathWatchStatus::Active,
            created_at: crate::util::now_iso(),
            error: None,
        };
        state.entries.push(Entry {
            info: info.clone(),
            root: root.clone(),
            is_dir: metadata.is_dir(),
            events: Some(events_tx),
        });
        if let Err(error) = state.sync_root(&root) {
            state.entries.pop();
            let _ = state.sync_root(&root);
            return Err(watch_error(&display, recursive, &error));
        }
        state.total += 1;
        drop(state);
        tokio::spawn(watch_task(
            Arc::downgrade(&self.state),
            info.clone(),
            events_rx,
            sink,
        ));
        Ok(info)
    }

    /// The session's platform watcher plus its dispatcher task: raw events
    /// route to every active watch that covers one of their paths; a
    /// backend error without paths fails every active watch.
    fn spawn_watcher(
        &self,
        display: &str,
        recursive: bool,
    ) -> anyhow::Result<notify::RecommendedWatcher> {
        let (raw_tx, mut raw_rx) = tokio::sync::mpsc::unbounded_channel();
        let watcher = notify::recommended_watcher(move |event| {
            // A closed receiver means the registry is gone.
            let _ = raw_tx.send(event);
        })
        .map_err(|error| watch_error(display, recursive, &error))?;
        let state = Arc::downgrade(&self.state);
        tokio::spawn(async move {
            while let Some(event) = raw_rx.recv().await {
                let Some(state) = state.upgrade() else {
                    return;
                };
                dispatch(&state.lock_or_recover(), event);
            }
        });
        Ok(watcher)
    }

    #[must_use]
    pub fn get(&self, watch_id: &str) -> Option<PathWatchInfo> {
        self.state
            .lock_or_recover()
            .entry_mut(watch_id)
            .map(|entry| entry.info.clone())
    }

    /// Every watch, finished ones included, in registration order.
    #[must_use]
    pub fn list(&self) -> Vec<PathWatchInfo> {
        self.state
            .lock_or_recover()
            .entries
            .iter()
            .map(|entry| entry.info.clone())
            .collect()
    }

    /// Stop one watch (`completed`); a finished watch answers unchanged.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown id.
    pub fn cancel(&self, watch_id: &str) -> anyhow::Result<PathWatchInfo> {
        let mut state = self.state.lock_or_recover();
        state.stop(watch_id, PathWatchStatus::Completed, None);
        state
            .entry_mut(watch_id)
            .map(|entry| entry.info.clone())
            .ok_or_else(|| anyhow::anyhow!("Unknown path watch: {watch_id}"))
    }

    /// Release every watch (the owner session ended or was replaced): the
    /// platform watcher and its thread stop, the watch tasks end, and the
    /// registry starts empty — the limits count the next session from zero.
    pub fn dispose(&self) {
        *self.state.lock_or_recover() = State::default();
    }

    /// Active watches, and whether the platform watcher is still held.
    #[must_use]
    pub fn active_count(&self) -> (usize, bool) {
        let state = self.state.lock_or_recover();
        (
            state.entries.iter().filter(|entry| entry.active()).count(),
            state.watcher.is_some(),
        )
    }
}

/// Route one raw platform event to the watches it concerns.
fn dispatch(state: &State, event: notify::Result<notify::Event>) {
    match event {
        Ok(event) => {
            for entry in state.entries.iter().filter(|entry| entry.active()) {
                let Some(events) = &entry.events else {
                    continue;
                };
                let paths = event
                    .paths
                    .iter()
                    .filter(|path| entry.covers(path))
                    .map(|path| PathBuf::from(entry.display_path(path)))
                    .collect::<Vec<_>>();
                if !paths.is_empty() {
                    let _ = events.send(Ok(paths));
                }
            }
        }
        Err(error) => {
            let message = error.to_string();
            for entry in state.entries.iter().filter(|entry| entry.active()) {
                let concerned =
                    error.paths.is_empty() || error.paths.iter().any(|path| entry.covers(path));
                if let (true, Some(events)) = (concerned, &entry.events) {
                    let _ = events.send(Err(message.clone()));
                }
            }
        }
    }
}

fn watch_error(path: &str, recursive: bool, error: &notify::Error) -> anyhow::Error {
    let how = if recursive { " recursively" } else { "" };
    anyhow::anyhow!("Cannot watch {path}{how}: {error}")
}

/// One watch's debounce loop: the registration's liveness check opens the
/// first [`PATH_WATCH_DEBOUNCE`] window (a deletion racing the
/// registration can drop its event, and the flush turns it into a failure
/// notice instead of silence); afterwards each first raw event opens the
/// next window, whose events coalesce into one batch. Ends when the watch
/// stops (its channel closes) or the registry is gone.
async fn watch_task(
    state: Weak<Mutex<State>>,
    info: PathWatchInfo,
    mut events: tokio::sync::mpsc::UnboundedReceiver<RawEvent>,
    sink: PathWatchSink,
) {
    let watched = PathBuf::from(&info.path);
    let mut batch = Batch::default();
    loop {
        if !flush_after_window(&state, &info, &mut events, &sink, &watched, batch).await {
            return;
        }
        let Some(event) = events.recv().await else {
            return;
        };
        batch = Batch::default();
        if let Err(error) = batch.absorb(event) {
            fail(&state, &info, &sink, error);
            return;
        }
    }
}

/// The changed paths of one debounce window, deduplicated in arrival order.
#[derive(Default)]
struct Batch {
    paths: Vec<String>,
    seen: HashSet<String>,
    truncated: bool,
}

impl Batch {
    /// Fold one raw event in; `Err` carries the watcher's failure.
    fn absorb(&mut self, event: RawEvent) -> Result<(), String> {
        for path in event? {
            let path = path.display().to_string();
            if self.seen.contains(&path) {
                continue;
            }
            if self.paths.len() >= PATH_WATCH_PENDING_MAX {
                self.truncated = true;
                continue;
            }
            self.seen.insert(path.clone());
            self.paths.push(path);
        }
        Ok(())
    }
}

/// Wait out one debounce window (absorbing its events), then flush.
/// `false` when the watch ended.
async fn flush_after_window(
    state: &Weak<Mutex<State>>,
    info: &PathWatchInfo,
    events: &mut tokio::sync::mpsc::UnboundedReceiver<RawEvent>,
    sink: &PathWatchSink,
    watched: &Path,
    mut batch: Batch,
) -> bool {
    let window = tokio::time::sleep(PATH_WATCH_DEBOUNCE);
    tokio::pin!(window);
    loop {
        tokio::select! {
            () = &mut window => break,
            event = events.recv() => match event {
                Some(event) => {
                    if let Err(error) = batch.absorb(event) {
                        fail(state, info, sink, error);
                        return false;
                    }
                }
                None => return false,
            },
        }
    }
    if !still_active(state, &info.watch_id) {
        return false;
    }
    // Observed removal stops the watch: recreating the path needs a new
    // registration.
    if std::fs::metadata(watched).is_err() {
        fail(state, info, sink, "Watched path was removed".to_string());
        return false;
    }
    if batch.paths.is_empty() {
        return true;
    }
    let (paths, capped) = cap_path_list(&batch.paths);
    sink(PathWatchEvent::Changed(PathWatchChange {
        watch_id: info.watch_id.clone(),
        path: info.path.clone(),
        recursive: info.recursive,
        paths,
        truncated: capped || batch.truncated,
    }));
    true
}

fn still_active(state: &Weak<Mutex<State>>, watch_id: &str) -> bool {
    state.upgrade().is_some_and(|state| {
        state
            .lock_or_recover()
            .entry_mut(watch_id)
            .is_some_and(|entry| entry.active())
    })
}

fn fail(state: &Weak<Mutex<State>>, info: &PathWatchInfo, sink: &PathWatchSink, error: String) {
    let Some(state) = state.upgrade() else {
        return;
    };
    let stopped =
        state
            .lock_or_recover()
            .stop(&info.watch_id, PathWatchStatus::Failed, Some(error.clone()));
    if stopped {
        sink(PathWatchEvent::Failed(PathWatchFailure {
            watch_id: info.watch_id.clone(),
            path: info.path.clone(),
            recursive: info.recursive,
            error,
        }));
    }
}

#[cfg(test)]
mod tests;
