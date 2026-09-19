import { execFileSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import type { FailureRecord } from "../src/core/ravo/failure-ledger.js";
import { type RefereeVerdict, type ReplayCase, verdictFromOutcome } from "../src/core/ravo/referee.js";
import { adjudicateFailureClaims } from "../src/core/ravo/referee-runner.js";
import {
	CLEAN_WINDOW_CREDIT,
	DEFAULT_ENTRY_TRUST,
	DORMANT_TRUST_THRESHOLD,
	entryTrustScore,
	faultIsStrictlyWorseThanCleanWindow,
	type HarnessTrustWindows,
	MAX_ENTRY_TRUST,
	MAX_TRUST_ADJUDICATION_RUNS,
	MEASURED_FAULT_DEBIT,
	MIN_ENTRY_TRUST,
	normalizeEntryTrust,
	normalizeTrustWindows,
	openTrustWindow,
	recordTrustWindowEvidence,
	settleTrustWindows,
	type TrustAdjudicationStatus,
	type TrustWindowEvidence,
} from "../src/core/refinement/harness-trust.js";
import {
	applyRefinementProposal,
	formatHarnessStateForPrompt,
	getHarnessStatePath,
	type HarnessEntry,
	type HarnessState,
	loadHarnessState,
	type RefinementEdit,
	type RefinementProposal,
	recordHarnessTrustEvidence,
	saveHarnessState,
	settleHarnessTrust,
} from "../src/core/refinement/refinement.js";

/**
 * M6 exit test. Trust only ever moves on a MEASURED fault: a referee verdict
 * produced by re-executing a recorded replay case in a subprocess, recorded on
 * the trust window of the commit that wrote the skill it ran for. The debit is
 * charged only to that skill entry, never to anything else the commit wrote.
 *
 * The seeded state mirrors the real 27-entry global harness state (27 memories,
 * three crowded paths holding 22 of them) with invented content, plus the one
 * skill entry the claim is attributed to. The live file is never read.
 */

const FINGERPRINT = "ca2e5aceb78edc4b";
const OTHER_FINGERPRINT = "ca578aff209c6a3c";
const SKILL_ID = "paramiko_transport_probe";
const SKILL_REF = `skill:${SKILL_ID}`;
const SKILL_IMPORT = "prime_agent_m6_absent_module";
const MEMORY_REF = "memory:deploy_note";
const PROMPT_REF = "prompt:deploy_policy";

/** A replay case that genuinely raises under an isolated interpreter. */
const MISSING_MODULE_CASE: ReplayCase = {
	language: "python",
	source: `import ${SKILL_IMPORT}`,
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

function skillEdit(action: "create" | "update"): RefinementEdit {
	return {
		action,
		kind: "skill",
		id: SKILL_ID,
		title: "paramiko transport probe",
		content: "Probe an SSH transport before opening a channel.",
		path: "skills/paramiko",
		reference: { type: "python", import: SKILL_IMPORT, callable: "probe" },
		arguments: { host: { type: "string", required: true } },
	};
}

const SECOND_SKILL_ID = "paramiko_channel_probe";
const SECOND_SKILL_REF = `skill:${SECOND_SKILL_ID}`;

const SECOND_SKILL_EDIT: RefinementEdit = {
	...skillEdit("create"),
	id: SECOND_SKILL_ID,
	title: "paramiko channel probe",
	content: "Probe an SSH channel after the transport is up.",
	reference: { type: "python", import: SKILL_IMPORT, callable: "probe_channel" },
};

const MEMORY_EDIT: RefinementEdit = {
	action: "create",
	kind: "memory",
	id: "deploy_note",
	title: "Deploy note",
	content: "Install the paramiko transport before probing.",
};

const PROMPT_EDIT: RefinementEdit = {
	action: "create",
	kind: "prompt",
	id: "deploy_policy",
	title: "Deploy policy",
	content: "Probe transports before deploying.",
};

function proposalOf(summary: string, edits: RefinementEdit[]): RefinementProposal {
	return {
		summary,
		rationale: `Recurring ${FINGERPRINT} is a missing paramiko transport import.`,
		expectedOutcome: "The import failure stops recurring.",
		edits,
	};
}

function skillProposal(action: "create" | "update", summary: string): RefinementProposal {
	return proposalOf(summary, [skillEdit(action)]);
}

/** Commit through the apply path, claiming `FINGERPRINT` over [turn, turn + 20]. */
function commitClaimingFingerprint(
	state: HarnessState,
	options: {
		id: string;
		action?: "create" | "update";
		turn: number;
		claims?: readonly string[];
		edits?: RefinementEdit[];
	},
): void {
	const edits = options.edits ?? [skillEdit(options.action ?? "create")];
	const result = applyRefinementProposal(state, proposalOf(`commit ${options.id}`, edits), {
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

/** The status the referee's own `verdictFromOutcome` gives a reproduced missing module, not a literal. */
function upheldStatus(): TrustAdjudicationStatus {
	const status = verdictFromOutcome(MISSING_MODULE_CASE, {
		kind: "raised",
		exceptionClass: "ModuleNotFoundError",
		detail: `ModuleNotFoundError: No module named '${SKILL_IMPORT}'`,
	});
	expect(status).toBe("upheld");
	return "upheld";
}

function adjudication(
	proposalId: string,
	ordinal: number,
	options: { status?: TrustAdjudicationStatus; fingerprintId?: string; entry?: string; at?: string } = {},
): TrustWindowEvidence {
	return {
		type: "adjudication",
		proposalId,
		entry: options.entry ?? SKILL_REF,
		fingerprintId: options.fingerprintId ?? FINGERPRINT,
		status: options.status ?? upheldStatus(),
		ordinal,
		at: options.at ?? `2026-09-16T10:00:${String(ordinal % 60).padStart(2, "0")}.000Z`,
	};
}

function recurrence(proposalId: string, ordinal: number, fingerprintId = FINGERPRINT): TrustWindowEvidence {
	return { type: "recurrence", proposalId, fingerprintId, ordinal };
}

function record(state: HarnessState, evidence: readonly TrustWindowEvidence[]): void {
	state.trustWindows = recordTrustWindowEvidence(state.trustWindows, evidence);
}

/** Evidence built from a verdict the referee returned, scoped to the window and skill it ran for. */
function evidenceFromVerdict(proposalId: string, verdict: RefereeVerdict, ordinal: number): TrustWindowEvidence[] {
	if (verdict.status !== "upheld" && verdict.status !== "cleared" && verdict.status !== "unverifiable") return [];
	return [adjudication(proposalId, ordinal, { status: verdict.status, fingerprintId: verdict.fingerprintId })];
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
		excerpt: `ModuleNotFoundError: No module named '${SKILL_IMPORT}'`,
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
	it("drives 50 -> 35 -> 20 on upheld adjudications recorded for the skill and evicts the entry from the prompt", () => {
		const state = seedGlobalHarnessState();
		expect(Object.keys(state.entries.memory)).toHaveLength(27);

		commitClaimingFingerprint(state, { id: "refine_a", action: "create", turn: 10 });
		expect(skillTrust(state)).toBe(DEFAULT_ENTRY_TRUST);
		expect(state.trustWindows?.refine_a).toMatchObject({
			outcome: "open",
			touched: [SKILL_REF],
			claimedFingerprints: [FINGERPRINT],
		});
		expect(renderedEntryIds(formatHarnessStateForPrompt(state))).toContain(SKILL_ID);

		record(state, [adjudication("refine_a", 12)]);
		const first = settleHarnessTrust(state, { turn: 12 });
		expect(first.adjustments).toHaveLength(1);
		expect(first.adjustments[0]).toMatchObject({
			kind: "skill",
			id: SKILL_ID,
			reason: "measured_fault",
			delta: -15,
			fingerprintId: FINGERPRINT,
		});
		expect(skillTrust(state)).toBe(35);
		expect(state.trustWindows?.refine_a).toMatchObject({ outcome: "faulted", faultedFingerprints: [FINGERPRINT] });
		// 35 is above the dormancy threshold: one fault is not eviction.
		expect(renderedEntryIds(formatHarnessStateForPrompt(state))).toContain(SKILL_ID);

		// A second refinement rewrites the entry claiming the same fingerprint.
		// The rewrite must carry the debited trust forward rather than resetting it.
		commitClaimingFingerprint(state, { id: "refine_b", action: "update", turn: 40 });
		expect(skillTrust(state)).toBe(35);
		expect(state.entries.skill[SKILL_ID].version).toBe(2);

		record(state, [adjudication("refine_b", 42)]);
		settleHarnessTrust(state, { turn: 42 });
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
		expect(reloaded.trustWindows?.refine_b).toMatchObject({
			outcome: "faulted",
			adjudications: [{ entry: SKILL_REF, fingerprintId: FINGERPRINT, status: "upheld", ordinal: 42 }],
		});

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
		commitClaimingFingerprint(state, { id: "refine_b", action: "update", turn: 11, claims: [OTHER_FINGERPRINT] });

		const records = [
			failureRecord(FINGERPRINT, MISSING_MODULE_CASE),
			failureRecord(OTHER_FINGERPRINT, RESOLVED_CASE),
		];
		const verdicts = await adjudicateFailureClaims(records, [FINGERPRINT, OTHER_FINGERPRINT], {
			pythonPath: "python3",
			skillImports: [SKILL_IMPORT, "json"],
		});

		expect(verdicts.map((verdict) => [verdict.fingerprintId, verdict.status])).toEqual([
			[FINGERPRINT, "upheld"],
			[OTHER_FINGERPRINT, "cleared"],
		]);

		record(state, [
			...evidenceFromVerdict("refine_a", verdicts[0], 12),
			...evidenceFromVerdict("refine_b", verdicts[1], 12),
		]);
		const settlement = settleHarnessTrust(state, { turn: 12 });
		expect(settlement.adjustments.map((adjustment) => [adjustment.proposalId, adjustment.delta])).toEqual([
			["refine_a", -15],
		]);
		expect(skillTrust(state)).toBe(35);
		expect(state.trustWindows?.refine_b).toMatchObject({
			outcome: "open",
			adjudications: [{ status: "cleared", fingerprintId: OTHER_FINGERPRINT }],
		});
	});

	it.skipIf(!PYTHON3)("leaves trust alone when the referee clears every claim it re-ran", async () => {
		const state = seedGlobalHarnessState();
		commitClaimingFingerprint(state, { id: "refine_a", action: "create", turn: 10, claims: [OTHER_FINGERPRINT] });

		const verdicts = await adjudicateFailureClaims(
			[failureRecord(OTHER_FINGERPRINT, RESOLVED_CASE)],
			[OTHER_FINGERPRINT],
			{
				pythonPath: "python3",
				skillImports: ["json"],
			},
		);
		expect(verdicts[0].status).toBe("cleared");

		record(state, evidenceFromVerdict("refine_a", verdicts[0], 12));
		expect(settleHarnessTrust(state, { turn: 12 }).adjustments).toEqual([]);
		expect(skillTrust(state)).toBe(DEFAULT_ENTRY_TRUST);
		expect(state.trustWindows?.refine_a?.outcome).toBe("open");
		expect(state.trustWindows?.refine_a?.adjudications?.[0]?.status).toBe("cleared");
	});

	it("credits a provisional window that closes with nothing refuted", () => {
		const state = seedGlobalHarnessState();
		commitClaimingFingerprint(state, { id: "refine_a", action: "create", turn: 10 });

		// Inside the window nothing settles.
		expect(settleHarnessTrust(state, { turn: 25 })).toEqual({ adjustments: [], settled: [] });
		expect(state.trustWindows?.refine_a?.outcome).toBe("open");

		const settlement = settleHarnessTrust(state, { turn: 31 });
		expect(settlement.adjustments).toHaveLength(1);
		expect(settlement.adjustments[0]).toMatchObject({ reason: "clean_window", delta: CLEAN_WINDOW_CREDIT });
		expect(settlement.settled).toEqual([
			{ proposalId: "refine_a", from: "open", outcome: "clean", turn: 31, fingerprints: [FINGERPRINT] },
		]);
		expect(skillTrust(state)).toBe(DEFAULT_ENTRY_TRUST + CLEAN_WINDOW_CREDIT);
		expect(state.trustWindows?.refine_a).toMatchObject({ outcome: "clean", settledTurn: 31 });
	});

	it("debits only the skill entry the replay ran for", () => {
		const state = seedGlobalHarnessState();
		commitClaimingFingerprint(state, { id: "refine_a", action: "create", turn: 10 });

		record(state, [adjudication("refine_a", 12), adjudication("refine_a", 12, { entry: "memory:version_control" })]);
		settleHarnessTrust(state, { turn: 12 });

		expect(skillTrust(state)).toBe(35);
		for (const entry of Object.values(state.entries.memory)) {
			expect(entry.trust).toBeUndefined();
			expect(entryTrustScore(entry.trust)).toBe(DEFAULT_ENTRY_TRUST);
		}
	});

	it("ignores upheld evidence on a fingerprint the refinement never claimed", () => {
		const state = seedGlobalHarnessState();
		commitClaimingFingerprint(state, { id: "refine_a", action: "create", turn: 10 });
		const before = structuredClone(state.trustWindows);

		record(state, [adjudication("refine_a", 12, { fingerprintId: OTHER_FINGERPRINT })]);
		expect(state.trustWindows).toEqual(before);
		expect(settleHarnessTrust(state, { turn: 12 }).adjustments).toEqual([]);
		expect(skillTrust(state)).toBe(DEFAULT_ENTRY_TRUST);
		expect(state.trustWindows?.refine_a?.outcome).toBe("open");
	});

	it("never settles on an unverifiable adjudication", () => {
		const state = seedGlobalHarnessState();
		commitClaimingFingerprint(state, { id: "refine_a", action: "create", turn: 10 });

		const status = verdictFromOutcome(MISSING_MODULE_CASE, { kind: "unrunnable", detail: "no kernel python" });
		expect(status).toBe("unverifiable");
		record(state, [adjudication("refine_a", 12, { status: "unverifiable" })]);

		expect(settleHarnessTrust(state, { turn: 12 }).adjustments).toEqual([]);
		expect(state.trustWindows?.refine_a?.outcome).toBe("open");
		// The replay ran because the failure recurred, so the window cannot close clean either.
		expect(settleHarnessTrust(state, { turn: 31 }).adjustments).toEqual([]);
		expect(state.trustWindows?.refine_a?.outcome).toBe("contested");
		expect(skillTrust(state)).toBe(DEFAULT_ENTRY_TRUST);
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

	it("settles a legacy window that claimed nothing on load, without credit, and never reopens it", () => {
		// The two windows on disk that predate claim recording: outcome "success"
		// and no claimedFingerprints. They used to normalize to an open window with
		// untilTurn 0 and be credited on the next settlement.
		const legacy = normalizeTrustWindows({
			refine_legacy: { proposalId: "refine_legacy", touched: [SKILL_REF], outcome: "success" },
			refine_empty: {
				proposalId: "refine_empty",
				touched: [SKILL_REF],
				claimedFingerprints: [],
				committedTurn: 4,
				untilTurn: 24,
				outcome: "open",
			},
		});
		expect(legacy?.refine_legacy).toMatchObject({ outcome: "unmeasured", claimedFingerprints: [] });
		expect(legacy?.refine_empty?.outcome).toBe("unmeasured");

		const state = seedGlobalHarnessState();
		commitClaimingFingerprint(state, { id: "refine_a", action: "create", turn: 10 });
		state.trustWindows = { ...legacy, ...state.trustWindows };
		const lookup = (kind: string, id: string) => state.entries[kind as "skill"]?.[id];

		const late = settleTrustWindows(state.trustWindows, lookup, { turn: 1000 });
		expect(late.adjustments.map((adjustment) => adjustment.proposalId)).toEqual(["refine_a"]);
		expect(late.windows.refine_legacy?.outcome).toBe("unmeasured");
		expect(late.windows.refine_empty?.outcome).toBe("unmeasured");
		expect(normalizeTrustWindows(JSON.parse(JSON.stringify(late.windows)))?.refine_legacy?.outcome).toBe(
			"unmeasured",
		);

		const recorded = recordTrustWindowEvidence(legacy, [
			adjudication("refine_legacy", 0),
			adjudication("refine_empty", 5),
		]);
		expect(recorded).toEqual(legacy);
		const upheld = settleTrustWindows(recorded, lookup, { turn: 1 });
		expect(upheld.adjustments).toEqual([]);
		expect(upheld.settled).toEqual([]);
	});

	it("keeps a measured fault strictly below a clean window across the whole clamp range", () => {
		for (let score = MIN_ENTRY_TRUST; score <= MAX_ENTRY_TRUST; score++) {
			expect(faultIsStrictlyWorseThanCleanWindow(score)).toBe(true);
		}
		expect(MEASURED_FAULT_DEBIT).toBeGreaterThan(CLEAN_WINDOW_CREDIT);
	});

	it("clamps trust to [0, 100] however many adjudications land", () => {
		const state = seedGlobalHarnessState();
		commitClaimingFingerprint(state, { id: "refine_a", action: "create", turn: 10 });
		record(state, [adjudication("refine_a", 12)]);
		settleHarnessTrust(state, { turn: 12 });

		for (let round = 0; round < 10; round++) {
			commitClaimingFingerprint(state, { id: `refine_fault_${round}`, action: "update", turn: 100 + round });
			record(state, [adjudication(`refine_fault_${round}`, 101 + round)]);
			settleHarnessTrust(state, { turn: 101 + round });
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

	it("round-trips trust, trust windows, skill imports, recurrences and adjudications through the state file", () => {
		const state = seedGlobalHarnessState();
		commitClaimingFingerprint(state, { id: "refine_a", action: "create", turn: 10 });
		record(state, [adjudication("refine_a", 12)]);
		settleHarnessTrust(state, { turn: 12 });
		commitClaimingFingerprint(state, { id: "refine_b", action: "update", turn: 40 });
		record(state, [recurrence("refine_b", 44), adjudication("refine_b", 45, { status: "cleared" })]);
		const dir = join(tempDir, "harness");
		saveHarnessState(dir, state);

		const reloaded = loadHarnessState(dir, "global");
		expect(reloaded.entries.skill[SKILL_ID].trust).toEqual(state.entries.skill[SKILL_ID].trust);
		expect(reloaded.trustWindows).toEqual(state.trustWindows);
		expect(reloaded.trustWindows?.refine_b).toMatchObject({
			skillImports: { [SKILL_REF]: [SKILL_IMPORT] },
			recurrences: { [FINGERPRINT]: 44 },
			adjudications: [{ entry: SKILL_REF, status: "cleared", ordinal: 45, runs: [expect.any(String)] }],
		});
	});

	it("records the imports a commit wrote on its trust window", () => {
		const state = seedGlobalHarnessState();
		commitClaimingFingerprint(state, { id: "refine_skill", turn: 10 });
		expect(state.trustWindows?.refine_skill?.skillImports).toEqual({ [SKILL_REF]: [SKILL_IMPORT] });

		const mixed = seedGlobalHarnessState();
		commitClaimingFingerprint(mixed, { id: "refine_mixed", turn: 10, edits: [MEMORY_EDIT, skillEdit("create")] });
		expect(mixed.trustWindows?.refine_mixed).toMatchObject({
			touched: [MEMORY_REF, SKILL_REF],
			skillImports: { [SKILL_REF]: [SKILL_IMPORT] },
		});

		const memoryOnly = seedGlobalHarnessState();
		commitClaimingFingerprint(memoryOnly, { id: "refine_memory", turn: 10, edits: [MEMORY_EDIT] });
		expect(memoryOnly.trustWindows?.refine_memory?.touched).toEqual([MEMORY_REF]);
		expect(memoryOnly.trustWindows?.refine_memory).not.toHaveProperty("skillImports");
	});

	it("never charges a memory or prompt entry, even in a window whose skill was debited", () => {
		const state = seedGlobalHarnessState();
		commitClaimingFingerprint(state, {
			id: "refine_a",
			turn: 10,
			edits: [MEMORY_EDIT, PROMPT_EDIT, skillEdit("create")],
		});

		record(state, [
			adjudication("refine_a", 12),
			adjudication("refine_a", 12, { entry: MEMORY_REF }),
			adjudication("refine_a", 12, { entry: PROMPT_REF }),
		]);
		expect(state.trustWindows?.refine_a?.adjudications).toHaveLength(1);
		const settlement = settleHarnessTrust(state, { turn: 12 });

		expect(settlement.adjustments).toHaveLength(1);
		expect(settlement.adjustments[0]).toMatchObject({ kind: "skill", id: SKILL_ID, delta: -MEASURED_FAULT_DEBIT });
		expect(state.entries.memory.deploy_note.trust?.events).toEqual([]);
		expect(state.entries.prompt.deploy_policy.trust?.events).toEqual([]);
	});

	it("never charges a memory-only window for evidence recorded on another window claiming the same fingerprint", () => {
		const state = seedGlobalHarnessState();
		commitClaimingFingerprint(state, { id: "refine_skill", turn: 10 });
		commitClaimingFingerprint(state, { id: "refine_memory", turn: 11, edits: [MEMORY_EDIT] });

		record(state, [adjudication("refine_skill", 12), adjudication("refine_memory", 12)]);
		const settlement = settleHarnessTrust(state, { turn: 12 });

		expect(settlement.adjustments.map((adjustment) => adjustment.proposalId)).toEqual(["refine_skill"]);
		expect(state.trustWindows?.refine_memory?.outcome).toBe("open");
		expect(state.trustWindows?.refine_memory).not.toHaveProperty("adjudications");
		expect(state.entries.memory.deploy_note.trust?.events).toEqual([]);
	});

	it("closes a window whose claimed failure recurred without an upheld verdict as contested, with no credit", () => {
		const state = seedGlobalHarnessState();
		commitClaimingFingerprint(state, { id: "refine_a", turn: 10 });
		record(state, [recurrence("refine_a", 15)]);

		expect(settleHarnessTrust(state, { turn: 25 })).toEqual({ adjustments: [], settled: [] });
		expect(state.trustWindows?.refine_a?.outcome).toBe("open");

		const settlement = settleHarnessTrust(state, { turn: 31 });
		expect(settlement.adjustments).toEqual([]);
		expect(settlement.settled).toEqual([
			{ proposalId: "refine_a", from: "open", outcome: "contested", turn: 31, fingerprints: [FINGERPRINT] },
		]);
		expect(state.trustWindows?.refine_a).toMatchObject({ outcome: "contested", settledTurn: 31 });
		expect(skillTrust(state)).toBe(DEFAULT_ENTRY_TRUST);
	});

	it("faults a contested or clean window on a late upheld verdict for an in-window recurrence, staying strictly below a clean close", () => {
		const contested = seedGlobalHarnessState();
		commitClaimingFingerprint(contested, { id: "refine_a", turn: 10 });
		record(contested, [recurrence("refine_a", 15)]);
		settleHarnessTrust(contested, { turn: 31 });
		expect(contested.trustWindows?.refine_a?.outcome).toBe("contested");
		record(contested, [adjudication("refine_a", 15)]);
		const fault = settleHarnessTrust(contested, { turn: 40 });
		expect(fault.adjustments.map((adjustment) => adjustment.delta)).toEqual([-MEASURED_FAULT_DEBIT]);
		expect(fault.settled).toEqual([
			{ proposalId: "refine_a", from: "contested", outcome: "faulted", turn: 40, fingerprints: [FINGERPRINT] },
		]);
		expect(skillTrust(contested)).toBe(35);

		const clean = seedGlobalHarnessState();
		commitClaimingFingerprint(clean, { id: "refine_a", turn: 10, edits: [MEMORY_EDIT, skillEdit("create")] });
		settleHarnessTrust(clean, { turn: 31 });
		expect(clean.trustWindows?.refine_a?.outcome).toBe("clean");
		const cleanScore = skillTrust(clean);
		expect(cleanScore).toBe(55);
		record(clean, [adjudication("refine_a", 15)]);
		expect(settleHarnessTrust(clean, { turn: 40 }).settled[0]).toMatchObject({ from: "clean", outcome: "faulted" });
		expect(skillTrust(clean)).toBe(40);
		expect(skillTrust(clean)).toBeLessThan(cleanScore);
		// The co-touched memory keeps the credit the clean close granted.
		expect(entryTrustScore(clean.entries.memory.deploy_note.trust)).toBe(55);

		const faulted = structuredClone(clean.trustWindows);
		record(clean, [adjudication("refine_a", 16, { at: "2026-09-16T11:00:00.000Z" }), recurrence("refine_a", 11)]);
		expect(clean.trustWindows).toEqual(faulted);
		expect(settleHarnessTrust(clean, { turn: 1000 })).toEqual({ adjustments: [], settled: [] });
		expect(skillTrust(clean)).toBe(40);
	});

	it("ignores evidence outside the window, on unclaimed fingerprints, untouched entries, unknown proposals, and unmeasured or faulted windows", () => {
		const state = seedGlobalHarnessState();
		commitClaimingFingerprint(state, { id: "refine_a", turn: 10, edits: [MEMORY_EDIT, skillEdit("create")] });
		commitClaimingFingerprint(state, { id: "refine_faulted", action: "update", turn: 10 });
		state.trustWindows = {
			...state.trustWindows,
			refine_faulted: { ...state.trustWindows!.refine_faulted, outcome: "faulted", settledTurn: 12 },
			...normalizeTrustWindows({ refine_legacy: { proposalId: "refine_legacy", touched: [SKILL_REF] } }),
		};
		const windows = structuredClone(state.trustWindows);

		for (const evidence of [
			adjudication("refine_a", 9),
			adjudication("refine_a", 31),
			recurrence("refine_a", 9),
			recurrence("refine_a", 31),
			adjudication("refine_a", 12, { fingerprintId: OTHER_FINGERPRINT }),
			recurrence("refine_a", 12, OTHER_FINGERPRINT),
			adjudication("refine_a", 12, { entry: "skill:another_skill" }),
			adjudication("refine_a", 12, { entry: MEMORY_REF }),
			adjudication("refine_unknown", 12),
			recurrence("constructor", 12),
			adjudication("refine_legacy", 12),
			adjudication("refine_faulted", 12),
			recurrence("refine_faulted", 12),
		]) {
			expect(recordTrustWindowEvidence(windows, [evidence])).toEqual(windows);
		}
	});

	it("records evidence idempotently and caps runs, with upheld outranking cleared", () => {
		const state = seedGlobalHarnessState();
		commitClaimingFingerprint(state, { id: "refine_a", turn: 10 });
		const windows = state.trustWindows;

		const once = recordTrustWindowEvidence(windows, [adjudication("refine_a", 14, { status: "cleared" })]);
		const twice = recordTrustWindowEvidence(once, [adjudication("refine_a", 14, { status: "cleared" })]);
		expect(twice).toEqual(once);
		expect(twice?.refine_a?.adjudications?.[0]?.runs).toHaveLength(1);

		const runs = [13, 14, 15, 16].map((ordinal) => adjudication("refine_a", ordinal, { status: "unverifiable" }));
		const capped = recordTrustWindowEvidence(windows, runs);
		expect(capped?.refine_a?.adjudications?.[0]).toMatchObject({ status: "unverifiable", ordinal: 13 });
		expect(capped?.refine_a?.adjudications?.[0]?.runs).toHaveLength(MAX_TRUST_ADJUDICATION_RUNS);
		expect(recordTrustWindowEvidence(windows, [...runs].reverse())).toEqual(capped);
		// Full runs still accept a status upgrade.
		expect(
			recordTrustWindowEvidence(capped, [adjudication("refine_a", 17)])?.refine_a?.adjudications?.[0],
		).toMatchObject({ status: "upheld", ordinal: 13, runs: capped?.refine_a?.adjudications?.[0]?.runs });

		const clearedThenUpheld = recordTrustWindowEvidence(windows, [
			adjudication("refine_a", 18, { status: "cleared" }),
			adjudication("refine_a", 12),
		]);
		const upheldThenCleared = recordTrustWindowEvidence(windows, [
			adjudication("refine_a", 12),
			adjudication("refine_a", 18, { status: "cleared" }),
		]);
		expect(clearedThenUpheld?.refine_a?.adjudications).toEqual([
			{ entry: SKILL_REF, fingerprintId: FINGERPRINT, status: "upheld", ordinal: 12, runs: expect.any(Array) },
		]);
		expect(upheldThenCleared).toEqual(clearedThenUpheld);
		expect(clearedThenUpheld?.refine_a?.recurrences).toEqual({ [FINGERPRINT]: 12 });
	});

	it("charges one debit per window and entry when several claimed fingerprints are upheld", () => {
		const state = seedGlobalHarnessState();
		commitClaimingFingerprint(state, { id: "refine_a", turn: 10, claims: [OTHER_FINGERPRINT, FINGERPRINT] });

		record(state, [
			adjudication("refine_a", 12, { fingerprintId: OTHER_FINGERPRINT }),
			adjudication("refine_a", 13, { fingerprintId: FINGERPRINT }),
		]);
		const settlement = settleHarnessTrust(state, { turn: 13 });

		expect(settlement.adjustments).toHaveLength(1);
		expect(settlement.adjustments[0]).toMatchObject({
			delta: -MEASURED_FAULT_DEBIT,
			fingerprintId: [FINGERPRINT, OTHER_FINGERPRINT].sort()[0],
		});
		expect(state.trustWindows?.refine_a?.faultedFingerprints).toEqual([FINGERPRINT, OTHER_FINGERPRINT].sort());
		expect(skillTrust(state)).toBe(35);
	});

	it("drops a verdict for a skill rewritten to another import since its commit, checked when the verdict is recorded", () => {
		const state = seedGlobalHarnessState();
		commitClaimingFingerprint(state, { id: "refine_a", turn: 10 });
		const verdict = adjudication("refine_a", 12);

		// Rewritten (by the kernel, or another commit) while the replay that produced the verdict ran.
		const rewritten = structuredClone(state);
		rewritten.entries.skill[SKILL_ID].reference = { type: "python", import: "json", callable: "probe" };
		expect(recordHarnessTrustEvidence(rewritten, [verdict])).toEqual(state.trustWindows);
		rewritten.trustWindows = recordHarnessTrustEvidence(rewritten, [verdict]);
		expect(settleHarnessTrust(rewritten, { turn: 12 })).toEqual({ adjustments: [], settled: [] });
		expect(rewritten.trustWindows?.refine_a?.outcome).toBe("open");
		expect(rewritten.entries.skill[SKILL_ID].trust?.events ?? []).toEqual([]);

		const unchanged = structuredClone(state);
		unchanged.trustWindows = recordHarnessTrustEvidence(unchanged, [verdict]);
		expect(settleHarnessTrust(unchanged, { turn: 12 }).adjustments).toEqual([
			expect.objectContaining({ id: SKILL_ID, reason: "measured_fault", delta: -MEASURED_FAULT_DEBIT }),
		]);

		// A deleted skill is not a rewrite: the window still faults, and the charge skips the missing entry.
		const deleted = structuredClone(state);
		delete deleted.entries.skill[SKILL_ID];
		deleted.trustWindows = recordHarnessTrustEvidence(deleted, [verdict]);
		expect(settleHarnessTrust(deleted, { turn: 12 }).adjustments).toEqual([]);
		expect(deleted.trustWindows?.refine_a).toMatchObject({ outcome: "faulted", faultedEntries: [SKILL_REF] });
	});

	it("charges a second skill of a faulted window whose own upheld verdict lands in a later flush, and neither twice", () => {
		const state = seedGlobalHarnessState();
		commitClaimingFingerprint(state, { id: "refine_a", turn: 10, edits: [skillEdit("create"), SECOND_SKILL_EDIT] });
		const refs = [SECOND_SKILL_REF, SKILL_REF].sort();

		record(state, [adjudication("refine_a", 12)]);
		expect(settleHarnessTrust(state, { turn: 12 }).adjustments.map((adjustment) => adjustment.id)).toEqual([
			SKILL_ID,
		]);
		expect(state.trustWindows?.refine_a).toMatchObject({ outcome: "faulted", faultedEntries: [SKILL_REF] });

		record(state, [adjudication("refine_a", 13, { entry: SECOND_SKILL_REF })]);
		expect(settleHarnessTrust(state, { turn: 13 })).toEqual({
			adjustments: [
				expect.objectContaining({ id: SECOND_SKILL_ID, reason: "measured_fault", delta: -MEASURED_FAULT_DEBIT }),
			],
			settled: [],
		});
		expect(state.trustWindows?.refine_a).toMatchObject({
			outcome: "faulted",
			settledTurn: 12,
			faultedEntries: refs,
			faultedFingerprints: [FINGERPRINT],
		});

		const windows = structuredClone(state.trustWindows);
		record(state, [
			adjudication("refine_a", 14, { at: "2026-09-16T11:00:00.000Z" }),
			adjudication("refine_a", 14, { entry: SECOND_SKILL_REF, at: "2026-09-16T11:00:00.000Z" }),
			recurrence("refine_a", 11),
		]);
		expect(state.trustWindows).toEqual(windows);
		expect(settleHarnessTrust(state, { turn: 1000 })).toEqual({ adjustments: [], settled: [] });
		expect(skillTrust(state)).toBe(35);
		expect(entryTrustScore(state.entries.skill[SECOND_SKILL_ID].trust)).toBe(35);
		expect(normalizeTrustWindows(JSON.parse(JSON.stringify(state.trustWindows)))).toEqual(state.trustWindows);

		// A faulted window written without the field (an older build) counts every entry as charged.
		const legacy = normalizeTrustWindows({
			refine_old: {
				proposalId: "refine_old",
				touched: refs,
				claimedFingerprints: [FINGERPRINT],
				committedTurn: 10,
				untilTurn: 30,
				outcome: "faulted",
				faultedFingerprints: [FINGERPRINT],
				faultedEntries: [MEMORY_REF, 3],
			},
		});
		expect(legacy?.refine_old).not.toHaveProperty("faultedEntries");
		expect(recordTrustWindowEvidence(legacy, [adjudication("refine_old", 12, { entry: SECOND_SKILL_REF })])).toEqual(
			legacy,
		);
	});

	it("charges the same entries whether a window's verdicts land in one flush or across two", () => {
		const opened = () =>
			openTrustWindow(
				openTrustWindow(undefined, {
					proposalId: "w1",
					touched: ["skill:a", "skill:b", "memory:m"],
					claimedFingerprints: ["fp1", "fp2"],
					committedTurn: 10,
					untilTurn: 30,
					skillImports: { "skill:a": ["pkg.a"], "skill:b": ["pkg.b"] },
				}),
				{
					proposalId: "w2",
					touched: ["skill:a"],
					claimedFingerprints: ["fp1"],
					committedTurn: 15,
					untilTurn: 35,
					skillImports: { "skill:a": ["pkg.a"] },
				},
			);
		let seed = 1;
		const random = () => {
			seed = (seed * 1664525 + 1013904223) >>> 0;
			return seed / 2 ** 32;
		};
		const pick = <T>(items: readonly T[]): T => items[Math.floor(random() * items.length)];
		const lookup = () => ({ trust: undefined });
		const charged = (adjustments: readonly { proposalId: string; kind: string; id: string; delta: number }[]) =>
			adjustments.map((item) => `${item.proposalId}/${item.kind}:${item.id}/${item.delta}`).sort();
		const mismatches: string[] = [];
		for (let run = 0; run < 400; run++) {
			const evidence: TrustWindowEvidence[] = Array.from({ length: 1 + Math.floor(random() * 12) }, () => {
				const proposalId = pick(["w1", "w2"]);
				const fingerprintId = pick(["fp1", "fp2"]);
				const ordinal = 8 + Math.floor(random() * 30);
				return random() < 0.4
					? { type: "recurrence", proposalId, fingerprintId, ordinal }
					: {
							type: "adjudication",
							proposalId,
							entry: pick(["skill:a", "skill:b", "memory:m"]),
							fingerprintId,
							status: pick(["upheld", "cleared", "unverifiable"] as const),
							ordinal,
							at: `2026-09-16T08:00:0${Math.floor(random() * 6)}.000Z`,
						};
			});
			const cut = Math.floor(random() * evidence.length);
			const once = settleTrustWindows(recordTrustWindowEvidence(opened(), evidence), lookup, { turn: 20, at: "t" });
			const first = settleTrustWindows(recordTrustWindowEvidence(opened(), evidence.slice(0, cut)), lookup, {
				turn: 20,
				at: "t",
			});
			const second = settleTrustWindows(recordTrustWindowEvidence(first.windows, evidence.slice(cut)), lookup, {
				turn: 20,
				at: "t",
			});
			const split = charged([...first.adjustments, ...second.adjustments]);
			if (JSON.stringify(charged(once.adjustments)) !== JSON.stringify(split)) mismatches.push(`run ${run}`);
		}
		expect(mismatches).toEqual([]);
	});

	it("drops malformed skill imports, recurrences and adjudications on load and loads contested", () => {
		const dir = join(tempDir, "harness");
		const state = seedGlobalHarnessState();
		saveHarnessState(dir, state);
		const run = "2026-09-16T10:00:00.000Z";
		const raw = JSON.parse(JSON.stringify(state)) as Record<string, unknown>;
		raw.trustWindows = {
			refine_a: {
				proposalId: "refine_a",
				touched: [SKILL_REF, MEMORY_REF],
				claimedFingerprints: [FINGERPRINT],
				committedTurn: 10,
				untilTurn: 30,
				outcome: "contested",
				settledTurn: 31,
				skillImports: {
					[SKILL_REF]: [SKILL_IMPORT, SKILL_IMPORT, 7],
					[MEMORY_REF]: ["not_a_skill"],
					"skill:untouched": ["x"],
					"skill:empty": [],
				},
				recurrences: { [FINGERPRINT]: 15, [OTHER_FINGERPRINT]: 15, extra: "15" },
				adjudications: [
					{ entry: SKILL_REF, fingerprintId: FINGERPRINT, status: "cleared", ordinal: 16, runs: [run] },
					{ entry: SKILL_REF, fingerprintId: FINGERPRINT, status: "upheld", ordinal: 18, runs: [run, "later"] },
					{ entry: SKILL_REF, fingerprintId: FINGERPRINT, status: "refuted", ordinal: 16, runs: [run] },
					{ entry: MEMORY_REF, fingerprintId: FINGERPRINT, status: "upheld", ordinal: 16, runs: [run] },
					{ entry: "skill:untouched", fingerprintId: FINGERPRINT, status: "upheld", ordinal: 16, runs: [run] },
					{ entry: SKILL_REF, fingerprintId: OTHER_FINGERPRINT, status: "upheld", ordinal: 16, runs: [run] },
					{ entry: SKILL_REF, fingerprintId: FINGERPRINT, status: "upheld", ordinal: 99, runs: [run] },
					{ entry: SKILL_REF, fingerprintId: FINGERPRINT, status: "upheld", ordinal: 16, runs: [3] },
					{ entry: SKILL_REF, fingerprintId: FINGERPRINT, status: "upheld", ordinal: 16, runs: [] },
					"garbage",
				],
			},
			refine_out_of_range: {
				proposalId: "refine_out_of_range",
				touched: [SKILL_REF],
				claimedFingerprints: [FINGERPRINT],
				committedTurn: 10,
				untilTurn: 30,
				outcome: "open",
				skillImports: "nonsense",
				recurrences: { [FINGERPRINT]: 31 },
				adjudications: { entry: SKILL_REF },
			},
		};
		writeFileSync(getHarnessStatePath(dir), JSON.stringify(raw), "utf8");

		const windows: HarnessTrustWindows | undefined = loadHarnessState(dir, "global").trustWindows;
		expect(windows?.refine_a).toEqual({
			proposalId: "refine_a",
			touched: [SKILL_REF, MEMORY_REF],
			claimedFingerprints: [FINGERPRINT],
			committedTurn: 10,
			untilTurn: 30,
			outcome: "contested",
			settledTurn: 31,
			skillImports: { [SKILL_REF]: [SKILL_IMPORT] },
			recurrences: { [FINGERPRINT]: 15 },
			adjudications: [
				{ entry: SKILL_REF, fingerprintId: FINGERPRINT, status: "upheld", ordinal: 16, runs: [run, "later"] },
			],
		});
		expect(windows?.refine_out_of_range).toEqual({
			proposalId: "refine_out_of_range",
			touched: [SKILL_REF],
			claimedFingerprints: [FINGERPRINT],
			committedTurn: 10,
			untilTurn: 30,
			outcome: "open",
		});
	});
});
