//! Daemon discovery: find every product daemon in this state root and probe
//! it for identity and session count — the OS census of listening sockets
//! plus a sweep of the default socket dir (orphaned files included).
//! Everything is scoped to one [`DaemonStateRoot`]. Under a test harness a
//! [`Containment`] guard also refuses this box's real daemon dirs outright,
//! whatever the root says; the shipped binary has no such list (TS parity) and
//! always manages the daemon in its own default socket dir.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use pa_types::daemon::DaemonCommand;
use serde::{Deserialize, Serialize};

use crate::config;
use crate::daemon_client::DaemonClient;

mod format;
mod kill;
pub(crate) mod plan;
pub(crate) mod scan;
pub(crate) mod stop;

pub(crate) use format::format_daemon_list_table;
pub(crate) use stop::{run_ps, run_reap, run_shutdown_all};

#[derive(Debug, Clone)]
pub(crate) struct DiscoveredDaemonProcess {
    pub pid: u32,
    pub socket_path: PathBuf,
    pub uptime_seconds: Option<u64>,
}

/// The state root an invocation reads: the agent dir and the default socket
/// dir, both following HOME/TMPDIR/agent-dir overrides, plus the containment
/// guard the invocation runs under.
#[derive(Debug, Clone)]
pub(crate) struct DaemonStateRoot {
    pub agent_dir: PathBuf,
    pub socket_dir: PathBuf,
    pub default_socket_path: PathBuf,
    pub containment: Containment,
}

/// Set to `1` by test harnesses on every `prime-agent` they spawn: discovery
/// then refuses [`TEST_NEVER_TOUCH_SOCKET_DIRS`]. `0` opts a `cargo run` out.
pub const DISCOVERY_CONTAINMENT_ENV: &str = "PRIME_AGENT_DISCOVERY_CONTAINMENT";

/// Socket dirs a test must never discover, probe, signal, or unlink: this
/// box's live daemons. A sandbox `HOME` with the ambient `TMPDIR` resolves
/// `current_state_root()` onto `/tmp/prime-agent-<uid>`, so root matching
/// alone cannot keep a test off the real daemon. The `-0` entries are the
/// uid-0 twins. These are also the product-default socket dirs for uid
/// 1000/0, so the list must never apply to the shipped binary.
pub(crate) const TEST_NEVER_TOUCH_SOCKET_DIRS: &[&str] = &[
    "/tmp/prime-agent-1000",
    "/tmp/mission-tmp/prime-agent-1000",
    "/tmp/mission-daemon",
    "/tmp/prime-agent-0",
    "/tmp/mission-tmp/prime-agent-0",
];

/// The socket dirs an invocation refuses outright, ahead of root matching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Containment {
    never_touch: Vec<PathBuf>,
}

impl Containment {
    /// The shipped binary: nothing beyond the state root is refused, so the
    /// daemon in the invocation's own default socket dir is always reachable.
    pub(crate) fn product() -> Self {
        Self {
            never_touch: Vec::new(),
        }
    }

    /// Any process under a test harness.
    pub(crate) fn test_harness() -> Self {
        Self {
            never_touch: TEST_NEVER_TOUCH_SOCKET_DIRS
                .iter()
                .map(PathBuf::from)
                .collect(),
        }
    }

    /// This process's guard: always on in unit tests, else read from the env.
    pub(crate) fn current() -> Self {
        if cfg!(test) {
            return Self::test_harness();
        }
        Self::resolve(|key| std::env::var_os(key), Self::test_harness())
    }

    /// `harness` when a harness set [`DISCOVERY_CONTAINMENT_ENV`] to `1`, or
    /// when cargo launched the process (`cargo test`/`cargo run` set
    /// `CARGO_MANIFEST_DIR` at runtime and every child inherits it) - the
    /// backstop for a harness that forgot the marker. `0` turns it off.
    /// Otherwise [`Containment::product`].
    fn resolve(env: impl Fn(&str) -> Option<OsString>, harness: Self) -> Self {
        let under_test = match env(DISCOVERY_CONTAINMENT_ENV) {
            Some(value) if value == "1" => true,
            Some(value) if value == "0" => false,
            _ => env("CARGO_MANIFEST_DIR").is_some(),
        };
        if under_test { harness } else { Self::product() }
    }

