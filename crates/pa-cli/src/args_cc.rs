//! Claude Code's command-line flags (the 2.1.296 `claude` program): the
//! headless output and input formats, the SDK switches, the print-mode
//! limits, the permission flags, the configuration flags, and the small
//! launch flags. [`parse_cc_flag`] recognises them inside
//! [`crate::args::parse_args`]; [`validate`] applies Claude Code's
//! combination rules (its error wording); [`launch_settings`] turns the
//! session-wide ones into the launch layer
//! ([`pa_core::settings::launch::LaunchSettings`]).

use pa_core::settings::launch::{
    CliPermissions,
    LaunchSettings,
    SettingSource,
    read_json_object_arg,
    split_tool_list,
};

use crate::args::Diagnostic;

/// `--output-format`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// The answer's text (the default).
    Text,
    /// One `result` object at the end.
    Json,
    /// The SDK message stream, one JSON object per line.
    StreamJson,
}

impl OutputFormat {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "text" => Some(OutputFormat::Text),
            "json" => Some(OutputFormat::Json),
            "stream-json" => Some(OutputFormat::StreamJson),
            _ => None,
        }
    }
}

/// `--input-format`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputFormat {
    /// The prompt from the arguments and piped stdin (the default).
    Text,
    /// SDK user messages and control requests, one JSON object per stdin
    /// line, for the life of the process.
    StreamJson,
}

/// `--tmux[=classic]`: how the worktree's tmux session is laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TmuxMode {
    /// A plain tmux session (this port has no iTerm2 integration).
    Native,
    Classic,
}

/// `--init`, `--init-only`, `--maintenance`: the `Setup` hooks' trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupTrigger {
    /// Run `Setup` (trigger `init`), then continue.
    Init,
    /// Run `Setup` and `SessionStart` (source `startup`), then exit.
    InitOnly,
    /// Run `Setup` (trigger `maintenance`), then continue.
    Maintenance,
}

impl SetupTrigger {
    #[must_use]
    pub fn trigger(self) -> &'static str {
        match self {
            SetupTrigger::Init | SetupTrigger::InitOnly => "init",
            SetupTrigger::Maintenance => "maintenance",
        }
    }
}

/// The Claude Code flags of one invocation.
#[derive(Debug, Clone, Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "Claude Code's independent command-line switches"
)]
pub struct CcFlags {
    pub output_format: Option<OutputFormat>,
    pub input_format: Option<InputFormat>,
    pub include_partial_messages: bool,
    pub replay_user_messages: bool,
    pub include_hook_events: bool,
    /// `--json-schema`: the schema text (validated by [`validate`]).
    pub json_schema: Option<serde_json::Value>,
    pub max_turns: Option<u32>,
    pub max_budget_usd: Option<f64>,
    pub allowed_tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
    pub tools: Option<Vec<String>>,
    pub permission_mode: Option<String>,
    pub dangerously_skip_permissions: bool,
    pub allow_dangerously_skip_permissions: bool,
    /// `--permission-prompt-tool`: `stdio` (the SDK host answers
    /// `can_use_tool`) or an MCP tool `mcp__<server>__<tool>`.
    pub permission_prompt_tool: Option<String>,
    pub settings: Option<String>,
    pub setting_sources: Option<String>,
    pub mcp_config: Vec<String>,
    pub strict_mcp_config: bool,
    pub agents: Option<String>,
    pub agent: Option<String>,
    pub session_id: Option<String>,
    pub fallback_model: Option<String>,
    pub bare: bool,
    pub betas: Vec<String>,
    pub tmux: Option<TmuxMode>,
    pub prefill: Option<String>,
    /// `-d, --debug [filter]`: `Some(None)` for the bare flag.
    #[expect(
        clippy::option_option,
        reason = "absent, the bare flag, and a filter are three different states"
    )]
    pub debug: Option<Option<String>>,
    pub debug_file: Option<String>,
    pub setup: Option<SetupTrigger>,
}

impl CcFlags {
    /// Whether a format flag makes this a headless (print) run.
    #[must_use]
    pub fn headless(&self) -> bool {
        matches!(
            self.output_format,
            Some(OutputFormat::Json | OutputFormat::StreamJson)
        ) || self.input_format == Some(InputFormat::StreamJson)
    }

