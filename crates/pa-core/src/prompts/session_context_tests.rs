use std::path::PathBuf;

use super::*;
use crate::skills::{SkillKind, SourceScope, create_synthetic_source_info};

fn skill(name: &str, description: &str) -> Skill {
    Skill {
        name: name.to_string(),
        description: description.to_string(),
        file_path: PathBuf::from(format!("/skills/{name}/SKILL.md")),
        base_dir: PathBuf::from(format!("/skills/{name}")),
        source_info: create_synthetic_source_info("/skills", "user", SourceScope::User, None),
        disable_model_invocation: false,
        kind: SkillKind::Markdown,
        python: None,
    }
}

fn environment(cwd: &str) -> EnvironmentFields {
    EnvironmentFields {
        working_directory: cwd.to_string(),
        additional_directories: Vec::new(),
        is_git_repo: Some(true),
        git_branch: Some("main".to_string()),
        platform: "linux".to_string(),
        shell: Some("/bin/bash".to_string()),
        os_version: Some("Linux 6.9".to_string()),
        model: Some("anthropic/claude".to_string()),
        conversation_log: Some("/log.jsonl".to_string()),
        vision_capable: Some(true),
        sandbox: None,
        scratch_directory: None,
    }
}

fn context(cwd: &str, skills: &[Skill], files: &[(&str, &str)], date: &str) -> SessionContext {
    SessionContext::build(SessionContextInputs {
        agent_types: None,
        instruction_files: files
            .iter()
            .map(|(path, content)| InstructionFile {
                path: (*path).to_string(),
                content: (*content).to_string(),
            })
            .collect(),
        skills,
        selected_tools: None,
        mcp_servers: &[
            "slack".to_string(),
            "github".to_string(),
            "slack".to_string(),
        ],
        environment: environment(cwd),
        date: date.to_string(),
    })
}

#[test]
fn the_full_row_carries_every_section_in_the_old_tail_order() {
    let skills = [skill("alpha", "Alpha skill")];
    let text = context(
        "/w",
        &skills,
        &[("/w/AGENTS.md", "Rule one.")],
        "2026-10-17",
    )
    .render_full();
    assert!(text.starts_with("<system-reminder>\n# Project Context"));
    assert!(text.ends_with(&format!("{AMBIENT_CONTEXT_NOTE}\n</system-reminder>")));
    let order = [
        "## /w/AGENTS.md\n\nRule one.",
        "<available_skills>",
        "<name>alpha</name>",
        "Enabled generic MCP servers: `github`, `slack`.",
        "# Environment\nWorking directory: /w\nIs a git repository: true\nGit branch: main\nPlatform: linux\nShell: /bin/bash\nOS version: Linux 6.9\nModel: anthropic/claude\nConversation log: /log.jsonl\nImage input: this model can see images",
        "Today's date is 2026-10-17.",
    ];
    let mut at = 0;
    for needle in order {
        let found = text[at..]
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} missing or out of order in {text}"));
        at += found + needle.len();
    }
}

#[test]
fn an_unchanged_context_has_no_delta() {
    let skills = [skill("alpha", "Alpha skill")];
    let first = context(
        "/w",
        &skills,
        &[("/w/AGENTS.md", "Rule one.")],
        "2026-10-17",
    );
    let fingerprint = first.fingerprint();
    // Round-trips through the row's details unchanged.
    let restored: ContextFingerprint =
        serde_json::from_value(serde_json::to_value(&fingerprint).unwrap()).unwrap();
    assert_eq!(restored, fingerprint);
    assert_eq!(first.render_delta(&restored), None);
}

#[test]
fn the_fingerprint_holds_hashes_not_contents() {
    let first = context(
        "/w",
        &[],
        &[("/w/AGENTS.md", "SECRET RULE TEXT")],
        "2026-10-17",
    );
    let json = serde_json::to_string(&first.fingerprint()).unwrap();
    assert!(!json.contains("SECRET RULE TEXT"));
    assert!(json.contains("/w/AGENTS.md"));
}

#[test]
fn the_delta_names_only_what_changed() {
    let skills = [skill("alpha", "Alpha skill"), skill("beta", "Beta skill")];
    let first = context(
        "/w",
        &skills,
        &[
            ("/w/AGENTS.md", "Rule one."),
            ("/w/sub/AGENTS.md", "Sub rule."),
        ],
        "2026-10-17",
    );
    let mut next_env = environment("/w2");
    next_env.git_branch = Some("feature".to_string());
    let next = SessionContext::build(SessionContextInputs {
        agent_types: None,
        instruction_files: vec![InstructionFile {
            path: "/w/AGENTS.md".to_string(),
            content: "Rule one, amended.".to_string(),
        }],
        skills: &[skill("alpha", "Alpha skill"), skill("gamma", "Gamma skill")],
        selected_tools: None,
        mcp_servers: &["github".to_string(), "slack".to_string()],
        environment: next_env,
        date: "2026-10-18".to_string(),
    });
    let delta = next.render_delta(&first.fingerprint()).expect("a delta");
    assert_eq!(
        delta,
        format!(
            "<system-reminder>\n\
             These instruction files changed \u{2014} each replaces its earlier copy, and the project context above still applies otherwise:\n\n\
             ## /w/AGENTS.md\n\nRule one, amended.\n\n\
             Instructions no longer present: /w/sub/AGENTS.md\n\n\
             New or changed skills are now available (each replaces any earlier entry of the same name):\n\
             <available_skills>\n{}\n</available_skills>\n\n\
             These skills are no longer available; do not use them: `beta`.\n\n\
             # Environment update\n\
             Working directory: /w2 (was /w)\n\
             Git branch: feature (was main)\n\n\
             The date has changed. Today's date is now 2026-10-18. No need to announce the new date \u{2014} the user's own clock shows it.\n\n\
             {AMBIENT_CONTEXT_NOTE}\n</system-reminder>",
            crate::skills::skill_inventory_entry(&skill("gamma", "Gamma skill"))
        )
    );
}

