//! The runtime source concern: the packaged
//! sidecar layout, the runtime embedded in the binary, and the content identity
//! that invalidates an existing venv on any runtime change.

use super::{expand_home, Digest, Path, PathBuf, RUNTIME_CONSTRAINTS_FILE, RUNTIME_REQUIREMENT};

/// Directory of the installed `prime-agent-runtime` sources. The Rust binary ships the same sidecar
/// layout the compiled TS executable uses.
pub(in crate::kernel::bootstrap) fn package_dir() -> PathBuf {
    if let Ok(env_dir) = std::env::var("PI_PACKAGE_DIR") {
        if !env_dir.is_empty() {
            return expand_home(&env_dir);
        }
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| crate::packages::exe_dir_of(&exe))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The packaged sidecar directory (the exe-adjacent layout): the TS `runtimeCandidateDirs`
/// bun-binary candidates.
pub(in crate::kernel::bootstrap) fn packaged_runtime_dir() -> Option<PathBuf> {
    let package = package_dir();
    [
        package.join("prime-agent-runtime"),
        package.join("dist").join("prime-agent-runtime"),
    ]
    .into_iter()
    .find(|candidate| candidate.join("pyproject.toml").exists())
}

/// Env override naming the runtime checkout to install; when set it is the
/// only candidate (a dev pointing at a specific checkout).
pub(in crate::kernel::bootstrap) const RUNTIME_SOURCE_ENV: &str = "PRIME_AGENT_RUNTIME_SOURCE";

/// Every directory searched for the runtime source, in order: the explicit
/// override alone when set; else the packaged sidecar (`PI_PACKAGE_DIR`, an
/// explicit package dir, is searched alone too); else the runtime embedded in
/// this binary, extracted beside the kernel venv. The live source checkout is
/// never a candidate: it may have moved on from the tree this binary was
/// built from, and a runtime the host cannot serve breaks the kernel.
pub(in crate::kernel::bootstrap) fn runtime_candidate_dirs() -> Vec<PathBuf> {
    if let Ok(explicit) = std::env::var(RUNTIME_SOURCE_ENV) {
        if !explicit.is_empty() {
            return vec![expand_home(&explicit)];
        }
    }
    if let Some(packaged) = packaged_runtime_dir() {
        return vec![packaged];
    }
    if std::env::var_os("PI_PACKAGE_DIR").is_some_and(|dir| !dir.is_empty()) {
        return Vec::new();
    }
    crate::embedded_bundle::embedded_bundle_dir()
        .map(|bundle| bundle.join(crate::embedded_bundle::RUNTIME_DIR))
        .into_iter()
        .collect()
}

pub(in crate::kernel::bootstrap) fn resolve_runtime_source_dir() -> Option<PathBuf> {
    runtime_candidate_dirs()
        .into_iter()
        .find(|candidate| candidate.join("pyproject.toml").exists())
}

/// Content identity of the runtime: a hash of every `rlm/*.py` file, the
/// packaged machine library under `src/rlm/machines` (wheel package data:
/// machine changes are runtime changes), `pyproject.toml`, and the shipped install constraints
/// (new pins are a runtime change), so any
/// runtime change invalidates an existing venv. Falls back to the bare
/// package name when the runtime resolves to a registry install (no local
/// source).
///
/// # Panics
///
/// Panics when hashing the resolved local runtime source fails.
#[must_use]
pub fn resolve_runtime_identity() -> String {
    let Some(source_dir) = resolve_runtime_source_dir() else {
        return RUNTIME_REQUIREMENT.to_string();
    };
    hash_runtime_source(&source_dir).unwrap_or_else(|error| {
        panic!(
            "cannot hash runtime source at {}: {error}",
            source_dir.display()
        )
    })
}

pub(super) fn hash_runtime_source(source_dir: &Path) -> anyhow::Result<String> {
    let rlm_dir = source_dir.join("src").join("rlm");
    let mut files = vec![source_dir.join("pyproject.toml")];
    let constraints = source_dir.join(RUNTIME_CONSTRAINTS_FILE);
    if constraints.is_file() {
        files.push(constraints);
    }
    collect_python_files(&rlm_dir, &mut files)?;
    collect_package_data_files(&rlm_dir.join("machines"), &mut files)?;
    files.sort();
    let mut hasher = sha2::Sha256::new();
    for file in &files {
        let relative = file.strip_prefix(source_dir)?;
        hasher.update(relative.to_string_lossy().as_bytes());
        hasher.update([0]);
        hasher.update(&std::fs::read(file)?);
        hasher.update([0]);
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}

pub(super) fn collect_python_files(dir: &Path, files: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_python_files(&path, files)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("py") {
            files.push(path);
        }
    }
    Ok(())
}

/// Collect every file under the packaged machine library (`src/rlm/machines`,
/// any extension: the wheel ships the MACHINE.md files as package data), so
/// machine changes invalidate an existing venv exactly like a `.py` change.
fn collect_package_data_files(dir: &Path, files: &mut Vec<PathBuf>) -> anyhow::Result<()> {
    if !dir.is_dir() {
        return Ok(()); // a runtime payload without a machine library is legal
    }
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_package_data_files(&path, files)?;
        } else {
            files.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The workspace root this crate was compiled in.
    fn compiled_checkout_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("workspace root")
            .to_path_buf()
    }

    /// A binary without a packaged sidecar or an explicit override never
    /// installs the live checkout's runtime: that checkout may hold a
    /// different runtime than the one the binary was built from (a
    /// `cargo install`ed binary whose checkout moved on rebuilt the kernel
    /// with a runtime the binary could not host).
    /// The runtime identity follows the bundle the running binary carries:
    /// a binary built from an edited runtime extracts a different identity
    /// (the venv rebuilds to it), and the old binary keeps its own.
    #[test]
    fn the_runtime_identity_follows_the_embedded_bundle() -> anyhow::Result<()> {
        const TREE_A: &[crate::embedded_bundle::BundleFile] = &[
            ("prime-agent-runtime/pyproject.toml", b"[project]\n"),
            (
                "prime-agent-runtime/src/rlm/bash.py",
                b"SERVED_BY = 'host'\n",
            ),
        ];
        const TREE_B: &[crate::embedded_bundle::BundleFile] = &[
            ("prime-agent-runtime/pyproject.toml", b"[project]\n"),
            (
                "prime-agent-runtime/src/rlm/bash.py",
                b"SERVED_BY = 'sidecar'\n",
            ),
        ];
        let temp = tempfile::tempdir()?;
        let a = crate::embedded_bundle::materialize(temp.path(), TREE_A)?;
        let b = crate::embedded_bundle::materialize(temp.path(), TREE_B)?;
        let runtime = crate::embedded_bundle::RUNTIME_DIR;
        let identity_a = hash_runtime_source(&a.join(runtime))?;
        assert_ne!(identity_a, hash_runtime_source(&b.join(runtime))?);
        assert_eq!(
            identity_a,
            hash_runtime_source(
                &crate::embedded_bundle::materialize(temp.path(), TREE_A)?.join(runtime)
            )?
        );
        Ok(())
    }

    #[test]
    fn no_runtime_candidate_is_the_live_checkout() {
        if std::env::var_os(RUNTIME_SOURCE_ENV).is_some()
            || std::env::var_os("PI_PACKAGE_DIR").is_some()
        {
            return; // an explicit override is deliberate
        }
        let checkout_runtime = compiled_checkout_root().join("prime-agent-runtime");
        let candidates = runtime_candidate_dirs();
        assert!(
            !candidates.contains(&checkout_runtime),
            "the live checkout is a runtime candidate: {candidates:?}"
        );
    }

    fn runtime_fixture(temp: &std::path::Path, machine_body: &str) -> anyhow::Result<()> {
        let rlm = temp.join("src").join("rlm");
        std::fs::create_dir_all(&rlm)?;
        std::fs::write(
            rlm.join("factory.py"),
            "X = 1
",
        )?;
        let machines = rlm.join("machines").join("review-sweep");
        std::fs::create_dir_all(&machines)?;
        std::fs::write(machines.join("MACHINE.md"), machine_body)?;
        std::fs::write(
            temp.join("pyproject.toml"),
            "[project]
",
        )?;
        Ok(())
    }

    #[test]
    fn machine_library_changes_invalidate_the_runtime_identity() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        runtime_fixture(
            temp.path(),
            "---
name: review-sweep
",
        )?;
        let before = hash_runtime_source(temp.path())?;
        runtime_fixture(
            temp.path(),
            "---
name: review-sweep
version: 2
",
        )?;
        let after = hash_runtime_source(temp.path())?;
        assert_ne!(before, after, "a machine file change is a runtime change");

        // A runtime without the machine library still hashes cleanly.
        std::fs::remove_dir_all(temp.path().join("src").join("rlm").join("machines"))?;
        let bare = hash_runtime_source(temp.path())?;
        assert_ne!(bare, after);
        Ok(())
    }
}
