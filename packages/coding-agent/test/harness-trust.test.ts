import { execFileSync } from "node:child_process";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import type { FailureRecord } from "../src/core/ravo/failure-ledger.js";
import { type RefereeVerdict, type ReplayCase, refereeVerdict, verdictFromOutcome } from "../src/core/ravo/referee.js";
import { adjudicateFailureClaims } from "../src/core/ravo/referee-runner.js";
import {
	CLEAN_WINDOW_CREDIT,
	DEFAULT_ENTRY_TRUST,
	DORMANT_TRUST_THRESHOLD,
	entryTrustScore,
	faultIsStrictlyWorseThanCleanWindow,
	MAX_ENTRY_TRUST,
	MEASURED_FAULT_DEBIT,
	MIN_ENTRY_TRUST,
	normalizeEntryTrust,
} from "../src/core/refinement/harness-trust.js";
import {
	applyRefinementProposal,
	formatHarnessStateForPrompt,
	type HarnessEntry,
	type HarnessState,
	loadHarnessState,
	type RefinementProposal,
	saveHarnessState,
	settleHarnessTrust,
} from "../src/core/refinement/refinement.js";

/**
 * M6 exit test. Trust only ever moves on a MEASURED fault: a referee verdict
 * produced by re-executing a recorded replay case in a subprocess. The debit is
 * charged only to the entry a refinement wrote while the gate accepted it as
 * addressing that fingerprint — the link recorded in `trustWindows`.
 *
 * The seeded state mirrors the real 27-entry global harness state (27 memories,
 * three crowded paths holding 22 of them) with invented content, plus the one
 * skill entry the claim is attributed to. The live file is never read.
 */

const FINGERPRINT = "ca2e5aceb78edc4b";
const OTHER_FINGERPRINT = "ca578aff209c6a3c";
const SKILL_ID = "paramiko_transport_probe";

/** A replay case that genuinely raises under an isolated interpreter. */
const MISSING_MODULE_CASE: ReplayCase = {
	language: "python",
	source: "import prime_agent_m6_absent_module",
	exceptionClass: "ModuleNotFoundError",
	verifiedAt: "2026-09-14T08:00:00.000Z",
};

/** A replay case that runs clean: the failure it was recorded for has stopped. */
const RESOLVED_CASE: ReplayCase = {
	language: "python",
	source: "import json",
	exceptionClass: "ModuleNotFoundError",
	verifiedAt: "2026-09-14T08:00:00.000Z",
};

interface SeedEntry {
	id: string;
	title: string;
	path: string;
	updated: string;
}

const SEED_ENTRIES: SeedEntry[] = [
	{
		id: "rust_trait_objects",
		title: "Rust trait object dispatch",
		path: "guides/rust-reference",
		updated: "2026-07-02",
	},
	{
		id: "version_control",
		title: "Commit incrementally, push on request",
		path: "preferences/version-control",
		updated: "2026-06-11",
	},
	{
		id: "ers_new_entity_a",
		title: "New entity: Invoice",
		path: "projects/ers-rs/new-entities",
		updated: "2026-08-04",
	},
	{
		id: "ers_new_entity_b",
		title: "New entity: Shipment",
		path: "projects/ers-rs/new-entities",
		updated: "2026-08-09",
	},
	{
		id: "ers_new_entity_c",
		title: "New entity: Consignee",
		path: "projects/ers-rs/new-entities",
		updated: "2026-08-14",
	},
	{
		id: "ers_new_entity_d",
		title: "New entity: Manifest",
		path: "projects/ers-rs/new-entities",
		updated: "2026-08-21",
	},
	{ id: "ers_kernel_venv", title: "ers-rs kernel venv location", path: "projects/kernel/venv", updated: "2026-09-02" },
	{ id: "ledger_schema", title: "Ledger schema revision", path: "projects/ledger/schema", updated: "2026-08-16" },
	{ id: "norm_cache_a", title: "Normalizer cache key", path: "projects/normalizer/cache", updated: "2026-09-04" },
	{ id: "norm_cache_b", title: "Normalizer cache eviction", path: "projects/normalizer/cache", updated: "2026-09-06" },
	{ id: "norm_cache_c", title: "Normalizer cache warmup", path: "projects/normalizer/cache", updated: "2026-09-08" },
	{ id: "norm_cache_d", title: "Normalizer cache metrics", path: "projects/normalizer/cache", updated: "2026-09-10" },
	{
		id: "obscura_baseline",
		title: "obscura baseline: SSL_CERT_DIR root cause",
		path: "projects/obscura/baseline",
		updated: "2026-09-15",
	},
];

