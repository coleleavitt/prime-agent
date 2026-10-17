use std::path::{Path, PathBuf};

use super::*;

fn write_skill(skills_dir: &Path, name: &str, text: &str) -> PathBuf {
    let dir = skills_dir.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(SKILL_FILE), text).unwrap();
    dir
}

fn names(skills: &[Skill]) -> Vec<&str> {
    skills.iter().map(|skill| skill.name.as_str()).collect()
}

fn variables(dir: &Path) -> SkillVariables<'_> {
    SkillVariables {
        project_dir: dir,
        session_id: "sess-9",
    }
}

#[test]
fn the_project_walk_stops_at_the_repository_root_and_before_home() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let repo = home.join("work").join("repo");
    let cwd = repo.join("pkg").join("app");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    for dir in [&cwd, &repo, &home.join("work"), &home] {
        std::fs::create_dir_all(dir.join(".claude").join("skills")).unwrap();
    }
    // `pkg` has no skills directory: only existing ones are listed.
    assert_eq!(
        project_claude_dirs(&cwd, CLAUDE_SKILLS_DIR, Some(&home)),
        vec![
            cwd.join(".claude").join("skills"),
            repo.join(".claude").join("skills"),
        ]
    );
    // Outside a repository the walk goes up to (not into) the home directory.
    std::fs::remove_dir_all(repo.join(".git")).unwrap();
    assert_eq!(
        project_claude_dirs(&cwd, CLAUDE_SKILLS_DIR, Some(&home)),
        vec![
            cwd.join(".claude").join("skills"),
            repo.join(".claude").join("skills"),
            home.join("work").join(".claude").join("skills"),
        ]
    );
}

#[test]
fn loads_a_skill_directory_with_claude_codes_frontmatter() {
    let tmp = tempfile::tempdir().unwrap();
    let skills_dir = tmp.path().join("skills");
    let dir = write_skill(
        &skills_dir,
        "Release_Notes",
        "---\nname: Release notes\ndescription: Write release notes\nwhen_to_use: after a tag\nargument-hint: <version>\narguments: [version]\nallowed-tools: Bash(git log:*), Read\nmodel: sonnet\ndisable-model-invocation: \"true\"\nuser-invocable: false\n---\nNotes for $version.",
    );
    write_skill(
        &skills_dir,
        "untitled",
        "# Untitled helper\n\nDo the thing.",
    );
    write_skill(&skills_dir, "empty", "");
    // A file at the top level and a directory without SKILL.md are no skills.
    std::fs::write(skills_dir.join("loose.md"), "---\ndescription: x\n---\n").unwrap();
    std::fs::create_dir_all(skills_dir.join("no-skill-file")).unwrap();
    let load = load_claude_skills(&ClaudeSkillRoots {
        user_dir: None,
        project_dirs: vec![skills_dir],
        shell: ShellExpansion::Disabled,
    });
    assert_eq!(load.diagnostics, Vec::new());
    assert_eq!(
        names(&load.skills),
        vec!["Release_Notes", "empty", "untitled"]
    );
    let release = &load.skills[0];
    assert_eq!(
        (
            release.description.as_str(),
            release.when_to_use.as_deref(),
            release.disable_model_invocation,
            release.user_invocable,
            release.kind,
            release.base_dir.clone(),
            release.source_info.scope,
            release.source_info.source.as_str(),
        ),
        (
            "Write release notes",
            Some("after a tag"),
            true,
            false,
            SkillKind::Markdown,
            dir,
            SourceScope::Project,
            "claude",
        )
    );
    assert_eq!(
        release.claude,
        Some(ClaudeSkillOptions {
            argument_names: vec!["version".to_string()],
            argument_hint: Some("<version>".to_string()),
            allowed_tools: vec!["Bash(git log:*)".to_string(), "Read".to_string()],
            model: Some("sonnet".to_string()),
            shell: ShellExpansion::Disabled,
            bare_command: true,
        })
    );
    // Claude Code's description fallbacks: the first line's heading text, else "Skill".
    assert_eq!(load.skills[2].description, "Untitled helper");
    assert_eq!(load.skills[1].description, "Skill");
    assert!(load.skills[1].user_invocable && !load.skills[1].disable_model_invocation);
}

