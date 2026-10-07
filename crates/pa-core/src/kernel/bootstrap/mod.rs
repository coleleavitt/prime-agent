//! Kernel Python environment bootstrap: the public entry point [`ensure_kernel_python`]
//! resolves (and on first run builds) the venv that runs `python -m rlm.repl`,
//! deduplicating concurrent bootstraps; the venv and version-file machinery lives in
//! [`venv`], the runtime bootstrap code in [`runtime_code`].

pub(crate) mod dir_lock;
mod disk_memo;
mod runtime_code;
pub(crate) mod venv;

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context};

use dir_lock::acquire_bootstrap_lock;
pub use runtime_code::{
    build_rlm_bootstrap_code, parse_unavailable_python_skills, UnavailablePythonSkills,
    PYTHON_SKILL_IMPORT_ERROR_REPORT_MARKER,
};
pub(crate) use venv::recorded_kernel_skill_paths;
use venv::store::{venv_key, VenvOps, VenvStore};
use venv::{
    bootstrap_venv, ensure_uv, expand_home, has_prime_agent_runtime,
    missing_python_skill_import_labels, missing_rlm_extra_import_labels, normalize_python_skills,
    resolve_kernel_venv_location, sync_python_skills, BootstrapPythonSkill, KernelVenvLocation,
};
pub use venv::{
    install_python_skill_package, installed_kernel_python, invalidate_runtime_probe_cache,
    invalidate_runtime_probe_cache_for, kernel_venv_dir, kernel_venv_python,
    resolve_runtime_identity, PythonSkillPackageInstall, PythonSkillPackageInstallResult,
};
use venv::{kernel_base_ready, kernel_ready};

/// One Python skill the kernel should import at bootstrap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelPythonSkill {
    pub name: String,
    pub import_name: String,
    pub package_path: PathBuf,
    pub pyproject_path: PathBuf,
}

/// The default extra packages pre-installed in the kernel venv and promised to
/// the model in the system prompt.
pub const DEFAULT_RLM_EXTRA_PACKAGES: [(&str, &str, &str); 12] = [
    // (uvArg, importName, promptLabel)
    ("requests", "requests", "requests"),
    ("httpx", "httpx", "httpx"),
    ("pyyaml", "yaml", "yaml (PyYAML)"),
    ("tomli", "tomli", "tomli"),
    ("python-dotenv", "dotenv", "dotenv (python-dotenv)"),
    ("pandas", "pandas", "pandas"),
    ("numpy", "numpy", "numpy"),
    ("scipy", "scipy", "scipy"),
    ("beautifulsoup4", "bs4", "bs4 (Beautiful Soup)"),
    ("lxml", "lxml", "lxml"),
    ("pydantic", "pydantic", "pydantic"),
    ("tyro", "tyro", "tyro"),
];

#[must_use]
pub fn default_rlm_extra_uv_args() -> Vec<&'static str> {
    DEFAULT_RLM_EXTRA_PACKAGES
        .iter()
        .map(|(uv, _, _)| *uv)
        .collect()
}

#[must_use]
pub fn default_rlm_extra_import_names() -> Vec<&'static str> {
    DEFAULT_RLM_EXTRA_PACKAGES
        .iter()
        .map(|(_, import, _)| *import)
        .collect()
}

#[must_use]
pub fn default_rlm_extra_import_labels() -> Vec<&'static str> {
    DEFAULT_RLM_EXTRA_PACKAGES
        .iter()
        .map(|(_, _, label)| *label)
        .collect()
}

/// Progress callback for the bootstrap (`ensure_kernel_python`).
pub type KernelBootstrapProgressHandler = std::sync::Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Default)]
pub struct EnsureKernelPythonOptions {
    pub python_skills: Vec<KernelPythonSkill>,
    pub on_progress: Option<KernelBootstrapProgressHandler>,
}

impl EnsureKernelPythonOptions {
    pub(crate) fn report(&self, message: &str) {
        match &self.on_progress {
            Some(handler) => handler(message),
            None => eprintln!("{message}"),
        }
    }
}

fn format_bootstrap_failure(error: &anyhow::Error) -> anyhow::Error {
    let mut message = format!(
        "Failed to set up the Python kernel runtime. {error:#}\n\
         First-time setup needs internet to install uv, Python, prime-agent-runtime, and default Python packages; once set up, prime-agent runs offline. \
         An interrupted runtime upgrade needs network once more, so re-run this while online. \
         Set PRIME_AGENT_KERNEL_PYTHON to a Python with a current prime-agent-runtime and default Python packages installed to skip auto-bootstrap."
    );
    // The packaged exe-adjacent sidecar (else the runtime embedded in the binary) is the kernel
    // runtime source; when none resolves, name where it was looked for.
    if venv::resolve_runtime_source_dir().is_none() {
        let package = venv::package_dir();
        let _ = write!(message,
            "\nThe packaged prime-agent-runtime directory was not found (looked next to the executable at {} and PI_PACKAGE_DIR); reinstall prime-agent so the kernel runtime ships beside the binary.",
            package.display()
        );
    }
    anyhow!(message)
}

