# Workflow V1 host final independent rereview

**Date:** 2026-09-14
**Review type:** independent terminal code, security, package-provenance, and evidence rereview
**Candidate:** `f73c7d295753e839fc78881dd697b2841001edb5`
**Candidate tree:** `6b848b33fd68ff1db51ea1d69b1fb1bda4c35032`
**Normative authority:** `pi-plugin-workflow` commit `e47fd2a80b45cd4b6a9be8c05adad85ded187c1b`
**Verdict:** **BLOCK**

## Terminal decision

Four prior blocker clusters are closed. The schema/package/authority cluster is not. The supplied acceptance is internally consistent and all 18 recorded gates passed, but gate 11 builds and tests a verifier-modified package rather than the normal built/package artifact. The machine manifest also still does not bind the normative authority commit. These are release-gate provenance failures under normative `docs/WORKFLOW-V1.md` §12. A printed PASS cannot override them.

Do not push, install, canary, or enable native Workflow V1 from this candidate. Per instruction, this BLOCK review is not committed.

## Prior blocker closure matrix

| Prior blocker | Disposition | Evidence |
|---|---|---|
| B1: mutating formatter / no post-gate identity | **CLOSED** | `scripts/accept-workflow-v1-host-clean.mjs` creates a closed Git ledger, checks tree plus full tracked/untracked status before and after every gate and after the final gate, and invokes non-mutating `biome check --error-on-warnings`. Independent Biome rerun passed: 1,091 files, no fixes. |
| B2: parent semantic-edge/session effects | **CLOSED** | `agent-session.ts` passes `preflight.streamSimple`, not `this.agent.streamFn`. `run-workflow-agent.ts` rebuilds the provider option bag from only `apiKey`, linked `signal`, explicit resolved auth headers, and forced `maxRetries: 0`. Integration tripwires cover parent stream, semantic-edge recorder, session messages, filesystem, and inherited request/idempotency headers. A mutation restoring `this.agent.streamFn` was caught. |
| B3: arbitrary header accepted as credential | **CLOSED** | Preflight requires an API key, a configured and nonempty header from the explicit credential-bearing header allowlist, or literal `authHeader: false`. User-Agent and arbitrary metadata headers fail before provider I/O. A mutation restoring “any nonempty header” was caught by two tests. |
| B4: installed hostile-cwd check absent | **CLOSED only for CLI/Python execution** | Gate 11 packs and installs outside the checkout, invokes the installed CLI and Python runtime from a hostile cwd under a clean environment, and checks CLI/runtime byte identity. The package-provenance limitation below prevents full closure of B5. |
| B5: both schemas / packaged copies / authority binding | **OPEN — BLOCKING** | Both repository fixture bytes equal both normative public and skill copies and their digest mutants are caught. However, the normal build does not create installed schema copies; gate 11 injects them into a private staging tree immediately before packing. Also, `acceptance.json` contains no normative commit field. |

## Blocking findings

### F1 — installed schema evidence is produced by a bespoke verifier-mutated package

`scripts/verify-workflow-v1-packed-install.mjs:44-48` copies `packages/coding-agent` into a private `packageStage`, then creates `dist/prime-agent-runtime/schemas` and copies both schema fixtures into it. It packs that modified staging directory at line 61.

The normal package pipeline does not create those files. `packages/coding-agent/package.json` builds `dist/prime-agent-runtime` by copying `../../prime-agent-runtime`; that source tree has no `schemas/` directory. The exact candidate's built `packages/coding-agent/dist/prime-agent-runtime` likewise has no schema directory. A repository search found no normal build/package rule for these schema copies outside the verifier.

Therefore the verifier proves that copies it manufactured can survive its own pack/install sequence. It does not prove byte equality with every schema copy in the actual normal package artifact. This fails normative §12's installed-artifact and packaged-copy requirement and leaves prior B5 open.

**Required closure:** put both schema copies into the repository-owned normal build/package inputs; make the normal build produce them; have the hostile-cwd gate pack the unmodified normal artifact; verify repository, built, tarball, and installed copies independently; and add mutation tests that alter or omit each actual copy.

### F2 — the machine manifest does not bind the normative authority

The supplied `acceptance.json` top-level keys are only `format`, `integrity`, `candidate`, `verdict`, and `gates`. It correctly binds the candidate commit/tree, but it contains neither `e47fd2a80b45cd4b6a9be8c05adad85ded187c1b` nor a normative authority object. The commit exists only in comments and prose. No committed gate checks a clean export of that authority commit or records its identity in the machine report.

Both fixture streams independently compare byte-for-byte equal to `pi-plugin-workflow` `docs/api/` and packaged skill copies at exact `e47fd2a`, with SHA-256 values `79913bb20831758935910a0a49b2ddaf40299c283f876b081cd75f21791f3b27` and `08ade62e424d7dad199ca87b1a2da8eb57da71657a497f6793862fa1d73e1f6a`. Those content facts are valid, but they do not cryptographically bind the normative commit in the acceptance manifest as required by the prior terminal remediation.

**Required closure:** record the full normative commit and both authoritative source paths/digests in `acceptance.json`; make a mutation-sensitive gate verify those fields against a clean export of the exact authority commit.

## Supplied evidence audit

- Manifest SHA-256: `b0905b896ed8efc09c8dc720f709a00f0d64e83fff2b551e45edde51135ad909`; sidecar matches.
- Log-ledger SHA-256: `72d174917c60bf17c0f9edeee670c65f8ce55743e239de9821b14c62ff75571c`.
- Candidate/tree match exact Git objects.
- 18/18 gate entries report exit zero; all 18 log hashes and both nested artifact hashes recomputed correctly.
- Nested Python manifest SHA-256: `85d3c59b16d28b7aa035d6c9b4c2c07b29b20ca8fbaa9c364a924b84413833fe`.
- Python result is **339/339**, not 338/338: 280 non-`test_bash` tests plus 59 individually isolated `test_bash` methods across 60 fresh processes, zero retries, zero allowlist exemptions. This is a count correction and not a blocker.
- Required focused evidence passes for exact protocol/route, `maxTurns=1`, one physical request, `maxRetries=0`, immutable tools-none rejection before dispatch, result/usage closure, cancellation/drain races, `execution_unknown`, and old-host `CAPABILITY_UNAVAILABLE`.
- History from `c4d9a5cbe` to candidate contains the ephemeral V1 implementation plus remediation commits. Searches found no SQLite/store/receipt/recovery implementation. Literal V2 strings occur only in rejection/mutation tests and schema descriptions.

## Independent reruns and mutations

| Check | Result |
|---|---|
| Non-mutating root Biome | PASS — 1,091 files, no fixes |
| Workflow schema/runner/host/race focused Vitest | PASS — 54/54 |
| Agent tool-policy focused Vitest | PASS — 2/2 |
| Packed hostile-cwd verifier | PASS as written, but does not cure F1 |
| Controller schema-byte mutation | REJECTED — digest test failed |
| Arbitrary-header auth mutation | REJECTED — both negative cases failed |
| Parent wrapped-stream mutation | REJECTED — isolation tripwire failed |
| Tool policy `reject` → `execute` mutation | REJECTED — construction invariant failed |

## Final disposition

**BLOCK.** The runtime isolation, authentication, one-turn/tool rejection, cancellation, and clean-tree gates are materially improved and independently mutation-sensitive. Release remains blocked until the actual normal package owns the schema copies and the machine acceptance binds the exact normative authority commit. After those implementation/evidence changes, run a new clean acceptance and obtain another independent terminal rereview.
