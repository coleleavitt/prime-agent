import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { emptyAssistedRavoState } from "../src/core/ravo/authority.js";
import type { JsonValue } from "../src/core/ravo/reducer.js";
import { type RavoState, ravoMarkProvisional, ravoObserveChampion, ravoStep } from "../src/core/ravo/reducer.js";
import {
	applyHarnessTrustWindowOutcome,
	applyRefinementProposal,
	applyTrustOutcome,
	clampTrust,
	formatHarnessStateForPrompt,
	type HarnessEntry,
	type HarnessState,
	isDormant,
	loadHarnessState,
	nextTrust,
	openHarnessTrustWindow,
	type RefinementKind,
	type RefinementProposal,
	ravoChampionWindowOutcome,
	saveHarnessState,
	settleHarnessTrustWindows,
	TRUST_DEFAULT,
	trustOf,
	trustTier,
} from "../src/core/refinement/index.js";

let tempDir: string | undefined;

afterEach(() => {
	if (tempDir) {
		rmSync(tempDir, { recursive: true, force: true });
		tempDir = undefined;
	}
});

function makeTempDir(): string {
	tempDir = mkdtempSync(join(tmpdir(), "prime-agent-harness-trust-test-"));
	return tempDir;
}

function entry(kind: RefinementKind, id: string, trust?: number): HarnessEntry {
	return {
		id,
		kind,
		title: `${id} title`,
		content: `${id} content`,
		path: "general",
		scope: "local",
		reference: {},
		arguments: {},
		metadata: {},
		source: "test",
		created_at: "2026-01-01T00:00:00.000Z",
		updated_at: "2026-01-01T00:00:00.000Z",
		version: 1,
		...(trust === undefined ? {} : { trust }),
	};
}

function emptyState(): HarnessState {
	return { schema: 1, entries: { prompt: {}, memory: {}, skill: {}, subagent: {} }, refinements: [] };
}

function stateWith(...entries: HarnessEntry[]): HarnessState {
	const state = emptyState();
	for (const item of entries) state.entries[item.kind][item.id] = item;
	return state;
}

function proposal(summary: string, edits: RefinementProposal["edits"]): RefinementProposal {
	return { summary, rationale: `${summary} rationale`, expectedOutcome: `${summary} outcome`, edits };
}

/** Commit `proposalId` as a RAVO champion with a provisional window `[turn, turn + 20]` claiming `fingerprint`. */
function committedRavoState(
	proposalId: string,
	options: { turn: number; fingerprint: string; base?: RavoState<JsonValue> },
): RavoState<JsonValue> {
	const base = options.base ?? emptyAssistedRavoState();
	const stepped = ravoStep(
		base,
		{ id: proposalId, artifact: null },
		{
			proposalId,
			screen: { status: "pass", score: 100 },
			deep: { status: "pass", score: 80 },
			criteria: base.opponents.criteria.map((criterion) => ({ criterionId: criterion.id, status: "pass" as const })),
		},
		{ screenThreshold: 50, epsilon: 1, deepTolerance: 10 },
	);
	expect(stepped.certificate.committed).toBe(true);
	return ravoMarkProvisional(stepped.state, proposalId, {
		claimedFingerprints: [options.fingerprint],
		window: { committedTurn: options.turn, untilTurn: options.turn + 20 },
	});
}