    /// The output format (text when unset).
    #[must_use]
    pub fn output(&self) -> OutputFormat {
        self.output_format.unwrap_or(OutputFormat::Text)
    }

    /// Whether stdin carries stream-json input.
    #[must_use]
    pub fn stream_input(&self) -> bool {
        self.input_format == Some(InputFormat::StreamJson)
    }
}

/// Value flags that also take `--flag=value`.
const EQUALS_FLAGS: [&str; 22] = [
    "--output-format",
    "--input-format",
    "--json-schema",
    "--max-turns",
    "--max-budget-usd",
    "--allowedTools",
    "--allowed-tools",
    "--disallowedTools",
    "--disallowed-tools",
    "--tools",
    "--permission-mode",
    "--permission-prompt-tool",
    "--settings",
    "--setting-sources",
    "--mcp-config",
    "--agents",
    "--agent",
    "--session-id",
    "--fallback-model",
    "--betas",
    "--prefill",
    "--debug-file",
];

/// Boolean Claude Code flags (the leading-flag scan must not take their
/// next token as a value).
pub const CC_BOOLEAN_FLAGS: [&str; 12] = [
    "--include-partial-messages",
    "--replay-user-messages",
    "--include-hook-events",
    "--dangerously-skip-permissions",
    "--allow-dangerously-skip-permissions",
    "--strict-mcp-config",
    "--bare",
    "--tmux",
    "--init",
    "--init-only",
    "--maintenance",
    "--debug",
];

/// Split `--flag=value` for the Claude Code value flags (Commander accepts
/// both spellings); every other token is kept as is.
#[must_use]
pub fn split_equals(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut end_of_options = false;
    for arg in args {
        if end_of_options {
            out.push(arg.clone());
            continue;
        }
        if arg == "--" {
            end_of_options = true;
            out.push(arg.clone());
            continue;
        }
        match arg.split_once('=') {
            Some((flag, value)) if EQUALS_FLAGS.contains(&flag) => {
                out.push(flag.to_string());
                out.push(value.to_string());
            }
            Some((flag, value)) if flag == "--debug" => {
                out.push(flag.to_string());
                out.push(value.to_string());
            }
            _ => out.push(arg.clone()),
        }
    }
    out
}

