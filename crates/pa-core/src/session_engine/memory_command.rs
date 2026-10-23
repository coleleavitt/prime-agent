//! `/memory`: what instruction files and memory this session carries
//! (Claude Code's `/memory` lists the memory files to open; a daemon
//! session has no editor, so this lists their paths to open elsewhere).

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;

use crate::resources::auto_memory::AutoMemory;
use crate::resources::instructions::{InstructionFile, NestedInstructions};

/// The instruction state a session keeps for `/memory`.
#[derive(Debug, Default)]
pub struct SessionInstructions {
    /// The session-start files, in load order.
    pub files: Vec<InstructionFile>,
    /// Files that load on first touch (`None` with `--no-context-files`).
    pub nested: Option<Arc<NestedInstructions>>,
    /// The session's auto memory (`None` when off).
    pub auto_memory: Option<AutoMemory>,
}

/// The `/memory` report.
#[must_use]
pub fn memory_report(instructions: &SessionInstructions) -> String {
    let mut text = String::new();
    if instructions.nested.is_none() {
        text.push_str("Instruction files are off for this session (--no-context-files).\n");
    } else if instructions.files.is_empty() {
        text.push_str("No instruction files were loaded at session start.\n");
    } else {
        text.push_str("Instruction files loaded at session start:\n");
        for file in &instructions.files {
            let _ = write!(text, "  [{}] {}", file.kind.as_str(), file.path.display());
            if let Some(parent) = &file.parent {
                let _ = write!(text, " (imported by {})", parent.display());
            }
            text.push('\n');
        }
    }
    if let Some(nested) = &instructions.nested {
        let delivered: Vec<PathBuf> = nested.delivered();
        if !delivered.is_empty() {
            text.push_str("Loaded when a file under them was first touched:\n");
            for path in delivered {
                let _ = writeln!(text, "  {}", path.display());
            }
        }
        let pending = nested.pending_rules();
        if !pending.is_empty() {
            text.push_str("Path-scoped rules waiting for a matching file:\n");
            for (path, globs) in pending {
                let _ = writeln!(text, "  {} (paths: {})", path.display(), globs.join(", "));
            }
        }
    }
    match &instructions.auto_memory {
        Some(memory) => {
            let state = if memory.index.is_empty() {
                "empty".to_string()
            } else if memory.truncated {
                "loaded, truncated".to_string()
            } else {
                format!("{} lines loaded", memory.index.lines().count())
            };
            let _ = writeln!(
                text,
                "Auto memory: {} (index {}: {state})",
                memory.dir.display(),
                memory.index_path().display()
            );
        }
        None => text.push_str(
            "Auto memory: off (top-level sessions only; autoMemoryEnabled, \
             PRIME_AGENT_DISABLE_AUTO_MEMORY and --no-context-files turn it off)\n",
        ),
    }
    text.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resources::instructions::{InstructionOptions, load_instructions};

    #[test]
    fn the_report_lists_loaded_delivered_and_waiting_files_and_memory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let repo = root.join("repo");
        std::fs::create_dir_all(repo.join("pkg")).unwrap();
        std::fs::create_dir_all(repo.join(".claude/rules")).unwrap();
        std::fs::write(repo.join("AGENTS.md"), "top @extra.md").unwrap();
        std::fs::write(repo.join("extra.md"), "extra").unwrap();
        std::fs::write(repo.join("pkg/AGENTS.md"), "pkg").unwrap();
        std::fs::write(
            repo.join(".claude/rules/py.md"),
            "---\npaths: \"*.py\"\n---\npy",
        )
        .unwrap();
        let (files, nested) = load_instructions(InstructionOptions {
            cwd: repo.clone(),
            agent_dir: root.join("agent"),
            home_dir: None,
            claude_user_files: false,
            additional_dirs: Vec::new(),
        });
        nested.touch(&[repo.join("pkg/x.txt")]);
        let instructions = SessionInstructions {
            files,
            nested: Some(Arc::new(nested)),
            auto_memory: Some(AutoMemory {
                dir: root.join("mem"),
                index: "- a\n- b".to_string(),
                truncated: false,
            }),
        };
        let r = |path: &str| repo.join(path).display().to_string();
        assert_eq!(
            memory_report(&instructions),
            format!(
                "Instruction files loaded at session start:\n  [project] {}\n  [project] {} \
                 (imported by {})\nLoaded when a file under them was first touched:\n  {}\n\
                 Path-scoped rules waiting for a matching file:\n  {} (paths: *.py)\n\
                 Auto memory: {} (index {}: 2 lines loaded)",
                r("AGENTS.md"),
                r("extra.md"),
                r("AGENTS.md"),
                r("pkg/AGENTS.md"),
                r(".claude/rules/py.md"),
                root.join("mem").display(),
                root.join("mem/MEMORY.md").display(),
            )
        );
        assert_eq!(
            memory_report(&SessionInstructions::default()),
            "Instruction files are off for this session (--no-context-files).\nAuto memory: off \
             (top-level sessions only; autoMemoryEnabled, PRIME_AGENT_DISABLE_AUTO_MEMORY and \
             --no-context-files turn it off)"
        );
    }
}