describe("harness trust arithmetic", () => {
	it("treats an absent trust as the default 50 and clamps to [0, 100]", () => {
		expect(trustOf(entry("memory", "m"))).toBe(TRUST_DEFAULT);
		expect(trustOf(entry("memory", "m", 0))).toBe(0);
		expect(clampTrust(-20)).toBe(0);
		expect(clampTrust(140)).toBe(100);
		expect(clampTrust(Number.NaN)).toBe(TRUST_DEFAULT);
		expect(nextTrust(3, "failure")).toBe(0);
		expect(nextTrust(98, "success")).toBe(100);
		expect(applyTrustOutcome(entry("memory", "m", 0), "failure").trust).toBe(0);
		expect(applyTrustOutcome(entry("memory", "m", 100), "success").trust).toBe(100);
		// Out-of-range persisted values are read as the default, never trusted verbatim.
		expect(trustOf({ trust: 250 })).toBe(TRUST_DEFAULT);
		expect(trustOf({ trust: 12.5 })).toBe(TRUST_DEFAULT);
	});

	it("charges one failure as three successes (50 -> 35 -> 50)", () => {
		let current = entry("memory", "m");
		current = applyTrustOutcome(current, "failure");
		expect(current.trust).toBe(35);
		for (let i = 0; i < 3; i++) current = applyTrustOutcome(current, "success");
		expect(current.trust).toBe(50);
	});

	it("drops a trusted entry to standard on a single failure (75 -> 60)", () => {
		const trusted = entry("prompt", "p", 75);
		expect(trustTier(trustOf(trusted))).toBe("trusted");
		const after = applyTrustOutcome(trusted, "failure");
		expect(after.trust).toBe(60);
		expect(trustTier(after.trust ?? 0)).toBe("standard");
	});

	it("needs five successes from the default to reach the trusted tier", () => {
		let current = entry("skill", "s");
		for (let i = 0; i < 4; i++) {
			current = applyTrustOutcome(current, "success");
			expect(trustTier(trustOf(current))).toBe("standard");
		}
		current = applyTrustOutcome(current, "success");
		expect(current.trust).toBe(75);
		expect(trustTier(trustOf(current))).toBe("trusted");
	});

	it("maps tiers at the exact boundaries", () => {
		expect(trustTier(0)).toBe("restricted");
		expect(trustTier(29)).toBe("restricted");
		expect(trustTier(30)).toBe("standard");
		expect(trustTier(70)).toBe("standard");
		expect(trustTier(71)).toBe("trusted");
		expect(trustTier(100)).toBe("trusted");
		expect(isDormant(entry("memory", "m", 29))).toBe(true);
		expect(isDormant(entry("memory", "m", 30))).toBe(false);
		expect(isDormant(entry("memory", "m"))).toBe(false);
	});
});

describe("dormant harness entries", () => {
	it("hides dormant entries from the prompt but keeps them in state", () => {
		const state = stateWith(
			entry("memory", "active_note", 50),
			entry("memory", "dormant_note", 20),
			entry("prompt", "dormant_policy", 0),
		);

		const prompt = formatHarnessStateForPrompt(state);

		expect(prompt).toContain("[local:active_note]");
		expect(prompt).not.toContain("[local:dormant_note]");
		expect(prompt).not.toContain("[local:dormant_policy]");
		expect(prompt).toContain("memory: 1");
		expect(prompt).toContain("prompt: 0");
		expect(prompt).toContain("2 dormant entries (trust < 30");
		expect(prompt).toContain("prompt:local:dormant_policy");
		expect(prompt).toContain("memory:local:dormant_note");
		// The entries are still present in the state (and therefore visible to CRUD).
		expect(state.entries.memory.dormant_note.trust).toBe(20);
		expect(state.entries.prompt.dormant_policy.trust).toBe(0);
	});

	it("omits the dormant footer when nothing is dormant", () => {
		const prompt = formatHarnessStateForPrompt(stateWith(entry("memory", "note", 31)));
		expect(prompt).not.toContain("dormant");
	});

	it("keeps dormant entries visible to update/delete edits and revives them on update", () => {
		const state = stateWith(entry("memory", "dormant_note", 10), entry("memory", "active_note", 60));

		const revived = applyRefinementProposal(
			state,
			proposal("Revive", [
				{ action: "update", kind: "memory", id: "dormant_note", title: "Revived", content: "Corrected content." },
				{ action: "update", kind: "memory", id: "active_note", title: "Touched", content: "Still active." },
			]),
			{ id: "refine_revive" },
		);

		expect(revived.appliedEdits.every((edit) => edit.applied)).toBe(true);
		expect(state.entries.memory.dormant_note.trust).toBe(TRUST_DEFAULT);
		expect(state.entries.memory.dormant_note.content).toBe("Corrected content.");
		// An update of an active entry is not a success: trust is carried, not bumped.
		expect(state.entries.memory.active_note.trust).toBe(60);

		const deleted = applyRefinementProposal(
			stateWith(entry("prompt", "dormant_policy", 5)),
			proposal("Delete", [{ action: "delete", kind: "prompt", id: "dormant_policy" }]),
			{ id: "refine_delete" },
		);
		expect(deleted.appliedEdits[0]).toMatchObject({ applied: true, before: { trust: 5 } });
	});

	it("creates entries without a trust field (read as 50)", () => {
		const state = emptyState();
		applyRefinementProposal(
			state,
			proposal("Create", [{ action: "create", kind: "memory", id: "fresh", title: "Fresh", content: "New." }]),
			{ id: "refine_create" },
		);
		expect(state.entries.memory.fresh.trust).toBeUndefined();
		expect(trustOf(state.entries.memory.fresh)).toBe(TRUST_DEFAULT);
	});
});

