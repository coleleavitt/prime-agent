# Brain: one memory store across sessions, projects and harnesses

Status: implemented in `crates/pa-retrieval/src/brain/` (fork feature `retrieval`). Research inputs (2026-11-15):
Claude Code 2.1.316's memory pipeline (`extract_memories`, `auto_dream`, the relevant-memory selector), claude.ai
web's memory client (`/memory/{synthesize,controls,reset}`, the `saffron`/`melange` tiers; extraction is
server-side and not in the bundle), CortexKit magic-context v0.49.49 (SQLite + FTS5 + MiniLM, historian and dreamer),
the fork's own jfc (`jfc-memory`, `jfc-economy`), and Letta, Mem0, Zep/Graphiti and Cognee.

## Problem

prime-agent already writes memories in several places, but every reader sees one shelf:

| store | written by | who could read it before |
|---|---|---|
| global harness memories, `<agent>/harness/harness_state.json` | refinement + RAVO | every session's digest |
| session-local harness memories, `<agent>/session-artifacts/<id>/harness/harness_state.json` (416 in 120 sessions) | refinement + RAVO | **that session only** |
| auto memory, `<agent>/projects/<slug>/memory/*.md`, and `<agent>/memory/personal/*.md` | the model (`MEMORY.md` discipline, `rlm.memory`) | that project's sessions |
| magic-context, `~/.local/share/cortexkit/magic-context/context.db` (3,040 active rows, 712 for this repo) | the TS pi/OpenCode plugin | **nothing in the Rust binary** |

The flyrag reranker is one per agent dir and learns from every session, but it only reorders the entries the
current digest already holds. There was no way for the model to ask "what did we learn about X anywhere", no
synthesized overview, no user pins, and superseded memories (handoff series `..._part_2` .. `_part_5`) all stayed
live.

## Shape

Everything lives in `pa-retrieval` (already installed by `pa-cli`, feature `retrieval`); no pa-core, kernel
protocol, daemon wire or system-prompt change.

```
  sources (read-only)                 brain (pa-retrieval/src/brain)
  ------------------                  ------------------------------
  global harness_state.json  ─┐       corpus.rs     BrainDoc per memory, cached by (path, mtime, len)
  session harness_state.json ─┤       mc.rs         magic-context rows (rusqlite, read-only)
  projects/*/memory/*.md     ─┼──▶    supersede.rs  explicit, series and duplicate supersession
  memory/personal/*.md       ─┤       search.rs     BM25 over the corpus, flyrag learned prior, boosts
  magic-context context.db   ─┘       controls.rs   pins / excludes / supersede map + synthesized summaries
                                      hook.rs       harness prompt hook: pins, withheld, summary section
                                      synth.rs      side-model consolidation (manual + gated auto)
                                      host.rs       brain.* host requests
                                      skills/.features/brain   the model-facing `brain` skill
```

## Decisions

1. **Pull, not push.** The digest stays small and cache-stable: cross-session memories reach the model through
   `brain.search` (and the synthesized summary section), not by widening the digest's candidate pool. Widening
   would churn the digest fingerprint whenever any session edits a memory and fight the 3-per-kind cap.
2. **No new store for memories.** The corpus is rebuilt from the sources on demand (mtime-cached); the brain owns
   only control state. Memories keep exactly one writer each (RAVO for harness entries, the model for files,
   magic-context for its DB).
3. **Control state rides `harness_state.json` extension keys** (`memoryControls`, `memorySynthesis` in the global
   file), which every existing writer round-trips untouched, written under the file's lock
   (`pa_core::refinement::update_harness_state`). This is the claude.ai web contract: one synthesized text per
   scope, a separate `controls` field, a `synthesized: true` marker.
