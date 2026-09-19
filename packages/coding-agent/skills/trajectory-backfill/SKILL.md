---
name: trajectory-backfill
description: Offline, read-only generator of cross-tool baseline data for the Engineer Trajectory Index. Walks the prime, Claude Code, and opencode session corpora on this machine and writes LearningDay-shaped day files under learning/backfill/. Use when bootstrapping or studying the trajectory index across tools; never on the turn path.
---

# Trajectory Backfill

`backfill.py` reconstructs a coarse, cross-tool failure history from the session
corpora already on this machine and writes it in the same day-signature schema
the Engineer Trajectory Index (ETI) reads, so `prime-agent learning trajectory
--include-backfill` can fold it into its printed table.

It is a study/bootstrapping tool, not part of any live loop:

- **Offline.** It calls no model provider and touches no network.
- **Read-only.** It only reads the corpora; it never writes back into them.
- **Secret-safe.** `auth.json` and `anthropic-auth-state.json` are never opened,
  and only STRUCTURAL error-class tokens (exception class names, well-known
  failure strings) are emitted — never message content.
- **Confounded by construction.** Every datum is corpus/tool-tagged with a
  synthetic fingerprint that can never collide with a runtime ledger id, so the
  ETI reader flags all three confounds (task-mix, tool-surface,
  measurement-instrument) on these windows. Cross-tool comparison is never
  presented as a clean derivative.

## Run it (from the Python REPL)

```python
bash("python3 skills/trajectory-backfill/backfill.py")
```

It walks three corpora, bucketing every record by UTC calendar day:

- prime   — `~/.prime/agent/sessions/**/*.jsonl`
- claude  — `~/.claude/projects/**/*.jsonl` (honors `isSidechain`/`isMeta`)
- opencode — `~/.local/share/opencode/storage` (session/message/part join)

and writes one file per (corpus, day):

```
~/.prime/agent/learning/backfill/<corpus>/<YYYY-MM-DD>.json   (mode 0600)
```

Each file is a `LearningDay`: `{schema, day, sealedAt, turns, parseErrors,
sourceFiles: [], commits: [], fingerprints: [...]}`. `commits` is always empty —
a backfill can never prove a `refinement.committed`, so a DROPPED label can
never be falsely earned from it. Each fingerprint is
`sha1(corpus + "\0" + tool + "\0" + token)[:16]` with `status: "error"`,
`failure: true`.

### Flags

- `--corpus prime|claude|opencode` — limit to one corpus (repeatable).
- `--out <dir>` — output dir (default `~/.prime/agent/learning/backfill`).
- `--prime-dir` / `--claude-dir` / `--opencode-store` — override a corpus root.
- `--home <dir>` — base for the path-collapse and the default roots.

## Read it back

```bash
prime-agent learning trajectory --include-backfill
```

The backfill days are read per-corpus and never summed with the live prime days;
they exist only for the CLI table. They are **never** written into
`trajectory.json` and **never** reach the harness prompt.
