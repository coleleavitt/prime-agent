# pa-cc-import

Claude Code sessions in prime-agent: list the transcripts Claude Code wrote under `~/.claude/projects` beside the
saved sessions, and continue one by importing it into a new prime-agent session file. The transcript is only read,
never written.

Wired by `pa-cli` behind `feature = "cc-import"` (default on): `pa_cc_import::install()` registers
`ClaudeCodeSessions` with the native seam `pa_core::session::external_sessions`. `--no-default-features` installs no
source: the daemon does not advertise `external_sessions`, the agents view shows no Claude Code section and
`--resume` never looks at Claude Code.

## Scope

- **Where**: `$CLAUDE_CONFIG_DIR/projects`, else `~/.claude/projects` (`projects_root`). A project's directory is
  Claude Code's exact name for its cwd (`pa_core::resources::auto_memory::sanitize`: every UTF-16 unit that is not an
  ASCII letter or digit becomes `-`, a name over 200 keeps its head plus the base-36 Java string hash). Only
  top-level `<uuid>.jsonl` files list; subagent transcripts live in subdirectories. A test process can never resolve
  the real directory (`test_isolation::guard_state_path`).
- **Format** (`transcript`, traced from Claude Code 2.1.296's `loadFullLog`/`processEntry`): conversation entries
  `user`/`assistant`/`system`/`attachment` (`uuid`, `parentUuid`, `isSidechain`, `isMeta`, `timestamp`, `cwd`,
  `gitBranch`, `version`, `message`); `progress` entries are skipped through in the parent chain; metadata entries
  `custom-title`, `ai-title`, `agent-name`, `tag`, `summary`, `last-prompt`; every other type
  (`file-history-snapshot`, `pr-link`, `cost-state`, ...) and every malformed line is ignored. The conversation is
  the parent chain from the newest leaf (the latest-timestamp main-chain user/assistant entry no other entry names as
  its parent). A `compact_boundary` system entry (null `parentUuid`) continues at its `logicalParentUuid`, so the
  history before a compaction stays in the chain. Sidechain entries never join it.
- **Listing** (`listing`): one row per session, newest first, with Claude Code's resume-picker title priority
  (agent name, custom title, AI title, the last summary; else the last recorded prompt, else the first real prompt,
  read like Claude Code: meta rows, tool results and harness-tagged text skipped, a slash command as `/name args`, a
  `!` command as `! cmd`, clipped at 200 characters), first and last timestamps, the last git branch, and Claude
  Code's message count over the conversation path. Sidechain transcripts and files without a conversation are left
  out, and so are transcripts whose own cwd is another directory with the same sanitized name. Rows are cached per
  file by size and modification time.
- **Finding** (`find`): an exact session id (any project), a unique id prefix of 8+ characters (this project), then
  an exact custom or AI title ignoring case (this project, then every project).
- **Import** (`import_transcript`, `convert`): a new session file `<sessions dir>/<uuid v7>.jsonl` whose header
  records `importedFrom: { source: "claude-code", sessionId, path, leafUuid, version }`, the transcript's cwd and
  branch. Reopening an unchanged session (same id and leaf) reuses its earlier import; a session that grew since
  imports again.
  - User prompts become `user` messages (text and base64 images; documents named). Meta and transcript-only rows are
    dropped.
  - A reply (every entry sharing one `message.id`, sibling branches included, like Claude Code's parallel-tool
    recovery) becomes one `assistant` message with `provider: "anthropic"` and Claude Code's model id, so resuming
    continues on the same model when this install can reach it (otherwise the usual "Could not restore model"
    fallback applies). Usage is zero: the spend was Claude Code's. Claude Code's API-error rows are dropped.
  - Thinking blocks are dropped (their signatures cover turns whose tool calls are rewritten here, which the provider
    would refuse). Claude Code's tools do not exist in this harness, so each tool call becomes a text record in the
    reply (`[Claude Code tool call: Bash]` + `$ cmd`, other tools their JSON input, 4,000 characters at most) and its
    result a `claude_code_tool_result` custom message after the reply (`[Claude Code tool result: Name]` or `tool
    error`, 20,000 characters at most); the model reads it as a user turn.
  - A compact boundary and its `isCompactSummary` message become a `compaction` entry (`tokensBefore` from
    `compactMetadata.preTokens`) that keeps nothing before it in the model's view: the model sees the summary and
    what followed, the transcript still shows the earlier history.
  - The session is named after the Claude Code title (when it has one), and a closing `claude_code_import` custom
    message tells the model where the conversation came from.

## Non-goals

- Writing to Claude Code's files, or keeping an import in sync with a transcript that grows later (a grown session
  imports again).
- Subagent (sidechain) transcripts, `--fork-session`, Claude Code's file-history snapshots, todos and PR links.
- Mapping Claude Code's tools onto `python_repl` calls: their inputs and results do not describe a kernel cell.

## Public API

`install`, `ClaudeCodeSessions` (the `ExternalSessionSource`; `root` overrides the projects directory),
`projects_root`, `project_dir`, `list_project`, `list_all`, `summarize`, `find_by_id`, `find`,
`import_transcript`, `SessionSummary`, `SOURCE_ID` (`claude-code`), `SOURCE_LABEL` (`Claude Code`),
`IMPORTED_FROM_KEY`, `TOOL_RESULT_CUSTOM_TYPE`, `IMPORT_NOTE_CUSTOM_TYPE`.

## Seams

- `pa_core::session::external_sessions` (native, generic): the source registry. The daemon's
  `list_external_sessions` / `import_external_session` commands (capability `external_sessions`, advertised only
  while a source is installed) serve the agents view; `pa-cli`'s resume selector (`--resume`, `/resume`, print-mode
  `--resume`) tries the sources after the saved sessions.

## Files

Reads `~/.claude/projects/**`. Writes only the imported session files in the sessions dir.

## Telemetry

None of its own; an import logs one `pa_cc_import` info event (ids only), and the agents view records the
`external_session_imported` action, counted into `tui_agents_action_count` with the view's other actions.
