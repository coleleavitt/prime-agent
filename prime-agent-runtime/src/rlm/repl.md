# REPL runtime protocol

`python -m rlm.repl` starts a CPython REPL runtime that executes code cells in
one persistent `__main__` namespace on a single asyncio event loop. The wire
format is newline-delimited JSON: one object per line, UTF-8, no other framing.
The current protocol version is `4`; the runtime announces it in the `ready`
event.

## Channels

- Requests arrive on fd 0 (stdin).
- Events leave on a private dup of the original fd 1, made before anything else
  runs. Every frame is one locked write sequence, so frames never interleave.
- Python-level writes through `sys.stdout`/`sys.stderr` are intercepted at
  write time, tagged with the writing context's cell id, and shipped straight
  to the protocol.
- fds 1 and 2 are redirected into pipes at startup; pump threads read them and
  ship the bytes as `stdout`/`stderr` events with `id: null` — raw fd bytes
  (`os.write`, `sys.stdout.buffer.write`, C extensions, subprocesses) are never
  attributed to a cell.
  Neither channel can corrupt protocol framing. Ordering is preserved within
  each channel, not across them.
- The host's original fd 2 (its stderr pipe) is kept as a tee target. Every
  raw byte that lands on the captured fd 2 is also copied there, verbatim and
  immediately, before it is decoded into a `stderr` event; the drain marker
  bytes the runtime writes to fd 2 before each `done` are stripped from the
  copy. Python-level `sys.stderr` writes never touch fd 2 and are not copied.
  The copy exists for forensics only: when native code writes a message to
  fd 2 and then calls `exit()`/`abort()`, the process is gone before the pump
  thread can ship a protocol event, so the host's stderr tail and
  `kernel-stderr.log` would otherwise be empty. On POSIX the copy is made by a
  forked helper process (`python -m rlm.repl` in `ps`, single-threaded, child
  of the kernel) sitting between fd 2 and the pump: it needs no GIL, so it
  still forwards the last words after the kernel has died, then exits on fd-2
  EOF or shortly after the kernel is gone (a grandchild that inherited fd 2
  cannot keep it alive: post-mortem copying is bounded to ~1 MiB and 50 ms of
  silence). Where `fork` is unavailable (Windows) or fails, the pump thread
  tees in-process, which is best effort: it needs the GIL, which an exiting
  native caller never releases. Writes to the host pipe never block; if the
  host stops reading, the rest of the copy is dropped. The protocol `stderr`
  events are unaffected either way.
- fd 0 is rebound to `/dev/null` after the reader thread takes it, so user
  `input()` sees EOF instead of consuming protocol frames.

## Requests

| Request | Fields |
|---|---|
| `execute` | `{"type":"execute","id":str,"code":str}` |
| `interrupt` | `{"type":"interrupt","id"?:str}` — no reply |
| `host_reply` | `{"type":"host_reply","id":str,"data":{"status":"ok","result":{...}}}` or an error envelope — no reply |
| `snapshot` | `{"type":"snapshot","id":str,"path":str,"manifest_path":str,"max_bytes"?:int,"max_variable_bytes"?:int,"prune_oversized"?:bool}` |
| `restore` | `{"type":"restore","id":str,"path":str}` |
| `list_names` | `{"type":"list_names","id":str}` |
| `mcp_status` | `{"type":"mcp_status","id":str,"servers":[str,...],"timeout_ms"?:number}` — host-side view query: per-server tool listing (opens each server on demand, bounded by `timeout_ms` per server; default 10s); the `done` frame carries `connections: [{server, tools: [{name, description}] | null, error: str | null}]` |
| `bash_activity` | `{"type":"bash_activity","id":str,"action":"list"|"tail"|"kill","activityId"?:str,"lines"?:int}` — out-of-band even during a running cell; tail lines 1–200, response capped at 16 KiB; opaque IDs resolve only against this kernel’s handles |
| `plan_guard` | `{"type":"plan_guard","id":str,"token":str,"enabled":bool,"writable_roots"?:[str,...]}` — host-only plan-mode switch, out-of-band even during a running cell; see Plan guard below |
| `shutdown` | `{"type":"shutdown","id"?:str}` |

