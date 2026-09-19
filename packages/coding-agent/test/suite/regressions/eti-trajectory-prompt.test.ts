import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { ENV_AGENT_DIR } from "../../../src/config.js";
import {
	type TrajectoryLabel,
	type TrajectoryStoreFile,
	type TrajectoryWindow,
	writeTrajectoryIndex,
} from "../../../src/core/distill/trajectory-index.js";
import {
	formatHarnessStateForPrompt,
	type HarnessEntry,
	type HarnessQueryTerms,
	type HarnessState,
} from "../../../src/core/refinement/index.js";
import { createHarness, type Harness } from "../harness.js";

/**
 * Engineer Trajectory Index (ETI) prompt-lane integration. Lever 1 biases which
 * harness entries win the rendered slots and surfaces up to three confound-flagged
 * stable-gap lines; Lever 2 mutes the auto-refine recurrence reminder for a
 * fingerprint the engineer has internalized, but never for a security/credential
 * class one, and never against a live recurrence. Nothing is added when the index
 * is absent or the kill switch is off.
 */

function memoryEntry(id: string, title: string, content: string): HarnessEntry {
	const timestamp = "2026-09-01T09:00:00.000Z";
	return {
		id,
		kind: "memory",
		title,
		content,
		path: `memories/${id}`,
		scope: "global",
		reference: {},
		arguments: {},
		metadata: {},
		source: "agent",
		created_at: timestamp,
		updated_at: timestamp,
		version: 1,
	};
}

/**
 * Eight live memory entries: one high-relevance entry that wins a slot by query
 * score alone, six mid-relevance entries, and one entry with no query overlap
 * that never reaches the slice on relevance. The trajectory bias reorders which
 * six survive.
 */
function seededState(): HarnessState {
	const state: HarnessState = {
		schema: 1,
		entries: { prompt: {}, memory: {}, skill: {}, subagent: {} },
		refinements: [],
	};
	// High relevance: the query term appears in both title and content.
	state.entries.memory.e_high = memoryEntry(
		"e_high",
		"worktree branch layout",
		"How to lay out a worktree for parallel worktree work.",
	);
	for (let index = 0; index < 6; index++) {
		state.entries.memory[`e_mid_${index}`] = memoryEntry(
			`e_mid_${index}`,
			`worktree note ${index}`,
			`Mid-relevance note ${index}.`,
		);
	}
	// No query overlap: excluded from the rendered slots on relevance alone.
	state.entries.memory.e_low = memoryEntry("e_low", "git hygiene reminder", "Commit incrementally.");
	return state;
}

function renderedEntryIds(prompt: string): string[] {
	const ids: string[] = [];
	for (const line of prompt.split("\n")) {
		const match = line.match(/^- \[global:([^\]]+)\]/);
		if (match) ids.push(match[1]);
	}
	return ids;
}

const QUERY: HarnessQueryTerms = new Map([["worktree", 3]]);

describe("ETI Lever 1 (surface + rerank) in formatHarnessStateForPrompt", () => {
	it("promotes a stable-gap entry into the slots and sinks an internalized one below them", () => {
		const state = seededState();

		// Baseline: by relevance the high-overlap entry wins a slot and the
		// no-overlap entry does not.
		const baseline = renderedEntryIds(formatHarnessStateForPrompt(state, { queryTerms: QUERY }));
		expect(baseline).toContain("e_high");
		expect(baseline).not.toContain("e_low");

		// With the bias, the internalized entry sinks below the unlabelled ones
		// (past the slice) and the stable-gap entry is surfaced.
		const classOf = new Map<string, "stable-gap" | "new" | "internalized">([
			["e_low", "stable-gap"],
			["e_high", "internalized"],
		]);
		const biased = renderedEntryIds(
			formatHarnessStateForPrompt(state, { queryTerms: QUERY, trajectory: { classOf, lines: [] } }),
		);
		expect(biased).toContain("e_low");
		expect(biased).not.toContain("e_high");
		expect(biased).toHaveLength(6);
	});

	it("renders up to three confound-flagged trajectory lines, each sanitized to one line", () => {
		const state = seededState();
		const lines = [
			"stable-gap: git-hygiene recurs in 3 of 4 windows [confounds: task-mix, tool-surface]",
			"stable-gap: verify-before-asserting recurs in 2 of 4 windows\nwith an injected <script>alert(1)</script> newline [confounds: task-mix]",
			"stable-gap: credential-hygiene recurs in 2 of 4 windows [confounds: task-mix]",
			"stable-gap: a fourth line that must be dropped [confounds: task-mix]",
		];
		const prompt = formatHarnessStateForPrompt(state, {
			queryTerms: QUERY,
			trajectory: { classOf: new Map(), lines },
		});

		expect(prompt).toContain("engineer trajectory (confound-flagged; local signal, may reflect task-mix):");
		expect(prompt).toContain(
			"- stable-gap: git-hygiene recurs in 3 of 4 windows [confounds: task-mix, tool-surface]",
		);
		// Injected angle brackets are escaped and the newline is collapsed onto one line.
		expect(prompt).toContain("&lt;script&gt;alert(1)&lt;/script&gt;");
		expect(prompt).not.toContain("<script>");
		// At most three lines survive.
		const trajectoryLines = prompt.split("\n").filter((line) => line.startsWith("- stable-gap:"));
		expect(trajectoryLines).toHaveLength(3);
		expect(prompt).not.toContain("a fourth line that must be dropped");
	});

	it("adds nothing and leaves ordering untouched when no trajectory or an empty one is passed", () => {
		const state = seededState();
		const plain = formatHarnessStateForPrompt(state, { queryTerms: QUERY });
		const empty = formatHarnessStateForPrompt(state, {
			queryTerms: QUERY,
			trajectory: { classOf: new Map(), lines: [] },
		});
		expect(plain).not.toContain("engineer trajectory");
		// An empty trajectory option is byte-identical to passing none.
		expect(empty).toBe(plain);
	});
});

