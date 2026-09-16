# Workflow V1 schema-authority terminal review — `270517fe8`

## Verdict

**PASS**

Reviewed candidate `270517fe823a237d5638ba5f2c6156913b1fd186` independently from the detached exact-commit checkout at `/tmp/prime-agent-workflow-v1-host-v4`.

This review did not push or install the candidate.

## Scope result

The candidate is limited to the intended Workflow V1 public schema-authority update:

- the normative producer is pinned to `coleleavitt/pi-plugin-workflow` commit `1fa3ffc7372815e0f358e0e1854bba2396d40325`;
- its tree is `f9b12a14af53a1da5f00e2070c528e7b9923b478`;
- the Workflow V1 schema blob is `8bf5fd80b9e6a1525ec4c73e5f1a9ca4149e6830` with SHA-256 `db3aa583523d4374e5ef455b1ada26744e0cd862d43384b86210e4c39bbf8663`;
- the native-host schema blob remains `18eef21d5126ade6989016591a4e2fb0b8907b94` with SHA-256 `08ade62e424d7dad199ca87b1a2da8eb57da71657a497f6793862fa1d73e1f6a`;
- the public schema change removes the obsolete top-level request `args` property;
- no file under `packages/agent/src`, `packages/coding-agent/src`, or `prime-agent-runtime/src` changed in the candidate;
- neither committed copy of `workflow-native-host-v1.schema.json` changed from the candidate parent.

The candidate changes nine authority/copy/test/verifier files only. The prior authority bundle is replaced by `scripts/fixtures/pi-plugin-workflow-1fa3ffc.bundle`.

## Independent authority and bundle checks

I verified the bundle directly rather than trusting only the authority JSON:

- bundle SHA-256: `b9bc393a60f3cb8439e65d9fffc00fc1c4c3916c3963c1c74b4f2c3e9c324dad`;
- advertised ref: `refs/authority/workflow-v1-1fa3ffc` at the exact producer commit;
- `git bundle verify` reports complete history;
- fetching the authority ref into a new empty repository reproduces the recorded commit, tree, both schema blob IDs, and both schema SHA-256 values;
- `node scripts/verify-workflow-v1-normative-authority.mjs` passes in the exact checkout and rejects four authority mutants.

The bundle is therefore sufficient for offline authority verification and is internally consistent with the committed authority record.

## Packaging and mutation evidence

The hostile packed-install gate proves identical schema bytes at all four required phases:

1. repository source;
2. normal built distribution;
3. generated npm tarball;
4. installed package.

For both schemas, each phase rejects both alteration and omission. That is **16/16 rejected mutations**. The tarball enumerates and extracts exactly the two expected schema members.

## Acceptance evidence integrity

Evidence directory:

`/home/cole/.prime/agent/session-artifacts/01a03411-8ff2-75b9-8da1-96727d5e9061/workflow-v1-host-acceptance-270517fe8`

Independent digest verification passed for:

- `acceptance.json` and its sidecar;
- the top-level `logs.sha256` manifest: **19/19 gate logs**;
- `python-isolated/python-isolated.json` and its sidecar;
- the isolated Python `logs.sha256` manifest: the runtime log plus **59/59 bash case logs**.

The acceptance record binds candidate commit `270517fe823a237d5638ba5f2c6156913b1fd186` to tree `3382a3207568b4608b5cb345dd241f66ae88ca7d`. It records archive identity checks before and after every gate, forbids untracked files, and reports all 19 gates with exit code zero and no signal.

## Full acceptance result

The clean archive acceptance is **PASS**. Material results include:

- locked install and normal build pass;
- repository host verifier passes: 62 TypeScript tests plus 20 isolated Python tests;
- schema/wire, object-route/race, and agent-policy focused suites pass;
- isolated Python runtime matrix passes;
- hostile tarball/install verification passes;
- recursion and native-bridge isolated suites pass;
- ENG-4600 and ENG-4606 regression suites pass;
- full agent-core suite passes;
- root typecheck passes;
- Biome checks 1,091 files with no fixes or warnings.

## Terminal conclusion

No blocking defect was found. The authority identity, offline provenance bundle, schema copies, packaging chain, mutation coverage, evidence manifests, native-host schema stability, and unchanged host implementation satisfy the requested terminal review criteria.
