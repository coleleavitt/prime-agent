//! `skill.load`: the model loads a skill's instructions (Claude Code's Skill tool), the kernel's
//! `rlm.load_skill(name, args="")`.
//!
//! The reply is the invocation text Claude Code injects when the model calls its Skill tool: for a
//! Claude Code skill the base-directory line and the body with the arguments, `${CLAUDE_SKILL_DIR}`,
//! `${CLAUDE_PROJECT_DIR}` and `${CLAUDE_SESSION_ID}` substituted and the `` !`cmd` `` blocks run
//! (arguments no placeholder consumed are appended as `ARGUMENTS: ...`); for a Prime Agent skill
//! its body after the references note. A skill with `disable-model-invocation` is refused with
//! Claude Code's reason.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::kernel::shared::{HostRequestHandlers, host_handler};
use crate::skills::Skill;
use crate::skills::claude_commands::UnusedArguments;
use crate::skills::claude_skills::SkillVariables;
use crate::skills::dynamic::DynamicSkills;

/// The host request `rlm.load_skill` sends.
pub const SKILL_LOAD_REQUEST: &str = "skill.load";

/// What the handler resolves skills against.
pub struct SkillHostConfig {
    pub skills: Vec<Skill>,
    pub dynamic: Option<Arc<DynamicSkills>>,
    pub cwd: PathBuf,
    pub session_id: String,
}

/// The reply to one `skill.load` request, or the refusal the model reads.
///
/// # Errors
///
/// Error when the payload is malformed, the skill is unknown or reserved for the user, or its
/// file cannot be read.
pub async fn load_skill(config: &SkillHostConfig, data: &Value) -> anyhow::Result<Value> {
    let Some(name) = data.get("name").and_then(Value::as_str) else {
        anyhow::bail!("skill.load name must be a string");
    };
    let args = match data.get("args") {
        None | Some(Value::Null) => "",
        Some(Value::String(args)) => args.as_str(),
        Some(_) => anyhow::bail!("skill.load args must be a string"),
    };
    let name = name.trim().trim_start_matches('/');
    let name = name.strip_prefix("skill:").unwrap_or(name);
    let skills = match &config.dynamic {
        Some(dynamic) => dynamic.with_known(&config.skills),
        None => config.skills.clone(),
    };
    let Some(skill) = skills.iter().find(|skill| skill.name == name) else {
        let mut available: Vec<&str> = skills
            .iter()
            .filter(|skill| !skill.disable_model_invocation)
            .map(|skill| skill.name.as_str())
            .collect();
        available.sort_unstable();
        anyhow::bail!(
            "Unknown skill: {name}. Available skills: {}",
            if available.is_empty() {
                "none".to_string()
            } else {
                available.join(", ")
            }
        );
    };
    if skill.disable_model_invocation {
        anyhow::bail!(
            "Skill {name} cannot be loaded by the model due to disable-model-invocation. The user can run it as /{name}. Do not replicate this skill's workflow by other means \u{2014} it is reserved for explicit user invocation."
        );
    }
    let variables = SkillVariables {
        project_dir: &config.cwd,
        session_id: &config.session_id,
    };
    let Some(text) =
        crate::skills::skill_invocation_text(skill, args, UnusedArguments::Append, &variables)
            .await
    else {
        anyhow::bail!(
            "Skill {name} could not be read from {}",
            skill.file_path.display()
        );
    };
    Ok(json!({
        "name": skill.name,
        "text": text,
        "location": skill.file_path.display().to_string(),
        "baseDir": skill.base_dir.display().to_string(),
    }))
}

/// Register `skill.load`.
pub(crate) fn register_host_handlers(handlers: &mut HostRequestHandlers, config: SkillHostConfig) {
    let config = Arc::new(config);
    handlers.register(
        SKILL_LOAD_REQUEST,
        host_handler(move |payload| {
            let config = Arc::clone(&config);
            async move { load_skill(&config, &payload.data).await }
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::claude_commands::ShellExpansion;
    use crate::skills::claude_skills::{ClaudeSkillRoots, load_claude_skills};

    fn config(root: &std::path::Path) -> SkillHostConfig {
        let skills = load_claude_skills(&ClaudeSkillRoots {
            user_dir: None,
            project_dirs: vec![root.join(".claude").join("skills")],
            shell: ShellExpansion::Disabled,
        })
        .skills;
        SkillHostConfig {
            skills,
            dynamic: None,
            cwd: root.to_path_buf(),
            session_id: "sess-1".to_string(),
        }
    }

    fn write(root: &std::path::Path, name: &str, text: &str) -> PathBuf {
        let dir = root.join(".claude").join("skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), text).unwrap();
        dir
    }

    #[tokio::test]
    async fn loads_a_claude_skill_with_its_substitutions() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = write(
            tmp.path(),
            "release",
            "---\ndescription: Cut a release\narguments: [version]\n---\nRelease $version with ${CLAUDE_SKILL_DIR}/notes.md in ${CLAUDE_PROJECT_DIR} (${CLAUDE_SESSION_ID}).",
        );
        let reply = load_skill(
            &config(tmp.path()),
            &json!({ "name": "release", "args": "1.2.0" }),
        )
        .await
        .unwrap();
        assert_eq!(
            reply,
            json!({
                "name": "release",
                "text": format!(
                    "Base directory for this skill: {0}\n\nRelease 1.2.0 with {0}/notes.md in {1} (sess-1).",
                    dir.display(),
                    tmp.path().display()
                ),
                "location": dir.join("SKILL.md").display().to_string(),
                "baseDir": dir.display().to_string(),
            })
        );
    }

    #[tokio::test]
    async fn unconsumed_arguments_are_appended() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = write(tmp.path(), "plain", "Just do it.");
        let reply = load_skill(
            &config(tmp.path()),
            &json!({ "name": "/plain", "args": "now" }),
        )
        .await
        .unwrap();
        assert_eq!(
            reply["text"],
            format!(
                "Base directory for this skill: {}\n\nJust do it.\n\nARGUMENTS: now",
                dir.display()
            )
        );
    }

    #[tokio::test]
    async fn refuses_user_only_and_unknown_skills() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "deploy",
            "---\ndescription: Deploy\ndisable-model-invocation: true\n---\nShip it.",
        );
        write(tmp.path(), "lint", "---\ndescription: Lint\n---\nLint.");
        let config = config(tmp.path());
        let error = load_skill(&config, &json!({ "name": "deploy" }))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.starts_with(
                "Skill deploy cannot be loaded by the model due to disable-model-invocation."
            ),
            "{error}"
        );
        let error = load_skill(&config, &json!({ "name": "missing" }))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(error, "Unknown skill: missing. Available skills: lint");
        assert!(load_skill(&config, &json!({ "name": 3 })).await.is_err());
    }
}