    pub(crate) fn forbids(&self, path: &Path) -> bool {
        self.never_touch.iter().any(|dir| path.starts_with(dir))
    }
}

/// True when `path` is refused for this root: by the root's own guard, or -
/// in unit tests, whatever a fixture root says - by the test list.
fn root_forbids(root: &DaemonStateRoot, path: &Path) -> bool {
    root.containment.forbids(path) || is_never_touch(path)
}

/// The root-less check for probes and unlinks: this process's guard.
pub(crate) fn is_never_touch(path: &Path) -> bool {
    Containment::current().forbids(path)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum DaemonStatus {
    Current,
    Stale,
    Unreachable,
    OrphanFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PidSource {
    Listener,
    Hello,
}

/// One discovered daemon, probed; field order matches the TS JSON shape.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DaemonInfo {
    pub socket_path: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uptime_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub protocol_version: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executable_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid_source: Option<PidSource>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_count: Option<u64>,
    pub status: DaemonStatus,
    pub is_default: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_tracked_workers: Option<bool>,
}

pub(crate) fn current_state_root() -> DaemonStateRoot {
    DaemonStateRoot {
        agent_dir: config::get_agent_dir(),
        socket_dir: pa_daemon::platform::socket_dir(),
        default_socket_path: pa_daemon::platform::default_daemon_socket_path(),
        containment: Containment::current(),
    }
}

/// A socket belongs to the root when it is the default path, sits in the socket
/// dir, or anywhere inside the agent dir (TS ownership registry not ported).
fn state_root_matches(root: &DaemonStateRoot, socket_path: &Path) -> bool {
    if root_forbids(root, socket_path) {
        return false;
    }
    #[cfg(windows)]
    {
        // Windows daemons share one named pipe per machine, so there is
        // nothing to scope beyond the containment guard.
        let _ = (root, socket_path);
        true
    }
    #[cfg(not(windows))]
    {
        if socket_path == root.default_socket_path || socket_path.parent() == Some(&root.socket_dir)
        {
            return true;
        }
        inside(socket_path.parent(), &root.agent_dir)
    }
}

#[cfg(not(windows))]
fn inside(directory: Option<&Path>, parent: &Path) -> bool {
    let Some(directory) = directory else {
        return false;
    };
    match directory.strip_prefix(parent) {
        Ok(rest) => !rest.as_os_str().is_empty() || directory == parent,
        Err(_) => false,
    }
}

/// Worker sockets: `worker-*.sock` in the given socket dir (never the
/// supervisor's own). The dir comes from the state root, never the ambient env.
pub(crate) fn is_worker_socket_path(socket_path: &Path, socket_dir: &Path) -> bool {
    if socket_path.parent() != Some(socket_dir) {
        return false;
    }
    let Some(name) = socket_path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    name.starts_with("worker-")
        && std::path::Path::new(name)
            .extension()
            .is_some_and(|ext| ext == "sock")
}

/// Listening daemons in this state root. The census is filtered to the root
/// inside the scan: a scan from one root can never see another root's daemon.
pub(crate) fn scan_listening_daemons(root: &DaemonStateRoot) -> Vec<DiscoveredDaemonProcess> {
    scan::scan_all_listening_daemons(config::APP_NAME, root)
}

/// True when the pid still listens on exactly this socket: a fresh scan against
/// the same root, so a re-probe sees the discovery's listener set.
pub(crate) fn is_daemon_process_listening(
    pid: u32,
    socket_path: &Path,
    root: &DaemonStateRoot,
) -> bool {
    scan_listening_daemons(root)
        .iter()
        .any(|daemon| daemon.pid == pid && daemon.socket_path == socket_path)
}

/// Socket files in the given socket dir: live daemons and orphaned files alike.
/// Never-touch paths are filtered here too.
#[cfg(unix)]
fn scan_socket_dir(root: &DaemonStateRoot) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(&root.socket_dir) else {
        return Vec::new();
    };
    let mut sockets = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if is_socket_file(&path) && !root_forbids(root, &path) {
            sockets.push(path);
        }
    }
    sockets
}

