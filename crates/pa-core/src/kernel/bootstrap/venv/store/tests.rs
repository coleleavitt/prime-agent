use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime};

use super::*;

/// The manifest a fake build writes: the runtime it was built for.
const FAKE_MANIFEST: &str = ".fake-runtime";

/// A runtime whose venvs are a directory holding [`FAKE_MANIFEST`]; every
/// build and readiness check is counted.
#[derive(Default)]
struct FakeRuntime {
    identity: String,
    builds: AtomicUsize,
    syncs: AtomicUsize,
    /// Fast-path readiness checks (before the lock), shared across
    /// contenders so a build can wait for the others to arrive.
    arrivals: Arc<(Mutex<usize>, Condvar)>,
    /// Hold each build until this many contenders arrived.
    hold_build_for: usize,
}

impl FakeRuntime {
    fn new(identity: &str) -> Self {
        Self {
            identity: identity.to_string(),
            ..Self::default()
        }
    }

    fn recorded(venv: &Path) -> Option<String> {
        std::fs::read_to_string(venv.join(FAKE_MANIFEST)).ok()
    }
}

impl VenvOps for FakeRuntime {
    fn ready(&self, venv: &Path) -> bool {
        let (count, arrived) = &*self.arrivals;
        *count.lock().unwrap() += 1;
        arrived.notify_all();
        self.base_ready(venv)
    }

    fn base_ready(&self, venv: &Path) -> bool {
        Self::recorded(venv).as_deref() == Some(self.identity.as_str())
    }

    fn records_this_runtime(&self, venv: &Path) -> bool {
        self.base_ready(venv)
    }

    async fn sync(&self, _venv: &Path) -> anyhow::Result<()> {
        // A real sync runs uv off the async thread.
        tokio::task::yield_now().await;
        self.syncs.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn build(&self, venv: &Path) -> anyhow::Result<()> {
        tokio::task::yield_now().await;
        assert!(!venv.exists(), "a build starts from an empty slot");
        let (count, arrived) = &*self.arrivals;
        let guard = count.lock().unwrap();
        let (_guard, wait) = arrived
            .wait_timeout_while(guard, Duration::from_secs(30), |count| {
                *count < self.hold_build_for
            })
            .unwrap();
        assert!(!wait.timed_out(), "every contender arrived");
        self.builds.fetch_add(1, Ordering::SeqCst);
        std::fs::create_dir_all(venv.join("bin"))?;
        std::fs::write(venv.join(FAKE_MANIFEST), &self.identity)?;
        Ok(())
    }

    fn report(&self, _message: &str) {}
}

fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
        .block_on(future)
}

fn entry_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    names
}

fn link_target(store: &VenvStore) -> Option<PathBuf> {
    std::fs::read_link(store.legacy()).ok()
}

/// Two binaries carrying different runtimes alternate: each builds its own
/// venv once, and switching back rebuilds nothing (the shared venv used to
/// be deleted and rebuilt on every switch, ~30 s each). The legacy path
/// follows the most recently booted one.
#[test]
fn two_runtimes_coexist_and_switching_back_rebuilds_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store = VenvStore::new(dir.path().to_path_buf());
    let release = FakeRuntime::new("sha256:release");
    let dev = FakeRuntime::new("sha256:dev");
    let (release_key, dev_key) = (venv_key(&release.identity), venv_key(&dev.identity));
    assert_ne!(release_key, dev_key);

    let mut booted = Vec::new();
    for runtime in [&release, &dev, &release, &dev, &release] {
        let venv = block_on(store.ensure(&venv_key(&runtime.identity), runtime)).unwrap();
        booted.push((venv, link_target(&store)));
    }
    let release_venv = store.venv(&release_key);
    let dev_venv = store.venv(&dev_key);
    #[cfg(unix)]
    let (to_release, to_dev) = (
        Some(Path::new(VENVS_DIR).join(&release_key)),
        Some(Path::new(VENVS_DIR).join(&dev_key)),
    );
    #[cfg(not(unix))]
    let (to_release, to_dev) = (None, None);
    assert_eq!(
        (
            booted,
            release.builds.load(Ordering::SeqCst),
            dev.builds.load(Ordering::SeqCst),
            FakeRuntime::recorded(&release_venv),
            FakeRuntime::recorded(&dev_venv),
        ),
        (
            vec![
                (release_venv.clone(), to_release.clone()),
                (dev_venv.clone(), to_dev.clone()),
                (release_venv.clone(), to_release.clone()),
                (dev_venv.clone(), to_dev),
                (release_venv, to_release),
            ],
            1,
            1,
            Some("sha256:release".to_string()),
            Some("sha256:dev".to_string()),
        )
    );
}

