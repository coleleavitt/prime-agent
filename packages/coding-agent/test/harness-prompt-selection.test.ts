import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { formatHarnessStateForPrompt, type HarnessEntry, type HarnessState } from "../src/core/refinement/index.js";
import { buildSystemPrompt } from "../src/core/system-prompt.js";

/**
 * Mirrors the shape of the real 27-entry global harness state (27 memories, no
 * prompt/skill/subagent entries, three crowded paths holding 22 of them) with
 * invented content. The live file is never read.
 */
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
	const day = String(5 + index).padStart(2, "0");
	SEED_ENTRIES.push({
		id: `ers_journal_${String(index).padStart(2, "0")}`,
		title: `ers-rs work journal ${index}`,
		path: "projects/ers-rs/work-journal",
		updated: `2026-08-${day}`,
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

function renderedEntryIds(prompt: string): string[] {
	const ids: string[] = [];
	for (const line of prompt.split("\n")) {
		const match = line.match(/^- \[global:([^\]]+)\]/);
		if (match) {
			ids.push(match[1]);
		}
	}
	return ids;
}

let tempDir: string | undefined;
let previousAgentDir: string | undefined;

beforeEach(() => {
	previousAgentDir = process.env.PRIME_AGENT_CODING_AGENT_DIR;
	tempDir = mkdtempSync(join(tmpdir(), "prime-agent-harness-prompt-"));
	process.env.PRIME_AGENT_CODING_AGENT_DIR = tempDir;
});

afterEach(() => {
	if (previousAgentDir === undefined) {
		delete process.env.PRIME_AGENT_CODING_AGENT_DIR;
	} else {
		process.env.PRIME_AGENT_CODING_AGENT_DIR = previousAgentDir;
	}
	previousAgentDir = undefined;
	if (tempDir) {
		rmSync(tempDir, { recursive: true, force: true });
		tempDir = undefined;
	}
});

describe("harness prompt selection", () => {
	it("seeds the 27-entry global shape used by the assertions below", () => {
		const state = seedGlobalHarnessState();
		expect(Object.keys(state.entries.memory)).toHaveLength(27);
		const byPath = new Map<string, number>();
		for (const entry of Object.values(state.entries.memory)) {
			byPath.set(entry.path, (byPath.get(entry.path) ?? 0) + 1);
		}
		const crowded = [...byPath.values()].sort((a, b) => b - a).slice(0, 3);
		expect(crowded.reduce((sum, count) => sum + count, 0)).toBe(22);
	});

	it("renders the freshest entry at each path and deprioritises superseded siblings", () => {
		const rendered = renderedEntryIds(formatHarnessStateForPrompt(seedGlobalHarnessState()));

		expect(rendered).toHaveLength(6);
		expect(rendered).toContain("obscura_baseline");

		const ersSiblings = rendered.filter((id) => id.startsWith("ers_new_entity_"));
		expect(ersSiblings.length).toBeLessThanOrEqual(1);
		expect(ersSiblings).toEqual(["ers_new_entity_d"]);
	});

	it("orders the rendered slots by kind, then supersession, then recency", () => {
		const rendered = renderedEntryIds(formatHarnessStateForPrompt(seedGlobalHarnessState()));

		// Behavioural rules rank ahead of episodic project notes; within each group it is
		// freshest-at-path first. Project notes are written constantly, so a pure recency sort
		// buries every durable preference within a day — see the regression test below.
		expect(rendered).toEqual([
			"version_control",
			"obscura_baseline",
			"norm_cache_d",
			"ers_kernel_venv",
			"ers_new_entity_d",
			"ers_journal_13",
		]);
	});

	it("keeps the overflow count over the full entry set", () => {
		const prompt = formatHarnessStateForPrompt(seedGlobalHarnessState());
		expect(prompt).toContain("memory: 27");
		expect(prompt).toContain("+21 more memory entries");
	});

	it("prefers a newer sibling even when it sorts last alphabetically", () => {
		const state = seedGlobalHarnessState();
		const older = state.entries.memory.ers_new_entity_d;
		state.entries.memory.ers_new_entity_z = {
			...older,
			id: "ers_new_entity_z",
			title: "New entity: Zone",
			updated_at: "2026-08-22T09:00:00.000Z",
		};

		const rendered = renderedEntryIds(formatHarnessStateForPrompt(state));
		expect(rendered).toContain("ers_new_entity_z");
		expect(rendered).not.toContain("ers_new_entity_d");
	});

	it("falls back to created_at when updated_at is unparseable", () => {
		const state = seedGlobalHarnessState();
		state.entries.memory.obscura_baseline = {
			...state.entries.memory.obscura_baseline,
			updated_at: "not-a-timestamp",
			created_at: "2026-09-15T09:00:00.000Z",
		};

		// First among the EPISODIC entries; behavioural rules occupy the leading slots.
		const episodic = renderedEntryIds(formatHarnessStateForPrompt(state)).filter((id) => id !== "version_control");
		expect(episodic[0]).toBe("obscura_baseline");
	});

	it("never lets fresher project notes crowd out a behavioural rule", () => {
		const state = seedGlobalHarnessState();
		// Every project note is newer than the preference, which is the real situation on disk:
		// the one behavioural rule the refinement pass ever produced was written 2026-08-19 and
		// every project memory since is more recent.
		for (const [id, entry] of Object.entries(state.entries.memory)) {
			if (id === "version_control") continue;
			state.entries.memory[id] = { ...entry, updated_at: "2026-09-16T12:00:00.000Z" };
		}

		const rendered = renderedEntryIds(formatHarnessStateForPrompt(state));
		expect(rendered[0]).toBe("version_control");
		expect(rendered).toContain("version_control");
	});

	it("surfaces the freshest entry in the rendered system prompt", () => {
		const state = seedGlobalHarnessState();

		const rlmPrompt = buildSystemPrompt({ cwd: "/workspace", selectedTools: ["ipython"], harnessState: state });
		expect(rlmPrompt).toContain("[global:obscura_baseline]");
		expect(rlmPrompt.split("[global:ers_new_entity_").length - 1).toBe(1);

		const customPrompt = buildSystemPrompt({
			cwd: "/workspace",
			customPrompt: "Custom system prompt.",
			selectedTools: ["ipython"],
			harnessState: state,
		});
		expect(customPrompt).toContain("[global:obscura_baseline]");
		expect(customPrompt.split("[global:ers_new_entity_").length - 1).toBe(1);
	});
});