/// Windows daemon endpoints are named pipes: there is no socket directory to
/// sweep, and discovery comes from tracked worker descriptors and the
/// default pipe (see [`discover_daemons_with`]; TS `scanSocketDir` returns
/// [] on win32).
#[cfg(not(unix))]
fn scan_socket_dir(_root: &DaemonStateRoot) -> Vec<PathBuf> {
    Vec::new()
}

#[cfg(unix)]
fn is_socket_file(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_socket())
}

#[derive(Debug, Clone)]
pub(crate) struct TrackedWorker {
    pub descriptor_path: PathBuf,
    pub supervisor_socket_path: PathBuf,
    pub worker_socket_path: PathBuf,
    pub pid: u32,
    pub process_start_id: Option<String>,
    pub recovery_journal_path: PathBuf,
}

/// Every tracked worker recorded in this agent dir: `daemon-workers/*/<worker>.json`
/// descriptors (`supervisor-config` has no `.json` and is skipped).
pub(crate) fn find_all_tracked_workers(agent_dir: &Path) -> Vec<TrackedWorker> {
    let root = agent_dir.join("daemon-workers");
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut workers = Vec::new();
    for directory in entries.flatten() {
        if !directory.file_type().is_ok_and(|kind| kind.is_dir()) {
            continue;
        }
        let Ok(files) = std::fs::read_dir(directory.path()) else {
            continue;
        };
        for file in files.flatten() {
            if file.file_name().to_string_lossy().ends_with(".json") {
                if let Some(worker) = read_tracked_worker(&file.path()) {
                    workers.push(worker);
                }
            }
        }
    }
    workers
}

/// Parse one descriptor file; invalid or concurrently removed descriptors
/// are not safe shutdown targets and are skipped.
fn read_tracked_worker(path: &Path) -> Option<TrackedWorker> {
    let content = std::fs::read_to_string(path).ok()?;
    let descriptor: serde_json::Value = serde_json::from_str(&content).ok()?;
    let pid = descriptor.get("pid")?.as_u64()?;
    if pid == 0 {
        return None;
    }
    let worker_id = descriptor.get("workerId")?.as_str()?;
    if worker_id.is_empty() {
        return None;
    }
    Some(TrackedWorker {
        descriptor_path: path.to_path_buf(),
        supervisor_socket_path: PathBuf::from(descriptor.get("supervisorSocketPath")?.as_str()?),
        worker_socket_path: PathBuf::from(descriptor.get("socketPath")?.as_str()?),
        pid: pid as u32,
        process_start_id: descriptor
            .get("processStartId")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        recovery_journal_path: PathBuf::from(descriptor.get("recoveryJournalPath")?.as_str()?),
    })
}

#[derive(Debug, Default)]
pub(crate) struct ProbeResult {
    version: Option<String>,
    protocol_version: Option<u64>,
    schema_id: Option<String>,
    build_id: Option<String>,
    executable_path: Option<String>,
    session_count: Option<u64>,
    supervisor_pid: Option<u32>,
    supervisor_process_start_id: Option<String>,
    reachable: bool,
}

