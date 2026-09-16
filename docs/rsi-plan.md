# prime-agent: what would actually make it self-improving

*Decision document. All numbers below are re-measured on this machine today (110 session harness states under `~/.prime/agent/session-artifacts/`, 242 sessions, global state at `~/.prime/agent/harness/harness_state.json`), not taken from the briefs. Where a brief's number disagrees with mine, I say so and pick a side.*

---

## 1. The one-paragraph answer

prime-agent is not failing to learn; it is failing to *keep*, *check*, and *show*. Three mechanisms fix that and everything else is optional. **The referee** is the one that matters most, because every gate in the system currently reduces to the proposal grading itself — `run-service.ts:386` returns `{status: "pass", ...}` unconditionally on the deep adapter, `refinement/ravo.ts:452-458` does the same on the assisted path, and both opponent predicates (`run-service.ts:336`, `authority.ts:194`) pass a failure criterion iff the proposal's own `addressedFingerprints` names it; until a subprocess re-runs a recorded counter-example, there is no signal in the system written by anything other than the thing being judged. **A durable global ledger** matters second, but not for the reason the brief claims: it is not that the recurrence trigger never fires (it fired for 31 of 96 fingerprints), it is that **0 of 49 champions has ever carried an `observedRecurrence`** — the outcome label `ravoObserveChampion` computes is thrown away with the session, so the referee would have nowhere to write `FlawUpheld`. **Prompt selection** matters third and is nearly free: `formatHarnessStateForPrompt` (`refinement.ts:459`) sorts by `[path,title,id].localeCompare` and slices at `DEFAULT_OVERVIEW_ENTRY_LIMIT = 6` (`refinement.ts:35`), so of 27 global memories the six the model sees are one Rust reference, one preference, and four August `ers-rs` work-journal notes, while `projects/obscura/baseline` — today's SSL_CERT_DIR root cause, written 2026-09-15 — ranks **27 of 27** and has never once reached the model. **Toolforge** is the only mechanism that creates capability rather than reshuffling pointers and I would ship it, but fourth, reusing the referee's runner. The other four — the ask primitive, the correction ledger, the learning index, and the distillation/RL pipeline — are all defensible engineering and **none of them is on the critical path**; the distillation one I would cut outright, for reasons in §4.

---

## 2. Dependency order

Ruthless version. Five real edges, three independent tracks.

**Real dependencies:**

1. **`harness.py:save()` must land before anything writes global RAVO state** — because `prime-agent-runtime/src/rlm/harness.py:285-301` serializes exactly `{schema, entries, refinements}` through a bare truncating `open("w")`, so any kernel-side `rlm.harness.upsert` silently deletes `ravo` and `failures` from whatever state dir `RLM_HARNESS_STATE_DIR` points at (`agent-session.ts:10385` points it at the *local* dir). Measured: of the 110 session harness states, the **5** where the kernel wrote (`source:"agent"`) are **5/5 missing both `ravo` and `failures`**; of the 105 where it did not, 41 have `ravo` and 39 have `failures`. Fisher exact one-tailed p ≈ 0.02. n=5 is small, but the mechanism is in the source, so the statistic is corroboration, not the argument.
2. **The global ledger must land before the referee is worth running** — the referee's verdict is only interesting if the fingerprint it refutes survives to the next session. Today `HarnessState.failures` is documented in-source as "Per-session failure ledger (local scope only)" (`refinement.ts:74`) and `_flushFailureLedger` (`agent-session.ts:8469`) writes only to `getLocalHarnessStateDir(...)`.
3. **The referee must land before trust/eviction** — Rule A ("debit an entry whose falsifiable claim was contradicted") can fault exactly 1 of the 27 current global entries; the brief concedes this. Without an executable oracle, "trust" is a heuristic wearing a measurement's clothes.
4. **The referee must land before the correction ledger** — both briefs independently reach this conclusion ("I would fix the hardcoded pass first", "shipping this on top of a gate that cannot fail means real corrections get turned into confidently wrong policy"). They are right. Concur.
5. **Something must produce an outcome label before the learning index has anything to roll up** — the index's own exit test needs `refinement.committed` lines carrying an `addressed` list, and `RefinementResult` (`refinement.ts:105`) has no such field today.

**Genuinely independent — ship whenever, blocks nothing:**

