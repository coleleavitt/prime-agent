// Pedantic-gate dispositions as src/lib.rs (large_futures/too_many_lines/casts).
#![allow(
    clippy::large_futures,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]

//! Prompt guard tests: cache safety (static layers never contain dynamic content; the cached prefix
//! is byte-identical across sessions) and tool surface (the core layer's documented tools match the
//! real registered surface; a tool change without a prompt update fails here).

use std::collections::BTreeSet;
use std::path::Path;

use pa_core::prompts::layers::{self, CORE_LAYER, OPINIONATED_LAYER, PER_MODEL_MAP, USAGE_LAYER};
use pa_core::prompts::system_prompt::{
    system_prompt_breakdown, BuildSystemPromptOptions, SegmentKind,
};
use pa_core::skills::load_skills_from_dir;

/// The workspace bundled skills directory (source-checkout layout):
/// pa-core lives at `<root>/crates/pa-core`.
fn bundled_skills_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .join("skills")
}

fn sorted_bundled_skills() -> Vec<pa_core::skills::Skill> {
    let skills_dir = bundled_skills_dir();
    let mut loaded = load_skills_from_dir(&skills_dir, "package");
    loaded
        .skills
        .sort_by(|left, right| left.name.cmp(&right.name));
    loaded.skills
}

// Cache safety

/// Strings that only dynamic segments generate. If one of these ever appears in a static layer
/// file, the cached prefix has started leaking per-session content.
const DYNAMIC_ONLY_MARKERS: &[&str] = &[
    "Working directory:",
    "Conversation log:",
    "Recursive agent depth:",
    "Current date:",
    "<available_skills>",
    "Enabled generic MCP servers:",
    "# Project Context",
    "# Additional Guidance",
    "Pre-installed Python packages:",
    "You are a child agent spawned by",
];

#[test]
fn static_layers_contain_no_dynamic_content() {
    for (name, text) in [
        ("core", CORE_LAYER),
        ("usage", USAGE_LAYER),
        ("opinionated", OPINIONATED_LAYER),
        ("per-model", PER_MODEL_MAP),
    ] {
        for marker in DYNAMIC_ONLY_MARKERS {
            assert!(
                !text.contains(marker),
                "static layer {name} contains dynamic-only marker {marker:?}: \
                 dynamic content must stay in the tail, not the cacheable prefix"
            );
        }
    }
}

#[test]
fn cached_prefix_is_stable_across_sessions() {
    let mut options = BuildSystemPromptOptions {
        cwd: "/first/cwd".to_string(),
        messages_path: Some("/first/session.jsonl".to_string()),
        model: Some("mock/mock-1"),
        skills: sorted_bundled_skills(),
        selected_tools: Some(vec!["ipython"]),
        ..Default::default()
    };
    let first = system_prompt_breakdown(&options);

    options.cwd = "/completely/different".into();
    options.messages_path = None;
    options.rlm_depth = Some(3);
    options.rlm_parent_agent = Some("orchestrator");
    options.generic_mcp_servers = vec!["slack".into(), "github".into()];
    options.context_files = vec![("AGENTS.md".into(), "Never commit main.".into())];
    options.prompt_guidelines = Some(vec!["prefer tabs".into()]);
    options.append_system_prompt = Some("And one more thing.".into());
    let second = system_prompt_breakdown(&options);

    assert_eq!(first.cached_prefix_len, second.cached_prefix_len);
    assert_eq!(
        &first.assembled[..first.cached_prefix_len],
        &second.assembled[..second.cached_prefix_len]
    );
    assert_eq!(
        &first.assembled[..first.cached_prefix_len],
        layers::static_prefix(Some("mock/mock-1"))
    );

    let first_tail = &first.assembled[first.cached_prefix_len..];
    assert!(first_tail.contains("Current date: "));
    assert!(first_tail.contains("Working directory: /first/cwd"));
    assert!(first_tail.contains("Recursive agent depth: 0 (root)"));
    assert!(first_tail.contains("<available_skills>"));
    assert!(first_tail.contains("Pre-installed Python packages:"));
    let second_tail = &second.assembled[second.cached_prefix_len..];
    assert!(second_tail.contains("/completely/different"));
    assert!(second_tail.contains("Enabled generic MCP servers: `github`, `slack`."));
    assert!(second_tail.contains("Recursive agent depth: 3 (not root)"));
    assert!(second_tail.contains("You are a child agent spawned by orchestrator."));
    assert!(second_tail.contains("# Project Context"));
    assert!(second_tail.contains("# Additional Guidance"));
    assert!(second_tail.ends_with("And one more thing."));

    for segment in &first.segments {
        let inside = first
            .assembled
            .find(&segment.text)
            .is_some_and(|at| at < first.cached_prefix_len);
        assert_eq!(
            inside,
            segment.kind == SegmentKind::Static,
            "segment {} on the wrong side of the cache boundary",
            segment.name
        );
    }
}

