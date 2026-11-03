//! Test-only shared state: one crate-wide lock for the process env.

/// Process-wide env reads and writes (the telemetry override vars)
/// serialize through one lock across the crate's test modules: parallel
/// test threads in the same binary otherwise race the process env. Both
/// the supervisor tests' scrub guard and `agent_engine`'s
/// `telemetry_opt_in` hold this lock through their restores.
#[cfg(test)]
pub(crate) static TELEMETRY_ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A per-test scratch directory under the temp dir, removed with everything in it when the
/// guard drops: on success, on panic, and on early return alike. It derefs to its path, so it
/// stands in wherever a test used a bare `temp_dir().join(..)` path.
///
/// The path is `<root>/<name>` inside a removed-on-drop `<root>` of the same name: a session
/// file a test writes at `<path>/<id>.jsonl` implies its artifact dir at
/// `<path>/../session-artifacts/<id>`, which then lands in `<root>` rather than the shared temp
/// dir.
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct TestDir {
    _root: tempfile::TempDir,
    path: std::path::PathBuf,
}

#[cfg(test)]
impl TestDir {
    /// A fresh directory named `<prefix><random>` under the temp dir.
    pub(crate) fn new(prefix: &str) -> Self {
        Self::nested(
            tempfile::Builder::new()
                .prefix(prefix)
                .tempdir()
                .expect("create a test temp dir"),
        )
    }

    /// [`TestDir::new`] under the canonicalized temp dir: for fixtures whose contract needs a
    /// symlink-free path (a symlinked `TMPDIR`, macOS `/var`).
    pub(crate) fn new_canonical(prefix: &str) -> Self {
        let root = std::fs::canonicalize(std::env::temp_dir()).expect("canonical temp dir");
        Self::nested(
            tempfile::Builder::new()
                .prefix(prefix)
                .tempdir_in(root)
                .expect("create a test temp dir"),
        )
    }

    fn nested(root: tempfile::TempDir) -> Self {
        let path = root
            .path()
            .join(root.path().file_name().expect("a named temp dir"));
        std::fs::create_dir(&path).expect("create the test dir");
        Self { _root: root, path }
    }
}

/// Removal waits for the crate's in-flight blocking work first ([`BlockingWork`]): a worker's
/// background walk that started during the test would otherwise finish after the dir is gone
/// and recreate it (`<dir>/agent/auth.json` outliving the test).
#[cfg(test)]
impl Drop for TestDir {
    fn drop(&mut self) {
        BlockingWork::wait_for_idle(std::time::Duration::from_secs(30));
    }
}

/// Fire-and-forget blocking work the product spawns (`spawn_blocking`) that writes under a
/// worker's agent dir. Tests cannot await it, and the runtime finishes a started blocking task
/// after the test body (and its temp dirs) are gone, so the product marks each such task with
/// [`BlockingWork::start`] (test builds only) and [`TestDir`] removal waits for none to be in
/// flight. The count is process-wide: a sibling test's walk delays removal by its few
/// milliseconds, never correctness.
#[cfg(test)]
#[must_use = "the work counts as in flight until this drops"]
pub(crate) struct BlockingWork(());

#[cfg(test)]
static BLOCKING_WORK: (std::sync::Mutex<usize>, std::sync::Condvar) =
    (std::sync::Mutex::new(0), std::sync::Condvar::new());

#[cfg(test)]
impl BlockingWork {
    /// Count one piece of work in flight. Take it BEFORE `spawn_blocking` and move it into the
    /// closure: a queued task the runtime drops unstarted releases it too.
    pub(crate) fn start() -> Self {
        *BLOCKING_WORK
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
        Self(())
    }

    /// Block until no work is in flight, or `limit` passes (a wedged walk must not hang the
    /// suite; the leak then shows in the temp dir instead).
    pub(crate) fn wait_for_idle(limit: std::time::Duration) {
        let (count, idle) = &BLOCKING_WORK;
        let guard = count
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = idle
            .wait_timeout_while(guard, limit, |in_flight| *in_flight > 0)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }
}

#[cfg(test)]
impl Drop for BlockingWork {
    fn drop(&mut self) {
        let (count, idle) = &BLOCKING_WORK;
        let mut in_flight = count
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *in_flight -= 1;
        if *in_flight == 0 {
            idle.notify_all();
        }
    }
}

