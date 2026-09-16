# Workflow V2 Slice 3 + OS-fence — Independent Implemented-Byte Terminal Review

**Verdict: BLOCK** (one narrow, test-only typecheck regression; every safety-critical invariant PASSES)

- Reviewed tree: `/tmp/prime-agent-osfence` (prime-agent fork), base `2e4a11cbed9df708a2db6fdf7bd817d840303eb6`
- Working tree: uncommitted (5 tracked modified + 16 untracked new source/test files)
- Contracts checked against: pi-plugin-workflow `WORKFLOW-V2-SLICE3.md` (9606583) and `WORKFLOW-V2-SLICE3-OSFENCE.md` (a5babbd)
- Method: read the actual bytes; ran the real vitest suite, independent mutant tests, git-stash A/B, and the repo typecheck/lint gates. Repo skills were not consulted.

---

## Summary

All seven contract checks pass on their substance **except** the repository typecheck gate:
`tsgo --noEmit` (the exact command in `npm run check` / `prepublishOnly`) reports **3 errors** in **two copied Slice 3 test files**, whereas the clean base compiles with **0 errors**. This regresses a previously-clean gate, so a release/terminal PASS cannot be claimed. The fix is trivial and test-only; no production source, safety invariant, or runtime test is affected.

---

## Check-by-check evidence

### (1) Capability truly `CAPABILITY_UNAVAILABLE` — PASS
- `negotiateWorkflowV2Capability(...)` unconditionally returns `{ available: false, code: "CAPABILITY_UNAVAILABLE" }` (`workflow-v2-capability.ts`).
- `resolveOsfenceMode` returns `{ enabled:false, capability:"unavailable", reason:"workflow_v2_capability_unavailable" }` on `!capabilityAvailable` before any other precondition (`daemon-osfence.ts`).
- The end-to-end pipeline (`WorkflowV2OsfencePipeline` / `resolveWorkflowV2OsfencePipeline`) is imported **only** by its test: `grep` of `workflow-v2-osfence-integration` across `packages/**/*.ts` (excluding node_modules) yields one hit, `test/workflow-v2-osfence-integration.test.ts`. No production module constructs the pipeline.
- The two production daemon files that *do* import `daemon-osfence.js` (`daemon-supervisor.ts`, `daemon-mode.ts`) gate every hook behind `this.osfenceMode.enabled` / `this.osfenceWorkerMode.enabled`, both resolved from the unavailable capability, so they are dormant. `wireGeneration()` falls back to the V1 `randomUUID()` `this.generation` because `this.osfence` is only ever assigned inside the mode-guarded `elevateEndpointPossession()`.
- No global authority flip: no code path enables the capability or mutates a global authority register. Confirmed behaviorally by the V1 no-regression A/B (check 6).

### (2) Copied Slice 3 lanes byte-identical to `/tmp/prime-agent-workflow-v2-s3` — PASS
SHA-256 identical for every lane and its test, and for the modified `rlm-ledger.ts` + `rlm-ledger.test.ts`:

| file | sha256 (16) | identical |
|---|---|---|
| src/core/workflow-v2-retained-executor.ts | 9cbc77fbda5d4e4c | yes |
| src/core/workflow-v2-retained-profile.ts | fa7d7bb16de0ad1a | yes |
| src/core/workflow-v2-settlement.ts | 5120926021fd4d55 | yes |
| src/core/workflow-v2-terminal-capture.ts | 9d5d90ecc8de60c5 | yes |
| src/modes/daemon/rlm-ledger.ts | 2a5d42cb09ecfed5 | yes |
| test/rlm-ledger.test.ts | cf9d0fa06ed98ad6 | yes |
| test/rlm-composite-admission.test.ts | 9fb19c68c239ef0e | yes |
| test/workflow-v2-retained-executor.test.ts | e3921649bad0e45c | yes |
| test/workflow-v2-retained-profile.test.ts | 2c33704305f82fdc | yes |
| test/workflow-v2-terminal-settlement.test.ts | 3301d867105202e1 | yes |