/// Concurrent first boots of one runtime build its venv exactly once: the
/// second waits on the bootstrap lock and finds the first's venv ready.
#[test]
fn concurrent_first_boots_build_once() {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(VenvStore::new(dir.path().to_path_buf()));
    let arrivals = Arc::new((Mutex::new(0), Condvar::new()));
    let contenders: Vec<_> = (0..2)
        .map(|_| {
            let store = Arc::clone(&store);
            let runtime = Arc::new(FakeRuntime {
                identity: "sha256:one".to_string(),
                arrivals: Arc::clone(&arrivals),
                // Both contenders checked the fast path before either builds.
                hold_build_for: 2,
                ..FakeRuntime::default()
            });
            let handle = {
                let runtime = Arc::clone(&runtime);
                std::thread::spawn(move || {
                    block_on(store.ensure(&venv_key("sha256:one"), &*runtime)).unwrap()
                })
            };
            (runtime, handle)
        })
        .collect();
    let mut venvs = Vec::new();
    let mut builds = 0;
    for (runtime, handle) in contenders {
        venvs.push(handle.join().unwrap());
        builds += runtime.builds.load(Ordering::SeqCst);
    }
    let venv = store.venv(&venv_key("sha256:one"));
    assert_eq!(
        (venvs, builds, entry_names(&store.root())),
        (vec![venv.clone(), venv], 1, vec![venv_key("sha256:one")],)
    );
}

fn legacy_venv(store: &VenvStore, identity: &str) -> PathBuf {
    let legacy = store.legacy();
    std::fs::create_dir_all(legacy.join("bin")).unwrap();
    std::fs::write(legacy.join(FAKE_MANIFEST), identity).unwrap();
    std::fs::write(legacy.join("installed-packages"), "pandas numpy").unwrap();
    legacy
}

/// The old shared venv is adopted once, by rename, when it holds the
/// current runtime: nothing rebuilds, its contents move, and the old path
/// keeps resolving through the link.
#[cfg(unix)]
#[test]
fn a_current_legacy_venv_is_adopted_by_rename() {
    let dir = tempfile::tempdir().unwrap();
    let store = VenvStore::new(dir.path().to_path_buf());
    let legacy = legacy_venv(&store, "sha256:current");
    let runtime = FakeRuntime::new("sha256:current");
    let key = venv_key("sha256:current");
    let venv = block_on(store.ensure(&key, &runtime)).unwrap();
    assert_eq!(
        (
            venv.clone(),
            runtime.builds.load(Ordering::SeqCst),
            std::fs::read_to_string(venv.join("installed-packages")).ok(),
            link_target(&store),
            std::fs::read_to_string(legacy.join("installed-packages")).ok(),
        ),
        (
            store.venv(&key),
            0,
            Some("pandas numpy".to_string()),
            Some(Path::new(VENVS_DIR).join(&key)),
            Some("pandas numpy".to_string()),
        )
    );
}

/// A legacy venv of another runtime is left exactly where it is (an older
/// binary may still use it): the current runtime builds its own, and the
/// legacy directory is not replaced by the link.
#[test]
fn a_legacy_venv_of_another_runtime_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let store = VenvStore::new(dir.path().to_path_buf());
    let legacy = legacy_venv(&store, "sha256:older");
    let runtime = FakeRuntime::new("sha256:current");
    let venv = block_on(store.ensure(&venv_key("sha256:current"), &runtime)).unwrap();
    assert_eq!(
        (
            runtime.builds.load(Ordering::SeqCst),
            FakeRuntime::recorded(&venv),
            std::fs::symlink_metadata(&legacy).unwrap().is_dir(),
            FakeRuntime::recorded(&legacy),
        ),
        (
            1,
            Some("sha256:current".to_string()),
            true,
            Some("sha256:older".to_string()),
        )
    );
}

