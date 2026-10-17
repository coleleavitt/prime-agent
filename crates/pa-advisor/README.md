# pa-advisor

Advisor escalation: the agent asks a stronger configured model for guidance, with its conversation as the context. A
fork feature crate (see `docs/fork-feature-crates.md`); built into `prime-agent` through `pa-cli`'s `advisor` Cargo
feature (on by default), absent from `--no-default-features`, and inert unless an advisor model is configured.

Claude Code 2.1.296 reference: `qwvy7ma3:bE`/`ITe` (the `advisor` server tool, beta `advisor-tool-2026-03-01`, and
its "# Advisor Tool" guidance), `5bcjy17x` (`/advisor`); flag `CLAUDE_CODE_ENABLE_EXPERIMENTAL_ADVISOR_TOOL`.

## Scope

- Serve the kernel host request `advisor.ask` (`{question}` -> `{advice, model}`), which the bundled `advisor`
  skill's `advisor.ask(question)` sends (`skills/.features/advisor`).
- Keep each session's finalized messages (the last 400) and render them as a bounded transcript (120,000
  characters, newest kept) for the advisor.
- Ask the model `experimental.advisorModel` (or `PRIME_AGENT_ADVISOR_MODEL`) names, resolved through the model
  registry with its credentials (`pa_core::session_engine::side_model`).

## Settings

- `experimental.advisorModel` / `PRIME_AGENT_ADVISOR_MODEL`: `provider/model-id`. Unset: no handler registered, the
  skill hidden (off means absent).

## Non-goals

- Not Anthropic's server-side advisor beta: the call is host-side, so any configured provider can advise, and the
  advisor reads a rendered transcript rather than the raw request.
- No `/advisor` picker and no capability ranking of advisor versus base model.
- No adoption telemetry yet (a new event bumps the `pa-telemetry` schema; left to the merge).

## Public API

`AdvisorFeature` (`pa_core::features::SessionFeature`; `new`, `with`), `ModelFactory`, `SelectorReader`,
`consultation_prompt`, `ADVISOR_REQUEST`, `ADVISOR_SKILL`, `ADVISOR_SYSTEM_PROMPT`, `TRANSCRIPT_CHARS`,
`MAX_QUESTION_CHARS`.

## Seams

`SessionFeature::{register_host_handlers, on_session_start, on_message_end, on_session_end, bundled_skills,
session_skill_visible}`, `pa_core::session_engine::side_model`.
