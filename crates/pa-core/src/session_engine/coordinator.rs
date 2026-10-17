//! Coordinator mode: the root session only coordinates workers.
//!
//! Claude Code 2.1.296's coordinator mode (`CLAUDE_CODE_COORDINATOR_MODE`,
//! `mddy60q9:xa`; unreleased) gives the main agent a coordinator system
//! prompt (`tajfxccb:pzs`) in place of the default one, filters its tools to
//! orchestration (`eab73rj3:zHs`: `Agent`, `SendMessage`, `TaskStop`, `Workflow`,
//! the read-only Skill tool, cross-session messaging, plus
//! `CLAUDE_CODE_COORDINATOR_EXTRA_TOOLS`), lists what workers can use
//! (`tajfxccb:uzs`, the `coordinator_context` row), refuses forks that keep
//! the parent running (`COORDINATOR_FORK_REFUSAL`), and with its flags makes
//! workers inherit the session model and hands the nested `CLAUDE.md` files
//! a worker loaded back to the coordinator (`propagateNestedMemory`).
//!
//! Here the model's tool is the kernel, so the mode is (all for the root
//! session only, never for an agent-definition session or a child):
//!
//! - the coordinator section appended to the system prompt
//!   ([`coordinator_section`]; the core layers stay, since they document the
//!   REPL the coordinator orchestrates through);
//! - a host-request gate ([`coordinator_gate`]) refusing the requests that do
//!   work instead of orchestrating it (shell commands, file edits, web, MCP
//!   tool calls, computer use, artifacts, worktree moves), unless
//!   `coordinatorExtraTools` lets one through; like an agent's tool scope it
//!   cannot stop plain Python from reading or writing files, so the prompt
//!   states the rule and the gate refuses what the harness can see;
//! - `coordinatorForceWorkerInheritModel`: `rlm.spawn` ignores `model=`;
//! - `coordinatorPropagateNestedMemory`: the nested instruction files a
//!   worker's session delivered reach the coordinator at its next boundary
//!   ([`NestedMemoryPropagation`]);
//! - the `coordinator` feature status, which the TUI reads to refuse a
//!   `/fork` that keeps the original running.

use std::fmt::Write as _;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use pa_types::sync::MutexExt;
use serde_json::Value;

use crate::kernel::shared::{HostRequestGate, HostRequestPolicy};
use crate::settings::experimental::ExperimentalSettings;

/// The feature status a coordinator session publishes (`{"enabled": true}`).
pub const COORDINATOR_FEATURE: &str = "coordinator";

/// The status line the agents view shows for a coordinator session.
pub const COORDINATOR_STATUS_LINE: &str = "coordinator mode";

/// One coordinator session's knobs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CoordinatorConfig {
    /// `coordinatorExtraTools`: Claude Code tool names (`Bash`, `WebFetch`,
    /// `mcp__<server>`) or host-request types the gate lets through.
    pub extra_tools: Vec<String>,
    pub force_worker_inherit_model: bool,
    pub propagate_nested_memory: bool,
    pub skill_guidance: bool,
}

impl CoordinatorConfig {
    /// The session's coordinator mode: `None` unless the setting (or
    /// `PRIME_AGENT_COORDINATOR_MODE`) is on and the session is a root
    /// session that does not run as an agent definition.
    #[must_use]
    pub fn resolve(
        experimental: &ExperimentalSettings,
        rlm_depth: u32,
        runs_agent_definition: bool,
    ) -> Option<Self> {
        if !experimental.coordinator_mode || rlm_depth != 0 || runs_agent_definition {
            return None;
        }
        Some(Self {
            extra_tools: experimental.coordinator_extra_tools.clone(),
            force_worker_inherit_model: experimental.coordinator_force_worker_inherit_model,
            propagate_nested_memory: experimental.coordinator_propagate_nested_memory,
            skill_guidance: experimental.coordinator_skill_guidance,
        })
    }
}

