// The generator of `trust.json` (`tests/golden.rs` replays every scenario in
// Rust and compares the JSON text, key order included).
//
// To regenerate: copy `packages/coding-agent/src/core/refinement/harness-trust.ts`
// from `perf/session-catalog-resume` into `<build>/refinement/`, put this file
// at `<build>/`, then (node 23+ strips the types itself)
//   node <build>/trust-generate.ts <this directory>
import { writeFileSync } from "node:fs";
import { join } from "node:path";
import {
	type HarnessEntryTrust,
	type HarnessTrustWindows,
	normalizeEntryTrust,
	normalizeTrustWindows,
	openTrustWindow,
	recordTrustWindowEvidence,
	settleTrustWindows,
	type TrustWindowEvidence,
} from "./refinement/harness-trust.ts";

const out = process.argv[2];
const AT = "2026-09-14T08:00:00.000Z";
const clone = <T>(value: T): T => JSON.parse(JSON.stringify(value)) as T;

// --- normalization -----------------------------------------------------------

const rawTrust: unknown[] = [
	undefined,
	null,
	[],
	{ score: "50" },
	{ score: 42.5, updated_at: 7, events: "no" },
	{ score: 140, updated_at: "t", events: [] },
	{ score: -3.5, updated_at: "t", events: [{ reason: "nope", delta: 1, score: 1 }] },
	{
		score: 35,
		updated_at: "t",
		events: [
			...Array.from({ length: 22 }, (_, index) => ({
				reason: index % 2 === 0 ? "clean_window" : "measured_fault",
				delta: index % 2 === 0 ? 5 : -15.5,
				score: 40 + index,
				at: `a${index}`,
				proposalId: `p${index}`,
				...(index % 3 === 0 ? { fingerprintId: `f${index}` } : {}),
				...(index === 4 ? { fingerprintId: "" } : {}),
			})),
			{ reason: "clean_window", delta: Number.NaN, score: 1 },
		],
	},
];

const rawWindows: unknown[] = [
	null,
	[],
	{
		legacy: { proposalId: "legacy", touched: ["memory:m"], committedTurn: 3, untilTurn: 23, outcome: "success" },
		"keyed-by-name": { touched: ["skill:s"], claimedFingerprints: ["f1"], committedTurn: 1.5, untilTurn: -2 },
		"": { touched: [] },
		notAnObject: 4,
		malformed: {
			proposalId: "malformed",
			touched: ["skill:s", "memory:m", 3, "", "skill:t"],
			claimedFingerprints: ["f2", "f1", "f2", 7],
			committedTurn: 10,
			untilTurn: 30,
			outcome: "faulted",
			settledTurn: -4,
			faultedFingerprints: ["f1", ""],
			faultedEntries: ["skill:t", "memory:m", "skill:zz", "skill:s", "skill:t"],
			skillImports: { "skill:s": ["b", "a", "b", ""], "memory:m": ["x"], "skill:t": [], "skill:zz": ["q"], "skill:x": 1 },
			recurrences: { f1: 12, f2: 31, f3: 12, f9: 9.5 },
			adjudications: [
				{ entry: "skill:s", fingerprintId: "f1", status: "cleared", ordinal: 12, runs: ["r3", "r1"] },
				{ entry: "skill:s", fingerprintId: "f1", status: "upheld", ordinal: 11, runs: ["r2", "r4", "r1"] },
				{ entry: "skill:t", fingerprintId: "f2", status: "unverifiable", ordinal: 15, runs: ["r9"] },
				{ entry: "memory:m", fingerprintId: "f1", status: "upheld", ordinal: 12, runs: ["r1"] },
				{ entry: "skill:s", fingerprintId: "f3", status: "upheld", ordinal: 12, runs: ["r1"] },
				{ entry: "skill:s", fingerprintId: "f1", status: "maybe", ordinal: 12, runs: ["r1"] },
				{ entry: "skill:s", fingerprintId: "f2", status: "upheld", ordinal: 40, runs: ["r1"] },
				{ entry: "skill:s", fingerprintId: "f2", status: "upheld", ordinal: 12, runs: [] },
				{ entry: "skill:s", fingerprintId: "f2", status: "upheld", ordinal: 12, runs: ["r1", ""] },
				{ entry: "skill:s", fingerprintId: "f2", status: "cleared", ordinal: 13, runs: ["r5"] },
			],
		},
		contested: {
			proposalId: "contested",
			touched: ["skill:s"],
			claimedFingerprints: ["f1"],
			committedTurn: 1,
			untilTurn: 2,
			outcome: "contested",
			settledTurn: 4,
			faultedEntries: ["skill:s"],
		},
	},
];