/// The bootstrap failure when no runtime source exists to install from: the
/// searched paths, the recovery steps, and that the existing venv is intact.
fn missing_runtime_source_error(venv: &std::path::Path) -> anyhow::Error {
    let searched = venv::runtime_candidate_dirs()
        .iter()
        .map(|dir| format!("  {}", dir.display()))
        .collect::<Vec<_>>()
        .join("\n");
    let searched = if searched.is_empty() {
        "  (no candidate directories)".to_string()
    } else {
        searched
    };
    anyhow!(
        "Failed to set up the Python kernel runtime: the prime-agent-runtime source directory was not found. Searched:\n{searched}\nThe existing kernel venv at {} was left untouched. Restart prime-agent from a valid install (reinstall it if the runtime directory beside the binary is gone), set {} to a prime-agent-runtime checkout, or set PRIME_AGENT_KERNEL_PYTHON to a Python with a current prime-agent-runtime installed.",
        venv.display(),
        venv::RUNTIME_SOURCE_ENV,
    )
}

/// One in-flight bootstrap per unique options set, joined by concurrent callers.
type InFlightBootstrap = Option<(
    String,
    Arc<tokio::sync::Mutex<Option<anyhow::Result<PathBuf>>>>,
)>;

static IN_FLIGHT: Mutex<InFlightBootstrap> = Mutex::new(None);

/// Resolve the Python interpreter for the kernel: the `PRIME_AGENT_KERNEL_PYTHON`
/// override when valid, else the auto-bootstrapped venv python.
///
/// # Errors
///
/// Returns an error when the override Python is invalid or the venv bootstrap fails.
///
/// # Panics
///
/// The in-flight promise stores its outcome under the same lock, so the `expect` is unreachable.
pub async fn ensure_kernel_python(options: EnsureKernelPythonOptions) -> anyhow::Result<PathBuf> {
    let python_skills = normalize_python_skills(&options.python_skills);
    let key = [
        std::env::var("PRIME_AGENT_KERNEL_PYTHON").unwrap_or_default(),
        std::env::var("PRIME_AGENT_KERNEL_VENV").unwrap_or_default(),
        std::env::var(venv::RUNTIME_SOURCE_ENV).unwrap_or_default(),
        std::env::var("HOME").unwrap_or_default(),
        std::env::var("XDG_DATA_HOME").unwrap_or_default(),
        serde_json::to_string(&python_skills).unwrap_or_default(),
    ]
    .join("\u{0}");

    let shared = {
        let mut in_flight = IN_FLIGHT
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match in_flight.as_ref() {
            Some((existing_key, promise)) if *existing_key == key => promise.clone(),
            _ => {
                let promise = Arc::new(tokio::sync::Mutex::new(None));
                *in_flight = Some((key, promise.clone()));
                promise
            }
        }
    };
    let mut guard = shared.lock().await;
    if guard.is_none() {
        *guard = Some(ensure_kernel_python_uncached(&options, &python_skills).await);
    }
    let outcome = guard.take().expect("outcome was just stored");
    // Drop the registry entry so a later call re-validates instead of reusing
    // a cached rejection forever.
    let mut in_flight = IN_FLIGHT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if matches!(in_flight.as_ref(), Some((_, promise)) if Arc::ptr_eq(promise, &shared)) {
        *in_flight = None;
    }
    outcome
}

async fn ensure_kernel_python_uncached(
    options: &EnsureKernelPythonOptions,
    python_skills: &[BootstrapPythonSkill],
) -> anyhow::Result<PathBuf> {
    if let Ok(override_python) = std::env::var("PRIME_AGENT_KERNEL_PYTHON") {
        if !override_python.is_empty() {
            let python = expand_home(&override_python);
            let python_str = python.to_string_lossy().to_string();
            let mut missing = Vec::new();
            if !has_prime_agent_runtime(&python_str) {
                missing.push(
                    "a current prime-agent-runtime with callable rlm.spawn, rlm.create_session, rlm.host_request, rlm.progress_note, rlm.rename, and explicit harness CRUD methods".to_string(),
                );
            }
            if missing.is_empty() {
                let missing_extras = missing_rlm_extra_import_labels(&python_str);
                if !missing_extras.is_empty() {
                    missing.push(format!(
                        "default Python packages ({})",
                        missing_extras.join(", ")
                    ));
                }
            }
            if missing.is_empty() && !options.python_skills.is_empty() {
                let missing_skills =
                    missing_python_skill_import_labels(&python_str, &options.python_skills);
                if !missing_skills.is_empty() {
                    options.report(&format!(
                        "Warning: Python skills unavailable in PRIME_AGENT_KERNEL_PYTHON and will be disabled: {}",
                        missing_skills.join(", ")
                    ));
                }
            }
            if missing.is_empty() {
                return Ok(python);
            }
            return Err(anyhow!(
                "PRIME_AGENT_KERNEL_PYTHON points to a Python missing {}: {}",
                missing.join(" and "),
                python.display()
            ));
        }
    }

    let location = resolve_kernel_venv_location()?;
    // Resolve the runtime source before looking at the venv: without it the
    // runtime identity is unknown, every venv looks stale, and the rebuild
    // below would delete the venv other sessions share before an install
    // that cannot succeed (#2203).
    if venv::resolve_runtime_source_dir().is_none() {
        let shown = match &location {
            KernelVenvLocation::Pinned(venv) => venv.clone(),
            KernelVenvLocation::Keyed(store) => store.root(),
        };
        return Err(missing_runtime_source_error(&shown));
    }
    let runtime_identity = resolve_runtime_identity();
    let result = match location {
        KernelVenvLocation::Pinned(venv) => {
            ensure_pinned_venv(&venv, &runtime_identity, options, python_skills).await
        }
        KernelVenvLocation::Keyed(store) => {
            ensure_keyed_venv(&store, &runtime_identity, options, python_skills).await
        }
    };
    result.map_err(|error| format_bootstrap_failure(&error))
}