#[test]
fn the_user_directory_wins_and_a_later_copy_reports_a_collision() {
    let tmp = tempfile::tempdir().unwrap();
    let user = tmp.path().join("user-skills");
    let project = tmp.path().join("project-skills");
    let winner = write_skill(&user, "deploy", "---\ndescription: user deploy\n---\nUser.");
    let loser = write_skill(
        &project,
        "deploy",
        "---\ndescription: project deploy\n---\nProject.",
    );
    write_skill(&project, "lint", "---\ndescription: lint\n---\nLint.");
    let load = load_claude_skills(&ClaudeSkillRoots {
        user_dir: Some(user),
        project_dirs: vec![project],
        shell: ShellExpansion::Enabled,
    });
    assert_eq!(names(&load.skills), vec!["deploy", "lint"]);
    assert_eq!(load.skills[0].source_info.scope, SourceScope::User);
    assert_eq!(
        load.diagnostics,
        vec![ResourceDiagnostic::Collision {
            message: "name \"deploy\" collision".to_string(),
            path: loser.join(SKILL_FILE).display().to_string(),
            collision: crate::skills::ResourceCollision {
                resource_type: "skill",
                name: "deploy".to_string(),
                winner_path: winner.join(SKILL_FILE).display().to_string(),
                loser_path: loser.join(SKILL_FILE).display().to_string(),
            },
        }]
    );
}

#[test]
fn a_prime_agent_skill_of_the_same_name_wins() {
    let tmp = tempfile::tempdir().unwrap();
    let native_dir = tmp.path().join("native");
    write_skill(
        &native_dir,
        "tasks",
        "---\nname: tasks\ndescription: native tasks\n---\nNative.",
    );
    let native = crate::skills::load_skills_from_dir(&native_dir, "user").skills;
    let claude_dir = tmp.path().join("claude");
    write_skill(
        &claude_dir,
        "tasks",
        "---\ndescription: claude tasks\n---\nClaude.",
    );
    write_skill(
        &claude_dir,
        "review",
        "---\ndescription: review\n---\nReview.",
    );
    let claude = load_claude_skills(&ClaudeSkillRoots {
        user_dir: None,
        project_dirs: vec![claude_dir],
        shell: ShellExpansion::Enabled,
    });
    let mut skills = native;
    let mut diagnostics = Vec::new();
    merge_claude_skills(&mut skills, &mut diagnostics, claude);
    assert_eq!(names(&skills), vec!["tasks", "review"]);
    assert!(skills[0].claude.is_none());
    assert!(matches!(
        diagnostics.as_slice(),
        [ResourceDiagnostic::Collision { collision, .. }]
            if collision.name == "tasks" && collision.winner_path.contains("native")
    ));
}

#[test]
fn builtin_command_and_template_names_keep_their_command() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("skills");
    for name in ["compact", "model", "mytemplate", "deploy"] {
        write_skill(&dir, name, "---\ndescription: d\n---\nbody");
    }
    let mut skills = load_claude_skills(&ClaudeSkillRoots {
        user_dir: None,
        project_dirs: vec![dir],
        shell: ShellExpansion::Disabled,
    })
    .skills;
    assign_bare_commands(&mut skills, &["mytemplate"]);
    let bare: Vec<(&str, bool)> = skills
        .iter()
        .map(|skill| {
            (
                skill.name.as_str(),
                skill.claude.as_ref().unwrap().bare_command,
            )
        })
        .collect();
    assert_eq!(
        bare,
        vec![
            ("compact", false),
            ("deploy", true),
            ("model", false),
            ("mytemplate", false),
        ]
    );
    assert_eq!(
        crate::skills::skill_command_names(&skills[1]),
        vec!["skill:deploy".to_string(), "deploy".to_string()]
    );
    assert_eq!(
        crate::skills::skill_command_names(&skills[0]),
        vec!["skill:compact".to_string()]
    );
}

