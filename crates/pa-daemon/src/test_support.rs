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
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct TestDir(tempfile::TempDir);

#[cfg(test)]
impl TestDir {
    /// A fresh directory named `<prefix><random>` under the temp dir.
    pub(crate) fn new(prefix: &str) -> Self {
        Self(
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
        Self(
            tempfile::Builder::new()
                .prefix(prefix)
                .tempdir_in(root)
                .expect("create a test temp dir"),
        )
    }
}

#[cfg(test)]
impl std::ops::Deref for TestDir {
    type Target = std::path::Path;

    fn deref(&self) -> &std::path::Path {
        self.0.path()
    }
}

#[cfg(test)]
impl AsRef<std::path::Path> for TestDir {
    fn as_ref(&self) -> &std::path::Path {
        self.0.path()
    }
}

/// Serializes as its path (fixtures put the dir straight into JSON commands).
#[cfg(test)]
impl serde::Serialize for TestDir {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.path().serialize(serializer)
    }
}

/// A fixture value plus the [`TestDir`] it lives in: helpers that build a worker or supervisor
/// in a scratch dir hand both back, so the dir outlives every use of the value and is removed
/// with it. Derefs to the value.
#[cfg(test)]
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
