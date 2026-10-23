//! Claude Code's dynamic skill discovery: a file the session touches below the working directory
//! brings in the `.claude/skills` directories between it and the working directory (Claude
//! Code's `OAr` / `lYt`, delivered as its `dynamic_skill` attachment).
//!
//! Each directory is checked once per session; a directory git ignores is skipped (Claude Code's
//! `TTr`); a discovered skill whose name the session already has is skipped (the session's skills
//! were loaded first, so they win). The working directory's own `.claude/skills` and its
//! ancestors' load at session start ([`super::claude_skills`]), so the walk covers only the
//! directories strictly below the working directory.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use super::Skill;
use super::claude_commands::{CLAUDE_DIR_NAME, ShellExpansion};
use super::claude_skills::{CLAUDE_SKILLS_DIR, ClaudeSkillRoots, load_claude_skills};

/// One directory's newly discovered skills.
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredSkills {
    /// The `.claude/skills` directory.
    pub dir: PathBuf,
    pub skills: Vec<Skill>,
}

#[derive(Debug, Default)]
struct State {
    checked: HashSet<PathBuf>,
    skills: Vec<Skill>,
}

/// The session's dynamically discovered skills.
#[derive(Debug)]
pub struct DynamicSkills {
    cwd: PathBuf,
    shell: ShellExpansion,
    state: Mutex<State>,
}

impl DynamicSkills {
    #[must_use]
    pub fn new(cwd: PathBuf, shell: ShellExpansion) -> Self {
        Self {
            cwd,
            shell,
            state: Mutex::default(),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The skills discovered so far, in discovery order.
    #[must_use]
    pub fn skills(&self) -> Vec<Skill> {
        self.state().skills.clone()
    }

    /// `known` followed by the discovered skills whose names it lacks.
    #[must_use]
    pub fn with_known(&self, known: &[Skill]) -> Vec<Skill> {
        let mut skills = known.to_vec();
        for skill in self.skills() {
            if !skills.iter().any(|existing| existing.name == skill.name) {
                skills.push(skill);
            }
        }
        skills
    }

    /// Check the `.claude/skills` directories between each touched path and the working
    /// directory; returns the skills new to the session, per directory, nearest first.
    pub fn touch(&self, touched: &[PathBuf], known: &[Skill]) -> Vec<DiscoveredSkills> {
        let mut found = Vec::new();
        for path in touched {
            let path = if path.is_absolute() {
                path.clone()
            } else {
                self.cwd.join(path)
            };
            let mut dir = path.parent();
            while let Some(current) = dir {
                if current == self.cwd || !current.starts_with(&self.cwd) {
                    break;
                }
                let candidate = current.join(CLAUDE_DIR_NAME).join(CLAUDE_SKILLS_DIR);
                if self.state().checked.insert(candidate.clone()) && candidate.is_dir() {
                    if git_ignores(current, &candidate) {
                        tracing::debug!(dir = %candidate.display(), "skipped gitignored skills dir");
                    } else if let Some(discovered) = self.load(&candidate, known) {
                        found.push(discovered);
                    }
                }
                dir = current.parent();
            }
        }
        found
    }

    fn load(&self, dir: &Path, known: &[Skill]) -> Option<DiscoveredSkills> {
        let load = load_claude_skills(&ClaudeSkillRoots {
            user_dir: None,
            project_dirs: vec![dir.to_path_buf()],
            shell: self.shell,
        });
        let mut state = self.state();
        let mut skills = Vec::new();
        for skill in load.skills {
            let taken = known.iter().any(|existing| existing.name == skill.name)
                || state
                    .skills
                    .iter()
                    .any(|existing| existing.name == skill.name);
            if taken {
                tracing::debug!(
                    skill = %skill.name,
                    path = %skill.file_path.display(),
                    "a discovered skill's name is taken; the loaded skill wins"
                );
                continue;
            }
            state.skills.push(skill.clone());
            skills.push(skill);
        }
        (!skills.is_empty()).then(|| DiscoveredSkills {
            dir: dir.to_path_buf(),
            skills,
        })
    }
}

/// Whether git ignores `path` in the repository around `dir` (no repository, or no git: not
/// ignored).
fn git_ignores(dir: &Path, path: &Path) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["check-ignore", "-q", "--"])
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, name: &str) {
        let dir = root.join(".claude").join("skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\ndescription: {name} skill\n---\nDo {name}."),
        )
        .unwrap();
    }

    #[test]
    fn a_touched_file_brings_in_the_skills_dirs_below_the_working_directory_once() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("repo");
        let package = cwd.join("packages").join("web");
        std::fs::create_dir_all(package.join("src")).unwrap();
        write_skill(&package, "deploy-web");
        write_skill(&cwd.join("packages"), "lint-all");
        // The working directory's own skills loaded at session start.
        write_skill(&cwd, "root-skill");
        let dynamic = DynamicSkills::new(cwd.clone(), ShellExpansion::Disabled);
        let touched = vec![package.join("src").join("main.rs")];
        let found = dynamic.touch(&touched, &[]);
        let summary: Vec<(PathBuf, Vec<String>)> = found
            .iter()
            .map(|found| {
                (
                    found.dir.clone(),
                    found
                        .skills
                        .iter()
                        .map(|skill| skill.name.clone())
                        .collect(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            vec![
                (
                    package.join(".claude").join("skills"),
                    vec!["deploy-web".to_string()]
                ),
                (
                    cwd.join("packages").join(".claude").join("skills"),
                    vec!["lint-all".to_string()]
                ),
            ]
        );
        assert_eq!(dynamic.touch(&touched, &[]), Vec::new(), "checked once");
        assert_eq!(
            dynamic
                .skills()
                .iter()
                .map(|skill| skill.name.as_str())
                .collect::<Vec<_>>(),
            vec!["deploy-web", "lint-all"]
        );
    }

    #[test]
    fn a_name_the_session_already_has_is_not_rediscovered() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_path_buf();
        let sub = cwd.join("sub");
        write_skill(&sub, "deploy");
        let known = load_claude_skills(&ClaudeSkillRoots {
            user_dir: None,
            project_dirs: vec![sub.join(".claude").join("skills")],
            shell: ShellExpansion::Disabled,
        })
        .skills;
        let dynamic = DynamicSkills::new(cwd, ShellExpansion::Disabled);
        assert_eq!(dynamic.touch(&[sub.join("file.txt")], &known), Vec::new());
        assert_eq!(dynamic.with_known(&known).len(), 1);
    }

    #[test]
    fn paths_outside_the_working_directory_discover_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("repo");
        std::fs::create_dir_all(&cwd).unwrap();
        let outside = tmp.path().join("other");
        write_skill(&outside, "elsewhere");
        let dynamic = DynamicSkills::new(cwd, ShellExpansion::Disabled);
        assert_eq!(dynamic.touch(&[outside.join("x.txt")], &[]), Vec::new());
    }
}