// Tool surface

/// Host-request types that are host-internal plumbing, not model-facing
/// programmatic tools (the prompt must not document them). The mcp
/// sidecar's and turn-boundary runtime's requests are never model-called;
/// `bash.consumed` is the kernel's own notice that a cell read a background
/// command's result, shipped ahead of that cell's `done`. `telemetry.emit`
/// IS reachable from model-authored kernel code (`host_request` is
/// importable), so its classification is "restricted to a fixed skill event
/// vocabulary with typed properties" — the handler's allowlist admits only
/// the two computer-use events and refuses everything else — which keeps it
/// out of the prompt's documented programmatic surface. The daemon
/// worker's `bash.completed` (the background-command completion notice)
/// and `bash.progress` (`rlm.watch.job`'s own poller) are runtime plumbing,
/// and `vision.read` is the bundled `attach_image` skill's own request.
/// The `harness.*` requests are the transport of `rlm.harness` (documented
/// through `HARNESS_TOKENS`). The `mcp.session.*` / `mcp.integration.*` requests carry the documented
/// kernel-local `mcp.list_tools` / `call_tool` / ... calls (and
/// `rlm.McpIntegration`) to the host-owned connections. The
/// `factory.*` requests are the bundled `factory` skill's client, the
/// `computer_use.*` requests the bundled `computer-use` skill's.
const INTERNAL_HOST_REQUESTS: &[&str] = &[
    "model.info",
    "mcp.config",
    "mcp.refresh",
    "mcp.begin_login",
    "bash.consumed",
    "telemetry.emit",
    "bash.completed",
    "bash.progress",
    "vision.read",
    "harness.load",
    "harness.save",
    "harness.get",
    "harness.list",
    "harness.search",
    "harness.overview",
    "harness.snapshot",
    "harness.upsert",
    "harness.create",
    "harness.update",
    "harness.delete",
    "harness.set_enabled",
    "harness.record_refinement",
    "harness.create_skill",
    "harness.update_skill",
    "harness.factory",
    "mcp.session.list_tools",
    "mcp.session.call_tool",
    "mcp.session.describe_tool",
    "mcp.session.search_tools",
    "mcp.session.reload",
    "mcp.session.close",
    "mcp.integration.list_tools",
    "mcp.integration.call_tool",
    // The kernel's factory client: `rlm.factory` is the bundled `factory`
    // skill's surface (documented by its SKILL.md and `rlm.factory.help()`),
    // `factory.spec` is the validator behind `rlm.harness` factory writes,
    // and `factory.library` the machine library behind the `rlm.factory`
    // MACHINE.md functions.
    "factory.spec",
    "factory.library",
    "factory.run",
    "factory.status",
    "factory.stop",
    "factory.resume",
    "factory.graph",
    "factory.watch",
    "factory.machine",
    // The kernel's `bash()` client: the host checks and runs the commands
    // (`pa_bash::REQUEST_TYPES`); the model-facing surface is `bash()`.
    "bash.check",
    "bash.isDestructiveGitDiscard",
    "bash.shell",
    "bash.childEnv",
    "bash.run",
    "bash.follow",
    "bash.output",
    "bash.kill",
    "bash.confirmExit",
    "bash.groupAlive",
    "bash.killAll",
    "bash.inventory",
    "bash.activity",
    // The bundled computer-use skill's client: the model calls the
    // `computer_use` module (documented as a bundled Python skill), whose
    // functions and `App` methods ride these five requests.
    "computer_use.get_state",
    "computer_use.list_apps",
    "computer_use.permissions_status",
    "computer_use.get_app",
    "computer_use.app",
];

