//! CLI argument parsing, a faithful port of `cli/args.ts`. The parser is
//! intentionally hand-written: `--resume`/`--print` optionally consume the
//! next token and diagnostics carry their own formatting, none of which a
//! stock derive-based parser can express.

use pa_types::ai::ModelThinkingLevel;

/// Supported reasoning-effort levels.
pub const THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

pub fn parse_thinking_level(value: &str) -> Option<ModelThinkingLevel> {
    match value {
        "off" => Some(ModelThinkingLevel::Off),
        "minimal" => Some(ModelThinkingLevel::Minimal),
        "low" => Some(ModelThinkingLevel::Low),
        "medium" => Some(ModelThinkingLevel::Medium),
        "high" => Some(ModelThinkingLevel::High),
        "xhigh" => Some(ModelThinkingLevel::Xhigh),
        "max" => Some(ModelThinkingLevel::Max),
        _ => None,
    }
}

/// Value flags whose free-form text may legitimately start with a dash.
const FREEFORM_VALUE_FLAGS: [&str; 2] = ["--goal", "--autonomous-gate"];
/// Prompt value flags whose text may look like a long option (YAML frontmatter).
const PROMPT_VALUE_FLAGS: [&str; 2] = ["--system-prompt", "--append-system-prompt"];

/// Marker argv token that re-enables internal runtime-only flags
/// (`--export`, `--list-models`) after the public command rewrite.
pub const INTERNAL_RUNTIME_COMMAND_MARKER: &str = "\0prime-agent-runtime-command";

/// Output mode selected by `--mode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Text,
    Json,
    Rpc,
    Acp,
    Daemon,
}

impl Mode {
    pub fn parse(value: &str) -> Option<Mode> {
        match value {
            "text" => Some(Mode::Text),
            "json" => Some(Mode::Json),
            "rpc" => Some(Mode::Rpc),
            "acp" => Some(Mode::Acp),
            "daemon" => Some(Mode::Daemon),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Text => "text",
            Mode::Json => "json",
            Mode::Rpc => "rpc",
            Mode::Acp => "acp",
            Mode::Daemon => "daemon",
        }
    }
}

/// A parse-time diagnostic: warnings are printed, errors also exit 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub is_error: bool,
    pub message: String,
}

