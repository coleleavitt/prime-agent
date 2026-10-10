//! Test processes never touch the user's real agent state.
//!
//! A test that resolves the agent dir, `auth.json`, the shared Anthropic
//! account store, Claude Code's credentials or the kernel venv from the
//! environment it inherited works on the developer's live state. A
//! `cargo test` started from a prime-agent session inherits the session's
//! `PRIME_AGENT_CODING_AGENT_DIR` (the kernel exports it to every command),
//! and each spawned binary that set `HOME` but not the agent dir then traced
//! into the real `~/.prime/agent/logs`, read the real `auth.json` and flushed
//! the real harness ledger. Three parts keep that from happening again:
//!
//! - isolation by construction for spawned processes: [`TestState`] gives a
//!   spawned binary every one of those paths under a temp root, and scrubs the
//!   inherited variables that redirect them;
//! - isolation by construction in-process: a libtest harness drops inherited
//!   redirects into the real state and moves off a real `HOME` to the test
//!   home before its first guarded resolution, so in-process code and the
//!   children it spawns never resolve the real home;
//! - a guard: in a test process, [`guard_state_path`] and
//!   [`refuse_real_state`] refuse a path inside a protected home's state and
//!   fail loudly. The protected home is the passwd entry's (`getpwuid`), not
//!   `$HOME`, so a sandboxed `HOME` cannot hide the real one.
//!
//! Outside a test process all of it is a no-op: production behaviour does
//! not change.

use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

/// Set (`1`) by [`TestState`] on every process it isolates: the process is a
/// test's, so the guards apply to it and to its children.
pub const ISOLATED_ENV: &str = "PA_TEST_ISOLATED";

/// An extra home whose state the guards protect, beside the passwd home. A
/// red-first run points it (and `HOME`) at a sentinel to stand in for the
/// real home. It only ever adds protection.
pub const PROTECTED_HOME_ENV: &str = "PA_TEST_PROTECTED_HOME";

/// The explicit opt-in (`1`) for a test that must use the real state.
pub const ALLOW_REAL_STATE_ENV: &str = "PA_TEST_ALLOW_REAL_STATE";

/// The `HOME` isolated processes run with (where the kernel venv store
/// lives). Unset: `$HOME` when it is not protected, else a per-user temp dir.
pub const TEST_HOME_ENV: &str = "PA_TEST_HOME";

/// The state directories under a home that tests must never resolve.
const PROTECTED_STATE: &[&str] = &[
    ".prime",
    ".anthropic-accounts",
    ".claude",
    ".claude.json",
    ".pi",
    ".grok",
    ".config/opencode",
    ".config/jfc",
    ".local/share/prime",
];

/// Inherited variables that point state somewhere: [`TestState`] removes
/// every one, then sets its own. Prefixes, matched against the inherited names.
const STATE_REDIRECT_PREFIXES: &[&str] = &[
    "PRIME_AGENT_",
    "PI_",
    "RLM_",
    "OPENCODE_",
    "ANTHROPIC_ACCOUNTS_",
    "CLAUDE_CONFIG_DIR",
    "CLAUDE_SECURESTORAGE_CONFIG_DIR",
    "GROK_HOME",
];

/// Whether this process belongs to a test run: a [`TestState`]-isolated
/// process (or its child), a process `cargo test` started (or its child:
/// cargo exports `CARGO_BIN_EXE_*` to integration tests), or a libtest
/// harness binary itself (`target/<profile>/deps/<name>-<hash>`).
#[must_use]
pub fn is_test_process() -> bool {
    static TEST_PROCESS: OnceLock<bool> = OnceLock::new();
    *TEST_PROCESS.get_or_init(|| {
        if is_test_harness() && !env_flag(ALLOW_REAL_STATE_ENV) {
            isolate_harness_env();
        }
        env_flag(ISOLATED_ENV)
            || std::env::vars_os().any(|(name, _)| {
                name.to_str()
                    .is_some_and(|name| name.starts_with("CARGO_BIN_EXE_"))
            })
            || is_test_harness()
    })
}

