//! The session's additional working directories (Claude Code's
//! `--add-dir`, `/add-dir` and `permissions.additionalDirectories`): extra
//! roots beyond the working directory that the model's environment names,
//! whose `.claude/commands` load, and that other layers (permissions,
//! context files) can read through [`AdditionalDirectories::list`].
//!
//! A `/add-dir` survives resume: each change appends a model-invisible
//! `additional_directories_state` entry holding the session-added list.

use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;

use pa_types::session::FileEntry;

/// The custom entry recording the directories `/add-dir` added.
pub const ADDITIONAL_DIRECTORIES_STATE_CUSTOM_TYPE: &str = "additional_directories_state";

/// Why a path cannot be added (Claude Code's `/add-dir` refusals).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddDirectoryError {
    Empty,
    NotFound {
        absolute: PathBuf,
    },
    NotADirectory {
        input: String,
        parent: PathBuf,
    },
    AlreadyAccessible {
        input: String,
        within: PathBuf,
        exact: bool,
        is_cwd: bool,
    },
}

impl std::fmt::Display for AddDirectoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AddDirectoryError::Empty => write!(f, "Please provide a directory path."),
            AddDirectoryError::NotFound { absolute } => {
                write!(f, "Path {} was not found.", absolute.display())
            }
            AddDirectoryError::NotADirectory { input, parent } => write!(
                f,
                "{input} is not a directory. Did you mean to add the parent directory {}?",
                parent.display()
            ),
            AddDirectoryError::AlreadyAccessible {
                input,
                within,
                exact,
                is_cwd,
            } => match (exact, is_cwd) {
                (true, true) => write!(f, "{input} is already the current working directory."),
                (true, false) => write!(f, "{input} is already added as a working directory."),
                (false, true) => write!(
                    f,
                    "{input} is already accessible within the current working directory {}.",
                    within.display()
                ),
                (false, false) => write!(
                    f,
                    "{input} is already accessible within the additional working directory {}.",
                    within.display()
                ),
            },
        }
    }
}

/// `path` made absolute against `cwd` (`~` expanded) and lexically
/// normalized, like Node's `path.resolve`.
#[must_use]
pub fn resolve_directory_input(input: &str, cwd: &Path) -> PathBuf {
    let input = input.trim();
    let home = || pa_types::platform::home_dir().unwrap_or_default();
    let joined = if input == "~" {
        home()
    } else if let Some(rest) = input.strip_prefix("~/") {
        home().join(rest)
    } else {
        cwd.join(input)
    };
    let mut normal = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                normal.pop();
            }
            Component::CurDir => {}
            other => normal.push(other),
        }
    }
    normal
}

/// Validate one directory for the session (Claude Code's
/// `validateDirectoryForWorkspace`): it must exist, be a directory, and lie
/// outside the working directory and every directory already added.
///
/// # Errors
///
/// The refusal, rendered by its `Display`.
pub fn validate_additional_directory(
    input: &str,
    cwd: &Path,
    existing: &[PathBuf],
) -> Result<PathBuf, AddDirectoryError> {
    if input.trim().is_empty() {
        return Err(AddDirectoryError::Empty);
    }
    let absolute = resolve_directory_input(input, cwd);
    match std::fs::metadata(&absolute) {
        Err(_) => return Err(AddDirectoryError::NotFound { absolute }),
        Ok(meta) if !meta.is_dir() => {
            return Err(AddDirectoryError::NotADirectory {
                input: input.trim().to_string(),
                parent: absolute.parent().map(Path::to_path_buf).unwrap_or_default(),
            });
        }
        Ok(_) => {}
    }
    for root in std::iter::once(cwd).chain(existing.iter().map(PathBuf::as_path)) {
        if absolute.starts_with(root) {
            return Err(AddDirectoryError::AlreadyAccessible {
                input: input.trim().to_string(),
                within: root.to_path_buf(),
                exact: absolute == root,
                is_cwd: root == cwd,
            });
        }
    }
    Ok(absolute)
}

/// The directories the latest `additional_directories_state` entry on a
/// branch records (empty without one).
pub(crate) fn persisted_directories<'a>(
    branch: impl DoubleEndedIterator<Item = &'a FileEntry>,
) -> Vec<PathBuf> {
    branch
        .rev()
        .find_map(|entry| {
            let FileEntry::Custom { payload, .. } = entry else {
                return None;
            };
            if payload.custom_type != ADDITIONAL_DIRECTORIES_STATE_CUSTOM_TYPE {
                return None;
            }
            let directories = payload.data.as_ref()?.get("directories")?.as_array()?;
            Some(
                directories
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .map(PathBuf::from)
                    .collect(),
            )
        })
        .unwrap_or_default()
}