### (3) One shared fence interface, no divergent vocabulary — PASS
- `osf-control-db.ts` is the single authority for the closed control-DB vocabulary: `OsfControlDb`, `RouteTuple`, `WorkerRouteRow`, `AcquisitionRecord`, `WriterFenceAssertion`, `EndpointIdentity`, all `FenceResult*` variants, and `OsfenceAuthoritySignal`.
- `daemon-osfence.ts` and `supervisor-control-db.ts` both `import ... from "./osf-control-db.js"` and re-export; neither redeclares the vocabulary. The only structural addition is `supervisor-control-db.ts`'s `WorkerRouteRow extends OsfWorkerRouteRow` (documented richer implementation row), not a competing definition.
- Minor (non-blocking) note: an error class named `SupervisorWriterFenceError` exists in both `daemon-osfence.ts` and `supervisor-control-db.ts`. Both carry `code = "supervisor_generation_stale"`, so `.code`-branching callers are consistent; the shared *type* vocabulary is single-authority. Consider consolidating the error class in a follow-up.

### (4) `assertWriterFence` gates BOTH boundaries; Form-1 CAS + Form-2 possession — PASS
- The integration builds **one** Form-2 fence closure (`assertVoidFence`) and injects the *same* closure into both `RlmCompositeAdmissionLedger` (`assertWriterFence`) and `Slice3RetainedDispatchExecutor` (`assertFence`).
- Admit-append boundary: `RlmCompositeAdmissionLedger.admitUnlocked` calls `assertWriterFence?.()` at entry **and** again immediately before `eventLog.appendSync([record], { durable: true })`, then verifies by re-read — a preflight cannot authorize a later write.
- Provider-effect boundary: `Slice3RetainedDispatchExecutor.dispatchOnce` calls `assertFence()` after `claimAndCommitDispatching` and before `commitProviderEntered` + the single `physicalCall`.
- Form-2 possession: `assertWriterFence`/`evaluateWriterFence` require `possession.isHeld()` (death-released listening fd), unchanged endpoint `(dev,ino)`, intact advisory lease, and `observedGeneration === adoptedGeneration`.
- Form-1 CAS: `supervisor-control-db.ts` runs every mutation in `BEGIN IMMEDIATE` with `PRAGMA synchronous = FULL` + WAL, integrity_check, `application_id`/`user_version` gate, and a `WHERE generation = prior` compare-and-advance; it cites only `control_db_generation_cas` as its authority signal.

### (5) Full workflow + OS-fence suite — PASS, 215/215
`npx vitest --run` over the 11 workflow-v2 / osfence / slice3 / control-db / rlm files:
```
Test Files  11 passed (11)
     Tests  215 passed (215)
```
Breakdown: daemon-osfence 32, daemon-osfence-v1-noregression 9, supervisor-control-db 30, workflow-v2-osfence-integration 15, rlm-composite-admission 16, workflow-v2-retained-executor 8, workflow-v2-retained-profile 23, workflow-v2-terminal-settlement 40, rlm-ledger 29, workflow-v2-capability 5, workflow-v2-wire 8.

### (6) Independent acceptance mutants — PASS (16/16, re-derived, not the shipped tests)
A throwaway independent test file (imported the real modules, then deleted) exercised:
- **paused-predecessor GENERATION_STALE zero-effect**: `assertWriterFence` throws `SupervisorWriterFenceError` when `observedGeneration(6) > adoptedGeneration(5)`; `fenceGenerationStale(...).zeroEffect === true`; passes only when observed === adopted.
- **dual-owner**: a changed endpoint inode throws "endpoint identity changed"; a relinquished possession throws "endpoint possession lost"; takeover acquisition must be `generation === priorGeneration + 1` (prior+2 rejected "monotonicity"); an acquisition citing a demoted authority signal is rejected.
- **process-kill single-dispatch**: a fresh executor after a simulated restart that reads `hasDispatching:true` returns `already_dispatched` with **0** physical calls and **0** provider-entered; happy path dispatches exactly once.
- **fence-loss -> execution_unknown**: `assertFence` throwing after the dispatching claim yields `fence_lost_after_dispatch` with 0 physical calls and 0 provider-entered.
- **tools-none frozen**: `WORKFLOW_V2_OSFENCE_BOUND_PROFILE` is `Object.isFrozen`, `tools:"none"`, `maxTurns:1`; mutation attempt throws and the value is unchanged.
- **route stale/mismatch/unavailable**: `resolveRoute` returns `ROUTE_STALE` (revision mismatch), `OWNER_MISMATCH` (foreign/absent worker), `OWNER_UNAVAILABLE` (recovering), each `zeroEffect === true`; exact match returns `OK`.