/// Map one registered host-request type to the prompt token that documents
/// it. `None` when the request is host-internal.
fn prompt_token_for_host_request(request: &str) -> Option<String> {
    if INTERNAL_HOST_REQUESTS.contains(&request) {
        return None;
    }
    Some(
        match request {
            "rlm.run" => "rlm.spawn",
            "rlm.progress.note" => "rlm.progress_note",
            "agent_observe.list" | "agent_message.list_agents" => "agent_observe.list_agents",
            "agent_observe.get" => "agent_observe.get_agent",
            "agent_observe.recent" => "agent_observe.recent_messages",
            // The bundled present-artifact skill's request.
            "artifact.present" => "present_artifact",
            other => other,
        }
        .to_string(),
    )
}

/// Kernel-side programmatic tools implemented by the vendored `prime-agent-runtime` package
/// (generic MCP calls). Update together with the runtime sidecar.
const KERNEL_LOCAL_TOKENS: &[&str] = &[
    "mcp.list_tools",
    "mcp.call_tool",
    "mcp.search_tools",
    "mcp.describe_tool",
];

/// The harness-state API of the vendored runtime (`rlm.harness` CRUD and
/// `rlm.get_harness_state`). Update together with the runtime sidecar.
const HARNESS_TOKENS: &[&str] = &[
    "rlm.harness.create_memory",
    "rlm.harness.update_memory",
    "rlm.harness.delete_memory",
    "rlm.harness.create_prompt_note",
    "rlm.harness.update_prompt_note",
    "rlm.harness.delete_prompt_note",
    "rlm.harness.create_skill",
    "rlm.harness.update_skill",
    "rlm.harness.delete_skill",
    "rlm.harness.create_subagent",
    "rlm.harness.update_subagent",
    "rlm.harness.delete_subagent",
    "rlm.harness.set_enabled",
    "rlm.harness.record_refinement",
    "rlm.harness.plan_refinement",
    "rlm.harness.overview",
    "rlm.harness.search",
    "rlm.get_harness_state",
];

/// A host-request type literal: lowercase dotted name (`rlm.collect`).
fn is_request_type_literal(literal: &str) -> bool {
    !literal.is_empty()
        && literal
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_lowercase())
        && literal
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '.')
        && literal.contains('.')
}

/// Scan the pa-core and pa-daemon sources for every host-request type the
/// session engine and the daemon worker register (the worker owns the
/// swarm handlers: `rlm.inbox.*`, `rlm.watch.*`, `rlm.messaging_stats`):
/// `handlers.register("<type>", ...)` literals plus
/// `for request_type in [ "<type>", ... ]` loop tables.
fn registered_host_requests() -> BTreeSet<String> {
    let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut found = BTreeSet::new();
    let mut stack = vec![
        crate_root.join("src"),
        crate_root
            .parent()
            .expect("crates dir")
            .join("pa-daemon")
            .join("src"),
    ];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("read source");
            // Direct registrations: the first string literal after each
            // `.register(` (the request type comes first in the call).
            for remainder in source.split(".register(").skip(1) {
                if let Some(literal) = first_string_literal(remainder) {
                    if is_request_type_literal(&literal) {
                        found.insert(literal);
                    }
                }
            }
            // Loop tables: `for request_type in [ "a.b", ... ]`.
            for remainder in source.split("for request_type in").skip(1) {
                let Some(open) = remainder.find('[') else {
                    continue;
                };
                let Some(close) = remainder[open..].find(']') else {
                    continue;
                };
                for literal in all_string_literals(&remainder[open..open + close]) {
                    if is_request_type_literal(&literal) {
                        found.insert(literal);
                    }
                }
            }
        }
    }
    found
}

fn first_string_literal(text: &str) -> Option<String> {
    let open = text.find('"')?;
    let tail = &text[open + 1..];
    let close = tail.find('"')?;
    Some(tail[..close].to_string())
}

