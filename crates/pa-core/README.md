# pa-core

The session engine.

## Scope
MCP host side (`mcp/`): auth gating for built-in integrations (the
disabled-skill overrides and `/mcp list` status), the `mcpServers` settings
seam, and the `mcp.*` host-request handlers the kernel's generic MCP
client reaches through (`mcp.config`, `mcp.refresh`, optional
`mcp.begin_login`). The OAuth flow lives behind it (`oauth*` submodules:
RFC 9728 discovery, dynamic registration, PKCE, the local callback
server, token refresh; `login.rs`: login execution that persists the
endpoint-bound credential into the shared auth store and the
`wire_begin_login`/`McpOAuth` refresh integration hosts wire). The MCP
client sessions themselves run here too (`mcp/session/`, the official Rust
SDK `rmcp` over stdio child processes and streamable HTTP): one
`McpSessions` per agent session behind the `mcp.session.*` /
`mcp.integration.*` host requests the kernel's `rlm.mcp` and
`rlm.McpIntegration` thin clients send; connections outlive kernel restarts
and close on reload, config change, idle, and session end. Declared servers
(`mcp/declared/`, [docs/mcp-json.md](../../docs/mcp-json.md)): Claude Code's
`.mcp.json` files from the filesystem root down to the working directory and,
opted in, `~/.claude.json`'s user and local servers, merged with the settings'
`mcpServers` (settings > local > project > user), parsed with Claude Code's
validation and `${VAR}` / `${VAR:-default}` expansion; a project server runs
once approved (`enabledMcpjsonServers` / `disabledMcpjsonServers` /
`enableAllProjectMcpServers`, the agent dir's `mcp-project-choices.json`), and
`ask_pending_project_servers` asks through the confirmation surface. Tools (bash, edit, python_repl + internal rename/stdout), file mutation queue, truncation and rendering rules, RLM kernel lifecycle (IPython spawn/execute/revive; the kernel's `bash()` commands are checked and run host-side by `pa-bash`, served as native `bash.*` host requests from one job table per kernel manager, answered for the activity view without a kernel round trip, started under the kernel's own prepared OS sandbox, and killed with the kernel), skills loading, system prompt assembly, compaction, harness refinement, settings/config, package manager (npm/git/local source install/remove/list/update against settings), session manager (persist/resume). Autonomous mode
(`autonomous`): runtime state with limit normalization, continuation and
gate-failure texts, shell quality gates with retry windows and git
worktree snapshotting, and the `AutonomousDriver` policy trait a turn
loop consults after every settled turn (the engine holds no autonomous
logic of its own). The RLM recursion host seam
(`session_engine::rlm_host`): the trait the kernel's `rlm.spawn`/
`rlm.create_session`/`rlm.list_subagents`/`rlm.collect`/
`rlm.interrupt_subagent`/`rlm.delete_subagent` host requests call into, with the roster/collect/
selector-error vocabulary the daemon implements over the supervisor link,
plus the typed `rlm.spawn` placement contract (`target` kwarg; the
`RlmSpawnTarget` vocabulary and its design note live in
`kernel::rlm_runtime`): omitted or `local` keeps the pre-contract local
spawn byte-identically, `cloud` is refused at admission before any host is
consulted until the cloud child backend exists (never a silent local
fallback), and the note pins the admission/messaging parity the backend
must uphold.
Typed agent definitions (`agents`): Claude Code's `.claude/agents/*.md`
(and `.prime/agents/*.md`) discovered over the project (the working
directory up to the repository root) and the user directories
(`<agent dir>/agents`, `~/.claude/agents`), parsed from YAML frontmatter;
the listing a spawning session reads at the end of its system prompt; the
instructions and the host-request gate (`bash.run`, `rlm.run`,
`agent_message.send`) a session running as one agent gets
(`SessionEngineConfig::agent_definition`). `rlm.spawn(agent=, isolation=,
cwd=)` resolves through `session_engine::rlm_spawn_agent`; an isolated
child's worktree is a managed `worktree` (`agent-<id>` cut from `HEAD`,
removed with the child only when unchanged). Child lifecycle notices
(`session_engine::rlm_notices`) render Claude Code's agent
`<task-notification>`. A definition's `memory` scope (`user`, `project`,
`local`) gives the session a persistent agent memory (`agents::memory`):
`<agent dir>/agent-memory/<agent>/`, `<project>/.prime/agent-memory/<agent>/`
or `<project>/.prime/agent-memory-local/<agent>/` (Claude Code's
`~/.claude/agent-memory`, `.claude/agent-memory`, `.claude/agent-memory-local`
directories in place when only they exist), its `MEMORY.md` index under
Claude Code's "Persistent Agent Memory" instructions after the agent's own,
and the directory writable under the OS sandbox; a project or local memory
that leads out of the project is read-only. Auto memory's switches govern it.
The in-process RLM host admits each child terminal notice through the parent
session inbox: one fsynced JSONL row precedes live registration and child
settlement. A child that replied settles only after its explicit reply row
is durably admitted through that same strict writer. On reopen, notices without
a `notice_consumed` marker replay from
the original row. Model delivery is **at least once**, not exactly once: a
crash after the parent model responds but before its assistant row and marker
are synced can repeat inference. No durable terminal row means the child's
pre-crash outcome remains interrupted or unknown, never fabricated as done.
Platform wall (`platform`): process control (signals/process groups, kernel exit waits), file locking, file permissions, and shell selection - every OS-specific behavior in the engine lives there behind cfg-gated implementations (the durable-write rename primitive is pa-telemetry's `rename_onto`, re-exported as `platform::rename_onto`). Scheduled jobs (`cron`, the `AgentCronJobStore` port of `core/cron-jobs.ts`): file-backed job state under session artifacts (`scheduled-jobs.json` partitions) with cross-process locking, plus the public read-only scan (`cron::store::read_scheduled_jobs_artifact`) the update flow's roster projection and boot re-arm read. Tools (bash, edit, python_repl + internal rename/stdout), file mutation queue, truncation and rendering rules, RLM kernel lifecycle (IPython spawn/execute/revive), skills loading, system prompt assembly, compaction, harness refinement, settings/config, package manager (npm/git/local source install/remove/list/update against settings, plus `resolve()`: precedence-ranked session resource resolution over configured packages, settings arrays, auto-discovery, and bundled skills), session manager (persist/resume). origin/main

Claude Code's session scheduling (`cron::tasks`; `CronCreate`/`CronList`/`CronDelete`,
`ScheduleWakeup`, `/loop`): a pure task book over an injected clock and zone. Five-field cron in the
user's local time (`chrono::Local` behind the `CronZone` seam; daylight-saving edges follow Vixie
cron), Claude Code's per-id jitter, recurring tasks that expire after seven days, one-shots that
delete themselves, self-paced loop wakeups clamped to [60, 3600] with the keepalive and the
interrupt rule, the autonomous-loop preambles and sentinels, and `/loop` parsing with the skill's
interval table. `SessionTasks` wraps the book for the `rlm.cron.*` / `rlm.schedule_wakeup` host
handlers (registered from `KernelCronWiring::session_tasks`) and the `/loop` session command, and
keeps durable tasks in the session artifact partition's `scheduled_tasks.json` (Claude Code's file
shape). The embedding's scheduler (pa-daemon's `session_tasks`) owns time and delivery. Flags:
`PRIME_AGENT_DISABLE_CRON`, `PRIME_AGENT_LOOP_PERSISTENT`, `PRIME_AGENT_LOOP_KEEPALIVE=0`.

