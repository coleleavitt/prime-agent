import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

// Every test worker starts with a throwaway agent dir. Without this, a test that
// builds a session without an explicit agentDir resolves getAgentDir() to the
// operator's real ~/.prime/agent: it reads their global harness memories into the
// session-start digest, which changed test outcomes on any machine that had real
// memories, and it can write live state. Set unconditionally so isolation never
// depends on the caller's shell. Tests that need a specific dir still override it
// in their own setup, and the restore-previous pattern they use keeps working.
//
// This deliberately does not import src/config.ts for ENV_AGENT_DIR. A setup file
// runs before each test file's vi.mock hoisting and env setup, so importing config
// here would evaluate and cache its module-level state (package dir, executable
// detection) too early: the self-update tests then saw the vitest worker as the
// installed executable and refused to update. The name is derived from
// piConfig.name ("prime-agent" -> PRIME_AGENT); PI_ is config's fallback prefix.
const agentDir = mkdtempSync(join(tmpdir(), "prime-agent-test-agentdir-"));
process.env.PRIME_AGENT_CODING_AGENT_DIR = agentDir;
process.env.PI_CODING_AGENT_DIR = agentDir;
// Workspace Recall writes a per-repo mark on every agent_end and appends a block to the first ipython
// result of a session. Off by default under test so session tests that happen to run inside a git
// checkout stay byte-identical; the recall tests turn it back on for themselves.
process.env.PRIME_AGENT_WORKSPACE_RECALL = "0";
// The global failure ledger lives in the agent dir, which every session in one test file shares, so failures
// counted by one test would cross the recurrence threshold in the next. The ledger suites turn it back on.
process.env.PRIME_AGENT_GLOBAL_LEDGER = "0";
