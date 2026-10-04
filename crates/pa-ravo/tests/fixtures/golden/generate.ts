// The generator of the TS goldens in this directory (`tests/golden.rs` replays
// every scenario in Rust and compares the results as whole JSON values, and
// the judge prompt as exact text).
//
// To regenerate: copy `packages/coding-agent/src/core/ravo/` and
// `packages/coding-agent/src/core/refinement/ravo.ts` from
// `perf/session-catalog-resume` into a build directory (keeping that layout),
// copy `stubs/*` over its root (they replace `learning-index`,
// `toolforge/ledger`, `orphan-process-journal`, `refinement/skill-dry-run` and
// `@earendil-works/pi-ai`), put this file at its root, then
//   esbuild generate.ts --bundle --platform=node --format=esm \
//     --alias:@earendil-works/pi-ai=./pi-ai.ts --outfile=gen.mjs
//   node gen.mjs <this directory>
import { writeFileSync } from "node:fs";
import { join } from "node:path";
import { judgeRequests, setJudgeReply } from "./pi-ai.ts";
import {
	authorizeAssistedRavo,
	emptyAssistedRavoState,
	normalizeAssistedRavoState,
	ravoArtifactDigest,
} from "./ravo/authority.ts";
import { canonicalJson, sha256 } from "./ravo/canonical-json.ts";
import type { FailureRecord } from "./ravo/failure-ledger.ts";
import {
	emptyRavoState,
	type RavoEvaluation,
	type RavoState,
	ravoMarkProvisional,
	ravoObserveChampion,
	ravoStep,
} from "./ravo/reducer.ts";
import type { RefereeVerdict } from "./ravo/referee.ts";
import { parseJudgeVerdict, RAVO_DEFAULT_CONFIG, ravoEvaluateProposal, ravoFastScreen } from "./refinement/ravo.ts";

const out = process.argv[2];
const write = (name: string, value: unknown) =>
	writeFileSync(join(out, name), `${JSON.stringify(value, null, 2)}\n`);
const clone = <T>(value: T): T => JSON.parse(JSON.stringify(value)) as T;

// --- reducer -----------------------------------------------------------------

const pool = {
	criteria: [
		"scope",
		"evidence",
		"failure:0a1b",
		"referee:0a1b",
		"arc:all-levels",
		"Novelty",
		"novelty-2",
		"novelty_2",
	].map((id) => ({ id, seedWeight: 1, currentWeight: 1 })),
};
const passAll = (ids: string[], fail: string[] = []) =>
	ids.map((criterionId) => ({ criterionId, status: fail.includes(criterionId) ? "fail" : "pass" }) as const);