/// Call before reading the environment for a state path: in a libtest
/// harness, the first call isolates the process environment (see
/// [`is_test_process`]), so the read sees the isolated values.
pub fn prepare() {
    let _ = is_test_process();
}

/// The variables that point a process's state somewhere, beside `HOME`.
const STATE_REDIRECT_VARS: &[&str] = &[
    "PRIME_AGENT_CODING_AGENT_DIR",
    "PI_CODING_AGENT_DIR",
    "PRIME_AGENT_SESSION_DIR",
    "PRIME_AGENT_KERNEL_VENV",
    "PRIME_AGENT_KERNEL_PYTHON",
    "ANTHROPIC_ACCOUNTS_FILE",
    "ANTHROPIC_ACCOUNTS_DIR",
    "CLAUDE_CONFIG_DIR",
    "CLAUDE_SECURESTORAGE_CONFIG_DIR",
    "PI_AGENT_DIR",
    "OPENCODE_CONFIG_DIR",
    "PI_ANTHROPIC_AUTH_FILE",
    "OPENCODE_ANTHROPIC_AUTH_FILE",
    "PI_ANTHROPIC_AUTH_ROUTING_STATE_FILE",
    "GROK_HOME",
];

/// A libtest harness never runs on the real state its runner's environment
/// points at: before its first guarded resolution, it drops each inherited
/// redirect into a protected home's state (a `cargo test` started from a
/// prime-agent session inherits the session's agent dir) and moves `HOME`
/// off a protected home ([`harness_home`]). In-process code and the children
/// it spawns then resolve the test home. Says so on stderr.
fn isolate_harness_env() {
    let inherited = STATE_REDIRECT_VARS
        .iter()
        .filter_map(|name| Some((*name, std::env::var_os(name)?)));
    for name in protected_redirects(inherited) {
        eprintln!("test isolation: ignoring the inherited {name}, which points at the real state");
        std::env::remove_var(name);
    }
    if let Some(home) = super::dirs::raw_home_dir() {
        let _ = harness_home(home);
    }
}

/// The names among `vars` whose value is inside a protected home's state.
fn protected_redirects<'a>(vars: impl Iterator<Item = (&'a str, OsString)>) -> Vec<&'a str> {
    let homes = protected_homes();
    vars.filter(|(_, value)| !value.is_empty())
        .filter(|(_, value)| {
            let path = expand_tilde(Path::new(value));
            homes.iter().any(|home| is_protected_under(&path, home))
        })
        .map(|(name, _)| name)
        .collect()
}

/// A leading `~/` against the environment's `HOME`.
fn expand_tilde(path: &Path) -> PathBuf {
    match (path.strip_prefix("~"), super::dirs::raw_home_dir()) {
        (Ok(rest), Some(home)) => home.join(rest),
        _ => path.to_path_buf(),
    }
}

/// Whether this process is a libtest harness binary (in-process tests).
fn is_test_harness() -> bool {
    static HARNESS: OnceLock<bool> = OnceLock::new();
    *HARNESS.get_or_init(|| std::env::current_exe().is_ok_and(|exe| is_test_harness_path(&exe)))
}

/// `…/deps/<crate>-<16 hex>`: the file name cargo gives a test harness.
fn is_test_harness_path(exe: &Path) -> bool {
    let in_deps = exe
        .parent()
        .and_then(Path::file_name)
        .is_some_and(|dir| dir == "deps");
    let hashed = exe
        .file_stem()
        .and_then(OsStr::to_str)
        .and_then(|stem| stem.rsplit_once('-'))
        .is_some_and(|(_, hash)| hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit()));
    in_deps && hashed
}

fn env_flag(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| value == "1")
}

