# RAVO and Prime Agent architecture (Mermaid)

Machine-readable maps for agents working on or inside the RAVO loop. The diagrams are plain Mermaid so a model can
read the edges directly; the Rocq references point at sections of the mechanised development in
~/RocqProjects/ravo/Ravo.v (outside this repo) that prove the corresponding property.

RAVO is a fork feature crate (`docs/fork-feature-crates.md`). The code is `crates/pa-ravo` (gate, referee, trust,
`ravo.run`, `/ravo`) on top of `crates/pa-ledger` (failure ledger, replay cases, resolution index), with
`crates/pa-learning` measuring the outcome. It plugs into the native refinement engine in `pa-core` only through
generic seams, and `pa-cli` wires it in `crates/pa-cli/src/features.rs` behind the Cargo features `ledger`, `ravo`
(implies `ledger`) and `learning` (implies `ravo`). Each crate's `README.md` is the precise reference; this document
is the map.

## AVO agentic variation loop

```mermaid
%% AVO agentic variation loop (Puget et al., arXiv:2603.24517) as RAVO instantiates it.
%% Notation from Ravo.v: Agent(P, K, f) with lineage P, knowledge K, evaluator f.
flowchart LR
  subgraph IN["Inputs to AVO"]
    P["Solution lineage P<br/>candidates + scores<br/>(Rocq: Lineage = list (A * nat))"]
    K["Knowledge base K<br/>harness entries + failure ledger<br/>(Rocq S11: Ledger)"]
    f["Evaluator f<br/>fast screen + deep judge + opponents<br/>(Rocq: fast, deep, clearsEW)"]
  end

  subgraph LOOP["AVO agentic variation loop: Agent(P, K, f)"]
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

  SUP["Supervisor child<br/>consulted after two rejections<br/>(ravo.run only)"]
  CAND["Candidate<br/>solution and score (x, deep x)"]
  GATE{"commit gate<br/>tau <= fast x<br/>bestScore P <= deep x + tolerance<br/>missedWeight <= eps"}
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
  UPD -.->|"weakness pressure: missed opponents double in weight<br/>(Rocq S4/S7: pressureW)"| f

  classDef input fill:#f4f6f8,stroke:#6b7c8a,color:#1c2b36;
  classDef loop fill:#e8f2f6,stroke:#2f6f8f,color:#1c2b36;
  classDef ok fill:#e6f4ea,stroke:#3a8f5a,color:#173a24;
  classDef bad fill:#fdecec,stroke:#d9534f,color:#5a1a1a;
  class P,K,f input;
  class S1,S2,S3,S4,S5 loop;
  class UPD ok;
  class REJ bad;
```

Two instantiations run this loop. Every gated `/refine` (and every other planned refine) is one pass of steps 4 and 5
with a single candidate: the gate decides, and a rejection is final for that proposal. `ravo.run` (or `/ravo <task>`)
runs the whole loop: inspect, plan, implement or repair, evaluate, step the reducer, until a commit or a limit.

## Prime Agent: three planes

