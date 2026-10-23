//! `prime-agent prompt`: dump the fully-assembled effective system prompt with
//! its per-layer breakdown, and the session context the first turn carries
//! as a hidden row, without starting a session or a provider.

use pa_core::resources::{ResourceLoaderOptions, load_resources};
use pa_core::settings::SettingsManager;

use crate::config::get_agent_dir;

/// The command's parsed arguments.
#[derive(Debug, Default, PartialEq, Eq)]
struct PromptCommandArgs {
    model: Option<String>,
    cwd: Option<String>,
    json: bool,
}

fn parse_args(args: &[String]) -> Result<PromptCommandArgs, String> {
    let mut parsed = PromptCommandArgs::default();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--json" => parsed.json = true,
            "--model" => {
                let Some(value) = rest.next() else {
                    return Err("--model requires a value".to_string());
                };
                parsed.model = Some(value.clone());
            }
            "--cwd" => {
                let Some(value) = rest.next() else {
                    return Err("--cwd requires a value".to_string());
                };
                parsed.cwd = Some(value.clone());
            }
            "--help" | "-h" => {
                return Err(String::new());
            }
            other => return Err(format!("unknown option {other:?}")),
        }
    }
    Ok(parsed)
}

/// Run the `prompt` command; returns the process exit code.
pub fn run_prompt_command(args: &[String]) -> i32 {
    let parsed = match parse_args(args) {
        Ok(parsed) => parsed,
        Err(help_request) if help_request.is_empty() => return 0,
        Err(error) => {
            eprintln!("Error: {error}");
            eprintln!("Run `prime-agent help prompt` for usage.");
            return 1;
        }
    };
    let cwd = parsed
        .cwd
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    match assemble_breakdown(&cwd, parsed.model.as_deref()) {
        Ok((breakdown, session_context, errors)) => {
            for error in &errors {
                eprintln!("Error: {error}");
            }
            if parsed.json {
                print_json(&breakdown, &session_context);
            } else {
                print_text(&breakdown, &session_context);
            }
            0
        }
        Err(error) => {
            eprintln!("Error: {error:#}");
            1
        }
    }
}

fn assemble_breakdown(
    cwd: &std::path::Path,
    model: Option<&str>,
) -> anyhow::Result<(pa_core::prompts::SystemPromptBreakdown, String, Vec<String>)> {
    let agent_dir = get_agent_dir();
    let model_prompts = pa_core::prompts::model_prompts::load_model_prompts(model, &agent_dir);
    let settings = SettingsManager::create(cwd, &agent_dir);
    // MCP gating: auth-gated built-in integrations drop their skills; enabled persistent generic
    // servers add the prompt MCP guidance.
    let user_servers = settings
        .settings()
        .mcp_servers
        .clone()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(server, config)| {
            serde_json::from_value(config)
                .ok()
                .map(|parsed| (server, parsed))
        })
        .collect::<std::collections::HashMap<String, pa_core::mcp::McpServerConfig>>();
    let (skill_overrides, generic_servers, _manager) =
        pa_core::mcp::McpManager::prompt_gating(user_servers, &agent_dir);
    let resources = load_resources(ResourceLoaderOptions {
        extra_builtin_skill_overrides: skill_overrides,
        system_prompt: None,
        // The dump shows a top-level session's prompt.
        auto_memory: true,
        ..ResourceLoaderOptions::new(cwd.to_path_buf(), agent_dir.clone())
    })?;
    let breakdown = pa_core::prompts::system_prompt::system_prompt_breakdown(
        &pa_core::prompts::BuildSystemPromptOptions {
            custom_prompt: resources.system_prompt.clone(),
            auto_memory: resources
                .auto_memory
                .as_ref()
                .map(pa_core::resources::auto_memory::memory_prompt_section),
            // The shipped model-tool surface: `python_repl` only; bash/edit are kernel-resident
            // programmatic tools.
            selected_tools: Some(vec![pa_types::ai::PYTHON_TOOL_NAME]),
            allow_recursion: Some(true),
            rlm_depth: Some(0),
            model_prompt_extras: model_prompts.extras.as_deref(),
            ..Default::default()
        },
    );
    // The first turn's session context row, as a session in `cwd` would send it.
    let source = pa_core::session_engine::context_source::ContextSourceConfig {
        cwd: std::sync::Arc::new(std::sync::Mutex::new(cwd.to_path_buf())),
        load_instruction_files: true,
        skills: resources.skills,
        tools: vec![pa_types::ai::PYTHON_TOOL_NAME.to_string()],
        mcp_servers: generic_servers,
        // A dump has no session file; the row states the value it uses.
        conversation_log: None,
        model: std::sync::Arc::default(),
        // The `sandbox` setting's environment line, as a session would state it.
        sandbox: pa_core::os_sandbox::SessionSandbox::resolve(&settings, None, cwd)
            .as_ref()
            .map(pa_core::os_sandbox::SessionSandbox::prompt_line),
        shell: pa_core::platform::shell::default_kernel_bash_shell(),
        scratch_directory: None,
        instruction_options: pa_core::resources::instructions::InstructionOptions {
            cwd: cwd.to_path_buf(),
            agent_dir: agent_dir.clone(),
            home_dir: pa_types::platform::home_dir(),
            claude_user_files: settings.get_claude_code_user_instructions(),
            additional_dirs: Vec::new(),
        },
        additional_directories: None,
        // A dump has no spawn host: no agent types.
        list_agent_types: false,
        agent_dir,
    };
    let session_context = (pa_core::session_engine::context_source::context_source(source))();
    Ok((
        breakdown,
        session_context.render_full(),
        model_prompts.errors,
    ))
}