/// Probe one socket: connect (300ms), read the hello (1500ms), and ask for the
/// session count over `list` (30s when greeted, else 1500ms).
pub(crate) fn probe_daemon(socket_path: &Path) -> ProbeResult {
    if is_never_touch(socket_path) {
        // Containment: under a test harness, never even connect to a
        // forbidden path, whatever the caller's root says.
        return ProbeResult::default();
    }
    let Ok(mut client) = DaemonClient::connect_probe(socket_path) else {
        return ProbeResult::default();
    };
    let mut probe = ProbeResult {
        reachable: true,
        ..ProbeResult::default()
    };
    let greeted = client.wait_for_hello(HELLO_TIMEOUT_MS).ok().map(|hello| {
        probe.version = hello
            .get("appVersion")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        probe.protocol_version = hello
            .get("protocol")
            .and_then(|protocol| protocol.get("version"))
            .and_then(serde_json::Value::as_u64);
        probe.schema_id = hello
            .get("schemaId")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        if let Some(runtime) = hello.get("runtime") {
            probe.build_id = runtime
                .get("buildId")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            probe.executable_path = ["launcherPath", "entrypointPath", "executablePath"]
                .iter()
                .find_map(|key| {
                    runtime
                        .get(*key)
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_string)
                });
        }
        probe.supervisor_pid = hello
            .get("supervisorPid")
            .and_then(serde_json::Value::as_u64)
            .and_then(|pid| u32::try_from(pid).ok());
        probe.supervisor_process_start_id = hello
            .get("supervisorProcessStartId")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
    });
    let timeout_ms = if greeted.is_some() {
        LIST_TIMEOUT_MS
    } else {
        HELLO_TIMEOUT_MS
    };
    let list = DaemonCommand::List {
        id: None,
        all: None,
        cwd: None,
        session_dir: None,
        include_client_owned: None,
        include_remote_mesh: None,
        rest: serde_json::Map::default(),
    };
    if let Ok(response) = client.request_with_timeout(list, timeout_ms) {
        if response.success {
            probe.session_count = response
                .data
                .as_ref()
                .and_then(|data| data.get("sessions"))
                .and_then(serde_json::Value::as_array)
                .map(|sessions| sessions.len() as u64);
        }
    }
    probe
}

const HELLO_TIMEOUT_MS: u64 = 1_500;
const LIST_TIMEOUT_MS: u64 = 30_000;

/// Reachable daemons are `current` only when the protocol, schema, and
/// app version all match this build.
fn classify_reachable(probe: &ProbeResult) -> DaemonStatus {
    if probe.protocol_version == Some(pa_types::daemon::DAEMON_PROTOCOL_VERSION)
        && probe.schema_id.as_deref() == Some(pa_types::daemon::DAEMON_SCHEMA_ID)
        && probe.version.as_deref() == Some(config::version())
    {
        DaemonStatus::Current
    } else {
        DaemonStatus::Stale
    }
}

/// The hello's supervisor pid, but only when it is alive and its process
/// identity still matches (the start-id gate defeats pid reuse).
pub(crate) fn verify_hello_supervisor_pid(
    pid: Option<u32>,
    expected_process_start_id: Option<&str>,
) -> Option<u32> {
    let pid = pid?;
    if pid == 0 {
        return None;
    }
    match pa_types::platform::process::is_process_alive(pid) {
        Ok(true) => {}
        // EPERM-equivalent: the process exists but is not ours to signal.
        Ok(false) | Err(_) => return None,
    }
    if let Some(expected) = expected_process_start_id {
        if pa_types::platform::process::process_start_id(pid).as_deref() != Some(expected) {
            return None;
        }
    }
    Some(pid)
}

/// Discover every daemon in this state root and probe each. The root is
/// explicit (the CLI passes the env-resolved root; tests pass fixture dirs).
pub(crate) fn discover_daemons(root: &DaemonStateRoot) -> Vec<DaemonInfo> {
    discover_daemons_with(root, cfg!(windows))
}

