//! Product-wide constants and environment handling.

use std::path::PathBuf;

pub const APP_NAME: &str = "prime-agent";

pub const CONFIG_DIR_NAME: &str = ".prime/agent";

pub const ENV_AGENT_DIR: &str = "PRIME_AGENT_CODING_AGENT_DIR";

pub const ENV_SESSION_DIR: &str = "PRIME_AGENT_SESSION_DIR";

/// `PRIME_AGENT_DAEMON_SOCKET`: overrides the daemon socket path when no explicit
/// `--daemon-socket` flag is given. The launcher pins it so the Rust daemon runs
/// beside — never replacing — the TS daemon (a Rust CLI that found the TS daemon
/// on the default socket would treat the schema-id mismatch as staleness).
pub const ENV_DAEMON_SOCKET: &str = "PRIME_AGENT_DAEMON_SOCKET";

/// The daemon socket path: an explicit `--daemon-socket` flag wins, then
/// [`ENV_DAEMON_SOCKET`], then the per-user default.
pub fn resolve_daemon_socket_path(daemon_socket: Option<&str>) -> PathBuf {
    daemon_socket
        .map(expand_tilde_path)
        .or_else(|| {
            std::env::var_os(ENV_DAEMON_SOCKET)
                .filter(|value| !value.is_empty())
                .as_deref()
                .map(expand_tilde_path_os)
        })
        .unwrap_or_else(pa_daemon::socket::default_daemon_socket_path)
}

/// [`expand_tilde_path`] over a raw environment value: anything but a
/// tilde-prefixed value passes through as the original bytes — a lossy read
/// here would rewrite a non-UTF-8 socket path.
pub fn expand_tilde_path_os(value: &std::ffi::OsStr) -> PathBuf {
    if value.to_str().is_some_and(|path| path.starts_with('~')) {
        return expand_tilde_path(&value.to_string_lossy());
    }
    PathBuf::from(value)
}

pub const ENV_LEGACY_SESSION_DIR: &str = "PRIME_AGENT_CODING_AGENT_SESSION_DIR";

/// The version compiled into this build, used when no packaged manifest
/// overrides it.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The product version: the packaged `package.json` manifest next to the
/// executable wins, falling back to the compiled-in version.
pub fn version() -> &'static str {
    static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    VERSION.get_or_init(|| match packaged_manifest_version() {
        Some(version) => version,
        None => crate::config::VERSION.to_string(),
    })
}

/// The packaged package-dir (`PI_PACKAGE_DIR` wins, else the directory of
/// the executable, launcher symlinks resolved through pa-core's
/// `exe_dir_of`).
fn package_dir() -> PathBuf {
    if let Ok(env_dir) = std::env::var("PI_PACKAGE_DIR") {
        if !env_dir.is_empty() {
            return expand_tilde_path(&env_dir);
        }
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| pa_core::packages::exe_dir_of(&exe))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn packaged_manifest_version() -> Option<String> {
    let manifest = std::fs::read_to_string(package_dir().join("package.json")).ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&manifest).ok()?;
    let version = parsed.get("version")?.as_str()?.trim();
    (!version.is_empty()).then(|| version.to_string())
}

pub const ENV_OFFLINE: &str = "PI_OFFLINE";

pub const ENV_STARTUP_BENCHMARK: &str = "PI_STARTUP_BENCHMARK";

/// Expand a leading `~`, `~/`, or (Windows) `~\` segment against the
/// home directory.
pub fn expand_tilde_path(path: &str) -> PathBuf {
    let Some(home) = pa_types::platform::home_dir() else {
        return PathBuf::from(path);
    };
    if path == "~" {
        return home;
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home.join(rest);
    }
    #[cfg(windows)]
    if let Some(rest) = path.strip_prefix("~\\") {
        return home.join(rest);
    }
    PathBuf::from(path)
}

/// The agent state directory: the `PRIME_AGENT_CODING_AGENT_DIR` override, else
/// `~/.prime/agent`.
///
/// # Panics
///
/// In a test process, when the directory is the real home's
/// (`pa_types::platform::test_isolation`).
pub fn get_agent_dir() -> PathBuf {
    pa_types::platform::test_isolation::prepare();
    let dir = match std::env::var(ENV_AGENT_DIR) {
        Ok(dir) if !dir.is_empty() => expand_tilde_path(&dir),
        _ => pa_types::platform::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(CONFIG_DIR_NAME),
    };
    pa_types::platform::test_isolation::guard_state_path("agent dir", &dir);
    dir
}

pub fn get_session_dir_env_override() -> Option<PathBuf> {
    std::env::var(ENV_SESSION_DIR)
        .ok()
        .or_else(|| std::env::var(ENV_LEGACY_SESSION_DIR).ok())
        .filter(|value| !value.is_empty())
        .map(|value| expand_tilde_path(&value))
}

/// Truthy environment flag check: only `1`, `true`, and `yes`
/// (case-insensitive) count.
pub fn is_truthy_env_flag(value: Option<&str>) -> bool {
    match value {
        None => false,
        Some(value) => {
            let lower = value.to_ascii_lowercase();
            value == "1" || lower == "true" || lower == "yes"
        }
    }
}

