# REPL runtime protocol

`python -m rlm.repl` starts a CPython REPL runtime that executes code cells in
one persistent `__main__` namespace on a single asyncio event loop. The wire
format is newline-delimited JSON: one object per line, UTF-8, no other framing.
The current protocol version is `5`; the runtime announces it in the `ready`
event, and the host refuses a runtime that announces any other version.
Version 5 changed no frame shape: it marks the runtime whose `bash()` and
computer use are thin clients of the host, so the host must serve the
`bash.*` and `computer_use.*` host requests (with `harness.*`,
`mcp.session.*` and `factory.*`). A version-4 host cannot run a version-5
runtime and vice versa, and the handshake says so instead of letting every
`bash()` call fail. The out-of-band `factory_activity` request is gone (the
factory executor runs in the host, so the `/factory` lane no longer needs the
kernel); a runtime that receives the retired request answers the
unknown-type protocol error.

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
| `snapshot` | `{"type":"snapshot","id":str,"path":str,"manifest_path":str,"max_bytes"?:int,"max_variable_bytes"?:int,"prune_oversized"?:bool,"budget_ms"?:int}` |
| `restore` | `{"type":"restore","id":str,"path":str}` |
| `list_names` | `{"type":"list_names","id":str}` |
| `mcp_status` | `{"type":"mcp_status","id":str,"servers":[str,...],"timeout_ms"?:number}` — host-side view query: per-server tool listing (opens each server on demand, bounded by `timeout_ms` per server; default 10s); the `done` frame carries `connections: [{server, tools: [{name, description}] | null, error: str | null}]` |
| `bash_activity` | `{"type":"bash_activity","id":str,"action":"list"|"tail"|"kill","activityId"?:str,"lines"?:int}` — out-of-band even during a running cell; tail lines 1–200, response capped at 16 KiB; opaque IDs resolve only against this kernel’s handles |
| `plan_guard` | `{"type":"plan_guard","id":str,"token":str,"enabled":bool,"writable_roots"?:[str,...],"protected_roots"?:[str,...]}` — host-only switch of plan mode's no-OS-sandbox fallback guard, out-of-band even during a running cell; see Plan guard below |
| `shutdown` | `{"type":"shutdown","id"?:str}` |

Requests other than `interrupt`, `host_reply`, `bash_activity`, and
`plan_guard` run strictly in order, one at a time. A malformed line
produces `{"event":"error","id":null,"ename":"ProtocolError",...}` and the
runtime keeps serving. Closing stdin is equivalent to `shutdown`.

## Events

- `{"event":"ready","protocol":5,"python":"3.13.11"}` — sent once at startup;
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
  adds `saved`, `skipped`, `pruned`, `stale`, `bytes`; a restore `done` adds `restored`,
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
  for listings —, `mcp.connected` whether the host had a live connection when
  the call began, set from the host's reply; `false` means a lazy
  connect/handshake ran inside the span; `mcp.tool_count` on listings). An
  exception marks the span `error` with `attrs.error` and propagates
  unchanged.

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

`rlm.repl.host_request_blocking(request, *, timeout_s=None)` is the synchronous
form for runtime APIs that are not coroutines (`rlm.harness`, and the factory
client: `factory.spec` behind factory writes and the `factory.*` executor
calls): the same `host_request` frame, but the calling thread blocks until the
reader thread hands it the reply (no event-loop turn is needed, so a cell may
call it directly). An interrupt ends the wait with `KeyboardInterrupt`; stdin
EOF or `shutdown` fails it with `HostConnectionLost`; a set `timeout_s` (the
factory client passes 30) bounds it with `HostDrainTimeout`.

### Harness store requests

`rlm.harness` is a client of the host's harness store; each call is one
blocking host request of type `harness.load`, `harness.save`, `harness.get`,
`harness.list`, `harness.search`, `harness.overview`, `harness.snapshot`,
`harness.upsert`, `harness.create`, `harness.update`, `harness.delete`,
`harness.set_enabled`, `harness.record_refinement`, `harness.create_skill`,
`harness.update_skill`, `harness.factory` (`create_factory` /
`update_factory`), or `harness.resolve_factory` (below). `data` carries:

- `store`: `{"file": str|null, "scope": "local"|"global", "document": object|null, "writeError": str|null}`
  — the state file the client resolved (from `RLM_HARNESS_STATE_DIR`,
  `RLM_SESSION_DIR`, `RLM_GLOBAL_HARNESS_STATE_DIR`, or an explicit path), or
  `file: null` with the in-memory store's `document`; a set `writeError` makes
  every write raise it as `RuntimeError` (a kernel without a session store).