const ids = pool.criteria.map((criterion) => criterion.id);
const config = { screenThreshold: 50, epsilon: 1, deepTolerance: 10 };
const steps: { name: string; proposal: { id: string; artifact: unknown }; evaluation: RavoEvaluation }[] = [
	{
		name: "commit missing one criterion",
		proposal: { id: "p1", artifact: { summary: "first", edits: [] } },
		evaluation: {
			proposalId: "p1",
			screen: { status: "pass", score: 80 },
			deep: { status: "pass", score: 70, detail: "good" },
			criteria: passAll(ids, ["scope"]).map((item) => (item.criterionId === "evidence" ? { ...item, detail: "ok" } : item)),
		},
	},
	{
		name: "already evaluated",
		proposal: { id: "p1", artifact: null },
		evaluation: {
			proposalId: "p1",
			screen: { status: "pass", score: 80 },
			deep: { status: "pass", score: 90 },
			criteria: passAll(ids),
		},
	},
	{
		name: "screen below threshold",
		proposal: { id: "p2", artifact: 1 },
		evaluation: { proposalId: "p2", screen: { status: "pass", score: 40 }, deep: { status: "pass", score: 99 }, criteria: [] },
	},
	{
		name: "deep under the slacked bar",
		proposal: { id: "p3", artifact: 1 },
		evaluation: {
			proposalId: "p3",
			screen: { status: "pass", score: 100 },
			deep: { status: "pass", score: 59 },
			criteria: passAll(ids),
		},
	},
	{
		name: "pressured criterion misses past epsilon",
		proposal: { id: "p4", artifact: 1 },
		evaluation: {
			proposalId: "p4",
			screen: { status: "pass", score: 100 },
			deep: { status: "pass", score: 65 },
			criteria: passAll(ids, ["scope"]),
		},
	},
	{
		name: "duplicate observation",
		proposal: { id: "p5", artifact: 1 },
		evaluation: {
			proposalId: "p5",
			screen: { status: "pass", score: 100 },
			deep: { status: "pass", score: 90 },
			criteria: [...passAll(ids), { criterionId: "scope", status: "pass" }],
		},
	},
	{
		name: "unknown criterion",
		proposal: { id: "p6", artifact: 1 },
		evaluation: {
			proposalId: "p6",
			screen: { status: "pass", score: 100 },
			deep: { status: "pass", score: 90 },
			criteria: [...passAll(ids), { criterionId: "nope", status: "pass" }],
		},
	},
	{
		name: "deep abstains",
		proposal: { id: "p7", artifact: 1 },
		evaluation: { proposalId: "p7", screen: { status: "pass", score: 100 }, deep: { status: "abstain" }, criteria: [] },
	},
	{
		name: "second commit, abstentions counted",
		proposal: { id: "p8", artifact: { second: true } },
		evaluation: {
			proposalId: "p8",
			screen: { status: "pass", score: 60 },
			deep: { status: "pass", score: 75 },
			criteria: passAll(ids).filter((item) => item.criterionId !== "novelty_2"),
		},
	},
];
let state: RavoState = emptyRavoState(pool);
const reducer = steps.map((step) => {
	const before = clone(state);
	const result = ravoStep(state, step.proposal as never, step.evaluation, config);
	state = result.state;
	return { name: step.name, state: before, proposal: step.proposal, evaluation: step.evaluation, config, result: clone(result) };
});
const marked = ravoMarkProvisional(state, "p1", {
	claimedFingerprints: ["b2", "a1", "b2", "", "A1"],
	window: { committedTurn: 40, untilTurn: 60, clock: "ordinal" },
});
const provisional = {
	marked: clone(marked),
	inside: clone(ravoObserveChampion(marked, "p1", ["a1", "zz"], 47)),
	outside: clone(ravoObserveChampion(marked, "p1", ["a1"], 61)),
	unclaimed: clone(ravoObserveChampion(marked, "p1", ["zz"], 47)),
	unknownChampion: clone(ravoMarkProvisional(state, "nope", { claimedFingerprints: ["x"] })),
	invalidWindow: clone(
		ravoMarkProvisional(state, "p8", { claimedFingerprints: ["x"], window: { committedTurn: 9, untilTurn: 3 } }),
	),
};
write("reducer.json", { steps: reducer, provisional });

// --- authority ----------------------------------------------------------------

