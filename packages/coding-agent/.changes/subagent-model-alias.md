Resolve a provider-qualified model alias for subagents again.

A reference such as `anthropic/claude-haiku-4-5` names no exact catalog id and used to
resolve to the latest dated authenticated model. Only exact and short-form matches were
being tried, so the spawn failed. Provider-qualified references fall back to alias resolution
once more; bare short-form names keep refusing an ambiguous match.