impl Diagnostic {
    pub(crate) fn error(message: impl Into<String>) -> Self {
        Diagnostic {
            is_error: true,
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AutonomousGates {
    pub commands: Vec<String>,
    pub max_retries: Option<u32>,
    pub timeout_ms: Option<u64>,
}

/// The autonomous configuration inferred from the CLI flags.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AutonomousConfig {
    pub max_continuations: Option<u32>,
    pub max_turns: Option<u32>,
    pub max_tokens: Option<u64>,
    pub timeout_ms: Option<u64>,
    pub gates: Option<AutonomousGates>,
}

impl AutonomousConfig {
    /// Build the autonomous runtime config from parsed args; `None` when no
    /// autonomous flag was given at all.
    pub fn from_args(args: &Args) -> Option<AutonomousConfig> {
        let has_autonomous_options = args.autonomous
            || args.autonomous_gates.is_some()
            || args.autonomous_gate_retries.is_some()
            || args.autonomous_gate_timeout_ms.is_some()
            || args.autonomous_max_continuations.is_some()
            || args.autonomous_max_turns.is_some()
            || args.autonomous_max_tokens.is_some()
            || args.autonomous_timeout_ms.is_some();
        if !has_autonomous_options {
            return None;
        }
        let has_gate_options = args.autonomous_gates.is_some()
            || args.autonomous_gate_retries.is_some()
            || args.autonomous_gate_timeout_ms.is_some();
        Some(AutonomousConfig {
            max_continuations: args.autonomous_max_continuations,
            max_turns: args.autonomous_max_turns,
            max_tokens: args.autonomous_max_tokens,
            timeout_ms: args.autonomous_timeout_ms,
            gates: has_gate_options.then(|| AutonomousGates {
                commands: args.autonomous_gates.clone().unwrap_or_default(),
                max_retries: args.autonomous_gate_retries,
                timeout_ms: args.autonomous_gate_timeout_ms,
            }),
        })
    }
}

#[derive(Debug, Clone, Default)]
#[allow(clippy::struct_excessive_bools)] // the TS `Args` surface mirrored verbatim
pub struct Args {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub cwd: Option<String>,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Vec<String>,
    pub thinking: Option<ModelThinkingLevel>,
    pub continue_: bool,
    /// `true` when `--resume` was given without a selector.
    pub resume_bare: bool,
    pub resume: Option<String>,
    pub help: bool,
    pub version: bool,
    pub mode: Option<Mode>,
    pub daemon_socket: Option<String>,
    /// `--daemon-port <n>`: the tailnet mesh listener's TCP port (TS
    /// #2517). `None` when unset.
    pub daemon_port: Option<u16>,
    /// `--daemon-bind <ip>`: the address the mesh listener binds (TS
    /// #2517). `None` when unset.
    pub daemon_bind_host: Option<String>,
    pub no_session: bool,
    pub fork: Option<String>,
    pub session_dir: Option<String>,
    pub models: Option<Vec<String>>,
    pub print: bool,
    pub export: Option<String>,
    pub no_skills: bool,
    pub skills: Vec<String>,
    pub prompt_templates: Vec<String>,
    pub no_prompt_templates: bool,
    pub themes: Vec<String>,
    pub no_themes: bool,
    pub no_context_files: bool,
    /// `--trust-workspace`: trust the cwd's project configuration (recorded
    /// like `prime-agent trust`) instead of skipping it.
    pub trust_workspace: bool,
    pub autonomous: bool,
    pub autonomous_gates: Option<Vec<String>>,
    pub autonomous_gate_retries: Option<u32>,
    pub autonomous_gate_timeout_ms: Option<u64>,
    pub autonomous_max_continuations: Option<u32>,
    pub autonomous_max_turns: Option<u32>,
    pub autonomous_max_tokens: Option<u64>,
    pub autonomous_timeout_ms: Option<u64>,
    pub goal: Option<String>,
    pub goal_token_budget: Option<u32>,
    /// The `--list-models` flag's tri-state (None = absent,
    /// Some(None) = the bare flag, Some(Some(x)) = with a value).
    #[allow(clippy::option_option)] // the CLI flag's tri-state is genuinely a double Option
    pub list_models: Option<Option<String>>,
    pub offline: bool,
    /// `--plan`: start the session in plan mode (edits blocked).
    pub plan: bool,
    /// `--sandbox <mode>`: the OS sandbox for this run (`off` | `read-only` |
    /// `workspace-write`), over the `sandbox` setting.
    pub sandbox: Option<pa_core::os_sandbox::SandboxMode>,
    /// `--add-dir <directory>` (repeatable): extra working directories for
    /// the session (Claude Code's flag).
    pub add_dirs: Vec<String>,
    pub verbose: bool,
    pub messages: Vec<String>,
    #[allow(clippy::struct_field_names)]
    // the trailing _args matches the TS `fileArgs` wire surface
    pub file_args: Vec<String>,
    pub diagnostics: Vec<Diagnostic>,
    /// `--name <name>`: open the current directory's session of that name, creating it when
    /// none exists (upstream #1294).
    pub name: Option<String>,
    /// `--list-sessions`: print the current directory's saved sessions and exit.
    pub list_sessions: bool,
    /// `--delete-session <id|name>`: delete one saved session and exit.
    pub delete_session: Option<String>,
    /// `-w, --worktree [name]`: run the session in a managed git worktree
    /// (`pa_core::worktree`), created or reused before the session starts.
    pub worktree: bool,
    /// The worktree's name (`None`: a generated one).
    pub worktree_name: Option<String>,
    /// Claude Code's flags (headless formats, SDK switches, permission and
    /// configuration flags; `crate::args_cc`).
    pub cc: crate::args_cc::CcFlags,
    /// `--from-pr [n|url|term]` (Claude Code's flag): open the resume picker on the sessions
    /// linked to a pull request; `Some("")` when given without a value.
    pub from_pr: Option<String>,
}

impl Args {
    /// True when `--resume` appeared (with or without a selector).
    pub fn has_resume(&self) -> bool {
        self.resume_bare || self.resume.is_some()
    }
}

fn parse_positive_u32(value: &str, flag: &str, diagnostics: &mut Vec<Diagnostic>) -> Option<u32> {
    // Number() accepts arbitrary precision, so parse as i128; a value past
    // u32::MAX is a user error, never a silent truncation.
    match value.trim().parse::<i128>() {
        Ok(parsed) if parsed > 0 && parsed <= i128::from(u32::MAX) => u32::try_from(parsed).ok(),
        _ => {
            diagnostics.push(Diagnostic::error(format!(
                "{flag} must be a positive integer between 1 and u32::MAX (4294967295)"
            )));
            None
        }
    }
}

fn parse_positive_u64(value: &str, flag: &str, diagnostics: &mut Vec<Diagnostic>) -> Option<u64> {
    // Number() accepts arbitrary precision, so parse as i128 to cover the
    // full accepted range before the integer/positivity check.
    match value.trim().parse::<i128>() {
        Ok(parsed) if parsed > 0 && parsed <= i128::from(u64::MAX) => u64::try_from(parsed).ok(),
        _ => {
            diagnostics.push(Diagnostic::error(format!(
                "{flag} must be a positive integer between 1 and u64::MAX (18446744073709551615)"
            )));
            None
        }
    }
}

pub fn parse_args(args: &[String]) -> Args {
    let args = &crate::args_cc::split_equals(args);
    let internal_runtime_command =
        args.first().map(String::as_str) == Some(INTERNAL_RUNTIME_COMMAND_MARKER);
    let mut parser = Parser {
        args,
        index: usize::from(internal_runtime_command),
        internal_runtime_command,
        result: Args::default(),
    };
    parser.parse();
    let mut result = parser.result;

    crate::args_cc::validate(
        &result.cc,
        result.print,
        result.worktree,
        &mut result.diagnostics,
    );
    if result.goal_token_budget.is_some() && result.goal.is_none() {
        result
            .diagnostics
            .push(Diagnostic::error("--goal-token-budget requires --goal"));
    }

    result
}

/// The argv walk: `index` is the token being parsed; a flag that consumes
/// the token after it moves `index` onto that token.
struct Parser<'a> {
    args: &'a [String],
    index: usize,
    internal_runtime_command: bool,
    result: Args,
}

impl Parser<'_> {
    fn parse(&mut self) {
        let args = self.args;
        let mut end_of_options = false;
        while self.index < args.len() {
            let arg = args[self.index].as_str();
            if end_of_options {
                self.result.messages.push(arg.to_string());
            } else if arg == "--" {
                end_of_options = true;
            } else {
                self.option(arg);
            }
            self.index += 1;
        }
    }