fn all_string_literals(text: &str) -> Vec<String> {
    let mut literals = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('"') {
        let tail = &rest[open + 1..];
        match tail.find('"') {
            Some(close) => {
                literals.push(tail[..close].to_string());
                rest = &tail[close + 1..];
            }
            None => break,
        }
    }
    literals
}

/// Kernel-bound REPL names from the real bootstrap code (`rlm`, `bash`,
/// `mcp`), derived from `build_rlm_bootstrap_code`.
fn kernel_bound_names() -> BTreeSet<String> {
    let code = pa_core::kernel::bootstrap::build_rlm_bootstrap_code(&[]);
    let mut names = BTreeSet::new();
    for line in code.lines() {
        let line = line.trim_start();
        if line.starts_with("rlm = ") {
            names.insert("rlm".to_string());
        }
        if line.starts_with("bash = ") {
            names.insert("bash".to_string());
        }
        if let Some(rest) = line.strip_prefix("import rlm.mcp as ") {
            names.insert(rest.trim().to_string());
        }
    }
    names
}

/// One bundled Python skill's public module-level functions (column-zero
/// `def`/`async def` only, so nested helpers stay out).
fn python_skill_functions(package_path: &Path) -> Vec<String> {
    let mut functions = Vec::new();
    let mut stack = vec![package_path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) != Some("py") {
                continue;
            }
            let Ok(source) = std::fs::read_to_string(&path) else {
                continue;
            };
            for line in source.lines() {
                let after = line
                    .strip_prefix("async def ")
                    .or_else(|| line.strip_prefix("def "));
                if let Some(signature) = after {
                    if let Some(name) = signature.split(['(', ':']).next() {
                        functions.push(name.trim().to_string());
                    }
                }
            }
        }
    }
    functions
}

/// Bundled skills that are auth-gated builtin MCP integrations: they are disabled in sessions whose
/// user is not logged into the integration, so the prompt documents them only through the dynamic
/// skills inventory (and its generic "additional skills may exist" note), not as API surface.
fn auth_gated_bundled_skills() -> BTreeSet<String> {
    pa_core::mcp::BUILTIN_MCP_CATALOG
        .iter()
        .map(|(server, _, _)| server.to_string())
        .collect()
}

/// (import name, public functions) for every bundled Python skill that is
/// always available (auth-gated integrations excluded).
fn bundled_python_skills() -> Vec<(String, Vec<String>)> {
    let gated = auth_gated_bundled_skills();
    sorted_bundled_skills()
        .into_iter()
        .filter(|skill| {
            skill
                .python
                .as_ref()
                .is_none_or(|python| !gated.contains(&python.import_name))
        })
        .filter_map(|skill| {
            let python = skill.python?;
            // `package_path` is the install root (where pyproject.toml
            // lives); the model-facing surface is the import package, so a
            // skill that also ships tests at its root is not scanned.
            let package_dir = python.package_path.join("src").join(&python.import_name);
            let scan_root = if package_dir.is_dir() {
                package_dir
            } else {
                python.package_path.clone()
            };
            let mut functions = python_skill_functions(&scan_root)
                .into_iter()
                .filter(|name| !name.starts_with('_'))
                .collect::<Vec<_>>();
            functions.sort();
            Some((python.import_name, functions))
        })
        .collect()
}

/// The surface tokens the prompt is allowed to document: kernel-bound names, registered host
/// requests, bundled Python skills, and the runtime-package (harness + generic MCP) API.
fn allowed_surface_tokens() -> BTreeSet<String> {
    let mut allowed = BTreeSet::new();
    allowed.extend(kernel_bound_names());
    for request in registered_host_requests() {
        if let Some(token) = prompt_token_for_host_request(&request) {
            allowed.insert(token);
        }
    }
    for (import, functions) in bundled_python_skills() {
        allowed.insert(import.clone());
        for function in functions {
            allowed.insert(format!("{import}.{function}"));
        }
    }
    for token in KERNEL_LOCAL_TOKENS.iter().chain(HARNESS_TOKENS) {
        allowed.insert(token.to_string());
    }
    allowed
}

