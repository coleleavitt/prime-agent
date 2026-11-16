---
name: brain
description: Search every memory the agent has, across all sessions and projects (global and session-local harness memories, auto-memory and personal files, magic-context memories), and curate them (pin, exclude, mark superseded, synthesize a summary). Use before re-investigating something a past session may already have learned, when the user refers to earlier work, or when the digest says memories were withheld.
---

# Brain

One searchable view over every memory store: this agent dir's global and
session-local harness memories (every session's, not only this one's), the
auto-memory files of every project, personal memory files, and CortexKit
magic-context's memories. Results carry a `key` you can pass back.

```python
hits = await brain.search("aft shim writer lease", scope="project", limit=5)
for hit in hits:
    print(hit["key"], hit["source"], hit["updatedAt"][:10], hit["preview"])
memory = await brain.get(hits[0]["key"])      # full content
```

## API

- `await brain.search(query, scope="all", limit=10, include_superseded=False) -> list[dict]`:
  ranked memories (`key`, `source`, `title`, `preview` (220 characters), `path`,
  `project`, `sessionId`, `category`, `updatedAt`, `score`, `pinned`,
  `superseded`). `scope`: `all`, `project` (this repository's sessions, files
  and magic-context rows, plus global memories), `global`, `session` (this
  session's own), `files`, `magic-context`. At most 25 results. Superseded
  memories are left out unless `include_superseded=True`.
- `await brain.get(key) -> dict`: one memory in full, with `pinned`,
  `excluded` and `superseded`.
- `await brain.pin(key)` / `await brain.unpin(key)`: a pinned memory leads the
  digest and ranks higher in search.
- `await brain.exclude(key)` / `await brain.include(key)`: an excluded memory
  leaves the digest and search.
- `await brain.supersede(key, by)`: `key` was replaced by `by` (it leaves the
  digest and ranks down); `brain.supersede(key)` clears the record.
- `await brain.summary() -> dict`: the synthesized summaries of this project
  and of all projects (they also appear in the harness digest).
- `await brain.synthesize(scope="project") -> dict`: consolidate the scope's
  memories into its summary now (`project` or `global`). One model call.
- `await brain.status() -> dict`: memory counts per source, this project's
  identity, superseded/pinned/excluded counts.

Controls change what later sessions see: pin, exclude or supersede only what
the user asked for or what you verified is wrong or replaced. The digest
picks up changes at its next refresh (session start, resume, compaction).

## When to search

- Before re-deriving something: a build command, a root cause, a decision.
- When the user says "like last time", "we fixed this before", "remember".
- When the digest announces withheld or superseded memories you may need.
