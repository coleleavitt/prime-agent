//! `/skill-doctor` (Claude Code's `skill-doctor`): which loaded skills are
//! unused and costing context. Each loaded skill's row shows its source,
//! what its inventory entry costs every turn (estimated, chars / 4; a skill
//! with `disable-model-invocation` costs nothing until it runs), how many of
//! this project's sessions over the last 7 days used it, and when.
//!
//! A use is a session entry that invokes the skill (`/skill:<name>` and the
//! commands that run a skill deliver a `<skill name="<name>"` block), reads
//! its `SKILL.md`, or calls its Python module (`<import>.`). The counts are
//! per session file, from this project's session directory only.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::skills::Skill;

/// The usage window (Claude Code's "7d").
pub const WINDOW: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// One skill's row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillUsageRow {
    pub name: String,
    pub source: String,
    /// The inventory entry's estimated tokens; `None` when the skill is
    /// not in the inventory.
    pub context_tokens: Option<u64>,
    /// Sessions in the window that used the skill.
    pub uses: usize,
    /// The newest such session's modification time.
    pub last_used: Option<SystemTime>,
}

/// Whether the user installed the skill (a bundled or package skill is
/// never reported as dead weight).
fn user_owned(source: &str) -> bool {
    matches!(source, "user" | "project" | "claude")
}

/// The markers a session entry carries when it uses `skill`.
fn markers(skill: &Skill) -> Vec<String> {
    let mut markers = vec![
        // Inside a JSONL string the attribute quotes are escaped.
        format!("<skill name=\\\"{}\\\"", skill.name),
        skill.file_path.display().to_string(),
    ];
    if let Some(python) = &skill.python {
        markers.push(format!("{}.", python.import_name));
    }
    markers
}