```mermaid
%% Prime Agent with the fork's RAVO crates, as of merge-rust-port.
%% Three planes: RLM execution (what runs), Continual Harness (what is learned), RAVO (how learning is gated).
flowchart TB
  subgraph RLM["RLM execution plane: pa-core, pa-daemon"]
    direction LR
    USER["Client<br/>pa-cli + pa-tui<br/>daemon wire only"] --> DAEMON["Supervisor + session worker<br/>pa-daemon"]
    DAEMON --> ENG["Session engine<br/>pa-core session_engine<br/>turn loop, host requests"]
    ENG --> MODEL["Model call<br/>pa-ai providers"]
    ENG --> KERNEL["Python kernel (rlm)<br/>persistent REPL<br/>bash(), edit, skills, mcp"]
    KERNEL --> SKILLS["Python skills<br/>refine, ravo, agent_message, ..."]
    SKILLS -->|"host request"| ENG
    KERNEL -->|"rlm.spawn"| CHILD["Child sessions"]
    DAEMON --> VIEW["Agents view<br/>featureStatus.ravo line"]
  end

  subgraph HARNESS["Continual Harness plane: pa-core refinement, pa-ledger"]
    direction LR
    HS["harness_state.json<br/>entries: memory, skill, subagent, prompt, factory<br/>keys: ravo, failures, trustWindows"]
    LEDGER["Failure ledger (pa-ledger)<br/>fingerprint(kind, source, class, msg)<br/>per session, and global unless<br/>PRIME_AGENT_GLOBAL_LEDGER=0"]
    TRIG["Refine triggers<br/>turn_interval 25, compact (reviewed, 20 min cooldown)<br/>recurrence, regression (queued by pa-ravo)<br/>manual /refine, refine.run"]
    PROP["plan_refinement<br/>one model call proposes edits"]
    HS --> PROP
    LEDGER --> TRIG --> PROP
  end

  subgraph RAVO["RAVO plane: pa-ravo"]
    direction LR
    FAST["fast screen<br/>share of well-formed edits"]
    DEEP["deep judge<br/>one model call: verdict, score 0-100,<br/>missed criteria, addressed fingerprints"]
    OPP["opponents<br/>evidence, scope, minimality, contracts, novelty<br/>failure:&lt;fp&gt; per charged recurrence<br/>referee:&lt;fp&gt; per adjudicated claim"]
    STEP["ravo_step (reducer)<br/>tau <= fast, best <= deep + 10, missed <= eps<br/>commit: missed weights double"]
    AUTH["authorize_assisted_ravo<br/>digest-binds proposal + baseline<br/>provisional champion, 20-observation window"]
    SVC["RavoRunService<br/>ravo.run, /ravo<br/>run_ravo_controller"]
    ARCH["RavoArchive<br/>hash-chained events.jsonl + champion CAS"]
    TRUST["Trust windows<br/>+5 clean, -15 upheld, dormant below 30"]
    FAST --> STEP
    DEEP --> STEP
    OPP --> STEP
    STEP --> AUTH
    SVC --> STEP
    SVC --> ARCH
  end

  LEARN["pa-learning<br/>learning index, prime-agent learning<br/>trajectory levers"]

  ENG -->|"every finalized message<br/>(on_message_end)"| LEDGER
  PROP -->|"RefinementGate::evaluate"| FAST
  AUTH -->|"admit: apply edits, save state"| HS
  AUTH --> TRUST
  TRUST --> HS
  HS -->|"refinement notice now<br/>harness digest at cold boundaries"| ENG
  LEDGER -->|"recurring fingerprints become opponents"| OPP
  LEDGER -->|"claimed fp recurs inside the window<br/>regression repair in the champion's scope"| TRIG
  SKILLS -->|"ravo.run host request"| SVC
  SVC -->|"publish_feature_status"| DAEMON
  ENG -->|"agent.jsonl spans + refinement.committed"| LEARN
  LEARN -->|"HarnessPromptHook, RecurrenceFilter"| HS

  classDef live fill:#e6f4ea,stroke:#3a8f5a,color:#173a24;
  classDef store fill:#fff7e0,stroke:#b8860b,color:#3a2e00;
  class USER,DAEMON,ENG,MODEL,KERNEL,SKILLS,CHILD,VIEW,LEDGER,TRIG,PROP,FAST,DEEP,OPP,STEP,AUTH,SVC,LEARN live;
  class HS,ARCH,TRUST store;
```

## Reading the loop

- `P`, `K`, `f` are the three inputs of Agent(P, K, f). In Prime Agent, `P` is the lineage in the `ravo` key of the
  target store's `harness_state.json` (`RavoState`), `K` is the harness entries plus the `failures` ledger, and `f` is
  the screen, judge and opponent set that `ravo_evaluate_proposal` (for a refine) or `RavoRunService` (for a run)
  builds.
- The commit gate is the only place the lineage changes. A rejection never applies an edit; in `/refine` the proposal
  id is spent, in `ravo.run` it routes to diagnose and repair.
- Weakness pressure runs on commit: every opponent the accepted candidate missed doubles in weight (`ravo_pressure`),
  so the same gap costs twice as much next time. Rocq's pressureW doubles one weak opponent; its lemma pressureW_ge
  (weights never decrease) holds for the reducer's per-criterion doubling as well.