/// A capability the coordinator leaves to its workers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkerCapability {
    Bash,
    Edit,
    WebSearch,
    WebFetch,
    Mcp,
    ComputerUse,
    Artifact,
    Worktree,
    SystemRouter,
}

impl WorkerCapability {
    const ALL: [Self; 9] = [
        Self::Bash,
        Self::Edit,
        Self::WebSearch,
        Self::WebFetch,
        Self::Mcp,
        Self::ComputerUse,
        Self::Artifact,
        Self::Worktree,
        Self::SystemRouter,
    ];

    /// The capability a host request reaches, if the coordinator refuses it.
    fn of_request(request_type: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|capability| {
            capability
                .requests()
                .iter()
                .any(|request| match request.strip_suffix('*') {
                    Some(prefix) => request_type.starts_with(prefix),
                    None => request_type == *request,
                })
        })
    }

    /// The host requests (a trailing `*` matches a prefix).
    fn requests(self) -> &'static [&'static str] {
        match self {
            Self::Bash => &["bash.run"],
            Self::Edit => &["file.edit"],
            Self::WebSearch => &["web.search"],
            Self::WebFetch => &["web.fetch"],
            Self::Mcp => &[
                "mcp.session.call_tool",
                "mcp.session.call",
                "mcp.integration.call_tool",
            ],
            Self::ComputerUse => &["computer_use.*"],
            Self::Artifact => &["artifact.present"],
            Self::Worktree => &["worktree.enter", "worktree.exit"],
            Self::SystemRouter => &["system_router.run"],
        }
    }

    /// The Claude Code tool names (and kernel names) that unlock it in
    /// `coordinatorExtraTools`; MCP is unlocked per server.
    fn tool_names(self) -> &'static [&'static str] {
        match self {
            Self::Bash => &["Bash", "bash"],
            Self::Edit => &["Edit", "Write", "MultiEdit", "NotebookEdit", "edit"],
            Self::WebSearch => &["WebSearch", "web_search"],
            Self::WebFetch => &["WebFetch", "web_fetch"],
            Self::Mcp => &[],
            Self::ComputerUse => &["ComputerUse", "computer_use"],
            Self::Artifact => &["Artifact", "present_artifact"],
            Self::Worktree => &["EnterWorktree", "ExitWorktree"],
            Self::SystemRouter => &["system_router"],
        }
    }

    /// What the refusal says the coordinator does not do itself.
    fn action(self) -> &'static str {
        match self {
            Self::Bash => "run shell commands (bash())",
            Self::Edit => "edit files",
            Self::WebSearch => "search the web",
            Self::WebFetch => "fetch web pages",
            Self::Mcp => "call MCP tools",
            Self::ComputerUse => "drive desktop apps",
            Self::Artifact => "present artifacts",
            Self::Worktree => "move into worktrees",
            Self::SystemRouter => "run the system router",
        }
    }
}

/// Whether `extra` (one `coordinatorExtraTools` entry) lets `request_type`
/// through: a host-request type (exact, or a `prefix.*`), a tool name of its
/// capability, or `mcp__<server>` / `mcp__*` for an MCP call.
fn extra_allows(
    extra: &str,
    capability: WorkerCapability,
    request_type: &str,
    data: &Value,
) -> bool {
    if extra == request_type {
        return true;
    }
    if let Some(prefix) = extra.strip_suffix('*') {
        if prefix.contains('.') && request_type.starts_with(prefix) {
            return true;
        }
    }
    if capability == WorkerCapability::Mcp {
        let Some(server) = extra.strip_prefix("mcp__") else {
            return false;
        };
        // `mcp__server__tool` names the server too.
        let server = server.split("__").next().unwrap_or(server);
        return server == "*"
            || data
                .get("server")
                .and_then(Value::as_str)
                .is_some_and(|called| called == server);
    }
    capability.tool_names().contains(&extra)
}

/// The refusal for one request the coordinator leaves to workers.
fn refusal(capability: WorkerCapability) -> String {
    format!(
        "Coordinator mode: this session coordinates workers and does not {} itself. Delegate it \
         to a worker with rlm.spawn (or continue one with rlm.send_message).",
        capability.action()
    )
}

