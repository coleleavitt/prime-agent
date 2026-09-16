#!/usr/bin/env node
/**
 * Compare two eval runs and say whether the difference is real.
 *
 * usage: node evals/report.mjs BASE.json AFTER.json [--control CONTROL.json] [--md OUT.md]
 *
 * BASE vs AFTER is the claim. CONTROL is a second BASE-equivalent run; the delta only counts as
 * improvement when it exceeds the noise CONTROL reveals. Without a control this prints the numbers
 * and explicitly refuses to call anything an improvement.
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

function load(p) {
	const d = JSON.parse(readFileSync(p, "utf8"));
	return d.results.map((r) => ({ ...r, tokens: (r.tokensIn ?? 0) + (r.tokensOut ?? 0), pass: r.pass ? 1 : 0 }));
}
const mean = (xs) => (xs.length ? xs.reduce((a, b) => a + b, 0) / xs.length : 0);

/** Per-task means, so a suite with one slow task does not swamp the aggregate. */
function byTask(rows) {
	const m = new Map();
	for (const r of rows) {
		if (!m.has(r.id)) m.set(r.id, []);
		m.get(r.id).push(r);
	}
	return m;
}

function agg(rows, key) {
	const t = byTask(rows);
	return mean([...t.values()].map((rs) => mean(rs.map((r) => r[key] ?? 0))));
}

function bar(v, max, width = 22) {
	if (max <= 0) return "";
	return "█".repeat(Math.max(0, Math.round((v / max) * width)));
}

function main() {
	const args = process.argv.slice(2);
	const files = args.filter((a) => !a.startsWith("--"));
	const ci = args.indexOf("--control");
	const mi = args.indexOf("--md");
	if (files.length < 2) {
		console.error("usage: node evals/report.mjs BASE.json AFTER.json [--control CONTROL.json] [--md OUT.md]");
		process.exit(1);
	}
	const base = load(files[0]);
	const after = load(files[1]);
	const control = ci >= 0 ? load(args[ci + 1]) : null;

	const out = [];
	const say = (s = "") => { out.push(s); console.log(s); };

	say(`# eval report`);
	say(``);
	say(`base:    ${files[0]}  (${base.length} runs)`);
	say(`after:   ${files[1]}  (${after.length} runs)`);
	say(`control: ${control ? `${args[ci + 1]}  (${control.length} runs)` : "NONE — no improvement will be declared"}`);
	say(``);
	say(`| metric | base | after | delta | noise | verdict |`);
	say(`|---|---|---|---|---|---|`);

	for (const m of METRICS) {
		const b = agg(base, m.key);
		const a = agg(after, m.key);
		const delta = a - b;
		const noise = control ? Math.abs(agg(control, m.key) - b) : null;
		const improved = m.better === "up" ? delta > 0 : delta < 0;
		let verdict;
		if (!control) verdict = "no control";
		else if (Math.abs(delta) <= noise) verdict = "within noise";
		else verdict = improved ? "**better**" : "**worse**";
		const sign = delta > 0 ? "+" : "";
		say(`| ${m.label} | ${m.fmt(b)} | ${m.fmt(a)} | ${sign}${m.fmt(delta)} | ${noise === null ? "—" : `±${m.fmt(noise)}`} | ${verdict} |`);
	}

	say(``);
	say(`## per task`);
	say(``);
	const ids = [...new Set([...base, ...after].map((r) => r.id))].sort();
	const maxTok = Math.max(...ids.map((id) => Math.max(agg(base.filter((r) => r.id === id), "tokens"), agg(after.filter((r) => r.id === id), "tokens"))), 1);
	for (const id of ids) {
		const b = base.filter((r) => r.id === id);
		const a = after.filter((r) => r.id === id);
		const bp = agg(b, "pass"), ap = agg(a, "pass");
		const bi = agg(b, "humanInterventions"), ai = agg(a, "humanInterventions");
		say(`  ${id}`);
		say(`    base  pass=${(bp * 100).toFixed(0)}% asks=${bi.toFixed(1)} turns=${agg(b, "turns").toFixed(1)} tok=${Math.round(agg(b, "tokens")).toLocaleString().padStart(8)} ${bar(agg(b, "tokens"), maxTok)}`);
		say(`    after pass=${(ap * 100).toFixed(0)}% asks=${ai.toFixed(1)} turns=${agg(a, "turns").toFixed(1)} tok=${Math.round(agg(a, "tokens")).toLocaleString().padStart(8)} ${bar(agg(a, "tokens"), maxTok)}`);
	}

	say(``);
	if (!control) {
		say(`> No control run supplied, so nothing here is called an improvement. Run the base phase twice`);
		say(`> and pass the second as \`--control\` — run-to-run variance on an LLM harness is large enough`);
		say(`> that a single pair of runs cannot distinguish learning from noise.`);
	}

	if (mi >= 0) {
		writeFileSync(args[mi + 1], out.join("\n"));
		console.log(`\nwrote ${args[mi + 1]}`);
	}
}

main();
