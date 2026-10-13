# Backfill parity fixture

`home/` is a synthetic home with the three corpora `learning backfill` walks (prime sessions, Claude Code projects,
opencode storage), including the edge cases: offsets that cross midnight, naive and space-separated timestamps, a
hidden directory, an `auth.json`, sidechain/meta turns, a subagent session, a float timestamp, parts of an unknown
message, more than eight tokens in one blob.

`expected/` is what the TS-era script (`skills/trajectory-backfill/backfill.py`, removed when the generator moved
to Rust) wrote for `--home home --out expected`, with `sealedAt` pinned to `2026-10-13T00:00:00.000Z` (the run
stamp). The script crashed on JSONL lines that are not objects; those cases are covered by the crate's own tests.