### (7) V1 no-regression (git-stash A/B on tracked files) — PASS
- With the change applied: `test/daemon-mode.test.ts` -> `4 failed | 202 passed`.
- After `git stash push` of the 5 tracked files (reverting daemon-mode/-supervisor/-worker-protocol/rlm-ledger[.test] to base; the untracked osfence files remain but base `daemon-mode.ts` does not import them, confirmed `grep -c daemon-osfence = 0`): **identical** `4 failed | 202 passed`.
- The 4 failing tests are the **same names** in both states: "discovers a non-resident child left running in the persisted registry", "reports failed passive children as errors without creating child runtimes", "validates a requested passive child name before hydration", "rehydrates a legacy passive subagent at depth one". They are therefore **pre-existing on the clean base**, not introduced by the OS-fence change. Stash restored; tree verified back to the integrated state.

### (8) O4 static authority gate + lint
- **O4 gate — PASS**: `daemon-osfence-v1-noregression.test.ts` covers **both** declared authority modules (`daemon-osfence.ts` and the shared `osf-control-db.ts`): no `proper-lockfile`, no PID/liveness/mtime/TTL/timer predicate, no demoted authority literal, and the `OsfenceAuthoritySignal` union is declared exactly once in the shared module and only re-exported by the consumer. Independently confirmed that `supervisor-control-db.ts` (the Layer-B CAS implementation, not in the gate's file list) also cites only `control_db_generation_cas` and uses none of the banned predicates.
- **biome — PASS**: `biome check --error-on-warnings` over all 21 changed/new files: "Checked 21 files. No fixes applied.", exit 0.
- **tsgo — FAIL (the blocker)**: see below.

---

## BLOCKER: repo-wide typecheck regresses from a clean base

`npm run check` runs `biome check --write --error-on-warnings . && tsgo --noEmit && ...`, and `tsgo --noEmit` uses the root `tsconfig.json`, whose `include` is `["packages/*/src/**/*", "packages/*/test/**/*", ...]` — i.e. it typechecks tests.

- Clean base `2e4a11cbe` (tracked changes stashed, untracked osfence files moved aside): `tsgo --noEmit` -> **0 errors**.
- Integrated tree: `tsgo --noEmit` -> **3 errors**, all in copied Slice 3 **test** files:
  ```
  test/rlm-composite-admission.test.ts(289,22): TS2339 Property 'record' does not exist on type 'AdmitOutcome'.
  test/rlm-composite-admission.test.ts(289,46): TS2339 Property 'record' does not exist on type 'AdmitOutcome'.
  test/workflow-v2-terminal-settlement.test.ts(436,43): TS2352 Conversion of type 'AtomicSettlementCommit' to 'Record<string, unknown>' may be a mistake ...
  ```
- Confirmed genuine (not a tsgo quirk): stock `tsc --noEmit` reports the identical 3 errors.
- These 3 errors are **byte-for-byte inherited** from `/tmp/prime-agent-workflow-v2-s3` (same test files; `tsgo` there also reports the same 3), so they predate the OS-fence integration work — but they still land in this tree and fail its gate.
- Scope is narrow: production source typechecks clean (`tsgo -p tsconfig.build.json` -> 0 errors); runtime is unaffected (vitest uses esbuild, 215/215); biome is clean.

**Why this blocks a terminal PASS:** the reviewed tree cannot pass `npm run check` / `prepublishOnly`, and it regresses a gate the base satisfied. Check (7) of the review charter ("tsgo+biome clean") is not met.

### Minimal, test-only fix (no production change, no invariant change)
1. `test/rlm-composite-admission.test.ts` ~L285-289: narrow before dereferencing `.record`, e.g. after `expect(first.disposition).toBe("admitted")` use `if (first.disposition !== "admitted" && first.disposition !== "replayed") throw ...` (the current `conflict||rejected` guard does not narrow the object to the record-bearing member under discriminated-union rules), or assert `first.record` via an explicit typed helper.
2. `test/workflow-v2-terminal-settlement.test.ts` L436: cast through `unknown` first — `c as unknown as Record<string, unknown>` — as the TS2352 message directs.

After those two edits, re-run `tsgo --noEmit` (expect 0) and the 215-test suite (expect 215) to clear the block. Because the same defect exists in the s3 sources, fix it there too so byte-identity is preserved on the next copy.

---

## Environment
- vitest 4.1.10; tsgo 7.0.0-dev.20260120.1; tsc (bundled); biome 2.5.5
- Full 215/215 suite and 16/16 independent mutants executed in this worktree.

*No reviewed files were modified. Scratch test and stash operations were reverted; `git status` returns to the integrated working-tree state (5 modified tracked + 16 untracked new).*

---
Report SHA-256 (of this file prior to this trailer): f5ac3be3b9edb38007226dadf791dd17bf472704a581a736a1a552d820a1382a
