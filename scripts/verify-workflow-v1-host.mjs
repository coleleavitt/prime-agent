#!/usr/bin/env node
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { spawnSync } from "node:child_process";

const required = [
  ["prime-agent-runtime/src/rlm/workflow.py", "prime.workflow.run-agent/v1"],
  ["packages/coding-agent/src/core/workflow-v1-wire.ts", "prime.workflow.run-agent/v1"],
  ["packages/coding-agent/src/core/agent-session.ts", "\"workflow.run_agent\""],
  ["packages/agent/src/agent-loop.ts", "unexpected_tool_call"],
];
for (const [file, marker] of required) {
  if (!readFileSync(file, "utf8").includes(marker)) throw new Error(`${file} missing ${marker}`);
}

// Normative source: pi-plugin-workflow e47fd2a, docs/api/workflow-native-host-v1.schema.json.
// The committed byte-for-byte copy makes clean-checkout acceptance independent of a sibling repository.
const schemaPath = "scripts/fixtures/workflow-native-host-v1.schema.json";
const schemaBytes = readFileSync(schemaPath);
const schemaSha256 = createHash("sha256").update(schemaBytes).digest("hex");
const expectedSchemaSha256 = "08ade62e424d7dad199ca87b1a2da8eb57da71657a497f6793862fa1d73e1f6a";
if (schemaSha256 !== expectedSchemaSha256) {
  throw new Error(`${schemaPath} digest mismatch: expected ${expectedSchemaSha256}, got ${schemaSha256}`);
}
const schema = JSON.parse(schemaBytes.toString("utf8"));
const requestProperties = schema.$defs.runAgentRequest.properties;
const schemaMarkers = [
  requestProperties.protocol.const,
  schema.$defs.runAgentReply.oneOf[0].properties.protocol.const,
  String(requestProperties.maxTurns.const),
  String(requestProperties.prompt.maxLength),
  String(requestProperties.softTokenBudget.oneOf[1].maximum),
  String(requestProperties.maxResultUtf8Bytes.maximum),
  String(requestProperties.drainTimeoutMs.maximum),
  requestProperties.tools.const,
  "execution_unknown",
  "known_prefix",
];
for (const codec of ["prime-agent-runtime/src/rlm/workflow.py", "packages/coding-agent/src/core/workflow-v1-wire.ts"]) {
  const text = readFileSync(codec, "utf8").replaceAll("_", "");
  for (const marker of schemaMarkers) {
    if (!text.includes(marker.replaceAll("_", ""))) throw new Error(`${codec} does not encode normative schema marker ${marker}`);
  }
}

const oldHostCode = [
  "import asyncio",
  "from rlm.workflow import run_agent, CapabilityUnavailable",
  "r = {'protocol':'prime.workflow.run-agent/v1','requestId':'r','nodeId':'n','prompt':'x','model':None,'maxTurns':1,'maxResultUtf8Bytes':10,'drainTimeoutMs':10,'tools':'none'}",
  "async def main():",
  "  try: await run_agent(r)",
  "  except CapabilityUnavailable: return",
  "  raise SystemExit(1)",
  "asyncio.run(main())",
].join("\n");
const oldHost = spawnSync("uv", ["run", "--project", "prime-agent-runtime", "python", "-c", oldHostCode], { stdio: "inherit" });
if (oldHost.status !== 0) process.exit(oldHost.status ?? 1);
const py = spawnSync("uv", ["run", "--project", "prime-agent-runtime", "python", "-m", "unittest", "discover", "-s", "prime-agent-runtime/test", "-p", "test_workflow*.py"], { stdio: "inherit", env: { ...process.env, PRIME_AGENT_BASH_ORPHAN_JOURNAL: "", PRIME_AGENT_BASH_SHELL: "" } });
if (py.status !== 0) process.exit(py.status ?? 1);
const node = spawnSync("npm", ["test", "--workspace", "packages/coding-agent", "--", "test/workflow-v1-wire.test.ts", "test/workflow-v1-schema-conformance.test.ts", "test/run-workflow-agent.test.ts", "test/repl-kernel-abort.test.ts", "test/suite/workflow-v1-host-handler.test.ts"], { stdio: "inherit" });
if (node.status !== 0) process.exit(node.status ?? 1);
console.log("workflow-v1-host acceptance: PASS");
