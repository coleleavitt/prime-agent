# Context manager: magic-context's context features as `pa-context`

Status: implemented in `crates/pa-context` (fork feature `context`, on by default). Source: CortexKit
magic-context v0.49.49 (`~/WebstormProjects/forks/magic-context`, MIT): the TS transform in
`packages/plugin/src/hooks/magic-context/` is the spec; `crates/mc-core/src/decay.rs` is vendored. Research notes:
2026-11-15 session (`mc-context.md`, `pa-context-seams.md`).

## Goal

Sessions that keep working past the context window without the one-shot native compaction throwing the history
away: the model drops spent outputs itself, a background historian folds old history into compartments that
decay with age, and the full history stays one call away.

## Why a port and not `mc-module`

`crates/mc-module` (42.6k-line `lib.rs`) is a full Rust reimplementation, but bound to CortexKit's subc daemon
wire, store and lease crates, an unpublished `cortexkit-cache-core`, its own conversation-item shape, and
rusqlite 0.32 (a second `sqlite3` link next to the brain's 0.41 is refused by Cargo). Upstream marks the Rust
transform experimental and removes it in 0.50.0. Only the pure decay curve is vendored.

## Seam (pa-core, generic)

`SessionFeature::request_context_hook` returns a `RequestContextHook` whose `transform(Vec<AgentMessage>)` rewrites
the messages of every model request (tool continuations included). `create_session` composes the installed hooks
in order into the agent loop's `transform_context` (previously a pass-through), inside the request-timing
instrumentation. A hook that errors or panics leaves its input. The stored session, the TUI transcript and the
kernel's state are never touched: only the request's copy changes.

## Per-request pipeline (`transform::run`)

1. **Tags.** Every user message and tool result gets a stable number `N`, assigned once by message identity
   (`tool_call_id`; a user message's timestamp plus its occurrence) and persisted. Its first text part renders
   `§N§ ` in front. Assistant messages are never edited (signed thinking, text signatures).
2. **Permission.** Changing an earlier message invalidates the provider's prompt cache from that message on, so
   queued work (drops, new compartments) lands only on a pass allowed to rebuild: the first request of the
   process (cache cold), a request after 5 minutes idle (cache expired), usage at or above the execute threshold
   (65% of the window), or the force band (85%). Otherwise the request replays the previous decisions byte for
   byte.
3. **Compartments.** Published compartments cover a contiguous range of messages cut at user-message boundaries
   (no tool call is separated from its result). A displayed custom row (an agent message, a task notification,
   a scheduled fire) is a boundary too: a session driven by `prime-agent send` or a sibling has one user
   message, so user messages alone would leave nothing to fold. On a permitted pass the covered range is replaced by one
   synthetic user message, `<session-history>`, in place: a frozen part (`m0`, re-rendered with the decay curve
   only on a refold) and the compartments published since (`m1`, newest tier). Refold when `m1` exceeds 20% of
   the history budget or on a cold-cache pass. A compartment whose messages are gone (native compaction) renders
   nothing.
4. **Drops.** A dropped tool result keeps its message (the `tool_use` pairing) with the one placeholder
   `[dropped §N§]`; a dropped user message becomes the placeholder.
5. **Emergency.** At the force band with nothing queued, the oldest tool results outside the protected tail are
   dropped (oldest first) until the estimate is under 75%.
6. **Historian trigger.** At 63% (execute minus 2) with at least 12 messages and 6k tokens older than the
   protected tail, a background run folds that range (capped at 100k tokens) into compartments.

Usage is the last assistant reply's input + cache tokens plus a 4-characters-per-token estimate of what came after
it, over `contextWindow`. The native compaction at 95% stays the last resort; drops and compartments lower the
usage it reads, so it fires later or never.

## Historian

A side-model call (`auxiliaryModel`, else the session's model; billed to the session as `context`) with a
condensed version of magic-context's compartment prompt: the range as `§N§ U:` / `A:` / `T:` lines (tool outputs
cut to 2,000 characters), asked for `<compartment start="N" end="M" title importance="1-100">` blocks with four
tiers: `<p1>` what happened with every anchor (paths, commands, commit hashes, numbers, user words verbatim),
`<p2>` canonical anchors, `<p3>` outcome and key decision in 1-2 sentences, `<p4>` one fragment. First person,
no invention. Parsed compartments must cover the range in order; the chunk is published only whole. One run at a
time per session; a failure backs off 10 minutes, three in a row back off an hour.

## Decay (vendored)

`tier(index, importance, pressure)`: `F = 2^((imp-50)/25)`, `H = 24F/p`, `z = (index-1)/H`; P1 below 0.201, P2
below 0.729, P3 below 1.322, P4 below 2.587, else archived (not rendered). Pressure `p` is the natural cost (tier
costs 322/109/35/20 tokens) over the history budget (15% of the window). Golden-tested against the TS reference.

## Model surface (`ctx` skill, host requests)

```python
await ctx.status()                 # usage, window, biggest undropped outputs, queued drops, compartments
await ctx.reduce("3-5,9")          # queue drops; returns what was queued and when it applies
await ctx.expand(12)               # one item's original text (dropped or folded)
await ctx.expand(start=40, end=60) # raw transcript of a tag range, capped at ~15k tokens
await ctx.flush()                  # let the next request apply queued work now (one cache rebuild)
```

The newest 3 tags are protected from `reduce`. Nudges (magic-context's channel 1, adapted): after a tool batch, a
hidden context row when the messages since the last fold hold at least 60k tokens and undropped tool output at
least 25k, gentle/firm/urgent at 20/40/60% of that, refired after 25k tokens of growth, naming the oldest outputs.

## State

`<session artifacts>/context/state.json` (schema 1, atomic write): the tag book, dropped and queued tags,
compartments (range keys, title, importance, tiers), the frozen `m0` text and the compartment count it covers,
the historian's failure count and next-allowed time. Resume rebuilds the same messages, so the same state renders
the same bytes.

## Scope and switches

Top-level sessions only (`rlm_depth == 0`); RLM children keep their messages. `PRIME_AGENT_CONTEXT_MANAGER=0`
turns the transform off; `PRIME_AGENT_CONTEXT_HISTORIAN=0` keeps tags and drops but runs no historian.

## Deferred

`ctx_note` and smart notes, primers, caveman compression, reasoning clearing, duplicate-output dedup, the
two-message `m0`/`m1` split across separate messages, channel-2 nudges, provider-overflow learning, blocking the
turn at 95% until the historian finishes.