/// The surface tokens the prompt MUST document. Stricter than the allowed set: skill entry points
/// (`run`/`status`) are callable through the module itself, so the module bullet suffices for them.
fn required_surface_tokens() -> BTreeSet<String> {
    let mut required = BTreeSet::new();
    for request in registered_host_requests() {
        if let Some(token) = prompt_token_for_host_request(&request) {
            required.insert(token);
        }
    }
    for (import, functions) in bundled_python_skills() {
        required.insert(import.clone());
        for function in functions {
            if function == "run" || function == "status" {
                continue;
            }
            required.insert(format!("{import}.{function}"));
        }
    }
    for token in KERNEL_LOCAL_TOKENS.iter().chain(HARNESS_TOKENS) {
        required.insert(token.to_string());
    }
    required
}

/// Surface tokens documented in the core layer: bullet entries whose first
/// backtick token is a lowercase module path.
fn documented_surface_tokens() -> BTreeSet<String> {
    let mut documented = BTreeSet::new();
    for line in CORE_LAYER.lines() {
        let Some(rest) = line.trim_start().strip_prefix("- `") else {
            continue;
        };
        let Some(end) = rest.find('`') else {
            continue;
        };
        let token = rest[..end].split('(').next().unwrap_or_default().trim();
        let valid = token
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_lowercase())
            && token
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '.')
            && !token.is_empty();
        if valid {
            documented.insert(token.to_string());
        }
    }
    documented
}

#[test]
fn core_layer_documents_the_real_tool_surface() {
    let documented = documented_surface_tokens();
    let allowed = allowed_surface_tokens();
    let required = required_surface_tokens();

    assert!(
        !required.is_empty() && !documented.is_empty(),
        "surface extraction produced nothing; the guard would be vacuous"
    );

    for token in &documented {
        let root = token.split('.').next().expect("non-empty token");
        let known = allowed.contains(token)
            || allowed.contains(root)
            || allowed
                .iter()
                .any(|candidate| candidate.starts_with(&format!("{root}.")));
        assert!(
            known,
            "prompt documents unknown tool {token:?}: a tool-surface change must \
             update prompts/layers/core.md (and this test's token mapping)"
        );
    }

    for token in &required {
        let documented_form = documented.contains(token)
            || documented
                .iter()
                .any(|entry| entry.starts_with(&format!("{token}.")))
            || documented
                .iter()
                .any(|entry| entry.starts_with(token.as_str()) && entry.contains('.'));
        assert!(
            documented_form,
            "tool {token:?} is on the registered surface but the prompt does not \
             document it: update prompts/layers/core.md"
        );
    }
}

/// The keyword-only parameter names of one `(..., *, a: T = x, b: U) -> R`
/// signature (names only; annotations and defaults dropped).
fn keyword_only_parameters(signature: &str) -> BTreeSet<String> {
    let Some((_, after_star)) = signature.split_once('*') else {
        return BTreeSet::new();
    };
    let parameters = after_star.split(')').next().unwrap_or_default();
    parameters
        .split(',')
        .filter_map(|parameter| {
            let name = parameter.split(':').next()?.split('=').next()?.trim();
            (!name.is_empty()).then(|| name.to_string())
        })
        .collect()
}