// --- a scripted run of windows ----------------------------------------------

interface Entry {
	trust?: HarnessEntryTrust;
	reference?: Record<string, unknown>;
}
const entries: Record<string, Record<string, Entry>> = {
	skill: {
		probe: { reference: { import: "absent_module" } },
		other: { reference: { import: "other_module" }, trust: { score: 22, updated_at: "old", events: [] } },
	},
	memory: { note: {} },
	prompt: { policy: { trust: { score: 98, updated_at: "old", events: [] } } },
};
const imports: Record<string, string[]> = { "skill:probe": ["absent_module"], "skill:other": ["other_module"] };
const lookup = (kind: string, id: string) => entries[kind]?.[id];
const current = (ref: string) => imports[ref];

type Step =
	| { open: Parameters<typeof openTrustWindow>[1] }
	| { record: TrustWindowEvidence[] }
	| { settle: number };

const steps: Step[] = [
	{
		open: {
			proposalId: "p1",
			touched: ["skill:probe", "memory:note", "skill:probe", "prompt:policy", "skill:other"],
			claimedFingerprints: ["fb", "fa", "fb"],
			committedTurn: 10,
			untilTurn: 30,
			skillImports: { "skill:probe": ["absent_module"], "skill:other": ["other_module", "other_module"], "memory:note": ["x"] },
		},
	},
	{ open: { proposalId: "p2", touched: ["memory:note"], claimedFingerprints: ["fa"], committedTurn: 12, untilTurn: 14 } },
	{ open: { proposalId: "p3", touched: ["prompt:policy", "skill:other"], claimedFingerprints: ["fc"], committedTurn: 5, untilTurn: 9 } },
	{ open: { proposalId: "p4", touched: ["skill:other"], claimedFingerprints: ["fd"], committedTurn: 20, untilTurn: 40 } },
	{
		record: [
			{ type: "recurrence", proposalId: "p2", fingerprintId: "fa", ordinal: 13 },
			{ type: "recurrence", proposalId: "p1", fingerprintId: "fa", ordinal: 15 },
			{ type: "recurrence", proposalId: "p1", fingerprintId: "fa", ordinal: 12 },
			{ type: "recurrence", proposalId: "p1", fingerprintId: "fz", ordinal: 12 },
			{ type: "recurrence", proposalId: "p1", fingerprintId: "fa", ordinal: 31 },
			{ type: "recurrence", proposalId: "nope", fingerprintId: "fa", ordinal: 12 },
			{
				type: "adjudication",
				proposalId: "p1",
				entry: "memory:note",
				fingerprintId: "fa",
				status: "upheld",
				ordinal: 12,
				at: "r0",
			},
			{
				type: "adjudication",
				proposalId: "p1",
				entry: "skill:probe",
				fingerprintId: "fb",
				status: "cleared",
				ordinal: 16,
				at: "r2",
			},
			{
				type: "adjudication",
				proposalId: "p4",
				entry: "skill:other",
				fingerprintId: "fd",
				status: "unverifiable",
				ordinal: 21,
				at: "r1",
			},
		],
	},
	{ settle: 14 },
	{ settle: 15 },
	{
		record: [
			{
				type: "adjudication",
				proposalId: "p1",
				entry: "skill:probe",
				fingerprintId: "fb",
				status: "upheld",
				ordinal: 18,
				at: "r1",
			},
			{
				type: "adjudication",
				proposalId: "p1",
				entry: "skill:probe",
				fingerprintId: "fa",
				status: "upheld",
				ordinal: 19,
				at: "r3",
			},
			{
				type: "adjudication",
				proposalId: "p1",
				entry: "skill:probe",
				fingerprintId: "fb",
				status: "cleared",
				ordinal: 17,
				at: "r4",
			},
			{
				type: "adjudication",
				proposalId: "p1",
				entry: "skill:probe",
				fingerprintId: "fb",
				status: "cleared",
				ordinal: 17,
				at: "r5",
			},
		],
	},
	{ settle: 20 },
	{
		record: [
			{ type: "recurrence", proposalId: "p1", fingerprintId: "fa", ordinal: 11 },
			{
				type: "adjudication",
				proposalId: "p1",
				entry: "skill:other",
				fingerprintId: "fa",
				status: "upheld",
				ordinal: 21,
				at: "r6",
			},
			{
				type: "adjudication",
				proposalId: "p1",
				entry: "skill:probe",
				fingerprintId: "fa",
				status: "upheld",
				ordinal: 21,
				at: "r7",
			},
			{
				type: "adjudication",
				proposalId: "p4",
				entry: "skill:other",
				fingerprintId: "fd",
				status: "upheld",
				ordinal: 22,
				at: "r8",
			},
		],
	},
	{ settle: 22 },
	{ settle: 41 },
];

