# pa-refusal

The content filter: a pre-flight refusal predictor that scores every outbound `anthropic` request and filters or
rewrites the request body before it is sent. A Rust port of the TS `anthropic-auth` fork's refusal engine
(`packages/core/src/`).

## Scope

- `ContentFilter` (`filter`): keyword classification per category, scoring, rewrite rules, size-based eviction with
  category-aware floors, dynamic threshold scaling. Its weights and the eviction-floor ladder come from
  `src/config.jsonc`.
- `SurrogateModel` (`surrogate`): hashed word n-gram features into a logistic model (`src/model.json`) that predicts
  P(refuse) for a request body. Deterministic arithmetic, no network.
- `RefusalGuard` (`guard`): the closed loop. Filter, ask the surrogate, and while the body still looks
  refusal-bound, re-filter with lower eviction floors and keep the lowest-scoring body. Fail-open: a surrogate error
  sends the plainly filtered body.
- `FeedbackEvent` (`logger`): JSONL event records of the filter's decisions.
- `RefusalHooks`: the `pa_ai::request_hooks::ProviderRequestHooks` implementation that runs the guard in `prepare`
  and replaces the payload when it filtered.

## Non-goals

- No refusal handling after the fact: provider refusals and `content_filter` stop reasons are classified natively
  (`pa-core`'s error classification, `pa-ledger`'s actionability tally).
- No settings key of its own: the `contentFilter` setting lives in `pa-core`'s settings; this crate never reads
  settings.
- No other provider than `anthropic`, no prompt text, no TUI surface, no telemetry event.

## Seams

- Provider requests: `pa_ai::request_hooks::install_request_hooks("anthropic", "content-filter", ..)`, called by
  `RefusalHooks::install_all`. `pa-cli` calls it at process start (feature `refusal`, on by default) when the
  `contentFilter` setting is on. Without the feature, or with the setting off, nothing is installed and requests
  go out unchanged.

## Public API

`RefusalHooks` (`new`, `install`, `install_all`), `RefusalGuard`, `GuardResult`, `ContentFilter`, `CategoryScore`,
`EvictResult`, `SurrogateModel`, `FeedbackEvent`, and the `filter`, `guard`, `logger`, `surrogate` modules.

## Dependencies

`pa-ai` (the request-hooks seam). No other workspace crate.

## Files owned

`logger` can write `refusal-events.jsonl`, `content-filter-outcomes.jsonl`, `feedback-events.jsonl` and
`refused-bodies/` under `~/.prime/agent/`; nothing on the request path calls it. `ContentFilter::semantic_rewrite_llm`
(a local OpenAI-compatible endpoint) is likewise unused by the hooks.

## Telemetry

None yet: the feature has no adoption event.