const verdict = (fingerprintId: string, status: RefereeVerdict["status"]): RefereeVerdict => ({
	fingerprintId,
	status,
	detail: `${status}: detail for ${fingerprintId}`,
});
const baseline = { schema: 1, entries: { memory: { m1: { id: "m1", title: "Tactic" } } } };
const artifact = { summary: "s", rationale: "r", expectedOutcome: "e", edits: [{ action: "create", kind: "memory" }] };
type AuthorityInput = Parameters<typeof authorizeAssistedRavo>[0];
const authorityCases: { name: string; input: AuthorityInput }[] = [];
const authority: unknown[] = [];
const authorize = (name: string, input: AuthorityInput) => {
	const result = authorizeAssistedRavo(input);
	authority.push({ name, input: clone(input), result: clone(result) });
	return result;
};
const directed = authorize("directed commit claims nothing", {
	proposalId: "a1",
	artifact,
	baseline,
	fastScore: 100,
	observation: { status: "pass", score: 72, detail: "fine", failedCriteria: [] },
	unclaimedCommit: "unmeasured",
});
const measured = authorize("measured commit opens a window", {
	proposalId: "a2",
	artifact,
	baseline,
	fastScore: 100,
	observation: { status: "pass", score: 80, detail: "fixes f1", failedCriteria: [], addressedFingerprints: ["f1"] },
	state: directed.nextState,
	epsilon: 1,
	deepTolerance: 10,
	failureOpponents: ["failure:f1", "failure:f2", "nonsense"],
	refereeVerdicts: [verdict("f1", "not_applicable")],
	turn: 40,
	turnClock: "ordinal",
	unclaimedCommit: "unmeasured",
});
authorize("an upheld claim misses two opponents", {
	proposalId: "a3",
	artifact,
	baseline,
	fastScore: 100,
	observation: { status: "pass", score: 90, failedCriteria: [], addressedFingerprints: ["f1"] },
	state: measured.nextState,
	deepTolerance: 10,
	failureOpponents: ["failure:f1"],
	refereeVerdicts: [verdict("f1", "upheld")],
	turn: 41,
	turnClock: "ordinal",
});
authorize("a failure refine that claims nothing", {
	proposalId: "a4",
	artifact,
	baseline,
	fastScore: 100,
	observation: { status: "pass", score: 90, failedCriteria: [] },
	state: measured.nextState,
	deepTolerance: 10,
	failureOpponents: [],
	unclaimedCommit: "reject",
});
authorize("the judge errored", {
	proposalId: "a5",
	artifact,
	baseline,
	fastScore: 100,
	observation: { status: "error", detail: "deep judge unavailable" },
	state: measured.nextState,
	failureOpponents: ["failure:f1"],
});
authorize("the screen failed", {
	proposalId: "a6",
	artifact,
	baseline,
	fastScore: 40,
	observation: { status: "abstain", detail: "structural screen scored 40 below threshold 50" },
});
const withDormant: RavoState = clone(measured.nextState);
withDormant.opponents.criteria.push(
	{ id: "arc:all-levels", seedWeight: 1, currentWeight: 4 },
	{ id: "referee:f9", seedWeight: 1, currentWeight: 2 },
);
authorize("dormant criteria pass, a cleared claim passes, no evidence fails closed", {
	proposalId: "a7",
	artifact: { other: true },
	baseline,
	fastScore: 75,
	observation: { status: "pass", score: 85, failedCriteria: ["minimality"], addressedFingerprints: ["f3", "f4"] },
	state: withDormant,
	epsilon: 3,
	deepTolerance: 10,
	failureOpponents: ["failure:f3", "failure:f4"],
	refereeVerdicts: [verdict("f3", "cleared"), verdict("f4", "no_evidence")],
	turn: 50,
	turnClock: "local-ordinal",
	observationWindowTurns: 5,
});
authorize("unmeasured policy without a turn", {
	proposalId: "a8",
	artifact,
	baseline,
	fastScore: 100,
	observation: { status: "pass", score: 99, failedCriteria: [], addressedFingerprints: ["f5"] },
	state: measured.nextState,
	deepTolerance: 10,
	failureOpponents: ["failure:f5"],
	refereeVerdicts: [verdict("f5", "unverifiable")],
});
void authorityCases;
write("authority.json", authority);

// --- normalization and digests ---------------------------------------------------

