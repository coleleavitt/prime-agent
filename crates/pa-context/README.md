# pa-context

The context manager: CortexKit magic-context's context features (`docs/context-manager-design.md`). Every model
request of a top-level session carries stable `§N§` tags on user messages and tool outputs; the model drops spent
outputs with `ctx.reduce`; a background historian folds old history into compartments rendered at a tier that decays
with age; `ctx.expand` brings any of it back. Queued changes land only on requests whose prompt cache is cold anyway.

## Scope

- The per-request pipeline (`transform`): tags, the rebuild permission (cold cache: first request or 5 minutes idle;
  65% usage; `ctx.flush`), applied compartments replaced by one `<session-history>` message, drops as the one
  placeholder `[dropped §N§]`, the emergency drop of the oldest outputs at 85%, the historian's next range at 63%.
- The historian (`historian`): a side-model call (`auxiliaryModel`, else the session's model; billed as `context`)
  with a condensed magic-context compartment prompt; a reply publishes only whole and only while the history it
  continues is still the published one; failures back off 10 minutes, an hour after three.
- The decay curve (`decay`, vendored from magic-context's `crates/mc-core/src/decay.rs`, MIT,
  `LICENSE-magic-context`, golden-tested against the TS reference).
- The reduce nudge after tool batches; the `ctx.*` host requests and the `ctx` skill.

## Non-goals

- No change to the stored session, the TUI transcript or the kernel's state: only the request's copy.
- RLM children (`rlm_depth > 0`): their messages pass through untouched.
- Assistant messages are never rewritten (signed thinking, text signatures).
- Not yet: notes, primers, caveman compression, reasoning clearing, dedup, blocking at 95% for the historian (the
  native compaction stays the last resort).

## Seams

- `pa_core::features::SessionFeature::request_context_hook`: the transform.
- `register_host_handlers`: `ctx.status`, `ctx.reduce`, `ctx.expand`, `ctx.flush`; `bundled_skills`: `ctx`.
- `after_tool_batch`: the nudge; `after_compaction`: forget compartments (their messages are gone).
- `pa-cli` installs `ContextFeature` behind the Cargo feature `context` (on by default).

## Public API

`ContextFeature` (`new`, the `SessionFeature` implementation), `CONTEXT_MANAGER_ENV`, `CTX_SKILL`.

## Configuration

`PRIME_AGENT_CONTEXT_MANAGER=0` turns the feature off; `PRIME_AGENT_CONTEXT_HISTORIAN=0` keeps tags and drops but
runs no historian.

## Files owned

`<session artifacts>/context/state.json`: tags, dropped and queued tags, compartments, the frozen history text, the
historian's backoff, the nudge's band.

## Telemetry

None yet.
