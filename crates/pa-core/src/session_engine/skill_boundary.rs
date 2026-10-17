//! Dynamic skills at the turn boundary (Claude Code's `dynamic_skill` attachment): after a tool
//! batch, the files its cells opened may bring in the `.claude/skills` directories below the
//! working directory ([`crate::skills::dynamic`]). The new skills join the running run as one
//! hidden row listing their inventory entries, delivered through the steering queue like the
//! nested instruction files, so the model reads them before its next request.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::steering_fold::FoldAdmissionSlot;
use super::{SessionAgentMessage, session_message_to_loop};
use crate::skills::Skill;
use crate::skills::dynamic::{DiscoveredSkills, DynamicSkills};

/// The custom type of the hidden row announcing discovered skills.
pub const DYNAMIC_SKILLS_CUSTOM_TYPE: &str = "dynamic_skills";

/// The row text's first line: a harness message, not the user.
const DYNAMIC_SKILLS_HEADER: &str = "[dynamic-skills]";

/// The hidden row for the discovered skills the model may load, or `None` when there are none.
#[must_use]
pub(super) fn discovered_row(
    cwd: &Path,
    discovered: &[DiscoveredSkills],
) -> Option<pa_types::session::CustomMessage> {
    let mut sections = Vec::new();
    let mut names = Vec::new();
    for found in discovered {
        let listed: Vec<&Skill> = found
            .skills
            .iter()
            .filter(|skill| !skill.disable_model_invocation)
            .collect();
        if listed.is_empty() {
            continue;
        }
        // Claude Code's wording (`dynamic_skill`), with the inventory entries the session
        // context uses for every other skill.
        let shown = found
            .dir
            .strip_prefix(cwd)
            .unwrap_or(&found.dir)
            .display()
            .to_string();
        let mut lines = vec![
            format!("New skills discovered in {shown}, now available:"),
            "<available_skills>".to_string(),
        ];
        lines.extend(
            listed
                .iter()
                .map(|skill| crate::skills::skill_inventory_entry(skill)),
        );
        lines.push("</available_skills>".to_string());
        sections.push(lines.join("\n"));
        names.extend(listed.iter().map(|skill| skill.name.clone()));
    }
    if sections.is_empty() {
        return None;
    }
    let text = format!("{DYNAMIC_SKILLS_HEADER}\n{}", sections.join("\n\n"));
    let mut row = super::plan_mode::hidden_row(DYNAMIC_SKILLS_CUSTOM_TYPE, &text);
    row.details = Some(serde_json::json!({
        "skills": names,
        "dirs": discovered
            .iter()
            .map(|found| found.dir.display().to_string())
            .collect::<Vec<_>>(),
    }));
    Some(row)
}

/// Discover the skills a finished batch's touched files bring in and deliver them to the
/// running run.
pub(super) async fn after_turn(
    slot: &FoldAdmissionSlot,
    dynamic: &Arc<DynamicSkills>,
    known: &Arc<Vec<Skill>>,
    touched: Vec<PathBuf>,
) {
    if touched.is_empty() {
        return;
    }
    let Some(admission) = slot.get() else {
        return;
    };
    let Some(agent) = admission.agent.upgrade() else {
        return;
    };
    let cwd = admission.cwd.clone();
    let worker = {
        let dynamic = Arc::clone(dynamic);
        let known = Arc::clone(known);
        // Directory reads and the `git check-ignore` probe stay off the runtime's workers.
        tokio::task::spawn_blocking(move || dynamic.touch(&touched, &known))
    };
    let Ok(discovered) = worker.await else {
        return;
    };
    let Some(row) = discovered_row(&cwd, &discovered) else {
        return;
    };
    if let Some(row) = session_message_to_loop(&SessionAgentMessage::Custom(row)) {
        agent.steer(row);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::claude_commands::ShellExpansion;

    #[test]
    fn the_row_lists_the_model_invocable_skills_per_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().to_path_buf();
        let skills_dir = cwd.join("web").join(".claude").join("skills");
        for (name, extra) in [
            ("deploy", "when_to_use: before a release\n"),
            ("secret", "disable-model-invocation: true\n"),
        ] {
            std::fs::create_dir_all(skills_dir.join(name)).unwrap();
            std::fs::write(
                skills_dir.join(name).join("SKILL.md"),
                format!("---\ndescription: {name} it\n{extra}---\nbody"),
            )
            .unwrap();
        }
        let dynamic = DynamicSkills::new(cwd.clone(), ShellExpansion::Disabled);
        let discovered = dynamic.touch(&[cwd.join("web").join("index.ts")], &[]);
        let row = discovered_row(&cwd, &discovered).expect("a row");
        let pa_types::ai::UserContent::Text(text) = &row.content else {
            panic!("text row");
        };
        let deploy = &discovered[0].skills[0];
        assert_eq!(
            text,
            &format!(
                "[dynamic-skills]\nNew skills discovered in web/.claude/skills, now available:\n<available_skills>\n{}\n</available_skills>",
                crate::skills::skill_inventory_entry(deploy)
            )
        );
        assert!(text.contains("<description>deploy it - before a release</description>"));
        assert_eq!(row.custom_type, DYNAMIC_SKILLS_CUSTOM_TYPE);
        assert!(!row.display);
        assert_eq!(
            row.details.as_ref().unwrap()["skills"],
            serde_json::json!(["deploy"])
        );
    }

    #[test]
    fn nothing_model_invocable_is_no_row() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(discovered_row(tmp.path(), &[]), None);
    }
}