- **Prompt selection ordering.** ~40 lines in one function. Depends on nothing. Highest measured payoff per line in the whole list.
- **The ask primitive.** Nine of ten measured defects survive it untouched; its own brief says so. It is a good product feature on its own schedule. It is *not* a precondition for the correction ledger either — see §4.
- **The resolution index** (the 200-line half of the distillation brief). Reads `failure-ledger.ts` outputs that already exist. No dependency.
- **Toolforge.** Nominally independent. I would still sequence it after the referee so it reuses `referee-runner.ts` rather than growing a second variant of `probeOne` (`skill-dry-run.ts:148`).

**Non-dependency worth stating explicitly:** the two-phase-commit, epoch-fenced, hash-chained `HarnessStore` is **not** a precondition for anything. `saveHarnessState` (`refinement.ts:372`) already writes to `${statePath}.${pid}.${uuid}.tmp` and `renameSync`s it — the TypeScript writer is atomic. The two zero-byte `harness_state.json` files on this machine cannot have come from it. They came from the Python side. Fixing the Python writer plus adding a lockfile is one day; the CAS store is four weeks solving a problem the TS side does not have.

---

## 3. The staged plan

| # | Milestone | Effort | Files | Exit test | Why it matters |
|---|---|---|---|---|---|
| 0 | **Kernel writer stops destroying harness state.** `HarnessState.save()` preserves unknown top-level keys and writes temp+`os.replace`; `loadHarnessState` emits a `harness.state.corrupt` log line instead of silently returning `emptyHarnessState()`. | **S** (1 day) | `prime-agent-runtime/src/rlm/harness.py` (`save` 285-301, `_load` round-trip), `prime-agent-runtime/test/test_harness.py`, `packages/coding-agent/src/core/refinement/refinement.ts:300-316` | Seed a `harness_state.json` containing non-empty `ravo` and `failures`; call `rlm.harness.upsert("memory", ...)` from the kernel against that dir; reload and assert both keys are byte-identical. Separately, SIGKILL between open and close and assert the file still parses. **Both fail on HEAD** — the first drops both keys (5/5 sessions in the corpus), the second yields the zero-byte file. | This is live, in-session data loss on the exact keys every other mechanism needs. Every milestone below writes into a file the kernel can currently truncate. |
| 1 | **Prompt selection: recency + supersession, not `localeCompare`.** Replace the sort key in `formatHarnessStateForPrompt` with `(supersededAtSamePath desc, updated_at desc, path)`. No trust, no scoring, no new fields. | **S** (1 day) | `packages/coding-agent/src/core/refinement/refinement.ts:459-500`, `packages/coding-agent/src/core/system-prompt.ts:109,148`, new `test/harness-prompt-selection.test.ts` | Seed the real 27-entry global state; assert `projects/obscura/baseline` (updated 2026-09-15) appears in the rendered prompt and that at most one of the four `projects/ers-rs/new-entities` August entries does. **Fails on HEAD**: obscura ranks 27/27, and three of the four ers-rs duplicates occupy slots 3-6. | 22 of 27 global memories are invisible and the selection was never designed. 3 of 6 slots change hands for ~40 lines. This is a retrieval fix, and it is the cheapest real improvement available. |
| 2 | **Global failure ledger + RAVO lineage, behind `PRIME_AGENT_GLOBAL_LEDGER=1`.** `_flushFailureLedger` writes to `getGlobalHarnessStateDir()` under `proper-lockfile`; `refinementBaselineView` narrowed to `{schema, entries, ravo}` so a concurrent flush cannot invalidate an in-flight `/refine` binding. | **M** (1 week) | `packages/coding-agent/src/core/agent-session.ts:8458-8489,9147`, `packages/coding-agent/src/core/ravo/failure-ledger.ts`, `packages/coding-agent/src/core/ravo/authority.ts:164`, `packages/coding-agent/src/core/refinement/refinement.ts:284-288,372`, `test/global-failure-ledger.test.ts` | Replay two archived sessions that each observe fingerprint `f` exactly once. With the flag off, neither session's `recurringFailures()` returns `f` and no refine is queued. With it on, session B's `recurringFailures()` returns `f` at count 2 and `_queueFailureTriggeredRefine` fires. Second assertion: two processes flushing concurrently both survive; neither loses the other's record. **Both fail on HEAD.** | Not for the reason the brief gives — see §4's correction. It buys **+10 newly-triggering fingerprints**, which is modest. What it actually buys is that `RavoProvisionalWindow` can finally observe a regression in a *later* session, which is the only place a referee verdict can be recorded. |
| 3 | **The referee.** Failure records carry a replay case captured with a self-check at capture time; a failure opponent passes iff a subprocess re-executes that case and the recorded exception does not recur. The deep adapter returns `fail` when the judge says fail instead of hardcoding pass. | **M→L** (2 weeks) | `packages/coding-agent/src/core/ravo/referee.ts` (new), `referee-runner.ts` (new), `failure-ledger.ts` (schema v2 + capture), `run-service.ts:336,386`, `authority.ts:194`, `refinement/ravo.ts:452-458`, `test/ravo-referee.test.ts` | Given a ledger whose recurring fingerprint `f` carries case `import paramiko`, a proposal setting `addressedFingerprints:["f"]` that changes nothing relevant must yield `certificate.committed === false` with `failure:f` in `missed`; the byte-identical proposal against a scratch root where `import paramiko` succeeds must yield `committed === true`. **On `perf/session-catalog-resume` both commit.** | This is the milestone. It is the first and only point where the system can be *wrong about itself and find out*. Everything upstream is plumbing for it; everything downstream is unjustified without it. |
| 4 | **Resolution index.** Join each `fingerprintId` to the cell that subsequently resolved it; on a fingerprint hit, append "you hit this before; this fixed it: `<cell>`" to the ipython tool result. Reuses `normalizeFailureMessage`, `fingerprintFailure`, `parsePythonTraceback` unchanged. | **S** (2-3 days) | `packages/coding-agent/src/core/distill/resolution-index.ts` (new, ~200 lines), `packages/coding-agent/src/core/tools/ipython.ts`, `packages/coding-agent/src/core/ravo/failure-ledger.ts`, `test/resolution-index.test.ts` | Replay a session where fingerprint `f` occurs, is resolved by cell *k*, then recurs. On the recurrence, the tool result must contain the text of cell *k*. **Fails on HEAD**: no join exists and the result is unannotated. | 74 of 96 fingerprint sources are `ipython`; the top two classes are `AttributeError: ? object has no attribute ?` (36 occ / 10 sessions) and `module ? has no attribute ?` (22 occ / 12 sessions). Those are retrieval failures. A lookup fixes them. Nothing else on this list touches them, and it is the only surviving piece of the distillation brief. |
| 5 | **Toolforge.** `rlm.toolforge.publish(name, source, doc, exit_test)` → materialize skill package → double-run gate (must fail without, pass with) → editable install → hot-bind into live `__main__` → global skill entry. Exit-test execution delegates to `referee-runner.ts`. | **M** (1.5 weeks) | `prime-agent-runtime/src/rlm/toolforge.py`, `rlm/skill.py`, `rlm/__init__.py`, `packages/coding-agent/src/core/toolforge/{publish,ledger}.ts`, `src/core/tools/ipython.ts`, `src/core/kernel/bootstrap.ts`, `src/core/refinement/skill-dry-run.ts`, `test/toolforge-publish.test.ts` | Session A: `await rlm.toolforge.publish("slugify", …)`, then in the *same cell* `slugify.run("A B") == "a-b"`. Session B, new session id, no kernel snapshot: same expression is true with no human step. **Both fail on HEAD** — and a proposal to create new code is guaranteed to fail its own screen, because `checkReference` (`skill-dry-run.ts:102`) and `_validate_python_skill_reference` (`harness.py:129`) both require an already-importable module. | 2 skills ever recorded, both session-local; `~/.prime/agent/skills/` contains exactly one entry and it is a symlink to an external repo. The agent has never written a durable line of capability. `skill` is the only one of four output kinds where "did it work" has a machine-checkable answer, and `RefinementEdit` (`refinement.ts:78-89`) has no field that can carry source. |
| 6 | **Trust + eviction**, now that Rule A can actually fire. Asymmetric ±, dormancy below threshold, trust folded into the M1 selection score as one term among recency and supersession. | **M** (1 week) | `packages/coding-agent/src/core/refinement/harness-trust.ts`, `refinement.ts:868-883` (spread `before`, stop enumerating fields), `refinement.ts:300-316` (loader must not drop the new top-level key — `trustWindows` already exists orphaned in 2 session files), `prime-agent-runtime/src/rlm/harness.py`, `test/harness-trust.test.ts` | Seed the 27-entry global state plus one skill entry committed by a refinement claiming fingerprint `F`; replay a session where the referee upholds `F` after the entry goes live; assert trust 50→35→20, the entry leaves the rendered prompt at 20, and remains readable via CRUD. **Fails on HEAD** (no trust field, no referee verdict to debit against). | Only worth doing after M3. Before it, the only rule with a causal basis can fault 1 of 27 entries and the rest is supersession heuristics — which M1 already delivers without the ceremony. |
| 7 | **Learning index + `prime-agent learning`.** Day-partitioned roll-up keyed by fingerprint; treated/untreated cohort comparison; refuses to print a p-value below minimum n. | **M** (1 week) | `packages/coding-agent/src/core/learning-index.ts`, `src/cli/learning-command.ts`, `src/cli/learning-chart.ts`, `src/cli/command-registry.ts`, `src/core/refinement/ravo.ts` (emit `refinement.committed` with `addressed`), `src/config.ts:583,598,625`, `test/learning-index.test.ts` | Against a seeded synthetic `agent.jsonl` (8 sessions × 1000 turns, 20 fingerprints, 10 named in a `refinement.committed` at turn 4000 and dropping to 1/1000 while the rest hold 8/1000): `buildLearningReport()` reports `cohorts.treated.medianDelta < cohorts.untreated.medianDelta - 0.5, pValue < 0.05`; with the `addressed` list moved onto the untreated ids, `pValue >= 0.05`. | Last, deliberately. Its own brief is honest that the first output will read "insufficient evidence" for weeks. Building the measurement before there is a durable outcome label to measure means shipping a dashboard of zeros. After M2+M3 it has something to say. |

