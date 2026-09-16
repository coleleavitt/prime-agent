Emit the failure fingerprint on the tool span, and let kernel errors carry their text.

`FAILURE_FINGERPRINT_ATTR` was defined and read by the learning index but written
by no producer: 0 of 157,374 span_end rows carried it, so the treated cohort could
never match an observed span and was empty by construction. The index's own exit
test passed only because its fixture seeded the attribute directly.

`afterToolCall` is the one hook that runs inside the open `tool.execute` span, so
coding-agent can stamp the ledger's own fingerprint there without `packages/agent`
importing from it. `fingerprintToolResultText` is shared with `observeToolResult`
so the span key and the ledger key cannot drift apart.

The Python runtime puts a span's error text in `attrs` while pi-ai's span_end shape
puts `error` at the top level, so every kernel-side error reached the log with no
error text at all — `prime-agent trace` showed a failed span with no reason, and
consumers keying on the error collapsed 2,386 distinct kernel and bash failures
into two. `forwardKernelTraceEvent` now hoists it.

Known and deliberate: an in-cell traceback ends `kernel.cell` as an error and is
separately fingerprinted onto its enclosing `tool.execute`, so one failure yields
two rows. Suppressing the descendant by name was tried and is worse — SyntaxErrors
and host-initiated snapshot errors have no covering ancestor, and dropping real
failures from the control cohort biases toward a false positive, while a shadow in
the control only biases toward the null.