- The token budget and deadline of a `ravo.run` are stop conditions, not gates: they bound cost and do not affect which
  candidates can commit.

## The self-improvement loop, end to end

Stages: run a turn; the ledger observes every finalized message on its own worker thread and treats each assistant
message as a turn boundary (`pa-ledger`, through the `on_message_end` hook of `SessionFeature`); a trigger fires (a
fingerprint entering the recurring set, a claimed fingerprint recurring inside a champion's window, the reviewed
turn-interval or compaction checkpoint, or a manual `/refine` / `refine.run`); the native planner proposes edits from
the serialized conversation (`plan_refinement`); the gate evaluates the plan with one model call (the `RefinementGate`
trait in `pa_core::refinement::gate`, implemented by `pa-ravo`); its verdict (`RefinementGateVerdict`) admits or
refuses against the target store re-read at apply time; an applied change reaches the running model as an in-context
refinement notice, and later contexts through the harness digest at cold boundaries. The system prompt stays static.

```mermaid
flowchart TB
  classDef live fill:#dcfce7,stroke:#15803d,color:#14532d
  classDef gate fill:#fef3c7,stroke:#b45309,color:#78350f
  classDef state fill:#e0e7ff,stroke:#4338ca,color:#1e1b4b
  classDef fixed fill:#f1f5f9,stroke:#475569,color:#0f172a,stroke-dasharray:4 3

  subgraph RUN["1 RUN A TURN (RLM execution plane)"]
    direction LR
    SP["context<br/>static system prompt<br/>+ harness digest at cold boundaries + refinement notices"]:::fixed
    LLM["model<br/>thinking, text, tool calls"]:::live
    K["Python kernel<br/>bash(), mcp, rlm children"]:::fixed
    TR["session JSONL"]:::state
    SP --> LLM --> K --> TR
  end

  subgraph OBS["2 OBSERVE (each assistant message, ledger worker thread, no model call)"]
    direction LR
    EX["extract_failures<br/>python_exception, tool_error, provider_error"]:::live
    FP["fingerprint = sha256(canonical kind, source, class, message)[:16]<br/>numbers to #, strings to ?, paths to &lt;path&gt;"]:::live
    LED[("FAILURE LEDGER<br/>failures key, local and (by default) global<br/>count, nonActionableCount, firstSeenTurn, lastSeenTurn<br/>replayCases, verified off the turn path")]:::state
    EX --> FP --> LED
  end
  TR --> EX

  subgraph TRIG["3 TRIGGER"]
    direction LR
    T1{"recurrence<br/>enters count >= 2 and actionable"}:::gate
    T2{"regression<br/>claimed fp recurs inside<br/>20-observation window on its clock"}:::gate
    T3{"turn_interval 25 / compact<br/>reviewer model call + 20 min cooldown"}:::gate
    T4["manual /refine, refine.run()"]:::live
  end
  LED --> T1
  LED --> T2
  TR --> T3

  subgraph PLAN["4 PLAN (pa-core planner, one model call)"]
    direction TB
    IN["input = serialized conversation (last 80 000 chars)<br/>+ harness overview + refinement history<br/>+ the ledger's recurrence or regression instructions"]:::live
    PR["RefinementProposal<br/>edits: create, update, delete of<br/>prompt, memory, skill, subagent, factory"]:::state
    IN --> PR
  end
  T1 --> IN
  T2 --> IN
  T3 --> IN
  T4 --> IN

  subgraph GATE["5 GATE (ravo_evaluate_proposal, authorize_assisted_ravo, ravo_step)"]
    direction TB
    FS{"fast screen >= 50<br/>share of well-formed edits<br/>no model call"}:::gate
    DJ["deep judge (one model call)<br/>verdict, score 0-100, failedCriteria<br/>addressedFingerprints: the only claim there is"]:::live
    RF["referee<br/>re-runs verified replay cases of claimed fps<br/>whose probe a skill the proposal writes imports"]:::gate
    OPP[("OPPONENT POOL<br/>evidence, scope, minimality, contracts, novelty<br/>+ failure:&lt;fp&gt; for each charged recurrence<br/>+ referee:&lt;fp&gt; for each adjudicated claim<br/>each with weight w")]:::state
    D1{"deep score + 10 >= best(lineage)?"}:::gate
    D2{"sum of w(missed) <= eps = 1?"}:::gate
    FS -- yes --> DJ --> RF --> D1 -- yes --> D2
    OPP -.-> D2
    FS -- no --> RS["reject_screen"]:::gate
    D1 -- no --> RD["reject_deep"]:::gate
    D2 -- no --> RC["reject_criteria"]:::gate
    DJ -->|"failure refine, nothing claimed"| RU["reject_unclaimed"]:::gate
  end
  PR --> FS

  subgraph COMMIT["6 COMMIT (at apply time, no model call)"]
    direction TB
    V["re-read target store, re-check digests<br/>mismatch: reject (baseline_changed)"]:::live
    AP["apply edits, save harness_state.json"]:::live
    LIN[("LINEAGE (append-only)<br/>best score is monotone")]:::state
    PRS["PRESSURE<br/>each missed criterion's weight doubles"]:::live
    WIN["champion claims fingerprints<br/>provisional window = 20 observations on its clock<br/>trust window over the entries it wrote"]:::live
    UM["commit_unmeasured<br/>claimed nothing, not a failure refine<br/>edits apply, RAVO state unchanged, no window"]:::live
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
  participant M as model + kernel
  participant L as failure ledger
  participant P as refine planner
  participant G as RAVO gate
  participant H as harness on disk
  Note over M: turn 3: an ipython cell raises ModuleNotFoundError in skill X
  M->>L: extract_failures, fp a1b2c3 (count 1), replay case import y derived
  Note over L: off the turn path the self-check reproduces it: case verified
  Note over M: turn 9: same error, different numbers and paths
  M->>L: normalize, same fp a1b2c3 (count 2)
  L-->>P: enters the recurring set, recurrence refine queued (no review, no cooldown)
  P->>P: serialized conversation + harness overview + recurrence instructions
  P->>G: proposal: update skill X (the planner claims nothing)
  G->>G: fast screen: every edit well formed, 100
  G->>G: deep judge: pass, 72, failedCriteria=[], addressedFingerprints=[a1b2c3]
  G->>G: referee: skill X imports y, the verified case runs clean, cleared
  G->>G: opponents include failure:a1b2c3 and referee:a1b2c3, both pass
  G->>G: 72+10 >= best(60), sum w(missed)=0 <= 1, commit
  G->>H: re-read store, digests match, write skill X
  G->>H: lineage += score 72, claim a1b2c3, window at ordinal 40 to 60
  H-->>M: an in-context refinement notice carries the new skill X
  alt fp a1b2c3 recurs at ordinal 47
    M->>L: regression recorded on the champion at the next flush
    L-->>P: regression repair in the champion's scope: must claim a1b2c3 or be reject_unclaimed
  else the ordinal passes 60 without it
    Note over H: champion stands, trust window closes clean (+5 to skill X)
  end
```