Ordering note: rows 0, 1 and 4 are all independently shippable inside week one and together are less code than row 3 alone.

---

## 4. What to cut

**Cut outright — the distillation / RL pipeline (Cell Replay Recorder + Verifiable Task Mint).** The brief's own saturation argument is correct and fatal, and the numbers make it worse than stated. 1,332 fail→repair pairs is already marginal for RL on an 8B model and nothing for a frontier model, but composition kills it before volume does: **74 of 96 fingerprint sources are `ipython`** — the model's own cell code, not library code — and the two largest classes are `AttributeError: ? object has no attribute ?` (36 occurrences across 10 sessions) and `module ? has no attribute ?` (22 across 12). Those are "I guessed an API that does not exist." Gradient descent does not fix a missing doc; a lookup does, and that lookup is **row 4, 200 lines, three days**. The pairs are also by construction ones this same policy already fixed on the next cell, so group advantages collapse and you pay real money for ~0 gradient. Add the reward-hacking hole the brief identifies — "does not raise `E`" is satisfied by `try/except: pass` — and the storage cost (`session-artifacts` is already 11 GB, 7.7 GB of dill) and the privacy exposure (the first blob the designer opened contained an operational note about fabricating tracker data; `assertNoSecrets` at `archive.ts:316` screens key names, and cell *values* are not key-shaped). Keep the resolution index. Delete the rest of the mechanism, including `assessTrainingReadiness()` — a gate nobody will enforce is worse than no gate, because it launders the decision.