describe("trust windows", () => {
	it("records the entries a committed proposal touched, excluding deletes", () => {
		const state = stateWith(entry("memory", "old_note", 40), entry("prompt", "stale_policy", 40));
		applyRefinementProposal(
			state,
			proposal("Touch", [
				{ action: "create", kind: "memory", id: "new_note", title: "New", content: "Created." },
				{ action: "update", kind: "memory", id: "old_note", title: "Old", content: "Updated." },
				{ action: "delete", kind: "prompt", id: "stale_policy" },
			]),
			{ id: "refine_touch" },
		);

		expect(state.trustWindows?.refine_touch).toEqual({
			proposalId: "refine_touch",
			touched: ["memory:new_note", "memory:old_note"],
		});
	});

	it("does not open a window for a proposal that failed to apply", () => {
		const state = stateWith(entry("memory", "note", 40));
		applyRefinementProposal(
			state,
			proposal("Partial", [
				{ action: "update", kind: "memory", id: "note", title: "Note", content: "Updated." },
				{ action: "update", kind: "memory", id: "missing", title: "Missing", content: "Nope." },
			]),
			{ id: "refine_partial" },
		);
		expect(state.trustWindows).toBeUndefined();
		expect(state.entries.memory.note.content).toBe("note content");
	});

	it("credits +5 once when the window closes clean, even if observed twice", () => {
		const base = stateWith(entry("memory", "note", 50), entry("skill", "tool", 70));
		const opened = openHarnessTrustWindow(base, "refine_a", ["memory:note", "skill:tool"]);
		opened.ravo = committedRavoState("refine_a", { turn: 10, fingerprint: "fp1" });

		// Inside the window: nothing settles.
		const open = settleHarnessTrustWindows(opened, 30);
		expect(open).toBe(opened);
		expect(open.entries.memory.note.trust).toBe(50);

		const closed = settleHarnessTrustWindows(opened, 31);
		expect(closed.entries.memory.note.trust).toBe(55);
		expect(closed.entries.skill.tool.trust).toBe(75);
		expect(closed.trustWindows?.refine_a).toMatchObject({ outcome: "success", settledTurn: 31 });

		const again = settleHarnessTrustWindows(closed, 40);
		expect(again).toBe(closed);
		expect(again.entries.memory.note.trust).toBe(55);
		expect(applyHarnessTrustWindowOutcome(again, "refine_a", "success").entries.memory.note.trust).toBe(55);
		// The original state was never mutated.
		expect(base.entries.memory.note.trust).toBe(50);
	});

	it("debits -15 once when a measured fault is recorded inside the window", () => {
		const base = stateWith(entry("memory", "note", 50), entry("prompt", "policy", 75));
		const opened = openHarnessTrustWindow(base, "refine_b", ["memory:note", "prompt:policy"]);
		const committed = committedRavoState("refine_b", { turn: 10, fingerprint: "fp1" });
		const observed = ravoObserveChampion(committed, "refine_b", ["fp1"], 15);
		expect(observed.regression).toBe(true);
		opened.ravo = observed.state;

		const faulted = settleHarnessTrustWindows(opened, 15);
		expect(faulted.entries.memory.note.trust).toBe(35);
		expect(faulted.entries.prompt.policy.trust).toBe(60);
		expect(faulted.trustWindows?.refine_b).toMatchObject({ outcome: "failure", settledTurn: 15 });

		// Later turns, including the window closing, never re-apply or convert the outcome.
		const later = settleHarnessTrustWindows(faulted, 45);
		expect(later).toBe(faulted);
		expect(later.entries.memory.note.trust).toBe(35);
		expect(applyHarnessTrustWindowOutcome(later, "refine_b", "failure").entries.memory.note.trust).toBe(35);
	});

	it("drives an entry dormant through repeated measured faults and revives it on update", () => {
		let state = stateWith(entry("memory", "note", 50));
		let ravo = emptyAssistedRavoState();
		for (const [index, proposalId] of ["refine_1", "refine_2"].entries()) {
			state = openHarnessTrustWindow(state, proposalId, ["memory:note"]);
			ravo = committedRavoState(proposalId, { turn: index * 10, fingerprint: `fp${index}`, base: ravo });
			ravo = ravoObserveChampion(ravo, proposalId, [`fp${index}`], index * 10 + 1).state;
			state = { ...state, ravo };
			state = settleHarnessTrustWindows(state, index * 10 + 1);
		}
		expect(state.entries.memory.note.trust).toBe(20);
		expect(isDormant(state.entries.memory.note)).toBe(true);
		expect(formatHarnessStateForPrompt(state)).not.toContain("[local:note]");

		applyRefinementProposal(
			state,
			proposal("Revive", [{ action: "update", kind: "memory", id: "note", title: "Note", content: "Fixed." }]),
			{ id: "refine_revive" },
		);
		expect(state.entries.memory.note.trust).toBe(TRUST_DEFAULT);
		expect(formatHarnessStateForPrompt(state)).toContain("[local:note]");
	});

	it("skips entries deleted since the commit and drops windows that never became champions", () => {
		const base = stateWith(entry("memory", "kept", 50));
		let state = openHarnessTrustWindow(base, "refine_c", ["memory:kept", "memory:gone"]);
		state = openHarnessTrustWindow(state, "refine_rollback", ["memory:kept"]);
		state.ravo = committedRavoState("refine_c", { turn: 0, fingerprint: "fp1" });

		const settled = settleHarnessTrustWindows(state, 21);
		expect(settled.entries.memory.kept.trust).toBe(55);
		expect(settled.entries.memory.gone).toBeUndefined();
		expect(settled.trustWindows?.refine_c?.outcome).toBe("success");
		expect(settled.trustWindows?.refine_rollback).toBeUndefined();
	});

	it("leaves a champion without a provisional window open forever", () => {
		const base = stateWith(entry("memory", "note", 50));
		const state = openHarnessTrustWindow(base, "refine_d", ["memory:note"]);
		const ravo = emptyAssistedRavoState();
		state.ravo = ravoStep(
			ravo,
			{ id: "refine_d", artifact: null },
			{
				proposalId: "refine_d",
				screen: { status: "pass", score: 100 },
				deep: { status: "pass", score: 80 },
				criteria: ravo.opponents.criteria.map((criterion) => ({
					criterionId: criterion.id,
					status: "pass" as const,
				})),
			},
			{ screenThreshold: 50, epsilon: 1 },
		).state;
		expect(ravoChampionWindowOutcome(state.ravo.lineage[0], 1000)).toBeUndefined();
		const settled = settleHarnessTrustWindows(state, 1000);
		expect(settled.entries.memory.note.trust).toBe(50);
		expect(settled.trustWindows?.refine_d?.outcome).toBeUndefined();
	});

	it("maps a champion window to an outcome", () => {
		expect(ravoChampionWindowOutcome({ provisional: { committedTurn: 5, untilTurn: 25 } }, 25)).toBeUndefined();
		expect(ravoChampionWindowOutcome({ provisional: { committedTurn: 5, untilTurn: 25 } }, 26)).toBe("success");
		expect(
			ravoChampionWindowOutcome(
				{ provisional: { committedTurn: 5, untilTurn: 25, observedRecurrence: { turn: 9, fingerprints: ["fp"] } } },
				9,
			),
		).toBe("failure");
		expect(ravoChampionWindowOutcome({}, 99)).toBeUndefined();
		expect(ravoChampionWindowOutcome({ provisional: { committedTurn: 5, untilTurn: 25 } }, -1)).toBeUndefined();
	});
});

