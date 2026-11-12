# pa-retrieval

Learning memory retrieval: the harness digest of a daemon session lists its editable entries in an order a flyrag
learning reranker picks for the turn, and the reranker learns from what the refinement history and the session
transcripts say about the memories it showed.

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
- The session feature `RetrievalFeature`, which serves daemon-worker sessions only.

## Non-goals

- No digest rendering: `pa-core` renders the digest and applies the hook; without one the order is native.
- Daemonless (print-mode) sessions: they keep the native order and write nothing here.
- No prompt text, host request, CLI command, slash command, TUI surface, or settings key.
- No cross-process locking of the persisted state: two processes on one agent dir each save their own (atomic
  rename, last writer wins).

## Seams

- `pa_core::features::SessionFeature::harness_rerank_hook`: the agent dir's shared reranker as the session's
  `HarnessRerankHook`, for a session whose `SessionFeatureContext::daemonless` is false.
- `SessionFeature::on_agent_stop`: the tick.
- `pa-cli` installs `RetrievalFeature` behind the Cargo feature `retrieval` (on by default; it requires `ledger`).

## Public API

`RetrievalFeature` (`new`, the `SessionFeature` implementation); the retriever: `Reranker`, `FlyRagReranker`,
`BasicReranker`, `Bm25Index`, `Signal`, `extract_signals`, `Candidate`, `OfferId`, `Outcome`, `Query`, `Ranked`,
`RerankResult`, `Result`, `RetrievalError`, and the `bm25`, `flyrag`, `signal` modules. The handle, taps, seed,
backfill and tick are private.

## Dependencies

`pa-core` (the session-feature seam, harness types) and `pa-ledger` (its prerequisite: the silent-failure classifier
the self-correction tap reads, the harness-state file name, its timestamp format).

## Files owned

Under `<agentDir>/harness/`: `retrieval_state.json` (the reranker), `retrieval_novelty.jsonl` (one row per memory
create/update the tick saw), the cursors `retrieval_memory_sync.cursor`, `retrieval_memory_sync_local.cursor`,
`feedback_tap.cursor`, `feedback_tap_local.cursor`, `user_correction_tap.cursor`, `self_correction_tap.cursor`, and
the backfill sentinel `targets_fingerprints_backfill.done`; the backfill also adds `targetsFingerprints` to memory
entries of `harness_state.json`. It reads, never writes, the refinement histories, `digest_memories.jsonl` (both
written by `pa-core`) and the session transcripts.

## Telemetry

None yet: the feature has no adoption event.