/// The passwd entry's home directory for this user (`getpwuid_r`), whatever
/// `$HOME` says.
#[cfg(unix)]
#[must_use]
pub fn passwd_home() -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut buffer = vec![0_u8; 4096];
    loop {
        // SAFETY: `passwd` is plain old data; `getpwuid_r` fills it and points
        // its strings into `buffer`, which outlives every read below.
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: every pointer is valid for the call and `buffer.len()` is its size.
        let status = unsafe {
            libc::getpwuid_r(
                libc::getuid(),
                &raw mut entry,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &raw mut found,
            )
        };
        if status == libc::ERANGE && buffer.len() < 1 << 20 {
            buffer.resize(buffer.len() * 2, 0);
            continue;
        }
        if status != 0 || found.is_null() || entry.pw_dir.is_null() {
            return None;
        }
        // SAFETY: on success `pw_dir` is a NUL-terminated string inside `buffer`.
        let dir = unsafe { std::ffi::CStr::from_ptr(entry.pw_dir) };
        let dir = OsStr::from_bytes(dir.to_bytes());
        return (!dir.is_empty()).then(|| PathBuf::from(dir));
    }
}

/// No passwd database off unix: the guards protect only
/// [`PROTECTED_HOME_ENV`] there.
#[cfg(not(unix))]
#[must_use]
pub fn passwd_home() -> Option<PathBuf> {
    None
}

/// The homes whose state is protected: the passwd home, plus
/// [`PROTECTED_HOME_ENV`] when set.
#[must_use]
pub fn protected_homes() -> Vec<PathBuf> {
    static PASSWD_HOME: OnceLock<Option<PathBuf>> = OnceLock::new();
    let mut homes: Vec<PathBuf> = PASSWD_HOME
        .get_or_init(passwd_home)
        .iter()
        .cloned()
        .collect();
    if let Some(extra) = std::env::var_os(PROTECTED_HOME_ENV).filter(|home| !home.is_empty()) {
        homes.push(PathBuf::from(extra));
    }
    homes
}

fn is_protected_home(home: &Path) -> bool {
    let home = resolve(home);
    protected_homes()
        .iter()
        .any(|protected| resolve(protected) == home)
}

/// The home an in-process test resolves `~` against: `home` itself, or
/// [`test_home`] in a libtest harness when `home` is a protected one (the
/// test runs with the real `HOME`). The first such resolution also points
/// the harness's own `HOME` at the test home, so the children in-process
/// code spawns (kernels, npm, git, shells) inherit it too. Spawned binaries
/// keep their `HOME`: [`refuse_real_state`] judges it.
pub(crate) fn harness_home(home: PathBuf) -> PathBuf {
    match harness_replacement_home(&home) {
        Some(test_home) => {
            std::env::set_var("HOME", &test_home);
            test_home
        }
        None => home,
    }
}

/// The test home that replaces `home` in this process, if it must.
fn harness_replacement_home(home: &Path) -> Option<PathBuf> {
    (is_test_harness() && !env_flag(ALLOW_REAL_STATE_ENV) && is_protected_home(home))
        .then(test_home)
}

/// Whether `path` is (or is inside) one of `home`'s state directories,
/// judged on the resolved paths (symlinks followed as far as they exist).
fn is_protected_under(path: &Path, home: &Path) -> bool {
    let path = resolve(path);
    let home = resolve(home);
    PROTECTED_STATE
        .iter()
        .any(|state| path.starts_with(home.join(state)))
}

/// `path` made absolute and lexically normalized, with its longest existing
/// prefix canonicalized (a symlink into the real state is the real state).
fn resolve(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };
    let mut normal = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::ParentDir => {
                normal.pop();
            }
            Component::CurDir => {}
            other => normal.push(other),
        }
    }
    let mut existing = normal.as_path();
    let mut rest = Vec::new();
    loop {
        if let Ok(canonical) = existing.canonicalize() {
            return rest
                .iter()
                .rev()
                .fold(canonical, |path: PathBuf, part| path.join(part));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                existing = parent;
            }
            _ => return normal,
        }
    }
}

