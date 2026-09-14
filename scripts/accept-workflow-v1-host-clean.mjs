#!/usr/bin/env node
import { chmodSync, mkdtempSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { isAbsolute, join, resolve } from "node:path";
import { spawnSync } from "node:child_process";
import { canonicalJson, normalizeEvidence, sha256 } from "./lib/workflow-v1-evidence.mjs";

const FORMAT = "prime.workflow-v1-host.clean-acceptance/v1";
const source = resolve(new URL("..", import.meta.url).pathname);
const obsoleteExemptionMarkers = [
  ["BASE", "LINE"].join(""),
  ["baseline", "ExemptionGate"].join(""),
  ["eng-", "4603"].join(""),
  ["baseline", "Exemptions"].join(""),
  ["ACCEPTED", "_WITH_", "BASE", "LINE_EXEMPTION"].join(""),
];
const runnerSource = readFileSync(new URL(import.meta.url), "utf8");
const obsoleteHits = obsoleteExemptionMarkers.filter((marker) => runnerSource.includes(marker));
if (obsoleteHits.length !== 0) throw new Error(`obsolete exemption machinery remains: ${obsoleteHits.join(", ")}`);
const args = process.argv.slice(2);
const outputArg = args.indexOf("--output");
if (outputArg === -1 || !args[outputArg + 1]) {
  console.error("usage: node scripts/accept-workflow-v1-host-clean.mjs --output <external-directory>");
  process.exit(2);
}
const output = resolve(args[outputArg + 1]);
if (!isAbsolute(output) || output === source || output.startsWith(`${source}/`)) {
  console.error("--output must be an absolute directory outside the candidate repository");
  process.exit(2);
}
mkdirSync(output, { recursive: true });
if (readdirSync(output).length !== 0) {
  console.error("--output directory must be empty");
  process.exit(2);
}
const sandbox = mkdtempSync(join(tmpdir(), "wv1-"));
const checkout = join(sandbox, "repo");
mkdirSync(checkout);
const normalize = (value) => normalizeEvidence(value, [[checkout, "<CHECKOUT>"], [sandbox, "<SANDBOX>"], [source, "<SOURCE>"]]);
const allowedEnvironmentNames = [
  "PATH", "LANG", "LC_ALL", "LC_CTYPE", "SHELL", "TERM", "USER", "LOGNAME",
  "SSL_CERT_FILE", "SSL_CERT_DIR", "NODE_EXTRA_CA_CERTS",
];
const baseEnvironment = Object.fromEntries(
  allowedEnvironmentNames
    .filter((name) => process.env[name] !== undefined)
    .map((name) => [name, process.env[name]]),
);
const runRaw = (command, commandArgs, options = {}) => spawnSync(command, commandArgs, {
  cwd: options.cwd ?? source,
  env: { ...baseEnvironment, CI: "1", TZ: "UTC", NODE_ENV: "test", NO_COLOR: "1", FORCE_COLOR: "0", ...options.env },
  encoding: "utf8",
  maxBuffer: 256 * 1024 * 1024,
});
const head = runRaw("git", ["rev-parse", "HEAD"]);
if (head.status !== 0) throw new Error(head.stderr || "cannot resolve HEAD");
const commit = head.stdout.trim();
const status = runRaw("git", ["status", "--porcelain=v1", "--untracked-files=all"]);
if (status.status !== 0) throw new Error(status.stderr || "cannot inspect candidate status");
if (status.stdout !== "") {
  console.error("candidate repository is not clean; commit all acceptance inputs first");
  console.error(status.stdout);
  process.exit(2);
}
const archive = spawnSync("git", ["archive", "--format=tar", commit], { cwd: source, encoding: null, maxBuffer: 256 * 1024 * 1024 });
if (archive.status !== 0) throw new Error(archive.stderr?.toString() || "git archive failed");
const extract = spawnSync("tar", ["-xf", "-", "-C", checkout], { input: archive.stdout, encoding: null });
if (extract.status !== 0) throw new Error(extract.stderr?.toString() || "archive extraction failed");

const logs = join(output, "logs");
mkdirSync(logs, { recursive: true });
const results = [];
const gate = (id, command, commandArgs, options = {}) => {
  process.stdout.write(`[gate] ${id}\n`);
  const gateState = join(sandbox, "g", String(results.length + 1));
  mkdirSync(gateState, { recursive: true });
  const isolatedEnv = {
    HOME: gateState,
    XDG_CACHE_HOME: join(sandbox, "shared-cache"),
    UV_CACHE_DIR: join(sandbox, "uv-cache"),
    XDG_CONFIG_HOME: join(gateState, "config"),
    XDG_DATA_HOME: join(gateState, "data"),
    XDG_STATE_HOME: join(gateState, "state"),
    TMPDIR: join(gateState, "tmp"),
    ...options.env,
  };
  mkdirSync(isolatedEnv.TMPDIR, { recursive: true });
  const result = runRaw(command, commandArgs, { ...options, env: isolatedEnv, cwd: checkout });
  const combined = normalize(`${result.stdout ?? ""}${result.stderr ?? ""}`);
  const logName = `${String(results.length + 1).padStart(2, "0")}-${id}.log`;
  writeFileSync(join(logs, logName), combined);
  results.push({
    id,
    command: [command, ...commandArgs],
    exitCode: result.status ?? 255,
    signal: result.signal ?? null,
    log: `logs/${logName}`,
    logSha256: sha256(combined),
    artifacts: options.artifacts ? Object.fromEntries(options.artifacts.map((artifact) => {
      const path = join(output, artifact);
      return [artifact, sha256(readFileSync(path))];
    })) : undefined,
  });
  return result.status === 0;
};
const acceptancePolicyCode = String.raw`
import { readFileSync } from "node:fs";
const files = ["scripts/accept-workflow-v1-host-clean.mjs", "scripts/verify-python-runtime-isolated.mjs"];
const forbidden = [
  ["BASE", "LINE_EXEMPTION"].join(""),
  ["ACCEPTED_WITH_BASE", "LINE_EXEMPTION"].join(""),
  ["baseline", "Exemption"].join(""),
  ["baseline", "Exemptions"].join(""),
];
const violations = [];
for (const file of files) {
  const text = readFileSync(file, "utf8");
  for (const marker of forbidden) if (text.includes(marker)) violations.push(file + ": " + marker);
}
if (violations.length) { console.error(violations.join("\n")); process.exit(1); }
console.log(JSON.stringify({ filesScanned: files.length, forbiddenMatches: 0 }));
`;

const oldHostCode = [
  "import asyncio",
  "from rlm.workflow import run_agent, CapabilityUnavailable",
  "r={'protocol':'prime.workflow.run-agent/v1','requestId':'old-host','nodeId':'n','prompt':'x','model':None,'maxTurns':1,'maxResultUtf8Bytes':10,'drainTimeoutMs':10,'tools':'none'}",
  "async def main():",
  "  try: await run_agent(r)",
  "  except CapabilityUnavailable: return",
  "  raise SystemExit('old host unexpectedly admitted workflow.run_agent')",
  "asyncio.run(main())",
].join("\n");
const scanCode = String.raw`
import { readFileSync, readdirSync, statSync } from "node:fs";
import { join } from "node:path";
const walk = (directory) => readdirSync(directory).flatMap((name) => {
  const path = join(directory, name);
  return statSync(path).isDirectory() ? walk(path) : [path.replace(/^\.\//, "")];
});
const relevant = [
  "packages/agent/src/agent-loop.ts",
  "packages/agent/src/agent.ts",
  "packages/agent/src/types.ts",
  "packages/coding-agent/src/core/agent-session.ts",
  "packages/coding-agent/src/core/kernel/bootstrap.ts",
  "packages/coding-agent/src/core/kernel/repl-manager.ts",
  "packages/coding-agent/src/core/kernel/shared.ts",
  "packages/coding-agent/src/core/run-workflow-agent.ts",
  "packages/coding-agent/src/core/workflow-v1-wire.ts",
  "prime-agent-runtime/src/rlm/__init__.py",
  "prime-agent-runtime/src/rlm/repl.py",
  "prime-agent-runtime/src/rlm/workflow.py",
];
const forbidden = [];
for (const file of relevant) {
  const text = readFileSync(file, "utf8");
  if (/\bnew\s+Function\s*\(/.test(text)) forbidden.push(file + ": new Function");
  if (/(?:^|["'\s=])\/(?:home|tmp)\//m.test(text)) forbidden.push(file + ": absolute host path");
}
if (forbidden.length) { console.error(forbidden.sort().join("\n")); process.exit(1); }
console.log(JSON.stringify({ filesScanned: relevant.length, forbiddenMatches: 0 }));
`;

let keepGoing = true;
const execute = (id, command, commandArgs, options) => {
  if (!keepGoing) return;
  keepGoing = gate(id, command, commandArgs, options);
};
execute("archive-tree-integrity", "node", ["-e", `
  const {execFileSync}=require("node:child_process");
  const fs=require("node:fs");
  if(fs.existsSync(".git")) throw new Error("archive unexpectedly contains .git");
  const names=execFileSync("find",[".","-type","l","-print"],{encoding:"utf8"}).trim();
  console.log(JSON.stringify({gitMetadata:false, symlinks:names ? names.split("\\n").length : 0}));
`]);
execute("forbidden-source-scan", "node", ["--input-type=module", "-e", scanCode]);
execute("acceptance-policy-self-test", "node", ["--input-type=module", "-e", acceptancePolicyCode]);
execute("install-locked", "npm", ["ci", "--ignore-scripts"]);
execute("build", "npm", ["run", "build"]);
execute("repository-host-verifier", "node", ["scripts/verify-workflow-v1-host.mjs"]);
execute("schema-wire-focused", "npm", ["exec", "--workspace", "@earendil-works/pi-coding-agent", "--", "vitest", "--run", "test/workflow-v1-schema-conformance.test.ts", "test/workflow-v1-wire.test.ts"]);
execute("object-route-race-focused", "npm", ["exec", "--workspace", "@earendil-works/pi-coding-agent", "--", "vitest", "--run", "test/run-workflow-agent.test.ts", "test/repl-kernel-abort.test.ts", "test/suite/workflow-v1-host-handler.test.ts"]);
execute("agent-policy-focused", "npm", ["exec", "--workspace", "@earendil-works/pi-agent-core", "--", "vitest", "--run", "test/tool-call-policy.test.ts"]);
execute("python-runtime-isolated", "node", ["scripts/verify-python-runtime-isolated.mjs", "--output", join(output, "python-isolated")], { artifacts: ["python-isolated/python-isolated.json", "python-isolated/python-isolated.json.sha256"] });
execute("old-host-negative", "uv", ["run", "--project", "prime-agent-runtime", "python", "-c", oldHostCode], { env: { PRIME_AGENT_BASH_ORPHAN_JOURNAL: "", PRIME_AGENT_BASH_SHELL: "" } });
execute("recursion-file-isolated", "npm", ["exec", "--workspace", "@earendil-works/pi-coding-agent", "--", "vitest", "--run", "test/agent-session-recursion.test.ts", "--reporter=dot"]);
execute("npm-native-bridge-isolated", "npm", ["exec", "--workspace", "@earendil-works/pi-coding-agent", "--", "vitest", "--run", "test/npm-native-bridge.test.ts", "--reporter=verbose"]);
execute("eng-4600-isolated", "npm", ["exec", "--workspace", "@earendil-works/pi-coding-agent", "--", "vitest", "--run", "test/suite/regressions/4600-supervisor-singleton.test.ts", "--reporter=verbose"]);
execute("eng-4606-isolated", "npm", ["exec", "--workspace", "@earendil-works/pi-coding-agent", "--", "vitest", "--run", "test/suite/regressions/4606-update-restart-coordinator.test.ts", "--reporter=verbose"]);
execute("agent-full", "npm", ["test", "--workspace", "@earendil-works/pi-agent-core"]);
execute("root-typecheck", "npm", ["exec", "--", "tsgo", "--noEmit"]);
execute("root-check", "npm", ["run", "check"]);

const manifest = {
  format: FORMAT,
  candidate: { commit, tree: runRaw("git", ["rev-parse", `${commit}^{tree}`]).stdout.trim(), source: "git archive" },
  verdict: results.length === 18 && results.every((result) => result.exitCode === 0) ? "PASS" : "FAIL",
  gates: results,
};
const canonical = canonicalJson(manifest);
writeFileSync(join(output, "acceptance.json"), canonical);
writeFileSync(join(output, "acceptance.json.sha256"), `${sha256(canonical)}  acceptance.json\n`);
const digestLines = results.map((result) => `${result.logSha256}  ${result.log}`).join("\n") + "\n";
writeFileSync(join(output, "logs.sha256"), digestLines);
chmodSync(join(output, "acceptance.json"), 0o444);
rmSync(sandbox, { recursive: true, force: true });
console.log(`workflow-v1-host clean acceptance: ${manifest.verdict}`);
console.log(`evidence: ${output}`);
process.exit(manifest.verdict === "FAIL" ? 1 : 0);
