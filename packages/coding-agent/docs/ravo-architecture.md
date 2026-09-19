# RAVO and Prime Agent architecture (Mermaid)

Machine-readable maps for agents working on or inside the RAVO loop. Both diagrams are plain Mermaid so a model can read the edges directly; the Rocq references point at `Ravo.v` sections that prove the corresponding property.

## AVO agentic variation loop

```mermaid
%% AVO agentic variation loop (Puget et al., arXiv:2603.24517) as RAVO instantiates it.
%% Notation from ~/RocqProjects/ravo/Ravo.v: Agent(P, K, f) with lineage P, knowledge K, evaluator f.
flowchart LR
  subgraph IN["Inputs to AVO"]
    P["Solution lineage P<br/>candidates + scores<br/>(Rocq: Lineage = list (A * nat))"]
    K["Knowledge base K<br/>docs + code + failure ledger<br/>(Rocq S11: Ledger)"]
    f["Evaluator f<br/>fast screen + deep outcome + opponents<br/>(Rocq: fast, deep, clearsEW)"]
  end

  subgraph LOOP["AVO agentic variation loop — Agent(P, K, f) is a general-purpose coding agent with Tools · Memory · Reasoning"]
    direction TB
    S1["1 Inspect context<br/>lineage, feedback, references"]
    S2["2 Plan<br/>choose the next change"]
    S3["3 Implement<br/>edit the candidate"]
    S4["4 Evaluate<br/>invoke scoring function f"]
    S5["5 Diagnose and repair<br/>adapt from failed attempts"]
    S1 --> S2 --> S3 --> S4
    S4 -->|"rejected"| S5
    S5 --> S1
  end

  SUP["Supervisor<br/>watches stagnation, budget, deadline<br/>(RavoControllerOptions.supervisor)"]
  CAND["Candidate<br/>solution and score (x, deep x)"]
  GATE{"commit gate<br/>tau <= fast x<br/>bestScore P <= deep x<br/>missedWeight <= eps"}
  UPD["Updated lineage<br/>accepted candidate appended<br/>(Rocq: commitGate, commitE)"]
  REJ(("reject"))

  P --> S1
  K --> S5
  f --> S4
  SUP -.->|"conditional intervention"| S2
  S4 --> CAND --> GATE
  GATE -->|"commit"| UPD
  GATE -->|"fail"| REJ
  REJ -.->|"repair and retry"| S5
  UPD -.->|"weakness pressure: double weight of missed opponents<br/>(Rocq S4/S6: pressureW)"| f

  classDef input fill:#f4f6f8,stroke:#6b7c8a,color:#1c2b36;
  classDef loop fill:#e8f2f6,stroke:#2f6f8f,color:#1c2b36;
  classDef ok fill:#e6f4ea,stroke:#3a8f5a,color:#173a24;
  classDef bad fill:#fdecec,stroke:#d9534f,color:#5a1a1a;
  class P,K,f input;
  class S1,S2,S3,S4,S5 loop;
  class UPD ok;
  class REJ bad;
```

## Prime Agent: three planes