- `args`: the call's arguments as JSON (a value JSON cannot carry is
  `{"__rlm_harness_unserializable__": "<type name>"}`), and `types`: each
  argument's Python type name.
- `agentDir`: where the `factory.enabled` opt-in is read; `factoryArguments`:
  the node table (the `factory.spec` encoding) of the arguments a write
  stores (`{machine, dag}` for `harness.factory`), whose spec the store runs
  the factory validator on when the write stores one (a host still accepts
  a client's own `factorySpecErrors` list in its place).

`harness.resolve_factory` (`args.id`: the spec id) is everything a factory
run needs from the harness in one read: the stored factory entry the id
names (a `local:`/`global:` prefix routes it; `globalStore` is the global
store's descriptor, or null when that is this store), else, when `library`
(`[[source, dir], ...]`) is set, the library machine (with
`rlm.factory.run`'s refusals for a broken, missing, or invalidly named one),
else `factorySpec` (a node table) as the spec a caller holds; then every
string `subagent` reference of its states resolves (the subagent entry by
id, else the first listed by title). Its `result` is the `factory.run`
payload: `{"spec_id", "value": <node table of {"spec", "subagents"}>}`
plus `machine`/`machine_path` for a library run.

The handler's `result` is `{"ok": true, "result": ..., "state": <the store's
document after the call>, "loadError": str|null}` or `{"ok": false, "error":
{"type": "ValueError"|"TypeError"|"RuntimeError"|"TimeoutError"|"OSError"|"RecursionError",
"message": str}}`, which the client raises as that exception. Outside a
kernel the client sends the same request to `prime-agent
--prime-agent-harness-request` (stdin: the request, stdout: the reply);
the host exports the binary to the kernel as `PRIME_AGENT_EXECUTABLE`
(`PRIME_AGENT_HOST_BINARY` names one for a runtime with no host). The
one-shot serves every request that needs no session the same way: the
`harness.*` requests and the factory client's `factory.spec` and
`factory.library` (a malformed one exits 1 with the reason on stderr).

### Factory library requests

The `rlm.factory` machine-library functions (`parse_machine_file`,
`render_machine_file`, `list_machines`, `resolve_machine`, `import_machine`,
`export_factory_spec`, `export_library_machine`, `export_machine`,
`cli_dispatch`, and the name and description rules) are clients of the
host's library (`pa_core::factory::library`, the same implementation behind
`prime-agent factory`); each call is one blocking `factory.library` request
whose `data` carries `op` and its fields:

- `cwd`: the kernel's working directory, which every relative path in the
  request is relative to (results and errors spell paths as sent).
- Python values the call received travel as node tables (the `factory.spec`
  encoding; an opaque leaf is `["o", index, repr, truthy, type name,
  json.dumps spelling or null]`): `value` (`name_errors`,
  `description_errors`), `text`/`source` (`parse`), `name`, `description`,
  `version`, `author`, `spec` (`render`, `export_spec`), `target` and
  `entry: {arguments, content, title} | null` (`export_machine`), `payload`
  (`cli`).
- `spec_json`: the client's own `json.dumps(spec, indent=2,
  ensure_ascii=False)` for a spec it holds, or `spec_json_error: {type,
  message}` for the exception that raised (re-raised where the renderer
  would).
- `dirs`: `[[source, dir], ...]`, the library levels in resolution order
  (`scan`, `resolve`, `export_library`, `export_machine`, `cli`); `path`,
  `target_dir` (`import`); `out_path`, `overwrite`; `run` (`export_machine`:
  the live run's `factory.machine` view, or null).

The handler's `result` is `{"ok": true, "result": ...}` (a parsed machine is
`{name, description, version, author, spec: <node table>}`) or `{"ok":
false, "error": {"type", "message", ...}}` naming the exception to raise:
`ValueError`, `TypeError`, `AttributeError`, `RecursionError`,
`MachineResolutionError` (with `broken`), `OSError` (with `errno`,
`strerror`, `filename`), or `UnicodeDecodeError` (with `start`, `end`,
`reason`, `bytes`). A malformed request is a host error.

### bash() requests

`rlm.bash` is a client of the host's command checker and job runner (the
`pa-bash` crate). A host that serves these requests exports
`PRIME_AGENT_HOST_BASH=1` to the kernel (read once at import); a host reply
that a `bash.*` type "is not available in this session" fails the call with a
`BashHostUnavailable` naming the host/runtime version skew (reinstall
prime-agent). A Prime Agent host too old to serve `bash()` speaks protocol 4
and is refused at the handshake. Otherwise (another REPL host, or outside a
kernel) the client sends the same requests to a
`prime-agent --prime-agent-bash-host` sidecar it starts on first use (one JSON line per request,
`{"id": str, "data": <request>}`, and per reply; the sidecar kills its jobs
when its stdin closes); a sidecar that exits at once (a binary without the
flag) fails the call with the same skew hint. Each request is a blocking host request
whose `result` carries `status`: `ok`, `refused` (`error`: the refusal class,
`message`, `warning`: the one-time late-bypass stderr text) or `error`
(`error`: `ValueError`/`RuntimeError`/`OSError`/`KeyError`/`TypeError`,
`message`, `errno` for `OSError`), which the client raises.

- `bash.run`: `{command, script, prefix?, allow: [guard], cwd, env |
  envKey, launchBypass: [guard], kernelPid, traceparent?, checkTraceparent?,
  waitMs, spillDir?, guards?}` is a `bash()` call in
  one request: the guards on `script` (the guard keys are
  `destructive_git`, `destructive_chmod`, `force_push`, `secret_echo`,
  `pipe_to_shell`, `sudo`; `guards: false` skips them for a script the
  kernel's caller declared validated), then the spawn of that same script
  (under the kernel's OS sandbox, when it has one), then a follow of the job for
  up to `waitMs` (at most 30 s). It answers a refusal like `bash.check`, or
  `{job: {id, pid, pgid, startedAt}, events, cursor, done}`: a quick command
  arrives finished and reaped (`done`); otherwise the client continues with
  `bash.follow` from `cursor`. The kernel waits 25 ms, or 0 while another
  of its handles is live. A `host_cancel` for the run (the kernel was
  interrupted while it waited; `host_request_blocking(...,
  cancel_on_interrupt=True)`) SIGTERMs the job, SIGKILLs it after 0.5 s,
  confirms the group's exit, and answers with `cancelled: true` and the
  events up to the reap. `cwd` and `env` are the kernel's own at call time:
  the command runs there, with the non-interactive settings and the guard
  bypass scrub applied host-side. `env` travels whole with a new `envKey`
  when it changed and as `envKey` alone otherwise; a host that does not
  hold the key answers `error: EnvUnknown` and the client resends it whole.
- `bash.check`: the guards alone (same fields, no spawn); `{}` or the
  refusal.
- `bash.follow {id, cursor, spillDir?}`: waits (up to 30 s) for the job's
  next events and answers `{events, cursor, done}`; events are `progress`
  (`msg`, `fields`), `finished` (`exitCode`, `output`, `duration`, `fields`),
  then `reaped` (`bytes`), always in that order. With `spillDir` (the
  kernel's temp directory), a result of 64 KiB or more is written to a new
  owner-only file there and the `finished` event carries `outputFile`
  instead of `output`; the client reads and removes it (and falls back to
  `bash.output` if it cannot). `bash.run` events follow the same rule.
- `bash.output {id, bytesOnly?}`, `bash.kill {id, signal, graceMs}`,
  `bash.confirmExit {id, termGraceMs, killWaitMs}` (the cancelled one-shot's
  bounded teardown), `bash.groupAlive {id}`, `bash.killAll {}`,
  `bash.inventory {limit}`, `bash.activity {action, activityId?, lines?}`,
  `bash.shell` / `bash.childEnv` (the shell and environment a command would
  get), and `bash.isDestructiveGitDiscard {command}`.

The host answers the `list_kernel_bash`/`tail_kernel_bash`/`kill_kernel_bash`
commands from the same job table; the `bash_activity` frame remains for a
runtime run by another host (the runtime forwards it to its sidecar).

### Computer-use requests

The bundled `computer-use` skill's `computer_use` package is a client of the
host's `pa-computer-use` backends (macOS, X11, Wayland/niri); every call is
one host request (`App.is_frontmost()`, a synchronous method, uses
`host_request_blocking` with a 30 s bound, the rest `host_request`):

| `type` | payload |
|---|---|
| `computer_use.get_state` | `{emit}` |
| `computer_use.list_apps` | `{}` |
| `computer_use.permissions_status` | `{}` |
| `computer_use.get_app` | `{spec, instructions_dir}`: `spec` is the argument's shape (`{"kind": "str", "value"}`, `{"kind": "dict", "entries", "keys"}`, or `{"kind": "other", "type"}`) plus its Python `str` and `repr`; `instructions_dir` is where the per-app guides ship |
| `computer_use.app` | `{handle, method, ...}`: one `App` method of the binding `handle` names, with its arguments encoded (`target`: `{"kind": "index", "index"}`, `{"kind": "point", "point": {x, y, repr}}`, or `{"kind": "invalid", "type"}`) |

Each answers `{"ok": result}` or `{"error": {"code", "message", "details"}}`,
which the client raises as `ComputerUseError(code, message, details)`. The
client validates the Python-typed arguments first, with the skill's messages.

## MCP sessions

The MCP connections behind `rlm.mcp` and `rlm.McpIntegration` are host-owned
(one set per agent session, kept across kernel restarts, closed on
`rlm.mcp.reload`/`close`, idle, configuration change, and session end).
The runtime reaches them through cancellation-aware host requests:

| `type` | payload |
|---|---|
| `mcp.session.list_tools` | `{server}` |
| `mcp.session.call_tool` | `{server, tool, arguments}` |
| `mcp.session.describe_tool` | `{server, tool}` |
| `mcp.session.search_tools` | `{server, query, limit}` |
| `mcp.session.reload` | `{server?}` (all servers when absent) |
| `mcp.session.close` | `{}` |
| `mcp.integration.list_tools` | `{server, url, headers}` |
| `mcp.integration.call_tool` | `{server, url, headers, tool, arguments}` |

Each answers `{"ok":true,"value":…,"connected":bool}` or
`{"ok":false,"error":{"type":str,"message":str},"connected":bool}`, where
`type` names the exception the runtime raises (`RuntimeError`, `KeyError`,
`PermissionError`, `ValueError`, `TimeoutError`, `FileNotFoundError`,
`OSError`, `McpStartupError`, `McpDiscoveryError`, `McpCredentialsUnavailable`,
`McpToolError`, or `CancelledError`). A cancelled caller sends `host_cancel`;
the host abandons the request (an in-flight `tools/call` is cancelled at the
server) and replies `CancelledError`.

## Plan guard

Where the OS can confine processes (Landlock, Seatbelt), the host enforces
plan mode without this frame: it restarts the kernel under a `read-only` OS
sandbox (restoring the namespace from the snapshot), and the `bash()` jobs it
runs for the kernel inherit that policy (see `docs/os-sandbox.md` -> Plan
mode). The frame is the fallback for a machine with no OS sandbox.

`plan_guard` switches that fallback guard (`rlm.plan_guard`): while enabled,
an irremovable `sys.addaudithook` hook refuses filesystem mutations outside
the writable roots (temp dirs, `~/.cache`, `/dev`, and the request's
`writable_roots`; `.git` metadata and the request's `protected_roots` stay
read-only inside them) and every process spawn. A refused operation raises
`rlm.plan_guard.PlanModeError` in the cell; `bash()` raises it before
sending its request, and the host refuses the kernel's `bash.*` jobs while
the guard is armed. `ctypes` can get past the hook.

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

`snapshot` serializes the user namespace one name at a time, with the C
pickler for values built from importable types and with `dill` (without
`recurse`, so a cell function's globals stay a reference to the namespace) for
anything that reaches a class or function defined in a cell: `_`-prefixed names and
`{rlm, mcp, bash, asyncio, In, Out, get_ipython, exit, quit, open}` are always
skipped; a name whose pickle exceeds `max_variable_bytes` or would push the
total over `max_bytes` is skipped and reported. With `prune_oversized`, only
names exceeding the per-variable cap (`max_variable_bytes`) are also deleted
from the namespace and listed in `pruned`; names skipped for the aggregate
`max_bytes` cap are reported in `skipped` but kept in the namespace. The
payload is written atomically (tmp file + `os.replace`) and a JSON manifest
(`version`, `savedNames`, `skipped`, `pruned`, `stale`, `bytes`,
`pythonVersion`, `timestamp`) is written to `manifest_path`. A manifest write
failure fails the snapshot (and nothing is pruned).

With `budget_ms`, serialization stops once that many milliseconds have passed
(checked between names and inside one value's pickling) and the snapshot
commits what it has: each remaining name keeps its record from the payload
already at `path` (`saved`), or is skipped when there is none, and `stale`
lists every such name with its reason. The commit itself is not budgeted.
Without `budget_ms` the snapshot runs to completion and `stale` is empty.

An `interrupt` aimed at a snapshot or restore only ever cancels that request:
it never raises into a detached task that holds the loop meanwhile.

`restore` loads the payload and revives each name independently; a missing
file yields an ok empty restore with `reason:"snapshot not found"`, a corrupt
file fails with a `reason`, and per-name failures are listed in `failed`.
Names `In`, `Out`, and `get_ipython` in a payload are never restored. `dill` is imported lazily; when unavailable, snapshot and restore
fail with `status:"error"` and a `reason`.

`list_names` replies with `done` carrying `names`: the sorted user-defined
top-level names under the same filter the snapshot applies.

## Shutdown

`shutdown` (or stdin EOF) kills live `rlm.bash` process groups (`bash.killAll`;
a host also kills a kernel's jobs at teardown), replies
`done` (when the request carried an id), stops the loop, and exits 0.