/// `rlm.spawn`'s documented keyword arguments are real on both sides of the
/// bridge: the kernel runtime's `spawn` takes each one and the host's
/// `rlm.run` kwarg allowlist admits each one (a kwarg the prompt teaches
/// but the host rejects fails every spawn that uses it).
#[test]
fn documented_spawn_kwargs_reach_the_runtime_and_the_host() {
    let documented_line = CORE_LAYER
        .lines()
        .find(|line| line.trim_start().starts_with("- `rlm.spawn("))
        .expect("the core layer documents rlm.spawn");
    let documented = keyword_only_parameters(documented_line);
    assert!(
        documented.contains("name") && documented.contains("token_budget"),
        "rlm.spawn's documented kwargs: {documented:?}"
    );

    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root");
    let runtime = std::fs::read_to_string(root.join("prime-agent-runtime/src/rlm/__init__.py"))
        .expect("read the kernel runtime");
    let runtime_signature = runtime
        .split("\nasync def spawn(")
        .nth(1)
        .and_then(|rest| rest.split(") -> RLMSpawnHandle").next())
        .expect("the runtime defines a module-level spawn");
    let runtime_parameters = keyword_only_parameters(&format!("{runtime_signature})"));

    let host = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/session_engine/rlm_host.rs"),
    )
    .expect("read the rlm host");
    let allowlist = host
        .split("fn spawn_request_from_payload(")
        .nth(1)
        .and_then(|rest| rest.split("reject_unsupported_kwargs(").nth(1))
        .and_then(|rest| rest.split_once("&[").map(|(_, tail)| tail))
        .and_then(|rest| rest.split(']').next())
        .expect("the rlm.run handler names its kwarg allowlist");
    let host_kwargs: BTreeSet<String> = all_string_literals(allowlist).into_iter().collect();

    for kwarg in &documented {
        assert!(
            runtime_parameters.contains(kwarg),
            "the prompt documents rlm.spawn({kwarg}=) but the kernel runtime's spawn does not take it: {runtime_parameters:?}"
        );
        assert!(
            host_kwargs.contains(kwarg),
            "the prompt documents rlm.spawn({kwarg}=) but the host rejects it: {host_kwargs:?}"
        );
    }
}

// Packaged-set parity (TS packages/coding-agent/skills)

/// The packaged skill set at TS tip `f62dae4d0`
/// (`packages/coding-agent/skills/`): 12 skills — the generic `mcp` doc
/// skill in, the per-service linear/notion pair out (TS removed theirs
/// when the generic MCP surface landed). `system-router` is the one
/// addition since: the System 1 / System 2 harness router (TS PR #2484)
/// ported as a bundled kernel skill.
const TS_PACKAGED_SKILL_SET: &[&str] = &[
    "agent-message",
    "agent-observe",
    "attach-image",
    "compact",
    "edit",
    "goal",
    "mcp",
    "prime-intellect",
    "refine",
    "rlm-heartbeat",
    "skill-creator",
    "system-router",
    // The fork's offline Engineer Trajectory Index backfill script (TS
    // `perf/session-catalog-resume`), read by `prime-agent learning trajectory
    // --include-backfill` (pa-learning). Model-invisible
    // (`disable-model-invocation`): it adds no prompt text.
    "trajectory-backfill",
    "websearch",
];

/// The bundled skills directory matches the TS packaged set name-for-name:
/// the generic `mcp` skill is present and markdown-only, and the retired
/// per-service pair (linear/notion) is gone.
/// Bundled skills with no TS counterpart: deliberate net-new features that
/// the packaged-set parity below must still declare name-for-name. Every
/// entry needs a justification here; adding one without a TS counterpart
/// is a surface decision, not silent drift.
const NET_NEW_BUNDLED_SKILLS: &[&str] = &[
    // PR #3226: computer use - the TS product has no computer-use feature,
    // so parity is not applicable; this is the declared net-new exception.
    "computer-use",
    // Upstream #1062 (closed upstream, not in the TS v0.9.8 package):
    // `present_artifact()`, the user-visible inline preview over the
    // `artifact.present` host request.
    "present-artifact",
];

#[test]
fn bundled_skills_match_the_ts_packaged_set() {
    let skills = sorted_bundled_skills();
    let names: Vec<&str> = skills.iter().map(|skill| skill.name.as_str()).collect();
    let mut expected: Vec<&str> = TS_PACKAGED_SKILL_SET
        .iter()
        .chain(NET_NEW_BUNDLED_SKILLS.iter())
        .copied()
        .collect();
    expected.sort_unstable();
    assert_eq!(
        names, expected,
        "the packaged skill set must match the TS packaged set"
    );
    assert!(
        !names.contains(&"linear") && !names.contains(&"notion"),
        "the per-service MCP skills retired with the generic mcp skill"
    );
    let mcp_skill = skills
        .iter()
        .find(|skill| skill.name == "mcp")
        .expect("the generic mcp skill is packaged");
    assert!(
        mcp_skill.python.is_none(),
        "the mcp skill is documentation for the pre-imported runtime module, not a Python package"
    );
    assert_eq!(
        mcp_skill.description,
        "Use external MCP services generically from Python - search the supported-service catalog, inspect the user's connections, discover live tool schemas, and call tools on any connection (Notion, Linear, Slack, and the rest of the catalog) without per-service packages."
    );
}