**Cut from the self-improvement roadmap, keep as product — the durable `ask` primitive.** I verified the gap: `grep -rn "askUser|requestInput|awaitAnswer|ask_user|prompt_user"` across `packages/` and `prime-agent-runtime/src` returns zero hits, and `createExtensionUIContext` fails open with no attached client. The transcript evidence (`01a03514…jsonl`: the agent asks "Say the word and I'll run scripts/deploy.sh" and the very next line is a `Keep working toward:` re-prompt) is real and it is genuinely annoying. But the brief's own accounting is that nine of ten measured defects survive it. The residency risk is also the largest single hazard on the list — `isSessionActive` (`agent-session.ts:7129`) is true whenever `isStreaming` is, so a blocked ask pins the worker against `canEvictWorker`/`canPassivateSession` (`session-action-store.ts:391-422`), holds `waitForIdle()` open forever, and blocks `beginMutation`; the designer explicitly says "I did not audit them all." That is an L with an unaudited blast radius buying a signal nothing downstream consumes yet. Ship it because you want it, on its own branch, not as step 1 of learning.

**Defer and reshape — the correction ledger.** This is the closest call, and the argument *for* it is the best single data point in the corpus: the one refinement of 409 that ever reached global state is `preferences/version-control` — "Commit repository work incrementally and push only when requested" — `source:"refine"`, rationale "The user directly asked…". The only thing this system has ever durably learned is a user correction. But read that record carefully: it worked because **the user explicitly asked**, through a mechanism that already exists. The proposal is a 900-line LLM detector plus an afternoon of hand-labelling 151 candidates, to automate a path that has a 100% success rate when invoked manually and exactly one instance of being invoked. The cheap version is not a detector: it is making "remember this" a one-keystroke global refine with the user's own words quoted verbatim, and *then* measuring how often it gets used. If that rate is high, build the detector with real prior data. If it is low, the detector would have been inferring a preference the user did not have. Also: detection precision is not lesson precision, the behaviour anchor is 36-of-49 dominated by `ipython` so fingerprints will collide, and `ravoExtendOpponents` never lowers a weight — an unaddressable correction fingerprint permanently tightens the gate for everything.