describe("ETI Lever 2 (recurrence-reminder suppression) via the trajectory index", () => {
	const harnesses: Harness[] = [];
	let agentDir: string | undefined;
	let previousAgentDir: string | undefined;
	let previousKillSwitch: string | undefined;

	beforeEach(() => {
		previousAgentDir = process.env[ENV_AGENT_DIR];
		previousKillSwitch = process.env.PRIME_AGENT_TRAJECTORY_INDEX;
		agentDir = mkdtempSync(join(tmpdir(), "prime-agent-eti-prompt-"));
		process.env[ENV_AGENT_DIR] = agentDir;
		delete process.env.PRIME_AGENT_TRAJECTORY_INDEX;
	});

	afterEach(() => {
		while (harnesses.length > 0) harnesses.pop()?.cleanup();
		if (previousAgentDir === undefined) delete process.env[ENV_AGENT_DIR];
		else process.env[ENV_AGENT_DIR] = previousAgentDir;
		if (previousKillSwitch === undefined) delete process.env.PRIME_AGENT_TRAJECTORY_INDEX;
		else process.env.PRIME_AGENT_TRAJECTORY_INDEX = previousKillSwitch;
		if (agentDir) rmSync(agentDir, { recursive: true, force: true });
		agentDir = undefined;
	});

	function droppedLabel(fingerprint: string, name: string, securityClass: boolean): TrajectoryLabel {
		return {
			fingerprint,
			name,
			message: name,
			corpus: "prime",
			label: "dropped",
			sinceWindow: "2026-W20",
			lastWindow: "2026-W21",
			windowsPresent: 2,
			windowsRecurring: 2,
			claimedByRefinement: false,
			domainActive: true,
			securityClass,
			confounds: ["task-mix"],
		};
	}

	function window(name: string): TrajectoryWindow {
		return {
			schema: 1,
			window: name,
			sealedAt: "2026-09-01T00:00:00.000Z",
			days: [`${name}-day`],
			turns: 10,
			corpus: "prime",
			fingerprints: [],
		};
	}

	function sealIndex(labels: TrajectoryLabel[]): void {
		const file: TrajectoryStoreFile = {
			version: 1,
			sealedAt: "2026-09-01T00:00:00.000Z",
			windowsObserved: 4,
			minWindows: 4,
			windows: ["2026-W20", "2026-W21", "2026-W22", "2026-W23"].map(window),
			labels,
			rate: [],
		};
		writeTrajectoryIndex(file);
	}

	interface Lever2Internals {
		_trajectoryInternalizedReminders(liveRecurringIds: Iterable<string>): Set<string>;
	}

	async function session(): Promise<Lever2Internals> {
		const harness = await createHarness();
		harnesses.push(harness);
		return harness.session as unknown as Lever2Internals;
	}

	it("suppresses an internalized fingerprint but never a security-class one", async () => {
		sealIndex([
			droppedLabel("a1b2c3d4e5f60718", "tool_error: git push rejected", false),
			droppedLabel("00ffeeddccbbaa99", "auth token refresh failed", true),
		]);
		const reminders = (await session())._trajectoryInternalizedReminders([]);
		expect(reminders.has("a1b2c3d4e5f60718")).toBe(true);
		// The credential-class fingerprint is excluded from the suppression set, so
		// its recurrence reminder always fires (fail closed on security hygiene).
		expect(reminders.has("00ffeeddccbbaa99")).toBe(false);
	});

	it("honours the live-recurrence override so a fresh recurrence outranks a stale DROP", async () => {
		sealIndex([droppedLabel("a1b2c3d4e5f60718", "tool_error: git push rejected", false)]);
		const internals = await session();
		expect(internals._trajectoryInternalizedReminders([]).has("a1b2c3d4e5f60718")).toBe(true);
		// Observed live in this session's ledger this turn: the DROP is stale.
		expect(internals._trajectoryInternalizedReminders(["a1b2c3d4e5f60718"]).has("a1b2c3d4e5f60718")).toBe(false);
	});

	it("suppresses nothing when the kill switch is off", async () => {
		sealIndex([droppedLabel("a1b2c3d4e5f60718", "tool_error: git push rejected", false)]);
		process.env.PRIME_AGENT_TRAJECTORY_INDEX = "0";
		expect((await session())._trajectoryInternalizedReminders([]).size).toBe(0);
	});

	it("suppresses nothing when no index has been sealed", async () => {
		// No sealIndex() call: the store file is absent.
		expect((await session())._trajectoryInternalizedReminders([]).size).toBe(0);
	});
});