    /// One token before `--`: a flag, an `@file` argument or a message.
    fn option(&mut self, arg: &str) {
        if crate::args_cc::parse_cc_flag(
            self.args,
            &mut self.index,
            &mut self.result.cc,
            &mut self.result.diagnostics,
        ) {
            return;
        }
        if let Some(switch) = switch(&mut self.result, arg) {
            *switch = true;
            return;
        }
        if let Some(flag) = ValueFlag::of(arg) {
            if flag.enables_autonomous() {
                self.result.autonomous = true;
            }
            if let Some(value) =
                take_required_value(self.args, self.index, arg, &mut self.result.diagnostics)
            {
                self.index += 1;
                flag.apply(arg, value, &mut self.result);
            }
            return;
        }
        let next = self.args.get(self.index + 1);
        match arg {
            "--resume" | "-r" => self.resume(),
            // The name is optional: the next token is taken only when it is a
            // valid worktree name, so `-w "fix the login bug"` stays a message.
            "--worktree" | "-w" => {
                self.result.worktree = true;
                if let Some(next) =
                    next.filter(|next| crate::global_flags::is_worktree_name_value(next))
                {
                    self.result.worktree_name = Some(next.clone());
                    self.index += 1;
                }
            }
            // The value is optional, like `--resume`'s.
            "--from-pr" => match next {
                Some(next) if !next.starts_with('-') && !next.starts_with('@') => {
                    self.result.from_pr = Some(next.clone());
                    self.index += 1;
                }
                _ => self.result.from_pr = Some(String::new()),
            },
            "--print" | "-p" => {
                self.result.print = true;
                if let Some(next) = next {
                    if !next.starts_with('@') && (!next.starts_with('-') || next.starts_with("---"))
                    {
                        self.result.messages.push(next.clone());
                        self.index += 1;
                    }
                }
            }
            "--export" => self.export(),
            "--list-models" => self.list_models(),
            _ => self.word(arg),
        }
    }