Requests other than `interrupt`, `host_reply`, `bash_activity`, `factory_activity`, and
`plan_guard` run strictly in order, one at a time. A malformed line
produces `{"event":"error","id":null,"ename":"ProtocolError",...}` and the
runtime keeps serving. Closing stdin is equivalent to `shutdown`.

## Events

- `{"event":"ready","protocol":4,"python":"3.13.11"}` — sent once at startup;
  the handshake. No banner precedes it.
- `{"event":"stdout"|"stderr","id":str|null,"text":str}` — captured output.
  `id` is the cell whose Python execution context performed the write; asyncio
  tasks inherit the spawning cell's id (even after that cell finished). `null`
  for user threads, raw fd writes (`os.write`, C extensions, subprocesses),
  and anything else without provable ownership — bytes read from the fd pipes
  are never attributed to a cell. A Python-level write ships at most 64 Ki
  characters per frame; a larger write arrives as multiple events in order.
- `{"event":"result","id":str,"text":str}` — `repr` of the cell's trailing
  expression when the body ends in an expression whose value is not `None`.
  The value is also bound to `_` in the namespace. The `repr` content is capped
  at 1,048,576 characters; a longer `repr` is truncated to the cap and a trailing
  truncation marker is appended after it, so the total `text` can exceed the cap
  by the marker's length.
- `{"event":"display","id":str|null,"data":{mime:payload,...}}` — one dict of
  MIME type to JSON payload, shipped verbatim from `emit()`. A payload whose
  JSON encoding exceeds 16 Mi characters is refused: `emit()` raises
  `ValueError` in the calling cell. `id` rides task
  context: an asyncio task spawned by a cell keeps that cell's id even after
  the cell finishes; user threads emit `null`.
- `{"event":"host_request","id":str,"data":{...},"traceparent":str}` — one
  typed request from runtime code to the host; the host answers with a
  `host_reply` request carrying the same id. `traceparent` is the runtime's
  `kernel.host_request` client span (see Trace context below).
  `data` is subject to the same encoding cap as a `display` payload:
  `host_request()` raises `ValueError` in the calling cell instead of sending
  an oversized request.
- `{"event":"host_cancel","id":str}` — cancellation for that exact in-flight
  host request. The host still sends its terminal `host_reply` after settlement.
- `{"event":"trace","id":str|null,"msg":"span_start"|"span_end","name":str,"traceId":str,"spanId":str,"parentSpanId"?:str,"attrs":{...}}`
  — one span lifecycle event. `span_end` also carries `durationMs` and `status`. `id` is the request whose handling produced it (task
  context, like `display`); `null` from user threads.
- `{"event":"error","id":str|null,"ename":str,"evalue":str,"traceback":[str,...]}`
  — `evalue` and each `traceback` entry are capped like `result` text (same
  cap, same trailing marker).
- `{"event":"done","id":str,"status":"ok"|"error"}` — exactly one per id'd
  request, always after all of that request's other events. A snapshot `done`
  adds `saved`, `skipped`, `pruned`, `bytes`; a restore `done` adds `restored`,
  `failed`; a `list_names` `done` adds `names`; a failed snapshot/restore adds
  `reason`. Bash activity `done` carries `activities` (list), `tail` (tail),
  or `killed` (kill); `status:"error"` with `reason` on unknown IDs.
  The daemon advertises `kernel_bash_activity`; clients poll `list_kernel_bash`
  for updates (no push events). Each row contains opaque `id`, `command`,
  `pid`, `durationMs`, and `status` (`running` or `finished`). Finished rows
  are retained for the latest 64 completions within a live kernel only; restart
  invalidates all IDs. `tail_kernel_bash` returns `{id,tail}`, and
  `kill_kernel_bash` returns `{id,killed}`. Neither action accepts a PID.
  Restoring a missing file reports `status:"ok"` with empty
  `restored`/`failed` lists and `reason:"snapshot not found"`. An `execute`
  `done` may add `bashCommands` (see below).