## Claims, clocks, and the referee

- **The judge makes the claim.** A `/refine` proposal carries no claim of its own: the planner's output is a summary,
  rationale, expected outcome and edits. The deep judge names the fingerprints the edits address, filtered to the
  recurring failures the gate charges, and only that list opens a provisional window and a trust claim. `ravo.run`
  differs: its implement child writes `addressedFingerprints` into the artifact, and its opponents, referee and commit
  gate read that. Its outcome line holds that claim to the judge: a run's commit logs `refinement.committed` only for
  claims the judge also named and the certificate credited; a self-claim alone logs `refinement.applied_unmeasured`,
  though the champion still records it.
- **Where a `ravo.run` commits.** A local run reads, gates against and commits into the session store. A global run
  (`global_=True` in the `ravo` skill, `/ravo --global`) uses the global store: its lineage, opponents, entries and
  failure ledger. Its commit reads, applies and saves under the harness state lock every ledger flush takes
  (`acquire_harness_state_lock`). Checkpoints (`<store>/ravo/runs/<runId>.json`) and the archive
  (`<store>/ravo/archive/`) live under the store the run targets. One run per session at a time, in the background,
  only in a top-level session with a local harness store (`ravo_run_allowed`); limits default to 4 rounds, 3 repairs,
  20 minutes and 1.5M tokens (`RAVO_RUN_MAX_ROUNDS`, `RAVO_RUN_MAX_REPAIRS`, `RAVO_RUN_DEADLINE`,
  `RAVO_RUN_TOKEN_BUDGET`). It stops with one of `RavoStopReason`: accepted, round or repair limit, deadline, budget,
  cancelled, or `stale_cas` when the store moved under it.
