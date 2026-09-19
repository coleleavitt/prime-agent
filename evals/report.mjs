#!/usr/bin/env node
/**
 * Compare two eval runs and say whether the difference is real.
 *
 * usage: node evals/report.mjs BASE.json AFTER.json [--control CONTROL.json] [--md OUT.md]
 *                              [--iters N] [--seed N]
 *
 * BASE vs AFTER is the claim. Each file should hold several independent reps per task (--repeat 5 or
 * more in run.mjs); with one rep per task there is no variance to test and the report says so. For
 * every metric we compute a paired-by-task effect, a two-sided permutation p-value, and a bootstrap
 * 95% CI, then Holm-correct the p-values across the six metrics. Invalid rows are dropped and the
 * count is reported. There is no |C-A| > |A'-A| coin-flip rule any more; a control, if given, is
 * shown only as a reference arm.
 */
import { readFileSync, writeFileSync } from "node:fs";

const METRICS = [
	{ key: "pass", label: "pass rate", better: "up", fmt: (v) => `${(v * 100).toFixed(0)}%` },
	{ key: "humanInterventions", label: "human interventions", better: "down", fmt: (v) => v.toFixed(2) },
	{ key: "turns", label: "turns", better: "down", fmt: (v) => v.toFixed(1) },
	{ key: "tokens", label: "tokens", better: "down", fmt: (v) => Math.round(v).toLocaleString() },
	{ key: "toolCalls", label: "tool calls", better: "down", fmt: (v) => v.toFixed(1) },
	{ key: "wallMs", label: "wall seconds", better: "down", fmt: (v) => (v / 1000).toFixed(1) },
];

/** Deterministic RNG so a p-value and CI are reproducible from the same files + seed. */
export function mulberry32(seed) {
	let a = seed >>> 0;
	return () => {
		a |= 0;
		a = (a + 0x6d2b79f5) | 0;
		let t = Math.imul(a ^ (a >>> 15), 1 | a);
		t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
		return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
	};
}

/** Load a result file, drop invalid rows, and normalise the fields the report reads. */
export function loadRuns(path) {
	const d = JSON.parse(readFileSync(path, "utf8"));
	const all = d.results ?? [];
	const rows = all
		.filter((r) => !r.invalid)
		.map((r) => ({
			id: r.id,
			pass: r.pass ? 1 : 0,
			humanInterventions: r.humanInterventions ?? 0,
			turns: r.turns ?? 0,
			tokens: r.tokens ?? (r.tokensIn ?? 0) + (r.tokensOut ?? 0),
			toolCalls: r.toolCalls ?? 0,
			wallMs: r.wallMs ?? 0,
		}));
	return { rows, dropped: all.length - rows.length, total: all.length, provenance: d.provenance ?? null };
}

const mean = (xs) => (xs.length ? xs.reduce((a, b) => a + b, 0) / xs.length : 0);

/** Map task id -> array of that metric's values. */
function samplesByTask(rows, key) {
	const m = new Map();
	for (const r of rows) {
		if (!m.has(r.id)) m.set(r.id, []);
		m.get(r.id).push(r[key] ?? 0);
	}
	return m;
}

/** The effect: mean over shared tasks of (mean(after) - mean(base)). Per-task means keep one busy
 *  task from swamping the aggregate; only tasks present in both arms count. */
function effect(baseByTask, afterByTask) {
	const ids = [...baseByTask.keys()].filter((id) => afterByTask.has(id));
	if (!ids.length) return { value: 0, ids: [] };
	const deltas = ids.map((id) => mean(afterByTask.get(id)) - mean(baseByTask.get(id)));
	return { value: mean(deltas), ids };
}

/**
 * Two-sided paired permutation test. Within each task the base+after samples are pooled and
 * re-split into the original group sizes; the aggregate effect is recomputed each iteration. The
 * p-value is the fraction of permutations whose |effect| is at least the observed |effect|.
 */