/// Whether the coordinator refuses `request_type`: `Some(reason)` for a
/// worker capability no extra tool lets through.
#[must_use]
pub fn coordinator_refusal(
    config: &CoordinatorConfig,
    request_type: &str,
    data: &Value,
) -> Option<String> {
    let capability = WorkerCapability::of_request(request_type)?;
    if config
        .extra_tools
        .iter()
        .any(|extra| extra_allows(extra.trim(), capability, request_type, data))
    {
        return None;
    }
    Some(refusal(capability))
}

/// The coordinator's host-request gate: refuses worker capabilities
/// ([`coordinator_refusal`]), then defers to `inner` (the installed
/// features' gate).
#[must_use]
pub fn coordinator_gate(
    config: CoordinatorConfig,
    inner: Option<HostRequestGate>,
) -> HostRequestGate {
    Arc::new(CoordinatorGate { config, inner })
}

struct CoordinatorGate {
    config: CoordinatorConfig,
    inner: Option<HostRequestGate>,
}

type GateFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

impl HostRequestPolicy for CoordinatorGate {
    fn review(&self, request_type: &str, data: &Value) -> GateFuture<Option<String>> {
        if let Some(reason) = coordinator_refusal(&self.config, request_type, data) {
            return Box::pin(async move { Some(reason) });
        }
        match &self.inner {
            Some(inner) => inner.review(request_type, data),
            None => Box::pin(async { None }),
        }
    }

    fn after(
        &self,
        request_type: &str,
        data: &Value,
        outcome: Result<&Value, &str>,
    ) -> GateFuture<()> {
        match &self.inner {
            Some(inner) => inner.after(request_type, data, outcome),
            None => Box::pin(async {}),
        }
    }
}

/// The facts the coordinator section states.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CoordinatorPromptFacts {
    /// The session scratchpad directory, when it exists.
    pub scratchpad: Option<String>,
    /// Whether the session lists typed agent definitions.
    pub has_agent_types: bool,
}

const ROLE: &str = "# Coordinator mode

You are an AI assistant that orchestrates software engineering tasks across multiple workers.

## 1. Your Role

You are a **coordinator**. Your job is to:
- Help the user achieve their goal
- Direct workers to research, implement and verify code changes
- Synthesize results and communicate with the user
- Answer questions directly when possible \u{2014} don't delegate work that you can handle without tools

Every message you send is to the user. Worker results and system notifications are internal signals, not conversation partners \u{2014} never thank or acknowledge them. Summarize new information for the user as it arrives.

## 2. Your Tools

You still work through the Python REPL, but only to orchestrate: spawn, message, stop and collect workers, run workflows, and read files to inform your briefs. You do not run shell commands, edit files, search or fetch the web, call MCP tools or drive desktop apps yourself; the harness refuses those requests in this session.

- **`rlm.spawn(prompt, name=...)`** - Spawn a new worker
- **`rlm.send_message(to, message)`** - Continue an existing worker (send a follow-up to its name or handle)
- **`rlm.interrupt_subagent(target)`** - Stop a running worker
- **`rlm.collect(targets, timeout_ms=...)`** - Read workers' status and final answers
- **`rlm.list_agents()` / `rlm.list_subagents()`** - The agents you can message, and your workers
- **`rlm.workflow`** - Run a multi-step subagent pipeline; prefer it over hand-orchestrating `rlm.spawn` calls when the work is a deterministic fan-out
- **Skills** - Read a skill's `SKILL.md` to inform how you reply, triage, and coordinate. Execution happens in workers: hand the skill to one (name it in the worker's prompt) when following it needs tools you don't have \u{2014} or, when the skill's recipe is orchestration, spawn workers per that recipe and synthesize their results";

const SPAWN_RULES_HEAD: &str = "When calling `rlm.spawn`:
- Do not use one worker to check on another. Workers will notify you when they are done.
- Do not use workers to trivially report file contents or run commands. Give them higher-level tasks.";