**Cut the architecture, keep the goal — `HarnessStore`.** Two-phase commit, monotonic epochs, hash-chained CAS, a 4-kill-point × 20-repetition SIGKILL matrix, and a `PROTOCOL_VERSION == 3` kernel renegotiation, budgeted at 4-6 weeks. `saveHarnessState` is already temp-write + `renameSync`. The corruption is entirely Python-side and is row 0, one day. Concurrency is real but it is `proper-lockfile` around one file, not an epoch-fenced log. The brief's own leverage section concedes the outcome: "this converts 409→1 into 409→N, and does nothing to make N good," and warns of "~49 newly-global entries pushed through a gate that cannot fail." Take that warning literally: build the small durable ledger (row 2, behind a flag), then the referee, then decide whether the store is needed. It almost certainly is not.

---

## 5. The first pull request

**Title:** `fix(runtime): kernel harness writes no longer truncate or drop RAVO/failure state`

**Scope:** one Python method, one Python loader, one TypeScript log line, two tests. Half a day of code, half a day of tests.

**Files:**

- `prime-agent-runtime/src/rlm/harness.py`
  - `HarnessState.save()` (lines 285-301). Two defects, both one-liners. First, the payload dict is built literally as `{"schema", "entries", "refinements"}`, so `ravo` and `failures` are dropped on every kernel-side write. Fix: carry an `_extra: dict` captured during `_load` for every top-level key the dataclass does not model, and splat it into the payload. Second, the write is `with self.file_path.open("w")` — truncate-then-write, non-atomic. Fix: write to `Path(f"{p}.{os.getpid()}.{uuid4().hex}.tmp")`, `os.replace()` onto the target, mirroring `saveHarnessState` (`refinement.ts:372-389`) exactly. Preserve the existing mode.
  - `_load` / `load()`: populate `_extra` with unmodelled top-level keys so the round-trip is closed rather than merely widened by two names.
- `prime-agent-runtime/test/test_harness.py` — the proving test, below.
- `packages/coding-agent/src/core/refinement/refinement.ts:300-316` — `loadHarnessState` currently returns `emptyHarnessState()` on parse failure with a comment explaining why it must not throw. That reasoning is correct; keep it. Add one `log.warn("harness.state.corrupt", { path: statePath, bytes })` before each of the three early returns, so a launder becomes a grep-able event instead of "nothing was ever learned."

**The test that proves it** (`test_harness.py::test_kernel_write_preserves_unmodelled_state`):

```python
seed = {"schema": 1,
        "entries": {"prompt": {}, "memory": {}, "skill": {}, "subagent": {}},
        "refinements": [],
        "ravo": {"champions": [{"id": "c1", "claimedFingerprints": ["abc123"]}]},
        "failures": {"schema": 1, "failures": {"abc123": {"count": 2}},
                     "lastScannedEntryIndex": 7}}
path.write_text(json.dumps(seed))
state = get_harness_state(state_dir=d, global_=False)
state.upsert("memory", title="t", content="c")      # the kernel path
after = json.loads(path.read_text())
assert after["ravo"] == seed["ravo"]                 # fails on HEAD: KeyError
assert after["failures"] == seed["failures"]         # fails on HEAD: KeyError
assert after["entries"]["memory"]                    # passes on HEAD
```