export function permutationTest(baseByTask, afterByTask, { iters = 10000, rng = Math.random } = {}) {
	const obs = effect(baseByTask, afterByTask);
	const ids = obs.ids;
	if (!ids.length) return { effect: 0, p: 1, ids: [] };
	// If no task has more than one sample in either arm, permutation cannot move anything.
	const anyVariance = ids.some((id) => baseByTask.get(id).length + afterByTask.get(id).length > 2);
	if (!anyVariance) return { effect: obs.value, p: null, ids, note: "n=1 per task: no variance to test" };
	const pools = ids.map((id) => ({ pool: [...baseByTask.get(id), ...afterByTask.get(id)], nb: baseByTask.get(id).length, na: afterByTask.get(id).length }));
	let ge = 0;
	const absObs = Math.abs(obs.value);
	for (let it = 0; it < iters; it++) {
		let sum = 0;
		for (const { pool, nb, na } of pools) {
			// Fisher-Yates shuffle of a copy, then split.
			const a = pool.slice();
			for (let i = a.length - 1; i > 0; i--) {
				const j = Math.floor(rng() * (i + 1));
				[a[i], a[j]] = [a[j], a[i]];
			}
			const b = mean(a.slice(0, nb));
			const af = mean(a.slice(nb, nb + na));
			sum += af - b;
		}
		if (Math.abs(sum / pools.length) >= absObs - 1e-12) ge++;
	}
	return { effect: obs.value, p: (ge + 1) / (iters + 1), ids };
}

/**
 * Bootstrap 95% CI for the aggregate effect: resample tasks with replacement, and within each task
 * resample its base and after reps with replacement, then recompute the effect.
 */
export function bootstrapCI(baseByTask, afterByTask, { iters = 10000, rng = Math.random } = {}) {
	const ids = [...baseByTask.keys()].filter((id) => afterByTask.has(id));
	if (!ids.length) return { lo: 0, hi: 0, ids: [] };
	const pick = (arr) => arr[Math.floor(rng() * arr.length)];
	const effects = [];
	for (let it = 0; it < iters; it++) {
		const taskSample = ids.map(() => pick(ids));
		const deltas = taskSample.map((id) => {
			const b = baseByTask.get(id);
			const a = afterByTask.get(id);
			const bm = mean(b.map(() => pick(b)));
			const am = mean(a.map(() => pick(a)));
			return am - bm;
		});
		effects.push(mean(deltas));
	}
	effects.sort((x, y) => x - y);
	const at = (q) => effects[Math.min(effects.length - 1, Math.max(0, Math.floor(q * effects.length)))];
	return { lo: at(0.025), hi: at(0.975), ids };
}

/** Holm-Bonferroni across a list of p-values; nulls pass through unchanged. */
export function holm(pvals) {
	const idx = pvals.map((p, i) => ({ p, i })).filter((x) => x.p != null).sort((a, b) => a.p - b.p);
	const adj = pvals.map(() => null);
	let prev = 0;
	idx.forEach((x, rank) => {
		const a = Math.min(1, Math.max(prev, x.p * (idx.length - rank)));
		adj[x.i] = a;
		prev = a;
	});
	return adj;
}

