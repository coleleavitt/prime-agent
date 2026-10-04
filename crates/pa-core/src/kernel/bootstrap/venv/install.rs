//! The single-package install concern: put one Python skill package into the
//! live kernel venv without rebuilding it — the same editable install
//! [`super::sync_python_skills`] runs, under the same bootstrap lock, recorded
//! in the same manifest so the next kernel start does not reinstall it.

use super::layout::xdg_kernel_venv_dir;
use super::skills::file_content_hash;
use super::version::{bootstrap_base_version_current, bootstrap_skill_key};
use super::{
    ensure_uv, expand_home, kernel_venv_dir, kernel_venv_python, read_bootstrap_version,
    resolve_runtime_identity, run_async, write_bootstrap_version, BootstrapPythonSkill, Path,
    PathBuf,
};
use crate::kernel::bootstrap::dir_lock::acquire_bootstrap_lock;

/// One package to install editable into the kernel venv.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PythonSkillPackageInstall {
    /// Package directory (holding `pyproject.toml`).
    pub package_path: PathBuf,
    /// The module name the package provides.
    pub import_name: String,
}

/// What an install did. `installed: false` is not an error: the package is
/// on disk either way and the next kernel start installs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PythonSkillPackageInstallResult {
    pub installed: bool,
    pub detail: String,
    pub duration_ms: u64,
    /// The interpreter the package was installed for, when one was on disk.
    pub python: Option<PathBuf>,
}

/// The kernel interpreter already on disk, without bootstrapping one: the
/// `PRIME_AGENT_KERNEL_PYTHON` override when it exists, else the managed venv
/// (then its XDG fallback) when its interpreter exists. `None` when there is
/// none yet.
#[must_use]
pub fn installed_kernel_python() -> Option<PathBuf> {
    resolve_installed_kernel_python().0
}

/// The interpreter on disk (if any) and the venv whose bootstrap lock and
/// manifest an install goes through.
fn resolve_installed_kernel_python() -> (Option<PathBuf>, PathBuf) {
    let venv = kernel_venv_dir();
    if let Ok(override_python) = std::env::var("PRIME_AGENT_KERNEL_PYTHON") {
        if !override_python.is_empty() {
            let expanded = expand_home(&override_python);
            let resolved = std::path::absolute(&expanded).unwrap_or(expanded);
            return (resolved.exists().then_some(resolved), venv);
        }
    }
    for candidate in [venv.clone(), xdg_kernel_venv_dir()] {
        let python = kernel_venv_python(&candidate);
        if python.exists() {
            return (Some(python), candidate);
        }
    }
    (None, venv)
}

/// Install one package into the live kernel venv.
///
/// `before_install` runs while the bootstrap lock is held and before the
/// install: the window in which a caller can move a staged package into
/// place without racing a concurrent skill sync.
///
/// # Errors
///
/// Only the lock acquisition and `before_install` fail the call. An
/// environment the install cannot fix (no interpreter yet, no uv, a package
/// uv refuses) answers `installed: false` with a detail instead.
pub async fn install_python_skill_package<F>(
    request: &PythonSkillPackageInstall,
    before_install: F,
) -> anyhow::Result<PythonSkillPackageInstallResult>
where
    F: FnOnce() -> anyhow::Result<()> + Send,
{
    let (python, venv) = resolve_installed_kernel_python();
    install_into(request, python, &venv, before_install).await
}

async fn install_into<F>(
    request: &PythonSkillPackageInstall,
    python: Option<PathBuf>,
    venv: &Path,
    before_install: F,
) -> anyhow::Result<PythonSkillPackageInstallResult>
where
    F: FnOnce() -> anyhow::Result<()> + Send,
{
    let started = std::time::Instant::now();
    let elapsed = || u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let release_lock = acquire_bootstrap_lock(venv).await?;
    before_install()?;
    let Some(python) = python else {
        return Ok(PythonSkillPackageInstallResult {
            installed: false,
            detail: "no kernel python on disk; the package installs at the next kernel start"
                .to_string(),
            duration_ms: elapsed(),
            python: None,
        });
    };
    let package_path =
        std::path::absolute(&request.package_path).unwrap_or_else(|_| request.package_path.clone());
    let pyproject_path = package_path.join("pyproject.toml");
    let skill = BootstrapPythonSkill {
        import_name: request.import_name.clone(),
        package_path: package_path.to_string_lossy().to_string(),
        pyproject_path: pyproject_path.to_string_lossy().to_string(),
        pyproject_hash: file_content_hash(&pyproject_path),
    };
    let uv = match ensure_uv() {
        Ok(uv) => uv,
        Err(error) => {
            return Ok(PythonSkillPackageInstallResult {
                installed: false,
                detail: format!("uv unavailable: {error:#}"),
                duration_ms: elapsed(),
                python: Some(python),
            });
        }
    };
    let install_args = [
        "pip".to_string(),
        "install".to_string(),
        "--python".to_string(),
        python.to_string_lossy().to_string(),
        "--editable".to_string(),
        skill.package_path.clone(),
    ];
    if let Err(error) = run_async(&uv, &install_args).await {
        return Ok(PythonSkillPackageInstallResult {
            installed: false,
            detail: format!("editable install failed: {error:#}"),
            duration_ms: elapsed(),
            python: Some(python),
        });
    }
    if let Err(error) = record_installed_python_skill(venv, &skill) {
        tracing::warn!(
            venv = %venv.display(),
            import_name = %skill.import_name,
            error = %error,
            "kernel skill manifest update failed"
        );
    }
    drop(release_lock);
    Ok(PythonSkillPackageInstallResult {
        installed: true,
        detail: format!("editable install of {}", package_path.display()),
        duration_ms: elapsed(),
        python: Some(python),
    })
}

