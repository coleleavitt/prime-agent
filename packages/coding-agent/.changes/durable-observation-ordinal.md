Measure trust windows and provisional regressions in a durable observation ordinal.

`committedTurn`/`untilTurn` were per-session assistant-turn counts, which restart
at 0 every session. A window opened at turn 40 in one session asked about turns
40..60 while the next session was at turn 2, so it could never settle except for
a champion committed at turn 0 — and `settleHarnessTrust` had no caller at all,
so `trust` was a field on disk, a filter that could never fire, and a prompt line
that could never print.

`observationOrdinal` totals the global failure ledger's occurrences. It is
monotone by construction (`count` only increments and nothing evicts), already
durable and already loaded, and its units are what the window means: the machine
observed this many more failures and none of them was the one you claimed. Wall
clock was rejected because a window would expire while the operator was away and
credit a clean window that nothing tested.

`/refine` now settles the windows its ordinal can decide before opening the new
claim, and passes `trustClaim` for the fingerprints the gate accepted. With the
global ledger flag off the ordinal never advances, so no trust moves — the
fail-closed direction.

Kernel-authored harness entries are stamped `source: "kernel"` instead of
`"agent"`, so the share of the global store that passed through no gate is
visible in the data rather than inferred. Rows with no recorded source are left
as `"agent"`.
