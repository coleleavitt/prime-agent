# Executable stall-hardening verification result

Command:

```bash
RYE_ROOT=/home/cole/RustProjects/active/rye scripts/verify-stall-hardening.sh
```

Exit code: `0`

Verified:

- all required Prime Agent and Rye hardening commits exist;
- all implementation and CI files exist;
- no modified or staged tracked files existed at verification start;
- workflow YAML parsed and every action reference was an immutable 40-character commit;
- Prime `npm run check` passed;
- Prime focused hardening suite passed: 413 tests, 2 skipped;
- Prime Python runtime passed: 328 tests;
- Rye format, Clippy, 1,024-test nextest, doctest, docs, and llvm-cov passed;
- Rye line coverage was 87.67%;
- historical EventBus reproduction timed out as expected and the current regression passed.

Final script result:

```text
PASS all executable owned-work gates
BLOCKED only: Rye has no committed pre-existing baseline for clean-checkout CI
```

The blocker is intentional and fail-closed: the script does not claim Rye clean-checkout integrity. The pre-existing Rye project baseline remains untracked and was not swept into these commits.
