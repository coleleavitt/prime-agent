//! A plugin's optional manifest, `<name>.toml` beside `<name>.wasm`: what the
//! sandbox grants it and how long and large its calls may run. Without one a
//! plugin gets the defaults: no directories, no environment, 2 s per call,
//! 64 MiB of linear memory.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The default per-call deadline.
pub const DEFAULT_TIMEOUT_MS: u64 = 2_000;
/// The default linear-memory cap.
pub const DEFAULT_MEMORY_MIB: u64 = 64;
/// The largest deadline a manifest may ask for: the hooks sit on the tool
/// and prompt paths.
pub const MAX_TIMEOUT_MS: u64 = 60_000;
/// The largest memory cap a manifest may ask for.
pub const MAX_MEMORY_MIB: u64 = 4_096;

/// One directory the plugin may see.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirGrant {
    /// The host directory: absolute, or relative to the plugin's own directory.
    pub host: PathBuf,
    /// Where the plugin sees it (`/data`, `.`).
    pub guest: String,
    /// Whether the plugin may write there (default read-only).
    #[serde(default)]
    pub write: bool,
}

/// The manifest as written.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    timeout_ms: Option<u64>,
    memory_mib: Option<u64>,
    #[serde(default)]
    env: Vec<String>,
    #[serde(default)]
    dirs: Vec<DirGrant>,
}

/// A plugin's resolved sandbox settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub timeout_ms: u64,
    pub memory_bytes: usize,
    /// Host environment variables passed through by name (absent ones skipped).
    pub env: Vec<String>,
    /// Directories with their host paths resolved.
    pub dirs: Vec<DirGrant>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self::resolve(RawManifest::default(), Path::new("."))
    }
}

impl Manifest {
    /// The manifest beside `wasm` (`<stem>.toml`), or the defaults when there
    /// is none.
    ///
    /// # Errors
    ///
    /// The manifest exists but cannot be read or parsed: the plugin is not
    /// loaded (a sandbox grant must never be guessed).
    pub fn for_plugin(wasm: &Path) -> Result<Self, String> {
        let path = wasm.with_extension("toml");
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
        };
        let raw: RawManifest =
            toml::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))?;
        let base = wasm.parent().unwrap_or(Path::new("."));
        Ok(Self::resolve(raw, base))
    }

    fn resolve(raw: RawManifest, base: &Path) -> Self {
        let timeout_ms = raw
            .timeout_ms
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .clamp(1, MAX_TIMEOUT_MS);
        let memory_mib = raw
            .memory_mib
            .unwrap_or(DEFAULT_MEMORY_MIB)
            .clamp(1, MAX_MEMORY_MIB);
        let dirs = raw
            .dirs
            .into_iter()
            .map(|grant| DirGrant {
                host: if grant.host.is_absolute() {
                    grant.host
                } else {
                    base.join(grant.host)
                },
                ..grant
            })
            .collect();
        Self {
            timeout_ms,
            memory_bytes: usize::try_from(memory_mib * 1024 * 1024).unwrap_or(usize::MAX),
            env: raw.env,
            dirs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_manifest_is_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = Manifest::for_plugin(&dir.path().join("p.wasm")).unwrap();
        assert_eq!(
            manifest,
            Manifest {
                timeout_ms: DEFAULT_TIMEOUT_MS,
                memory_bytes: 64 * 1024 * 1024,
                env: Vec::new(),
                dirs: Vec::new(),
            }
        );
    }

    #[test]
    fn grants_resolve_beside_the_plugin_and_limits_clamp() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("p.toml"),
            "timeout_ms = 999999\nmemory_mib = 8\nenv = [\"HOME\"]\n\
             [[dirs]]\nhost = \"data\"\nguest = \"/data\"\n\
             [[dirs]]\nhost = \"/abs\"\nguest = \".\"\nwrite = true\n",
        )
        .unwrap();
        let manifest = Manifest::for_plugin(&dir.path().join("p.wasm")).unwrap();
        assert_eq!(
            manifest,
            Manifest {
                timeout_ms: MAX_TIMEOUT_MS,
                memory_bytes: 8 * 1024 * 1024,
                env: vec!["HOME".to_string()],
                dirs: vec![
                    DirGrant {
                        host: dir.path().join("data"),
                        guest: "/data".to_string(),
                        write: false,
                    },
                    DirGrant {
                        host: PathBuf::from("/abs"),
                        guest: ".".to_string(),
                        write: true,
                    },
                ],
            }
        );
    }

    #[test]
    fn an_unknown_key_refuses_the_manifest() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("p.toml"), "network = true\n").unwrap();
        let error = Manifest::for_plugin(&dir.path().join("p.wasm")).unwrap_err();
        assert!(error.contains("unknown field `network`"), "{error}");
    }
}