/// [`discover_daemons`] with the endpoint model explicit. With
/// `named_pipes` (Windows) there is no listener census and no socket dir to
/// sweep, so the fixed daemon pipe is always probed: an idle daemon with no
/// tracked workers is otherwise invisible and `shutdown` would report
/// success without stopping it. A pipe exists only while its server holds
/// it, so an unanswered one with no listener or tracked worker is simply
/// absent - there is no orphan file to report or remove.
fn discover_daemons_with(root: &DaemonStateRoot, named_pipes: bool) -> Vec<DaemonInfo> {
    let mut process_by_socket = std::collections::HashMap::new();
    for daemon in scan_listening_daemons(root) {
        if is_worker_socket_path(&daemon.socket_path, &root.socket_dir) {
            continue;
        }
        process_by_socket.insert(daemon.socket_path.clone(), daemon);
    }
    let tracked = find_all_tracked_workers(&root.agent_dir);
    let worker_sockets: BTreeSet<PathBuf> = tracked
        .iter()
        .map(|worker| worker.supervisor_socket_path.clone())
        .collect();
    let mut sockets: BTreeSet<PathBuf> = process_by_socket
        .keys()
        .cloned()
        .chain(
            scan_socket_dir(root)
                .into_iter()
                .filter(|path| !is_worker_socket_path(path, &root.socket_dir)),
        )
        .chain(worker_sockets.iter().cloned())
        .chain(named_pipes.then(|| root.default_socket_path.clone()))
        .collect();
    sockets.retain(|path| state_root_matches(root, path));

    let mut infos: Vec<DaemonInfo> = sockets
        .into_iter()
        .filter_map(|socket_path| {
            let proc = process_by_socket.get(&socket_path);
            let probe = probe_daemon(&socket_path);
            let pid = proc.map(|daemon| daemon.pid).or_else(|| {
                verify_hello_supervisor_pid(
                    probe.supervisor_pid,
                    probe.supervisor_process_start_id.as_deref(),
                )
            });
            let has_tracked_workers = worker_sockets.contains(&socket_path);
            let status = if probe.reachable {
                classify_reachable(&probe)
            } else if proc.is_some() || has_tracked_workers {
                DaemonStatus::Unreachable
            } else if named_pipes {
                return None;
            } else {
                DaemonStatus::OrphanFile
            };
            Some(DaemonInfo {
                pid_source: pid.map(|_| {
                    if proc.is_some() {
                        PidSource::Listener
                    } else {
                        PidSource::Hello
                    }
                }),
                is_default: socket_path == root.default_socket_path,
                socket_path,
                pid,
                uptime_seconds: proc.and_then(|daemon| daemon.uptime_seconds),
                version: probe.version,
                protocol_version: probe.protocol_version,
                schema_id: probe.schema_id,
                build_id: probe.build_id,
                executable_path: probe.executable_path,
                session_count: probe.session_count,
                status,
                has_tracked_workers: has_tracked_workers.then_some(true),
            })
        })
        .collect();
    sort_daemons(&mut infos);
    infos
}

/// Default first, then by status severity, then by socket path.
pub(crate) fn sort_daemons(infos: &mut [DaemonInfo]) {
    infos.sort_by(|left, right| {
        right
            .is_default
            .cmp(&left.is_default)
            .then(left.status.cmp(&right.status))
            .then(left.socket_path.cmp(&right.socket_path))
    });
}

/// The quiet-period decision for the shutdown residual sweep: a sweep
/// completes once no listener has been seen for a full quiet period.
pub(crate) fn evaluate_shutdown_quiet_period(now_ms: u128, quiet_since_ms: Option<u128>) -> bool {
    quiet_since_ms
        .is_some_and(|quiet_since| now_ms.saturating_sub(quiet_since) >= SHUTDOWN_QUIET_PERIOD_MS)
}

/// How long the residual sweep must see no listener before it succeeds.
const SHUTDOWN_QUIET_PERIOD_MS: u128 = 1_000;

#[cfg(test)]
mod tests {
    use super::*;

    /// A synthetic state root inside a fixture directory: unit tests
    /// never touch the ambient environment's real agent dir or socket dir.
    #[cfg(not(windows))]
    fn fixture_root(dir: &Path) -> DaemonStateRoot {
        DaemonStateRoot {
            agent_dir: dir.join("agent"),
            socket_dir: dir.join("agent").join("sockets"),
            default_socket_path: dir.join("agent").join("sockets").join("daemon.sock"),
            containment: Containment::current(),
        }
    }

    #[test]
    fn worker_socket_paths_are_scoped_to_the_given_socket_dir() {
        let dir = Path::new("/fixture/agent/sockets");
        assert!(is_worker_socket_path(&dir.join("worker-abc-123.sock"), dir));
        assert!(!is_worker_socket_path(&dir.join("daemon.sock"), dir));
        assert!(!is_worker_socket_path(
            &dir.join("nested/worker-a.sock"),
            dir
        ));
        assert!(!is_worker_socket_path(
            &dir.join("worker-a.sock"),
            Path::new("/fixture/other-sockets")
        ));
    }

    #[cfg(not(windows))]
    #[test]
    fn state_root_matches_own_paths_only() {
        let root = fixture_root(Path::new("/fixture"));
        assert!(state_root_matches(&root, &root.default_socket_path));
        assert!(state_root_matches(
            &root,
            &root.socket_dir.join("daemon.sock")
        ));
        assert!(state_root_matches(
            &root,
            &root.agent_dir.join("nested/daemon.sock")
        ));
        assert!(!state_root_matches(
            &root,
            Path::new("/tmp/other-root/daemon.sock")
        ));
    }