function main() {
	const args = process.argv.slice(2);
	const files = [];
	let controlFile = null, mdFile = null, iters = 10000, seed = 1;
	for (let i = 0; i < args.length; i++) {
		const a = args[i];
		if (a === "--control") controlFile = args[++i];
		else if (a === "--md") mdFile = args[++i];
		else if (a === "--iters") iters = Number(args[++i]);
		else if (a === "--seed") seed = Number(args[++i]);
		else if (!a.startsWith("--")) files.push(a);
	}
	if (files.length < 2) {
		console.error("usage: node evals/report.mjs BASE.json AFTER.json [--control CONTROL.json] [--md OUT.md] [--iters N] [--seed N]");
		process.exit(1);
	}
	const base = loadRuns(files[0]);
	const after = loadRuns(files[1]);
	const control = controlFile ? loadRuns(controlFile) : null;

	const out = [];
	const say = (s = "") => { out.push(s); console.log(s); };

	say(`# eval report`);
	say(``);
	say(`base:    ${files[0]}  (${base.rows.length} valid, ${base.dropped} dropped of ${base.total})`);
	say(`after:   ${files[1]}  (${after.rows.length} valid, ${after.dropped} dropped of ${after.total})`);
	if (control) say(`control: ${controlFile}  (${control.rows.length} valid, ${control.dropped} dropped of ${control.total})`);
	const bv = base.provenance?.version, av = after.provenance?.version;
	if (bv || av) say(`build:   base ${bv ?? "?"} / after ${av ?? "?"}${bv && av && bv !== av ? "  ** builds differ **" : ""}`);
	say(``);

	const rng = mulberry32(seed);
	const rows = METRICS.map((m) => {
		const b = samplesByTask(base.rows, m.key);
		const a = samplesByTask(after.rows, m.key);
		const perm = permutationTest(b, a, { iters, rng });
		const ci = bootstrapCI(b, a, { iters, rng });
		const bMean = mean([...perm.ids].map((id) => mean(b.get(id))));
		const aMean = mean([...perm.ids].map((id) => mean(a.get(id))));
		return { m, bMean, aMean, delta: perm.effect, p: perm.p, note: perm.note, lo: ci.lo, hi: ci.hi };
	});
	const adjP = holm(rows.map((r) => r.p));

	say(`| metric | base | after | delta | 95% CI | p (Holm) | verdict |`);
	say(`|---|---|---|---|---|---|---|`);
	rows.forEach((r, i) => {
		const improved = r.m.better === "up" ? r.delta > 0 : r.delta < 0;
		// Decision rule: the Holm-adjusted permutation p-value. The bootstrap CI is shown alongside as
		// the effect's magnitude; a "significant" p with a CI that straddles 0 means the tasks disagree.
		const sig = adjP[i] != null && adjP[i] < 0.05;
		const ciStraddles = r.lo <= 0 && r.hi >= 0;
		let verdict;
		if (r.p == null) verdict = "n=1 (no test)";
		else if (sig) verdict = `${improved ? "**better**" : "**worse**"}${ciStraddles ? " (CI straddles 0)" : ""}`;
		else verdict = "n.s.";
		const sign = r.delta > 0 ? "+" : "";
		const pStr = r.p == null ? "—" : adjP[i] != null ? adjP[i].toFixed(3) : r.p.toFixed(3);
		say(`| ${r.m.label} | ${r.m.fmt(r.bMean)} | ${r.m.fmt(r.aMean)} | ${sign}${r.m.fmt(r.delta)} | [${r.m.fmt(r.lo)}, ${r.m.fmt(r.hi)}] | ${pStr} | ${verdict} |`);
	});

	say(``);
	say(`## per task`);
	say(``);
	const ids = [...new Set([...base.rows, ...after.rows].map((r) => r.id))].sort();
	for (const id of ids) {
		const b = base.rows.filter((r) => r.id === id);
		const a = after.rows.filter((r) => r.id === id);
		const p = (rs, k) => mean(rs.map((r) => r[k]));
		say(`  ${id}  (base n=${b.length}, after n=${a.length})`);
		say(`    base  pass=${(p(b, "pass") * 100).toFixed(0)}% hi=${p(b, "humanInterventions").toFixed(1)} turns=${p(b, "turns").toFixed(1)} tok=${Math.round(p(b, "tokens")).toLocaleString()}`);
		say(`    after pass=${(p(a, "pass") * 100).toFixed(0)}% hi=${p(a, "humanInterventions").toFixed(1)} turns=${p(a, "turns").toFixed(1)} tok=${Math.round(p(a, "tokens")).toLocaleString()}`);
	}

	say(``);
	const minReps = Math.min(...ids.map((id) => Math.min(base.rows.filter((r) => r.id === id).length, after.rows.filter((r) => r.id === id).length)));
	if (minReps < 5) {
		say(`> Only ${minReps} rep(s) per task in the thinner arm. Run each phase with \`--repeat 5\` (or more)`);
		say(`> so the permutation test and bootstrap CI have something to work with; below that, treat every`);
		say(`> verdict as provisional.`);
	}
	if (control) {
		say(``);
		say(`> control is shown as a reference arm only; it no longer gates the verdict. If base-vs-control`);
		say(`> shows a "better"/"worse" verdict on any metric, the suite is too noisy to trust base-vs-after.`);
	}

	if (mdFile) {
		writeFileSync(mdFile, out.join("\n"));
		console.log(`\nwrote ${mdFile}`);
	}
}

const invokedDirectly = process.argv[1]?.endsWith("report.mjs");
if (invokedDirectly) main();