/// The next token as a flag's value; a token starting with `-` is not one.
fn value(
    args: &[String],
    i: &mut usize,
    flag: &str,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<String> {
    match args.get(*i + 1) {
        Some(next) if next != "--" && (!next.starts_with('-') || flag == "--json-schema") => {
            *i += 1;
            Some(next.clone())
        }
        _ => {
            diagnostics.push(Diagnostic::error(format!("{flag} requires a value")));
            None
        }
    }
}

/// A variadic flag's values (Commander's `<values...>`): every following
/// token up to the next option. An empty string is a value (`--tools ""`).
fn values(
    args: &[String],
    i: &mut usize,
    flag: &str,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<String> {
    let mut taken = Vec::new();
    while let Some(next) = args.get(*i + 1) {
        if next == "--" || (next.starts_with('-') && !next.is_empty()) {
            break;
        }
        taken.push(next.clone());
        *i += 1;
    }
    if taken.is_empty() {
        diagnostics.push(Diagnostic::error(format!("{flag} requires a value")));
    }
    taken
}

/// Recognise one Claude Code flag at `args[*i]`, leaving `*i` on its last
/// token; `false` when `args[*i]` is not one.
pub fn parse_cc_flag(
    args: &[String],
    i: &mut usize,
    flags: &mut CcFlags,
    diagnostics: &mut Vec<Diagnostic>,
) -> bool {
    let arg = args[*i].as_str();
    match arg {
        "--output-format" => {
            if let Some(text) = value(args, i, arg, diagnostics) {
                match OutputFormat::parse(&text) {
                    Some(format) => flags.output_format = Some(format),
                    None => diagnostics.push(Diagnostic::error(format!(
                        "option '--output-format <format>' argument '{text}' is invalid. Allowed choices are text, json, stream-json."
                    ))),
                }
            }
        }
        "--input-format" => {
            if let Some(text) = value(args, i, arg, diagnostics) {
                match text.as_str() {
                    "text" => flags.input_format = Some(InputFormat::Text),
                    "stream-json" => flags.input_format = Some(InputFormat::StreamJson),
                    _ => diagnostics.push(Diagnostic::error(format!(
                        "Invalid input format \"{text}\"."
                    ))),
                }
            }
        }
        "--include-partial-messages" => flags.include_partial_messages = true,
        "--replay-user-messages" => flags.replay_user_messages = true,
        "--include-hook-events" => flags.include_hook_events = true,
        "--json-schema" => {
            if let Some(text) = value(args, i, arg, diagnostics) {
                match serde_json::from_str::<serde_json::Value>(&text) {
                    Ok(schema @ serde_json::Value::Object(_)) => flags.json_schema = Some(schema),
                    Ok(_) => {
                        diagnostics.push(Diagnostic::error("--json-schema must be a JSON object"))
                    }
                    Err(error) => diagnostics.push(Diagnostic::error(format!(
                        "--json-schema is not valid JSON: {error}"
                    ))),
                }
            }
        }
        "--max-turns" => {
            if let Some(text) = value(args, i, arg, diagnostics) {
                match text.trim().parse::<u32>() {
                    Ok(turns) if turns > 0 => flags.max_turns = Some(turns),
                    _ => diagnostics
                        .push(Diagnostic::error("--max-turns must be a positive integer")),
                }
            }
        }
        "--max-budget-usd" => {
            if let Some(text) = value(args, i, arg, diagnostics) {
                match text.trim().parse::<f64>() {
                    Ok(amount) if amount.is_finite() && amount > 0.0 => {
                        flags.max_budget_usd = Some(amount);
                    }
                    _ => diagnostics.push(Diagnostic::error(
                        "--max-budget-usd must be a positive number greater than 0",
                    )),
                }
            }
        }
        "--allowedTools" | "--allowed-tools" => {
            for text in values(args, i, arg, diagnostics) {
                flags.allowed_tools.extend(split_tool_list(&text));
            }
        }
        "--disallowedTools" | "--disallowed-tools" => {
            for text in values(args, i, arg, diagnostics) {
                flags.disallowed_tools.extend(split_tool_list(&text));
            }
        }
        "--tools" => {
            let texts = values(args, i, arg, diagnostics);
            let tools = flags.tools.get_or_insert_with(Vec::new);
            for text in texts {
                tools.extend(split_tool_list(&text));
            }
        }
        "--permission-mode" => {
            if let Some(text) = value(args, i, arg, diagnostics) {
                flags.permission_mode = Some(text);
            }
        }
        "--dangerously-skip-permissions" => flags.dangerously_skip_permissions = true,
        "--allow-dangerously-skip-permissions" => {
            flags.allow_dangerously_skip_permissions = true;
        }
        "--permission-prompt-tool" => {
            flags.permission_prompt_tool = value(args, i, arg, diagnostics);
        }
        "--settings" => flags.settings = value(args, i, arg, diagnostics),
        "--setting-sources" => {
            // An empty list is a value: no settings files at all.
            match args.get(*i + 1) {
                Some(next) if !next.starts_with('-') => {
                    *i += 1;
                    flags.setting_sources = Some(next.clone());
                }
                _ => diagnostics.push(Diagnostic::error(format!("{arg} requires a value"))),
            }
        }
        "--mcp-config" => {
            let configs = values(args, i, arg, diagnostics);
            flags.mcp_config.extend(configs);
        }
        "--strict-mcp-config" => flags.strict_mcp_config = true,
        "--agents" => flags.agents = value(args, i, arg, diagnostics),
        "--agent" => flags.agent = value(args, i, arg, diagnostics),
        "--session-id" => flags.session_id = value(args, i, arg, diagnostics),
        "--fallback-model" => flags.fallback_model = value(args, i, arg, diagnostics),
        "--bare" => flags.bare = true,
        "--betas" => {
            for text in values(args, i, arg, diagnostics) {
                flags.betas.extend(
                    text.split(',')
                        .map(str::trim)
                        .filter(|beta| !beta.is_empty())
                        .map(str::to_string),
                );
            }
        }
        "--tmux" => flags.tmux = Some(TmuxMode::Native),
        "--tmux=classic" => flags.tmux = Some(TmuxMode::Classic),
        "--prefill" => flags.prefill = value(args, i, arg, diagnostics),
        "-d" | "--debug" => {
            // The filter is optional: the next token is taken unless it is
            // another option (Commander's `[filter]`).
            let filter = args
                .get(*i + 1)
                .filter(|next| !next.starts_with('-') && !next.starts_with('@'))
                .cloned();
            if filter.is_some() {
                *i += 1;
            }
            flags.debug = Some(filter);
        }
        "--debug-file" => flags.debug_file = value(args, i, arg, diagnostics),
        "--init" => flags.setup = Some(SetupTrigger::Init),
        "--init-only" => flags.setup = Some(SetupTrigger::InitOnly),
        "--maintenance" => flags.setup = Some(SetupTrigger::Maintenance),
        _ => return false,
    }
    true
}

/// The permission modes `--permission-mode` takes.
const PERMISSION_MODES: [&str; 5] = [
    "default",
    "acceptEdits",
    "bypassPermissions",
    "plan",
    "dontAsk",
];

/// Claude Code's combination rules, its wording (`Error: ` is the CLI's).
pub fn validate(flags: &CcFlags, print: bool, worktree: bool, diagnostics: &mut Vec<Diagnostic>) {
    let output = flags.output_format.unwrap_or(OutputFormat::Text);
    // A headless format is a print run whether or not `-p` was given (the
    // Agent SDK spawns the CLI with the formats alone).
    let print = print || flags.headless();
    let mut error = |message: &str| diagnostics.push(Diagnostic::error(message));
    if flags.input_format == Some(InputFormat::StreamJson) {
        if output != OutputFormat::StreamJson {
            error("--input-format=stream-json requires output-format=stream-json.");
        } else if !print {
            error("--input-format=stream-json requires --print.");
        }
    }
    if flags.include_partial_messages && (!print || output != OutputFormat::StreamJson) {
        error("--include-partial-messages requires --print and --output-format=stream-json.");
    }
    if flags.replay_user_messages
        && (flags.input_format != Some(InputFormat::StreamJson)
            || output != OutputFormat::StreamJson)
    {
        error(
            "--replay-user-messages requires both --input-format=stream-json and \
             --output-format=stream-json.",
        );
    }
    if flags.include_hook_events && output != OutputFormat::StreamJson {
        error("--include-hook-events requires --output-format=stream-json.");
    }
    if flags.tmux.is_some() && !worktree {
        error("--tmux requires --worktree");
    }
    if let Some(mode) = &flags.permission_mode {
        if !PERMISSION_MODES.contains(&mode.as_str()) {
            error(&format!(
                "Invalid permission mode '{mode}'. Valid modes: {}",
                PERMISSION_MODES.join(", ")
            ));
        }
    }
    if let Some(id) = &flags.session_id {
        if uuid::Uuid::try_parse(id).is_err() {
            error("Invalid session ID. Must be a valid UUID.");
        }
    }
    if let Some(sources) = &flags.setting_sources {
        if let Err(problem) = SettingSource::parse_list(sources) {
            error(&problem);
        }
    }
    if let Some(tool) = &flags.permission_prompt_tool {
        if tool != "stdio" && !tool.starts_with("mcp__") {
            error(&format!(
                "--permission-prompt-tool takes \"stdio\" or an MCP tool name (mcp__<server>__<tool>), not \"{tool}\""
            ));
        }
    }
}

/// The session-wide flags as the launch layer: `--settings` (with
/// `--fallback-model` folded in as `fallbackModels`), `--setting-sources`,
/// the permission flags, `--mcp-config` / `--strict-mcp-config`,
/// `--agents` / `--agent`, `--bare` and `--betas`. Files resolve against
/// `cwd`.
///
/// # Errors
///
/// A `--settings`, `--mcp-config` or `--agents` value that cannot be read or
/// parsed (Claude Code's wording where it has one).
pub fn launch_settings(flags: &CcFlags, cwd: &std::path::Path) -> Result<LaunchSettings, String> {
    let mut flag_settings = match &flags.settings {
        Some(value) => Some(
            read_json_object_arg("--settings", value, cwd).map_err(|problem| {
                if problem.contains("invalid JSON") && value.trim().starts_with('{') {
                    "Invalid JSON provided to --settings".to_string()
                } else {
                    problem
                }
            })?,
        ),
        None => None,
    };
    if let Some(models) = &flags.fallback_model {
        let chain: Vec<serde_json::Value> = models
            .split(',')
            .map(str::trim)
            .filter(|model| !model.is_empty())
            .map(|model| serde_json::Value::String(model.to_string()))
            .collect();
        flag_settings
            .get_or_insert_with(serde_json::Map::new)
            .insert("fallbackModels".into(), serde_json::Value::Array(chain));
    }
    let mut mcp_servers: Option<serde_json::Map<String, serde_json::Value>> = None;
    for config in &flags.mcp_config {
        let document = read_json_object_arg("--mcp-config", config, cwd)?;
        // Claude Code's file shape (`{"mcpServers": {...}}`), or the
        // servers object itself.
        let servers = match document.get("mcpServers") {
            Some(serde_json::Value::Object(servers)) => servers.clone(),
            Some(_) => {
                return Err(format!(
                    "--mcp-config: `mcpServers` must be an object in {config}"
                ));
            }
            None => document,
        };
        mcp_servers
            .get_or_insert_with(serde_json::Map::new)
            .extend(servers);
    }
    let agents = match &flags.agents {
        Some(value) => Some(
            read_json_object_arg("--agents", value, cwd)
                .map_err(|problem| format!("Invalid --agents configuration: {problem}"))?,
        ),
        None => None,
    };
    let mode = if flags.dangerously_skip_permissions {
        Some("bypassPermissions".to_string())
    } else {
        flags.permission_mode.clone()
    };
    Ok(LaunchSettings {
        flag_settings,
        setting_sources: match &flags.setting_sources {
            Some(sources) => Some(SettingSource::parse_list(sources)?),
            None => None,
        },
        permissions: CliPermissions {
            allowed_tools: flags.allowed_tools.clone(),
            disallowed_tools: flags.disallowed_tools.clone(),
            tools: flags.tools.clone(),
            mode,
            allow_bypass: flags.allow_dangerously_skip_permissions
                || flags.dangerously_skip_permissions,
        },
        mcp_servers,
        strict_mcp_config: flags.strict_mcp_config,
        agents,
        agent: flags.agent.clone(),
        bare: flags.bare,
        betas: flags.betas.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(list: &[&str]) -> crate::args::Args {
        let args: Vec<String> = list.iter().map(ToString::to_string).collect();
        crate::args::parse_args(&args)
    }

    fn errors(args: &crate::args::Args) -> Vec<String> {
        args.diagnostics
            .iter()
            .filter(|d| d.is_error)
            .map(|d| d.message.clone())
            .collect()
    }

    #[test]
    fn headless_flags_parse_in_both_spellings() {
        let parsed = parse(&[
            "-p",
            "--output-format=stream-json",
            "--input-format",
            "stream-json",
            "--include-partial-messages",
            "--replay-user-messages",
            "--max-turns",
            "3",
            "--max-budget-usd=0.5",
            "--json-schema",
            r#"{"type":"object"}"#,
        ]);
        assert_eq!(errors(&parsed), Vec::<String>::new());
        let cc = &parsed.cc;
        assert_eq!(
            (
                cc.output_format,
                cc.input_format,
                cc.include_partial_messages,
                cc.replay_user_messages,
                cc.max_turns,
                cc.max_budget_usd,
                cc.json_schema.clone(),
            ),
            (
                Some(OutputFormat::StreamJson),
                Some(InputFormat::StreamJson),
                true,
                true,
                Some(3),
                Some(0.5),
                Some(serde_json::json!({"type": "object"})),
            )
        );
        assert!(parsed.messages.is_empty());
    }

    #[test]
    fn combination_rules_use_claude_codes_wording() {
        assert_eq!(
            errors(&parse(&["-p", "--input-format", "stream-json", "hi"])),
            vec!["--input-format=stream-json requires output-format=stream-json."]
        );
        assert_eq!(
            errors(&parse(&[
                "--include-partial-messages",
                "--output-format",
                "json",
                "hi"
            ])),
            vec!["--include-partial-messages requires --print and --output-format=stream-json."]
        );
        assert_eq!(
            errors(&parse(&[
                "-p",
                "--output-format",
                "stream-json",
                "--replay-user-messages"
            ])),
            vec![
                "--replay-user-messages requires both --input-format=stream-json and \
                 --output-format=stream-json."
            ]
        );
        assert_eq!(
            errors(&parse(&["--tmux"])),
            vec!["--tmux requires --worktree"]
        );
        assert_eq!(
            errors(&parse(&["--permission-mode", "auto"])),
            vec![
                "Invalid permission mode 'auto'. Valid modes: default, acceptEdits, \
                 bypassPermissions, plan, dontAsk"
            ]
        );
        assert_eq!(
            errors(&parse(&["--session-id", "nope"])),
            vec!["Invalid session ID. Must be a valid UUID."]
        );
        assert_eq!(
            errors(&parse(&["--output-format", "yaml"])),
            vec![
                "option '--output-format <format>' argument 'yaml' is invalid. Allowed choices \
                 are text, json, stream-json."
            ]
        );
        assert_eq!(
            errors(&parse(&["--json-schema", "[1]"])),
            vec!["--json-schema must be a JSON object"]
        );
        assert_eq!(
            errors(&parse(&["--max-budget-usd", "0"])),
            vec!["--max-budget-usd must be a positive number greater than 0"]
        );
    }

    /// The tool lists are variadic (every token up to the next option) and
    /// split on commas and spaces outside parentheses.
    #[test]
    fn tool_lists_are_variadic_and_split() {
        let parsed = parse(&[
            "--allowedTools",
            "Bash(git log *), Edit",
            "Read",
            "--disallowed-tools=WebFetch",
            "--tools",
            "",
            "-p",
            "go",
        ]);
        assert_eq!(errors(&parsed), Vec::<String>::new());
        assert_eq!(
            parsed.cc.allowed_tools,
            vec!["Bash(git log *)", "Edit", "Read"]
        );
        assert_eq!(parsed.cc.disallowed_tools, vec!["WebFetch"]);
        assert_eq!(parsed.cc.tools, Some(Vec::new()));
        assert_eq!(parsed.messages, vec!["go"]);
    }

    #[test]
    fn launch_settings_fold_the_session_wide_flags() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("mcp.json"),
            r#"{"mcpServers": {"fs": {"command": "fs-server"}}}"#,
        )
        .unwrap();
        let parsed = parse(&[
            "--settings",
            r#"{"theme": "dark"}"#,
            "--setting-sources",
            "project,local",
            "--mcp-config",
            "mcp.json",
            r#"{"gh": {"type": "http", "url": "https://x"}}"#,
            "--strict-mcp-config",
            "--agents",
            r#"{"reviewer": {"description": "d", "prompt": "p"}}"#,
            "--agent",
            "reviewer",
            "--fallback-model",
            "a/b, c/d",
            "--dangerously-skip-permissions",
            "--betas",
            "x-1,x-2",
            "--bare",
        ]);
        assert_eq!(errors(&parsed), Vec::<String>::new());
        let launch = launch_settings(&parsed.cc, dir.path()).unwrap();
        assert_eq!(
            serde_json::to_value(&launch).unwrap(),
            serde_json::json!({
                "flagSettings": {"theme": "dark", "fallbackModels": ["a/b", "c/d"]},
                "settingSources": ["project", "local"],
                "permissions": {"mode": "bypassPermissions", "allowBypass": true},
                "mcpServers": {
                    "fs": {"command": "fs-server"},
                    "gh": {"type": "http", "url": "https://x"},
                },
                "strictMcpConfig": true,
                "agents": {"reviewer": {"description": "d", "prompt": "p"}},
                "agent": "reviewer",
                "bare": true,
                "betas": ["x-1", "x-2"],
            })
        );
        let bad = parse(&["--settings", "{nope"]);
        assert_eq!(
            launch_settings(&bad.cc, dir.path()),
            Err("Invalid JSON provided to --settings".to_string())
        );
    }

    #[test]
    fn small_launch_flags_parse() {
        let parsed = parse(&[
            "-w",
            "--tmux=classic",
            "--prefill",
            "draft",
            "--debug",
            "api,hooks",
            "--init-only",
        ]);
        assert_eq!(errors(&parsed), Vec::<String>::new());
        assert_eq!(parsed.cc.tmux, Some(TmuxMode::Classic));
        assert_eq!(parsed.cc.prefill.as_deref(), Some("draft"));
        assert_eq!(parsed.cc.debug, Some(Some("api,hooks".to_string())));
        assert_eq!(parsed.cc.setup, Some(SetupTrigger::InitOnly));
        assert_eq!(parse(&["-d", "--verbose"]).cc.debug, Some(None));
    }
}
