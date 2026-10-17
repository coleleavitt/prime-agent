//! Golden snapshot of the assembled layered system prompt and of the session context row the first
//! turn carries. The prompt no longer pins the TS text (the layered redesign supersedes TS-prompt
//! parity); the golden pins the Rust prompt itself so any layer edit is a visible, reviewed change.
//! Regenerate with `PA_UPDATE_GOLDEN=1 cargo test`.

use std::path::Path;

use pa_core::prompts::session_context::{
    EnvironmentFields,
    InstructionFile,
    SessionContext,
    SessionContextInputs,
};
use pa_core::prompts::system_prompt::{BuildSystemPromptOptions, build_system_prompt};
use pa_core::skills::load_skills_from_dir;

const GOLDEN: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/golden/corpus/system-prompt.json"
);

/// The same fixture with the OS sandbox enabled: the one environment line it adds to the session
/// context. The system prompt is byte-identical to the default golden's.
const GOLDEN_SANDBOX: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/golden/corpus/system-prompt-sandbox.json"
);

/// The `workspace-write` line `SessionSandbox::prompt_line` renders on a Landlock ABI 6 kernel
/// (pinned here so the golden does not depend on the machine's kernel).
const SANDBOX_LINE: &str = "OS sandbox: `workspace-write` (Landlock ABI 6 + seccomp): the kernel \
    and every command it runs can write only inside the working directory, the temp directories \
    and the configured writable roots; other writes fail with a permission error (EACCES). \
    Network access is blocked (only pipes and unix sockets work).";

#[derive(serde::Serialize, serde::Deserialize)]
struct GoldenCorpus {
    fixture: serde_json::Value,
    skill_count: usize,
    #[serde(rename = "systemPrompt")]
    system_prompt: String,
    /// The first turn's hidden `session_context` row.
    #[serde(rename = "sessionContext")]
    session_context: String,
}

/// The workspace bundled skills directory (source-checkout layout):
/// pa-core lives at `<root>/crates/pa-core`.
fn bundled_skills_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .join("skills")
}

/// Fixture-state prompt and session context with the per-run values normalized: the skills
/// directory path and raw readdir skill order (both sides sort); the date and the environment are
/// fixture values.
fn fixture(sandbox: Option<&str>) -> (String, String, usize) {
    let skills_dir = bundled_skills_dir();
    let mut loaded = load_skills_from_dir(&skills_dir, "package");
    // Skill enumeration is directory-order; pin content, not order.
    loaded
        .skills
        .sort_by(|left, right| left.name.cmp(&right.name));
    let count = loaded.skills.len();
    let prompt = build_system_prompt(&BuildSystemPromptOptions {
        selected_tools: Some(vec![pa_types::ai::PYTHON_TOOL_NAME]),
        allow_recursion: Some(true),
        rlm_depth: Some(0),
        ..Default::default()
    });
    let context = SessionContext::build(SessionContextInputs {
        agent_types: None,
        instruction_files: vec![InstructionFile {
            path: "/w/AGENTS.md".to_string(),
            content: "Fixture project rules.".to_string(),
        }],
        skills: &loaded.skills,
        selected_tools: Some(&[pa_types::ai::PYTHON_TOOL_NAME]),
        mcp_servers: &[],
        environment: EnvironmentFields {
            working_directory: "/w".to_string(),
            additional_directories: Vec::new(),
            is_git_repo: Some(true),
            git_branch: Some("main".to_string()),
            platform: "linux".to_string(),
            shell: Some("/bin/bash".to_string()),
            os_version: Some("Linux 6.0.0".to_string()),
            model: Some("provider/fixture-model".to_string()),
            conversation_log: Some("/w/sessions/fixture-session.jsonl".to_string()),
            vision_capable: None,
            sandbox: sandbox.map(str::to_string),
            scratch_directory: None,
        },
        date: "2026-01-01".to_string(),
    })
    .render_full()
    .replace(skills_dir.to_string_lossy().as_ref(), "<skills-dir>");
    (prompt, context, count)
}

/// Compare the fixture against `golden` (or rewrite it under `PA_UPDATE_GOLDEN=1`).
fn check_golden(golden_path: &str, sandbox: Option<&str>) {
    let (prompt, session_context, skill_count) = fixture(sandbox);
    if std::env::var("PA_UPDATE_GOLDEN").as_deref() == Ok("1") {
        let mut fixture = serde_json::json!({
            "cwd": "/w",
            "messagesPath": "/w/sessions/fixture-session.jsonl",
            "selectedTools": [pa_types::ai::PYTHON_TOOL_NAME],
            "skillsSource": "bundled skills directory (<skills-dir>)",
            "date": "2026-01-01",
        });
        if let Some(sandbox) = sandbox {
            fixture["sandbox"] = serde_json::json!(sandbox);
        }
        let corpus = GoldenCorpus {
            fixture,
            skill_count,
            system_prompt: prompt,
            session_context,
        };
        std::fs::write(
            golden_path,
            serde_json::to_string_pretty(&corpus).expect("serialize golden") + "\n",
        )
        .expect("write golden");
        return;
    }
    let raw = std::fs::read_to_string(golden_path).expect("golden corpus");
    let golden: GoldenCorpus = serde_json::from_str(&raw).expect("golden corpus json");
    assert_eq!(
        skill_count, golden.skill_count,
        "bundled skill count changed; re-run with PA_UPDATE_GOLDEN=1 after updating the prompt layers"
    );
    assert_eq!(
        prompt, golden.system_prompt,
        "assembled prompt changed; if intended, re-run with PA_UPDATE_GOLDEN=1 and review the diff"
    );
    assert_eq!(
        session_context, golden.session_context,
        "session context row changed; if intended, re-run with PA_UPDATE_GOLDEN=1 and review the diff"
    );
}

#[test]
fn system_prompt_matches_golden_snapshot() {
    check_golden(GOLDEN, None);
}

#[test]
fn sandboxed_system_prompt_matches_golden_snapshot() {
    check_golden(GOLDEN_SANDBOX, Some(SANDBOX_LINE));
}

/// Cache safety: the sandbox (like every other environment fact) changes only the session context
/// row; the system prompt the provider caches is the same bytes.
#[test]
fn the_sandbox_leaves_the_system_prompt_untouched() {
    let (plain, plain_context, _) = fixture(None);
    let (sandboxed, sandboxed_context, _) = fixture(Some(SANDBOX_LINE));
    assert_eq!(plain, sandboxed);
    assert_ne!(plain_context, sandboxed_context);
}