```mermaid
%% Prime Agent architecture as of fix/forkserver-probe-hardening (2026-09-09).
%% Three planes: RLM execution (what runs), Continual Harness (what is learned), RAVO (how learning is gated).
flowchart TB
  subgraph RLM["RLM execution plane — packages/coding-agent/src/core"]
    direction LR
    USER["User / CLI / TUI<br/>modes/interactive, daemon client"] --> AS["AgentSession<br/>core/agent-session.ts<br/>turn loop, events, host bridge"]
    AS --> MODEL["Model call<br/>@earendil-works/pi-ai providers"]
    AS --> KERNEL["IPython kernel (RLM)<br/>persistent Python REPL<br/>bash(), edit, skills, mcp"]
    KERNEL --> SKILLS["Python skills<br/>skills/<name>/src<br/>refine · ravo · agent_message · ..."]
    SKILLS -->|"host_request(name, payload)"| AS
    KERNEL -->|"await rlm(task)"| CHILD["Child AgentSessions<br/>run-agent.ts / retained workers"]
    CHILD --> AS
    AS --> DAEMON["Daemon supervisor + workers<br/>modes/daemon<br/>protocol 7, schema 29, capabilities"]
    DAEMON --> VIEW["Agents View<br/>modes/agents-view<br/>rows · usage · ravo status line"]
    AS --> SESS["Session JSONL + two-tier catalog index<br/>session-manager.ts · session-catalog-index.ts"]
  end

  subgraph HARNESS["Continual Harness plane — core/refinement"]
    direction LR
    HS["HarnessState (harness_state.json)<br/>memories · skills · subagent specs · prompt notes<br/>ravo: ReducerState · failures: FailureLedger"]
    LEDGER["Failure ledger<br/>core/ravo/failure-ledger.ts<br/>fingerprint(kind, source, class, msg) → count<br/>per session, and global unless PRIME_AGENT_GLOBAL_LEDGER=0"]
    TRIG["Refine triggers<br/>turn_interval · compact · recurrence(actionable, count>=2) · regression"]
    PROP["refine.run / _planRefine<br/>LLM proposes RefinementProposal edits"]
    HS --> PROP
    LEDGER --> TRIG --> PROP
  end

  subgraph RAVO["RAVO plane — core/ravo"]
    direction LR
    FAST["fast screen<br/>structural validity − skill dry-run failures<br/>(refinement/skill-dry-run.ts)"]
    DEEP["deep score<br/>LLM judge 0-100, or ARC-AGI-3 levels completed<br/>(arc-agi-evaluator.ts)"]
    OPP["opponents<br/>evidence · scope · minimality · contracts · novelty<br/>failure:&lt;fp&gt; per recurring error · referee:&lt;fp&gt; per adjudicated claim<br/>arc:no-crash · arc:all-levels (dormant passes outside an ARC run)"]
    STEP["ravoStep (reducer.ts)<br/>tau ≤ fast ∧ best ≤ deep ∧ missedWeight ≤ eps<br/>commit → pressure doubles missed weights"]
    AUTH["authorizeAssistedRavo (authority.ts)<br/>digest-binds proposal + baseline<br/>provisional champion, 20-observation window on its clock"]
    CTRL["runRavoController (controller.ts)<br/>inspect → plan → implement → evaluate → gate → diagnose/repair<br/>stops: accepted · round_limit · repair_limit · deadline · budget · cancelled"]
    SVC["RavoRunService (run-service.ts)<br/>/ravo &lt;task&gt; · ravo.run()<br/>--arc-repo DIR --arc-game ID"]
    ARCH["RavoArchive<br/>hash-chained NDJSON + CAS"]
    FAST --> STEP
    DEEP --> STEP
    OPP --> STEP
    STEP --> AUTH
    SVC --> CTRL --> STEP
    STEP --> ARCH
  end

  AS -->|"tool errors · tracebacks · provider errors<br/>at every assistant turn boundary"| LEDGER
  PROP -->|"proposal"| FAST
  AUTH -->|"commit: apply edits, save state"| HS
  HS -->|"memories · skills · notes into context<br/>digest at cold boundaries · notice after apply"| AS
  LEDGER -->|"recurring fingerprints become opponents"| OPP
  AS -->|"recurrence inside the provisional window = measured fault<br/>→ regression refine in the champion's scope (gated repair)"| TRIG
  SKILLS -->|"ravo.run → host bridge ravo.run"| SVC
  CTRL -->|"children via runAgent / retained worker"| CHILD
  CTRL -->|"RavoProgressEvent → ravo_run_update"| DAEMON
  DEEP -.->|"ARC mode: uv run main.py --agent --game"| ARC["ARC-AGI-3 harness<br/>/tmp/arc-agi-3"]

  classDef live fill:#e6f4ea,stroke:#3a8f5a,color:#173a24;
  classDef store fill:#fff7e0,stroke:#b8860b,color:#3a2e00;
  class USER,AS,MODEL,KERNEL,SKILLS,CHILD,DAEMON,VIEW,LEDGER,TRIG,PROP,FAST,DEEP,OPP,STEP,AUTH,CTRL,SVC live;
  class HS,SESS,ARCH,ARC store;
```