    /// `--resume [selector]`: an empty next token is consumed as no selector.
    fn resume(&mut self) {
        match self.args.get(self.index + 1) {
            Some(next) if !next.starts_with('-') && !next.starts_with('@') && !next.is_empty() => {
                self.result.resume = Some(next.clone());
                self.index += 1;
            }
            Some(next) if next.is_empty() => {
                self.result.resume_bare = true;
                self.index += 1;
            }
            _ => self.result.resume_bare = true,
        }
    }

    /// `--export <file>`: an internal runtime flag; a user's `--export` is
    /// the removed command, its value skipped.
    fn export(&mut self) {
        let next = self.args.get(self.index + 1);
        if !self.internal_runtime_command {
            self.result.diagnostics.push(Diagnostic::error(
                "--export was removed. Use \"prime-agent session export <file> [output]\".",
            ));
            if next.is_some_and(|next| !next.starts_with('-')) {
                self.index += 1;
            }
        } else if let Some(next) = next {
            self.result.export = Some(next.clone());
            self.index += 1;
        } else {
            self.result
                .diagnostics
                .push(Diagnostic::error("--export requires a value"));
        }
    }

    /// `--list-models [search]`: an internal runtime flag; a user's
    /// `--list-models` is the removed command, its search skipped.
    fn list_models(&mut self) {
        let search = self
            .args
            .get(self.index + 1)
            .filter(|next| !next.starts_with('-') && !next.starts_with('@'));
        if !self.internal_runtime_command {
            self.result.diagnostics.push(Diagnostic::error(
                "--list-models was removed. Use \"prime-agent model list [search]\".",
            ));
            if search.is_some() {
                self.index += 1;
            }
        } else if let Some(search) = search {
            self.result.list_models = Some(Some(search.clone()));
            self.index += 1;
        } else {
            self.result.list_models = Some(None);
        }
    }

    /// A token that is no known flag: a `--flag=value` form, an `@file`
    /// argument, an unknown option, or a message.
    fn word(&mut self, arg: &str) {
        let result = &mut self.result;
        if let Some(value) = arg.strip_prefix("--from-pr=") {
            result.from_pr = Some(value.to_string());
        } else if let Some(value) = arg.strip_prefix("--resume=") {
            if value.is_empty() {
                result.resume_bare = true;
            } else {
                result.resume = Some(value.to_string());
            }
        } else if arg.starts_with("--export=") {
            result.diagnostics.push(Diagnostic::error(
                "--export was removed. Use \"prime-agent session export <file> [output]\".",
            ));
        } else if arg.starts_with("--list-models=") {
            result.diagnostics.push(Diagnostic::error(
                "--list-models was removed. Use \"prime-agent model list [search]\".",
            ));
        } else if let Some(file) = arg.strip_prefix('@') {
            result.file_args.push(file.to_string());
        } else if arg.starts_with('-') {
            result
                .diagnostics
                .push(Diagnostic::error(format!("Unknown option: {arg}")));
        } else {
            result.messages.push(arg.to_string());
        }
    }
}

/// The field a boolean flag sets.
fn switch<'r>(result: &'r mut Args, arg: &str) -> Option<&'r mut bool> {
    Some(match arg {
        "--help" | "-h" => &mut result.help,
        "--version" | "-v" => &mut result.version,
        "--continue" | "-c" => &mut result.continue_,
        "--no-session" => &mut result.no_session,
        "--list-sessions" => &mut result.list_sessions,
        "--no-skills" | "-ns" => &mut result.no_skills,
        "--no-prompt-templates" | "-np" => &mut result.no_prompt_templates,
        "--no-themes" => &mut result.no_themes,
        "--no-context-files" | "-nc" => &mut result.no_context_files,
        "--trust-workspace" => &mut result.trust_workspace,
        "--autonomous" => &mut result.autonomous,
        "--verbose" => &mut result.verbose,
        "--offline" => &mut result.offline,
        "--plan" => &mut result.plan,
        _ => return None,
    })
}

/// A flag that takes one required value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ValueFlag {
    Mode,
    DaemonSocket,
    DaemonPort,
    DaemonBind,
    Provider,
    Model,
    ApiKey,
    Cwd,
    SystemPrompt,
    AppendSystemPrompt,
    Name,
    DeleteSession,
    Fork,
    SessionDir,
    Models,
    Thinking,
    Skill,
    PromptTemplate,
    Theme,
    AutonomousGate,
    AutonomousGateRetries,
    AutonomousGateTimeoutMs,
    AutonomousMaxContinuations,
    AutonomousMaxTurns,
    AutonomousMaxTokens,
    AutonomousTimeoutMs,
    Goal,
    GoalTokenBudget,
    AddDir,
    Sandbox,
}