#[cfg(test)]
impl std::ops::Deref for TestDir {
    type Target = std::path::Path;

    fn deref(&self) -> &std::path::Path {
        &self.path
    }
}

#[cfg(test)]
impl AsRef<std::path::Path> for TestDir {
    fn as_ref(&self) -> &std::path::Path {
        &self.path
    }
}

/// Serializes as its path (fixtures put the dir straight into JSON commands).
#[cfg(test)]
impl serde::Serialize for TestDir {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.path.serialize(serializer)
    }
}

/// A fixture value plus the [`TestDir`] it lives in: helpers that build a worker or supervisor
/// in a scratch dir hand both back, so the dir outlives every use of the value and is removed
/// with it. Derefs to the value.
#[cfg(test)]
#[cfg_attr(
    not(unix),
    expect(dead_code, reason = "every worker fixture that uses it is unix-only")
)]
pub(crate) struct InTestDir<T> {
    value: T,
    dir: TestDir,
}

#[cfg(test)]
impl<T> InTestDir<T> {
    pub(crate) fn new(value: T, dir: TestDir) -> Self {
        Self { value, dir }
    }

    /// The value and the dir guard apart: a test that drops the value (a worker "restart")
    /// keeps the dir until its own end.
    #[cfg_attr(
        not(unix),
        expect(dead_code, reason = "every worker fixture that uses it is unix-only")
    )]
    pub(crate) fn into_parts(self) -> (T, TestDir) {
        (self.value, self.dir)
    }
}

#[cfg(test)]
impl<T> std::ops::Deref for InTestDir<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