`bashCommands` is optional and additive, so the protocol version stays `4` and
a host that predates it ignores it. It is present only on an `execute` `done`,
only when at least one `bash()` command started in the cell's context (the cell
or a task it spawned) finished while the cell body was still running, and it is
never sent empty. Each entry is
`{"command":str,"exitCode":int,"startedAt":str,"endedAt":str,"commandTruncated"?:true}`:
`command` is secret-redacted and cut at 300 characters, `commandTruncated` marks
a cut (the text then names the command but is not what ran), and the timestamps
are ISO 8601 UTC. Entries are in completion order, and only the newest 32 are
kept. A command still running when the body ends is reported on no frame. The
host (`parseKernelBashCommands` in `core/kernel/repl-manager.ts`) keeps the
well-formed entries and exposes `command`, `exitCode` and `commandTruncated` on
the `ipython` tool result's `details.bashCommands`, where Workspace Recall reads
build claims from them.

Before a cell's `done`, the runtime drains both channels: tagged Python-level
writes ship synchronously from the writing thread, and the fd pipes are fenced
with a marker byte sequence awaited in the pumps, so every byte the cell wrote
synchronously — including direct fd writes — precedes its `done`. Ordering
between a cell's Python-level writes and its raw fd writes is not guaranteed
(two channels).

## Trace context

`execute`, `snapshot`, and `restore` accept an optional W3C `traceparent`
string (`00-<32 hex>-<16 hex>-<2 hex>`, lowercase, non-zero ids). A valid
value becomes the parent of the request's `kernel.cell` span
(`attrs`: `kernel.request_id`, `kernel.request_type`); a missing or invalid
value is ignored (no protocol error) and the request becomes a child of the
context inherited from the `TRACEPARENT` environment variable at startup, or
starts a fresh trace. The `kernel.cell` emits `span_start` before execution and `span_end` before the
request's `done`. User code sees the cell context through `rlm.trace`
(`current()`, `start_span()`), `host_request` spans are children of the cell
span, and `bash()` children receive `TRACEPARENT` in their environment. See
`docs/observability.md` at the repository root for the cross-process contract.

Runtime spans emitted by the kernel itself (all children of the current
context, or a fresh trace when there is none):

- `kernel.cell` — one per `execute`/`snapshot`/`restore` request (`attrs`:
  `kernel.request_id`, `kernel.request_type`).
- `kernel.host_request` — client side of each `host_request` (`attrs`:
  `host_request.rid`, `host_request.type`).
- `bash.command` — one per `bash(command)` call, opened when the process is
  spawned and ended exactly once when the foreground result is known
  (`await`, `poll()`, `kill()`, or the process exiting unobserved all end it
  through the same path). `attrs`: `bash.command` (text, truncated to 200
  chars), `bash.pid`, `bash.exit_code`, `bash.output_bytes` (bytes written,
  including any dropped middle), `bash.signal` (signal name when the shell
  died by signal, i.e. a negative exit code), `bash.killed` (`kill()` was
  called). Status is `error` with `attrs.error` `"exit code N"` /
  `"killed by SIG…"` for a non-zero exit, `"spawn failed: …"` when the
  process could not be started, and `"kernel shutdown"` when the kernel shuts
  down with the command still running (the span is closed immediately at
  shutdown instead of dangling; the later reap of the killed group does not
  emit a second span). The child process receives `TRACEPARENT` of the
  `bash.command` span, so anything it runs nests under the command. `bash.command`
  emits `span_start` after spawn with `bash.pid`, `bash.pgid`, and
  `bash.started_at`. `active_bash_commands()` returns up to 100 immutable
  progress snapshots with elapsed/silence times, output byte count, and last
  output time. Long silence emits structured `bash/command_no_output` events
  after five minutes by default (`PRIME_AGENT_BASH_NO_OUTPUT_WARN_MS`, `0`
  disables). Output matching Cargo's build-directory lock wait emits
  `bash/cargo_lock_wait` immediately and records
  `bash.wait_reason="cargo_build_lock"`; no captured output content is emitted.
- `mcp.call` — one per `mcp.list_tools(server)` / `mcp.call_tool(server,
  tool, arguments)` call (`attrs`: `mcp.server`, `mcp.tool` — `"list_tools"`
  for listings —, `mcp.connected` whether an open generation existed when the
  call began; `false` means a lazy connect/handshake ran inside the span;
  `mcp.tool_count` on listings). An exception marks the span `error` with
  `attrs.error` and propagates unchanged.