/// A live process running a Python from the legacy venv (an older binary's
/// kernel) pins it in place: renaming it would break that kernel's later
/// imports.
#[cfg(target_os = "linux")]
#[test]
fn a_legacy_venv_a_live_process_runs_from_is_not_moved() {
    let dir = tempfile::tempdir().unwrap();
    let store = VenvStore::new(dir.path().to_path_buf());
    let legacy = legacy_venv(&store, "sha256:current");
    // `$0` puts the legacy interpreter path on the command line, as a
    // kernel's `<venv>/bin/python -m rlm.repl` does.
    let mut kernel = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg("read _")
        .arg(legacy.join("bin").join("python"))
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let runtime = FakeRuntime::new("sha256:current");
    let venv = block_on(store.ensure(&venv_key("sha256:current"), &runtime)).unwrap();
    drop(kernel.stdin.take());
    kernel.wait().unwrap();
    assert_eq!(
        (
            runtime.builds.load(Ordering::SeqCst),
            std::fs::symlink_metadata(&legacy).unwrap().is_dir(),
            FakeRuntime::recorded(&legacy),
            FakeRuntime::recorded(&venv),
        ),
        (
            1,
            true,
            Some("sha256:current".to_string()),
            Some("sha256:current".to_string()),
        )
    );
}

/// A half-built venv (an interrupted first boot) is moved out of its slot
/// and rebuilt.
#[test]
fn a_half_built_venv_is_rebuilt() {
    let dir = tempfile::tempdir().unwrap();
    let store = VenvStore::new(dir.path().to_path_buf());
    let key = venv_key("sha256:current");
    std::fs::create_dir_all(store.venv(&key).join("lib")).unwrap();
    let runtime = FakeRuntime::new("sha256:current");
    let venv = block_on(store.ensure(&key, &runtime)).unwrap();
    assert_eq!(
        (
            runtime.builds.load(Ordering::SeqCst),
            FakeRuntime::recorded(&venv),
            venv.join("lib").exists(),
            entry_names(&store.root()),
        ),
        (1, Some("sha256:current".to_string()), false, vec![key])
    );
}

/// A ready venv missing a requested skill is synced, not rebuilt.
#[test]
fn a_ready_base_only_syncs_skills() {
    struct NeedsSkill(FakeRuntime);
    impl VenvOps for NeedsSkill {
        fn ready(&self, venv: &Path) -> bool {
            self.0.syncs.load(Ordering::SeqCst) > 0 && self.0.ready(venv)
        }
        fn base_ready(&self, venv: &Path) -> bool {
            self.0.base_ready(venv)
        }
        fn records_this_runtime(&self, venv: &Path) -> bool {
            self.0.records_this_runtime(venv)
        }
        async fn sync(&self, venv: &Path) -> anyhow::Result<()> {
            self.0.sync(venv).await
        }
        async fn build(&self, venv: &Path) -> anyhow::Result<()> {
            self.0.build(venv).await
        }
        fn report(&self, _message: &str) {}
    }
    let dir = tempfile::tempdir().unwrap();
    let store = VenvStore::new(dir.path().to_path_buf());
    let key = venv_key("sha256:current");
    let venv = store.venv(&key);
    std::fs::create_dir_all(&venv).unwrap();
    std::fs::write(venv.join(FAKE_MANIFEST), "sha256:current").unwrap();
    let runtime = NeedsSkill(FakeRuntime::new("sha256:current"));
    block_on(store.ensure(&key, &runtime)).unwrap();
    assert_eq!(
        (
            runtime.0.builds.load(Ordering::SeqCst),
            runtime.0.syncs.load(Ordering::SeqCst)
        ),
        (0, 1)
    );
}

/// The key covers everything a base install is judged by, so a key's venv
/// never needs a base rebuild.
#[test]
fn the_key_is_a_stable_function_of_the_runtime() {
    let key = venv_key("sha256:abc");
    assert_eq!(
        (
            key.len(),
            is_venv_key(&key),
            key == venv_key("sha256:abc"),
            key == venv_key("sha256:abd"),
        ),
        (KEY_LEN, true, true, false)
    );
}

fn fake_venv(store: &VenvStore, key: &str, used: SystemTime) -> PathBuf {
    let venv = store.venv(key);
    std::fs::create_dir_all(venv.join("bin")).unwrap();
    let marker = std::fs::File::create(venv.join(LAST_USED_FILE)).unwrap();
    marker.set_modified(used).unwrap();
    venv
}

