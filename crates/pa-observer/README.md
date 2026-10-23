# pa-observer

Observer agents: a background observer model reads a read-only digest of each turn of the session it observes and
reports only when it has something useful; the expected steady state is silence. A fork feature crate (see
`docs/fork-feature-crates.md`); built into `prime-agent` through `pa-cli`'s `observer` Cargo feature (on by
default), absent from `--no-default-features`, and inert unless a session's settings turn it on.

Claude Code 2.1.296 reference: `t53z3rcv:Jbn`/`j`/`U` (gate, observer prompt, digest), tool `tkdb70xb`
(`ObserverReport`), `7y9jhgf9:Vdo` (how the observed agent reads a report); flag
`CLAUDE_CODE_EXPERIMENTAL_OBSERVER_AGENTS`.

## Scope

- Collect each observed session's finalized messages as activity items (user messages, assistant text, tool calls,
  tool results, non-user rows; the observer's own notices never feed back).
- At every run end, render Claude Code's nonce-tagged `<main-activity>` digest (2,000-character items, a
  64,000-character delivery cap that drops the oldest items) and send it to the observer: a tool-less model
  conversation kept per session (the last 16 exchanges).
- The observer's reply is its report (`NO_REPORT` is silence). A report reaches the observed session as a
  `feature_notice` row with Claude Code's one-way-advisory framing: it joins a running turn at its next boundary or
  wakes an idle session.

## Settings

All under the `experimental` settings object (`pa_core::settings::experimental`):

- `observer` (or `PRIME_AGENT_EXPERIMENTAL_OBSERVER_AGENTS=1`): observe sessions up to RLM depth 2 (Claude Code's
  fan-out cap). Off by default.
- `observerModel`: `provider/model-id` of the observer; unset uses the session's model.
- `observerInstructions`: supplemental postamble appended to each digest (Claude Code's `observerMessage`).

## Non-goals

- No observer tools: Claude Code's observer is a full agent with read-only tools and an `ObserverReport` tool; here
  the observer only reads digests and replies.
- No per-agent-definition pairing (`observer:` in an agent definition) and no coordinator routing: one observer per
  observed session, configured by settings.
- Telemetry: `observer_session` (schema v5) when an observed session ends: digest, report and failure counts only.

## Public API

`ObserverFeature` (`pa_core::features::SessionFeature`; `new`, `with`, `with_nonce`), `ObserverSettings`,
`SettingsReader`, `ModelFactory`, the digest helpers (`activity_of`, `render_digest`, `system_prompt`, `report_of`,
`report_notice`, `ActivityItem`, `POSTAMBLE`, `ITEM_CHARS`, `DELIVERY_CHARS`, `OBSERVER_NOTICE_FEATURE`),
`MAX_OBSERVED_DEPTH`, `HISTORY_EXCHANGES`.

## Seams

`SessionFeature::{on_session_start, on_message_end, on_agent_end, on_session_end}`,
`pa_core::features::deliver_feature_notice`, `pa_core::session_engine::side_model`.