- **What a refine is charged.** A fingerprint recurs when its record is at or over the threshold (count >= 2,
  `DEFAULT_RECURRENCE_THRESHOLD`) and actionable. A refine is always charged the fingerprints whose recurrence or
  regression queued it (`triggerFingerprintIds`), on their record in the recurrence ledger (the global one while it is
  on), else on the session's own record. A failure refine (`recurrence`, `regression`) is held to its triggers alone.
  Any other refine is also charged the fingerprints recurring in the session's own ledger that were last seen no more
  than 20 assistant turns before the branch's current turn and not after it; a failure last seen at a later turn than
  the branch has reached (as after a rewind) was seen on another branch and is not recent. A record is actionable
  unless a strict majority of its occurrences classified non-actionable (`nonActionableCount`: an abort, a denial, a
  dead kernel, the network, a timeout, provider capacity). A `refine.run` merged into a queued failure refine is gated
  as directed.
- **Claimless results.** A failure refine the judge credits with no fingerprint is `reject_unclaimed`. Any other refine
  that commits without a claim is `commit_unmeasured`: the edits apply, the RAVO state is left as it was (no lineage
  entry, no pressure, no window), and the outcome line is `refinement.applied_unmeasured`, never
  `refinement.committed`. A `ravo.run` commit with no credited claim logs the same line but does advance the RAVO
  state: it earned its certificate on that run's own evaluators.
- **Window clocks.** A provisional window is `[ordinal, ordinal + 20]`, counted in failure observations rather than
  turns, on the clock stamped with it (`RavoWindowClock`). `ordinal` is the global ledger's observation total, used for
  local and global champions alike while the global ledger is on. `local-ordinal` is the session ledger's total, used
  for a local window opened while it is off; a global window opened then gets no clock. A window is compared only with
  an ordinal read off its own clock, and a window with no clock or one this build does not know (stripped on load)
  never regresses, so flipping `PRIME_AGENT_GLOBAL_LEDGER` cannot reopen a closed window or match across clocks.
- **Trust after commit.** An upheld post-commit replay is the only thing that debits trust, and attribution is the
  whole difficulty: a fingerprint folds every missing module into one, a memory or prompt fix cannot be probed, and the
  skill may have been rewritten since. So a gated commit that claims fingerprints opens a trust window
  (`trustWindows[<proposalId>]`) recording the entries it wrote and the imports each written skill names. A replay is
  planned only for a skill that still imports exactly what the commit recorded, only on the newest window that wrote
  it, and only when the recurrence's own case probes one of those imports. A case not yet verified waits for its
  self-check. The replays run off the turn path under the root span `harness.trust.adjudicate`, at most 8 per batch
  (`MAX_TRUST_ADJUDICATION_JOBS`) and one batch at a time per session. Windows settle at each ledger flush and before a
  refine's edits apply (`prepare_application`).

  | situation | trust effect | re-run |
  |---|---|---|
  | `upheld` | -15 once per window on the skill entry the replay ran for; window `faulted` | never |
  | `cleared` / `unverifiable` | none | on a later qualifying recurrence, up to 3 runs per (window, entry, fingerprint) |
  | recurrence of another module, or no applicable verified case | nothing runs | none |
  | window closes after a recurrence with no upheld verdict | `contested`: no credit, no debit | none |
  | window closes with no recurrence | `clean`: +5 to every entry it touched | none |

  A memory or prompt entry the same commit wrote is never debited; it can only miss the credit. Trust is clamped to
  [0, 100] and defaults to 50 (`DEFAULT_ENTRY_TRUST`). Below 30 (`DORMANT_TRUST_THRESHOLD`) an entry is dormant: the
  `HarnessPromptHook` withholds it from the rendered harness digest and from the judge's overview (a
  `- +N dormant <kind> entries (below trust threshold; still readable and editable)` line), and it stays fully readable
  and editable.
