# Refinement: how it works today, and what we want

Audience: leadership. Evidence: the refinement history and failure ledger on this machine, read on 2026-11-01.
Everything marked "goal" is a proposal, not a decision.

## In one sentence

Today the system writes a lot of notes but does not check whether they help. The goal is a loop that picks the errors
that repeat, fixes them, and then checks that they stopped.

## 1. Today: a loop with no measurement

```mermaid
flowchart TB
    A([Agent works in a session]):::start --> B[A trigger fires<br/>compaction, timer or repeated error]
    B --> C[Planner drafts edits<br/><b>158 of 159 accepted edits are notes</b>]
    C --> D{Judge scores 0 to 100<br/>pass if within 10 of the best ever}
    D -->|pass| E[Saved as a note<br/>mostly for one session]
    D -->|fail| F[Dropped<br/>score and bar not shown]
    E --> A
    F --> A
    E -.-> G[/No check that the error<br/>stopped coming back/]:::gap
    classDef start fill:#e8f1ff,stroke:#3b6fd4,color:#111
    classDef gap fill:#ffe3e3,stroke:#d64545,color:#7a1c1c,stroke-dasharray: 5 3
```

![Today](current.png)

What the numbers say (204 proposals from Aug 19 to Oct 29):

- 147 accepted, 51 rejected.
- 158 of 159 accepted edits are memory notes. One is a prompt note. None are skills or helpers.
- Only 6 of the 147 accepted proposals targeted a recorded error.
- The error log holds 659 kinds of error and 4,304 occurrences. None is marked as fixed.
- One status note was rewritten 38 times.
- Of the 4 fixes the gate tracked, 3 never came back and 1 came back 13 days later. The counts are small (2 to 8).

## 2. Goal: a closed loop that measures

```mermaid
flowchart TB
    A([Agent works in a session]):::start --> B[Log every error and group the repeats]
    B --> C[Pick the errors that repeat the most]
    C --> D[Draft the right fix<br/>prompt note, skill, helper or shared memory]
    D --> E{Judge with the evidence<br/>one clear bar, score and reason shown}
    E -->|pass| F[Apply where it helps<br/>all sessions, not one]
    E -->|fail| G[Explain why, retry with a smaller edit]
    G --> D
    F --> H{Measure: did the error come back<br/>in the next sessions?}:::new
    H -->|gone| I[Keep the fix and raise the bar]:::good
    H -->|still there| J[Revert or try a stronger fix]:::warn
    I --> A
    J --> D
    classDef start fill:#e8f1ff,stroke:#3b6fd4,color:#111
    classDef new fill:#fff4d6,stroke:#c98a00,color:#111
    classDef good fill:#e3f7e8,stroke:#2f9e5b,color:#111
    classDef warn fill:#ffe9d6,stroke:#d9731f,color:#111
```

![Goal](target.png)

The two new pieces are the yellow box (measure) and the way a failed measurement feeds back into the next draft.

## 3. Keep status notes and lessons apart

```mermaid
flowchart LR
    subgraph NOW[Today: one pipe for everything]
        direction TB
        N1[Session status<br/>deploy state, commit hashes] --> P1((Refinement gate))
        N2[Real lessons<br/>why an error keeps happening] --> P1
        P1 --> M1[Memory notes<br/>rewritten again and again]
    end
    subgraph GOAL[Goal: two lanes]
        direction TB
        S1[Session status] --> L1[Short session note<br/>one per task, replaced in place]
        S2[Repeated errors and real lessons] --> L2((Learning loop<br/>with measurement))
        L2 --> L3[Prompt notes, skills, shared memories]
    end
    NOW ==> GOAL
    style NOW fill:#fff1f1,stroke:#d64545,color:#111
    style GOAL fill:#effaf2,stroke:#2f9e5b,color:#111
```

![Two lanes](lanes.png)

Most of today's traffic is session status. It crowds out the lessons that would reduce errors, and it competes for the same
quality bar.

## 4. What changes, in plain terms

| Topic | Today | Goal |
|---|---|---|
| What gets fixed | Whatever the last session produced | The errors that repeat the most |
| Kinds of fix | Almost only memory notes | Prompt notes, skills and shared memories as needed |
| Where a fix applies | Mostly one session | All sessions when the lesson is general |
| The quality bar | 0 to 95 depending on the store | One rule per kind of edit |
| What the judge sees | The last ~40,000 characters of the chat | The turns the proposal cites |
| A rejection tells you | The judge's text only | Score, bar, reason and which edit was weak |
| After a fix | Nothing is checked | The error is tracked for the next sessions; a fix that fails is reverted |

## 5. How we will know it works

| Measure | Today | Goal |
|---|---|---|
| Accepted proposals that target a recorded error | 4% (6 of 147) | Most of them |
| Errors with a tracked fix | 0 of 659 | Every accepted fix |
| Fixed errors that come back within 30 days | not measured | Falling month over month |
| Share of Python results that are errors | 3% to 5% over the last 3 weeks | Falling |
| Time from a rejection to a resubmitted fix | not measured | Under one session |

## Caveats

- The history is partial: logs rotate, and only about three weeks of session files remain.
- The error-rate numbers have no control group, so they cannot show cause.
- The counts of fixes that "never came back" rest on 2 to 8 occurrences each.
- The target flow is a design proposal. Nothing in it is built yet.
