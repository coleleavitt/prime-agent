# pa-ravo

RAVO (Recursive Agentic Variation with co-evolving Opponents) on the continual harness: every planned `/refine`
proposal meets a gate before it applies — a structural screen, one deep-judge model call, the referee re-running the
verified replay cases of the failures the judge credits, and the pure reducer's decision against a weighted opponent
pool — and the decision is bound to digests of the proposal and the harness baseline it was judged against. A commit
that claims recurring failures is a provisional champion with an observation window; a claimed failure recurring inside
the window is recorded on it when the failure ledger flushes.

Behavioural spec: the TS product's `packages/coding-agent/src/core/ravo/{reducer,authority,referee,referee-runner,
python-environment,canonical-json}.ts`, `refinement/ravo.ts`, and the gate and regression half of `agent-session.ts`
(`_planRefineInSpan`, `_applyRefineInSpan`, `_gateRecurringFailures`, `_provisionalWindowClock`,
`_observeFailuresAtTurnBoundary`'s regression search, `_flushFailureLedger` / `_flushGlobalFailureLedger`'s recording)
on `perf/session-catalog-resume`, and `docs/ravo-architecture.md` there.

## Scope

- The reducer: lineage, champion chain, opponent pool, weakness pressure, pool extension, provisional windows and
  their clocks (`ordinal`, `local-ordinal`), the `ravoW` invariants, the one-shot step and its certificate.
- The assisted authority: the five assisted criteria, failure opponents (`failure:<fp>`, dormant when not recurring),
  referee opponents (`referee:<fp>`), dormant passes for criteria `/refine` never observes, the unclaimed-commit
  policies, digest binding, legacy-state migration and unknown-clock stripping.
- The referee: applicability (missing-module and missing-distribution probes against the proposal's skill imports),
  the five verdicts, adjudication over a `ReplayRunner`, and `PythonReplayRunner` (kernel Python, `-I -B`, program on
  stdin, fresh temporary working directory, sanitized environment, own process group killed at the end of the run and
  journaled as a possible orphan while it runs, 10 s timeout).
- The gate: fast screen, judge prompt and reply parsing, decision, missed criteria and weight, rejection causes, the
  baseline view (schema, entries, `ravo` without recorded recurrences) and carrying recorded recurrences into an
  authorized state, the charged recurring failures and the window clock of a non-failure refine.
- The session feature: the refinement gate for every session (kill switch `PRIME_AGENT_RAVO=0|off|false`), the
  rejected result, the outcome log line, the adoption event, the failure-ledger observer.
- Failure-triggered refines (`_queueFailureTriggeredRefine`, `mergeRefineRequests`, `withRefineRun`): a fingerprint
  entering the recurring set queues a `recurrence` refine, a regressed champion a `regression` repair in its own scope
  (local and global repairs separately), each fingerprint once per kind per session, only in sessions that may
  auto-refine with auto-refine on. A request merges into a pending one of its scope (instructions appended, the stronger
  reason, a failure refine only if both were, the union of triggers), parks behind one of the other scope (released at
  the next boundary that finds the slot empty), and an agent `refine.run` joining it makes it directed. A refine is
  charged its triggers (on the session's own record when the judged ledger lacks them) and, unless it is a failure
  refine, the recent recurrences; a failure refine that claims nothing is `reject_unclaimed`; results carry
  `triggerFingerprintIds`.
- Replay self-checks (`_startReplayVerification`): each boundary's newly derived, unverified cases run off the turn
  path in the sanitized environment, one batch at a time per session on a thread of their own, each (fingerprint,
  source) once per session and never one the ledger already holds verified; a reproduction is queued through
  `LedgerHandle::record_replay_verifications` for the ledger's next flush. At exit the feature waits for running
  checks until the flush deadline (it is installed before the ledger, which flushes after it).

## Non-goals (this slice)

- Releasing a queued failure refine's fingerprints when it is cancelled before it applies (TS `refine_failed`): the
  native turn boundary drops a pending refine on an aborted turn without telling the feature, so a dropped request's
  fingerprints stay triggered for the rest of the session.
- Trust windows (`trustWindows`, `harness-trust.ts`, `trust-adjudication.ts`): opening a window at commit, recording
  evidence, settling at flushes, the `trust.*` flush attributes.
- The skill dry-run in the fast screen (`skill-dry-run.ts`); the screen is structural only.
- Evidence drift and the stale-evidence re-plan; the RAVO archive (`refinement-ravo/`); the rejection-history and
  related-rejection prompt sections; per-session `local-refinements/<id>.jsonl`.
- `ravo.run` / `/ravo` (`RavoRunService`, controller, retained worker runtime, context view, error-budget ledger), the
  skill, the `ravo_run_update` daemon event and the agents-view line, and the ARC-AGI evaluator (benchmark code; keep it
  outside the product).

## Seams

- `pa_core::features::SessionFeature::refinement_gate` → `pa_core::refinement::gate::RefinementGate`: `begin_refine`
  holds the session's ledger flushes for the whole refine (the guard requests a flush when it drops), `evaluate` runs
  the gate with the one model call the session hands it, the verdict admits or refuses against the target store re-read
  at apply time and records `ravo` into the saved state and the report on the result.
- `RefinementGate::lock_store`: a global refine holds the harness state lock (`pa_ledger::acquire_harness_state_lock`,
  the TS `proper-lockfile` protocol every ledger flush takes) from the re-read of the global store until its save.
- `RefinementGate::attach_refine_requester`: the session's `RefineRequester`, through which RAVO queues its own refines
  (with a `RefineTrigger` carrying `{reason, kind, triggerFingerprintIds}`) onto the pending refine the next serviced
  turn boundary consumes. The ledger reports boundaries from its worker thread, so a request lands at the boundary the
  host services after the worker processed the turn (TS queued synchronously at `message_end`).
- `RecurrenceFilter` (`RavoFeature::attach_recurrence_filter`, attached by `pa-cli`): at each boundary with newly
  recurring failures the filter names fingerprints whose recurrence refine is not queued (TS
  `_trajectoryInternalizedReminders`, implemented by `pa-learning`). It is shown the fingerprints recurring in the
  session's own ledger, and RAVO never mutes one of those (the live-recurrence override); a muted fingerprint stays
  untriggered, so its reminder fires once the filter lets it through.
- `pa_ledger::LedgerObserver` (built into `FailureLedgerFeature::with_observers` by `pa-cli`) and
  `pa_ledger::LedgerHandle` (`attach_ledger`): `on_boundary` finds provisional regressions on each window's own clock
  (local lineage on the local and, with the global ledger on, the global ordinal; the global lineage on the global
  ordinal), `on_flush` records them into the `ravo` key of the document being written (a regression stays pending
  while the state has no lineage), `on_flush_result` drops what landed, `hold_flush` holds while a refine runs,
  `wants_global_flush` asks for a global flush when global regressions are pending. The global flush's
  `harness.ledger.flush` span gets `ledger.regressions` through `pa_types::trace_context::SPAN_ATTRIBUTES_TARGET`.

## Files owned

- The `ravo` key of `<sessionArtifactDir>/harness/harness_state.json` and `<agentDir>/harness/harness_state.json`
  (`RavoState`, field order of the TS objects; a new key lands before `failures` / `trustWindows`, where a TS save puts
  it). Byte-compatible with the TS product: `tests/golden.rs` replays the reducer, authority, normalization, digests and
  the full gate report and judge request against outputs of the TS sources run under node
  (`tests/fixtures/golden/generate.ts` regenerates them).
- The `ravo` and `rejectionCause` keys of the refinement results the session records.

Known, deliberate differences: `localeCompare` is ICU's root order for printable ASCII (the identifiers RAVO sorts);
other characters sort by code point. A JSON number `serde_json` parses one ULP off V8 (very large or long floats) digests
differently. A judge reply that is not JSON reports `serde_json`'s parse error where TS reported V8's.

## Telemetry

`ravo_gate_decision` (schema v4): `decision`, `scope`, `reason`, `cause`, `recurring` (failure opponents charged),
`claimed` (fingerprints credited). Never a fingerprint, a proposal, a score or judge text. The outcome line
(`refinement.committed`, `refinement.applied_unmeasured`, `refinement.rejected`) is a `tracing` event under
`pa_ravo::refinement`.

## Public API

`RavoFeature` (`new`, `ledger_observer`, `attach_ledger`, `attach_recurrence_filter`, `wait_replay_checks`),
`RavoOptions`, `RecurrenceFilter`, `ravo_enabled`; the reducer
(`ravo_step`, `ravo_w`, `ravo_pressure`, `ravo_extend_opponents`, `ravo_mark_provisional`, `ravo_observe_champion`,
`ravo_best_score` and their types); the authority (`authorize_assisted_ravo`, `normalize_assisted_ravo_state`,
`ravo_artifact_digest`, binding checks); the referee (`adjudicate_failure_claims`, `ReplayRunner`,
`PythonReplayRunner`, verdict helpers); the gate (`ravo_evaluate_proposal`, `RavoGateReport`,
`refinement_baseline_view`, `carry_observed_recurrences`, `extract_judge_json`, `proposal_artifact`).
