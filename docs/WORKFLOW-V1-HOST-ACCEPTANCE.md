# Workflow V1 host terminal acceptance review

**Date:** 2026-09-14
**Review type:** independent code, security, and clean-artifact terminal review
**Candidate:** `886e9a8721a880a3f599256d61ef1db05703232c`
**Candidate tree:** `94ab667582d4528aa90e93166610ae6ca9f5605e`
**Normative authority:** `pi-plugin-workflow` commit `e47fd2a80b45cd4b6a9be8c05adad85ded187c1b`
**Verdict:** **BLOCK**

## Scope and method

I reviewed all 29 files changed by the candidate and the supplied external acceptance evidence. I independently reran the repository acceptance command into `/tmp/workflow-v1-independent-acceptance-886e9a872`, checked every supplied manifest and log hash, inspected the closed wire/runtime/host route, and ran the non-mutating formatter check against the exact candidate. I made no implementation changes.

The supplied manifest correctly names the candidate commit and tree and records 18/18 commands with exit code zero. Its nested Python evidence records 338 tests: 279 combined non-`test_bash` tests and 59 isolated `test_bash` methods. Those results are real but do not overcome the blockers below.

## Blocking findings

### B1 — acceptance gate 18 mutates the archived candidate and creates a false PASS

`scripts/accept-workflow-v1-host-clean.mjs` runs `npm run check`. The root script invokes `biome check --write --error-on-warnings .`. The committed `logs/18-root-check.log` reports `Fixed 1 file`, but the runner does not compare the checkout bytes/tree after the gate with the archived candidate.

A separate non-mutating `npm exec -- biome check --error-on-warnings .` against exact HEAD exits 1 at `packages/coding-agent/test/interactive-mode-startup.test.ts:584`. Therefore the committed candidate bytes fail formatting while `acceptance.json` claims PASS. This violates the normative stop rule that a printed PASS cannot override a failed machine gate.

### B2 — the Workflow child reaches the parent session's persistent semantic-edge wrapper

`AgentSession` replaces its provider stream with `wrapStreamFnWithSemanticEdges(...)` at `packages/coding-agent/src/core/agent-session.ts:1587`. The Workflow route passes that wrapped function at `agent-session.ts:10458`. `runWorkflowAgent` invokes it at `run-workflow-agent.ts:185`. The wrapper calls `recorder.startTurnRequest(...)` at `semantic-edges.ts:542`; the recorder is constructed with a ledger path derived from the parent session artifacts.

Thus each Workflow provider turn can mutate the parent semantic-edge ledger. This contradicts normative §4's runtime boundary (no parent session, ledger, persistence, or host-file effect; provider I/O is the only child-originated external effect). The current tests assert only that parent messages and RLM/session routes are unchanged. They do not tripwire semantic-edge persistence. The runner must receive the unwrapped authenticated provider transport, not the parent session observability wrapper.

### B3 — arbitrary non-credential headers bypass the authentication preflight

`preflightWorkflowModel` treats any nonempty resolved header map as proof of authentication (`model-registry.ts:1234-1246`). Provider validation now also admits `headers` without an API key and without explicit `authHeader: false` (`model-registry.ts:726-730`, `1769-1773`). A remote provider configured only with a routing or telemetry header such as `User-Agent` therefore passes preflight and can start a physical turn. An arbitrary header is neither a credential nor an explicit no-auth policy. This violates the exact authenticated selector and absent-auth fail-closed boundary. Require a resolved API/OAuth credential, an explicitly typed credential-bearing header policy, or literal `authHeader: false`; add a negative arbitrary-header test.

### B4 — clean acceptance does not validate installed bytes from a hostile working directory

Normative §12 requires installed-byte verification from a hostile working directory. The outer runner exports a clean archive but runs all gates with the extracted checkout as `cwd`. `verify-python-runtime-isolated.mjs` also runs through the source project using `uv run --project prime-agent-runtime`. It does not pack/install the Prime artifact and invoke it from an unrelated directory. The evidence therefore proves clean source behavior, not the required installed boundary.

### B5 — schema-copy acceptance is incomplete

Normative §12 requires byte equality, digest verification, and mutations for both public schemas and every packaged/installed copy. The candidate pins only `scripts/fixtures/workflow-native-host-v1.schema.json`. It has no controller `workflow-v1.schema.json` copy/equality/digest/mutant check and no installed-copy verification. The supplied evidence also does not cryptographically bind the normative authority commit; it appears only in comments/prose.

## Security and invariant disposition

| Area | Disposition |
|---|---|
| Exact route/protocol, closed request/reply, maxTurns=1 | Evidence passes |
| One physical request and `maxRetries: 0` | Focused evidence passes |
| Immutable `toolCallPolicy: reject` before dispatcher | Focused evidence passes |
| Bounded result bytes/SHA-256 and usage finality | Focused evidence passes |
| Shielded exact-ID cancel/drain and unknown classification | Focused evidence passes |
| Explicit `authHeader: false` | The explicit no-auth policy itself is acceptable for administrator-configured local/anonymous endpoints, but preflight is fail-open because any unrelated nonempty header is treated as a credential. BLOCK per B3. |
| No V2/durable host leakage | No SQLite/store/controller/profile/receipt/recovery implementation entered the changed runtime files |
| Ordinary Agent behavior | Default `toolCallPolicy` remains `execute`; agent-core full suite reports 72/72. However the candidate formatting failure still blocks repository acceptance. |
| Environment/exemptions | Outer environment is a positive allowlist plus fixed CI/TZ/test/color values; Python evidence declares allowlist `[]`, retry 0, and forbids `PRIME_AGENT_`. No active baseline exemption list was found. |

## Evidence hashes

- Supplied `acceptance.json`: `15e9255711125a1abed97dd1daf71ad25681308e8a8a2935657db0f39a65713b`
- Supplied `logs.sha256`: `8d8b175e27237d4ca2f7d80815a4c07e6db79c414b53bbea809d6282a084c8de`
- Supplied manifest/log ledger verification: PASS; all declared hashes matched.
- Independent acceptance: 18/18 recorded zero exits, but it reproduced the same mutating false-PASS defect.
- Independent `acceptance.json`: `3c3cf913ef4c2681a50f8ac9cbf604f0e1ecfb7b04268116d3b3d000e8b02904`
- Independent `logs.sha256`: `76bec37871bb9f903da1a5de6062404b79d4037a60d958b472d356c6e6411187`

The independent hashes differ because the logs contain run-specific normalized data; both ledgers validate internally. Neither run supplies a post-gate candidate-tree identity check.

## Disposition

**BLOCK.** Do not commit, push, install, or enable this candidate. Required remediation is: fix the committed formatting defect; make acceptance non-mutating or verify post-gate archive/tree identity; pass an unwrapped authenticated provider transport that cannot touch semantic-edge persistence; make auth preflight reject arbitrary non-credential headers unless `authHeader: false` is explicit; add the missing installed hostile-cwd gate; validate both schemas and every installed/package copy; and bind the normative commit plus evidence hashes in the machine report. Then obtain a fresh independent terminal review.