const normalizeInputs: unknown[] = [
	null,
	[],
	"x",
	{ lineage: [], evaluatedProposalIds: [], opponents: null },
	{
		lineage: [
			{ id: "r1", score: 60, missedCriteria: ["scope", 3], summary: "first", created_at: "2026-01-01T00:00:00.000Z" },
			{ id: "", score: 70 },
			{ id: "r2", score: -1 },
			{ id: "r3", score: 75 },
		],
		evaluator: {
			criteria: [
				{ id: "scope", weight: 4, description: "d" },
				{ id: "evidence", weight: 0 },
				{ id: "novelty", weight: 2.5 },
			],
		},
	},
	clone(measured.nextState),
	(() => {
		const unknownClock = clone(measured.nextState) as unknown as { lineage: { provisional?: { clock?: string } }[] };
		const window = unknownClock.lineage.at(-1)?.provisional;
		if (window) window.clock = "wall-clock";
		return unknownClock;
	})(),
	(() => {
		const broken = clone(measured.nextState);
		broken.championId = "nobody";
		return broken;
	})(),
];
write(
	"normalize.json",
	normalizeInputs.map((input) => ({ input, output: clone(normalizeAssistedRavoState(input)) })),
);

const digestInputs: unknown[] = [
	{ b: 1, a: [true, null, "x"], "é": { z: 0.1, y: -0 }, A: 1e21, c: 1.5e-7, d: 2e20, e: 1.2e-6 },
	"plain   text \u0007",
	[1, 2.5, { k: "v" }],
	artifact,
];
write(
	"digests.json",
	digestInputs.map((value) => ({
		value,
		canonical: canonicalJson(value),
		sha256: sha256(canonicalJson(value)),
		artifactDigest: ravoArtifactDigest(value as never),
	})),
);

// --- the gate -------------------------------------------------------------------

const record = (
	id: string,
	kind: string,
	count: number,
	extra: Partial<FailureRecord> = {},
	fingerprint: Record<string, unknown> = {},
): FailureRecord =>
	({
		fingerprint: { id, kind, message: `message of ${id}`, ...fingerprint },
		count,
		firstSeenTurn: 1,
		lastSeenTurn: count + 3,
		firstSeenAt: "2026-01-01T00:00:00.000Z",
		lastSeenAt: "2026-01-01T00:05:00.000Z",
		excerpt: `Traceback ...\nModuleNotFoundError: No module named 'foo'  ${id}`,
		addressedByProposalIds: [],
		...extra,
	}) as FailureRecord;