#[test]
fn a_date_only_change_is_the_date_line() {
    let first = context("/w", &[], &[], "2026-10-17");
    let next = context("/w", &[], &[], "2026-10-18");
    let delta = next.render_delta(&first.fingerprint()).unwrap();
    assert_eq!(
        delta,
        format!(
            "<system-reminder>\n{}\n\n{AMBIENT_CONTEXT_NOTE}\n</system-reminder>",
            render_date_change("2026-10-18")
        )
    );
}

#[test]
fn environment_update_covers_each_field_and_scratch_seam() {
    let previous = environment("/w");
    let mut next = previous.clone();
    next.is_git_repo = Some(false);
    next.git_branch = None;
    next.model = Some("openai/gpt".to_string());
    next.vision_capable = Some(false);
    next.sandbox = Some("OS sandbox: `read-only`.".to_string());
    next.scratch_directory = Some("/tmp/scratch".to_string());
    let update = render_environment_update(&previous, &next);
    assert_eq!(
        update,
        "# Environment update\n\
         Is a git repository: false\n\
         Git branch: none (was main)\n\
         Model: openai/gpt (was anthropic/claude)\n\
         Image input: this model cannot see images; `attach_image` has the configured vision-capable `imageModel` read them and returns its text description (it errors when no such image model is configured).\n\
         OS sandbox: `read-only`.\n\
         Scratchpad directory: /tmp/scratch \u{2014} always use it for temporary files \
         (intermediate results, scripts, outputs that don't belong in the project) instead of \
         `/tmp` or other system temp directories; it is session-specific, isolated from the \
         project, and private to you (`os.environ[\"PRIME_AGENT_SCRATCHPAD\"]` in the kernel). \
         Only use `/tmp` if the user explicitly asks."
    );
    let mut gone = next.clone();
    gone.scratch_directory = None;
    gone.sandbox = None;
    assert_eq!(
        render_environment_update(&next, &gone),
        "# Environment update\nOS sandbox: off.\nThe scratch directory announced earlier is no longer available; use it no further."
    );
    assert_eq!(render_environment_update(&previous, &previous), "");
}

#[test]
fn additional_directories_are_listed_and_their_changes_are_updates() {
    let mut previous = environment("/w");
    previous.additional_directories = vec!["/a".to_string()];
    assert!(
        render_environment(&previous)
            .contains("Working directory: /w\nAdditional working directories:\n - /a\n")
    );
    let mut next = previous.clone();
    next.additional_directories = vec!["/b".to_string()];
    assert_eq!(
        render_environment_update(&previous, &next),
        "# Environment update\n\
         Additional working directories added:\n - /b\n\
         Additional working directories no longer available:\n - /a"
    );
}

#[test]
fn the_agent_types_ride_the_row_and_a_change_replaces_them() {
    let listing = "Available agent types for `rlm.spawn(agent=...)`:\n- explore: Searches";
    let with = |agent_types: Option<&str>| {
        SessionContext::build(SessionContextInputs {
            agent_types: agent_types.map(str::to_string),
            environment: environment("/w"),
            date: "2026-10-17".to_string(),
            ..SessionContextInputs::default()
        })
    };
    let first = with(Some(listing));
    assert!(first.render_full().contains(listing));
    assert_eq!(first.render_delta(&first.fingerprint()), None);
    let changed = with(Some(
        "Available agent types for `rlm.spawn(agent=...)`:\n- review: Reviews",
    ));
    assert!(
        changed
            .render_delta(&first.fingerprint())
            .unwrap()
            .contains("The available agent types changed; this replaces the earlier list.")
    );
    assert!(
        with(None)
            .render_delta(&first.fingerprint())
            .unwrap()
            .contains("no longer available to `rlm.spawn(agent=...)`")
    );
}

#[test]
fn tools_gate_the_skills_and_mcp_sections() {
    let skills = [skill("alpha", "Alpha skill")];
    let without_files = SessionContext::build(SessionContextInputs {
        agent_types: None,
        skills: &skills,
        selected_tools: Some(&["echo"]),
        mcp_servers: &["slack".to_string()],
        environment: environment("/w"),
        date: "2026-10-17".to_string(),
        ..Default::default()
    });
    assert!(without_files.skills.is_empty());
    assert!(without_files.mcp_servers.is_empty());
    let bash_only = SessionContext::build(SessionContextInputs {
        agent_types: None,
        skills: &skills,
        selected_tools: Some(&["bash"]),
        mcp_servers: &["slack".to_string()],
        environment: environment("/w"),
        date: "2026-10-17".to_string(),
        ..Default::default()
    });
    assert_eq!(bash_only.skills.len(), 1);
    assert!(bash_only.mcp_servers.is_empty());
}

#[test]
fn hidden_skills_never_list() {
    let mut hidden = skill("hidden", "Hidden");
    hidden.disable_model_invocation = true;
    assert!(skill_entries(&[hidden]).is_empty());
}

#[test]
fn the_skills_inventory_matches_the_prompt_formatter() {
    let skills = [skill("alpha", "Alpha <skill>"), skill("beta", "Beta")];
    assert_eq!(
        render_skills(&skill_entries(&skills)),
        crate::skills::format_skills_for_prompt(&skills).trim()
    );
}

#[test]
fn civil_dates() {
    assert_eq!(civil_date_from_days(0), "1970-01-01");
    assert_eq!(civil_date_from_days(20_743), "2026-10-17");
    assert_eq!(civil_date_from_days(11_016), "2000-02-29");
}
