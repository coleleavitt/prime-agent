Make the RAVO deep gate fail closed and keep its rejections.

`parseJudgeVerdict` returned `"pass"` for any value it did not recognise, so a
judge that omitted the `verdict` field silently authorized the candidate. Only
an explicit pass token now passes; everything else abstains, which propagates
through `authorizeAssistedRavo` as a conservative miss.

A RAVO rejection is now appended to the global refinement history instead of
living only in the session JSONL, so the gate's negative decisions survive the
session. `isRollbackableRefinement` keeps them out of the rollback target set,
since a rejection applied no edits.

`loadHarnessState` now emits a `harness.state.corrupt` warning when the state
file is unreadable or is not an object, instead of silently degrading to an
empty store and letting the next save overwrite it.
