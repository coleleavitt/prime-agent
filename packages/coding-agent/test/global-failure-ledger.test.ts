import { spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import type { AgentTool } from "@earendil-works/pi-agent-core";
import { type Context, fauxAssistantMessage, fauxToolCall } from "@earendil-works/pi-ai";
import { Type } from "typebox";
import { afterEach, describe, expect, it, vi } from "vitest";
import { ravoArtifactDigest } from "../src/core/ravo/authority.js";
import {
	emptyFailureLedger,
	type FailureLedger,
	fingerprintFailure,
	GLOBAL_FAILURE_LEDGER_ENV,
	recurringFailures,
} from "../src/core/ravo/failure-ledger.js";
import {
	getGlobalHarnessStateDir,
	getLocalHarnessStateDir,
	loadHarnessState,
	refinementBaselineView,
} from "../src/core/refinement/index.js";
import { createHarness, type Harness } from "./suite/harness.js";

const FLAKY_ERROR = "cannot connect to db host at port 5432 (attempt 1)";
// Digits normalize to "#", so every session raising this tool error fingerprints identically.
const FINGERPRINT_ID = fingerprintFailure("tool_error", "flaky", undefined, FLAKY_ERROR).id;

const FAILURE_LEDGER_MODULE = resolve(__dirname, "../src/core/ravo/failure-ledger.js");
const REFINEMENT_MODULE = resolve(__dirname, "../src/core/refinement/refinement.js");
const TSX_TSCONFIG_PATH = resolve(__dirname, "../../../tsconfig.json");

/**
 * Child-process fixture: one read-modify-write of the global failure ledger per
 * round, either through `withHarnessStateLock` or deliberately without it. Both
 * children wait on a start barrier and hold each read open for `holdMs`, so the
 * two critical sections genuinely overlap rather than happening to interleave
 * cleanly. In unlocked mode they additionally rendezvous inside the read, which
 * makes the lost update deterministic rather than merely likely.
 */
const FLUSH_FIXTURE = `
import { existsSync, writeFileSync } from "node:fs";
import { emptyFailureLedger, fingerprintFailure, mergeFailureObservations } from ${JSON.stringify(FAILURE_LEDGER_MODULE)};
import { loadHarnessState, saveHarnessState, withHarnessStateLock } from ${JSON.stringify(REFINEMENT_MODULE)};

const [, , dir, label, sibling, mode, roundsRaw, holdRaw] = process.argv;
const rounds = Number(roundsRaw);
const holdMs = Number(holdRaw);
const fingerprint = fingerprintFailure("tool_error", label, undefined, "concurrent flush by " + label);
const sleep = (ms) => Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, ms);

function waitFor(marker) {
	const deadline = Date.now() + 20000;
	while (!existsSync(dir + "/" + marker) && Date.now() < deadline) sleep(1);
}

function commit(round) {
	const state = loadHarnessState(dir, "global");
	if (mode === "unlocked") {
		writeFileSync(dir + "/" + label + ".holding", "1");
		waitFor(sibling + ".holding");
	}
	sleep(holdMs);
	const merged = mergeFailureObservations(state.failures ?? emptyFailureLedger(), [
		{ fingerprint, excerpt: label, entryIndex: round, turn: round, at: new Date().toISOString() },
	]);
	state.failures = merged.ledger;
	saveHarnessState(dir, state);
}

writeFileSync(dir + "/" + label + ".started", "1");
waitFor(sibling + ".started");
for (let round = 0; round < rounds; round++) {
	if (mode === "locked") withHarnessStateLock(dir, () => commit(round));
	else commit(round);
}
`;

function flakyTool(): AgentTool {
	return {
		name: "flaky",
		label: "Flaky",
		description: "Always fails",
		parameters: Type.Object({}),
		execute: async () => {
			throw new Error(FLAKY_ERROR);
		},
	};
}

function userPromptText(context: Context): string {
	return context.messages
		.flatMap((message) => {
			if (message.role !== "user") return [];
			return typeof message.content === "string"
				? [message.content]
				: message.content.map((part) => (part.type === "text" ? part.text : ""));
		})
		.join("\n");
}

function emptyPlan(): string {
	return JSON.stringify({ summary: "noop", rationale: "none", expectedOutcome: "none", edits: [] });
}

function runFlusher(
	script: string,
	dir: string,
	label: string,
	sibling: string,
	mode: string,
	rounds: number,
	holdMs: number,
) {
	return new Promise<void>((resolvePromise, reject) => {
		const child = spawn(
			process.execPath,
			["--import", "tsx", script, dir, label, sibling, mode, String(rounds), String(holdMs)],
			{ env: { ...process.env, TSX_TSCONFIG_PATH }, stdio: ["ignore", "ignore", "pipe"] },
		);
		let stderr = "";
		child.stderr.on("data", (chunk) => {
			stderr += chunk.toString();
		});
		child.once("error", reject);
		child.once("close", (code) => {
			if (code === 0) resolvePromise();
			else reject(new Error(`flusher ${label} exited ${code}: ${stderr}`));
		});
	});
}

describe("global failure ledger", () => {
	const harnesses: Harness[] = [];
	const tempDirs: string[] = [];
	const previousAgentDir = process.env.PRIME_AGENT_CODING_AGENT_DIR;
	const previousFlag = process.env[GLOBAL_FAILURE_LEDGER_ENV];

	afterEach(() => {
		while (harnesses.length > 0) harnesses.pop()?.cleanup();
		while (tempDirs.length > 0) rmSync(tempDirs.pop()!, { recursive: true, force: true });
		if (previousAgentDir === undefined) delete process.env.PRIME_AGENT_CODING_AGENT_DIR;
		else process.env.PRIME_AGENT_CODING_AGENT_DIR = previousAgentDir;
		if (previousFlag === undefined) delete process.env[GLOBAL_FAILURE_LEDGER_ENV];
		else process.env[GLOBAL_FAILURE_LEDGER_ENV] = previousFlag;
	});

	/** A shared agent dir stands in for "the same machine" across the two replayed sessions. */
	function useSharedAgentDir(): string {
		const dir = mkdtempSync(join(tmpdir(), "prime-agent-global-ledger-"));
		tempDirs.push(dir);
		process.env.PRIME_AGENT_CODING_AGENT_DIR = dir;
		return dir;
	}

	/** Replay one archived session: a single tool failure, observed exactly once. */
	async function replaySession(): Promise<{ harness: Harness; plannerPrompts: string[] }> {
		const harness = await createHarness({
			persistSession: true,
			rlmDepth: 0,
			tools: [flakyTool()],
			settings: { autoRefine: { enabled: true, turnInterval: 25, cooldownMs: 20 * 60_000 } },
		});
		harnesses.push(harness);
		const plannerPrompts: string[] = [];
		harness.setResponses([
			fauxAssistantMessage(fauxToolCall("flaky", {}), { stopReason: "toolUse" }),
			fauxAssistantMessage("done"),
			(context) => {
				plannerPrompts.push(userPromptText(context));
				return fauxAssistantMessage(emptyPlan());
			},
		]);
		await harness.session.prompt("go");
		await harness.session.waitForIdle();
		return { harness, plannerPrompts };
	}

	function globalLedger(): FailureLedger | undefined {
		return loadHarnessState(getGlobalHarnessStateDir(), "global").failures;
	}

	function localLedger(harness: Harness): FailureLedger {
		const dir = getLocalHarnessStateDir(harness.sessionManager.getSessionArtifactDir())!;
		return loadHarnessState(dir, "local").failures ?? emptyFailureLedger();
	}

	it("keeps the ledger per-session with the flag off: one observation each, no recurrence, no refine", async () => {
		useSharedAgentDir();
		delete process.env[GLOBAL_FAILURE_LEDGER_ENV];

		const first = await replaySession();
		const second = await replaySession();

		expect(globalLedger()).toBeUndefined();
		for (const session of [first, second]) {
			const ledger = localLedger(session.harness);
			expect(ledger.failures[FINGERPRINT_ID]?.count).toBe(1);
			expect(recurringFailures(ledger).map((record) => record.fingerprint.id)).not.toContain(FINGERPRINT_ID);
			expect(session.plannerPrompts).toHaveLength(0);
			expect(session.harness.eventsOfType("refine_complete")).toHaveLength(0);
		}
	}, 60_000);

	it("carries the ledger across sessions with the flag on: session B recurs at count 2 and queues a refine", async () => {
		useSharedAgentDir();
		process.env[GLOBAL_FAILURE_LEDGER_ENV] = "1";

		const first = await replaySession();
		expect(globalLedger()?.failures[FINGERPRINT_ID]?.count).toBe(1);
		expect(recurringFailures(globalLedger() ?? emptyFailureLedger())).toHaveLength(0);
		expect(first.plannerPrompts).toHaveLength(0);
		expect(first.harness.eventsOfType("refine_complete")).toHaveLength(0);

		const second = await replaySession();
		await vi.waitFor(() => expect(second.harness.eventsOfType("refine_complete")).toHaveLength(1), {
			timeout: 10_000,
		});

		const ledger = globalLedger() ?? emptyFailureLedger();
		expect(ledger.failures[FINGERPRINT_ID]?.count).toBe(2);
		expect(recurringFailures(ledger).map((record) => record.fingerprint.id)).toContain(FINGERPRINT_ID);
		// The local ledger stays per-session: it owns the scan cursor, not the count.
		expect(localLedger(second.harness).failures[FINGERPRINT_ID]?.count).toBe(1);
		expect(second.plannerPrompts).toHaveLength(1);
		expect(second.plannerPrompts[0]).toContain("Automatic refine triggered by recurrence");
		expect(second.plannerPrompts[0]).toContain(`failure:${FINGERPRINT_ID}`);
		expect(second.plannerPrompts[0]).toContain("count=2");
	}, 60_000);

	it("keeps two concurrently flushing processes from losing each other's records", async () => {
		const agentDir = useSharedAgentDir();
		const globalDir = getGlobalHarnessStateDir();
		mkdirSync(globalDir, { recursive: true });
		const script = join(agentDir, "flush-fixture.ts");
		writeFileSync(script, FLUSH_FIXTURE, "utf8");
		const alpha = fingerprintFailure("tool_error", "alpha", undefined, "concurrent flush by alpha").id;
		const beta = fingerprintFailure("tool_error", "beta", undefined, "concurrent flush by beta").id;

		await Promise.all([
			runFlusher(script, globalDir, "alpha", "beta", "locked", 10, 15),
			runFlusher(script, globalDir, "beta", "alpha", "locked", 10, 15),
		]);

		const ledger = globalLedger() ?? emptyFailureLedger();
		expect(ledger.failures[alpha]?.count).toBe(10);
		expect(ledger.failures[beta]?.count).toBe(10);
	}, 120_000);

	it("loses a record when the same read-modify-write runs without the harness state lock", async () => {
		const agentDir = useSharedAgentDir();
		const globalDir = getGlobalHarnessStateDir();
		mkdirSync(globalDir, { recursive: true });
		const script = join(agentDir, "flush-fixture.ts");
		writeFileSync(script, FLUSH_FIXTURE, "utf8");

		await Promise.all([
			runFlusher(script, globalDir, "alpha", "beta", "unlocked", 1, 15),
			runFlusher(script, globalDir, "beta", "alpha", "unlocked", 1, 15),
		]);

		// Both children read the same empty ledger before either wrote; the second
		// write wins outright, which is exactly what the lock exists to prevent.
		expect(Object.keys((globalLedger() ?? emptyFailureLedger()).failures)).toHaveLength(1);
	}, 120_000);

	it("keeps the failure ledger out of the baseline a RAVO certificate binds", () => {
		const state = loadHarnessState(join(tmpdir(), "prime-agent-global-ledger-absent"), "global");
		const before = ravoArtifactDigest(refinementBaselineView(state));

		state.failures = {
			schema: 1,
			failures: {
				[FINGERPRINT_ID]: {
					fingerprint: fingerprintFailure("tool_error", "flaky", undefined, FLAKY_ERROR),
					count: 3,
					firstSeenTurn: 1,
					lastSeenTurn: 5,
					firstSeenAt: "2026-01-01T00:00:00.000Z",
					lastSeenAt: "2026-01-01T00:05:00.000Z",
					excerpt: FLAKY_ERROR,
					addressedByProposalIds: [],
				},
			},
			lastScannedEntryIndex: 9,
		};
		expect(ravoArtifactDigest(refinementBaselineView(state))).toBe(before);

		state.schema = 2;
		expect(ravoArtifactDigest(refinementBaselineView(state))).not.toBe(before);
	});
});
