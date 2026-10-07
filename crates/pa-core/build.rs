//! Embeds the kernel runtime (`prime-agent-runtime/`) and the bundled skills
//! (`skills/`, `.features/` included) this binary is built from, so a binary
//! without a packaged sidecar (a `cargo install`) runs the runtime and skills
//! it was built with instead of whatever its source checkout holds later.
//!
//! The shipped-content policy mirrors the release packer
//! (`scripts/package_release.py`, `scripts/release/assemble_artifacts.py`):
//! development caches never ship, nor do the runtime's tests and lockfile.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Entries no asset tree ships (the packer's `EXCLUDED_NAMES`).
const EXCLUDED_NAMES: &[&str] = &[
    "node_modules",
    ".venv",
    "__pycache__",
    ".pytest_cache",
    ".ruff_cache",
    ".mypy_cache",
    ".git",
    ".DS_Store",
    ".coverage",
    "htmlcov",
];
const EXCLUDED_SUFFIXES: &[&str] = &[".pyc", ".egg-info"];

/// What the runtime ships: the install manifest, its pins, the package, and
/// the schemas the wheel force-includes.
const RUNTIME_ENTRIES: &[&str] = &["pyproject.toml", "kernel-constraints.txt", "src", "schemas"];

fn excluded(name: &str) -> bool {
    EXCLUDED_NAMES.contains(&name) || EXCLUDED_SUFFIXES.iter().any(|s| name.ends_with(s))
}

/// Collect every shipped file under `path` as (published relative path with
/// `/` separators, absolute source path).
fn collect(path: &Path, relative: &str, out: &mut Vec<(String, PathBuf)>) {
    let metadata = std::fs::symlink_metadata(path)
        .unwrap_or_else(|error| panic!("cannot stat {}: {error}", path.display()));
    assert!(
        !metadata.file_type().is_symlink(),
        "unexpected symlink in the embedded runtime assets: {}",
        path.display()
    );
    if metadata.is_dir() {
        let mut entries: Vec<_> = std::fs::read_dir(path)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
            .map(|entry| entry.expect("directory entry").file_name())
            .collect();
        entries.sort();
        for name in entries {
            let name = name
                .to_str()
                .unwrap_or_else(|| panic!("non-UTF-8 asset name under {}", path.display()))
                .to_string();
            if excluded(&name) {
                continue;
            }
            collect(&path.join(&name), &format!("{relative}/{name}"), out);
        }
    } else if metadata.is_file() {
        out.push((relative.to_string(), path.to_path_buf()));
    }
}

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let root = manifest_dir
        .ancestors()
        .nth(2)
        .expect("pa-core lives at <root>/crates/pa-core")
        .to_path_buf();
    let runtime = root.join("prime-agent-runtime");
    let skills = root.join("skills");

    let mut files = Vec::new();
    // A pa-core built outside the workspace (no runtime tree) embeds nothing.
    if runtime.join("pyproject.toml").is_file() && skills.is_dir() {
        for entry in RUNTIME_ENTRIES {
            let path = runtime.join(entry);
            // Directories are watched whole (cargo scans them recursively):
            // an added or removed file re-embeds too.
            println!("cargo:rerun-if-changed={}", path.display());
            if path.exists() {
                collect(&path, &format!("prime-agent-runtime/{entry}"), &mut files);
            }
        }
        println!("cargo:rerun-if-changed={}", skills.display());
        collect(&skills, "skills", &mut files);
    } else {
        println!("cargo:rerun-if-changed={}", runtime.display());
        println!("cargo:rerun-if-changed={}", skills.display());
    }

    files.sort();
    let mut source = String::from(
        "/// The embedded runtime and skills: (relative path, contents), sorted by path.\n\
         pub(crate) static EMBEDDED_FILES: &[(&str, &[u8])] = &[\n",
    );
    for (relative, path) in &files {
        let absolute = path.to_str().expect("UTF-8 asset path");
        let _ = writeln!(source, "    ({relative:?}, include_bytes!({absolute:?})),");
    }
    source.push_str("];\n");
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("embedded_bundle.rs");
    // Rewrite only on change: an unchanged bundle keeps its mtime.
    if std::fs::read_to_string(&out).ok().as_deref() != Some(source.as_str()) {
        std::fs::write(&out, source).expect("write the embedded bundle index");
    }
}