Plus `test_kernel_write_is_atomic`: fork a child that calls `save()` with a `SIGKILL` delivered from a `faulthandler`-style timer mid-write, loop 20×, assert `json.loads(path.read_text())` never raises and the file is never zero bytes. **On HEAD this fails**, and it explains both zero-byte `harness_state.json` files currently on disk (`01a0884b-eb47-719e-8411-2ed469bcafdb`, `01a08945-7b12-76bc-b422-74d661fc630d`).

**How you know within a week:**

1. **Immediately, on the archive.** Re-run the contingency scan. Today: sessions where the kernel wrote harness state are 5/5 missing both `ravo` and `failures`; sessions where it did not are 41/105 with `ravo`, 39/105 with `failures` (Fisher one-tailed p ≈ 0.02). Any *new* session with a kernel harness write and a non-empty `ravo` key falsifies the old behaviour in one observation. Because only ~4.5% of sessions currently take that path, do not wait for it passively — add the assertion to CI and drive one deliberately.
2. **Day 7 counters.** `grep -c harness.state.corrupt ~/.prime/agent/logs/agent.jsonl*`. Expected: 0. Any non-zero is a second corruption source the Python fix did not cover, and you want to know that before row 2 starts writing the global file.
3. **Zero-byte census.** `find ~/.prime/agent -name harness_state.json -size 0 | wc -l` must not grow past 2. It is the cheapest possible regression alarm and costs nothing to keep running.

If all three hold at day 7, row 2 is safe to start. If (1) never gets a sample, that itself is the finding: the kernel harness path is rarer than the 5/110 suggests and row 2 should widen the flush trigger before promoting anything global.

---

## 6. How you will know it is working

Baseline, re-measured today. Where my scan disagrees with the figures in the brief I use mine and flag it.

