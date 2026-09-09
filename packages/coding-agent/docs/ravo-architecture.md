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
    LEDGER["Failure ledger<br/>core/ravo/failure-ledger.ts<br/>fingerprint(kind, source, class, msg) → count"]
    TRIG["Refine triggers<br/>turn_interval · compact · recurrence(count>=2) · regression"]
    PROP["refine.run / _planRefine<br/>LLM proposes RefinementProposal edits"]
    HS --> PROP
    LEDGER --> TRIG --> PROP
  end

  subgraph RAVO["RAVO plane — core/ravo"]
    direction LR
    FAST["fast screen<br/>structural validity − skill dry-run failures<br/>(refinement/skill-dry-run.ts)"]
    DEEP["deep score<br/>LLM judge 0-100, or ARC-AGI-3 levels completed<br/>(arc-agi-evaluator.ts)"]
    OPP["opponents<br/>evidence · scope · minimality · contracts · novelty<br/>failure:&lt;fp&gt; per recurring error · arc:no-crash · arc:all-levels"]
    STEP["ravoStep (reducer.ts)<br/>tau ≤ fast ∧ best ≤ deep ∧ missedWeight ≤ eps<br/>commit → pressure doubles missed weights"]
    AUTH["authorizeAssistedRavo (authority.ts)<br/>digest-binds proposal + baseline<br/>provisional champion, 20-turn window"]
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
  HS -->|"memories · skills · notes into system prompt"| AS
  LEDGER -->|"recurring fingerprints become opponents"| OPP
  AS -->|"recurrence inside the provisional window = measured fault<br/>→ regression refine (gated repair)"| TRIG
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

Stages: run a turn → observe failures at the turn boundary (`_observeFailuresAtTurnBoundary`) → trigger (recurrence ≥ 2, regression inside the 20-turn window, turn interval, manual) → plan from the serialized chain of thought (`planRefinement`) → weighted gate (`ravoDecide`) → commit with pressure (`ravoCommit`) → harness rendered into the next system prompt.

```mermaid
flowchart TB
  classDef live fill:#dcfce7,stroke:#15803d,color:#14532d
  classDef gate fill:#fef3c7,stroke:#b45309,color:#78350f
  classDef state fill:#e0e7ff,stroke:#4338ca,color:#1e1b4b
  classDef fixed fill:#f1f5f9,stroke:#475569,color:#0f172a,stroke-dasharray:4 3

  subgraph RUN["① RUN A TURN  (RLM execution plane · immutable substrate)"]
    direction LR
    SP["system prompt<br/>= base prompt (fixed)<br/>+ Continual Harness State (mutable)"]:::fixed
    LLM["model<br/>thinking · text · tool calls"]:::live
    K["IPython kernel<br/>bash() · mcp · rlm() children"]:::fixed
    TR["session JSONL<br/>(thinking blocks kept)"]:::state
    SP --> LLM --> K --> TR
  end

  subgraph OBS["② OBSERVE  (every turn boundary · zero LLM tokens)"]
    direction LR
    EX["extractFailures<br/>python_exception · tool_error · provider_error"]:::live
    FP["fingerprint = sha256(kind, source, class, normalized msg)[:16]<br/>numbers→#  strings→?  paths→&lt;path&gt;"]:::live
    LED[("FAILURE LEDGER<br/>HarnessState.failures<br/>count · firstSeenTurn · lastSeenTurn")]:::state
    EX --> FP --> LED
  end
  TR --> EX

  subgraph TRIG["③ TRIGGER"]
    direction LR
    T1{"recurrence<br/>count ≥ 2"}:::gate
    T2{"regression<br/>claimed fp recurs<br/>inside 20-turn window"}:::gate
    T3{"turn_interval 25 / compact<br/>→ reviewer LLM + 20 min cooldown"}:::gate
    T4["manual /refine · refine.run()"]:::live
  end
  LED --> T1
  LED --> T2
  TR --> T3

  subgraph PLAN["④ PLAN  (agentic variation operator · 1 LLM call)"]
    direction TB
    IN["input = serializeConversation(last 80k chars)<br/>[Assistant thinking] + [Assistant] + [tool calls] + [Tool result]<br/>+ harness overview + refinement history + ledger"]:::live
    PR["RefinementProposal<br/>edits[]: create|update|delete × prompt|memory|skill|subagent<br/>+ addressedFingerprints"]:::state
    IN --> PR
  end
  T1 --> IN
  T2 --> IN
  T3 --> IN
  T4 --> IN

  subgraph GATE["⑤ GATE  (ravoEvaluateProposal → ravoDecide · pure reducer)"]
    direction TB
    FS{"fast screen ≥ 50<br/>structural + skill dry-run<br/>(kernel imports skill, resolves callable)<br/>no LLM"}:::gate
    DJ["deep judge (1 LLM call)<br/>deepScore 0–100 · missedCriteria · addressedFingerprints"]:::live
    OPP[("OPPONENT POOL<br/>evidence · scope · minimality · contracts · novelty<br/>+ failure:&lt;fp&gt; for every recurring error<br/>each with weight w")]:::state
    D1{"deepScore + 10 ≥ best(lineage)?"}:::gate
    D2{"Σ w(missed) ≤ ε = 1?"}:::gate
    FS -- yes --> DJ --> D1 -- yes --> D2
    OPP -.-> D2
    FS -- no --> RS["reject_screen"]:::gate
    D1 -- no --> RD["reject_deep"]:::gate
    D2 -- no --> RC["reject_criteria"]:::gate
  end
  PR --> FS

  subgraph COMMIT["⑥ COMMIT  (sync · LLM-free · digest re-verified)"]
    direction TB
    V["re-hash proposal + baseline harness<br/>mismatch ⇒ reject (stale context)"]:::live
    AP["apply edits to disk<br/>memory · skill · prompt note · subagent"]:::live
    LIN[("LINEAGE (append-only)<br/>best score is monotone")]:::state
    PRS["PRESSURE<br/>w(missed[0]) ×= 2<br/>same weakness cannot pass twice"]:::live
    WIN["champion claims fingerprints<br/>provisional window = 20 turns"]:::live
    V --> AP --> LIN --> PRS --> WIN
  end
  D2 -- yes --> V
  PRS --> OPP
  WIN --> T2

  AP ==>|"formatHarnessStateForPrompt<br/>rendered into next turn's prompt"| SP
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
  P->>G: proposal: update skill X, addressedFingerprints=[a1b2c3]
  G->>G: fast screen: kernel imports skill X → 100
  G->>G: deep judge → 72, missed=[]
  G->>G: opponents now include failure:a1b2c3 (w=1)
  G->>G: 72+10 ≥ best(60) ✓ · Σw(missed)=0 ≤ 1 ✓ → commit
  G->>H: re-verify digests → write skill X
  G->>H: lineage += {score 72} · claim fp a1b2c3 · window turns 10–30
  H-->>M: next system prompt carries the new skill X
  alt fp a1b2c3 recurs at turn 17
    M->>L: regression (measured fault, inside window)
    L-->>P: regression refine → must re-address a1b2c3, w(failure:a1b2c3) doubles on commit
  else silent through turn 30
    Note over H: champion stands · correction confirmed by outcome, not opinion
  end
```