const verifiedFoo = record(
	"1111aaaa2222bbbb",
	"python_exception",
	3,
	{ replayCases: [{ language: "python", source: "import foo", exceptionClass: "ModuleNotFoundError", verifiedAt: "2026-01-01T00:01:00.000Z" }] },
	{ source: "ipython", exceptionClass: "ModuleNotFoundError" },
);
const toolError = record("3333cccc4444dddd", "tool_error", 2, {}, { source: "bash" });
const memoryEdit = { action: "create", kind: "memory", title: "Use tactic A", content: "Always do A." };
const skillEdit = {
	action: "update",
	kind: "skill",
	id: "fetcher",
	title: "Fetcher",
	content: "Fetch things.",
	reference: { type: "python", import: "foo.bar", callable: "run" },
	arguments: { url: "string" },
};
const proposal = (edits: unknown[], summary = "teach the tactic") => ({
	summary,
	rationale: "seen twice",
	expectedOutcome: "no repeat",
	edits,
});
type GateOptions = Parameters<typeof ravoEvaluateProposal>[1];
const gateCases: {
	name: string;
	proposal: ReturnType<typeof proposal>;
	options: Omit<GateOptions, "model" | "apiKey" | "config">;
	reply: { text?: string; error?: string };
}[] = [
	{
		name: "a directed commit that claims nothing",
		proposal: proposal([memoryEdit]),
		options: {
			state: emptyAssistedRavoState(),
			validEdits: 1,
			conversationText: "[User]: do it twice",
			harnessOverview: "memory: 0",
			baseline,
			proposalId: "g1",
			refineKind: "directed",
		},
		reply: { text: '{"verdict":"pass","score":72,"failedCriteria":[],"rationale":"fine"}' },
	},
	{
		name: "an unverifiable replay charges the claim",
		proposal: proposal([skillEdit, memoryEdit]),
		options: {
			state: emptyAssistedRavoState(),
			validEdits: 2,
			conversationText: "[User]: fetch it",
			harnessOverview: "skill: 1",
			baseline,
			proposalId: "g2",
			recurringFailures: [verifiedFoo, toolError],
			turn: 40,
			turnClock: "ordinal",
			refineKind: "directed",
		},
		reply: {
			text: `{"verdict":"pass","score":80,"failedCriteria":[],"addressedFingerprints":["${verifiedFoo.fingerprint.id}","unknownfp"],"rationale":"fixes the import"}`,
		},
	},
	{
		name: "a failure refine that claims nothing",
		proposal: proposal([memoryEdit]),
		options: {
			state: emptyAssistedRavoState(),
			validEdits: 1,
			conversationText: "",
			harnessOverview: "",
			baseline,
			proposalId: "g3",
			recurringFailures: [toolError],
			turn: 7,
			turnClock: "local-ordinal",
			refineKind: "failure",
		},
		reply: { text: '{"verdict":"pass","score":90,"failedCriteria":[],"addressedFingerprints":[],"rationale":"unrelated"}' },
	},
	{
		name: "the judge is unavailable",
		proposal: proposal([memoryEdit]),
		options: {
			state: emptyAssistedRavoState(),
			validEdits: 1,
			conversationText: "x",
			harnessOverview: "y",
			baseline,
			proposalId: "g4",
			recurringFailures: [toolError],
			refineKind: "directed",
		},
		reply: { error: "rate limited" },
	},
	{
		name: "the structural screen fails",
		proposal: proposal([memoryEdit, { action: "update", kind: "memory" }, { action: "delete", kind: "skill" }]),
		options: {
			state: emptyAssistedRavoState(),
			validEdits: 1,
			conversationText: "x",
			harnessOverview: "y",
			baseline,
			proposalId: "g5",
			recurringFailures: [toolError],
			refineKind: "checkpoint",
		},
		reply: { text: "never asked" },
	},
	{
		name: "a fenced reply with a status token and a string score",
		proposal: proposal([memoryEdit]),
		options: {
			state: emptyAssistedRavoState(),
			validEdits: 1,
			conversationText: "[Assistant]: done",
			harnessOverview: "memory: 1",
			baseline,
			proposalId: "g6",
			recurringFailures: [toolError, verifiedFoo],
			turn: 40,
			turnClock: "ordinal",
			refineKind: "failure",
		},
		reply: {
			text: `Here you go:\n\`\`\`json\n{"status":" Accept ","score":"88.5","failedCriteria":["novelty"],"addressedFingerprints":["${toolError.fingerprint.id}"],"rationale":"targets the tool error"}\n\`\`\`\ntrailing {`,
		},
	},
];
const gate: unknown[] = [];
for (const item of gateCases) {
	setJudgeReply(item.reply);
	judgeRequests.length = 0;
	const report = await ravoEvaluateProposal(item.proposal as never, {
		...item.options,
		config: RAVO_DEFAULT_CONFIG,
		model: { maxTokens: 8000 } as never,
		apiKey: "",
	});
	gate.push({
		name: item.name,
		proposal: item.proposal,
		options: clone(item.options),
		reply: item.reply,
		requests: clone(judgeRequests),
		report: clone(report),
	});
}
write("gate.json", gate);

write("judge.json", {
	verdicts: [" PASS ", "accept", "passed", "true", "fail", "reject", "false", "", "maybe", null, true, 1].map((value) => ({
		value,
		verdict: parseJudgeVerdict(value),
	})),
	screens: [
		[0, 0],
		[3, 1],
		[3, 2],
		[8, 5],
		[2, 1],
	].map(([edits, valid]) => ({ edits, valid, score: ravoFastScreen({ edits: new Array(edits).fill({}) } as never, valid) })),
});