describe("trust persistence", () => {
	it("round-trips entry trust and trust windows through save/load", () => {
		const dir = makeTempDir();
		const state = stateWith(entry("memory", "note", 35), entry("prompt", "policy"));
		const opened = openHarnessTrustWindow(state, "refine_e", ["memory:note"]);
		const settled = applyHarnessTrustWindowOutcome(
			openHarnessTrustWindow(opened, "refine_f", ["prompt:policy"]),
			"refine_f",
			"failure",
			{ turn: 7 },
		);
		saveHarnessState(dir, settled);

		const reloaded = loadHarnessState(dir, "local");

		expect(reloaded.entries.memory.note.trust).toBe(35);
		expect(reloaded.entries.prompt.policy.trust).toBe(35);
		expect(reloaded.trustWindows).toEqual({
			refine_e: { proposalId: "refine_e", touched: ["memory:note"] },
			refine_f: { proposalId: "refine_f", touched: ["prompt:policy"], outcome: "failure", settledTurn: 7 },
		});
		// A settled window stays settled after reload: the outcome is never re-applied.
		expect(applyHarnessTrustWindowOutcome(reloaded, "refine_f", "failure").entries.prompt.policy.trust).toBe(35);
	});

	it("drops malformed persisted trust values instead of trusting them", () => {
		const dir = makeTempDir();
		const state = stateWith(entry("memory", "too_high"), entry("memory", "fraction"), entry("memory", "text"));
		(state.entries.memory.too_high as { trust?: unknown }).trust = 500;
		(state.entries.memory.fraction as { trust?: unknown }).trust = 40.5;
		(state.entries.memory.text as { trust?: unknown }).trust = "high";
		(state as { trustWindows?: unknown }).trustWindows = {
			ok: { touched: ["memory:too_high", 5], outcome: "bogus", settledTurn: -1 },
			"": { touched: [] },
			bad: "nope",
		};
		saveHarnessState(dir, state);

		const reloaded = loadHarnessState(dir, "local");

		expect(reloaded.entries.memory.too_high.trust).toBeUndefined();
		expect(reloaded.entries.memory.fraction.trust).toBeUndefined();
		expect(reloaded.entries.memory.text.trust).toBeUndefined();
		expect(reloaded.trustWindows).toEqual({ ok: { proposalId: "ok", touched: ["memory:too_high"] } });
	});

	it("leaves state files without trust untouched on load", () => {
		const dir = makeTempDir();
		saveHarnessState(dir, stateWith(entry("memory", "legacy")));
		const reloaded = loadHarnessState(dir, "local");
		expect(reloaded.entries.memory.legacy.trust).toBeUndefined();
		expect(reloaded.trustWindows).toBeUndefined();
		expect(isDormant(reloaded.entries.memory.legacy)).toBe(false);
	});
});
