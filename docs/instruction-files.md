# Instruction files and auto memory

prime-agent reads the same instruction files Claude Code does, alongside its own AGENTS.md family, and keeps
Claude Code's per-project auto memory. Code: `crates/pa-core/src/resources/instructions/`,
`crates/pa-core/src/resources/auto_memory.rs`, `crates/pa-core/src/session_engine/instruction_boundary.rs`.

## Session-start files

Loaded in this order into the system prompt's `# Project Context` segment (later files are nearer the working
directory and take precedence when they disagree):

1. User scope: the first of `AGENTS.md`, `AGENTS.MD`, `CLAUDE.md`, `CLAUDE.MD` in the agent dir
   (`~/.prime/agent`), then `<agent dir>/rules/**/*.md`. With `claudeCodeUserInstructions: true`, also
   `~/.claude/CLAUDE.md` and `~/.claude/rules/**/*.md`.
2. Every directory from the filesystem root down to the working directory, each in this order:
   `AGENTS.md` (or `AGENTS.MD`), `CLAUDE.md` (or `CLAUDE.MD`), `.prime/agent/AGENTS.md`, `.claude/CLAUDE.md`,
   the rules under `.prime/agent/rules/` and `.claude/rules/`, then the private `AGENTS.local.md` and
   `CLAUDE.local.md` (keep these out of version control).

Each file loads at most once (a symlinked or byte-identical copy is skipped), so a repository with both
`AGENTS.md` and a `CLAUDE.md` that says `@AGENTS.md` pays for one copy. Files are capped at 256 KiB; binary
files are skipped. `--no-context-files` turns all of this off, for the session and every subagent it spawns.

### Decisions

- Both families load: before this, a directory with `AGENTS.md` never read its `CLAUDE.md`.
- Project `.claude/` is honored (it is checked into the repository like `CLAUDE.md`). The home directory's
  `.claude/` is Claude Code's user scope and stays off unless `claudeCodeUserInstructions` is set, so installing
  Claude Code never silently changes this agent's prompt.
- prime-agent's own project directory is `.prime/agent/` (`AGENTS.md`, `rules/`), next to its skills and
  `SYSTEM.md`.

## Imports

`@path` anywhere in a file's text imports another file right after it: `@AGENTS.md`, `@./docs/style.md`,
`@~/notes.md`, `@/abs/path.md`; relative paths resolve against the importing file's directory, `\ ` keeps a
space, a `#fragment` and trailing sentence punctuation are dropped. Tokens in fenced code blocks and inline code
spans are text, `user@example.com` is not an import. Imports nest up to 5 hops; each file loads once, so cycles
end. Files from a project directory may import only from inside that directory (a checked-out repository cannot
pull `~/.ssh/id_rsa` into the prompt); user-scope files may import from anywhere.

## Rules

Every `*.md` under a rules directory (subdirectories included, sorted by path) is an instruction file. A rule
whose frontmatter names `paths:` is conditional:

```markdown
---
paths:
  - "crates/**/*.rs"
  - "*.toml"
---
Rust conventions...
```

`paths` is a list or a comma-separated string; globs match gitignore-style against the path relative to the
directory holding the rules directory (`*.rs` matches at any depth, `/src/*.rs` only at the top, `*` never
crosses `/`). A conditional rule loads the first time the agent touches a matching file.

## Nested files

The instruction files of directories below the working directory are not in the prompt at start. The kernel
runtime records the files a cell opens (an audit hook on `open`; reads and writes, the `edit` skill included;
`bash()` subprocesses are not observed) and reports them on the execute `done` frame as `accessedPaths`
(additive, kernel protocol unchanged). The `python_repl` result keeps those under the working directory in
`details.accessedPaths`. At the turn boundary the session delivers the files of every directory between the
working directory and each touched file, plus the conditional rules it matches, as one hidden
`nested_instructions` row (`Contents of <path> (<kind>):` blocks) that the next model request carries. Each file
is delivered once per session; a resumed session reads the delivered paths back from the rows, and a file whose
row a compaction summarized away is delivered again on its next touch.

## Auto memory

Each project gets `<agent dir>/projects/<sanitized project root>/memory/`; the project root is the main checkout
of the working directory's git repository, so every worktree shares one memory. The directory is created at
session start and its `MEMORY.md` index (first 200 lines, then at most 25,000 bytes, with a warning when cut) is
loaded into the `auto-memory` prompt segment together with Claude Code's memory instructions: the
`user` / `feedback` / `project` / `reference` taxonomy, one file per memory with `name` / `description` / `type`
frontmatter, and a one-line pointer per file in `MEMORY.md`. The model writes these files with the tools it
already has (Python file writes, `edit`); there is no memory API.

Auto memory is on for top-level sessions; subagents do not get it. Turn it off with `"autoMemoryEnabled": false`
in settings, `PRIME_AGENT_DISABLE_AUTO_MEMORY=1`, or `--no-context-files`.

## `/memory`

Lists the files loaded at session start (kind, and the importing file for imports), the files delivered since,
the scoped rules still waiting with their globs, and the auto-memory directory with its index state.

## Not ported

Claude Code's managed (enterprise policy) instruction files, the approval prompt for external imports (imports
outside a project directory are refused instead), `claudeMdExcludes`, team memory and memory stores, and the
post-turn memory-extraction agent.