## Execution

Cells compile with `PyCF_ALLOW_TOP_LEVEL_AWAIT` and run as tasks on the
persistent event loop, so `await` works at top level and background tasks
created by a cell keep running between cells. Each cell's source is registered
in `linecache` under `<cell-N>`, so tracebacks show the offending source line.
Tracebacks are plain `traceback` formatting with the runtime's own frames
stripped, keeping cell and library frames; no colors, no decoration.

## Interrupt

`{"type":"interrupt"}` raises `KeyboardInterrupt` in the running cell. Without
an `id` the interrupt applies to the running request, or — when none is running
yet — to the next queued one; with an `id` it applies to that request only.
An interrupt that arrives before its request starts executing is parked and
delivered the moment the request becomes active, so `execute` + `interrupt`
written back-to-back still interrupts the cell. A request stays
interrupt-targetable until its `done` event is emitted: this covers the
post-run trailing-expression `repr` and output drain. Interrupts for finished
or unknown requests are dropped.

Delivery: the reader thread sends SIGINT to the main thread (also the loop
thread); the handler asks asyncio which task's step the signal interrupted.
On Windows (no `signal.pthread_kill`) the reader instead cancels the active
cell task on the loop, so await-suspended cells interrupt normally but cells
blocked in synchronous code cannot be broken (best-effort parity):

- The active cell's own task is mid-step (sync bytecode such as a `time.sleep`
  loop, or a blocking syscall such as `selectors.select()` woken by EINTR):
  the handler raises `KeyboardInterrupt` directly and it propagates out of the
  cell task.
- The loop is idle in `select()` (the cell is suspended at an `await`) or a
  different task — a background task or the runtime itself — is mid-step:
  raising there would land in the wrong context, so the handler cancels the
  active cell task and the runtime reports the cancellation as a
  `KeyboardInterrupt`. When the mid-step task is a background task, the
  handler also raises `KeyboardInterrupt` into it: a background task blocked
  in synchronous code occupies the only thread, so it receives the
  `KeyboardInterrupt` (and dies with it) to unblock the loop and let the
  cancel take effect. Limitation: the interrupt lands at the await point as a
  cancellation, so user code catching `KeyboardInterrupt` around an `await`
  does not intercept it.

Both paths end with an `error` event (`ename` `KeyboardInterrupt`) and
`done` with `status:"error"`; the runtime keeps serving. When nothing is
running or queued, SIGINT and interrupt requests are ignored.

## Display bridge

`from rlm.repl import emit` inside a cell (or any user thread) ships a
`display` event. `emit(data)` takes one non-empty dict keyed by MIME type
strings; the dict is forwarded verbatim as the event's `data`.

## Host bridge

`await rlm.repl.host_request(data)` ships a `host_request` event with a
runtime-minted id and awaits the matching `host_reply`, returning its `data`
dict verbatim. Replies are routed on the reader thread like `interrupt` —
never through the request queue, since the awaiting cell is itself the
in-flight execute. Replies for unknown ids are dropped. Cancellation-aware calls emit one exact-ID `host_cancel`, shield the same reply future, and keep it alive through their bounded drain. `rlm.repl.is_active()` reports whether the
process is serving the protocol (importing the module does not count).

`rlm.repl.host_request_blocking(data)` is the synchronous form for runtime
APIs that are not coroutines (`rlm.harness`): the same `host_request` frame,
but the calling thread blocks until the reader thread hands it the reply (no
event-loop turn is needed, so a cell may call it directly). An interrupt ends
the wait with `KeyboardInterrupt`; stdin EOF or `shutdown` fails it with
`HostConnectionLost`.

### Harness store requests

`rlm.harness` is a client of the host's harness store; each call is one
blocking host request of type `harness.load`, `harness.save`, `harness.get`,
`harness.list`, `harness.search`, `harness.overview`, `harness.snapshot`,
`harness.upsert`, `harness.create`, `harness.update`, `harness.delete`,
`harness.set_enabled`, `harness.record_refinement`, `harness.create_skill`,
`harness.update_skill`, or `harness.factory` (`create_factory` /
`update_factory`). `data` carries:

- `store`: `{"file": str|null, "scope": "local"|"global", "document": object|null, "writeError": str|null}`
  — the state file the client resolved (from `RLM_HARNESS_STATE_DIR`,
  `RLM_SESSION_DIR`, `RLM_GLOBAL_HARNESS_STATE_DIR`, or an explicit path), or
  `file: null` with the in-memory store's `document`; a set `writeError` makes
  every write raise it as `RuntimeError` (a kernel without a session store).
- `args`: the call's arguments as JSON (a value JSON cannot carry is
  `{"__rlm_harness_unserializable__": "<type name>"}`), and `types`: each
  argument's Python type name.
- `agentDir`: where the `factory.enabled` opt-in is read; `factorySpecErrors`:
  the kernel factory validator's errors for the spec a factory write stores.

The handler's `result` is `{"ok": true, "result": ..., "state": <the store's
document after the call>, "loadError": str|null}` or `{"ok": false, "error":
{"type": "ValueError"|"TypeError"|"RuntimeError"|"TimeoutError"|"OSError",
"message": str}}`, which the client raises as that exception. Outside a
kernel the client sends the same request to `prime-agent
--prime-agent-harness-request` (stdin: the request, stdout: the reply);
the host exports the binary to the kernel as `PRIME_AGENT_EXECUTABLE`.

## Plan guard

`plan_guard` switches plan mode (`rlm.plan_guard`): while enabled, an
irremovable `sys.addaudithook` hook refuses filesystem mutations outside the
writable roots (temp dirs, `~/.cache`, `/dev`, and the request's
`writable_roots`; `.git` metadata stays read-only inside them) and direct
process spawns; `subprocess.Popen` runs under a
read-only OS sandbox (`bwrap` on Linux, `sandbox-exec` on macOS) or, without
one, only classifiable read-only commands. A refused operation raises
`rlm.plan_guard.PlanModeError` in the cell. `bash()` checks its command
before spawning.

The runtime claims the one host controller before it announces `ready`, and
only the reader thread holds it: cells cannot claim another. The first frame
binds its `token` (the host sends one right after `ready`, before any cell);
every later frame must carry the same token. The reply is
`{"event":"done","id":str,"status":"ok","enabled":bool}`, or `status:"error"`
with `reason` for a malformed frame or a wrong token. The protocol version
stays `4`: a host that never sends the frame leaves the guard off, and a
runtime that predates it answers a protocol error without an id, which the host
bounds with a timeout.

## Snapshot / restore

`snapshot` serializes the user namespace with `dill` (recurse mode), one name
at a time: `_`-prefixed names and
`{rlm, mcp, bash, asyncio, In, Out, get_ipython, exit, quit, open}` are always
skipped; a name whose pickle exceeds `max_variable_bytes` or would push the
total over `max_bytes` is skipped and reported. With `prune_oversized`, only
names exceeding the per-variable cap (`max_variable_bytes`) are also deleted
from the namespace and listed in `pruned`; names skipped for the aggregate
`max_bytes` cap are reported in `skipped` but kept in the namespace. The
payload is written atomically (tmp file + `os.replace`) and a JSON manifest
(`version`, `savedNames`, `skipped`, `pruned`, `bytes`, `pythonVersion`,
`timestamp`) is written to `manifest_path`. A manifest write failure fails the
snapshot (and nothing is pruned).

`restore` loads the payload and revives each name independently; a missing
file yields an ok empty restore with `reason:"snapshot not found"`, a corrupt
file fails with a `reason`, and per-name failures are listed in `failed`.
Names `In`, `Out`, and `get_ipython` in a payload are never restored. `dill` is imported lazily; when unavailable, snapshot and restore
fail with `status:"error"` and a `reason`.

`list_names` replies with `done` carrying `names`: the sorted user-defined
top-level names under the same filter the snapshot applies.

## Shutdown

`shutdown` (or stdin EOF) kills live `rlm.bash` child process groups, replies
`done` (when the request carried an id), stops the loop, and exits 0.
