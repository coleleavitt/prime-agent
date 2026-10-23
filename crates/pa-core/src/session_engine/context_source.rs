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
    /// The session model; a live switch moves it
    /// ([`super::context_rows::SessionContextRows::set_model`]).
    pub model: Arc<Mutex<ModelFacts>>,
    /// The OS sandbox's environment line.
    pub sandbox: Option<String>,
    /// The shell `bash()` runs commands with.
    pub shell: Option<String>,
    /// A per-session scratch directory, when the embedding provides one.
    pub scratch_directory: Option<String>,
    /// How instruction files load beyond the working directory (the user
    /// scope opt-in, the home, the additional directories' opt-in); the
    /// working directory itself is the session's current one.
    pub instruction_options: crate::resources::instructions::InstructionOptions,
    /// The session's additional working directories (live: `/add-dir`
    /// moves them).
    pub additional_directories: Option<Arc<super::additional_dirs::AdditionalDirectories>>,
    /// List the agent types `rlm.spawn(agent=...)` can run (spawning works
    /// in this session).
    pub list_agent_types: bool,
}

/// What the environment states about the session model.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelFacts {
    /// `provider/id`.
    pub label: Option<String>,
    /// Whether the model accepts image input, when known.
    pub vision_capable: Option<bool>,
    /// The context window the token countdown counts down from (0: unknown).
    pub context_window: u64,
}

impl ModelFacts {
    /// The facts of a resolved model.
    #[must_use]
    pub fn of(model: &pa_types::ai::Model) -> Self {
        Self {
            label: Some(format!("{}/{}", model.provider, model.id)),
            vision_capable: Some(model.input.contains(&pa_types::ai::ModelInput::Image)),
            context_window: model.context_window,
        }
    }
}

/// The session-start instruction files for a working directory, through the
/// canonical loader ([`crate::resources::instructions`]: the user scope, the
/// ancestor chain root-most first with its imports, rules and local files,
/// then any opted-in additional directories). Conditional rules and the
/// files of directories below the working directory are not here: they
/// arrive as `nested_instructions` rows when a cell touches them.
#[must_use]
pub fn instruction_files(
    cwd: &Path,
    options: &crate::resources::instructions::InstructionOptions,
) -> Vec<InstructionFile> {
    let (files, _) = crate::resources::instructions::load_instructions(
        crate::resources::instructions::InstructionOptions {
            cwd: cwd.to_path_buf(),
            ..options.clone()
        },
    );
    files
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
    let model = config
        .model
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    EnvironmentFields {
        working_directory: forward_slashes(cwd),
        additional_directories: config
            .additional_directories
            .as_ref()
            .map(|dirs| dirs.list().iter().map(|dir| forward_slashes(dir)).collect())
            .unwrap_or_default(),
        is_git_repo: Some(git.is_some()),
        git_branch: git.and_then(|git| git.branch),
        platform: platform_name().to_string(),
        shell: config.shell.clone(),
        os_version: os_version(),
        model: model.label,
        conversation_log: config
            .conversation_log
            .as_deref()
            .map(|log| log.replace('\\', "/")),
        vision_capable: model.vision_capable,
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
    /// The session model at start (its window bounded by any context cap).
    pub model: ModelFacts,
    pub sandbox: Option<String>,
    pub shell: Option<String>,
    pub reminders: super::reminders::ReminderToggles,
    /// The `tasks` skill's store, when the session keeps a directory.
    pub tasks_file: Option<PathBuf>,
    /// The session's scratchpad directory, when one was made.
    pub scratch_directory: Option<String>,
    /// The instruction loader's options beyond the working directory.
    pub instruction_options: crate::resources::instructions::InstructionOptions,
    /// The live additional working directories.
    pub additional_directories: Option<Arc<super::additional_dirs::AdditionalDirectories>>,
    /// Whether the context lists the agent types a spawn can run.
    pub list_agent_types: bool,
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
    let context_window = wiring.model.context_window;
    let model = Arc::new(Mutex::new(wiring.model));
    let source = context_source(ContextSourceConfig {
        cwd: Arc::clone(&cwd),
        agent_dir: wiring.agent_dir,
        load_instruction_files: wiring.load_instruction_files,
        skills: wiring.skills,
        tools: wiring.tools,
        mcp_servers: wiring.mcp_servers,
        conversation_log: wiring.conversation_log,
        model: Arc::clone(&model),
        sandbox: wiring.sandbox,
        shell: wiring.shell,
        scratch_directory: wiring.scratch_directory,
        instruction_options: wiring.instruction_options,
        additional_directories: wiring.additional_directories,
        list_agent_types: wiring.list_agent_types,
    });
    let reminders = super::reminders::Reminders::new(
        wiring.reminders,
        Arc::new(super::file_tracker::FileReadTracker::new()),
        has_tasks,
        wiring.tasks_file,
        context_window,
    );
    Arc::new(
        super::context_rows::SessionContextRows::new(source, reminders)
            .with_cwd(cwd)
            .with_model(model),
    )
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
                instruction_files(&cwd, &config.instruction_options)
            } else {
                Vec::new()
            },
            agent_types: config
                .list_agent_types
                .then(|| {
                    crate::agents::agent_listing(&crate::agents::discover_agents(
                        &crate::agents::AgentSearch::for_session(&cwd, &config.agent_dir),
                    ))
                })
                .flatten(),
            skills: &config.skills,
            selected_tools: Some(&tools),
            mcp_servers: &config.mcp_servers,
            environment: probe_environment(&cwd, &config),
            date: today_utc(),
        })
    })
}