fn print_text(breakdown: &pa_core::prompts::SystemPromptBreakdown, session_context: &str) {
    println!("# prime-agent system prompt breakdown");
    println!();
    println!("cached prefix (static layers, byte-stable across sessions):");
    for segment in &breakdown.segments {
        if segment.kind == pa_core::prompts::SegmentKind::Static {
            print_segment(segment);
        }
    }
    println!("dynamic tail (session-specific, appended after the cached prefix):");
    for segment in &breakdown.segments {
        if segment.kind == pa_core::prompts::SegmentKind::Dynamic {
            print_segment(segment);
        }
    }
    println!();
    println!(
        "cached prefix: {} of {} total chars; dynamic tail follows",
        breakdown.cached_prefix_len,
        breakdown.assembled.len()
    );
    println!();
    println!("{}", breakdown.assembled);
    println!();
    println!(
        "# session context (the first turn's hidden row, {} chars; later turns carry only changes)",
        session_context.chars().count()
    );
    println!();
    println!("{session_context}");
}

fn print_segment(segment: &pa_core::prompts::PromptSegment) {
    println!(
        "  {:<20} {:<38} {:>7} chars",
        segment.name,
        segment.source,
        segment.text.chars().count()
    );
}

fn print_json(breakdown: &pa_core::prompts::SystemPromptBreakdown, session_context: &str) {
    let segments = breakdown
        .segments
        .iter()
        .map(|segment| {
            serde_json::json!({
                "name": segment.name,
                "kind": if segment.kind == pa_core::prompts::SegmentKind::Static { "static" } else { "dynamic" },
                "source": segment.source,
                "chars": segment.text.chars().count(),
            })
        })
        .collect::<Vec<_>>();
    println!(
        "{}",
        serde_json::json!({
            "segments": segments,
            "cachedPrefixLen": breakdown.cached_prefix_len,
            "prompt": breakdown.assembled,
            "sessionContext": session_context,
        })
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flags() {
        let parsed = parse_args(&[
            "--model".to_string(),
            "mock/mock-1".to_string(),
            "--json".to_string(),
            "--cwd".to_string(),
            "/w".to_string(),
        ])
        .unwrap();
        assert_eq!(parsed.model.as_deref(), Some("mock/mock-1"));
        assert_eq!(parsed.cwd.as_deref(), Some("/w"));
        assert!(parsed.json);
        assert_eq!(
            parse_args(&["--bogus".to_string()]),
            Err("unknown option \"--bogus\"".to_string())
        );
        assert_eq!(
            parse_args(&["--model".to_string()]),
            Err("--model requires a value".to_string())
        );
    }

    #[test]
    fn help_request_prints_usage() {
        assert_eq!(run_prompt_command(&["--help".to_string()]), 0);
    }

    #[test]
    fn assembles_the_layered_prompt_for_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        let (breakdown, session_context, errors) =
            assemble_breakdown(dir.path(), Some("mock/mock-1")).unwrap();
        assert!(errors.is_empty());
        assert!(breakdown.assembled.starts_with("# prime-agent harness"));
        assert!(
            breakdown
                .assembled
                .contains("Recursive agent depth: 0 (root)")
        );
        // The environment is the first turn's row, not prompt text.
        assert!(!breakdown.assembled.contains("Conversation log:"));
        assert!(session_context.contains("Conversation log: not persisted"));
        assert!(session_context.contains("<available_skills>"));
        // Every static segment sits inside the cached prefix.
        for segment in &breakdown.segments {
            let inside = breakdown
                .assembled
                .find(&segment.text)
                .is_some_and(|at| at < breakdown.cached_prefix_len);
            assert_eq!(
                inside,
                segment.kind == pa_core::prompts::SegmentKind::Static,
                "segment {} on the wrong side of the cache boundary",
                segment.name
            );
        }
    }
}
