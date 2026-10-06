# pa-ai

Provider APIs and model registry.

## Scope
Provider trait + per-provider streaming clients (anthropic, openai-completions/responses, google, bedrock, mistral, azure, prime-inference), model registry/resolution, usage accounting, stream-failure retry, provider-error shapes (per-SDK user-facing texts, diagnostic error names, connection-error profiles), bedrock transport selection (h2c prior-knowledge HTTP/2 cleartext, h2-preferred TLS ALPN, the http1 AWS_BEDROCK_FORCE_HTTP1/proxy mode), overflow handling, JSON repair parsing, faux provider for tests.

## Non-goals
No agent loop, no tool execution, no session state, no UI. Receives/returns `pa-types` messages.

## Public API
`Provider` trait, `ProviderRegistry`, model lookup/resolution, faux provider. `request_hooks`: the provider request hooks registry (`install_request_hooks(provider_id, Arc<dyn ProviderRequestHooks>)`) a composition root fills for a provider id whose credentials come from a store outside the process: a fresher credential before each request, the built request's headers and payload, each response's status and headers, and a credential to re-send a rejected request with (a 401 once; a 429 or a rate-limited stream opening while the hooks name an unused credential, up to `MAX_CREDENTIAL_ATTEMPTS` sends). The `anthropic-messages` provider consults them; nothing is registered natively, and an unclaimed credential is sent as before. `catalog_invariants::validate_model_catalog`: the generation-time catalog invariants (maxTokens within the window, Copilot family endpoints, Codex/OpenAI window agreement, sendable and cross-provider-consistent thinking levels), checked over the compiled catalog here and the live payload in pa-models. Per-provider internals are `pub(crate)`.

## Depends on
pa-types (one-way).