/// Why a test process must not use `path` as its `what`, or `None` when it
/// may: outside a test process, with [`ALLOW_REAL_STATE_ENV`], or when the
/// path is outside every protected home's state.
#[must_use]
pub fn real_state_violation(what: &str, path: &Path) -> Option<String> {
    if !is_test_process() || env_flag(ALLOW_REAL_STATE_ENV) {
        return None;
    }
    let home = protected_homes()
        .into_iter()
        .find(|home| is_protected_under(path, home))?;
    Some(format!(
        "a test process resolved its {what} to {path}, inside the real home's state ({home}). \
         Tests must not read or write the user's live agent dir, auth.json, account store, \
         Claude Code credentials or kernel venv. Give the process explicit temp paths \
         (pa_types::platform::test_isolation::TestState), or set {ALLOW_REAL_STATE_ENV}=1 \
         if this test must use the real state.",
        path = path.display(),
        home = home.display(),
    ))
}

/// Panic when a test process resolves `path` (its `what`) inside the real
/// home's state; a no-op outside tests.
///
/// # Panics
///
/// When [`real_state_violation`] reports one.
pub fn guard_state_path(what: &str, path: &Path) {
    if let Some(violation) = real_state_violation(what, path) {
        panic!("refusing to touch real state: {violation}");
    }
}