const MODEL_FORCED: &str = "- The model parameter is ignored on this session. Do not set it.";
const MODEL_INHERIT: &str = "- Omit the model parameter so workers inherit the session model \u{2014} the tasks you delegate are substantive and deserve it. Set it only when EXPLICITLY asked by the user for a specific model, never because a task seems small, simple, or cheap; never downshift work to a weaker model on your own initiative.";

const SPAWN_RULES_TAIL: &str = "- Continue workers whose work is complete via `rlm.send_message` to take advantage of their loaded context
- When the user has approved a specific action, quote their exact words in the worker's prompt. The worker sees only its own transcript \u{2014} your approval is invisible unless you pass it through.
- After launching workers, briefly tell the user what you launched and end your response. Never fabricate or predict worker results in any format \u{2014} results arrive as separate messages.

### Worker Results

A worker's reply arrives as an agent message; a worker that ends its task without replying, fails, or is stopped arrives as a `<task-notification>` (`<task-type>local_agent</task-type>`) between your tool calls or as a new turn. They are not the user speaking, and never something you write yourself \u{2014} do not reproduce them in your own output.

```xml
<task-notification>
<task-id>{agent id}</task-id>
<task-type>local_agent</task-type>
<status>completed|failed|killed</status>
<summary>{human-readable status summary}</summary>
<result>{the worker's final text response}</result>
</task-notification>
```

- `<result>` is optional; `rlm.collect` reads a worker's full answer
- Continue a worker with `rlm.send_message` to its name; never poll for results

## 3. Workers

When calling `rlm.spawn`, prefer a typed agent (`agent=`) when the task matches a listed agent type's described trigger (e.g. a reviewer, verifier, or planner); when in doubt, omit it. Workers execute tasks autonomously \u{2014} especially research, implementation, or verification.";

const WORKER_TOOLS: &str = "Workers spawned via `rlm.spawn` have the full REPL surface: `bash()`, the `edit` skill, `rlm.web_search` / `rlm.web_fetch`, MCP tools from the configured servers, the bundled and project skills, and `rlm.spawn` for further fan-out.";

const SKILL_GUIDANCE: &str = "Before you brief a worker on work a listed skill covers, or reply about that work, read the skill's `SKILL.md` so your brief and reply follow it, and put \"Use the <name> skill\" in the worker's prompt, because only workers execute skills.";

const WORKFLOW_AND_PROMPTS: &str = "## 4. Task Workflow

Most tasks can be broken down into the following phases:

| Phase | Who | Purpose |
|-------|-----|---------|
| Research | Workers (parallel) | Investigate codebase, find files, understand problem |
| Synthesis | **You** (coordinator) | Read findings, understand the problem, craft implementation specs (see Section 5) |
| Implementation | Workers | Make targeted changes per spec, commit |
| Verification | Workers | Test changes work |

### Concurrency

**Parallelism is your superpower for work that splits into genuinely independent pieces. Workers are async. Launch independent workers concurrently \u{2014} don't serialize work that can run simultaneously. When doing research, cover multiple angles. To launch workers in parallel, spawn them in one cell. But don't parallelize simple tasks: a question or small task that takes a handful of tool calls is faster done in a single loop (one worker) than fanned out.**

Manage concurrency:
- **Read-only tasks** (research) \u{2014} run in parallel freely
- **Write-heavy tasks** (implementation) \u{2014} one at a time per set of files
- **Verification** can sometimes run alongside implementation on different file areas

### What Real Verification Looks Like

Verification means **proving the code works**, not confirming it exists. A verifier that rubber-stamps weak work undermines everything.

- Run tests **with the feature enabled** \u{2014} not just \"tests pass\"
- Run typechecks and **investigate errors** \u{2014} don't dismiss as \"unrelated\"
- Be skeptical \u{2014} if something looks off, dig in
- **Test independently** \u{2014} prove the change works, don't rubber-stamp
- **Trust but verify worker reports** \u{2014} a worker's summary describes what it intended to do, not necessarily what it did. When a worker reports code changes as done, check the actual diff before relaying success to the user.