/// Merge one installed skill into the venv manifest. A manifest that does not
/// describe the current runtime is about to be rebuilt wholesale by the next
/// bootstrap; writing into it would only claim a state the venv is not in.
fn record_installed_python_skill(venv: &Path, skill: &BootstrapPythonSkill) -> anyhow::Result<()> {
    let runtime_identity = resolve_runtime_identity();
    let version = read_bootstrap_version(venv);
    if !bootstrap_base_version_current(version.clone(), &runtime_identity) {
        return Ok(());
    }
    let key = bootstrap_skill_key(skill);
    let mut skills: Vec<BootstrapPythonSkill> = version
        .and_then(|version| version.python_skills)
        .unwrap_or_default()
        .into_iter()
        .filter(|recorded| bootstrap_skill_key(recorded) != key)
        .collect();
    skills.push(skill.clone());
    skills.sort_by(|a, b| {
        a.package_path
            .cmp(&b.package_path)
            .then(a.import_name.cmp(&b.import_name))
    });
    write_bootstrap_version(venv, &runtime_identity, &skills)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(package_path: &Path) -> PythonSkillPackageInstall {
        PythonSkillPackageInstall {
            package_path: package_path.to_path_buf(),
            import_name: "probe_skill".to_string(),
        }
    }

    /// With no interpreter on disk the caller's step still runs under the
    /// lock, and the answer is a value, not an error.
    #[tokio::test]
    async fn without_a_kernel_python_the_step_runs_and_nothing_installs() {
        let dir = tempfile::tempdir().unwrap();
        let venv = dir.path().join("kernel-venv");
        let package = dir.path().join("pkg");
        let marker = dir.path().join("moved");
        let lock = dir.path().join("kernel-venv.bootstrap.lock");
        let result = install_into(&request(&package), None, &venv, || {
            assert!(
                lock.exists(),
                "the step runs while the bootstrap lock is held"
            );
            std::fs::write(&marker, "1")?;
            Ok(())
        })
        .await
        .unwrap();
        assert!(marker.exists());
        assert!(!lock.exists(), "the lock is released afterwards");
        assert_eq!(
            result,
            PythonSkillPackageInstallResult {
                installed: false,
                detail: "no kernel python on disk; the package installs at the next kernel start"
                    .to_string(),
                duration_ms: result.duration_ms,
                python: None,
            }
        );
    }

    /// A failing caller step aborts the install and propagates.
    #[tokio::test]
    async fn a_failing_step_propagates() {
        let dir = tempfile::tempdir().unwrap();
        let venv = dir.path().join("kernel-venv");
        let error = install_into(
            &request(&dir.path().join("pkg")),
            Some(dir.path().join("python")),
            &venv,
            || Err(anyhow::anyhow!("rename failed")),
        )
        .await
        .unwrap_err();
        assert_eq!(error.to_string(), "rename failed");
    }

    /// The manifest gains the installed skill (replacing an older record of
    /// the same key), and a stale manifest is left for the rebuild.
    #[test]
    fn the_manifest_records_the_install_only_when_current() {
        let dir = tempfile::tempdir().unwrap();
        let venv = dir.path();
        let skill = |hash: &str| BootstrapPythonSkill {
            import_name: "probe_skill".to_string(),
            package_path: "/skills/probe-skill".to_string(),
            pyproject_path: "/skills/probe-skill/pyproject.toml".to_string(),
            pyproject_hash: hash.to_string(),
        };
        let other = BootstrapPythonSkill {
            import_name: "another".to_string(),
            package_path: "/skills/another".to_string(),
            pyproject_path: "/skills/another/pyproject.toml".to_string(),
            pyproject_hash: "sha256:a".to_string(),
        };

        record_installed_python_skill(venv, &skill("sha256:new")).unwrap();
        assert!(
            read_bootstrap_version(venv).is_none(),
            "no manifest, no write"
        );

        let runtime = resolve_runtime_identity();
        write_bootstrap_version(venv, &runtime, &[skill("sha256:old"), other.clone()]).unwrap();
        record_installed_python_skill(venv, &skill("sha256:new")).unwrap();
        assert_eq!(
            read_bootstrap_version(venv).and_then(|version| version.python_skills),
            Some(vec![other, skill("sha256:new")])
        );
    }
}