## Reading the loop

- `P`, `K`, `f` are the three inputs of `Agent(P, K, f)`. In Prime, `P` is `HarnessState.ravo.lineage`, `K` is the harness plus the failure ledger, `f` is the fast/deep/opponent adapter set built by `RavoRunService`.
- The commit gate is the only place the lineage changes (`mutation_requires_authority`). A rejection never mutates state; it routes to Diagnose and repair.
- Weakness pressure runs on commit: every opponent the accepted candidate missed doubles in weight, so the same gap cannot be exploited twice (`pressureW_ge`).
- The token budget and deadline are stop conditions, not gates: they bound cost, they do not affect which candidates can be committed.

## The self-improvement loop, end to end

Stages: run a turn → observe failures at the turn boundary (`_observeFailuresAtTurnBoundary`) → trigger (actionable recurrence ≥ 2, regression inside the champion's 20-observation window, turn interval, compact, manual) → plan from the serialized chain of thought (`planRefinement`) → weighted gate with the referee, judging the live conversation and recording how far it moved while the proposal was planned (`ravoEvaluateProposal` → `authorizeAssistedRavo`) → apply, which decides the final outcome (`_applyRefine`) → the applied change reaches the running model as an in-context refinement notice, and later contexts through the harness digest delivered at cold boundaries (session start, resume, compaction); the system prompt itself stays static.

```mermaid
flowchart TB
  classDef live fill:#dcfce7,stroke:#15803d,color:#14532d
  classDef gate fill:#fef3c7,stroke:#b45309,color:#78350f
  classDef state fill:#e0e7ff,stroke:#4338ca,color:#1e1b4b
  classDef fixed fill:#f1f5f9,stroke:#475569,color:#0f172a,stroke-dasharray:4 3

  subgraph RUN["① RUN A TURN  (RLM execution plane · immutable substrate)"]
    direction LR
    SP["context<br/>= static system prompt<br/>+ harness digest at cold boundaries + refinement notices"]:::fixed
    LLM["model<br/>thinking · text · tool calls"]:::live
    K["IPython kernel<br/>bash() · mcp · rlm() children"]:::fixed
    TR["session JSONL<br/>(thinking blocks kept)"]:::state
    SP --> LLM --> K --> TR
  end

  subgraph OBS["② OBSERVE  (every turn boundary · zero LLM tokens)"]
    direction LR
    EX["extractFailures<br/>python_exception · tool_error · provider_error"]:::live
    FP["fingerprint = sha256(kind, source, class, normalized msg)[:16]<br/>numbers→#  strings→?  paths→&lt;path&gt;"]:::live
    LED[("FAILURE LEDGER<br/>HarnessState.failures, local and (by default) global<br/>count · nonActionableCount · firstSeenTurn · lastSeenTurn<br/>replayCases, verified off the turn path")]:::state
    EX --> FP --> LED
  end
  TR --> EX

  subgraph TRIG["③ TRIGGER"]
    direction LR
    T1{"recurrence<br/>enters count ≥ 2 ∧ actionable"}:::gate
    T2{"regression<br/>claimed fp recurs inside<br/>20-observation window on its clock"}:::gate
    T3{"turn_interval 25 / compact<br/>→ reviewer LLM + 20 min cooldown"}:::gate
    T4["manual /refine · refine.run()"]:::live
  end
  LED --> T1
  LED --> T2
  TR --> T3

  subgraph PLAN["④ PLAN  (agentic variation operator · 1 LLM call)"]
    direction TB
    IN["input = serializeConversation(last 80k chars)<br/>[Assistant thinking] + [Assistant] + [tool calls] + [Tool result]<br/>+ harness overview + ledger<br/>+ refinement history (rejections: decision, judge rationale, missed criteria)"]:::live
    PR["RefinementProposal<br/>edits[]: create|update|delete × prompt|memory|skill|subagent<br/>no addressedFingerprints: normalization strips any, the judge decides"]:::state
    IN --> PR
  end
  T1 --> IN
  T2 --> IN
  T3 --> IN
  T4 --> IN

  subgraph GATE["⑤ GATE  (ravoEvaluateProposal → authorizeAssistedRavo → ravoStep · pure reducer)"]
    direction TB
    FS{"fast screen ≥ 50<br/>structural + skill dry-run<br/>(kernel imports skill, resolves callable)<br/>no LLM"}:::gate
    DJ["deep judge (1 LLM call)<br/>verdict · deepScore 0–100 · missedCriteria<br/>addressedFingerprints: the only claim there is"]:::live
    RF["referee (ravo.referee)<br/>re-runs verified replay cases of claimed fps<br/>whose probe a skill the proposal writes imports"]:::gate
    OPP[("OPPONENT POOL<br/>evidence · scope · minimality · contracts · novelty<br/>+ failure:&lt;fp&gt; for every recurring error the refine is charged<br/>+ referee:&lt;fp&gt; for every adjudicated claim<br/>each with weight w")]:::state
    D1{"deepScore + 10 ≥ best(lineage)?"}:::gate
    D2{"Σ w(missed) ≤ ε = 1?"}:::gate
    FS -- yes --> DJ --> RF --> D1 -- yes --> D2
    OPP -.-> D2
    FS -- no --> RS["reject_screen"]:::gate
    D1 -- no --> RD["reject_deep"]:::gate
    D2 -- no --> RC["reject_criteria"]:::gate
    DJ -->|"failure refine, nothing claimed"| RU["reject_unclaimed"]:::gate
  end
  PR --> FS

  subgraph COMMIT["⑥ COMMIT  (sync · LLM-free · digest re-verified)"]
    direction TB
    V["re-hash proposal + baseline harness<br/>mismatch ⇒ reject (stale context)"]:::live
    AP["apply edits to disk<br/>memory · skill · prompt note · subagent"]:::live
    LIN[("LINEAGE (append-only)<br/>best score is monotone")]:::state
    PRS["PRESSURE<br/>w(missed[0]) ×= 2<br/>same weakness cannot pass twice"]:::live
    WIN["champion claims fingerprints<br/>provisional window = 20 observations on its clock"]:::live
    UM["commit_unmeasured<br/>claimed nothing, not a failure refine<br/>edits apply · RAVO state unchanged · no window"]:::live
    V --> AP --> LIN --> PRS --> WIN
    AP -->|"nothing claimed"| UM
  end
  D2 -- yes --> V
  PRS --> OPP
  WIN --> T2

  AP ==>|"refinement notice in context now<br/>harness digest at the next cold boundary"| SP
```

One concrete cycle:

```mermaid
sequenceDiagram
  autonumber
  participant U as user
  participant M as model + kernel
  participant L as failure ledger
  participant P as /refine planner
  participant G as RAVO gate
  participant H as harness on disk
  Note over M: turn 3 — bash() raises TypeError in skill X
  M->>L: extractFailures → fp a1b2c3 (count 1)
  Note over M: turn 9 — same TypeError, different numbers/paths
  M->>L: normalize → same fp a1b2c3 (count 2)
  L-->>P: recurrence → refine (no reviewer, no cooldown)
  P->>P: read thinking + tool results (last 80k) + ledger
  P->>G: proposal: update skill X (it claims nothing itself)
  G->>G: fast screen: kernel python dry-run imports skill X → 100
  G->>G: deep judge → pass, 72, missed=[], addressedFingerprints=[a1b2c3]
  G->>G: referee: a TypeError has no replay case → not_applicable, the claim stands on the window
  G->>G: opponents now include failure:a1b2c3 (w=1)
  G->>G: 72+10 ≥ best(60) ✓ · Σw(missed)=0 ≤ 1 ✓ → commit
  G->>H: apply: re-verify digests → write skill X → refine.decision commit
  G->>H: lineage += {score 72} · claim fp a1b2c3 · window at global ordinal 40–60
  H-->>M: an in-context refinement notice carries the new skill X
  alt fp a1b2c3 recurs at global ordinal 47
    M->>L: regression (measured fault, inside window, same clock)
    L-->>P: regression refine in the champion's scope → must claim a1b2c3 or be reject_unclaimed
  else ordinal passes 60 without it
    Note over H: champion stands · correction confirmed by outcome, not opinion
  end
```

## Claims, clocks, and the referee

- **The judge makes the claim.** A `/refine` proposal is normalized to its summary, rationale, expected outcome and edits, so an `addressedFingerprints` list the planner writes is dropped. The deep judge names the fingerprints the edits address, filtered to the recurring failures the gate charges, and only that list opens a provisional window and a trust claim. `ravo.run` differs: its implement child still writes `addressedFingerprints` into the artifact, and its opponents and commit gate read that. Its log line holds that claim to the judge: a `ravo.run` commit logs `refinement.committed` only for claims the judge also named (as `<fp>` or `failure:<fp>`) and the certificate did not charge. A self-claim alone is `refinement.applied_unmeasured`, though the champion still records it. A `ravo.run` is charged every actionable fingerprint recurring in the ledger of the store it runs against, with no recency window.
- **Where a `ravo.run` commits.** A local run reads, gates against and commits into the session store. A global run (`global_=True`, `/ravo --global`) uses the global store: its lineage, opponents, entries and failure ledger — the global ledger, which only has data while `PRIME_AGENT_GLOBAL_LEDGER` is on. Its commit reads, applies and saves under the same harness state lock every session's ledger flush takes, with the referee verdicts computed before the lock; checkpoints and the archive stay under the global dir. It does not see this session's observations that have not been flushed yet, which `/refine --global` folds in.
- **What a refine is charged.** A fingerprint recurs when its record is at or over the threshold (count ≥ 2) and actionable. A refine is always charged the recurring fingerprints whose recurrence or regression queued it, including those of a failure request merged into the agent's `refine.run`. Such a trigger is charged on its record in the recurrence ledger (the global one when it is on) when it recurs there, and otherwise on its record in the session's own ledger, so the repair of a failure counted while the global ledger was off can still claim it. A failure refine (`recurrence`, `regression`) is held to its triggers alone. Any other refine is also charged the fingerprints that recur in both the session's own ledger and the recurrence ledger and were last seen no more than 20 assistant turns before the branch's current turn and not after it (`0 ≤ currentTurn − lastSeenTurn ≤ 20`). The ledger does not track branches, so a failure last seen at a later turn than the branch has reached, as after a rewind, was seen on another branch and is not recent. A record is actionable unless a strict majority of its occurrences classified non-actionable (`nonActionableCount`: an outage, a denial, the network, a timeout); when the fingerprint's normalized message or exception class alone classifies that way, every occurrence counts, whatever tally was stored. A non-actionable occurrence never regresses a champion. A recurrence refine is queued when a fingerprint enters the recurring set (at or over the threshold and actionable by the majority of its occurrences), whether it crosses the threshold or turns actionable after crossing it. A `refine.run` merged into a queued failure refine is gated as `directed`.
- **Claimless results.** A failure refine the judge credits with no fingerprint is `reject_unclaimed`. Any other refine that commits without a claim is `commit_unmeasured`: the edits apply, but the RAVO state is left as it was (no lineage entry, no pressure, no window), and the log line is `refinement.applied_unmeasured`, never `refinement.committed`. A `ravo.run` commit with no credited claim logs the same line, but it does advance the RAVO state: it earned its certificate on that run's own evaluators.
- **Window clocks.** A provisional window is `[ordinal, ordinal + 20]`, counted in failure observations rather than turns, on the clock stamped with it. `"ordinal"` is the global ledger's observation total, used for local and global champions alike whenever `PRIME_AGENT_GLOBAL_LEDGER` is on. `"local-ordinal"` is the session ledger's total, used for a local window opened while the flag is off; a global window opened then gets no clock. At a turn boundary a window is compared only with an ordinal read off its own clock: `"local-ordinal"` windows always, `"ordinal"` windows only while the flag is on. A window with no clock (a legacy per-session turn count) or with a clock this build does not know (stripped on load) never regresses, so flipping the flag cannot reopen a closed window or match across clocks. `refineHarness`, which has no session, stamps `"ordinal"` for a global refine and `"local-ordinal"` for a local one. A session settles harness trust windows on the global ordinal whatever the flag says, at every failure ledger flush as well as at a `/refine` apply: local windows on the ordinal the flush merged, global windows under the harness state lock. With the flag off this session never advances that ordinal, so on its own it settles no window and moves no trust, and a recurrence inside a window is neither recorded nor adjudicated.
- **Trust after commit.** An upheld post-commit replay is the only thing that debits trust, and attribution is the whole difficulty: a fingerprint folds every missing module into one, a memory or prompt fix cannot be probed, and the skill may have been rewritten since. So a commit records, per skill it wrote, the modules and distributions that skill imports, and a replay is planned only for a `skill:<id>` whose imports are still exactly those; only on the newest overlapping window that recorded them (an older one, such as the window a regression repair replaced, is superseded); and only when the recurrence's own derived case probes one of those imports — an unrelated missing module recurring in the window runs nothing and uses no attempt. A case that has not reproduced yet waits for its self-check (`ravo.replay_verify`) and is planned once that lands. The replays run off the turn path under `harness.trust.adjudicate`, at most 8 per batch and one batch at a time.

  | situation | trust effect | re-run |
  |---|---|---|
  | `upheld` | −15 once per window on the skill entry the replay ran for; window `faulted` (terminal, also from `clean` or `contested`) | never |
  | `cleared` / `unverifiable` | none | on a later qualifying recurrence, up to 3 runs per (window, entry, fingerprint) |
  | recurrence of another module, or no applicable verified case | nothing runs | — |
  | window closes after a recurrence with no upheld verdict | `contested`: no credit, no debit | — |
  | window closes with no recurrence | `clean`: +5 to every entry it touched | — |

  A memory or prompt entry the same commit wrote is never debited; it can only miss the credit. A `+5` a clean close already granted stays even if a later upheld verdict faults that window. Below the dormancy threshold (30) an entry is dropped from the rendered prompt but stays fully readable and editable.
- **Regression repair.** A regressed champion is repaired in its own scope. Local and global regressions queue separate failure refines, a request never merges into a pending one of the other scope, and a parked request runs after the one ahead of it.
- **After a rejection.** When the certificate still binds, the proposal id is spent and cannot be evaluated again. The rejected result goes to the session JSONL and to the refinement history of the scope it targeted — `<agentDir>/harness/refinements.jsonl` for a global refine, `<agentDir>/harness/local-refinements/<sessionId>.jsonl` for a local one — and `refinement.rejected` is logged with the `cause` that classified it (`gate`, `screen`, `judge_unavailable`, `baseline_changed`, `stale_evidence`). The next planner reads a rejection's gate decision, the judge's rationale and the criteria it missed, never its scores; the judge's text is stripped of markup and invisible characters, truncated and quoted as untrusted output first. A serialized checkpoint or an approved auto-refine restarts the 20-minute cooldown whatever the gate decided, and a failure trigger fires once per fingerprint per session, so a rejected recurrence or regression refine is not retried in that session. A queued or requested refine (`refine.run`, `/refine`, a recurrence or regression repair) cancelled before it applies (by an abort, a branch change, or a compaction that aborts its plan), whether still queued or already planning, reports `refine_failed` and frees the fingerprints no other live request carries to trigger again. A periodic `turn_interval` or `compact` auto-refine that is cancelled is dropped without a report. The working model is not told: only an applied refinement adds a model-facing notice.
- **A rejection on evidence that moved.** The proposer and the judge read the conversation at different moments, and the session keeps working in between. The gate records the difference by message identity (`refine.evidence_drift`: `appended` when the judge read something new, `rewritten` when a compaction, a rewind or a dropped partial reply took away something the proposer read) and judges the live conversation either way — drift never cancels a refine. When the judge itself refused the proposal (`reject_deep`, `reject_criteria`, `reject_unclaimed`), the conversation had moved, and no referee verdict speaks against the claim, the rejection is a timing artefact: it is tagged `stale_evidence` and its round is left open. No cooldown is stamped, the turn interval is not reset, and the failures that queued it stay held; the refine plans once more on the current conversation as soon as the session is idle, at the same checkpoint in a serialized session. That re-plan carries `refine.replan_of`/`replanOf`, closes the round whatever it decides, and is never re-planned itself. A user `/refine`, a rollback and an already-re-planned refine are tagged but not re-planned. An armed re-plan an abort or a branch change drops releases its triggers, and reports `refine_failed` when the refine was the agent's own request; dispose drains it if it can and otherwise drops it silently.

The referee derives replay cases only from the kernel's own traceback for an `ipython` cell that raised, and only as two side-effect-free probes: `import X` (no module named X) and `importlib.metadata.version("d")` (no package metadata). A private module segment or a denylisted top-level module is never derived or run, a record keeps at most 8 distinct cases, all of them valid probes: a stored case of a retired kind (a module attribute, a `from X import n` name, an executable), or any source that no longer re-renders as a probe, is dropped when the ledger is next loaded or written, so it never holds a slot a live case would be evicted to free, and every place that runs or lists a case still refuses one: when X imports, a skill cannot make a guessed name exist. A case is evidence only after the self-check (`ravo.replay_verify`, in the sanitized environment: `PATH`, `HOME`, `LANG`, on Windows also what CPython needs to start, and explicit roots, no inherited `PYTHONPATH`) saw it reproduce its recorded exception. At the gate, each claimed fingerprint gets one verdict:

| status | when | effect |
|---|---|---|
| `not_applicable` | no case probes a module or distribution that a skill create/update in the proposal imports (a memory or prompt fix), or no case is derivable | nothing runs; the claim stands and the provisional window is its referee |
| `no_evidence` | applicable cases exist, none ever verified | `failure:<fp>` fails: the expected evidence is missing, so the claim fails closed |
| `upheld` | a verified applicable case raised its recorded exception again | `failure:<fp>` and `referee:<fp>` fail |
| `unverifiable` | a verified applicable case could not run, or raised something else | `failure:<fp>` and `referee:<fp>` fail |
| `cleared` | every verified applicable case ran clean | `failure:<fp>` and `referee:<fp>` pass |

Adjudication replays in `skillImportEnvironment`: the sanitized base plus the toolforge source roots and the host's `PYTHONPATH` entries, the same environment the skill dry-run screen imports in, and the same one a post-commit trust replay uses. Both run the interpreter in a fresh temporary working directory, so neither the environment nor the working directory can make the two disagree. Every replay interpreter leads its own process group, which is killed when the run ends, on host exit and on SIGINT, SIGTERM, SIGHUP or SIGQUIT, and is recorded in the orphan process journal while it runs, so supervisor recovery reaps it after a worker is killed outright. Only `upheld`, `unverifiable` and `cleared` add `referee:<fp>` to the pool. Persisted `arc:*` criteria are dormant passes in `/refine` and in a judge `ravo.run`, and keep their weights.