| Metric | Measured baseline (today) | Week 1 | Month 1 | Month 3 |
|---|---|---|---|---|
| Global promotion rate | **409 refinement events → 1 global entry** (0.24%). Global file: 27 memories, 0 prompt, 0 skill, 0 subagent, 26/27 `source:"agent"`. | Unchanged — nothing in rows 0/1/4 promotes anything. Deliberate. If this moves in week 1 something is wrong. | ≥3 global entries, **every one of them carrying a referee verdict**. Raw count is a vanity metric; the certificate is the metric. | 15-30 global, with ≥1 *rejected* commit whose rejection reason is `failure:<fp>` and not `screen`. **A month with zero referee rejections means the referee is vacuously passing — treat it as a defect, not a success.** |
| Fingerprints reaching the recurrence trigger | 96 distinct; **31 reached ≥2 inside one session**; 65 never did; of those, **10 would cross threshold if the ledger were global** (35 occurrences); 33 appear in >1 session. **The brief's "25 of 40 recurring / 193 occurrences / threshold can essentially never fire" is wrong** — it fires for 31 of 96. Sell row 2 as the outcome-label fix, not the trigger fix. | n/a | +10 newly-triggering fingerprints once row 2 is flagged on. That is the honest ceiling; do not budget for more. | Of the 33 cross-session fingerprints, ≥10 have a non-empty `addressedByProposalIds` that *persists* — i.e. the system stops re-deriving from zero what it already tried. |
| `observedRecurrence` on committed champions | **0 of 49** (6 of 49 carry `claimedFingerprints`). This is the single most damning number in the corpus: the outcome label the reducer is architected to produce has never once been written. | 0 | **≥1.** This is the pass/fail for rows 2+3 combined. One non-null `observedRecurrence` is worth more than twenty new global memories. | ≥10, with the uphold/refute split visible. If refutes are 0/10, `ravoObserveChampion` is not actually observing. |
| Durable skills | **2 ever**, both session-local, 0 global. `~/.prime/agent/skills/` contains one entry: a symlink to `/home/cole/WebstormProjects/active/pi-plugin-workflow/skills/workflow-v1`. | 2 | 2 (toolforge is row 5). | **≥5 agent-authored packages**, each with an `_exit_test.py` that still passes in the eviction sweep. First-ever non-zero on `entries.skill` in the global file. |
| Prompt slot utilisation | 6 of 27 global memories render; the 6 are chosen by `localeCompare`; `projects/obscura/baseline` (2026-09-15, today's root cause) ranks **27/27**; 3 paths hold 22 of the 27 entries. | **Day 1, hard gate:** obscura renders, ≤1 of the four August `ers-rs/new-entities` duplicates renders. Binary, checkable in one test. | Median `updated_at` of rendered global memories within 14 days of now (currently ~August). | Median age <7 days, and ≥1 rendered slot occupied by a toolforge skill. |
| Corrections | 1 of 409 refinements is correction-derived, and it was invoked manually. The brief's 7.0% is the *candidate detection* rate over transcripts, not the learning rate — do not conflate them. | n/a | Count manual `refine.run(global=True)` invocations after an explicit user correction. **If that count is <3/month, do not build the detector** — you would be automating a path the user does not use. | Decide build-or-drop on that number, not on the 151-candidate labelling exercise. |
| Log retention for measurement | 12.7h dense (`agent.jsonl` + 4 rotations at `AGENT_LOG_MAX_BYTES = 20 MB`, `DEFAULT_LOG_RETENTION = 5`, `config.ts:583` / `logging.ts:13`). | n/a | Row 7 not started. Raise retention to 20 generations now — it is a one-line config change and it costs 400 MB, and without it row 7's first window is already gone. | ≥30 sealed days in the index with `parseErrors` surfaced, not swallowed. Note `redactLocalLog` (`config.ts:598`) rewrites inside serialized lines, so `head -1 agent.jsonl` on this machine is not parseable today — fix that before trusting any roll-up. |

**The single number I would put on the wall:** `observedRecurrence`, currently 0 of 49. It is the only metric in this table that cannot be gamed by the thing being measured, it is already computed by `ravoObserveChampion` (`reducer.ts:281`) and thrown away, and rows 0-3 exist precisely to let it be written down. If it is still 0 at month 1, the plan failed and no amount of new global entries redeems it.
---

## 7. Amendment, 2026-09-15 — found during implementation

**M2 landed PARTIAL and the reason invalidates a claim in section 3.**

The ledger half works and its exit test was demonstrated in both directions (neutered
`globalFailureLedgerEnabled()` → the cross-session recurrence assertion fails; restored → passes, with the
source verified byte-identical against a backup). What does *not* follow is M2's stated payoff — "that
`RavoProvisionalWindow` can finally observe a regression in a LATER session, which is the only place a
referee verdict can be written."

It cannot, for a reason section 3 did not consider:

> `ProvisionalRegression.committedTurn` / `untilTurn` are in **per-session assistant-turn numbering**.
> `_failureLedgerTurn()` (`agent-session.ts:~8508`) counts assistant messages on the current branch and
> therefore restarts at 0 in every session. `findProvisionalRegressions` gates on
> `if (turn < committedTurn || turn > untilTurn) continue`. A champion committed at session A's turn 40 with a
> 20-turn window is asking about turns 40..60; session B is at turn 2. **The window can never match except by
> coincidence** (a champion committed at turn 0).

Two further gaps point the same way: `_observeFailuresAtTurnBoundary` reads only the *local* ravo state
(`_loadLocalHarnessRavoState`), and `_flushFailureLedger` writes `recordProvisionalRegressions` only into
local state — so a champion committed by `/refine --global` is never examined and never annotated.

### New row

| # | Milestone | Effort | Files | Exit test | Why it matters |
|---|---|---|---|---|---|
| 2b | **Durable provisional windows.** Re-key `ProvisionalRegression.committedTurn`/`untilTurn` off per-session assistant-turn numbers onto a durable ordinal (a global monotonic observation counter, or wall-clock with an explicit skew budget). Make `_observeFailuresAtTurnBoundary` read, and `_flushFailureLedger` write, the **global** ravo lineage when the flag is on. | **M** | `packages/coding-agent/src/core/agent-session.ts` (`_failureLedgerTurn`, `_observeFailuresAtTurnBoundary`, `_flushFailureLedger`), `src/core/ravo/reducer.ts` (`findProvisionalRegressions`, `recordProvisionalRegressions`), `test/provisional-window-cross-session.test.ts` | A champion committed in session A with a 20-unit provisional window must have `observedRecurrence` set when its claimed fingerprint recurs in session B. Fails today for every champion except one committed at turn 0. | Without it the referee (M3) has nowhere to write its verdict across sessions, which is the entire point of M2 and M3 together. This is the missing link between "0 of 49 champions ever carried an observedRecurrence" and a system that can be wrong about itself and find out. |

**Ordering:** 2b lands between 2 and 3, or immediately after 3. It is a precondition for the referee's
verdict to survive a session boundary — which is the only way the loop closes.

## 8. Amendment, 2026-09-16 — measured, then re-scoped

Three analyses over the real corpora settled a question the plan had been assuming: **whether to build a
durable fact/answer store.** The answer is no, and the number is not close.

- **Oracle ceiling on prime-agent itself.** Reconstructing 47,392 `ipython` cells with their outputs from the
  248 transcripts in `~/.prime/agent/sessions/` (157 sessions with cells, 122,369,440 chars of tool output),
  a *perfect, omniscient, zero-error* cross-session output cache recovers **786 cells (1.66%) and 0.52% of
  output** — roughly 1,008 tokens per session. On the span log, 87.0% of bash commands are first-ever and only
  56.5% of the 209 cross-session repeats are even `(exit_code, bytes)`-identical.
- **Same answer from opencode**, independently: of 398,522 fact-establishing calls, exactly **2,651 (0.67%)**
  are executions an omniscient store could have removed. No fact class has a median TTL longer than one root
  task (median root task 1.1 h; pooled P(answer unchanged) is 39% past 1 h, 27% past 24 h).
- **Widening the scope makes it strictly worse.** Replaying every fact call against caches of varying scope
  and TTL: root-tree/1h → global/inf buys **+4.58pp hits of which 94.7% are wrong**. A machine-wide store is
  not a bigger in-task cache, it is a different and worse artifact.

**So the thesis — "recheck and verify never persists" — is right about persistence and wrong about what is
worth persisting.** What is actually large is orientation, not repetition: **42.4% of cells are
orientation-only** (read/list/grep, no mutation) and carry **58.3% of all tool output**, and **72.5% of
real-repo sessions start in a directory a prior session already used** (median gap 6.3 h). The agent
re-derives workspace shape every session because nothing durable records what the workspace looked like when
it last knew.

The design consequence, and the new milestone: **store the invalidation, not the answer.**

### New row

| # | Milestone | Effort | Files | Exit test | Why it matters |
|---|---|---|---|---|---|
| 8 | **Workspace Recall.** A per-repo durable *mark* (`~/.prime/agent/recall/<repo-key>.json`: HEAD, a blake2b-128 digest per dirty path, a tracked-tree digest, ≤8 build claims), written on `turn_end` under `proper-lockfile`. On the first `ipython` tool result of a session, every digest is recomputed against the live filesystem and a ≤2 KB `<workspace_recall>` block is appended to the result: changed paths listed, unchanged paths counted, uncheckable paths named `unverifiable`, and any prior build claim rendered `EXPIRED` with its reason. The store holds digests and never content; a mark is invalidated by recomputation, not by a TTL or a scope rule. | **M** (1 week) | `src/core/recall/{mark,witness,render}.ts` (new), `src/core/extensions/builtin/workspace-recall.ts` (new), `src/core/extensions/index.ts`, `src/core/agent-session-services.ts:172`, `src/config.ts` (`getRecallDir`), `docs/observability.md` (`recall.witness`), `test/workspace-recall.test.ts` (new). No `agent-session.ts` change — `afterToolCall` already dispatches `tool_result`, which `runner.ts:948` lets a handler rewrite. | Temp git repo, 3 tracked files, temp `PRIME_AGENT_CODING_AGENT_DIR`; mark written in session A with claim `npx tsgo --noEmit` exit 0. (a) Session B, nothing changed → `unchanged since the mark`, 3 tracked files, claim `CURRENT`. (b) Mutate `a.txt` **outside** the agent → session C lists it under `Changed`, counts 2 unchanged, claim `EXPIRED`. (c) `chmod 000 b.txt` → session D puts it under `Unverifiable` and **not** under unchanged. (d) Remove `.git` → empty block. All four fail on HEAD. | It is the first durable artifact here whose stored object is an *invalidation* rather than an answer, so its worst case is today's behaviour rather than a confident lie — and (c) is the first gate in this repo tested in the direction where it must refuse. |

**Explicitly excluded by measurement, not difficulty:** file contents (0.52% ceiling), grep results, build/test
*answers* (they may expire, never be served), process liveness, and anything outside the git toplevel. The
mechanism is a designed no-op for the 25.5% of sessions that start in `/tmp`.

**The honest caveat.** Rendering is not effect: the 118 commit-and-push requests across 95 sessions happened
*with* the relevant memory rendered every turn. Week-1 instrumentation must count how often a session that
received a block still issues a workspace-shape cell in its first 10 cells. If that does not fall, the
mechanism failed.