- **Regression repair.** A regressed champion is repaired in its own scope. Local and global regressions queue separate
  failure refines, a request merges only into a pending one of its own scope and parks behind one of the other scope,
  and each fingerprint queues at most once per kind per session. A pending request an aborted turn drops unserviced
  (`RefineRequester::on_dropped`) releases its fingerprints so they may queue a repair again.
- **After a rejection.** When the certificate still binds, the consumed evaluation is saved into the `ravo` key, so the
  proposal id cannot be evaluated again. The rejected result is recorded on the session JSONL (an audit row and an
  outcome row) and, for a global refine, in `<agentDir>/harness/refinement_history.jsonl`; the outcome line
  `refinement.rejected` carries the `cause` that classified it (`gate`, `screen`, `judge_unavailable`,
  `baseline_changed`). A failure trigger fires once per fingerprint per session, so a rejected recurrence or
  regression refine is not retried in that session. The working model is not told: only an applied refinement adds a
  model-facing notice.

The referee derives replay cases only from the kernel's own traceback for an `ipython` cell that raised, and only as
two side-effect-free probes (`ReplayProbe`): `import X` (no module named X) and `importlib.metadata.version("d")` (no
package metadata). A private module segment or a top-level module in `REPLAY_MODULE_DENYLIST` is never derived or run
(`is_replayable_module_path`), and a record keeps at most 8 distinct cases (`MAX_REPLAY_CASES`); stored cases that no
longer re-render as a valid probe are dropped when the ledger is next loaded or written. A case is evidence only after
the self-check saw it reproduce its recorded exception: each boundary's newly derived cases run off the turn path in
the sanitized environment, and a reproduction is written at the ledger's next flush (`record_replay_verifications`).
At the gate, each claimed fingerprint gets one verdict (`RefereeVerdictStatus`):

| status | when | effect |
|---|---|---|
| `not_applicable` | no case probes a module or distribution that a skill create/update in the proposal imports (a memory or prompt fix), or no case is derivable | nothing runs; the claim stands and the provisional window is its referee |
| `no_evidence` | applicable cases exist, none ever verified | `failure:<fp>` fails: the expected evidence is missing, so the claim fails closed |
| `upheld` | a verified applicable case raised its recorded exception again | `failure:<fp>` and `referee:<fp>` fail |
| `unverifiable` | a verified applicable case could not run, or raised something else | `failure:<fp>` and `referee:<fp>` fail |
| `cleared` | every verified applicable case ran clean | `failure:<fp>` and `referee:<fp>` pass |

`PythonReplayRunner` runs every case in the kernel's Python in isolated mode (`-I -B`, the program on stdin), in a
fresh temporary working directory, with an environment of `PATH`, `HOME` and `LANG` (on Windows also what CPython needs
to start) plus explicit `sys.path` roots, under a 10 s timeout (`DEFAULT_REPLAY_TIMEOUT`). The self-check uses that
sanitized environment (`ReplayEnvironment::Sanitized`); adjudication and the post-commit trust replay add the host's
`PYTHONPATH` entries (`ReplayEnvironment::SkillImport`), so the two adjudications cannot disagree. The interpreter
leads its own process group, killed when the run ends, and is recorded in the orphan process journal while it runs, so
supervisor recovery reaps it after a worker is killed outright. Only `upheld`, `unverifiable` and `cleared` add
`referee:<fp>` to the pool. Persisted criteria a `/refine` or a judge `ravo.run` never observes are dormant passes and
keep their weights.

## Seams and files