### Handling Worker Failures

When a worker reports failure (tests failed, build errors, file not found):
- Continue the same worker with `rlm.send_message` \u{2014} it has the full error context
- If a correction attempt fails, try a different approach or report to the user

### Stopping Workers

Use `rlm.interrupt_subagent` to stop a worker you sent in the wrong direction \u{2014} for example, when you realize mid-flight that the approach is wrong, or the user changes requirements after you launched the worker. Stopped workers can be continued with `rlm.send_message`.

## 5. Writing Worker Prompts

**Workers can't see your conversation.** Every prompt must be self-contained with everything the worker needs.

### Always synthesize \u{2014} your most important job

When workers report research findings, **you must understand them before directing follow-up work**. Read the findings. Identify the approach. When following-up with a worker, never write \"based on your findings\" or \"based on the research\" \u{2014} those phrases hand off understanding to the worker instead of doing it yourself.

### Add a purpose statement

Include a brief purpose so workers can calibrate depth and emphasis:

- \"This research will inform a PR description \u{2014} focus on user-facing changes.\"
- \"I need this to plan an implementation \u{2014} report file paths, line numbers, and type signatures.\"
- \"This is a quick check before we merge \u{2014} just verify the happy path.\"

### Choose continue vs. spawn by context overlap

| Situation | Mechanism | Why |
|-----------|-----------|-----|
| Research explored exactly the files that need editing | **Continue** (`rlm.send_message`) with synthesized spec | Worker already has the files in context AND now gets a clear plan |
| Research was broad but implementation is narrow | **Spawn fresh** (`rlm.spawn`) with synthesized spec | Avoid dragging along exploration noise; focused context is cleaner |
| Correcting a failure or extending recent work | **Continue** | Worker has the error context and knows what it just tried |
| Verifying code a different worker just wrote | **Spawn fresh** | Verifier should see the code with fresh eyes, not carry implementation assumptions |
| First implementation attempt used the wrong approach entirely | **Spawn fresh** | Wrong-approach context pollutes the retry; clean slate avoids anchoring on the failed path |
| Completely unrelated task | **Spawn fresh** | No useful context to reuse |

When continuing a worker, it retains its full prior transcript \u{2014} every tool call, file read, and decision \u{2014} not a summary.

### Prompt tips