/// The env-mutating tests serialize on this one crate-wide lock: two
/// private locks guarding the same variable do not serialize each other,
/// so every test in this crate's binary that mutates the process env
/// (HOME, the credential keys, ...) takes it.
#[cfg(test)]
pub(crate) fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Puts one env variable back the way it was when dropped, on every
    /// exit path (a failed assertion included). Create it while holding
    /// [`env_lock`] and keep the lock guard alive longer (declare it first).
    struct RestoreEnv {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl RestoreEnv {
        fn new(key: &'static str) -> Self {
            Self {
                key,
                previous: std::env::var_os(key),
            }
        }
    }

    impl Drop for RestoreEnv {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    /// The flag/env/default order is the co-existence contract (the launcher pins the env).
    #[test]
    fn daemon_socket_resolution_prefers_flag_then_env_then_default() {
        // The default reads the agent dir: hold the env lock so a concurrent
        // agent-dir test cannot move it between the two reads.
        let _env = env_lock();
        let default = pa_daemon::socket::default_daemon_socket_path();
        std::env::set_var(ENV_DAEMON_SOCKET, "/tmp/rust-launcher.sock");
        assert_eq!(
            resolve_daemon_socket_path(None),
            PathBuf::from("/tmp/rust-launcher.sock")
        );
        assert_eq!(
            resolve_daemon_socket_path(Some("/tmp/flag.sock")),
            PathBuf::from("/tmp/flag.sock")
        );
        std::env::remove_var(ENV_DAEMON_SOCKET);
        assert_eq!(resolve_daemon_socket_path(None), default);
    }

    /// Upstream #768/#786/#815: the default daemon endpoint is keyed by the
    /// agent dir, so a second `PRIME_AGENT_CODING_AGENT_DIR` gets its own
    /// daemon instead of attaching to the first one's. The product-default
    /// agent dir keeps the unkeyed `daemon.sock`, and every key shares the
    /// socket dir (only the file name grows).
    #[cfg(unix)]
    #[test]
    fn default_daemon_socket_is_keyed_by_a_non_default_agent_dir() {
        use std::path::Path;
        let _env = env_lock();
        let _agent_dir = RestoreEnv::new(ENV_AGENT_DIR);
        let socket_for = |agent_dir: Option<&Path>| {
            match agent_dir {
                Some(dir) => std::env::set_var(ENV_AGENT_DIR, dir),
                None => std::env::remove_var(ENV_AGENT_DIR),
            }
            pa_daemon::socket::default_daemon_socket_path()
        };
        let default = socket_for(None);
        let explicit_default = socket_for(Some(
            &pa_types::platform::home_dir()
                .unwrap()
                .join(CONFIG_DIR_NAME),
        ));
        let a = socket_for(Some(Path::new("/tmp/agent-dir-a")));
        let a_again = socket_for(Some(Path::new("/tmp/agent-dir-a")));
        let b = socket_for(Some(Path::new("/tmp/agent-dir-b")));
        let name = |path: &Path| path.file_name().unwrap().to_string_lossy().to_string();
        assert_eq!(
            (
                name(&default),
                explicit_default == default,
                a == a_again,
                a == b,
                a.parent() == default.parent() && b.parent() == default.parent(),
                name(&a).len(),
            ),
            (
                "daemon.sock".to_string(),
                true,
                true,
                false,
                true,
                "daemon-01234567.sock".len()
            )
        );
    }

    /// `expands_tilde` hands HOME back the way it found it: the rest of the
    /// test binary (any test resolving `~` or the agent dir) reads it.
    #[test]
    fn expands_tilde_restores_home() {
        let before = {
            let _env = env_lock();
            std::env::var_os("HOME")
        };
        expands_tilde();
        let after = {
            let _env = env_lock();
            std::env::var_os("HOME")
        };
        assert_eq!(after, before);
    }

    #[test]
    fn expands_tilde() {
        let _env = env_lock();
        let _home = RestoreEnv::new("HOME");
        std::env::set_var("HOME", "/home/tester");
        assert_eq!(expand_tilde_path("~"), PathBuf::from("/home/tester"));
        assert_eq!(
            expand_tilde_path("~/sessions"),
            PathBuf::from("/home/tester/sessions")
        );
        assert_eq!(expand_tilde_path("/abs/path"), PathBuf::from("/abs/path"));
        assert_eq!(expand_tilde_path("~foo"), PathBuf::from("~foo"));
    }

    /// The TS `expandTildePath` win32 arm (the pa-types twin carries it too).
    #[test]
    #[cfg(windows)]
    fn expands_the_win32_backslash_tilde() {
        let _env = env_lock();
        let _home = RestoreEnv::new("HOME");
        let _profile = RestoreEnv::new("USERPROFILE");
        std::env::remove_var("HOME");
        std::env::set_var("USERPROFILE", r"C:\Users\tester");
        assert_eq!(
            expand_tilde_path(r"~\sessions"),
            PathBuf::from(r"C:\Users\tester\sessions")
        );
        assert_eq!(
            expand_tilde_path(r"~\deep\dir"),
            PathBuf::from(r"C:\Users\tester\deep\dir")
        );
    }
}