fn lease(venv: &Path, owner: &str, name: &str) {
    std::fs::create_dir_all(venv.join(LEASES_DIR)).unwrap();
    std::fs::write(venv.join(LEASES_DIR).join(name), owner).unwrap();
}

/// A pid that named a process which has exited.
fn dead_pid() -> u32 {
    let mut child = std::process::Command::new(if cfg!(windows) { "cmd" } else { "true" })
        .args(if cfg!(windows) {
            &["/C", "exit"][..]
        } else {
            &[][..]
        })
        .spawn()
        .unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

/// Pruning removes only venvs nothing may still use: the current one, the
/// newest others, any used within the window, the link's target and any a
/// live process leases are kept; a dead lease protects nothing.
#[test]
fn pruning_keeps_every_venv_that_may_be_in_use() {
    let dir = tempfile::tempdir().unwrap();
    let store = VenvStore::new(dir.path().to_path_buf());
    let now = SystemTime::now();
    let days = |n: u64| now - Duration::from_hours(24 * n);
    let key = |n: u8| format!("{n:0>20}");
    // Current, but last used long ago: kept.
    fake_venv(&store, &key(1), days(90));
    // The KEEP_NEWEST most recent others: kept.
    fake_venv(&store, &key(2), days(1));
    fake_venv(&store, &key(3), days(2));
    // Not among the newest, but used within the window: kept.
    fake_venv(&store, &key(4), days(6));
    // Old, leased by this (live) process: kept.
    let leased = fake_venv(&store, &key(5), days(30));
    lease(&leased, &format!("{}\n", std::process::id()), "live");
    // Old, leased by a dead process: removed (and so is its lease).
    let dead = fake_venv(&store, &key(6), days(31));
    lease(&dead, &format!("{}\n", dead_pid()), "dead");
    // Old and unused: removed.
    fake_venv(&store, &key(7), days(40));
    // Old, but the legacy link's target: kept.
    #[cfg(unix)]
    {
        fake_venv(&store, &key(8), days(50));
        std::os::unix::fs::symlink(Path::new(VENVS_DIR).join(key(8)), store.legacy()).unwrap();
    }
    // Not a venv key: never touched.
    std::fs::create_dir_all(store.root().join("not-a-venv")).unwrap();

    let removed = store.prune(&key(1), now);
    let mut kept = vec![
        key(1),
        key(2),
        key(3),
        key(4),
        key(5),
        "not-a-venv".to_string(),
    ];
    if cfg!(unix) {
        kept.push(key(8));
    }
    kept.sort();
    assert_eq!(
        (removed, entry_names(&store.root())),
        (vec![key(6), key(7)], kept)
    );
}

/// Pruning skips a venv whose bootstrap lock a live process holds (it is
/// being built or synced right now).
#[test]
fn pruning_skips_a_venv_being_bootstrapped() {
    let dir = tempfile::tempdir().unwrap();
    let store = VenvStore::new(dir.path().to_path_buf());
    let now = SystemTime::now();
    let days = |n: u64| now - Duration::from_hours(24 * n);
    let key = |n: u8| format!("{n:0>20}");
    fake_venv(&store, &key(1), days(90));
    fake_venv(&store, &key(2), days(50));
    fake_venv(&store, &key(3), days(51));
    fake_venv(&store, &key(4), days(60));
    let _held = block_on(crate::kernel::bootstrap::dir_lock::acquire_bootstrap_lock(
        &store.venv(&key(4)),
    ))
    .unwrap();
    // 1 is current, 2 and 3 are the newest others, 4 is old but locked.
    assert_eq!(store.prune(&key(1), now), Vec::<String>::new());
}

/// A process that resolves a venv leaves a lease naming itself.
#[test]
fn resolving_a_venv_leases_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = VenvStore::new(dir.path().to_path_buf());
    let runtime = FakeRuntime::new("sha256:current");
    let venv = block_on(store.ensure(&venv_key("sha256:current"), &runtime)).unwrap();
    let lease = venv.join(LEASES_DIR).join(std::process::id().to_string());
    assert_eq!(
        std::fs::read_to_string(lease).ok(),
        Some(owner_content()),
        "the lease records this process"
    );
    assert!(live_lease(&venv, 0));
    assert!(!live_lease(&venv, std::process::id()));
}
