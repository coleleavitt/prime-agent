# pa-retrieval

Learning memory retrieval: the harness digest of a daemon session lists its editable entries in an order a flyrag
learning reranker picks for the turn, and the reranker learns from what the refinement history and the session
transcripts say about the memories it showed. On top of it, the brain (`docs/brain-design.md`): one searchable,
curated view over every memory store, for every session.

## Scope

- The retriever (`bm25`, `flyrag`, `signal`): a BM25 base retriever, the flyrag re-ranker (fly mushroom-body sparse
  codes, two opposing readout banks, bounded updates, lazy decay; the pinned `flyrag` and `kenyon` crates), and the
  extraction of `Helped`/`Misled` signals from refinement records.
- The reranker handle: one `FlyRagReranker` per agent dir and process behind a mutex, its rerank hook for the
  digest, and its persisted state. A fresh handle that loads no state is seeded from the memory entries in
  `harness_state.json`, and the one-shot `targetsFingerprints` backfill runs (sentinel-gated).
- The tick, run at every agent stop on its own thread (one at a time): the feedback tap (new records of
  `refinement_history.jsonl` and `local_refinement_history.jsonl`), the user- and self-correction taps (the newest
  session transcript and `digest_memories.jsonl`), the memory upserts and removals of new records with their
  admission-time novelty, periodic compaction, and the save.
- The brain (`src/brain/`):
  - the corpus: global and every session's local harness memories, every project's auto-memory files, personal
    memory files, and CortexKit magic-context's active rows (read-only, `rusqlite`), re-read by file stamp;
  - project identity as magic-context's `git:<smallest root commit>`, a session's project from its transcript
    header's `cwd`;
  - search: BM25 over the corpus, source and current-project boosts, flyrag's learned term (read without an offer),
    pins; excluded memories never surface, superseded ones only on request;
  - supersession: recorded (`brain.supersede`), declared (`metadata.supersededBy`), a `<stem>_part_<N>` series
    keeps its highest part, identical normalized content keeps the newest copy; each newly superseded harness
    memory gets one detached `Misled` (at the tick);
  - controls and synthesized summaries as the `memoryControls` / `memorySynthesis` extension keys of the global
    `harness_state.json` (written under its lock);
  - synthesis: a side-model consolidation of a scope's memories into at most 20 one-line bullets (guarded against
    credential-shaped lines), on `brain.synthesize` and automatically at agent stop when a scope's summary is at
    least 24 h old and 5 of its sessions started since (top-level daemon sessions, lock file, stops after 3
    consecutive failures; `PRIME_AGENT_BRAIN_AUTO_SYNTHESIZE=0` turns it off).
- The session feature `RetrievalFeature`: the rerank hook and tick for daemon-worker sessions; the brain's host
  requests, skill and digest hook for every session.

## Non-goals

- No digest rendering: `pa-core` renders the digest and applies the hooks; without them the digest is native.
- Daemonless (print-mode) sessions keep the native order, run no tick and no automatic synthesis; they still get
  the brain's requests and digest hook.
- No memory writes: harness entries keep RAVO as their one writer, files the model, magic-context its own process.
- No CLI command, slash command, TUI surface, or settings key.
- No cross-process locking of the reranker's persisted state: two processes on one agent dir each save their own
  (atomic rename, last writer wins). The brain's own writes go through the harness file lock.

## Seams

- `pa_core::features::SessionFeature::harness_rerank_hook`: the agent dir's shared reranker as the session's
  `HarnessRerankHook`, for a session whose `SessionFeatureContext::daemonless` is false.
- `SessionFeature::harness_prompt_hook`: the brain's pins (lead rank), withheld excluded and superseded memories, and
  the synthesized summaries of this project and of all projects as sections. Empty brain state: byte-identical
  digest.
- `SessionFeature::register_host_handlers`: `brain.search`, `brain.get`, `brain.pin`, `brain.unpin`, `brain.exclude`,
  `brain.include`, `brain.supersede`, `brain.summary`, `brain.synthesize`, `brain.status`.
- `SessionFeature::bundled_skills`: the `brain` skill (`skills/.features/brain`).
- `SessionFeature::on_agent_stop`: the demotion of newly superseded memories, the tick, the automatic synthesis.
- `pa-cli` installs `RetrievalFeature` behind the Cargo feature `retrieval` (on by default; it requires `ledger`).

## Public API

`RetrievalFeature` (`new`, the `SessionFeature` implementation); the retriever: `Reranker`, `FlyRagReranker`
(including `learned_prior`), `BasicReranker`, `Bm25Index`, `Signal`, `extract_signals`, `Candidate`, `OfferId`,
`Outcome`, `Query`, `Ranked`, `RerankResult`, `Result`, `RetrievalError`, and the `bm25`, `flyrag`, `signal` modules.
The handle, taps, seed, backfill, tick and brain are private.

## Dependencies

`pa-core` (the session-feature seam, harness types, side model, auto-memory paths), `pa-ledger` (its prerequisite:
the silent-failure classifier the self-correction tap reads, the harness-state file name, its timestamp format),
`rusqlite` with `bundled` SQLite (the magic-context source; synchronous because the digest hook is), and `tokio`
(the blocking pool for the brain's requests, the automatic synthesis task).

## Configuration

- `PRIME_AGENT_BRAIN_MAGIC_CONTEXT_DB`: another magic-context database, or `0`/`off` to skip the source (default
  `$XDG_DATA_HOME/cortexkit/magic-context/context.db`, else `~/.local/share/...`).
- `PRIME_AGENT_BRAIN_AUTO_SYNTHESIZE=0`: no automatic synthesis. Synthesis uses `auxiliaryModel` when set, else the
  session's model, and bills the session (`SideBill` source `brain`).

## Files owned

Under `<agentDir>/harness/`: `retrieval_state.json` (the reranker), `retrieval_novelty.jsonl` (one row per memory
create/update the tick saw), the cursors `retrieval_memory_sync.cursor`, `retrieval_memory_sync_local.cursor`,
`feedback_tap.cursor`, `feedback_tap_local.cursor`, `user_correction_tap.cursor`, `self_correction_tap.cursor`, the
backfill sentinel `targets_fingerprints_backfill.done`, `brain_demoted.json` (memory ids already demoted as
superseded) and the run marker `brain_synthesis.lock`; the backfill also adds `targetsFingerprints` to memory entries
of `harness_state.json`, and the brain owns its `memoryControls` and `memorySynthesis` keys. It reads, never writes,
the refinement histories, `digest_memories.jsonl` (both written by `pa-core`), the session transcripts, the
session-local harness files, the auto-memory and personal memory files, and magic-context's database.

## Telemetry

None yet: the feature has no adoption event.