/// The pinned venv (`PRIME_AGENT_KERNEL_VENV`): one directory, rebuilt in
/// place when its runtime changes.
async fn ensure_pinned_venv(
    venv: &Path,
    runtime_identity: &str,
    options: &EnsureKernelPythonOptions,
    python_skills: &[BootstrapPythonSkill],
) -> anyhow::Result<PathBuf> {
    let python = kernel_venv_python(venv);
    let python_str = python.to_string_lossy().to_string();
    if kernel_ready(&python_str, venv, runtime_identity, python_skills) {
        return Ok(python);
    }
    let release_lock = acquire_bootstrap_lock(venv).await;
    let result = async {
        if kernel_ready(&python_str, venv, runtime_identity, python_skills) {
            return Ok(python);
        }
        if kernel_base_ready(&python_str, venv, runtime_identity) {
            sync_skills(venv, &python, runtime_identity, python_skills, options).await?;
            return Ok(python);
        }
        let had_venv = venv.exists();
        options.report("› setting up python kernel (one-time, ~30s)…");
        if had_venv {
            options.report("rebuilding kernel venv");
            std::fs::remove_dir_all(venv)
                .with_context(|| format!("removing {}", venv.display()))?;
        }
        bootstrap_venv(venv, python_skills, options).await?;
        Ok(python)
    }
    .await;
    drop(release_lock);
    options.report("✓ ready");
    result
}

/// The keyed venv of this runtime (see [`VenvStore::ensure`]), built and
/// probed with the real interpreter and uv.
async fn ensure_keyed_venv(
    store: &VenvStore,
    runtime_identity: &str,
    options: &EnsureKernelPythonOptions,
    python_skills: &[BootstrapPythonSkill],
) -> anyhow::Result<PathBuf> {
    static PRUNED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    let key = venv_key(runtime_identity);
    let ops = KernelVenvOps {
        runtime_identity,
        options,
        python_skills,
    };
    let venv = store.ensure(&key, &ops).await?;
    // Prune each store at most once per process: a long-lived host boots
    // many kernels, and old venvs only age out over days.
    let first = {
        let mut pruned = PRUNED
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let fresh = !pruned.contains(&store.root());
        if fresh {
            pruned.push(store.root());
        }
        fresh
    };
    if first {
        let removed = store.prune(&key, std::time::SystemTime::now());
        if !removed.is_empty() {
            tracing::info!(root = %store.root().display(), removed = ?removed, "pruned old kernel venvs");
        }
    }
    Ok(kernel_venv_python(&venv))
}

/// The real [`VenvOps`]: readiness is the manifest plus the interpreter
/// probe, a build is the uv bootstrap.
struct KernelVenvOps<'a> {
    runtime_identity: &'a str,
    options: &'a EnsureKernelPythonOptions,
    python_skills: &'a [BootstrapPythonSkill],
}

impl VenvOps for KernelVenvOps<'_> {
    fn ready(&self, venv: &Path) -> bool {
        let python = kernel_venv_python(venv).to_string_lossy().to_string();
        kernel_ready(&python, venv, self.runtime_identity, self.python_skills)
    }

    fn base_ready(&self, venv: &Path) -> bool {
        let python = kernel_venv_python(venv).to_string_lossy().to_string();
        kernel_base_ready(&python, venv, self.runtime_identity)
    }

    fn records_this_runtime(&self, venv: &Path) -> bool {
        venv::recorded_base_install_is(venv, self.runtime_identity)
    }

    async fn sync(&self, venv: &Path) -> anyhow::Result<()> {
        sync_skills(
            venv,
            &kernel_venv_python(venv),
            self.runtime_identity,
            self.python_skills,
            self.options,
        )
        .await
    }

    async fn build(&self, venv: &Path) -> anyhow::Result<()> {
        bootstrap_venv(venv, self.python_skills, self.options).await
    }

    fn report(&self, message: &str) {
        self.options.report(message);
    }
}

async fn sync_skills(
    venv: &Path,
    python: &Path,
    runtime_identity: &str,
    python_skills: &[BootstrapPythonSkill],
    options: &EnsureKernelPythonOptions,
) -> anyhow::Result<()> {
    let uv = ensure_uv()?;
    sync_python_skills(&uv, venv, python, runtime_identity, python_skills, options).await
}
