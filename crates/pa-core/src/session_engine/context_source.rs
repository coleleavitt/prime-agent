//! Where a live session's context comes from: the environment probed now,
//! the instruction files read now, and the session's skills, tools and MCP
//! servers ([`super::context_rows`] compares the result with what the
//! conversation already announced).
//!
//! The instruction-file loader is one function ([`instruction_files`]); the
//! rendering of the files is [`crate::prompts::session_context`]'s. A
//! loader change (imports, local files, rules directories) changes only the
//! former.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::context_rows::SessionContextSource;
use crate::prompts::session_context::{
    EnvironmentFields,
    InstructionFile,
    SessionContext,
    SessionContextInputs,
    today_utc,
};
use crate::skills::Skill;

/// The session facts the context source reads.
pub struct ContextSourceConfig {
    /// The session cwd; `/cwd` moves it ([`super::context_rows::SessionContextRows::set_cwd`]).
    pub cwd: Arc<Mutex<PathBuf>>,
    pub agent_dir: PathBuf,
    /// `--no-context-files`: no instruction files at all.
    pub load_instruction_files: bool,
    /// The skills the model may see.
    pub skills: Vec<Skill>,
    /// The session's active tool names.
    pub tools: Vec<String>,
    pub mcp_servers: Vec<String>,
    pub conversation_log: Option<String>,
    pub vision_capable: Option<bool>,
    /// The OS sandbox's environment line.
    pub sandbox: Option<String>,
    /// The shell `bash()` runs commands with.
    pub shell: Option<String>,
    /// A per-session scratch directory, when the embedding provides one.
    pub scratch_directory: Option<String>,
}

/// The instruction files for a working directory (the global agent-dir file
/// first, then the ancestor chain, root-most first).
#[must_use]
pub fn instruction_files(cwd: &Path, agent_dir: &Path) -> Vec<InstructionFile> {
    crate::resources::load_project_context_files(cwd, agent_dir)
        .into_iter()
        .map(|file| InstructionFile {
            path: file.path.display().to_string(),
            content: file.content,
        })
        .collect()
}

/// The platform name the environment states.
#[must_use]
pub fn platform_name() -> &'static str {
    std::env::consts::OS
}

/// The kernel release, where the OS reports it without spawning anything.
#[must_use]
pub fn os_version() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").ok()?;
        let release = release.trim();
        (!release.is_empty()).then(|| format!("Linux {release}"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

fn forward_slashes(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

/// The environment fields for `cwd` now (git probes spawn `git`: blocking).
#[must_use]
pub fn probe_environment(cwd: &Path, config: &ContextSourceConfig) -> EnvironmentFields {
    let git = crate::session::manager::capture_git_context(cwd);
    EnvironmentFields {
        working_directory: forward_slashes(cwd),
        is_git_repo: Some(git.is_some()),
        git_branch: git.and_then(|git| git.branch),
        platform: platform_name().to_string(),
        shell: config.shell.clone(),
        os_version: os_version(),
        conversation_log: config
            .conversation_log
            .as_deref()
            .map(|log| log.replace('\\', "/")),
        vision_capable: config.vision_capable,
        sandbox: config.sandbox.clone(),
        scratch_directory: config.scratch_directory.clone(),
    }
}

/// What the engine hands over to build a session's context rows.
pub struct ContextRowsWiring {
    pub cwd: PathBuf,
    pub agent_dir: PathBuf,
    pub load_instruction_files: bool,
    pub skills: Vec<Skill>,
    pub tools: Vec<String>,
    pub mcp_servers: Vec<String>,
    pub conversation_log: Option<String>,
    pub vision_capable: Option<bool>,
    pub sandbox: Option<String>,
    pub shell: Option<String>,
    pub reminders: super::reminders::ReminderToggles,
    /// The `tasks` skill's store, when the session keeps a directory.
    pub tasks_file: Option<PathBuf>,
    /// The context window the token countdown counts down from.
    pub context_window: u64,
}

/// The session's context rows: the live source over a movable cwd, and the
/// reminders with a fresh file tracker.
#[must_use]
pub fn session_context_rows(
    wiring: ContextRowsWiring,
) -> Arc<super::context_rows::SessionContextRows> {
    let has_tasks = wiring
        .tools
        .iter()
        .any(|tool| tool == pa_types::ai::PYTHON_TOOL_NAME)
        && wiring
            .skills
            .iter()
            .any(|skill| skill.name == TASKS_SKILL_NAME && !skill.disable_model_invocation);
    let cwd = Arc::new(Mutex::new(wiring.cwd));
    let source = context_source(ContextSourceConfig {
        cwd: Arc::clone(&cwd),
        agent_dir: wiring.agent_dir,
        load_instruction_files: wiring.load_instruction_files,
        skills: wiring.skills,
        tools: wiring.tools,
        mcp_servers: wiring.mcp_servers,
        conversation_log: wiring.conversation_log,
        vision_capable: wiring.vision_capable,
        sandbox: wiring.sandbox,
        shell: wiring.shell,
        scratch_directory: None,
    });
    let reminders = super::reminders::Reminders::new(
        wiring.reminders,
        Arc::new(super::file_tracker::FileReadTracker::new()),
        has_tasks,
        wiring.tasks_file,
        wiring.context_window,
    );
    Arc::new(super::context_rows::SessionContextRows::new(source, reminders).with_cwd(cwd))
}

/// The bundled skill whose store the task-list reminder reads.
pub const TASKS_SKILL_NAME: &str = "tasks";

/// The live session's context source.
#[must_use]
pub fn context_source(config: ContextSourceConfig) -> SessionContextSource {
    Arc::new(move || {
        let cwd = config
            .cwd
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let tools: Vec<&str> = config.tools.iter().map(String::as_str).collect();
        SessionContext::build(SessionContextInputs {
            instruction_files: if config.load_instruction_files {
                instruction_files(&cwd, &config.agent_dir)
            } else {
                Vec::new()
            },
            skills: &config.skills,
            selected_tools: Some(&tools),
            mcp_servers: &config.mcp_servers,
            environment: probe_environment(&cwd, &config),
            date: today_utc(),
        })
    })
}