impl ValueFlag {
    fn of(arg: &str) -> Option<Self> {
        Some(match arg {
            "--mode" => Self::Mode,
            "--daemon-socket" => Self::DaemonSocket,
            "--daemon-port" => Self::DaemonPort,
            "--daemon-bind" => Self::DaemonBind,
            "--provider" => Self::Provider,
            "--model" => Self::Model,
            "--api-key" => Self::ApiKey,
            "--cwd" => Self::Cwd,
            "--system-prompt" => Self::SystemPrompt,
            "--append-system-prompt" => Self::AppendSystemPrompt,
            "--name" => Self::Name,
            "--delete-session" => Self::DeleteSession,
            "--fork" => Self::Fork,
            "--session-dir" => Self::SessionDir,
            "--models" => Self::Models,
            "--thinking" => Self::Thinking,
            "--skill" => Self::Skill,
            "--prompt-template" => Self::PromptTemplate,
            "--theme" => Self::Theme,
            "--autonomous-gate" => Self::AutonomousGate,
            "--autonomous-gate-retries" => Self::AutonomousGateRetries,
            "--autonomous-gate-timeout-ms" => Self::AutonomousGateTimeoutMs,
            "--autonomous-max-continuations" => Self::AutonomousMaxContinuations,
            "--autonomous-max-turns" => Self::AutonomousMaxTurns,
            "--autonomous-max-tokens" => Self::AutonomousMaxTokens,
            "--autonomous-timeout-ms" => Self::AutonomousTimeoutMs,
            "--goal" => Self::Goal,
            "--goal-token-budget" => Self::GoalTokenBudget,
            "--add-dir" => Self::AddDir,
            "--sandbox" => Self::Sandbox,
            _ => return None,
        })
    }

    /// The autonomous flags turn autonomy on even when their value is missing.
    fn enables_autonomous(self) -> bool {
        matches!(
            self,
            Self::AutonomousGate
                | Self::AutonomousGateRetries
                | Self::AutonomousGateTimeoutMs
                | Self::AutonomousMaxContinuations
                | Self::AutonomousMaxTurns
                | Self::AutonomousMaxTokens
                | Self::AutonomousTimeoutMs
        )
    }