The crates touch native code only through these generic seams (details in each crate's README):

| seam | used by | for |
|---|---|---|
| `SessionFeature::refinement_gate` (`RefinementGate`, `RefinementGateVerdict`) | `pa-ravo` | gate every planned refine, hold ledger flushes while it runs, lock the global store, admit or refuse at apply time |
| `RefineRequester` (`pa_core::session_engine::turn_boundary`) | `pa-ravo` | queue recurrence and regression refines for the next serviced turn boundary |
| `SessionFeature::auto_refine_policy` (`AutoRefinePolicy`) | `pa-ravo` | `GlobalDefaultAutoRefine`: the reviewer may pick `scope`, global by default |
| `SessionFeature::harness_prompt_hook` (`HarnessPromptHook`) | `pa-ravo`, `pa-learning` | withhold dormant entries; rank entries by trajectory and add the trajectory section |
| `LedgerObserver` / `LedgerHandle` (`pa-ledger`) | `pa-ravo` | see each boundary, write the `ravo` and `trustWindows` keys inside the ledger's flush |
| `RecurrenceFilter` | `pa-learning` | mute recurrence refines for fingerprints the trajectory index labels DROPPED |
| `SessionFeature::register_host_handlers`, `slash_commands`, `bundled_skills` | `pa-ravo` | `ravo.run`, `ravo.status`, `ravo.cancel`; `/ravo`; the `ravo` skill (`skills/.features/ravo`) |
| `publish_feature_status` | `pa-ravo` | each run update as feature `ravo` (`featureStatus.ravo`, `ravo_status_line`) |

Files: the `ravo` and `trustWindows` keys of `<sessionArtifactDir>/harness/harness_state.json` and
`<agentDir>/harness/harness_state.json` (`pa-ravo`), their `failures` key and
`<agentDir>/resolution/<basename>.<hash>.json` (`pa-ledger`), and `<agentDir>/learning/` (`pa-learning`). All are
byte-compatible with the TS fork, proven by node-generated goldens in each crate's `tests/`. `PRIME_AGENT_RAVO=0`
turns the gate off at run time; building `pa-cli` without the `ravo` feature removes it entirely.

## Differences from the TS fork

The Rust crates reproduce the TS gate, referee, trust and run service byte for byte where they write files. These TS
behaviours are not ported (each crate README lists its own non-goals):

- **Skill dry-run in the fast screen.** TS imported each proposed skill in the kernel and resolved its callable
  (`refinement/skill-dry-run.ts`); the Rust screen is structural only.
- **Evidence drift and the stale-evidence re-plan.** TS recorded how far the conversation moved between planner and
  judge (`refine.evidence_drift`) and re-planned a judge rejection made on moved evidence once (`stale_evidence`,
  `refine.replan_of`). `RejectionCause` keeps the `stale_evidence` spelling for stored results, but nothing produces
  it.
- **Rejection history for the planner.** TS fed each rejection's gate decision, cleaned judge rationale and missed
  criteria into the next planner prompt, and kept a per-session `local-refinements/<sessionId>.jsonl`. The native
  planner's history lists prior results' edits and expected outcomes only, and a local rejection is recorded only on
  the session JSONL.
- **Spans.** `ravo.referee`, `ravo.replay_case` and `ravo.replay_verify` are not emitted; `ravo.run`, `ravo.round`,
  `ravo.proposal`, `ravo.evaluation` and `harness.trust.adjudicate` are. `harness.trust.adjudicate` has no
  `trigger.trace_id` (the ledger's boundary runs on its worker thread, outside the turn's trace), and there is no
  native refine span to carry `trust.*` attributes or `refine.trust_window_opened`.
- **Trust on ungated refines.** TS gave every entry any refine wrote a trust record and settled windows at every apply;
  here only a gated commit does (an absent record reads as the default score).
- **The skill-import environment** has no toolforge source roots: `pa-cli` passes no extra `sys.path` roots
  (`replay_sys_path` is empty), so adjudication sees the sanitized base plus the host's `PYTHONPATH`.
- **Around `ravo.run`:** the status reaches clients as the generic `feature_status` event, not the TS
  `ravo_run_update`; children are single provider calls (no retained worker runtime); a run cannot resume from its
  checkpoint; evaluators run one at a time; the `ravo` skill is listed to every session of a build with the feature,
  where its calls fail as unregistered outside a session offered runs.
- **The ARC-AGI-3 evaluator** (`docs/ravo-arc-agi-evaluator.md`) is benchmark code outside the product:
  `/ravo --arc-repo/--arc-game` and `ravo.run(arc_agi=...)` are refused, and persisted `arc:*` criteria stay dormant
  passes.
