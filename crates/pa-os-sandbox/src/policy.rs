//! The sandbox vocabulary: the user-facing mode, the enforced confinement,
//! and the resolved policy one launch applies.

use std::path::{Path, PathBuf};

/// The `sandbox.mode` setting and the `--sandbox` flag (the Codex CLI's
/// names, with `off` in place of `danger-full-access`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxMode {
    /// No OS confinement (the default): every process runs as before.
    #[default]
    Off,
    /// Nothing outside the kernel's own scratch directories is writable.
    ReadOnly,
    /// The working directory, the scratch directories and the configured
    /// writable roots are writable; everything else is read-only.
    WorkspaceWrite,
}

impl SandboxMode {
    /// Every mode, in the order the CLI help lists them.
    pub const ALL: [SandboxMode; 3] = [
        SandboxMode::Off,
        SandboxMode::ReadOnly,
        SandboxMode::WorkspaceWrite,
    ];

    /// Parse the wire form (`off` | `read-only` | `workspace-write`).
    #[must_use]
    pub fn from_wire(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|mode| mode.wire_name() == value)
    }

    /// The wire form used by settings, the CLI flag and the session state.
    #[must_use]
    pub fn wire_name(self) -> &'static str {
        match self {
            SandboxMode::Off => "off",
            SandboxMode::ReadOnly => "read-only",
            SandboxMode::WorkspaceWrite => "workspace-write",
        }
    }

    /// The adoption-telemetry category (`off` | `read_only` | `workspace_write`).
    #[must_use]
    pub fn telemetry_name(self) -> &'static str {
        match self {
            SandboxMode::Off => "off",
            SandboxMode::ReadOnly => "read_only",
            SandboxMode::WorkspaceWrite => "workspace_write",
        }
    }

    /// The enforced level, `None` for [`SandboxMode::Off`].
    #[must_use]
    pub fn confinement(self) -> Option<Confinement> {
        match self {
            SandboxMode::Off => None,
            SandboxMode::ReadOnly => Some(Confinement::ReadOnly),
            SandboxMode::WorkspaceWrite => Some(Confinement::WorkspaceWrite),
        }
    }

    /// The stricter of two modes (`read-only` > `workspace-write` > `off`):
    /// how a project scope may tighten, never loosen, the user's mode.
    #[must_use]
    pub fn stricter(self, other: SandboxMode) -> SandboxMode {
        let rank = |mode: SandboxMode| match mode {
            SandboxMode::Off => 0,
            SandboxMode::WorkspaceWrite => 1,
            SandboxMode::ReadOnly => 2,
        };
        if rank(other) > rank(self) {
            other
        } else {
            self
        }
    }
}

/// What an enabled sandbox lets a confined process write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confinement {
    /// Only the scratch directories (and the terminal devices).
    ReadOnly,
    /// The workspace, the extra writable roots and the scratch directories.
    WorkspaceWrite,
}

impl Confinement {
    /// The mode this confinement enforces.
    #[must_use]
    pub fn mode(self) -> SandboxMode {
        match self {
            Confinement::ReadOnly => SandboxMode::ReadOnly,
            Confinement::WorkspaceWrite => SandboxMode::WorkspaceWrite,
        }
    }
}

/// Whether a confined process may open network sockets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkAccess {
    /// Network sockets work as usual.
    Allowed,
    /// Every socket outside the unix domain is refused (TCP connect and
    /// bind, UDP, raw sockets); local unix sockets and pipes keep working.
    Denied,
}

/// One enabled sandbox: the level, the network toggle, and the user's
/// extra writable roots (absolute paths; honored by
/// [`Confinement::WorkspaceWrite`] only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxPolicy {
    pub confinement: Confinement,
    pub network: NetworkAccess,
    pub writable_roots: Vec<PathBuf>,
}

/// The per-launch directories a policy is applied against.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SandboxPaths {
    /// The working directory (writable under `workspace-write`).
    pub workspace: PathBuf,
    /// The confined process's own scratch (the temp directory, the session
    /// artifacts, the kernel's state files): writable in every mode, since
    /// the kernel cannot run without them.
    pub scratch: Vec<PathBuf>,
}

impl SandboxPolicy {
    /// The directories the confined process may write, canonicalized,
    /// deduplicated and in a stable order. Missing directories are dropped:
    /// nothing can be created under a path whose parent is read-only.
    #[must_use]
    pub fn writable_roots_for(&self, paths: &SandboxPaths) -> Vec<PathBuf> {
        let mut candidates: Vec<&Path> = paths.scratch.iter().map(PathBuf::as_path).collect();
        if self.confinement == Confinement::WorkspaceWrite {
            candidates.push(&paths.workspace);
            candidates.extend(self.writable_roots.iter().map(PathBuf::as_path));
        }
        let mut roots: Vec<PathBuf> = Vec::new();
        for candidate in candidates {
            let Ok(root) = candidate.canonicalize() else {
                continue;
            };
            if !roots.contains(&root) {
                roots.push(root);
            }
        }
        roots
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_round_trip_their_wire_names() {
        let parsed: Vec<Option<SandboxMode>> = ["off", "read-only", "workspace-write", "full"]
            .into_iter()
            .map(SandboxMode::from_wire)
            .collect();
        assert_eq!(
            parsed,
            vec![
                Some(SandboxMode::Off),
                Some(SandboxMode::ReadOnly),
                Some(SandboxMode::WorkspaceWrite),
                None,
            ]
        );
    }

    #[test]
    fn a_project_scope_can_only_tighten_the_mode() {
        use SandboxMode::{Off, ReadOnly, WorkspaceWrite};
        let pairs = [
            (Off, WorkspaceWrite),
            (WorkspaceWrite, Off),
            (WorkspaceWrite, ReadOnly),
            (ReadOnly, WorkspaceWrite),
            (ReadOnly, Off),
        ];
        let merged: Vec<SandboxMode> = pairs
            .into_iter()
            .map(|(user, project)| user.stricter(project))
            .collect();
        assert_eq!(
            merged,
            vec![WorkspaceWrite, WorkspaceWrite, ReadOnly, ReadOnly, ReadOnly]
        );
    }

    #[test]
    fn read_only_writes_only_the_scratch_and_workspace_write_adds_the_roots() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for name in ["work", "tmp", "extra"] {
            std::fs::create_dir(root.join(name)).unwrap();
        }
        let paths = SandboxPaths {
            workspace: root.join("work"),
            // The duplicate collapses; a missing directory drops out.
            scratch: vec![root.join("tmp"), root.join("tmp/../tmp"), root.join("gone")],
        };
        let policy = |confinement| SandboxPolicy {
            confinement,
            network: NetworkAccess::Denied,
            writable_roots: vec![root.join("extra")],
        };
        assert_eq!(
            (
                policy(Confinement::ReadOnly).writable_roots_for(&paths),
                policy(Confinement::WorkspaceWrite).writable_roots_for(&paths),
            ),
            (
                vec![root.join("tmp")],
                vec![root.join("tmp"), root.join("work"), root.join("extra")],
            )
        );
    }
}