/// The live list: the launch's directories (flag and settings) plus the
/// ones `/add-dir` added this session.
#[derive(Debug, Default)]
pub struct AdditionalDirectories {
    cwd: PathBuf,
    state: Mutex<DirectoryState>,
}

#[derive(Debug, Default)]
struct DirectoryState {
    all: Vec<PathBuf>,
    session_added: Vec<PathBuf>,
}

impl AdditionalDirectories {
    /// Resolve the launch's candidates (`--add-dir`, the setting) and the
    /// session's persisted additions; a candidate that fails validation
    /// is dropped (Claude Code warns and skips it at startup).
    #[must_use]
    pub fn resolve(cwd: &Path, launch: &[PathBuf], persisted: &[PathBuf]) -> Self {
        let mut state = DirectoryState::default();
        for (candidate, session_added) in launch
            .iter()
            .map(|path| (path, false))
            .chain(persisted.iter().map(|path| (path, true)))
        {
            let input = candidate.display().to_string();
            if let Ok(dir) = validate_additional_directory(&input, cwd, &state.all) {
                if session_added {
                    state.session_added.push(dir.clone());
                }
                state.all.push(dir);
            }
        }
        Self {
            cwd: cwd.to_path_buf(),
            state: Mutex::new(state),
        }
    }

    /// Every additional directory, in the order added.
    #[must_use]
    pub fn list(&self) -> Vec<PathBuf> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .all
            .clone()
    }

    /// Validate and add one directory; returns the added path and the
    /// session-added list to persist.
    ///
    /// # Errors
    ///
    /// The validation refusal.
    pub fn add(&self, input: &str) -> Result<(PathBuf, Vec<PathBuf>), AddDirectoryError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = validate_additional_directory(input, &self.cwd, &state.all)?;
        state.all.push(dir.clone());
        state.session_added.push(dir.clone());
        Ok((dir, state.session_added.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validation_matches_the_claude_code_refusals() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("work");
        let other = tmp.path().join("other");
        std::fs::create_dir_all(cwd.join("sub")).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("file.txt"), "x").unwrap();
        let message = |input: &str, existing: &[PathBuf]| {
            validate_additional_directory(input, &cwd, existing).map_err(|error| error.to_string())
        };
        assert_eq!(message("../other", &[]), Ok(other.clone()));
        assert_eq!(
            message("", &[]),
            Err("Please provide a directory path.".to_string())
        );
        assert_eq!(
            message("../missing", &[]),
            Err(format!(
                "Path {} was not found.",
                tmp.path().join("missing").display()
            ))
        );
        assert_eq!(
            message("../other/file.txt", &[]),
            Err(format!(
                "../other/file.txt is not a directory. Did you mean to add the parent directory {}?",
                other.display()
            ))
        );
        assert_eq!(
            message("sub", &[]),
            Err(format!(
                "sub is already accessible within the current working directory {}.",
                cwd.display()
            ))
        );
        assert_eq!(
            message(".", &[]),
            Err(". is already the current working directory.".to_string())
        );
        assert_eq!(
            message("../other", std::slice::from_ref(&other)),
            Err("../other is already added as a working directory.".to_string())
        );
    }

    #[test]
    fn launch_and_persisted_directories_merge_and_additions_persist() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("work");
        let (a, b, c) = (
            tmp.path().join("a"),
            tmp.path().join("b"),
            tmp.path().join("c"),
        );
        for dir in [&cwd, &a, &b, &c] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let dirs = AdditionalDirectories::resolve(
            &cwd,
            &[a.clone(), tmp.path().join("gone"), a.clone()],
            std::slice::from_ref(&b),
        );
        assert_eq!(dirs.list(), vec![a.clone(), b.clone()]);
        assert_eq!(
            dirs.add(&c.display().to_string()),
            Ok((c.clone(), vec![b.clone(), c.clone()]))
        );
        assert_eq!(dirs.list(), vec![a, b, c]);
    }

    #[test]
    fn the_latest_state_entry_wins() {
        let entry = |dirs: &[&str]| FileEntry::Custom {
            payload: pa_types::session::CustomEntry {
                custom_type: ADDITIONAL_DIRECTORIES_STATE_CUSTOM_TYPE.to_string(),
                data: Some(serde_json::json!({ "directories": dirs })),
                rest: serde_json::Map::default(),
            },
            base: pa_types::session::EntryBase {
                id: None,
                parent_id: None,
                timestamp: None,
                rest: serde_json::Map::default(),
            },
        };
        let entries = [entry(&["/x"]), entry(&["/x", "/y"])];
        assert_eq!(
            persisted_directories(entries.iter()),
            vec![PathBuf::from("/x"), PathBuf::from("/y")]
        );
    }
}
