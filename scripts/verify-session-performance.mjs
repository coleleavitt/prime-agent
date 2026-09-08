import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { spawnSync } from "node:child_process";

const root = resolve(import.meta.dirname, "..");
const codingAgent = resolve(root, "packages/coding-agent");
const cleanEnv = { ...process.env };
delete cleanEnv.RLM_MAX_DEPTH;

function requireInvariant(condition, message) {
	if (!condition) throw new Error(`Session performance invariant failed: ${message}`);
}

function run(label, command, args, cwd = root) {
	console.log(`\n== ${label} ==`);
	const result = spawnSync(command, args, { cwd, env: cleanEnv, stdio: "inherit" });
	if (result.error) throw result.error;
	if (result.status !== 0) throw new Error(`${label} failed with exit code ${result.status}`);
}

const agentsView = readFileSync(
	resolve(codingAgent, "src/modes/agents-view/agents-view-mode.ts"),
	"utf8",
);
const refreshStart = agentsView.indexOf("private async refreshSavedSessions");
const refreshEnd = agentsView.indexOf("private async refreshHeartbeats", refreshStart);
requireInvariant(refreshStart >= 0 && refreshEnd > refreshStart, "saved-session refresh block must be discoverable");
const refreshBlock = agentsView.slice(refreshStart, refreshEnd);
requireInvariant(!refreshBlock.includes("const onSession"), "saved sessions must not reconcile per progress item");
requireInvariant(
	refreshBlock.match(/this\.reconcileCatalogs\(\)/g)?.length === 2,
	"refresh must have one success reconcile and one mutually exclusive failure rollback",
);

const resolver = readFileSync(resolve(codingAgent, "src/core/session-resolver.ts"), "utf8");
requireInvariant(!resolver.includes("SessionManager.list("), "resume must not scan the flat catalog twice");
requireInvariant(
	resolver.match(/SessionManager\.listAll\(/g)?.length === 1,
	"resume must perform exactly one flat-catalog scan",
);

const performanceTests = [
	"test/agents-view-mode.test.ts",
	"test/agents-view-usage-layout.test.ts",
	"test/session-manager-list.test.ts",
	"test/session-view-search.test.ts",
	"test/suite/regressions/4722-invalid-resume-selector.test.ts",
];
const compatibilityTests = [
	"test/saved-session-catalog.test.ts",
	"test/daemon-client.test.ts",
	"test/agent-connection-daemon.test.ts",
];
const vitest = resolve(root, "node_modules/vitest/dist/cli.js");
const tsx = resolve(root, "node_modules/tsx/dist/cli.mjs");
run("performance and resume regressions", process.execPath, [tsx, vitest, "--run", ...performanceTests], codingAgent);
run("daemon compatibility", process.execPath, [tsx, vitest, "--run", ...compatibilityTests], codingAgent);
run("repository check", process.platform === "win32" ? "npm.cmd" : "npm", ["run", "check"]);
console.log("\nSession performance verification passed.");
