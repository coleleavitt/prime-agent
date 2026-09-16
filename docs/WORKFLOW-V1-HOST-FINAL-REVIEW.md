# Workflow V1 host final independent rereview

**Date:** 2026-09-14
**Review type:** terminal independent code, security, package-provenance, and evidence rereview
**Implementation candidate:** `e01de61d218170796590941a6d49bdd5385bf288`
**Candidate tree:** `786c634944749482ec85a78b7d807035b8a1c1c9`
**Normative authority:** `pi-plugin-workflow` commit `e47fd2a80b45cd4b6a9be8c05adad85ded187c1b`
**Verdict:** **PASS**

## Terminal decision

All prior blocker clusters B1–B5 are closed for the exact implementation candidate above. The supplied clean-copy acceptance has 19/19 zero-exit gates. I independently rehashed its manifest, sidecar, complete log ledger, every gate log, and both nested Python artifacts. All hashes match. Focused security and protocol reruns passed, the packed hostile-cwd verifier passed, the offline normative bundle verified, and no Workflow V2 implementation was found.

This commit adds review evidence only on top of the implementation candidate. It does not authorize or perform a push, install, canary, or production enablement.

## Prior blocker closure matrix

| Prior blocker | Disposition | Evidence |
|---|---|---|
| B1: mutating formatter / no post-gate identity | **CLOSED** | Acceptance runs from `git archive`, creates a closed temporary Git ledger, permits no untracked files, and checks the exact tree and full status before and after every gate and after the final gate. Root Biome is non-mutating and passed over 1,091 files with no fixes. |
| B2: parent semantic-edge/session effects | **CLOSED** | The workflow path uses the preflight stream, rebuilds the provider option bag, links only the cancellation signal, forces `maxRetries: 0`, and leaves parent semantic-edge persistence/session/filesystem effects outside the child path. Focused object/route/race tests passed 49/49; the combined focused rerun passed 62/62. |
| B3: arbitrary header accepted as credential | **CLOSED** | Preflight accepts an API key, an explicit allowlisted credential-bearing header, or literal `authHeader: false`; arbitrary metadata does not satisfy authentication. The focused workflow/security suite passed. |
| B4: installed hostile-cwd validation absent | **CLOSED** | The generated CLI tarball is installed outside the checkout under a clean environment, then its CLI and Python runtime are invoked from a hostile cwd. CLI/runtime identity checks pass and the installed Python entry validates the request before returning `CAPABILITY_UNAVAILABLE` without a native host. |
| B5: schema copies / package phases / authority binding | **CLOSED** | Both schema hashes are verified at repository source, normal built dist, directly enumerated/extracted generated tgz, and installed package. Sixteen real alter/omit mutants—two schemas × four phases × two mutations—are rejected by the same phase validators. No schema is injected by the verifier. The acceptance manifest binds the offline authority bundle, exact commit/tree, source paths, blobs, and hashes. |

## Package and authority ground truth

The packed-install verifier stages the already normally built coding-agent package. It rewrites only internal workspace dependency URLs to local packed tarballs. It does not create or copy schema files into staging. Before installation, it:

1. enumerates the generated npm `.tgz` with `tar -tzf`;
2. requires exactly one member for each schema;
3. extracts each member directly from that same archive with `tar -xOf`;
4. hashes those bytes against the normative digests;
5. constructs alter and omit mutants and proves rejection;
6. verifies the original tarball digest did not change during mutation work.

The two schema SHA-256 values are:

- `workflow-v1.schema.json`: `79913bb20831758935910a0a49b2ddaf40299c283f876b081cd75f21791f3b27`
- `workflow-native-host-v1.schema.json`: `08ade62e424d7dad199ca87b1a2da8eb57da71657a497f6793862fa1d73e1f6a`

The repository-owned offline bundle has SHA-256 `b61e746cdef154108a96239994454ab8bbf023270d72e866e0198fbcd6f28f2f` and verifies as a complete Git bundle containing `refs/authority/workflow-v1-e47fd2a` at exact commit `e47fd2a80b45cd4b6a9be8c05adad85ded187c1b`, tree `95114d5cf2e03c3de25d0f667eb2be7ca7ebb5c1`. The authority verifier also validates exact schema blob IDs and rejects four authority mutants.

## Acceptance evidence audit

- External evidence directory: `/home/cole/.prime/agent/session-artifacts/01a03411-8ff2-75b9-8da1-96727d5e9061/workflow-v1-host-acceptance-e01de61d2`
- Manifest SHA-256: `c0a7790eaf0a1c8a0695eba2c348589d44d5e18c5c6e9dac4dafd2b36fc7a340`; sidecar matches.
- Log-ledger SHA-256: `7d728f43a4f4bd23202bd6cc7aa3614412f7e447b4496c73af176e8b982e7e00`.
- Candidate commit and tree match the exact Git objects.
- 19/19 gates report exit zero; all declared gate-log and nested-artifact hashes independently recompute correctly.
- Python runtime: 280 non-Bash tests plus 59 individually isolated Bash methods across 60 fresh processes, 339/339 total, with no retries or allowlist exemptions.
- Agent full suite: 72/72.
- Focused independent reruns: Workflow 62/62; agent tool policy 2/2.
- Forbidden source scan, root typecheck, and non-mutating root check passed.
- A repository search found no Workflow V2 implementation markers.

Committed normalized evidence is intentionally compact and contains results and digest inventories rather than duplicated large logs:

- `docs/evidence/workflow-v1-host-acceptance-e01de61d2/acceptance.normalized.json`
- `docs/evidence/workflow-v1-host-acceptance-e01de61d2/log-digests.json`

## Final disposition

**PASS.** The exact implementation candidate `e01de61d218170796590941a6d49bdd5385bf288` satisfies this terminal independent review against normative `e47fd2a80b45cd4b6a9be8c05adad85ded187c1b`. This evidence commit changes no implementation. Push, installation, canary, and enablement remain separate actions and were not performed.