    /// Store `value` given to `flag` (as written), or record why it is invalid.
    fn apply(self, flag: &str, value: String, result: &mut Args) {
        let diagnostics = &mut result.diagnostics;
        match self {
            Self::Mode => {
                if let Some(mode) = Mode::parse(&value) {
                    result.mode = Some(mode);
                } else {
                    diagnostics.push(Diagnostic::error(format!(
                        "Invalid --mode \"{value}\". Valid values: text, json, rpc, acp, daemon"
                    )));
                }
            }
            Self::DaemonSocket => result.daemon_socket = Some(value),
            Self::DaemonPort => match value.parse::<u16>() {
                Ok(port) if (1..=u16::MAX).contains(&port) => result.daemon_port = Some(port),
                _ => diagnostics.push(Diagnostic::error(format!(
                    "Invalid --daemon-port \"{value}\": expected an integer between 1 and 65535"
                ))),
            },
            Self::DaemonBind => {
                let value = value.trim().to_string();
                // An IP literal binds exactly one interface; a hostname
                // would resolve through DNS at listen time and could dodge
                // the tailnet-only default.
                if value.parse::<std::net::IpAddr>().is_err() {
                    diagnostics.push(Diagnostic::error(format!(
                        "Invalid --daemon-bind \"{value}\": expected an IP address (e.g. the tailnet address of this machine)"
                    )));
                } else {
                    result.daemon_bind_host = Some(value);
                }
            }
            Self::Provider => result.provider = Some(value),
            Self::Model => result.model = Some(value),
            Self::ApiKey => result.api_key = Some(value),
            Self::Cwd => result.cwd = Some(value),
            Self::SystemPrompt => result.system_prompt = Some(value),
            Self::AppendSystemPrompt => result.append_system_prompt.push(value),
            Self::Name => result.name = Some(value),
            Self::DeleteSession => result.delete_session = Some(value),
            Self::Fork => result.fork = Some(value),
            Self::SessionDir => result.session_dir = Some(value),
            Self::Models => {
                result.models = Some(
                    value
                        .split(',')
                        .map(str::trim)
                        .map(str::to_string)
                        .collect(),
                );
            }
            Self::Thinking => {
                if let Some(level) = parse_thinking_level(&value) {
                    result.thinking = Some(level);
                } else {
                    diagnostics.push(Diagnostic::error(format!(
                        "Invalid thinking level \"{value}\". Valid values: {}",
                        THINKING_LEVELS.join(", ")
                    )));
                }
            }
            Self::Skill => result.skills.push(value),
            Self::PromptTemplate => result.prompt_templates.push(value),
            Self::Theme => result.themes.push(value),
            Self::AutonomousGate => result
                .autonomous_gates
                .get_or_insert_with(Vec::new)
                .push(value),
            Self::AutonomousGateRetries => {
                result.autonomous_gate_retries = parse_positive_u32(&value, flag, diagnostics);
            }
            Self::AutonomousGateTimeoutMs => {
                result.autonomous_gate_timeout_ms = parse_positive_u64(&value, flag, diagnostics);
            }
            Self::AutonomousMaxContinuations => {
                result.autonomous_max_continuations = parse_positive_u32(&value, flag, diagnostics);
            }
            Self::AutonomousMaxTurns => {
                result.autonomous_max_turns = parse_positive_u32(&value, flag, diagnostics);
            }
            Self::AutonomousMaxTokens => {
                result.autonomous_max_tokens = parse_positive_u64(&value, flag, diagnostics);
            }
            Self::AutonomousTimeoutMs => {
                result.autonomous_timeout_ms = parse_positive_u64(&value, flag, diagnostics);
            }
            Self::Goal => {
                if value.trim().is_empty() {
                    diagnostics.push(Diagnostic::error("--goal requires a non-empty objective"));
                } else {
                    result.goal = Some(value);
                }
            }
            Self::GoalTokenBudget => {
                result.goal_token_budget = parse_positive_u32(&value, flag, diagnostics);
            }
            Self::AddDir => result.add_dirs.push(value),
            Self::Sandbox => {
                if let Some(mode) = pa_core::os_sandbox::SandboxMode::from_wire(&value) {
                    result.sandbox = Some(mode);
                } else {
                    diagnostics.push(Diagnostic::error(format!(
                        "Invalid sandbox mode \"{value}\". Valid values: {}",
                        pa_core::os_sandbox::SandboxMode::ALL
                            .map(pa_core::os_sandbox::SandboxMode::wire_name)
                            .join(", ")
                    )));
                }
            }
        }
    }
}