/// The generic mcp skill loads through the normal markdown discovery and renders in the
/// `<available_skills>` inventory exactly like any other markdown skill: the `[skill name
/// location]`-style XML the TS `formatSkillsForPrompt` emits, without a `python_import` line.
#[test]
fn generic_mcp_skill_renders_in_the_prompt_inventory() {
    let mut options = BuildSystemPromptOptions {
        cwd: "/w".to_string(),
        messages_path: Some("/log.jsonl".to_string()),
        model: Some("mock/mock-1"),
        skills: sorted_bundled_skills(),
        selected_tools: Some(vec!["ipython"]),
        ..Default::default()
    };
    let breakdown = system_prompt_breakdown(&options);
    let inventory = breakdown
        .segments
        .iter()
        .find(|segment| {
            matches!(segment.kind, SegmentKind::Dynamic)
                && segment.text.contains("<available_skills>")
        })
        .map(|segment| segment.text.clone())
        .expect("the skills inventory segment renders");
    assert!(inventory.contains("<name>mcp</name>"));
    assert!(inventory.contains("<type>markdown</type>"));
    let mcp_block_start = inventory
        .find("<name>mcp</name>")
        .expect("the mcp skill block");
    let mcp_block_end = inventory[mcp_block_start..]
        .find("</skill>")
        .map_or(inventory.len(), |end| mcp_block_start + end);
    let mcp_block = &inventory[mcp_block_start..mcp_block_end];
    assert!(
        !mcp_block.contains("python_import"),
        "a markdown skill never carries a python_import line"
    );
    options.generic_mcp_servers = vec!["notion".into()];
    let second = system_prompt_breakdown(&options);
    assert!(second
        .assembled
        .contains("await mcp.list_tools(\"notion\")"));
}

// Image input

/// A text-only session model reads images through the configured image model
/// (`vision.read`): no model-facing text may still claim `attach_image` simply
/// fails there, and each must name the condition (a vision-capable `imageModel`).
#[test]
fn attach_image_texts_describe_the_image_model_path() {
    let core_line = CORE_LAYER
        .lines()
        .find(|line| line.starts_with("- `attach_image("))
        .expect("the core layer documents attach_image");
    assert_eq!(
        core_line,
        "- `attach_image(*paths: str) -> str`: loads images directly into context if the agent's \
         model is vision-capable; on a text-only model, a configured vision-capable `imageModel` \
         reads them and returns its text description (errors when none is configured)"
    );

    let skill = sorted_bundled_skills()
        .into_iter()
        .find(|skill| skill.name == "attach-image")
        .expect("attach-image is bundled");
    assert!(
        !skill.description.contains("errors clearly otherwise"),
        "attach-image description still claims text-only models fail: {}",
        skill.description
    );
    assert!(
        skill.description.contains("imageModel"),
        "attach-image description names the image-model path: {}",
        skill.description
    );

    let mut options = BuildSystemPromptOptions {
        cwd: "/w".to_string(),
        vision_capable: Some(false),
        ..Default::default()
    };
    let text_only = system_prompt_breakdown(&options).assembled;
    assert!(
        text_only.contains(
            "Image input: this model cannot see images; `attach_image` has the configured \
             vision-capable `imageModel` read them and returns its text description (it errors \
             when no such image model is configured)."
        ),
        "text-only environment line: {text_only}"
    );
    options.vision_capable = Some(true);
    assert!(system_prompt_breakdown(&options).assembled.contains(
        "Image input: this model can see images; `attach_image` loads them into context."
    ));
}

/// The host serves every kernel `bash()` request natively; each one is
/// host-internal (the model-facing surface is `bash()` itself).
#[test]
fn every_bash_host_request_is_listed_as_internal() {
    let missing: Vec<&str> = pa_bash::REQUEST_TYPES
        .iter()
        .copied()
        .filter(|request| !INTERNAL_HOST_REQUESTS.contains(request))
        .collect();
    assert_eq!(missing, Vec::<&str>::new());
}