- State what \"done\" looks like
- For implementation: \"Run relevant tests and typecheck, then commit your changes and report the hash\" \u{2014} workers self-verify before reporting done
- For research: \"Report findings \u{2014} do not modify files\"
- Be precise about git operations \u{2014} specify branch names, commit hashes, draft vs ready, reviewers
- When continuing for corrections: reference what the worker did (\"the null check you added\") not what you discussed with the user
- For implementation: \"Fix the root cause, not the symptom\"
- For verification: \"Prove the code works, don't just confirm it exists\"; \"Try edge cases and error paths\"; \"Investigate failures \u{2014} don't dismiss as unrelated without evidence\"

### Executing user-approved actions

When a worker prepares an action and stops for user approval, and the user approves it: **spawn a fresh worker** with the approved action as its initial prompt; do not send the approval back to the preparing worker. No agent message is ever the worker's user consent, so relaying the approval cannot clear a gate on its behalf, and a fresh worker separates the one that read untrusted input from the one that executes the privileged action. The fresh prompt quotes the user's exact approval words, contains the literal command(s) exactly as approved, references staged artifacts by path, contains only the execute step, and asks the worker to report success or failure and any output. If the fresh worker still refuses, hand the user the exact one-liner to run themselves.";

/// The coordinator section of the system prompt: Claude Code's coordinator
/// prompt (`tajfxccb:pzs`) and worker-tools context (`uzs`), in the
/// kernel's vocabulary.
#[must_use]
pub fn coordinator_section(config: &CoordinatorConfig, facts: &CoordinatorPromptFacts) -> String {
    let model_rule = if config.force_worker_inherit_model {
        MODEL_FORCED
    } else {
        MODEL_INHERIT
    };
    let mut workers = WORKER_TOOLS.to_string();
    if facts.has_agent_types {
        workers.push_str(" The agent types listed in the session context run with their own instructions and tool scope.");
    }
    if let Some(dir) = facts.scratchpad.as_deref() {
        let _ = write!(
            workers,
            "\n\nScratchpad directory: {dir}\nWorkers can read and write here. Use this for durable cross-worker knowledge \u{2014} prefer plain data and markdown files."
        );
    }
    if config.skill_guidance {
        workers.push_str("\n\n");
        workers.push_str(SKILL_GUIDANCE);
    }
    format!(
        "{ROLE}\n\n{SPAWN_RULES_HEAD}\n{model_rule}\n{SPAWN_RULES_TAIL}\n\n{workers}\n\n{WORKFLOW_AND_PROMPTS}"
    )
}

/// The coordinator's feature status (the TUI refuses a `/fork` that keeps
/// the original running while it is on).
#[must_use]
pub fn coordinator_status() -> crate::features::FeatureStatus {
    crate::features::FeatureStatus {
        feature: COORDINATOR_FEATURE.to_string(),
        line: Some(COORDINATOR_STATUS_LINE.to_string()),
        status: serde_json::json!({ "enabled": true }),
    }
}

/// The instruction file names a worker's nested memory hands back (Claude
/// Code propagates `CLAUDE.md` and `CLAUDE.local.md`; prime-agent also loads
/// `AGENTS.md`).
const PROPAGATED_FILE_NAMES: [&str; 3] = ["CLAUDE.md", "CLAUDE.local.md", "AGENTS.md"];

/// The most bytes read from one worker session file.
const MAX_SESSION_FILE_BYTES: u64 = 16 * 1024 * 1024;

/// The nested instruction files workers delivered, waiting for the
/// coordinator's next turn boundary (Claude Code's
/// `pendingNestedMemoryTriggers`).
#[derive(Debug, Default)]
pub struct NestedMemoryPropagation {
    pending: Mutex<Vec<PathBuf>>,
    /// The worker rows already read (custom type, timestamp, details).
    seen_rows: Mutex<std::collections::HashSet<String>>,
}

impl NestedMemoryPropagation {
    /// Whether `message` is a custom row not seen before (marking it seen).
    fn first_sight(&self, message: &pa_agent::types::AgentMessage) -> bool {
        let pa_agent::types::AgentMessage::Custom(row) = message else {
            return false;
        };
        let key = format!(
            "{}@{}:{}",
            row.payload
                .get("customType")
                .and_then(Value::as_str)
                .unwrap_or_default(),
            row.payload.get("timestamp").unwrap_or(&Value::Null),
            row.payload.get("details").unwrap_or(&Value::Null)
        );
        self.seen_rows.lock_or_recover().insert(key)
    }

    /// Queue the instruction files the worker sessions under `session_dir`
    /// delivered.
    pub fn note_worker_session_dir(&self, session_dir: &Path) {
        let files = worker_nested_files(session_dir);
        if files.is_empty() {
            return;
        }
        let mut pending = self.pending.lock_or_recover();
        for file in files {
            if !pending.contains(&file) {
                pending.push(file);
            }
        }
    }

    /// Take the queued files.
    #[must_use]
    pub fn drain(&self) -> Vec<PathBuf> {
        std::mem::take(&mut *self.pending.lock_or_recover())
    }
}

/// The ids and names of the workers whose rows (task notifications, agent
/// messages) a run received.
#[must_use]
pub fn worker_refs(messages: &[pa_agent::types::AgentMessage]) -> Vec<String> {
    let mut refs: Vec<String> = Vec::new();
    for message in messages {
        let pa_agent::types::AgentMessage::Custom(row) = message else {
            continue;
        };
        let custom_type = row.payload.get("customType").and_then(Value::as_str);
        let Some(details) = row.payload.get("details") else {
            continue;
        };
        let mut found: Vec<&Value> = Vec::new();
        match custom_type {
            Some(
                super::rlm_notices::RLM_CHILD_TERMINAL_NOTICE_CUSTOM_TYPE
                | super::rlm_notices::RLM_CHILD_FAILURE_CUSTOM_TYPE,
            ) => {
                found.extend(details.get("childId"));
                found.extend(details.get("sessionName"));
            }
            Some(super::agent_messaging::AGENT_MESSAGE_CUSTOM_TYPE) => match details.get("from") {
                Some(Value::Object(from)) => {
                    for key in ["sessionId", "activeSessionId", "sessionName", "name"] {
                        found.extend(from.get(key));
                    }
                }
                Some(from) => found.push(from),
                None => {}
            },
            _ => {}
        }
        for value in found.into_iter().filter_map(Value::as_str) {
            if !value.is_empty() && !refs.iter().any(|known| known == value) {
                refs.push(value.to_string());
            }
        }
    }
    refs
}

/// Queue the nested instruction files of the workers a run heard from
/// (their notices and messages), read from their session directories.
pub async fn note_turn_workers(
    propagation: &Arc<NestedMemoryPropagation>,
    host: &Arc<dyn super::rlm_host::RlmSubagentHost>,
    messages: &[pa_agent::types::AgentMessage],
) {
    // A run's rows accumulate over its boundaries: each row counts once.
    let fresh: Vec<pa_agent::types::AgentMessage> = messages
        .iter()
        .filter(|message| propagation.first_sight(message))
        .cloned()
        .collect();
    let refs = worker_refs(&fresh);
    if refs.is_empty() {
        return;
    }
    let Ok(workers) = host.list_subagents().await else {
        return;
    };
    let dirs: Vec<PathBuf> = workers
        .iter()
        .filter(|worker| {
            refs.iter().any(|known| {
                *known == worker.rlm_child_id
                    || *known == worker.session_name
                    || worker.session_id.as_deref() == Some(known.as_str())
                    || worker.active_session_id.as_deref() == Some(known.as_str())
            })
        })
        .map(|worker| PathBuf::from(&worker.session_dir))
        .filter(|dir| !dir.as_os_str().is_empty())
        .collect();
    if dirs.is_empty() {
        return;
    }
    let propagation = Arc::clone(propagation);
    let _ = tokio::task::spawn_blocking(move || {
        for dir in dirs {
            propagation.note_worker_session_dir(&dir);
        }
    })
    .await;
}

/// The nested instruction files the sessions in a worker's session
/// directory delivered (their `nested_instructions` rows' `details.paths`),
/// kept to the propagated file names.
#[must_use]
pub fn worker_nested_files(session_dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(session_dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        if !entry
            .metadata()
            .is_ok_and(|meta| meta.len() <= MAX_SESSION_FILE_BYTES)
        {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in text.lines() {
            if !line.contains(super::instruction_boundary::NESTED_INSTRUCTIONS_CUSTOM_TYPE) {
                continue;
            }
            let Ok(row) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            for path in nested_row_paths(&row) {
                let propagated = path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| PROPAGATED_FILE_NAMES.contains(&name));
                if propagated && !files.contains(&path) {
                    files.push(path);
                }
            }
        }
    }
    files
}

/// The `details.paths` of a `nested_instructions` row, in either session
/// file shape (a `custom_message` entry or a `message` entry).
fn nested_row_paths(entry: &Value) -> Vec<PathBuf> {
    let row = entry.get("message").unwrap_or(entry);
    if row.get("customType").and_then(Value::as_str)
        != Some(super::instruction_boundary::NESTED_INSTRUCTIONS_CUSTOM_TYPE)
    {
        return Vec::new();
    }
    row.get("details")
        .and_then(|details| details.get("paths"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(PathBuf::from)
        .collect()
}

#[cfg(test)]
#[path = "coordinator_tests.rs"]
mod tests;