    #[test]
    fn never_touch_paths_are_excluded_even_when_the_root_points_at_them() {
        // A fixture root claiming product containment still loses to the
        // unit-test floor: this is a pure path check, nothing is opened.
        for dir in TEST_NEVER_TOUCH_SOCKET_DIRS {
            let dir = Path::new(dir);
            let root = DaemonStateRoot {
                agent_dir: dir.to_path_buf(),
                socket_dir: dir.to_path_buf(),
                default_socket_path: dir.join("daemon.sock"),
                containment: Containment::product(),
            };
            assert!(
                !state_root_matches(&root, &root.default_socket_path),
                "containment must beat root matching for {dir:?}"
            );
            assert!(!state_root_matches(
                &root,
                &root.socket_dir.join("worker-x.sock")
            ));
        }
        assert!(is_never_touch(Path::new("/tmp/mission-daemon")));
        assert!(is_never_touch(Path::new("/tmp/mission-daemon/daemon.sock")));
        assert!(is_never_touch(Path::new("/tmp/prime-agent-1000")));
        assert!(!is_never_touch(Path::new("/tmp")));
        assert!(!is_never_touch(Path::new("/tmp/other/daemon.sock")));
    }

    #[test]
    fn a_scan_rooted_on_a_never_touch_dir_surfaces_no_listeners() {
        // The ambient real daemon's workers listen under these dirs, owned
        // by real `prime-agent` processes: root matching alone would find them.
        for dir in TEST_NEVER_TOUCH_SOCKET_DIRS {
            let dir = *dir;
            let root = DaemonStateRoot {
                agent_dir: PathBuf::from(dir),
                socket_dir: PathBuf::from(dir),
                default_socket_path: PathBuf::from(dir).join("daemon.sock"),
                containment: Containment::current(),
            };
            assert!(
                scan_listening_daemons(&root).is_empty(),
                "scan rooted at {dir} must not surface listeners"
            );
        }
    }

    #[test]
    fn a_probe_never_connects_to_a_never_touch_path() {
        // When the guard works, this never opens a connection; a regression
        // would probe the live mission daemon once and fail the assert.
        let probe = probe_daemon(Path::new("/tmp/mission-daemon/daemon.sock"));
        assert!(!probe.reachable);
    }

    #[test]
    fn unit_tests_always_run_under_the_harness_guard() {
        assert_eq!(Containment::current(), Containment::test_harness());
    }

    /// The env a shipped binary sees: neither the harness marker nor cargo.
    fn product_env(_key: &str) -> Option<OsString> {
        None
    }

    #[test]
    fn a_product_invocation_never_refuses_its_own_default_socket_dir() {
        // Pure path checks against the product resolution: the uid-1000
        // and uid-0 default socket dirs are reachable, nothing is opened.
        let product = Containment::resolve(product_env, Containment::test_harness());
        assert_eq!(product, Containment::product());
        for socket in [
            "/tmp/prime-agent-1000/daemon.sock",
            "/tmp/prime-agent-0/daemon.sock",
        ] {
            assert!(!product.forbids(Path::new(socket)), "{socket}");
        }
    }