for (let index = 0; index < 14; index++) {
	SEED_ENTRIES.push({
		id: `ers_journal_${String(index).padStart(2, "0")}`,
		title: `ers-rs work journal ${index}`,
		path: "projects/ers-rs/work-journal",
		updated: `2026-08-${String(5 + index).padStart(2, "0")}`,
	});
}

function seedEntry(seed: SeedEntry): HarnessEntry {
	const timestamp = `${seed.updated}T09:00:00.000Z`;
	return {
		id: seed.id,
		kind: "memory",
		title: seed.title,
		content: `Invented content for ${seed.id}.`,
		path: seed.path,
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

function seedGlobalHarnessState(): HarnessState {
	const state: HarnessState = {
		schema: 1,
		entries: { prompt: {}, memory: {}, skill: {}, subagent: {} },
		refinements: [],
	};
	for (const seed of SEED_ENTRIES) {
		state.entries.memory[seed.id] = seedEntry(seed);
	}
	return state;
}

function skillProposal(action: "create" | "update", summary: string): RefinementProposal {
	return {
		summary,
		rationale: `Recurring ${FINGERPRINT} is a missing paramiko transport import.`,
		expectedOutcome: "The import failure stops recurring.",
		edits: [
			{
				action,
				kind: "skill",
				id: SKILL_ID,
				title: "paramiko transport probe",
				content: "Probe an SSH transport before opening a channel.",
				path: "skills/paramiko",
				reference: { type: "python", import: "prime_agent_m6_absent_module", callable: "probe" },
				arguments: { host: { type: "string", required: true } },
			},
		],
	};
}

/** Commit the skill entry through the apply path, claiming `FINGERPRINT`. */
function commitClaimingFingerprint(
	state: HarnessState,
	options: { id: string; action: "create" | "update"; turn: number; claims?: readonly string[] },
): void {
	const result = applyRefinementProposal(state, skillProposal(options.action, `${options.action} paramiko probe`), {
		id: options.id,
		scope: "global",
		trustClaim: {
			claimedFingerprints: options.claims ?? [FINGERPRINT],
			committedTurn: options.turn,
			untilTurn: options.turn + 20,
		},
	});
	expect(result.appliedEdits.every((edit) => edit.applied)).toBe(true);
}

function renderedEntryIds(prompt: string): string[] {
	const ids: string[] = [];
	for (const line of prompt.split("\n")) {
		const match = line.match(/^- \[global:([^\]]+)\]/);
		if (match) ids.push(match[1]);
	}
	return ids;
}

function skillTrust(state: HarnessState): number {
	return entryTrustScore(state.entries.skill[SKILL_ID]?.trust);
}

/** A verdict built by the referee's own `verdictFromOutcome`, not a literal. */
function upheldVerdict(fingerprintId: string): RefereeVerdict {
	const status = verdictFromOutcome(MISSING_MODULE_CASE, {
		kind: "raised",
		exceptionClass: "ModuleNotFoundError",
		detail: "ModuleNotFoundError: No module named 'prime_agent_m6_absent_module'",
	});
	expect(status).toBe("upheld");
	return refereeVerdict(fingerprintId, status, `${status}: the recorded exception recurred`);
}

function failureRecord(fingerprintId: string, replayCase: ReplayCase): FailureRecord {
	return {
		fingerprint: {
			id: fingerprintId,
			kind: "python_exception",
			source: "ipython",
			exceptionClass: "ModuleNotFoundError",
			message: "no module named ?",
		},
		count: 2,
		firstSeenTurn: 3,
		lastSeenTurn: 9,
		firstSeenAt: "2026-09-14T07:00:00.000Z",
		lastSeenAt: "2026-09-14T08:00:00.000Z",
		excerpt: "ModuleNotFoundError: No module named 'prime_agent_m6_absent_module'",
		addressedByProposalIds: [],
		replayCase,
	};
}

function hasPython3(): boolean {
	try {
		execFileSync("python3", ["-c", "pass"], { stdio: "ignore" });
		return true;
	} catch {
		return false;
	}
}

const PYTHON3 = hasPython3();

let tempDir: string;
let previousAgentDir: string | undefined;

beforeEach(() => {
	previousAgentDir = process.env.PRIME_AGENT_CODING_AGENT_DIR;
	tempDir = mkdtempSync(join(tmpdir(), "prime-agent-harness-trust-"));
	process.env.PRIME_AGENT_CODING_AGENT_DIR = tempDir;
});

afterEach(() => {
	if (previousAgentDir === undefined) {
		delete process.env.PRIME_AGENT_CODING_AGENT_DIR;
	} else {
		process.env.PRIME_AGENT_CODING_AGENT_DIR = previousAgentDir;
	}
	previousAgentDir = undefined;
	rmSync(tempDir, { recursive: true, force: true });
});

describe("harness trust and eviction", () => {
	it("drives 50 -> 35 -> 20 on upheld referee verdicts and evicts the entry from the prompt", () => {
		const state = seedGlobalHarnessState();
		expect(Object.keys(state.entries.memory)).toHaveLength(27);

		commitClaimingFingerprint(state, { id: "refine_a", action: "create", turn: 10 });
		expect(skillTrust(state)).toBe(DEFAULT_ENTRY_TRUST);
		expect(state.trustWindows?.refine_a).toMatchObject({
			outcome: "open",
			touched: [`skill:${SKILL_ID}`],
			claimedFingerprints: [FINGERPRINT],
		});
		expect(renderedEntryIds(formatHarnessStateForPrompt(state))).toContain(SKILL_ID);

		const first = settleHarnessTrust(state, { verdicts: [upheldVerdict(FINGERPRINT)], turn: 12 });
		expect(first).toHaveLength(1);
		expect(first[0]).toMatchObject({ kind: "skill", id: SKILL_ID, reason: "measured_fault", delta: -15 });
		expect(skillTrust(state)).toBe(35);
		expect(state.trustWindows?.refine_a).toMatchObject({ outcome: "faulted", faultedFingerprints: [FINGERPRINT] });
		// 35 is above the dormancy threshold: one fault is not eviction.
		expect(renderedEntryIds(formatHarnessStateForPrompt(state))).toContain(SKILL_ID);

		// A second refinement rewrites the entry claiming the same fingerprint.
		// The rewrite must carry the debited trust forward rather than resetting it.
		commitClaimingFingerprint(state, { id: "refine_b", action: "update", turn: 40 });
		expect(skillTrust(state)).toBe(35);
		expect(state.entries.skill[SKILL_ID].version).toBe(2);

		settleHarnessTrust(state, { verdicts: [upheldVerdict(FINGERPRINT)], turn: 42 });
		expect(skillTrust(state)).toBe(20);
		expect(skillTrust(state)).toBeLessThan(DORMANT_TRUST_THRESHOLD);

		const prompt = formatHarnessStateForPrompt(state);
		expect(renderedEntryIds(prompt)).not.toContain(SKILL_ID);
		expect(prompt).toContain("+1 dormant skill entries");

		// Dormant, never deleted: still fully readable and editable through CRUD.
		const statePath = saveHarnessState(join(tempDir, "harness"), state);
		expect(statePath).toContain("harness_state.json");
		const reloaded = loadHarnessState(join(tempDir, "harness"), "global");
		const survivor = reloaded.entries.skill[SKILL_ID];
		expect(survivor).toBeDefined();
		expect(survivor.content).toBe("Probe an SSH transport before opening a channel.");
		expect(survivor.trust?.score).toBe(20);
		expect(survivor.trust?.events).toHaveLength(2);
		expect(reloaded.trustWindows?.refine_b?.outcome).toBe("faulted");

		const edit = applyRefinementProposal(reloaded, skillProposal("update", "edit a dormant entry"), {
			id: "refine_c",
			scope: "global",
		});
		expect(edit.appliedEdits[0].applied).toBe(true);
		expect(reloaded.entries.skill[SKILL_ID].version).toBe(3);
		expect(entryTrustScore(reloaded.entries.skill[SKILL_ID].trust)).toBe(20);
	});

	it.skipIf(!PYTHON3)("charges the debit off a verdict the referee produced in a subprocess", async () => {
		const state = seedGlobalHarnessState();
		commitClaimingFingerprint(state, { id: "refine_a", action: "create", turn: 10 });

		const records = [
			failureRecord(FINGERPRINT, MISSING_MODULE_CASE),
			failureRecord(OTHER_FINGERPRINT, RESOLVED_CASE),
		];
		const verdicts = await adjudicateFailureClaims(records, [FINGERPRINT, OTHER_FINGERPRINT], {
			pythonPath: "python3",
		});

		expect(verdicts.map((verdict) => [verdict.fingerprintId, verdict.status])).toEqual([
			[FINGERPRINT, "upheld"],
			[OTHER_FINGERPRINT, "cleared"],
		]);

		settleHarnessTrust(state, { verdicts, turn: 12 });
		expect(skillTrust(state)).toBe(35);
	});

	it.skipIf(!PYTHON3)("leaves trust alone when the referee clears every claim it re-ran", async () => {
		const state = seedGlobalHarnessState();
		commitClaimingFingerprint(state, { id: "refine_a", action: "create", turn: 10, claims: [OTHER_FINGERPRINT] });

		const verdicts = await adjudicateFailureClaims(
			[failureRecord(OTHER_FINGERPRINT, RESOLVED_CASE)],
			[OTHER_FINGERPRINT],
			{
				pythonPath: "python3",
			},
		);
		expect(verdicts[0].status).toBe("cleared");

		expect(settleHarnessTrust(state, { verdicts, turn: 12 })).toEqual([]);
		expect(skillTrust(state)).toBe(DEFAULT_ENTRY_TRUST);
		expect(state.trustWindows?.refine_a?.outcome).toBe("open");
	});

	it("credits a provisional window that closes with nothing refuted", () => {
		const state = seedGlobalHarnessState();
		commitClaimingFingerprint(state, { id: "refine_a", action: "create", turn: 10 });

		// Inside the window nothing settles.
		expect(settleHarnessTrust(state, { turn: 25 })).toEqual([]);
		expect(state.trustWindows?.refine_a?.outcome).toBe("open");

		const adjustments = settleHarnessTrust(state, { turn: 31 });
		expect(adjustments).toHaveLength(1);
		expect(adjustments[0]).toMatchObject({ reason: "clean_window", delta: CLEAN_WINDOW_CREDIT });
		expect(skillTrust(state)).toBe(DEFAULT_ENTRY_TRUST + CLEAN_WINDOW_CREDIT);
		expect(state.trustWindows?.refine_a).toMatchObject({ outcome: "clean", settledTurn: 31 });
	});

	it("debits only entries the faulted refinement actually wrote", () => {
		const state = seedGlobalHarnessState();
		commitClaimingFingerprint(state, { id: "refine_a", action: "create", turn: 10 });

		settleHarnessTrust(state, { verdicts: [upheldVerdict(FINGERPRINT)], turn: 12 });

		expect(skillTrust(state)).toBe(35);
		for (const entry of Object.values(state.entries.memory)) {
			expect(entry.trust).toBeUndefined();
			expect(entryTrustScore(entry.trust)).toBe(DEFAULT_ENTRY_TRUST);
		}
	});

	it("ignores an upheld verdict on a fingerprint the refinement never claimed", () => {
		const state = seedGlobalHarnessState();
		commitClaimingFingerprint(state, { id: "refine_a", action: "create", turn: 10 });

		expect(settleHarnessTrust(state, { verdicts: [upheldVerdict(OTHER_FINGERPRINT)], turn: 12 })).toEqual([]);
		expect(skillTrust(state)).toBe(DEFAULT_ENTRY_TRUST);
		expect(state.trustWindows?.refine_a?.outcome).toBe("open");
	});

	it("never settles on a verdict that could not be verified", () => {
		const state = seedGlobalHarnessState();
		commitClaimingFingerprint(state, { id: "refine_a", action: "create", turn: 10 });

		const status = verdictFromOutcome(MISSING_MODULE_CASE, { kind: "unrunnable", detail: "no kernel python" });
		expect(status).toBe("unverifiable");

		expect(settleHarnessTrust(state, { verdicts: [refereeVerdict(FINGERPRINT, status, status)], turn: 12 })).toEqual(
			[],
		);
		expect(skillTrust(state)).toBe(DEFAULT_ENTRY_TRUST);
		expect(state.trustWindows?.refine_a?.outcome).toBe("open");
	});

	it("carries an unmodelled top-level entry key through an update", () => {
		const state = seedGlobalHarnessState();
		commitClaimingFingerprint(state, { id: "refine_a", action: "create", turn: 10 });
		(state.entries.skill[SKILL_ID] as unknown as Record<string, unknown>).provenance = "toolforge";

		commitClaimingFingerprint(state, { id: "refine_b", action: "update", turn: 40 });

		expect((state.entries.skill[SKILL_ID] as unknown as Record<string, unknown>).provenance).toBe("toolforge");
	});

	it("opens no window for a commit that claimed nothing", () => {
		const state = seedGlobalHarnessState();
		const result = applyRefinementProposal(state, skillProposal("create", "unclaimed commit"), {
			id: "refine_a",
			scope: "global",
			trustClaim: { claimedFingerprints: [], committedTurn: 10 },
		});

		expect(result.appliedEdits[0].applied).toBe(true);
		expect(state.trustWindows).toBeUndefined();
		expect(skillTrust(state)).toBe(DEFAULT_ENTRY_TRUST);
	});

	it("keeps a measured fault strictly below a clean window across the whole clamp range", () => {
		for (let score = MIN_ENTRY_TRUST; score <= MAX_ENTRY_TRUST; score++) {
			expect(faultIsStrictlyWorseThanCleanWindow(score)).toBe(true);
		}
		expect(MEASURED_FAULT_DEBIT).toBeGreaterThan(CLEAN_WINDOW_CREDIT);
	});

	it("clamps trust to [0, 100] however many verdicts land", () => {
		const state = seedGlobalHarnessState();
		commitClaimingFingerprint(state, { id: "refine_a", action: "create", turn: 10 });
		settleHarnessTrust(state, { verdicts: [upheldVerdict(FINGERPRINT)], turn: 12 });

		for (let round = 0; round < 10; round++) {
			commitClaimingFingerprint(state, { id: `refine_fault_${round}`, action: "update", turn: 100 + round });
			settleHarnessTrust(state, { verdicts: [upheldVerdict(FINGERPRINT)], turn: 101 + round });
		}
		expect(skillTrust(state)).toBe(MIN_ENTRY_TRUST);

		for (let round = 0; round < 40; round++) {
			commitClaimingFingerprint(state, { id: `refine_clean_${round}`, action: "update", turn: 500 + round });
			settleHarnessTrust(state, { turn: 600 + round });
		}
		expect(skillTrust(state)).toBe(MAX_ENTRY_TRUST);
	});

	it("drops a malformed trust record on load instead of hiding the entry", () => {
		const state = seedGlobalHarnessState();
		const dir = join(tempDir, "harness");
		state.entries.memory.obscura_baseline = {
			...state.entries.memory.obscura_baseline,
			trust: { score: "nonsense" } as unknown as HarnessEntry["trust"],
		};
		saveHarnessState(dir, state);

		const reloaded = loadHarnessState(dir, "global");
		expect(reloaded.entries.memory.obscura_baseline.trust).toBeUndefined();
		expect(renderedEntryIds(formatHarnessStateForPrompt(reloaded))).toContain("obscura_baseline");
		expect(normalizeEntryTrust({ score: "nonsense" })).toBeUndefined();
	});

	it("round-trips trust and trust windows through the state file", () => {
		const state = seedGlobalHarnessState();
		commitClaimingFingerprint(state, { id: "refine_a", action: "create", turn: 10 });
		settleHarnessTrust(state, { verdicts: [upheldVerdict(FINGERPRINT)], turn: 12 });
		const dir = join(tempDir, "harness");
		saveHarnessState(dir, state);

		const reloaded = loadHarnessState(dir, "global");
		expect(reloaded.entries.skill[SKILL_ID].trust).toEqual(state.entries.skill[SKILL_ID].trust);
		expect(reloaded.trustWindows).toEqual(state.trustWindows);
	});
});
