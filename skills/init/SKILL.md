---
name: init
description: Analyze this codebase and write (or improve) its AGENTS.md, the instruction file every Prime Agent session loads. Invoked by /init.
disable-model-invocation: true
---

# /init

Please analyze this codebase and create an AGENTS.md file, which will be given
to future agent sessions (Prime Agent, and any other agent that reads
AGENTS.md) to operate in this repository.

## Which file

Prime Agent loads `AGENTS.md` first and `CLAUDE.md` beside it from the
project root (and both from every directory between the git root and the
working directory). Pick the target before you write:

- An `AGENTS.md` at the project root exists: improve it.
- Only a `CLAUDE.md` exists: improve that file instead of creating a second
  one; both load, so a new `AGENTS.md` would duplicate it.
- Neither exists: create `AGENTS.md` at the project root.

## What to add

1. Commands that will be commonly used, such as how to build, lint, and run
   tests. Include the necessary commands to develop in this codebase, such as
   how to run a single test.
2. High-level code architecture and structure so that future sessions can be
   productive more quickly. Focus on the "big picture" architecture that
   requires reading multiple files to understand.

## Usage notes

- If there's already an instruction file, suggest improvements to it (and
  apply the ones the user agrees with).
- When you make the initial file, do not repeat yourself and do not include
  obvious instructions like "Provide helpful error messages to users", "Write
  unit tests for all new utilities", "Never include sensitive information
  (API keys, tokens) in code or commits".
- Avoid listing every component or file structure that can be easily
  discovered.
- Don't include generic development practices.
- If there are Cursor rules (in `.cursor/rules/` or `.cursorrules`), Copilot
  rules (in `.github/copilot-instructions.md`), Claude Code rules
  (`.claude/rules/`, `CLAUDE.local.md`) or a `GEMINI.md`, make sure to include
  the important parts.
- If there is a README.md, make sure to include the important parts.
- Do not make up information such as "Common Development Tasks", "Tips for
  Development", "Support and Documentation" unless this is expressly included
  in other files that you read.
- Be sure to prefix a new file with the following text:

```
# AGENTS.md

This file provides guidance to coding agents when working with code in this repository.
```

Gather what you need from the kernel (`bash("git ls-files | head -200")`,
reading the README, manifests and CI config) before writing; write the file
with an ordinary file write or `edit.run`, then tell the user what you wrote
and that the next session loads it (this one already has its instruction
files; `/memory` lists them).