/// The session files modified inside the window.
fn recent_sessions(session_dir: &Path, now: SystemTime) -> Vec<(PathBuf, SystemTime)> {
    let Ok(entries) = std::fs::read_dir(session_dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .filter_map(|path| {
            let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
            let age = now.duration_since(modified).unwrap_or_default();
            (age <= WINDOW).then_some((path, modified))
        })
        .collect()
}

/// Every loaded skill's row, in load order.
#[must_use]
pub fn skill_usage(skills: &[Skill], session_dir: &Path, now: SystemTime) -> Vec<SkillUsageRow> {
    let sessions: Vec<(String, SystemTime)> = recent_sessions(session_dir, now)
        .into_iter()
        .filter_map(|(path, modified)| Some((std::fs::read_to_string(path).ok()?, modified)))
        .collect();
    skills
        .iter()
        .map(|skill| {
            let markers = markers(skill);
            let hits: Vec<SystemTime> = sessions
                .iter()
                .filter(|(text, _)| markers.iter().any(|marker| text.contains(marker.as_str())))
                .map(|(_, modified)| *modified)
                .collect();
            SkillUsageRow {
                name: skill.name.clone(),
                source: skill.source_info.source.clone(),
                context_tokens: (!skill.disable_model_invocation).then(|| {
                    super::context_breakdown::estimate_text_tokens(
                        &crate::skills::skill_inventory_entry(skill),
                    )
                }),
                uses: hits.len(),
                last_used: hits.into_iter().max(),
            }
        })
        .collect()
}

fn ago(when: Option<SystemTime>, now: SystemTime) -> String {
    let Some(when) = when else {
        return "never (7d)".to_string();
    };
    let seconds = now.duration_since(when).unwrap_or_default().as_secs();
    match seconds {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", seconds / 60),
        3600..=86_399 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

/// The `/skill-doctor` report text.
#[must_use]
pub fn skill_doctor_report(rows: &[SkillUsageRow], now: SystemTime) -> String {
    let mut text = String::from("Skills loaded this session\n\n");
    if rows.is_empty() {
        text.push_str("  (no skills loaded)");
        return text;
    }
    let name_width = rows
        .iter()
        .map(|row| row.name.chars().count())
        .max()
        .unwrap_or(0)
        .max("skill".len());
    let source_width = rows
        .iter()
        .map(|row| row.source.chars().count())
        .max()
        .unwrap_or(0)
        .max("source".len());
    let _ = writeln!(
        text,
        "  {:<name_width$}  {:<source_width$}  {:>7}  {:>4}  last used",
        "skill", "source", "context", "uses"
    );
    for row in rows {
        let context = row
            .context_tokens
            .map_or_else(|| "-".to_string(), |tokens| tokens.to_string());
        let _ = writeln!(
            text,
            "  {:<name_width$}  {:<source_width$}  {context:>7}  {:>3}x  {}",
            row.name,
            row.source,
            row.uses,
            ago(row.last_used, now)
        );
    }
    text.push_str(
        "\n  context = this skill's entry in the skills inventory, sent every turn (est. tokens)\n  \
         (dash = not in the inventory, costs nothing; the full SKILL.md loads only when it runs)\n  \
         uses = this project's sessions over the last 7 days that ran or read the skill\n",
    );
    let unused: Vec<&SkillUsageRow> = rows
        .iter()
        .filter(|row| row.uses == 0 && row.context_tokens.is_some() && user_owned(&row.source))
        .collect();
    if !unused.is_empty() {
        let tokens: u64 = unused.iter().filter_map(|row| row.context_tokens).sum();
        let _ = write!(
            text,
            "\n{} {} loaded but never used ({tokens} est. tokens every turn): {}. Set \
             `disable-model-invocation: true` in a SKILL.md to keep the skill as a command only, or \
             remove it from its skills directory.",
            unused.len(),
            if unused.len() == 1 { "skill" } else { "skills" },
            unused
                .iter()
                .map(|row| row.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        );
    } else if rows.iter().all(|row| row.uses > 0) {
        text.push_str("\nAll loaded skills have been used at least once.");
    }
    text.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::{SkillKind, SourceScope, create_synthetic_source_info};

    fn skill(dir: &Path, name: &str, source: &str, hidden: bool) -> Skill {
        let base = dir.join(name);
        Skill {
            name: name.to_string(),
            description: format!("{name} does things"),
            file_path: base.join("SKILL.md"),
            base_dir: base.clone(),
            source_info: create_synthetic_source_info(
                &base.join("SKILL.md").display().to_string(),
                source,
                SourceScope::Project,
                None,
            ),
            disable_model_invocation: hidden,
            kind: SkillKind::Markdown,
            python: None,
            when_to_use: None,
            user_invocable: true,
            claude: None,
        }
    }

    #[test]
    fn rows_count_invocations_and_reads_within_the_window() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions = tmp.path().join("sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let skills_dir = tmp.path().join("skills");
        let skills = vec![
            skill(&skills_dir, "deploy", "project", false),
            skill(&skills_dir, "notes", "user", false),
            skill(&skills_dir, "release", "user", true),
            skill(&skills_dir, "code-review", "path", false),
        ];
        std::fs::write(
            sessions.join("a.jsonl"),
            "{\"text\":\"<skill name=\\\"deploy\\\" location=\\\"x\\\">\"}\n",
        )
        .unwrap();
        std::fs::write(
            sessions.join("b.jsonl"),
            format!(
                "{{\"code\":\"open('{}')\"}}\n",
                skills_dir.join("deploy/SKILL.md").display()
            ),
        )
        .unwrap();
        std::fs::write(sessions.join("notes.txt"), "<skill name=\\\"notes\\\"").unwrap();
        let now = SystemTime::now();
        let rows = skill_usage(&skills, &sessions, now);
        assert_eq!(
            rows.iter()
                .map(|row| (row.name.as_str(), row.uses, row.context_tokens.is_some()))
                .collect::<Vec<_>>(),
            [
                ("deploy", 2, true),
                ("notes", 0, true),
                ("release", 0, false),
                ("code-review", 0, true)
            ]
        );
        // A week later nothing is in the window.
        let later = now + WINDOW + Duration::from_secs(60);
        assert!(skill_usage(&skills, &sessions, later)
            .iter()
            .all(|row| row.uses == 0));

        let report = skill_doctor_report(&rows, now);
        assert!(report.starts_with("Skills loaded this session\n\n  skill"));
        assert!(report.contains("deploy       project"));
        assert!(report.contains("  2x  just now"));
        assert!(report.contains("never (7d)"));
        // Only the user's own model-visible skill is dead weight: a hidden
        // skill costs nothing and a bundled one is never reported.
        assert!(report.contains("\n1 skill loaded but never used ("));
        assert!(report.contains("): notes. Set `disable-model-invocation: true`"));
    }

    #[test]
    fn an_empty_list_says_so() {
        assert_eq!(
            skill_doctor_report(&[], SystemTime::now()),
            "Skills loaded this session\n\n  (no skills loaded)"
        );
    }
}