/// Every state path a process resolves from its environment, as the product
/// resolves them: the agent dir (and so `auth.json`, `settings.json`, the
/// logs, the harness dirs), the sessions dir, the shared Anthropic account
/// store, Claude Code's credentials and config, the kernel venv, and the
/// plugin sidecar dirs.
#[must_use]
pub fn resolved_state_paths() -> Vec<(&'static str, PathBuf)> {
    let var = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
    let home = super::dirs::raw_home_dir();
    let under_home = |rest: &str| home.as_ref().map(|home| home.join(rest));
    let mut paths = Vec::new();
    if let Some(agent_dir) = super::dirs::resolve_agent_dir() {
        paths.push(("auth.json", agent_dir.join("auth.json")));
        paths.push(("agent dir", agent_dir));
    }
    if let Some(sessions) = var("PRIME_AGENT_SESSION_DIR") {
        paths.push(("sessions dir", PathBuf::from(sessions)));
    }
    let account_store = var("ANTHROPIC_ACCOUNTS_FILE")
        .map(PathBuf::from)
        .or_else(|| {
            var("ANTHROPIC_ACCOUNTS_DIR").map(|dir| PathBuf::from(dir).join("accounts.json"))
        })
        .or_else(|| under_home(".anthropic-accounts/accounts.json"));
    paths.extend(account_store.map(|path| ("Anthropic account store", path)));
    let claude_dir = var("CLAUDE_SECURESTORAGE_CONFIG_DIR")
        .or_else(|| var("CLAUDE_CONFIG_DIR"))
        .map(PathBuf::from)
        .or_else(|| under_home(".claude"));
    paths.extend(claude_dir.map(|dir| ("Claude Code credentials", dir.join(".credentials.json"))));
    if var("CLAUDE_CONFIG_DIR").is_none() {
        paths.extend(under_home(".claude.json").map(|path| ("Claude Code config", path)));
    }
    let kernel_venv = var("PRIME_AGENT_KERNEL_VENV")
        .map(PathBuf::from)
        .or_else(|| under_home(".prime/agent/kernel-venv"));
    paths.extend(kernel_venv.map(|path| ("kernel venv", path)));
    let pi_dir = var("PI_AGENT_DIR")
        .map(PathBuf::from)
        .or_else(|| under_home(".pi/agent"));
    paths.extend(pi_dir.map(|path| ("pi agent dir", path)));
    let opencode_dir = var("OPENCODE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| var("XDG_CONFIG_HOME").map(|dir| PathBuf::from(dir).join("opencode")))
        .or_else(|| under_home(".config/opencode"));
    paths.extend(opencode_dir.map(|path| ("opencode config dir", path)));
    for (what, name) in [
        ("Anthropic auth sidecar", "PI_ANTHROPIC_AUTH_FILE"),
        ("Anthropic auth sidecar", "OPENCODE_ANTHROPIC_AUTH_FILE"),
        (
            "Anthropic routing state",
            "PI_ANTHROPIC_AUTH_ROUTING_STATE_FILE",
        ),
    ] {
        paths.extend(var(name).map(|path| (what, PathBuf::from(path))));
    }
    paths
}

/// The startup guard of a binary: in a test process, refuse to start when
/// any [`resolved_state_paths`] entry is inside the real home's state. Prints
/// why and exits 78 (`EX_CONFIG`); a no-op outside tests.
pub fn refuse_real_state() {
    if !is_test_process() {
        return;
    }
    let violations: Vec<String> = resolved_state_paths()
        .into_iter()
        .filter_map(|(what, path)| real_state_violation(what, &path))
        .collect();
    if violations.is_empty() {
        return;
    }
    for violation in &violations {
        eprintln!("refusing to start: {violation}");
    }
    std::process::exit(78);
}

/// The `HOME` isolated processes run with: [`TEST_HOME_ENV`], else `$HOME`
/// when it is not a protected home, else `<temp>/pa-test-home-<uid>`. The
/// kernel venv store lives under it, so test runs share one bootstrap.
#[must_use]
pub fn test_home() -> PathBuf {
    if let Some(home) = std::env::var_os(TEST_HOME_ENV).filter(|home| !home.is_empty()) {
        return PathBuf::from(home);
    }
    if let Some(home) = super::dirs::raw_home_dir() {
        if !is_protected_home(&home) {
            return home;
        }
    }
    let home = std::env::temp_dir().join(format!("pa-test-home-{}", user_key()));
    let _ = std::fs::create_dir_all(&home);
    home
}

#[cfg(unix)]
fn user_key() -> String {
    // SAFETY: `getuid` cannot fail.
    unsafe { libc::getuid() }.to_string()
}

#[cfg(not(unix))]
fn user_key() -> String {
    std::env::var("USERNAME").unwrap_or_else(|_| "user".to_string())
}

/// The kernel Python tests run against: `explicit_env` (a suite's override,
/// such as `PA_CORE_KERNEL_PYTHON`) when set, else the bootstrapped venv
/// under [`test_home`]; `None` when neither exists. Never the real home's venv.
///
/// # Panics
///
/// When `explicit_env` names a file that does not exist, or a protected one.
#[must_use]
pub fn test_kernel_python(explicit_env: &str) -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os(explicit_env).filter(|path| !path.is_empty()) {
        let explicit = PathBuf::from(explicit);
        assert!(
            explicit.exists(),
            "{explicit_env} {} not found",
            explicit.display()
        );
        guard_state_path("kernel python", &explicit);
        return Some(explicit);
    }
    let venv = test_home().join(".prime").join("agent").join("kernel-venv");
    let python = if cfg!(windows) {
        venv.join("Scripts").join("python.exe")
    } else {
        venv.join("bin").join("python")
    };
    if python.exists() {
        guard_state_path("kernel python", &python);
        return Some(python);
    }
    eprintln!(
        "kernel python {} not found; skipping the live kernel test",
        python.display()
    );
    None
}

/// Explicit temp paths for one test's processes: everything the guards
/// protect, rooted at `root` (the test's temp dir).
///
/// - agent dir `<root>/agent` (`auth.json`, `settings.json`, logs, harness);
/// - account store `<root>/anthropic-accounts`, Claude Code config
///   `<root>/claude` (file credentials), pi `<root>/pi-agent`, opencode
///   `<root>/opencode`;
/// - `HOME` = [`test_home`] (the kernel venv store);
/// - [`ISOLATED_ENV`], so the process and its children run the guards.
///
/// Every inherited state redirect (`PRIME_AGENT_*`, `PI_*`, `RLM_*`, …) is
/// removed first. Apply it before a test's own `.env(..)` calls: those win.
#[derive(Debug, Clone)]
pub struct TestState {
    root: PathBuf,
    agent_dir: PathBuf,
}

