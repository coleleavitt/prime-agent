Let a provider declare that it does not accept tools.

`ModelRegistry.supportsTools(provider)` reads a new optional `supportsTools` field
on a provider request config; an undeclared provider still defaults to true. A
provider config may now be defined by `supportsTools` alone, without a baseUrl,
headers or compat entry.

`AgentLoopConfig.getRequestContext(context, model)` supplies request-local
`systemPrompt` and `tools` overrides immediately before each LLM call. The
overrides apply to that request only and never mutate the session context, so a
tool-less provider can be driven without changing what the session believes it
has.
