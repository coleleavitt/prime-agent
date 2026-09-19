# Observability: end-to-end trace context

Status: implemented incrementally on branch `fix/forkserver-probe-hardening`.

## Goal

Every log line, every provider request, every tool execution, every Python
kernel cell, every host request and every RLM child session carries one
**W3C `traceparent`** so a single user turn can be followed across
processes with one id. No third-party dependency is required; the format and the API surface mirror
OpenTelemetry. An optional dependency-free OTLP/HTTP JSON adapter can subscribe
to the same span sink without changing call sites.

## Identity

`traceparent = "00-" + traceId(32 hex) + "-" + spanId(16 hex) + "-" + flags(2 hex)`

* `traceId` is minted once per user turn (or inherited from the `TRACEPARENT`
  environment variable when the process is started by an external caller).
* Every hop creates a child span: new `spanId`, same `traceId`,
  `parentSpanId = caller spanId`.
* `flags` is `01` (sampled) by default.

## Span names (stable, dotted)

All hops below are implemented; the last column names the file that opens
the span so `grep withSpan`/`start_span` lands on it.

| span                  | attributes                                                   | status | created in |
|-----------------------|---------------------------------------------------------------|--------|------------|
| `agent.prompt`        | `session.id`                                                  | done   | coding-agent `core/agent-session.ts` |
| `agent.turn`          | `session.id`, `turn.index`                                    | done   | pi-agent-core `agent-loop.ts` |
| `llm.request`         | `llm.provider`, `llm.api`, `llm.model`, `llm.base_url`        | done   | pi-ai `stream.ts` |
| `tool.execute`        | `tool.name`, `tool.call_id`                                   | done   | pi-agent-core `agent-loop.ts` |
| `kernel.execute`      | `kernel.request_id`, `kernel.request_type`, `kernel.status`   | done   | coding-agent `core/kernel/repl-manager.ts` |
| `kernel.cell`         | `kernel.request_id`                                           | done   | Python `rlm/repl.py` |
| `kernel.host_request` | `host_request.rid`, `host_request.type`                       | done   | Python `rlm/repl.py` (client) / TS `core/kernel/repl-manager.ts` (server) |
| `rlm.child`           | `rlm.child_id`, `rlm.depth`                                   | done   | coding-agent `modes/daemon/daemon-mode.ts` |
| `daemon.command`      | `daemon.request_id`, `daemon.command_type`                    | done   | coding-agent `modes/daemon/daemon-mode.ts` |
| `client.prompt`       | `client.command`, `client.source`, `client.queue_if_busy`, `session.active_id` | done | coding-agent `modes/agent-connection/daemon-agent-connection.ts` (TUI/CLI side root; worker spans nest under it) |
| `tool.prepare`        | `tool.name`, `tool.call_id`, `tool.blocked`, `tool.block_reason` | done | pi-agent-core `agent-loop.ts` (argument validation + `beforeToolCall`: permission prompts live here) |
| `extension.hooks`     | `hook.event`, `hook.handlers`, `hook.slowest`, `hook.slowest_ms`, `hook.errors`, `hook.<ext>_ms` (>25 ms) | done | coding-agent `core/extensions/runner.ts` (one span per emit, skipped when no handler) |
| `session.compact`     | `session.id`, `llm.provider`, `llm.model`, `compact.tokens_before`, `compact.summary_chars`, `compact.first_kept_entry` | done | coding-agent `core/agent-session.ts` (manual and automatic) |
| `agent.retry`         | `retry.attempt`, `retry.max_attempts`, `retry.delay_ms`, `retry.error` | done | coding-agent `core/agent-session.ts` (backoff wait before re-issuing a failed turn) |
| `rlm.run_agent`       | `rlm.requested_model`, `rlm.model`, `rlm.status`, `rlm.turns` | done | coding-agent `core/agent-session.ts` (`ctx.runAgent` children) |
| `rlm.child.run`       | `rlm.child_id`, `rlm.child_name`, `rlm.depth`, `rlm.child.status`, `rlm.child.session_id`, `rlm.child.duration_ms`, `rlm.child.error` | done | coding-agent `core/agent-session.ts` (one per detached `rlm.spawn` child; covers the whole run so an unfinished or failed child stays visible to `prime-agent health`) |
| `cron.job`            | `cron.job_id`, `cron.name`, `cron.kind`, `cron.runtime_kind`, `cron.session_id`, `cron.deferred`, `cron.delivery`, `cron.result` | done | coding-agent `modes/daemon/daemon-mode.ts` (scheduled/heartbeat prompts nest their `agent.prompt` under it) |
| `context.transform`   | `context.messages_in/out`, `context.targets`, `context.usage_percent`, `context.input_tokens`, `context.context_limit` | done | Magic Context `pi-plugin/src/context-handler.ts` (optional pi-ai bridge) |
| `client.turn`         | `session.active_id`, `client.source`, `turn.queued`, `turn.messages` | done | coding-agent `modes/agent-connection/daemon-agent-connection.ts` (submit → `agent_end` seen by the window; error on close/dispose/rejected admission) |
| `oauth.refresh`       | `oauth.provider`, `oauth.expired_ms`, `oauth.outcome` | done | coding-agent `core/auth-storage.ts` (refresh under the auth-file lock; a failure is otherwise swallowed into "no API key") |
| `trace.upload`        | `upload.status`, `upload.bytes`, `http.status` | done | coding-agent `core/agent-traces.ts` |
| `kernel.start`        | `kernel.python`, `kernel.restore`, `kernel.bootstrapped`, `kernel.python_ms`, `kernel.python_path` (`stamped`/`verified`/`synced`/`bootstrapped`/`override`), `kernel.pid` | done | coding-agent `core/kernel/repl-manager.ts` (the child's `TRACEPARENT` names this span) |
| `extensions.load`     | `extensions.count`, `extensions.loader_ms`, `extensions.errors`, `extensions.slowest`, `extensions.slowest_ms`, `extensions.<label>_ms` (>100 ms) | done | coding-agent `core/extensions/loader.ts` |
| `session.load`        | `session.path`, `session.bytes`, `session.entries` | done | coding-agent `core/session-manager.ts` (`open`/`openAsync`) |
| `bash.command`        | `bash.command`, `bash.pid`, `bash.exit_code`, `bash.signal`, `bash.killed`, `bash.output_bytes` | done | Python `rlm/bash.py` (the child's `TRACEPARENT` names this span; kernel shutdown ends it as error "kernel shutdown") |
| `mcp.call`            | `mcp.server`, `mcp.tool`, `mcp.connected`, `mcp.tool_count` | done | Python `rlm/mcp.py` |
| `ravo.run`            | `ravo.run_id`, `ravo.resumed`, `ravo.reason`, `ravo.rounds`, `ravo.repairs`, `ravo.spent_tokens`, `ravo.certificate_digest` | done | coding-agent `core/ravo/controller.ts` (deadline/budget/cancel are ok + reason; each evaluated proposal also logs one `refinement.*` record, see [Refinement outcome records](#refinement-outcome-records)) |
| `ravo.round`          | `ravo.round`, `ravo.phase`, `ravo.outcome`, `ravo.reason` | done | coding-agent `core/ravo/controller.ts` |
| `ravo.proposal`       | `ravo.round`, `ravo.kind`, `ravo.proposal_id`, `ravo.candidate_tokens` | done | coding-agent `core/ravo/controller.ts` (implement/repair child call) |
| `ravo.evaluation`     | `ravo.proposal_id`, `ravo.evaluator`, `ravo.evaluator_kind`, `ravo.verdict`, `ravo.certificate_digest` | done | coding-agent `core/ravo/controller.ts` (each evaluator + the commit gate) |
| `ravo.referee`        | `referee.claimed`, `referee.skill_imports`, `referee.adjudicated`, `referee.upheld`, `referee.cleared`, `referee.unverifiable`, `referee.no_evidence`, `referee.not_applicable` | done | coding-agent `core/ravo/referee-runner.ts` (one per gate evaluation whose claim names a recurring fingerprint, or per post-commit trust replay; child of `refine.plan` for `/refine`, of `ravo.evaluation` for `ravo.run`, and of `harness.trust.adjudicate` for a trust replay. Only verified cases whose probe names a module or distribution that a skill create/update in the proposal imports are run; any other claim is `not_applicable`, and an applicable claim with no verified case is `no_evidence`, which fails closed) |
| `ravo.replay_case`    | `referee.language`, `referee.timeout_ms`, `referee.environment` (`sanitized`/`skill-import`/`inherited`), `referee.python`, `referee.outcome`, `referee.exception_class` | done | coding-agent `core/ravo/referee-runner.ts` (the replay-case subprocess; child of `ravo.referee` (`skill-import`, at the gate or under `harness.trust.adjudicate`), `ravo.replay_verify` (`sanitized`) or `toolforge.gate` (`inherited`). On POSIX it leads its own process group, killed when the run ends or when the host exits or receives SIGINT, SIGTERM or SIGHUP; `outcome: unrunnable` fails the gate closed) |
| `ravo.replay_verify`  | `referee.cases`, `referee.ran`, `referee.verified` | done | coding-agent `core/ravo/referee-runner.ts` (`verifyObservedReplayCases`, started as a detached root by `AgentSession._drainReplayVerificationBacklog`: the self-check of replay cases derived at a turn boundary. One batch runs at a time, each (fingerprint, source) at most once per session, and dispose aborts it; a case that reproduced is marked verified at the next ledger flush, and a trust replay waiting on one of its cases is released when the batch lands) |
| `dream.experiment`    | `dream.experiment_id`, `dream.task`, `dream.seed`, `dream.rounds`, `dream.arms` (comma-joined), `dream.mode` (`local`/`llm`), `dream.stopped` (`aborted`), `trigger.trace_id` (LLM path only) | done | coding-agent `core/dream/experiment.ts` (`runExperiment`, sync local CLI: an in-turn root) and `core/dream/experiment-llm.ts` (`runExperimentWithAgent`, in-session: a detached root carrying the launching turn's `trigger.trace_id`; each arm's `dream.run` is its own detached root whose `trigger.trace_id` is this span's trace). The paper's fixed-exploration control: every arm from the same initial policy, seed and budget, the `fixed` arms never dreaming |
| `dream.experiment_arm` | `dream.experiment_id`, `dream.arm`, `dream.fixed_policy`, `dream.guided`, `dream.run_id` | done | coding-agent `core/dream/experiment.ts` / `core/dream/experiment-llm.ts` (one per arm, child of `dream.experiment`; `dream.run_id` is set once the arm's loop has an id) |
| `dream.run`           | `dream.task`, `dream.seed`, `dream.workers`, `dream.k1`, `dream.k2`, `dream.dreams`, `dream.iterations`, `dream.mode` (`local`/`llm`), `dream.fixed_policy`, `dream.stopped` (`aborted`, on cancel), `trigger.trace_id` (LLM path only) | done | coding-agent `core/dream/loop.ts` (`runDreamLoop`, sync local CLI: an in-turn root, or a child of `dream.experiment_arm` under an experiment) and `core/dream/llm.ts` (`runDreamLoopWithAgent`, in-session: a detached root carrying the launching turn's `trigger.trace_id`, ended on success/abort/error). `dream.fixed_policy` marks the control arm, which never dreams. `DreamRunService` opens no span of its own |
| `dream.explore`       | `dream.policy_id`, `dream.k1`, `dream.workers`, `dream.iteration`, `dream.tree_id` | done | coding-agent `core/dream/rollout.ts` (`runOnlineExploration` / `runOnlineExplorationWithAgent`; one per rollout) |
| `dream.round`         | `dream.round`, `dream.batch_size`, `dream.revealed_count`, `dream.best_score` | done | coding-agent `core/dream/rollout.ts` (one per online round) |
| `dream.attempt`       | `dream.node_id`, `dream.parent_id`, `dream.task`, `dream.valid`, `dream.score`, `dream.tokens`, `dream.origin` (`local`/`llm`: who generated the candidate; `local` on the LLM path is a fallback for a rejected child output), `dream.fail_class` | done | coding-agent `core/dream/rollout.ts` (`commitAttempt`; one per generation+evaluation; the local proposer opens no span, so a local rollout's tree is byte-identical to a sync one; `dream.origin` is persisted on the node line as `origin`) |
| `dream.dream`         | `dream.candidates`, `dream.pool_size`, `dream.chosen_policy_id`, `dream.chosen_score`, `dream.current_score`, `dream.chosen_quality`, `dream.current_quality`, `dream.quality_rejected`, `dream.improved` | done | coding-agent `core/dream/improve.ts` (`runDreaming`; one per dreaming step). Scores are the normalized replay objective; `*_quality` is the mean pool-normalized best, and `quality_rejected` counts candidates the no-quality-regression guard excluded |
| `dream.replay`        | `dream.policy_id`, `dream.simulations` | done | coding-agent `core/dream/improve.ts` (one coarse replay summary per dreaming step, not one per simulation) |
| `dream.redeploy`      | `dream.policy_id`, `dream.k1`, `dream.workers`, `dream.iteration`, `dream.tree_id`, `dream.fixed_policy` | done | coding-agent `core/dream/loop.ts` / `core/dream/llm.ts` (wraps the redeploy rollout of a chosen policy; on a fixed-policy iteration the initial policy is redeployed without dreaming and `dream.fixed_policy` is true) |
| `dream.llm_propose`   | `dream.round`, `dream.tokens`, `dream.llm_output_tokens`, `dream.llm_attempts` (child results examined, retries included), `dream.llm_fallback`, `dream.origin` (`llm` when the child's candidate entered the tree, `local` when the local mutator stood in), `dream.llm_reject_reason` (`parse`/`shape`/`invalid-candidate`/`error`/`length`/`aborted`/`turn-limit`/`budget`, the LAST result's reason when it was rejected), `dream.llm_status` (that result's `RunAgentStatus`), `dream.llm_reject_excerpt` (a 240-character head/tail of its output) | done | coding-agent `core/dream/llm.ts` (`createLlmProposer`; only on the `--llm-proposer` path, one per generation attempt wrapping every child call of that attempt; the child's JSON object is extracted from around fences and prose (`extractJsonValue`), a retryable rejection gets one more call, a `length` cut is not retried; every rejection is also a line in `<dreamDir>/rejections/<runKey>.jsonl` and a count in the round's `proposals` tally) |
| `dream.llm_dream`     | `dream.candidates_requested`, `dream.candidates_kept`, `dream.tokens`, `dream.llm_fallback` | done | coding-agent `core/dream/llm.ts` (`proposePoliciesWithAgent`; only on the `--llm-dreamer` path; the JSON array is extracted leniently from the child's output; a call failure or all-dropped candidates falls back to the local search) |
| `dream.llm_guidance`  | `dream.iteration`, `dream.pool_size`, `dream.tokens`, `dream.llm_fallback` | done | coding-agent `core/dream/llm.ts` (the semantic-guidance ablation: one guidance-writer child call per iteration >= 1 on a `dream-guided` / `fixed-guided` arm, digesting the frozen pool's recorded artifacts and scores, never hidden tests; the `{"insights"}` object is extracted leniently; a failed call falls back to empty guidance with `dream.llm_fallback`, an aborted one aborts the run) |
| `refine.plan`         | `refine.source` (`user`/`self`/`auto`), `refine.reason` (`manual`/`refine_run`/`recurrence`/`regression`/`turn_interval`/`compact`/`rollback`), `refine.kind` (`directed`/`checkpoint`/`failure`), `refine.scope`, `refine.rollback`, `refinement.id`, `refine.edits`, `trigger.trace_id`, `refine.skipped`, `refine.replan_of` (on a re-plan); only when the RAVO gate ran: `refine.recurring_failures`, `refine.evidence_drift` (`none`/`appended`/`rewritten`), `refine.snapshot_messages`, `refine.judge_messages`, `refine.drift_messages`, `refine.drift_removed_messages`, `refine.drift_chars`, `refine.snapshot_leaf_id`, `refine.judge_leaf_id`, `ravo.decision`, `ravo.fast_score`, `ravo.deep_score`, `ravo.missed`, `ravo.missed_weight`, `ravo.claimed`, `ravo.measurable`, `ravo.judge_error`, `referee.cleared`, `referee.upheld`, `referee.unverifiable`, `referee.no_evidence`, `referee.not_applicable`, `refine.stale_evidence` | done | coding-agent `core/agent-session.ts` (`_planRefine`; a detached root, see [Refinement outcome records](#refinement-outcome-records). `refine.scope` is corrected to the baseline scope once a rollback target resolves it. An extension skip ends ok with `refine.skipped`; a throw ends error. The judge always reads the live conversation: the `refine.drift_*` attributes are measured against the proposer's snapshot on the same tick as the judge call, never cancel it, and `refine.stale_evidence` is set once the judge has answered) |
| `refine.apply`        | `refinement.id`, `refine.scope` (the target scope), `refine.decision` (`commit`/`commit_unmeasured`/`reject_screen`/`reject_deep`/`reject_criteria`/`reject_unclaimed`/`partial`/`rollback`/`no_edits`), `refine.applied_edits`, `refine.trust_window_opened`, `trigger.trace_id`, `refine.history_record` (`appended`/`failed`/`skipped`), `refine.replan_of` (on a re-plan); once the gate ran: `refine.stale_evidence`, `refine.replan_scheduled`; on a rejection `refine.rejection_cause` (`gate`/`screen`/`judge_unavailable`/`baseline_changed`/`stale_evidence`); on an apply `trust.faulted`, `trust.clean`, `trust.contested` | done | coding-agent `core/agent-session.ts` (`_applyRefine`; a detached root. The decision is final here, not at the gate: a gate commit whose certificate no longer matches the baseline becomes `reject_deep`, an incomplete apply `partial`, and a commit that claimed no fingerprint `commit_unmeasured`. `refine.history_record` is whether the result reached its scope's refinement history; `trust.*` count the trust windows this apply settled) |
| `package.install` / `package.remove` / `package.update` / `package.check_updates` | `package.source`, `package.local`, `package.count`, `package.updates` | done | coding-agent `core/package-manager.ts` |
| `package.command`     | `command` (program + first arg), `exit_code`, `signal` | done | coding-agent `core/package-manager.ts`, `package-manager-cli.ts` (nested git/npm child processes) |
| `update.check`        | `update.current`, `update.latest`, `update.available`, `http.status` | done | coding-agent `utils/version-check.ts` |
| `update.self`         | `update.from`, `update.to` | done | coding-agent `package-manager-cli.ts` |
| `tools.download` / `tools.release_lookup` | `tool`, `version`, `bytes`, `tool.repo`, `http.status` | done | coding-agent `utils/tools-manager.ts` |
| `historian.run` / `historian.subagent` / `historian.validate` / `historian.publish` | `historian.session_id`, `historian.chunk_start/end`, `historian.model`, `historian.status` (run), `historian.pass`, `historian.outcome` (subagent), `historian.valid`, `historian.compartments`, `historian.facts`, `historian.failure_reason` | done | Magic Context `packages/pi-plugin/src/pi-historian-runner.ts` (via the optional pi-trace bridge) |
| `auth.refresh` / `auth.catalog` / `auth.route` | `auth.reason`, `auth.account`, `auth.source`, `auth.outcome`, `http.status`, `catalog.models`, `catalog.cached`, `auth.pool_size`, `auth.selected` | done | anthropic-auth `packages/pi/src/{shared-refresh,index,stream}.ts` (via `trace-bridge.ts`) |
| `harness.ledger.flush` | `session.id`, `ledger.scope`, `ledger.observations`, `ledger.regressions`, `ledger.verifications`, `ledger.fingerprints`, `trust.recurrences`, `trust.adjudications`, `trust.faulted`, `trust.clean`, `trust.contested` | done | coding-agent `core/agent-session.ts` (global failure ledger read-modify-write under the cross-process harness state lock, at an assistant turn boundary or `agent_end` when observations, regressions of global champions, replay verifications, or trust evidence are pending; skipped while a refine plan or apply is in flight. It settles the global trust windows on the merged ordinal, so `trust.*` count what this flush closed. A flush that writes the verdicts of a finished `harness.trust.adjudicate` batch is itself a root. On by default; `PRIME_AGENT_GLOBAL_LEDGER=0` keeps the ledger per-session and opens no flush span) |
| `harness.trust.adjudicate` | `session.id`, `trigger.trace_id` (only when every job in the batch shares one), `trust.jobs`, `trust.windows`, `trust.ran`, `trust.upheld`, `trust.cleared`, `trust.unverifiable`, `trust.skipped`, `trust.aborted` | done | coding-agent `core/refinement/trust-adjudication.ts` (`adjudicateTrustRecurrences`: the post-commit replays, a detached root started by `AgentSession._drainTrustAdjudicationBacklog` when an actionable failure a committed refinement claimed recurs inside its trust window, the recurrence's own derived case probes an import the window's skill still has exactly as the commit recorded it, and the global ledger is on. One `ravo.referee` child per (window, skill entry, fingerprint); a job whose case has not reproduced yet waits for `ravo.replay_verify` and is dropped when no pending self-check can verify it. At most 8 jobs per batch, one batch at a time, at most 3 runs per (window, entry, fingerprint), and never re-run once upheld. Dispose aborts it and an aborted job records nothing; it ends ok whatever the verdicts) |
| `recall.mark`         | `trigger.trace_id`, `recall.repo_key`, `recall.dirty_count`, `recall.claims`, `recall.unverifiable`, `recall.ms`, `recall.skipped`, `recall.skip_reason` (`git_timeout`/`git_unavailable`/`not_repo`/`lock_busy`/`write_failed`), `recall.negative_cache` | done | coding-agent `core/extensions/builtin/workspace-recall.ts` (a detached root opened on `agent_end` of a top-level session inside a git worktree; writes the repo's mark under a lock. `recall.dirty_count` and `recall.unverifiable` include the paths past the mark's limits; `write_failed` ends error) |
| `recall.witness`      | `recall.repo_key`, `recall.has_mark`, `recall.changed`, `recall.changed_unknown`, `recall.unchanged` (absent when it cannot be established), `recall.unverifiable` (listed paths), `recall.uncompared` (paths that could not be compared, listed or not; a lower bound when a partial mark left paths unrecorded), `recall.claims_current`, `recall.claims_expired`, `recall.head_moved`, `recall.block_bytes`, `recall.skipped`, `recall.skip_reason` (`git_timeout`/`deadline`/`git_unavailable`/`not_repo`), `recall.negative_cache` | done | coding-agent `core/extensions/builtin/workspace-recall.ts` (child of the `tool_result` `extension.hooks`, on the first `ipython` result of a top-level session inside a git worktree; bounded by a 1 s deadline. `recall.block_bytes` is the size of the `<workspace_recall>` block appended to the result, at most 2048) |
| `recall.digest`       | `recall.phase` (`tool_call`/`tool_result`), `recall.repo_key`, `recall.verifiable`, `recall.digest_matched`, `recall.ms`, `recall.skipped`, `recall.skip_reason` (`git_timeout`/`deadline`/`git_unavailable`/`not_repo`), `recall.negative_cache` | done | coding-agent `core/extensions/builtin/workspace-recall.ts` (the workspace digest a build claim is checked against: in the `tool_call` `extension.hooks` of an `ipython` cell whose source names a build or test command, and in its `tool_result` hooks only when the cell reported a qualifying `bash()` command that exited 0, with `recall.digest_matched`; bounded by the same 1 s deadline) |
| `toolforge.publish`   | `toolforge.name`, `toolforge.import`, `toolforge.status`, `toolforge.installed`, `toolforge.gate_runs`, `toolforge.reason` | done | coding-agent `core/toolforge/publish.ts` (one per `rlm.toolforge.publish` host request; `status: rejected` carries the refusal reason) |
| `toolforge.gate`      | `toolforge.name`, `toolforge.negative`, `toolforge.positive`, `toolforge.passed` | done | coding-agent `core/toolforge/publish.ts` (the double run; each half spawns a `ravo.replay_case` child, and a run that cannot be performed never passes) |
| `trajectory.seal`     | `windows`, `labelled`, `withheld`, `backfill` | done | coding-agent `cli/learning-command.ts` (`learning trajectory`, an off-turn-path CLI root wrapping `sealTrajectoryWindows` + the store write of the Engineer Trajectory Index; attrs are counts only. The turn-path `readTrajectoryIndex` opens NO span — a plain sealed-file read guarded by a stat cache, like `loadHarnessState`) |

Supporting pieces:

| piece                                   | status | file |
|-----------------------------------------|--------|------|
| trace context API + `withSpan`          | done   | pi-ai `trace-context.ts` |
| log stamping + `span_end` entries       | done   | pi-ai `log.ts` |
| `AsyncLocalStorage` install, `TRACEPARENT` inbound read | done | coding-agent `core/logging.ts` (`withInboundTraceContext`, called from `main.ts`) |
| worker/daemon envelope `traceparent`    | done   | coding-agent `modes/daemon/daemon-protocol.ts`, `daemon-worker-protocol.ts`, `daemon-worker-client.ts` |
| Python context + OTel bridge            | done   | `prime-agent-runtime/src/rlm/trace.py` |
| session records stamped `traceId`/`spanId` | done | coding-agent `core/session-manager.ts` (`stampTraceContext` in the single append path) |
| `prime-agent trace` viewer              | done   | coding-agent `cli/trace-command.ts` (registered in `cli/command-registry.ts` + `cli/public-command.ts`) |

## Carriers (how the context crosses a boundary)

| boundary                         | carrier                                      |
|----------------------------------|----------------------------------------------|
| async continuation in one process| `AsyncLocalStorage` (TS) / `contextvars` (Py)|
| TS -> Python kernel               | JSONL request frame field `traceparent`      |
| Python -> TS `host_request`       | event frame field `traceparent`              |
| kernel process spawn              | env `TRACEPARENT`                            |
| daemon supervisor -> worker       | command envelope field `traceparent`         |
| child session spawn (rlm)        | worker env `TRACEPARENT` + create command    |
| external caller -> prime-agent   | env `TRACEPARENT` (read once at startup)     |

## Logging contract

Every `LogEntry` written to `~/.prime/agent/logs/agent.jsonl` gains
`traceId`, `spanId` and, when present, `parentSpanId`, filled from the
active context at emit time. Span completion is itself a log entry:

```json
{"component":"trace","msg":"span_end","name":"llm.request","traceId":"…","spanId":"…","parentSpanId":"…","durationMs":812,"status":"error","attrs":{"llm.provider":"openai","llm.base_url":"https://api.openai.com/v1"}}
```

Provider failures (`ai.provider` / `provider stream failure`) additionally
log `baseUrl` so a mis-routed request is visible from the failure line
alone.

## Refinement outcome records

A refinement reports its final decision once, at apply time, as one
`coding-agent.refinement` log line. The gate's decision is never logged on its
own, because the apply can still downgrade it. `AgentSession._applyRefine`,
`refineHarness` and `RavoRunService` write these lines.

| `msg` | fields | written for |
|---|---|---|
| `refinement.committed` | `proposalId`, `addressed`, `deepScore`, `missed`, `reason`, `scope` | a `commit` that claimed at least one fingerprint; the learning index's treated cohort |
| `refinement.applied_unmeasured` | `proposalId`, `deepScore`, `reason`, `scope` | `commit_unmeasured`, `rollback`, and a `commit` with nothing addressed |
| `refinement.rejected` | `proposalId`, `decision`, `deepScore`, `missed`, `claimed`, `reason`, `scope`, `cause` (`reject_*` only) | every `reject_*` decision, `partial`, and `no_edits` |
| `refinement.history_append_failed` (warn) | `proposalId`, `scope`, `code` | the durable history append failed; the refine still reports its outcome |
| `refinement.history_unreadable` (warn) | `scope`, `code` | a history file that exists but cannot be read, treated as empty |

Every refinement result, applied or rejected, is also appended to a durable
history: `<agentDir>/harness/refinements.jsonl` for a global refine, and
`<agentDir>/harness/local-refinements/<sessionId>.jsonl` for a local one
(`refine.history_record` is `skipped` when the session has no artifact dir).
The next proposer reads a rejection's gate decision, the judge's cleaned and
quoted rationale and its missed criteria ids, never its scores; for a refine
that carries trigger fingerprints it also reads up to three recent rejections
other sessions recorded for the same failures.

Drift is measured by message identity against the copy of the conversation the
proposer read: a readable message the proposer never saw is `appended`, and one
it read that the live conversation no longer holds (a compaction, a rewind, a
retry dropping a partial reply) is `rewritten`. Neither cancels the refine, and
the judge reads the live conversation either way. A rejection counts as stale
only when the drift is not `none`, the judge itself refused it (`reject_deep`,
`reject_criteria` or `reject_unclaimed`, never the structural screen and never a
judge error), and no referee verdict was `upheld`, `unverifiable` or
`no_evidence` — a mechanical verdict holds whatever the conversation did. Such a
line carries `staleEvidence`, `driftKind` and `driftMessages`, plus
`replanScheduled` when its round stays open for one re-plan. That re-plan is a
further `refine.plan`/`refine.apply` root pair, and every line and span of it
carries `replanOf`/`refine.replan_of` naming the rejection it re-planned.

A `ravo.run` (or `/ravo`) writes one record per evaluated proposal, with
`reason` `ravo_run`: `refinement.rejected` when the gate rejects it (on its
`ravo.round` span) or the commit gate refuses it (`partial` when its edits do
not apply), and, once its state is saved, `refinement.committed` or
`refinement.applied_unmeasured` (on the commit-gate `ravo.evaluation` span,
whose `ravo.proposal_id` is the line's `proposalId`). Its `addressed` holds only
fingerprints the proposal claimed, that recur in the ledger of the store it runs
against, that the judge named (as `<fp>` or `failure:<fp>`), and whose
`failure:`/`referee:` criteria the certificate did not count as missed, so a run
with no judge (`arc_agi`) never logs `refinement.committed`. Its `missed` is the
certificate's count, which is 0 when the proposal stopped at the screen or the
deep gate, and a proposal a stopped run never evaluated logs nothing. No span
attribute carries `ravo_run`.

`refine.plan`, `refine.apply`, `ravo.replay_verify`, `harness.trust.adjudicate`
and `recall.mark` are detached roots. Each runs after, or alongside, the turn
that started it, so as a child it would outlive its parent. On `refine.plan`,
`refine.apply`, `harness.trust.adjudicate` and `recall.mark`,
`trigger.trace_id` names that turn's trace where one was active, and
`refinement.id` joins a plan to its apply.

Switches that change what these spans report. Each is on unless set to `0`,
`off`, `false` or `no` (`PRIME_AGENT_RAVO` does not accept `no`):

* `PRIME_AGENT_GLOBAL_LEDGER`: the failure ledger is also kept in the global
  harness state, recurrence is counted there, and `harness.ledger.flush`
  writes it. Off, the ledger stays per-session, a local provisional window runs
  on the session ledger's ordinal (clock `local-ordinal`), and a global window
  opened meanwhile has no clock and is never checked. Trust windows are measured
  on the global ordinal, so a recurrence inside one is recorded and adjudicated
  only while it is on; off, a session advances no ordinal and settles no window
  of its own.
* `PRIME_AGENT_RAVO`: off, `refine.plan` carries no `ravo.*`, `referee.*` or
  `refine.recurring_failures` attributes, and a fully applied refine other than
  a rollback records `commit_unmeasured`.
* `PRIME_AGENT_WORKSPACE_RECALL`: off, no `recall.*` span is opened and no
  mark is written. The extension is also left out of RLM child runtimes and of
  runs started with `--no-extensions`.

A Workspace Recall mark is `<agentDir>/recall/<repoKey>.json`, where `repoKey`
(`recall.repo_key`) is the repo root's basename, with characters outside
`[A-Za-z0-9._-]` replaced, a dot, and the first 16 hex characters of the sha256
of the resolved root path. It holds digests (`digestAlgorithm: "sha256-128"`),
paths, HEAD and build claims, never file content or command output.

Workspace Recall keeps a negative cache per repo. A git call that times out
(3 s) writes `<agentDir>/recall/<repoKey>.skip.json` with reason `git_timeout`
for 10 minutes, and every process sharing the agent dir then skips marks,
witnesses and digests for that repo with `recall.negative_cache=true`. A missed
1 s tool-path deadline is never written there: it keeps only that runtime's
witness and digests off the repo, in memory, for 60 s, and marks still run. A
`deadline` entry found in the file is ignored.

## Harness trust records

Trust moves on measured outcomes only, and each move is one
`coding-agent.harness-trust` log line. They are written where a trust window
settles: at a failure ledger flush (local windows on the global observation
ordinal, global windows under the harness state lock) and on the applied branch
of `refine.apply`. The learning index reads neither.

| `msg` | fields | written for |
|---|---|---|
| `harness.trust.settled` | `proposalId`, `scope`, `from`, `outcome` (`clean`/`contested`/`faulted`), `ordinal`, `fingerprints` | one per window this settlement closed |
| `harness.trust.adjusted` | `proposalId`, `scope`, `entry` (`kind:id`), `reason` (`clean_window`/`measured_fault`), `delta`, `before`, `after`, `dormant`, `fingerprintId` (on a fault) | one per entry whose score moved |

`fingerprints` is the upheld fingerprints for a `faulted` window, the recurred
ones for a `contested` one, and every claimed one for a `clean` one. `delta` is
`+5` for a clean window, charged to every entry the commit touched, and `-15`
for a measured fault, charged once per window to the skill entry the replay ran
for; `dormant` says the entry fell below the trust threshold and is no longer
rendered into the prompt. A local ledger flush opens no span of its own, so its
lines are stamped with whatever span was active where it ran (a turn boundary,
`agent_end`, dispose); the global flush and `refine.apply` stamp theirs with
their own span.

## Session records

Every entry appended to a session file (`~/.prime/agent/sessions/<id>.jsonl`:
messages, custom entries, model changes, compactions, ...) carries optional
`traceId` and `spanId` copied from the span active at append time. They are
omitted when no span is active, so files written outside a traced turn are
unchanged and older readers keep working (unknown keys are ignored on load).
`parentSpanId` is deliberately not stored: the session record is a join key
into the log, not a second span store.

## Python runtime (`prime-agent-runtime/src/rlm/trace.py`)

* `parse_traceparent(str) -> TraceContext | None`, `format_traceparent(ctx)`.
* `current() -> TraceContext | None` (contextvar backed).
* `start_span(name, **attrs) -> Span` context manager: mints a child span,
  sets it current, restores parent on exit, and emits a `span_end` trace
  event through the repl event stream (`{"event":"trace", ...}`) so the host
  writes it to the shared log with the same shape as TS spans.
* `inject_env(env: dict) -> dict` sets `TRACEPARENT` for subprocesses.
* Optional bridge: if `opentelemetry` is importable, the current context is
  also attached to the OTel context so user code that uses the OTel SDK
  sees the same trace; absence of the package is not an error.
* `rlm.repl` reads `traceparent` from every `execute`/`snapshot`/`restore`
  request, runs the cell under `start_span("kernel.cell")`, and stamps
  `traceparent` into every `host_request` frame it emits.

## How to use

### Find a trace id

* From a log line: every entry in `~/.prime/agent/logs/agent.jsonl` written
  while a span was active has `"traceId":"<32 hex>"`. For example, to find
  the trace behind the most recent provider failure:

  ```sh
  grep '"provider stream failure"' ~/.prime/agent/logs/agent.jsonl | tail -1 | grep -o '"traceId":"[0-9a-f]*"'
  ```

* From a session file: the message/turn records in
  `~/.prime/agent/sessions/<sessionId>.jsonl` carry the same `traceId`
  (and the `spanId` of the turn that produced them):

  ```sh
  grep -o '"traceId":"[0-9a-f]*"' ~/.prime/agent/sessions/<sessionId>.jsonl | sort | uniq -c
  ```

* From a kernel frame, a daemon envelope or a subprocess environment: the
  `traceparent` / `TRACEPARENT` value can be passed to the command as-is; the
  trace id is extracted from it.

### `prime-agent trace`

```
prime-agent trace <traceId|traceparent> [--log <path>] [--json]
```

Reads `~/.prime/agent/logs/agent.jsonl` and its rotated sibling
`agent.jsonl.old` (see `appendRotatingLog` in `config.ts`; the writer keeps a
single previous generation), keeps the entries for one trace id and prints a
tree:

```
trace 0af7651916cd43dd8448eb211c80319c  (3 spans, 3 log lines, /home/me/.prime/agent/logs/agent.jsonl)
├─ agent.turn  1050ms  ok  session.id=abc turn.index=1  [b7ad6b7169203331]
│  ├─ 10:00:00.100  info   session  turn started  sessionId=abc pid=4242
│  ├─ llm.request  750ms  error  llm.provider=openai llm.base_url=https://api.openai.com/v1  error=401 archived  [c8be7c8270314442]
│  │  └─ 10:00:00.200  info   ai.provider  request  baseUrl=https://api.openai.com/v1 pid=4242
│  └─ tool.execute  50ms  ok  tool.name=bash tool.call_id=call_1  [d9cf8d9381425553]
└─ (no span)
   └─ 10:00:01.200  debug  daemon  context only  pid=4242
```

* Spans are the `span_end` entries, nested by `parentSpanId`, showing name,
  `durationMs`, status, every attribute (`llm.provider`, `llm.base_url`,
  `tool.name`, `kernel.request_id`, `rlm.child_id`, ...) and the recorded
  error. The trailing `[spanId]` lets you grep the raw log for one span.
* Log lines sit under the span whose `spanId` they carry, interleaved with
  child spans in time order; every other field on the line (including `pid`,
  so cross-process traces stay attributable) is shown as `key=value`.
* A span that has not ended yet (the turn is still running, or its end was
  rotated away, or it belongs to an external caller) is shown as
  `(open span) <spanId>` so its children and lines are still grouped.
* Lines that carry the trace id but no span id go under `(no span)`.
* `--json` prints the raw matching log lines (oldest file first) for piping
  into `jq`.
* `--log <path>` reads another log file (a copy from another machine; its
  `<path>.old` sibling is read too when present).
* Exit code 1 with an `Error:` line on stderr when the id is malformed, the
  log does not exist, or nothing matched.

### `prime-agent health`

```
prime-agent health [--since <duration>] [--stuck-after <duration>] [--limit <n>] [--log <path>] [--json]
```

Reads the retained compressed generations, `agent.jsonl.old`, and `agent.jsonl`
without contacting the daemon. It gives operators a bounded summary of recent:

* failed `historian.*` spans;
* provider stream failures and failed `llm.request` spans (deduplicated by trace);
* likely stuck turns and retained `bash.command`, `kernel.cell`, `kernel.execute`,
  `rlm.child`, `cargo_lock_wait`, and `bootstrap_lock_wait` starts with no matching end;
* unexpected kernel exits, fatal process crashes, child lifecycle failures, and failed
  lock-wait spans;
* orphan-journal corruption, write, and reap failures reported through structured logs; and
* daemon recovery log lines that report a failure, interruption, cancellation,
  or unanswered recovery probe.

The default window is 24 hours, the stuck threshold is 10 minutes, and at
most 20 incident details are printed (hard maximum 200). Counts always cover
the full selected window. Open-span correlation is retained independently of
the 100,000-entry analysis buffer, so high-volume logs do not hide an old open
operation. Python `bash.command` and `kernel.cell` `span_start` records may carry
`bash.command` and `kernel.cell` attributes; health accepts these records without
requiring the attributes.

`--json` includes `status` (`healthy`, `unhealthy`, or `unknown`), `parseErrors`,
`stale`, the counts, and the bounded incident list. Exit status is 0 only when
recent evidence is valid and has no incidents. It is 2 for incidents or UNKNOWN
(malformed, empty, or stale evidence), so unattended scripts fail closed; usage
and file-read errors remain exit status 1. The command is a retained-log heuristic,
not a live health probe. Retention can remove a span completion and create a false
open-operation candidate, and successful recovery lines are intentionally omitted.
Use the reported trace id with `prime-agent trace` for the full timeline.

### Parenting Prime Agent from outside

Any process that starts `prime-agent` can hand it a span through the W3C
environment carrier; everything the run traces then becomes a child of that
span and shares its trace id:

```sh
TRACEPARENT=00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01 prime-agent -p "summarize the failing test"
prime-agent trace 0af7651916cd43dd8448eb211c80319c
```

`main.ts` reads the variable once at startup (`withInboundTraceContext` in
`core/logging.ts`); a malformed value is ignored rather than failing the
run. The same variable is what Prime Agent sets when it spawns the Python
kernel (`injectTraceparentEnv` in `core/kernel/repl-manager.ts`), and daemon
command envelopes carry the equivalent `traceparent` field, so a trace id
chosen by CI or by a parent agent is the one that appears on every log line,
session record and kernel cell below it.

## Optional OTLP export and derived metrics

`createOtlpSpanExporter()` in pi-ai is a fully opt-in adapter. Creating it does
not replace the JSONL reporter: attach `exporter.sink` with `addSpanSink()` and
call the returned unsubscribe function during shutdown. The adapter posts
OTLP/HTTP JSON to `<endpoint>/v1/traces` and `<endpoint>/v1/metrics` using the
built-in `fetch`; no OpenTelemetry package is required.

```ts
const exporter = createOtlpSpanExporter({
  endpoint: process.env.OTEL_EXPORTER_OTLP_ENDPOINT!,
  headers: { Authorization: `Bearer ${process.env.OTLP_TOKEN}` },
  serviceName: "prime-agent",
});
const unsubscribe = addSpanSink(exporter.sink);

// On orderly process shutdown:
unsubscribe();
await exporter.shutdown();
```

Prime Agent creates and attaches the adapter only when
`OTEL_EXPORTER_OTLP_ENDPOINT` is set. `OTEL_EXPORTER_OTLP_HEADERS` optionally
provides comma-separated `key=value` request headers. When the endpoint is
unset, the path has no timer, queue, network calls, or derived-metric work.
Export is diagnostic-only: sink and transport failures are swallowed. Orderly
CLI completion drains it for up to one second and then proceeds with exit.

The defaults batch 128 spans, retain at most 2,048 queued spans (dropping the
oldest and exposing the count through `stats()`), flush every 5 seconds, and
bound derived metrics to 256 distinct span names. Each flush exports delta
`prime_agent.span.count`, `prime_agent.span.error_count`, and
`prime_agent.span.duration_ms` sums grouped by `span.name`. `flush()` sends one
batch; `shutdown()` stops the unrefed timer and drains all queued batches.
All bounds and intervals are configurable for a host integration.

## Local log safety and retention

All local diagnostic writes pass through `appendRotatingLog`. The logs directory
is owner-only (`0700`) and log generations are owner-readable (`0600`) on POSIX.
High-confidence bearer tokens, API keys, refresh/access tokens, client secrets,
password assignments, JWTs, and common provider tokens are redacted before disk.
Rotation is serialized across processes. The active file and newest `.old` remain
plain text; older generations are gzip-compressed. Five total generations are kept
by default. Set `PRIME_AGENT_LOG_RETENTION` to an integer from 1 to 100 to change
the bound. `prime-agent trace` and `prime-agent health` read all retained generations.

Only long-running operations emit `span_start`, which makes a silent crash or hang
visible without doubling all trace traffic. Successful `extension.hooks` spans
under 25 ms are suppressed; failures and slow hooks remain visible. A suppressed
hook span still parents what ran inside it, so a child of a fast hook (a
`recall.digest` skipped by the negative cache, say) appears under
`(open span)` in `prime-agent trace`.

## Non-goals

* No `@opentelemetry/*` runtime dependency. The OTLP/HTTP JSON adapter is built in.
* No collector is contacted unless `OTEL_EXPORTER_OTLP_ENDPOINT` is explicitly set.
* Derived metrics are process-local deltas exported through OTLP; Prime Agent does
  not embed a metrics database, dashboard server, or alerting engine.