    #[test]
    fn the_harness_marker_and_cargo_switch_the_guard_on() {
        let harness = Containment::test_harness;
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |key: &str| {
                pairs
                    .iter()
                    .find(|(name, _)| *name == key)
                    .map(|(_, value)| OsString::from(value))
            }
        };
        let marker = env(&[(DISCOVERY_CONTAINMENT_ENV, "1")]);
        assert_eq!(Containment::resolve(marker, harness()), harness());
        let cargo = env(&[("CARGO_MANIFEST_DIR", "/src/pa-cli")]);
        assert_eq!(Containment::resolve(cargo, harness()), harness());
        let opted_out = env(&[
            (DISCOVERY_CONTAINMENT_ENV, "0"),
            ("CARGO_MANIFEST_DIR", "/src/pa-cli"),
        ]);
        assert_eq!(
            Containment::resolve(opted_out, harness()),
            Containment::product()
        );
        let unknown = env(&[(DISCOVERY_CONTAINMENT_ENV, "yes")]);
        assert_eq!(
            Containment::resolve(unknown, harness()),
            Containment::product()
        );
    }

    /// Bind a socket that accepts and drops every connection: a reachable,
    /// non-greeting daemon (the probe skips the hello wait).
    #[cfg(unix)]
    fn serve_dropping_connections(socket: &Path) {
        let listener = std::os::unix::net::UnixListener::bind(socket).expect("bind");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                drop(stream);
            }
        });
    }

    #[cfg(unix)]
    #[test]
    fn a_product_invocation_discovers_the_live_daemon_in_its_default_dir() {
        // A temp dir stands in for `/tmp/prime-agent-<uid>`: the harness
        // list names it the way the real list names the uid-1000 default.
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut root = fixture_root(tmp.path());
        std::fs::create_dir_all(&root.socket_dir).expect("socket dir");
        serve_dropping_connections(&root.default_socket_path);
        let stand_in_harness = Containment {
            never_touch: vec![root.socket_dir.clone()],
        };

        root.containment = Containment::resolve(product_env, stand_in_harness.clone());
        let infos = discover_daemons(&root);
        assert_eq!(infos.len(), 1, "the product sees its default daemon");
        assert_eq!(infos[0].socket_path, root.default_socket_path);
        assert!(infos[0].is_default);
        assert_eq!(infos[0].status, DaemonStatus::Stale);

        let marker = |key: &str| (key == DISCOVERY_CONTAINMENT_ENV).then(|| OsString::from("1"));
        root.containment = Containment::resolve(marker, stand_in_harness);
        assert!(
            discover_daemons(&root).is_empty(),
            "a harness-spawned invocation still refuses the guarded dir"
        );
    }

    #[cfg(unix)]
    #[test]
    fn discovery_reports_orphan_files_inside_the_given_root_only() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = fixture_root(tmp.path());
        std::fs::create_dir_all(&root.socket_dir).expect("socket dir");
        // Bind and drop: the listener's socket file stays behind (std
        // does not unlink it), an orphan file in the fixture root.
        drop(
            std::os::unix::net::UnixListener::bind(root.socket_dir.join("leftover.sock"))
                .expect("bind"),
        );
        let infos = discover_daemons(&root);
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].socket_path, root.socket_dir.join("leftover.sock"));
        assert_eq!(infos[0].status, DaemonStatus::OrphanFile);
        assert!(!infos[0].is_default);
    }

    #[cfg(unix)]
    #[test]
    fn named_pipe_discovery_probes_the_default_endpoint_without_tracked_workers() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut root = fixture_root(tmp.path());
        // Outside the socket dir, so the socket-dir sweep cannot find it:
        // only the named-pipe arm's fixed endpoint does, like the Windows
        // daemon pipe.
        root.default_socket_path = root.agent_dir.join("daemon-pipe.sock");
        std::fs::create_dir_all(&root.socket_dir).expect("socket dir");

        // No daemon on the pipe: nothing is reported, not a fake orphan.
        assert!(discover_daemons_with(&root, true).is_empty());

        // An idle daemon with no tracked workers. Accepted connections are
        // dropped at once, so the probe is reachable without the hello wait.
        serve_dropping_connections(&root.default_socket_path);
        assert!(
            discover_daemons_with(&root, false).is_empty(),
            "the unix arm only sees socket-dir files and listeners"
        );
        let infos = discover_daemons_with(&root, true);
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].socket_path, root.default_socket_path);
        assert!(infos[0].is_default);
        assert_eq!(infos[0].has_tracked_workers, None);
        assert_eq!(infos[0].status, DaemonStatus::Stale);
    }

    #[test]
    fn quiet_period_completes_after_the_threshold() {
        assert!(evaluate_shutdown_quiet_period(1_500, Some(500)));
        assert!(!evaluate_shutdown_quiet_period(1_200, Some(500)));
        assert!(!evaluate_shutdown_quiet_period(2_000, None));
    }
}