Managed git worktrees (`worktree`, Claude Code's `--worktree`/`EnterWorktree`): the one helper the
`--worktree` launch, the mid-session `worktree.enter`/`worktree.exit` host requests and child isolation share.
`create(&CreateRequest)` makes or reuses `<repo>/.prime/worktrees/<name>` on branch `worktree-<name>` (`fresh`:
`origin/HEAD` as last fetched, else `HEAD`; `head`: the requesting `HEAD`; setting `worktree.baseRef`), never
resets an existing branch, records the base commit (`PRIME_AGENT_BASE` in the worktree's git dir), adds the
managed dir to `info/exclude`, and copies `.worktreeinclude` files; `changes`, `exit_choice` and `remove` (keeps
work unless `discard`; deletes the branch only when its tip is reachable elsewhere) end it. A
`WorktreeHooks` provider (`install_hooks`) replaces git. Session scratch directories
(`session_engine::scratch`): each session's private `scratchpad/` and `tasks/` in its artifact partition (removed
with the session), named to the kernel as `PRIME_AGENT_SCRATCHPAD`/`PRIME_AGENT_TASKS_DIR` and in the
environment section; the kernel manager's job table persists every `bash()` output to `tasks/<job id>.output`.

Provider resilience policies in `session_engine`: the shared quick-retry
policy (TS `provider-retry.ts`: permanent-kind classification,
Retry-After-aware capped delays), the interactive auto-retry loop, and the
provider-failover driver (when a provider exhausts its quick retries and
another configured provider serves the same model, the failed turn
re-routes to the next provider in catalog order; the TS
`auto_retry_start`/`reason: "backup"` event vocabulary surfaces the
progression). The drivers are pure decision logic: the host (pa-daemon)
owns the attempt, the wait, and the model switch.

Session HTML export (`export_html`): the standalone viewer file for
`/export` and `session export` - the embedded product template
(assets/export-html, vendored marked/highlight.js attributed in its
NOTICE.md) plus the theme-to-CSS resolution over the bundled theme data
(pa-types) and custom themes under `<agent-dir>/themes/`, the tools
section mapping (`tools_section`: the session's tool registry to the
template's name/description/parameters list), and the custom-tool
pre-render pipeline (`ToolHtmlRenderer`/`pre_render_custom_tools` +
`ansi_to_html`: the exporter walks the entries, the session layer's
renderer produces line-oriented output, ANSI converts to inline-styled
HTML at the export step), and the two entry points
(`export_session_to_html` for the daemon worker's `export_html`
command, `export_from_file` for the CLI). Terminal theme rendering stays
pa-tui's; tool renderers live with the session's tool surface;
the share-viewer flow (gist creation) is the caller's.
Cloud workspace snapshots (`workspace_snapshot`): the portable, verifiable
capture of a git worktree's working state - tracked modifications, staged
paths, deletions, and nonignored untracked files - staged into an
owner-private directory as content-addressed blobs plus a deterministic
manifest that hashes them, with credential-shaped, symlink-ancestor,
escaping-symlink, nested-repo, and submodule exclusions recorded in the
manifest, hard entry/size bounds, race-free openat(O_NOFOLLOW) leaf reads
(Unix), and offline verification of a staged snapshot before upload. Capture
refuses on non-Unix targets: owner-only 0700 staging and 0600 files cannot be
enforced there. The baseline is an explicit caller choice (`BaselineMode`):
HEAD's tree content staged through the same filters as the delta (each entry
verified against HEAD's recorded object id and mode, no git history ships),
or an explicit external reference the consumer must reach itself. The secret
skip is a filename-shaped denylist only; ordinary-named files containing
secrets stage verbatim. Snapshot directories are secret-bearing: every
consumer must treat them as credentials. Capture does synchronous file I/O
inside its async entry point; callers must offload the entire operation with
`tokio::task::spawn_blocking` and `Handle::block_on`. Foundation plumbing
only: nothing wires it to a user-facing cloud toggle yet.

Workspace trust (`workspace_trust`): the gate on project-scope configuration
that can run code or change the agent's instructions. `SettingsManager::create`
evaluates the cwd's trust against `<agentDir>/trusted-workspaces.json` (owner-only,
keyed by the canonical cwd, each record pinned to a `sha256` over the gated
content). An untrusted workspace keeps only the
`UNTRUSTED_PROJECT_SETTINGS_KEYS` of its project settings (presentation and
turn-behaviour keys; everything else, unknown keys included, waits for trust),
and `load_resources` skips its `SYSTEM.md`/`APPEND_SYSTEM.md`, project
prompt templates, and project skills (`.prime/agent/skills/`, ancestor
`.agents/skills/`, settings `skills` paths), so an untrusted Python skill is
never editable-installed into the kernel venv or imported; Claude Code's
`.claude/commands/` (custom commands, whose `` !`cmd` `` blocks run at
admission), `.claude/skills/` (the working directory's and its ancestors' up to
the repository root) and `.claude/output-styles/` (plus
`.prime/agent/output-styles/`) are gated the same way. The hash covers
every file of those skill trees except tool caches. A change to the gated content reads as `Changed` (asks again);
a theme edit does not. Project-scope writes from an untrusted workspace are
refused; the product's own project writes in a trusted one re-pin the record.
`evaluate` never prompts or writes, so daemon workers just read the decision;
the composition root asks (`record`, `list`). A corrupt store trusts nothing
and is never overwritten. A run from the home directory, whose project config
dir is the agent dir, needs no decision.

Claude Code skills (`skills::claude_skills`, `skills::dynamic`): instruction
skills from `.claude/skills/<name>/SKILL.md` (Claude Code 2.1.296's loader:
the directory names the skill; `description` with the first-line fallback,
`when_to_use`, `argument-hint`, `arguments`, `allowed-tools`, `model`,
`disable-model-invocation`, `user-invocable`). The project directories are the
working directory's and its ancestors' up to the repository root (stopping
before home), then each additional directory's; `~/.claude/skills`
(`CLAUDE_CONFIG_DIR` aware) loads only with the `claudeCodeUserSkills` setting
(unset is off). They follow the Prime Agent skills: a Prime Agent skill of the
same name wins and the Claude Code copy is reported as a collision. The
inventory lists `<description> - <when_to_use>` and the `rlm.load_skill` call;
the user runs one as `/<name>` (unless a builtin command or a Prime Agent
prompt template has the name) or `/skill:<name>`; `skill.load`
(`session_engine::skill_host`, the kernel's `rlm.load_skill(name, args)`) is
the model's Skill tool and refuses `disable-model-invocation` skills. An
invocation's text is Claude Code's: `Base directory for this skill: <dir>`
and the body with the arguments, `${CLAUDE_SKILL_DIR}`,
`${CLAUDE_PROJECT_DIR}` and `${CLAUDE_SESSION_ID}` substituted and the
`` !`cmd` `` blocks run (`disableSkillShellExecution` turns them off). In a
trusted workspace a file a cell touches below the working directory brings in
the `.claude/skills` directories between it and the working directory (Claude
Code's dynamic skills), announced as a hidden `dynamic_skills` row
(`session_engine::skill_boundary`).

Saved workflows (`session_engine::workflows_command`): `/workflows` lists the
saved workflow scripts `rlm.workflow.run(name=...)` resolves
(`.prime/workflows/` then `.claude/workflows/` per project directory, nearest
first, then `<agent dir>/workflows/` and `~/.claude/workflows/`; Python
scripts only, named by file stem) and the session's recent runs (the
`workflows/<run id>/run.json` records under the session artifact directory).

Instruction files and auto memory (`resources::instructions`,
`resources::auto_memory`; contract in `docs/instruction-files.md`): one
loader for the AGENTS.md and CLAUDE.md families with Claude Code's rules
(`@path` imports, `*.local.md`, `.claude/rules` and `.prime/agent/rules`
with `paths:`-scoped rules, the opt-in `~/.claude` user scope), the
`NestedInstructions` tracker that delivers a subdirectory's files and the
matching scoped rules as a hidden `nested_instructions` row
(`session_engine::instruction_boundary`) once a cell opens a file there,
and the per-project auto-memory directory whose `MEMORY.md` index rides
the `auto-memory` prompt segment. `/memory` lists both
(`session_engine::memory_command`). `auto_memory::sanitize` is Claude Code's
exact project-directory name (UTF-16 units, the base-36 Java string hash past
200 characters), so `~/.claude/projects/<name>` is the directory Claude Code
writes.

External sessions (`session::external_sessions`): the registry of other
products' session sources (`ExternalSessionSource`: `list`, `find`,
`import`) the composition root installs (`pa-cc-import`'s Claude Code
transcripts). Nothing is installed natively; the daemon serves the
`external_sessions` commands only while a source is.

Claude Code's settings files (`settings::claude_code`): one reader for the
managed, `.claude/settings.local.json`, `.claude/settings.json` and
`~/.claude/settings.json` files, with Claude Code's precedence and merge,
behind the user-settings opt-in `claudeCodeSettings: true` (top level or in
`permissions`). Project files apply only in a trusted workspace, and with
the opt-in on their keys that run code or loosen the session (`hooks`,
`statusLine`, `env`, `model`, `outputStyle`, `permissions.allow`,
`defaultMode`, `additionalDirectories`) are part of the trust hash
(`GatedItem::ClaudeSettings`). pa-permissions and pa-hooks read through it;
`SettingsManager` layers `statusLine`, `outputStyle`, `model` (a full
`claude-*` id as `anthropic` default, a soft preference), `cleanupPeriodDays`
(the archive age) under this agent's own settings, joins
`permissions.additionalDirectories`, hands `env` to the kernel (the
harness's own variables win; a project file cannot set `HOME`/`XDG_*`; a
`PATH`/`HOME` override is logged), and reads `disableAllHooks` (hooks and
the status line command off). `includeCoAuthoredBy`/`attribution` have no
counterpart: the system prompt has no git-commit section.

OS sandbox (`os_sandbox`, enforcement in `pa-os-sandbox`): `SessionSandbox::resolve` reads
the `sandbox` setting (off by default; a project scope only tightens it, so the key is
untrusted-safe) and the `--sandbox` override (`SessionEngineConfig::sandbox_mode`) once per
session. The kernel spawns under it (`KernelManagerOptions::sandbox`: the workspace plus the
temp dir, the session artifacts, the harness stores and the orphan journal's directory stay
writable), so every `bash()`/`subprocess` child inherits it; the host's stdio MCP servers
(`McpSessionOptions::sandbox`) and the daemon's `!` lane spawn through `SessionSandbox::command`; the session context's environment section gets
`prompt_line` (no line when off); `agent started` reports `sandbox_mode`. An enabled sandbox
the machine cannot enforce refuses to spawn. See `docs/os-sandbox.md`.

Session context and reminders (Claude Code's context attachments): the system prompt holds
only the static layers and the session-fixed tail (packages, session role, appended prompt), so
it is byte-stable for the provider cache. `prompts::session_context` renders the volatile
context (instruction files, skills inventory, generic MCP servers, `EnvironmentFields`, date)
as one hidden `session_context` row ahead of the first turn and `session_context_update` deltas
after; each row persists a `ContextFingerprint` (paths and hashes), and
`session_engine::context_rows` computes the next row against the live conversation, so resume
and compaction need no other state. `session_engine::context_source` reads the live context
(`instruction_files` is the one loader; `ContextRowsWiring` the engine's seam);
`session_engine::reminders` adds the per-turn `<system-reminder>` row (changed files from
`file_tracker`, fed by the kernel's `fileAccess`; the task-list nag; silent turns; elapsed
time; `<total_tokens>`; re-reads), each switchable under the `reminders` setting.
`SessionEngine::session_context_text` returns the first-turn row's text.

The factory (`factory`): state-machine workflows of spawned child agents,
host-side. `factory::spec` is the single write-time validator, dag compiler,
and canonicalizer (the kernel's `rlm.factory` validator functions are thin
clients over it, through the blocking `factory.spec` host request or, with
no serving kernel, the `prime-agent --prime-agent-harness-request` one-shot;
`factory::pyvalue` carries
the spec's Python semantics across the wire). `factory::executor` is the
executor: one per session, built in the session wiring over the session's own
`rlm.spawn` admission path, with a durable record per run under the session's
artifact dir (`factory-runs/<run id>.json`); a kernel restart never touches a
run, and a host restart reloads the records and pauses in-flight runs as
interrupted for `resume`. Every run milestone (completed, failed, paused,
interrupted by a restart, cancelled by the user) reaches the session once
through the `FactoryNotices` port as a `<task-notification>` row
(`executor::notice::FactoryNotice`, rendered by
`session_engine::messages::task_notification`, the shared builder for
background-completion notices); the embedding supplies the lane
(`SessionEngineConfig::factory_notices`: the daemon worker's follow-up
queue), and a kernel `factory.status`/`watch`/`stop` withdraws a notice
still queued for the state it read. `factory::library` is the machine library
(MACHINE.md parse/render, the repo-first listing and resolution, the import
gate, the exports) behind the kernel's `rlm.factory` library functions (the
`factory.library` request) and `prime-agent factory`. `factory::lane`
serves the daemon/TUI `factory_activity` lane from the executor (no kernel
round trip); `factory::host` registers the kernel's `factory.*` host
requests.

File checkpoints (`file_history`, Claude Code's file history behind
`/rewind`): the bundled `edit` skill reports each write around itself
(`file_history.before_write` / `file_history.after_write`, host-internal
requests registered for top-level sessions); the host backs the prior bytes
up content-addressed under `<session artifacts>/file-history/` (private
directories, pinned-handle leaf operations, 16 MiB per file and 1 GiB per
session caps) and appends each side to a write log. `FileHistory::plan`/
`plans` answer what restoring the code to a point in time would change and
`restore` applies it, recording the restore as writes and skipping a file
changed outside the agent since its last write unless forced. The write
itself stays in the kernel (sandbox and plan guard apply); bash and plain
Python writes are not tracked. `fileCheckpointingEnabled: false` or
`PRIME_AGENT_DISABLE_FILE_CHECKPOINTING=1` turns it off.

Claude Code's unreleased session modes (`session_engine::modes`, both off
unless their `experimental` setting or environment variable is on):
coordinator mode (`session_engine::coordinator`;
`experimental.coordinatorMode` / `PRIME_AGENT_COORDINATOR_MODE`) gives a root
session that does not run as an agent definition the coordinator section in
its prompt and a host-request gate (ahead of the agent-scope and feature
gates) that refuses worker capabilities (`bash.run`, `file.edit`, `web.*`,
MCP tool calls, `computer_use.*`, `artifact.present`, worktree moves, the
system router) unless `coordinatorExtraTools` names them; plain Python file
access stays possible, so the prompt carries the rule. Its flags make
`rlm.spawn` ignore `model=` and hand the nested instruction files a worker
loaded to the coordinator. Brief mode (`session_engine::brief_mode`;
`experimental.briefMode` / `PRIME_AGENT_BRIEF`, `/brief`) routes the model's
replies through `rlm.send_user_message` (`user_message.send`, a displayed
`user_message` row through the `artifact.present` row seam), records
`brief_mode_change` rows a resume restores, and continues a root run once
when it stops without a message. Both publish feature statuses (`coordinator`,
`brief`) the TUI reads.

Pull-request links (Claude Code's `linkSessionToPR`,
`session_engine::pr_link_gate`, outermost on every session's host-request
gate): a `bash.run` whose command creates a pull or merge request (`gh pr
create`, `glab mr create`, not `--dry-run`) and that finishes with exit 0
(in its own reply or a later `bash.follow` of the job) appends a `custom`
entry of type `pr_link` (`session::pr_link`: `{prNumber, prUrl,
prRepository}`, the last GitHub/GitLab/Bitbucket URL in its output) unless it
repeats the last link. The saved-session catalog folds it into
`CatalogSessionRow::pr_link`.

## Non-goals
No provider HTTP (pa-ai), no loop policy (pa-agent), no daemon supervision (pa-daemon), no TUI (pa-tui). No update coordination (the pa-cli coordinator owns the FSM; the update-flow seam here is the daemon-free support layer: `update::version` (semver/channel policy), `update::install` (the managed install-root layout), `update::release` (the channel manifest fetch), `update::download` (sha256-verified archive download + staging)). The package manager installs sources and resolves resource paths only. No workspace snapshot upload, transport, or remote materialization (staging and verification live here; the cloud attach surface owns the transport when it ships).

## Public API
`SessionEngine` (message in -> events out), `export_html` (the HTML exporter plus its renderer seam: `tools_section`, `pre_render_custom_tools` + `ToolHtmlRenderer`/`RenderedToolHtml`/`RenderedToolResult`, and `ansi_to_html::ansi_lines_to_html`/`tool_render::trim_rendered_result_lines` for renderer implementers), `ToolRegistry`, kernel manager, settings (`SettingsManager`), packages (`packages::PackageManager` + source types), `platform` (process control, file locking, permissions, shell selection - usable by higher crates, e.g. pa-cli's detached spawn), `session_engine::rlm_host::RlmSubagentHost` (implemented by pa-daemon for supervisor-backed children and by pa-cli's print mode for in-process children; the default is no children; the trait crate also owns the shared spawn/depth facts — `DEFAULT_RLM_MAX_DEPTH`, the depth-limit and name errors, the daemonless `create_session` refusal, `rlm_child_session_dir`, and the `persisted_rlm_max_depth`/`resolve_rlm_max_depth` precedence chain), `session_engine::semantic_edges` (the TS `semantic-edges.ts` WRITER: `SemanticEdgeRecorder`/`SemanticEdgeIdentity`, `semantic_edge_ledger_path` + `last_committed_request_id`, and the `X-ACP-Model-Request-ID`/`Idempotency-Key` constants — one durable per-session ledger of model-request events, byte-compatible with the TS lines; the fold and any publishing stay downstream), `mcp::McpManager` (the session's host-side MCP manager, exposed as `SessionEngine.mcp_manager`: auth gating for built-in integrations, the `mcpServers` setting seam, and the `mcp.*` host-request handlers - `mcp.config`/`mcp.refresh`/optional `mcp.begin_login` - the kernel's generic MCP client resolves through), `mcp::McpSessions` + `mcp::McpSessionOptions` + `McpManager::register_session_handlers` (the session's MCP client connections, exposed as `SessionEngine.mcp_sessions`: `close_all` on session end, `set_cwd` to follow `/cwd`), `mcp::McpManager::connection_roster` + `mcp::McpConnectionEntry` (the `/mcp` view's roster: every resolved integration with its connected state, display kind, transport, and generic-listable flag), `mcp::McpManager::service_catalog_views` + `service_catalog_diagnostics` (the `/mcp` view's resolved service-catalog cards and their diagnostics, answered from local state - the TS `buildServiceCatalogViews` parity; the picker never round-trips the kernel), `mcp::McpManager::api_key_credential_views` + `mcp::API_KEY_CREDENTIALS`/`McpCredentialView` (the `/mcp` view's api-key credential rows - the stored keys the surface manages alongside the connections, e.g. the web-search key - configured from the shared auth store), `auth::ProviderCredentialSource` + `auth::install_credential_source` (the provider credential source seam: a store outside `auth.json` the composition root installs for one provider id, consulted after the runtime `--api-key` override and before `auth.json`; it serves the request credential and its headers, refreshes under its own lock, and its failure is the provider's OAuth authentication failure; an OAuth login `auth.json` still holds for the provider is offered to it (`adopt_stored_login`) and removed from `auth.json` once the source takes custody; `remove_login` + `credential_source_providers` let `/logout` remove a source's login; nothing is installed natively), `auth::raise_auth_notice` + `auth::clear_auth_notice` + `auth::AuthNotice`/`AuthNoticeSink` + `SessionEngine::set_auth_notice_sink` (non-fatal auth notices: a refreshed login `auth.json` could not take, or a credential source's own conditions; raised while a condition stands, each heard once per session; the daemon sends them as the `auth_notice` session event, print mode as a stderr warning or a json-mode event line), `models` (the registry and resolver, plus `models::order_for_picker`: the picker-catalog order the composition root applies to its snapshot), `session_engine::provider_adapter` (`switchable_stream_fn`/`ProviderTarget`: the live provider-target slot a host swaps on `set_model`; the adapter bridges the loop's abort into pa-ai's `StreamOptions::signal` cancellation token, so a turn abort cancels the in-flight HTTP fetch at the transport — closed by `ModelStream::close` and the stream's drop), `session_engine::ipython_state` (the post-compaction `ipython_state` kernel-persistence notice: the `CompactionKernelProbe` view of the kernel provisioner the engine wiring binds via `AgentSession::set_kernel_state_probe`, and the notice row that rides `CompactRun` for the surfaces to broadcast). `session_engine::session_commands` (the `/compact`/`/refine`/`/goal`/`/autonomous` executor: the durable echo/result rows with their persistence order - the echo ahead of the command so its own work sees it - the compaction/refinement/goal/autonomous arms, and the post-execution live-context rebuild that keeps the rows in the agent state; hosts own the wire surface), `session_engine::auto_refine_trigger` (the compact-trigger auto-refine machine on `AgentSession`: the arm/discard/increment accessors and `consume_compact_auto_refine`, the shared gate/review/stamp sequence every scheduling surface applies - TS `_compactAutoRefinePending`/`_lastAutoRefineReviewAt`/`_assistantTurnsSinceAutoRefine` plus `_maybeAutoRefine`'s compact arm, the serialized checkpoint's compact step, and dispose's serialized drain, with the surface parameter carrying the one behavioral difference between a boundary and a disposal). `session_engine::goal_boundary` (the goal arms of the natural turn end on `SessionEngine`: the `--goal` construction seed with its next-turn context row - TS `_pendingNextTurnMessages` on `AgentSession` - the usage accounting, the budget-limit steer mint, the natural continuation mint with its slot rollback, the RLM deferral behind unsettled descendant work (mark/drop/take owed, the wake-up ownership probe, and the latest settled turn the post-settle deliveries decide from), and the terminal-error finish; the transports - the print driver's in-loop hook, the ACP settle loop, the daemon worker's queue - drive the one goal driver through these methods). `SessionEngine::expand_skill_submission` + `AgentSession`'s prompt-path expansion (the `/skill:<name>` submission seam, TS `_expandSkillCommand`: `skills::expand_skill_command` builds the `<skill ...>` block over the shared `pa_types::skill_blocks` parse, the accepted-turn row and the admitted user message agree on the expanded text, and the `skill_use_count` session counter counts at the admission). `factory` (`spec`: `validate_factory_spec`/`validate_factory_machine`/`compile_factory_dag`/`canonicalize_factory_spec`/`topological_order` over `pyvalue::PyValue`; `spec_ops::run_spec_op`; `library::handle_request`/`cli_dispatch` with `LibraryDirs`/`Fs`/`Raise`; `executor::FactoryExecutor` with its `ports` seams (`FactoryChildren`, `FactoryNotices` + `FactoryNotice`, `FactoryClock`) and `RunRequest`; `session_engine::messages::task_notification` + `TaskNotification` (the `<task-notification>` builder every background-completion lane renders through); `lane::activity`/`capped_reply`/`FactoryLaneContext`; `host::register_session_free_factory_handlers`/`register_factory_executor_handlers`), `SessionEngine.factory` (the session's executor). `workspace_snapshot` (`create_workspace_snapshot`: bounded, secret/nested-repo/submodule/symlink-ancestor-excluding capture of a git worktree into an owner-private staging directory, with the caller-chosen `BaselineMode` (HEAD-tree content verified against HEAD's object ids, or explicit external) and a `ConcurrentMutation` error when the worktree changes under the capture; `verify_workspace_snapshot`: offline manifest/blob/baseline integrity check; the `SnapshotManifest`/`WorkspaceSnapshot`/`SnapshotLimits`/`BaselineMode`/`SnapshotError` types). All subsystem internals `pub(crate)`; the engine is the only facade - subsystems must not import each other directly (the package manager consumes the public settings API only).

## Depends on
pa-types, pa-ai, pa-agent, pa-os-sandbox (one-way).

The Unix platform process API exposes `current_user_id()` for the daemon
endpoint namespace to match the TypeScript `process.getuid()` contract on
macOS as well as Linux. This reads the OS identity without filesystem or
subprocess work.

Automatic trace delivery (`agent_traces::ContinuousTraceUpload`) owns consent-gated
persist scheduling and startup outbox recovery alongside the existing manual uploader.
Hosts reuse `load_settings` to capture consent before their existing settings parse,
attach `persisted` after successful disk writes, and own the returned installation
for the session lifetime. Replacements use `rebind` with the effective cwd; forks
use `forked`. The installation never drains on drop. A detached process-wide runtime
owns all transcript reads, scans, settings reloads, networking and retries; a maximum
of 256 registered controllers and four deliveries bounds resident work. Admission
returns `None` at capacity and logs rejection; hosts attach no hook in that case.
Retired descriptors free capacity only after background recovery acknowledges them.
Session replacement transfers its existing slot and cancels the predecessor; forks
require a distinct slot. Replacement retains a bounded retirement flag until the
background recovery snapshot acknowledges its exact controller identity. The synchronous
persist seam checks the cached consent snapshot and performs an exists/create
pending-marker write; consent metadata reads and reloads stay in the worker after
the host captures its initial settings generation; it does not fsync the marker (matching TS v0.9.8 process-crash
recovery rather than promising power-loss durability). A short nonblocking mutation lease prevents cursor/prune races; contention
records one stable fallback marker per session, which is retained conservatively
until a future sender has delivered it. Its cost must be measured
separately; startup/paint parity is not established by scheduler unit tests.
Active requests monitor consent changes every second on the worker and cancel on
revocation/errors; bytes already sent cannot be recalled. Recovery belongs to all
live hosts sharing the agent directory and cancels when the last host disappears.
Catch-up uses at most three delivery cycles per pending entry, honors Retry-After,
and releases upload capacity during retry waits; exhaustion retains durable intent.