/// The value flag either consumes the next token or records a
/// "requires a value" error.
fn take_required_value(
    args: &[String],
    index: usize,
    flag: &str,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<String> {
    let Some(next) = args.get(index + 1) else {
        diagnostics.push(Diagnostic::error(format!("{flag} requires a value")));
        return None;
    };
    let value_may_start_with_dash = FREEFORM_VALUE_FLAGS.contains(&flag);
    let value_is_arbitrary_prompt_text = PROMPT_VALUE_FLAGS.contains(&flag);
    if next == "--"
        || (!value_is_arbitrary_prompt_text
            && next.starts_with(if value_may_start_with_dash { "--" } else { "-" }))
    {
        diagnostics.push(Diagnostic::error(format!("{flag} requires a value")));
        return None;
    }
    Some(next.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Args {
        let args: Vec<String> = args.iter().map(std::string::ToString::to_string).collect();
        parse_args(&args)
    }

    fn last_error(args: &Args) -> &str {
        args.diagnostics
            .iter()
            .rev()
            .find(|d| d.is_error)
            .map(|d| d.message.as_str())
            .unwrap_or_default()
    }

    /// The named-session flags (upstream #1294) take their values; nothing else changes.
    #[test]
    fn named_session_flags_parse() {
        let parsed = parse(&[
            "--name",
            "work",
            "--list-sessions",
            "--delete-session",
            "old",
        ]);
        assert_eq!(
            (
                parsed.name.as_deref(),
                parsed.list_sessions,
                parsed.delete_session.as_deref(),
                parsed.diagnostics.len(),
            ),
            (Some("work"), true, Some("old"), 0)
        );
        assert_eq!(last_error(&parse(&["--name"])), "--name requires a value");
    }

    #[test]
    fn unknown_short_option_is_an_error() {
        let parsed = parse(&["-x"]);
        assert_eq!(last_error(&parsed), "Unknown option: -x");
    }

    #[test]
    fn unknown_long_flag_is_an_error() {
        let parsed = parse(&["--bogus-flag"]);
        assert_eq!(last_error(&parsed), "Unknown option: --bogus-flag");
        let parsed = parse(&["--bogus-flag", "value"]);
        assert_eq!(last_error(&parsed), "Unknown option: --bogus-flag");
    }

    /// `--plan` is a boolean flag (the next token stays a message) and
    /// reaches the session config.
    #[test]
    fn plan_flag_starts_in_plan_mode() {
        let parsed = parse(&["--plan", "audit the parser"]);
        assert_eq!(
            (parsed.plan, parsed.messages.clone(), last_error(&parsed)),
            (true, vec!["audit the parser".to_string()], "")
        );
        let config = crate::mode::runtime_config_from_args(
            &parsed,
            std::path::PathBuf::from("/work"),
            std::path::PathBuf::from("/agent"),
            None,
            crate::mode::AppMode::Print,
            /*telemetry_disabled*/ true,
        );
        assert!(config.plan_mode);
        assert!(!parse(&["hello"]).plan);
    }

    /// `--add-dir` repeats, resolves against the working directory, and reaches the config.
    #[test]
    fn add_dir_flags_reach_the_config_resolved() {
        let parsed = parse(&["--add-dir", "../lib", "--add-dir", "/abs", "hi"]);
        let config = crate::mode::runtime_config_from_args(
            &parsed,
            std::path::PathBuf::from("/work"),
            std::path::PathBuf::from("/agent"),
            None,
            crate::mode::AppMode::Print,
            /*telemetry_disabled*/ true,
        );
        assert_eq!(
            (config.additional_directories, parsed.messages),
            (
                vec![
                    std::path::PathBuf::from("/work/../lib"),
                    std::path::PathBuf::from("/abs")
                ],
                vec!["hi".to_string()]
            )
        );
    }

    /// `--sandbox <mode>` takes one of the three modes, reaches the session config, and
    /// rejects anything else.
    #[test]
    fn sandbox_flag_selects_the_mode_for_the_run() {
        use pa_core::os_sandbox::SandboxMode;
        let parsed = parse(&["--sandbox", "workspace-write", "fix the bug"]);
        let config = crate::mode::runtime_config_from_args(
            &parsed,
            std::path::PathBuf::from("/work"),
            std::path::PathBuf::from("/agent"),
            None,
            crate::mode::AppMode::Print,
            /*telemetry_disabled*/ true,
        );
        assert_eq!(
            (
                config.sandbox_mode,
                parsed.messages,
                parse(&["--sandbox", "off"]).sandbox,
                parse(&["hello"]).sandbox,
                last_error(&parse(&["--sandbox", "full"])).to_string(),
                last_error(&parse(&["--sandbox"])).to_string(),
            ),
            (
                Some(SandboxMode::WorkspaceWrite),
                vec!["fix the bug".to_string()],
                Some(SandboxMode::Off),
                None,
                "Invalid sandbox mode \"full\". Valid values: off, read-only, workspace-write"
                    .to_string(),
                "--sandbox requires a value".to_string(),
            )
        );
    }

    #[test]
    fn print_consumes_next_message() {
        let parsed = parse(&["-p", "hello", "world"]);
        assert!(parsed.print);
        assert_eq!(parsed.messages, vec!["hello", "world"]);
    }

    #[test]
    fn resume_accepts_selector_bare_and_equals() {
        assert_eq!(parse(&["-r", "abc"]).resume.as_deref(), Some("abc"));
        let named = parse(&["--worktree", "feature/login", "hello"]);
        assert!(named.worktree);
        assert_eq!(named.worktree_name.as_deref(), Some("feature/login"));
        assert_eq!(named.messages, vec!["hello"]);
        let unnamed = parse(&["-w", "fix the login bug"]);
        assert!(unnamed.worktree);
        assert_eq!(unnamed.worktree_name, None);
        assert_eq!(unnamed.messages, vec!["fix the login bug"]);
        let flag_next = parse(&["-w", "--plan"]);
        assert!(flag_next.worktree && flag_next.plan);
        assert_eq!(flag_next.worktree_name, None);
        assert!(parse(&["--resume", "--verbose"]).resume_bare);
        assert_eq!(parse(&["--resume=abc"]).resume.as_deref(), Some("abc"));
        assert!(parse(&["--resume="]).resume_bare);
        assert_eq!(parse(&["--from-pr"]).from_pr.as_deref(), Some(""));
        assert_eq!(parse(&["--from-pr", "42"]).from_pr.as_deref(), Some("42"));
        assert_eq!(parse(&["--from-pr=42"]).from_pr.as_deref(), Some("42"));
        assert_eq!(
            parse(&["--from-pr", "--verbose"]).from_pr.as_deref(),
            Some("")
        );
        // A selector starting with - or @ stays bare.
        assert!(parse(&["-r", "-x"]).resume_bare);
    }

    #[test]
    fn end_of_options_marker() {
        let parsed = parse(&["--", "-x", "--mode"]);
        assert_eq!(parsed.messages, vec!["-x", "--mode"]);
    }

    #[test]
    fn export_removed_without_internal_marker() {
        let parsed = parse(&["--export", "foo"]);
        assert_eq!(
            last_error(&parsed),
            "--export was removed. Use \"prime-agent session export <file> [output]\"."
        );
        let marker: Vec<String> = vec![
            INTERNAL_RUNTIME_COMMAND_MARKER.into(),
            "--export".into(),
            "foo".into(),
        ];
        let parsed = parse_args(&marker);
        assert_eq!(parsed.export.as_deref(), Some("foo"));
    }

    #[test]
    fn goal_budget_requires_goal() {
        let parsed = parse(&["--goal-token-budget", "5"]);
        assert_eq!(last_error(&parsed), "--goal-token-budget requires --goal");
    }

    #[test]
    fn positive_int_rejects_zero_and_text() {
        let parsed = parse(&["--autonomous-max-turns", "0"]);
        assert_eq!(
            last_error(&parsed),
            "--autonomous-max-turns must be a positive integer between 1 and u32::MAX (4294967295)"
        );
        let parsed = parse(&["--autonomous-max-turns", "abc"]);
        assert_eq!(
            last_error(&parsed),
            "--autonomous-max-turns must be a positive integer between 1 and u32::MAX (4294967295)"
        );
    }

    #[test]
    fn positive_int_rejects_past_the_field_range() {
        // 2**32: the u32 flags reject it out loud instead of silently
        // truncating into the flag's u32 field.
        let parsed = parse(&["--autonomous-max-turns", "4294967296"]);
        assert_eq!(
            last_error(&parsed),
            "--autonomous-max-turns must be a positive integer between 1 and u32::MAX (4294967295)"
        );
        // The u64 flags accept the same value.
        let parsed = parse(&["--autonomous-timeout-ms", "4294967296"]);
        assert_eq!(parsed.autonomous_timeout_ms, Some(4_294_967_296));
    }
    /// `--daemon-port` (TS #2517): a valid port parses; an invalid one is
    /// a named diagnostic, never silently ignored.
    #[test]
    fn daemon_port_parses_and_validates() {
        let parsed = parse(&["--daemon-port", "4700"]);
        assert!(parsed.diagnostics.is_empty());
        assert_eq!(parsed.daemon_port, Some(4700));
        for bad in ["0", "65536", "not-a-port"] {
            let parsed = parse(&["--daemon-port", bad]);
            let error = last_error(&parsed);
            assert!(error.contains("Invalid --daemon-port"), "{bad}: {error}");
            assert_eq!(parsed.daemon_port, None, "{bad} must not set a port");
        }
    }

    /// `--daemon-bind` (TS #2517): an IP literal parses; a hostname is
    /// a named diagnostic (a hostname would resolve through DNS at
    /// listen time and could dodge the tailnet-only default).
    #[test]
    fn daemon_bind_requires_an_ip_literal() {
        let parsed = parse(&["--daemon-bind", "100.64.1.2"]);
        assert!(parsed.diagnostics.is_empty());
        assert_eq!(parsed.daemon_bind_host.as_deref(), Some("100.64.1.2"));
        let parsed = parse(&["--daemon-bind", "::1"]);
        assert!(parsed.diagnostics.is_empty());
        assert_eq!(parsed.daemon_bind_host.as_deref(), Some("::1"));
        let parsed = parse(&["--daemon-bind", "example.com"]);
        let error = last_error(&parsed);
        assert!(
            error.contains("Invalid --daemon-bind") && error.contains("expected an IP address"),
            "{error}"
        );
        assert_eq!(parsed.daemon_bind_host, None);
    }
}