let windows: HarnessTrustWindows | undefined;
const trace: unknown[] = [];
for (const step of steps) {
	if ("open" in step) {
		windows = openTrustWindow(windows, step.open);
		trace.push({ step: clone(step), windows: clone(windows) });
	} else if ("record" in step) {
		windows = recordTrustWindowEvidence(windows, step.record, current);
		trace.push({ step: clone(step), windows: clone(windows) });
	} else {
		const settlement = settleTrustWindows(windows, lookup, { turn: step.settle, at: AT });
		windows = settlement.windows;
		for (const adjustment of settlement.adjustments) {
			const entry = lookup(adjustment.kind, adjustment.id);
			if (entry) entry.trust = adjustment.trust;
		}
		trace.push({
			step: clone(step),
			windows: clone(windows),
			adjustments: clone(settlement.adjustments),
			settled: clone(settlement.settled),
			entries: clone(entries),
		});
	}
}

// --- pruning: 102 settled windows and an open one -----------------------------

let pruned: HarnessTrustWindows | undefined = { open: { ...openTrustWindow(undefined, { proposalId: "open", touched: ["memory:note"], claimedFingerprints: ["fa"], committedTurn: 0, untilTurn: 1 }).open } };
for (let index = 0; index < 102; index++) {
	const id = `s${String(index % 7)}${index}`;
	pruned = openTrustWindow(pruned, { proposalId: id, touched: [], claimedFingerprints: [], committedTurn: 0, untilTurn: 0 });
	pruned[id] = { ...pruned[id], outcome: "clean", settledTurn: index % 5 };
}
pruned = openTrustWindow(pruned, { proposalId: "last", touched: [], claimedFingerprints: ["fa"], committedTurn: 0, untilTurn: 0 });

writeFileSync(
	join(out, "trust.json"),
	`${JSON.stringify(
		{
			normalizeTrust: rawTrust.map((input) => ({ input, output: normalizeEntryTrust(input) ?? null })),
			normalizeWindows: rawWindows.map((input) => ({ input, output: normalizeTrustWindows(input) ?? null })),
			trace,
			pruned: Object.keys(pruned),
		},
		null,
		2,
	)}\n`,
);