#[cfg(test)]
impl<T: AsRef<std::path::Path>> AsRef<std::path::Path> for InTestDir<T> {
    fn as_ref(&self) -> &std::path::Path {
        self.value.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::{BlockingWork, TestDir};

    /// The leak class: a worker's background walk finished after its test, and its
    /// `AuthStorage::create` recreated `<dir>/agent/auth.json` in the removed dir. A
    /// [`TestDir`] removal now waits for in-flight [`BlockingWork`], so a write that lands
    /// while the work is in flight is removed with the dir.
    #[test]
    fn removal_waits_for_in_flight_blocking_work() {
        let dir = TestDir::new("pa-blocking-work-");
        let auth = dir.join("agent").join("auth.json");
        let work = BlockingWork::start();
        let (dropped_tx, dropped_rx) = std::sync::mpsc::channel();
        let dropper = std::thread::spawn(move || {
            drop(dir);
            dropped_tx.send(()).expect("report the removal");
        });
        // Removal must not finish while the work is in flight. (Without the wait it finishes
        // at once and the write below recreates the dir, as the walk did.)
        assert_eq!(
            dropped_rx.recv_timeout(std::time::Duration::from_millis(200)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        );
        std::fs::create_dir_all(auth.parent().expect("agent dir")).expect("agent dir");
        std::fs::write(&auth, "{}").expect("late write");
        drop(work);
        dropped_rx.recv().expect("the removal finishes");
        dropper.join().expect("dropper");
        assert!(!auth.parent().expect("agent dir").exists());
    }
}

/// Scoped process-env overrides for the live-kernel tests: applied on
/// construction, restored on drop.
#[cfg(test)]
pub(crate) struct KernelEnvOverride {
    saved: Vec<(String, Option<String>)>,
}

#[cfg(test)]
impl KernelEnvOverride {
    pub(crate) fn apply(pairs: &[(&str, Option<String>)]) -> Self {
        let saved = pairs
            .iter()
            .map(|(key, _)| ((*key).to_string(), std::env::var(key).ok()))
            .collect();
        for (key, value) in pairs {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
        KernelEnvOverride { saved }
    }
}

#[cfg(test)]
impl Drop for KernelEnvOverride {
    fn drop(&mut self) {
        for (key, value) in &self.saved {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

/// The kernel python for the live-kernel tests (skipped without one).
#[cfg(test)]
pub(crate) fn live_kernel_python() -> Option<std::path::PathBuf> {
    pa_types::platform::test_isolation::test_kernel_python("PA_CORE_KERNEL_PYTHON")
}

/// Write a harness store holding one factory spec per id (plus a memory, which never counts as a
/// spec) — the on-disk shape the kernel's harness writes.
#[cfg(test)]
pub(crate) fn write_factory_specs(harness_dir: &std::path::Path, ids: &[&str]) {
    let entry = |id: &str, kind: &str| {
        serde_json::json!({
            "id": id, "kind": kind, "title": id, "content": "", "path": "general",
            "reference": {}, "arguments": {}, "metadata": {}, "source": "agent",
            "created_at": "2026-10-16T00:00:00Z", "updated_at": "2026-10-16T00:00:00Z",
            "version": 1,
        })
    };
    let factory: serde_json::Map<String, serde_json::Value> = ids
        .iter()
        .map(|id| ((*id).to_string(), entry(id, "factory")))
        .collect();
    std::fs::create_dir_all(harness_dir).unwrap();
    std::fs::write(
        pa_core::refinement::get_harness_state_path(harness_dir),
        serde_json::json!({
            "schema": 1,
            "entries": { "factory": factory, "memory": { "m": entry("m", "memory") } },
            "refinements": [],
        })
        .to_string(),
    )
    .unwrap();
}

/// Hold a real file read behind a FIFO until another future on the same
/// current-thread runtime releases it. A bounded rescue makes the synchronous
/// regression fail without hanging the process; restore the file before asserting.
#[cfg(all(test, unix))]
pub(crate) async fn assert_async_file_read<T>(
    path: &std::path::Path,
    read: impl std::future::Future<Output = T>,
) -> T {
    let raw = std::fs::read(path).expect("original state");
    std::fs::remove_file(path).expect("replace state with FIFO");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(path)
            .status()
            .expect("mkfifo")
            .success()
    );
    let (opened_tx, opened_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let fifo_path = path.to_path_buf();
    let fifo_raw = raw.clone();
    let writer = std::thread::spawn(move || {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(fifo_path)
            .expect("FIFO writer");
        opened_tx.send(()).expect("reader opened");
        let responsive = release_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .is_ok();
        file.write_all(&fifo_raw).expect("release file read");
        responsive
    });
    let (result, ()) = tokio::join!(read, async {
        opened_rx.await.expect("storage read started");
        let _ = release_tx.send(());
    });
    let responsive = writer.join().expect("FIFO writer");
    std::fs::remove_file(path).expect("remove FIFO");
    std::fs::write(path, raw).expect("restore state");
    assert!(
        responsive,
        "the storage read must not block the async executor"
    );
    result
}

/// Serve a real file through a FIFO and count how often it is read: every read receives the
/// original bytes. A read after the first is served only once `finish` stays silent for a
/// bounded window, so an extra read is counted instead of hanging the process.
#[cfg(all(test, unix))]
pub(crate) struct FileReadCounter {
    path: std::path::PathBuf,
    raw: Vec<u8>,
    done: std::sync::mpsc::Sender<()>,
    server: std::thread::JoinHandle<usize>,
}

#[cfg(all(test, unix))]
pub(crate) fn count_file_reads(path: &std::path::Path) -> FileReadCounter {
    let raw = std::fs::read(path).expect("original file");
    std::fs::remove_file(path).expect("replace file with FIFO");
    assert!(
        std::process::Command::new("mkfifo")
            .arg(path)
            .status()
            .expect("mkfifo")
            .success()
    );
    let (done, done_rx) = std::sync::mpsc::channel();
    let fifo_path = path.to_path_buf();
    let fifo_raw = raw.clone();
    let server = std::thread::spawn(move || {
        use std::io::Write;
        let mut reads = 0;
        loop {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .open(&fifo_path)
                .expect("FIFO writer");
            if done_rx.try_recv().is_ok() {
                return reads;
            }
            file.write_all(&fifo_raw).expect("serve file read");
            drop(file);
            reads += 1;
            if done_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .is_ok()
            {
                return reads;
            }
        }
    });
    FileReadCounter {
        path: path.to_path_buf(),
        raw,
        done,
        server,
    }
}

#[cfg(all(test, unix))]
impl FileReadCounter {
    /// Stop serving, restore the file, and return how many reads it served.
    pub(crate) fn finish(self) -> usize {
        self.done.send(()).expect("FIFO server running");
        let unblock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)
            .expect("FIFO unblock");
        let reads = self.server.join().expect("FIFO server");
        drop(unblock);
        std::fs::remove_file(&self.path).expect("remove FIFO");
        std::fs::write(&self.path, self.raw).expect("restore file");
        reads
    }
}
