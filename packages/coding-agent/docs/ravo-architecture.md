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