#[tokio::test]
async fn the_invocation_text_substitutes_arguments_and_variables() {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().join("skill");
    let options = ClaudeSkillOptions {
        argument_names: vec!["target".to_string()],
        shell: ShellExpansion::Disabled,
        ..ClaudeSkillOptions::default()
    };
    let body = "Deploy $target ($0) from ${CLAUDE_SKILL_DIR}/run.sh in ${CLAUDE_PROJECT_DIR}, session ${CLAUDE_SESSION_ID}. Rest: $ARGUMENTS. Status: !`echo hi`";
    let text = render_claude_skill(
        &options,
        &base,
        body,
        "prod --fast",
        UnusedArguments::Omit,
        &variables(tmp.path()),
    )
    .await;
    assert_eq!(
        text,
        format!(
            "Base directory for this skill: {0}\n\nDeploy prod (prod) from {0}/run.sh in {1}, session sess-9. Rest: prod --fast. Status: !`echo hi`",
            base.display(),
            tmp.path().display()
        )
    );
}

#[tokio::test]
async fn shell_blocks_run_in_the_project_directory_when_enabled() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("marker.txt"), "").unwrap();
    let options = ClaudeSkillOptions {
        shell: ShellExpansion::Enabled,
        ..ClaudeSkillOptions::default()
    };
    let text = render_claude_skill(
        &options,
        tmp.path(),
        "Files: !`ls`",
        "",
        UnusedArguments::Append,
        &variables(tmp.path()),
    )
    .await;
    assert_eq!(
        text,
        format!(
            "Base directory for this skill: {}\n\nFiles: marker.txt",
            tmp.path().display()
        )
    );
}

#[tokio::test]
async fn a_user_command_expands_a_claude_skill_by_its_bare_name() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("skills");
    let deploy = write_skill(
        &dir,
        "deploy",
        "---\ndescription: d\n---\nShip $ARGUMENTS now.",
    );
    write_skill(
        &dir,
        "hidden",
        "---\ndescription: h\nuser-invocable: false\n---\nModel only.",
    );
    let mut skills = load_claude_skills(&ClaudeSkillRoots {
        user_dir: None,
        project_dirs: vec![dir],
        shell: ShellExpansion::Disabled,
    })
    .skills;
    assign_bare_commands(&mut skills, &[]);
    let expected = format!(
        "<skill name=\"deploy\" location=\"{0}/SKILL.md\">\nBase directory for this skill: {0}\n\nShip api now.\n</skill>\n\napi",
        deploy.display()
    );
    for text in ["/deploy api", "/skill:deploy api"] {
        let (expanded, used) =
            crate::skills::expand_skill_command(text, &skills, &variables(tmp.path())).await;
        assert_eq!(expanded, expected, "{text}");
        assert_eq!(used.map(|skill| skill.name.as_str()), Some("deploy"));
        let parsed = pa_types::skill_blocks::parse_skill_block(&expanded).expect("a block");
        assert_eq!(parsed.user_message.as_deref(), Some("api"));
    }
    // Not user-invocable: neither command expands.
    for text in ["/hidden", "/skill:hidden"] {
        let (expanded, used) =
            crate::skills::expand_skill_command(text, &skills, &variables(tmp.path())).await;
        assert_eq!((expanded.as_str(), used.is_none()), (text, true));
    }
    assert!(crate::skills::skill_command_names(&skills[1]).is_empty());
}

#[test]
fn the_inventory_lists_when_to_use_and_the_load_call() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("skills");
    write_skill(
        &dir,
        "deploy",
        "---\ndescription: Deploy the app\nwhen_to_use: when the user asks to ship\n---\nbody",
    );
    let skills = load_claude_skills(&ClaudeSkillRoots {
        user_dir: None,
        project_dirs: vec![dir.clone()],
        shell: ShellExpansion::Disabled,
    })
    .skills;
    assert_eq!(
        crate::skills::skill_inventory_entry(&skills[0]),
        format!(
            "  <skill>\n    <name>deploy</name>\n    <type>markdown</type>\n    <description>Deploy the app - when the user asks to ship</description>\n    <location>{}</location>\n    <load>await rlm.load_skill(\"deploy\", args=\"\")</load>\n  </skill>",
            dir.join("deploy").join(SKILL_FILE).display()
        )
    );
}