impl TestState {
    /// Everything under `root`, the agent dir at `<root>/agent`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let agent_dir = root.join("agent");
        Self { root, agent_dir }
    }

    /// For a test that already chose its agent dir: the siblings go beside
    /// it (its parent is the root).
    #[must_use]
    pub fn for_agent_dir(agent_dir: impl Into<PathBuf>) -> Self {
        let agent_dir = agent_dir.into();
        let root = agent_dir
            .parent()
            .map_or_else(|| agent_dir.clone(), Path::to_path_buf);
        Self { root, agent_dir }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The agent dir the isolated process runs with.
    #[must_use]
    pub fn agent_dir(&self) -> &Path {
        &self.agent_dir
    }

    /// The variables to remove (`None`) and set, in order.
    #[must_use]
    pub fn env(&self) -> Vec<(OsString, Option<OsString>)> {
        let mut env: Vec<(OsString, Option<OsString>)> = std::env::vars_os()
            .filter(|(name, _)| {
                name.to_str().is_some_and(|name| {
                    STATE_REDIRECT_PREFIXES
                        .iter()
                        .any(|prefix| name.starts_with(prefix))
                })
            })
            .map(|(name, _)| (name, None))
            .collect();
        let set = |name: &str, value: &Path| {
            (OsString::from(name), Some(value.as_os_str().to_os_string()))
        };
        env.push(set("HOME", &test_home()));
        env.push(set("PRIME_AGENT_CODING_AGENT_DIR", &self.agent_dir));
        env.push(set(
            "ANTHROPIC_ACCOUNTS_DIR",
            &self.root.join("anthropic-accounts"),
        ));
        env.push(set("CLAUDE_CONFIG_DIR", &self.root.join("claude")));
        env.push(set("PI_AGENT_DIR", &self.root.join("pi-agent")));
        env.push(set("OPENCODE_CONFIG_DIR", &self.root.join("opencode")));
        env.push((
            OsString::from("ANTHROPIC_CLAUDE_CREDENTIALS_BACKEND"),
            Some(OsString::from("file")),
        ));
        env.push((OsString::from(ISOLATED_ENV), Some(OsString::from("1"))));
        env
    }

    /// Apply [`TestState::env`] to a std command.
    pub fn apply<'a>(
        &self,
        command: &'a mut std::process::Command,
    ) -> &'a mut std::process::Command {
        for (name, value) in self.env() {
            match value {
                Some(value) => command.env(name, value),
                None => command.env_remove(name),
            };
        }
        command
    }

    /// Apply [`TestState::env`] to a tokio command.
    pub fn apply_tokio<'a>(
        &self,
        command: &'a mut tokio::process::Command,
    ) -> &'a mut tokio::process::Command {
        for (name, value) in self.env() {
            match value {
                Some(value) => command.env(name, value),
                None => command.env_remove(name),
            };
        }
        command
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_binaries_are_recognized_by_their_hashed_deps_name() {
        assert!(is_test_harness_path(Path::new(
            "/w/target/debug/deps/pa_core-0123456789abcdef"
        )));
        assert!(!is_test_harness_path(Path::new(
            "/w/target/debug/prime-agent"
        )));
        assert!(!is_test_harness_path(Path::new("/opt/deps/prime-agent")));
        assert!(!is_test_harness_path(Path::new(
            "/opt/deps/prime-agent-cli"
        )));
    }

    #[test]
    fn this_unit_test_binary_is_a_test_process() {
        assert!(is_test_process());
    }

    #[test]
    fn state_under_a_protected_home_is_refused_and_siblings_are_not() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        assert!(is_protected_under(
            &home.join(".prime/agent/auth.json"),
            home
        ));
        assert!(is_protected_under(&home.join(".anthropic-accounts"), home));
        assert!(is_protected_under(&home.join(".claude.json"), home));
        assert!(is_protected_under(&home.join(".config/opencode/x"), home));
        assert!(is_protected_under(&home.join("tmp/../.prime/agent"), home));
        assert!(!is_protected_under(
            &home.join("src/prime-agent/target/tmp"),
            home
        ));
        assert!(!is_protected_under(&home.join(".primer"), home));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_into_the_protected_state_is_the_protected_state() {
        let home = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".prime/agent")).unwrap();
        let link = elsewhere.path().join("agent");
        std::os::unix::fs::symlink(home.path().join(".prime/agent"), &link).unwrap();
        assert!(is_protected_under(
            &link.join("logs/agent.jsonl"),
            home.path()
        ));
    }

    #[cfg(unix)]
    #[test]
    fn the_passwd_home_is_protected_whatever_home_says() {
        let real = passwd_home().expect("a passwd entry for this user");
        assert!(protected_homes().contains(&real));
        let violation = real_state_violation("agent dir", &real.join(".prime/agent"))
            .expect("the real agent dir is refused in a test process");
        assert!(violation.contains("agent dir"), "{violation}");
    }

    #[cfg(unix)]
    #[test]
    fn an_in_process_test_resolves_the_test_home_in_place_of_the_real_one() {
        let real = passwd_home().expect("a passwd entry for this user");
        let home = harness_replacement_home(&real).expect("the real home is replaced");
        assert!(!is_protected_home(&home), "{}", home.display());
        assert_eq!(
            harness_replacement_home(Path::new("/nonexistent/home")),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn inherited_redirects_into_the_real_state_are_the_ones_dropped() {
        let real = passwd_home().expect("a passwd entry for this user");
        let temp = tempfile::tempdir().unwrap();
        let vars = [
            (
                "PRIME_AGENT_CODING_AGENT_DIR",
                real.join(".prime/agent").into_os_string(),
            ),
            (
                "ANTHROPIC_ACCOUNTS_DIR",
                real.join(".anthropic-accounts").into_os_string(),
            ),
            (
                "CLAUDE_CONFIG_DIR",
                temp.path().join("claude").into_os_string(),
            ),
            ("PRIME_AGENT_SESSION_DIR", OsString::new()),
        ];
        assert_eq!(
            protected_redirects(vars.into_iter()),
            ["PRIME_AGENT_CODING_AGENT_DIR", "ANTHROPIC_ACCOUNTS_DIR"]
        );
    }

    #[test]
    fn the_isolated_env_scrubs_redirects_and_points_everything_under_the_root() {
        let root = Path::new("/r");
        let env = TestState::new(root).env();
        let value = |name: &str| {
            env.iter()
                .rev()
                .find(|(key, _)| key == name)
                .and_then(|(_, value)| value.clone())
        };
        assert_eq!(
            value("PRIME_AGENT_CODING_AGENT_DIR"),
            Some("/r/agent".into())
        );
        assert_eq!(
            value("ANTHROPIC_ACCOUNTS_DIR"),
            Some("/r/anthropic-accounts".into())
        );
        assert_eq!(value("CLAUDE_CONFIG_DIR"), Some("/r/claude".into()));
        assert_eq!(value("PI_AGENT_DIR"), Some("/r/pi-agent".into()));
        assert_eq!(value("OPENCODE_CONFIG_DIR"), Some("/r/opencode".into()));
        assert_eq!(value(ISOLATED_ENV), Some("1".into()));
        let home = PathBuf::from(value("HOME").expect("HOME is set"));
        assert!(
            !is_protected_home(&home),
            "the isolated HOME is never a protected one: {}",
            home.display()
        );
    }
}
