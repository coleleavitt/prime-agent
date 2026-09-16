Keep the npm release bridge and native update free of package dependencies.

Both run before an installation's node_modules exist, so the modules they load must depend
only on Node builtins. `config.ts` had gained a `proper-lockfile` import for log rotation and
`version-check.ts` a `pi-ai` import for tracing, and either import alone failed those
processes at link time. Log rotation now takes a builtin lock on the same `<target>.lock`
directory, and update checks receive their tracer from callers instead of importing it, so
the `update.check` span is still recorded on every path that can trace.