4. **Supersession is explicit first** (magic-context's `superseded_by`, jfc's status field):
   - `memoryControls.superseded[old] = new` (the `brain.supersede` request), an entry's `metadata.supersededBy`,
     and magic-context's archived rows (never loaded);
   - then two mechanical rules for legacy data: a `<stem>_part_<N>[_<rest>]` series keeps only its highest N, and
     identical normalized content (whitespace and case folded, at least 24 characters) keeps only the newest copy.
   A superseded memory is withheld from the digest (announced as `+n superseded`), left out of search unless
   `include_superseded` (then ranked down and flagged with `superseded`), and gets one detached `Misled` in flyrag
   (tracked in `harness/brain_demoted.json`).
5. **Ranking** follows magic-context's caps: BM25 over title + content (every matching memory, filtered to the
   scope first), normalized to the best in-scope score, times a
   source boost (global 1.3, session 1.2, project file 1.25, personal 1.25, magic-context 1.1), times 1.2 for the
   current project, times `1 + 0.25 * learned` (flyrag's learned term for harness memories, read without
   allocating an offer), times 0.3 when superseded. Default 10 results, max 25, 220-character previews;
   `brain.get` returns full text.
6. **Project identity** is magic-context's: `git:<lexicographically smallest root commit>`, so the same key selects
   this repo's magic-context rows, its auto-memory slug and its sessions (a session's cwd from its transcript
   header).
7. **Synthesis** (`brain.synthesize`, and automatically) consolidates one project's memories (plus the global ones)
   into at most 20 one-line bullets of at most 140 characters, with Claude Code `auto_dream`'s rules (merge,
   resolve contradictions toward the newest evidence, absolute dates, no speculation) and magic-context's curate
   rules (never invent, keep the survivor). The output passes a guard (line length, no credential-looking
   strings, jfc's verifier) before it is stored. Auto-synthesis: top-level daemon sessions only, at agent stop,
   when the project's summary is at least 24 h old and at least 5 of its sessions started since (Claude Code's
   gate), under a lock file, with a 10-minute scan throttle and a stop after 3 consecutive failures.
   `PRIME_AGENT_BRAIN_AUTO_SYNTHESIZE=0` turns the automatic run off. The model is `auxiliaryModel` when set,
   else the session's model; the call bills the session (`SideBill` source `brain`).
8. **rusqlite, not sqlx.** The digest hook is synchronous; SQLite is an in-process file, so an async driver adds a
   worker thread and channels and nothing else. `rusqlite 0.41` with `bundled` (MIT; libsqlite3-sys 0.39 bundles
   SQLite 3.54.1, past RUSTSEC-2026-0459), opened `SQLITE_OPEN_READ_ONLY` with `query_only`; a missing or locked DB
   is skipped.

## Live check (2026-11-15, a copy of the real agent dir, magic-context's real DB read-only)

3,673 memories: 3,040 magic-context, 416 session-local in 120 sessions, 147 global, 69 project files, 1 personal;
718 belong to this repository (`git:a74c5da...`, the same key magic-context uses). First refresh 4.5 s (one `git
rev-list` per distinct session cwd, then cached), searches after it 45-60 ms.

## Model surface (`brain` skill)

```python
hits = await brain.search("aft shim writer lease", scope="project", limit=5)
doc  = await brain.get(hits[0]["key"])
await brain.pin(key) ; await brain.unpin(key) ; await brain.exclude(key) ; await brain.include(key)
await brain.supersede(old_key, new_key)
summary = await brain.summary()            # this project's + the global synthesized summary
await brain.synthesize()                   # consolidate now
```

Keys are namespaced: `global:<id>`, `session:<session id>:<id>`, `file:<slug>/<name>`, `personal:<name>`,
`mc:<row id>`. Scopes: `all` (default), `project`, `global`, `session` (this session), `files`, `magic-context`.

## Deferred

- A per-turn auto-search hint (magic-context's background `ctx_search` on each user message); needs a user-message
  hook in the session engine.
- Reward from mechanical build outcomes (jfc's bounty scoring idea): pa-recall's CURRENT/EXPIRED build claims as
  Helped/Misled for the memories shown that turn.
- Unused-memory demotion (jfc's 120-day rule) once `brain.search` hits are journaled.
- Embeddings (magic-context's MiniLM vectors are in its DB; using them needs the same encoder on the query side).
