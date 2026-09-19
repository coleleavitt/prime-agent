#!/usr/bin/env node
/**
 * Black-box harness benchmark runner.
 *
 * Drives `prime-agent --print --mode json` against each task in its own workspace with an isolated
 * agent-state dir, then runs the task's `verify.sh` as the referee. Never touches ~/.prime/agent.
 */
import { spawn } from "node:child_process";
import { mkdtempSync, mkdirSync, readdirSync, readFileSync, writeFileSync, existsSync, rmSync, cpSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const TASKS = join(HERE, "tasks");

function parseArgs(argv) {
	const out = { phase: "cold", out: null, state: null, only: null, repeat: 1, keep: false, bin: "prime-agent", model: null, delay: 0 };
	for (let i = 0; i < argv.length; i++) {
		const a = argv[i];
		if (a === "--phase") out.phase = argv[++i];
		else if (a === "--out") out.out = argv[++i];
		else if (a === "--state") out.state = argv[++i];
		else if (a === "--only") out.only = argv[++i];
		else if (a === "--repeat") out.repeat = Number(argv[++i]);
		else if (a === "--bin") out.bin = argv[++i];
		else if (a === "--model") out.model = argv[++i];
		else if (a === "--delay") out.delay = Number(argv[++i]);
		else if (a === "--seed-harness") out.seedHarness = true;
		else if (a === "--seed-from") out.seedFrom = argv[++i];
		else if (a === "--keep") out.keep = true;
		else if (a === "--help" || a === "-h") out.help = true;
	}
	return out;
}

function loadTasks(only) {
	if (!existsSync(TASKS)) return [];
	return readdirSync(TASKS, { withFileTypes: true })
		.filter((d) => d.isDirectory())
		.map((d) => {
			const dir = join(TASKS, d.name);
			const spec = JSON.parse(readFileSync(join(dir, "task.json"), "utf8"));
			return { ...spec, id: spec.id ?? d.name, dir };
		})
		.filter((t) => !only || t.id === only || (t.tags ?? []).includes(only))
		.sort((a, b) => a.id.localeCompare(b.id));
}

function sh(cmd, cwd, env, timeoutMs) {
	return new Promise((res) => {
		const p = spawn("bash", ["-lc", cmd], { cwd, env, stdio: ["ignore", "pipe", "pipe"] });
		let so = "", se = "", killed = false;
		const timer = setTimeout(() => { killed = true; p.kill("SIGKILL"); }, timeoutMs);
		p.stdout.on("data", (d) => { so += d; });
		p.stderr.on("data", (d) => { se += d; });
		p.on("close", (code) => { clearTimeout(timer); res({ code, stdout: so, stderr: se, killed }); });
		p.on("error", (e) => { clearTimeout(timer); res({ code: -1, stdout: so, stderr: `${se}${e.message}`, killed }); });
	});
}

/** Fold the --mode json event stream into the metrics we score on. */
function foldEvents(stdout) {
	const m = { turns: 0, toolCalls: 0, tools: {}, tokensIn: 0, tokensOut: 0, errors: 0, asks: 0, events: 0, stopReason: null };
	for (const line of stdout.split("\n")) {
		const s = line.trim();
		if (!s.startsWith("{")) continue;
		let e;
		try { e = JSON.parse(s); } catch { continue; }
		m.events++;
		const t = e.type;
		if (t === "turn_end" || t === "turn-end") m.turns++;
		if (t === "tool_execution_start" || t === "tool_call" || t === "tool-start") {
			m.toolCalls++;
			const name = e.toolName ?? e.tool ?? e.name;
			if (name) m.tools[name] = (m.tools[name] ?? 0) + 1;
		}
		// An ask is any point the agent handed control back expecting a human.
		if (t === "ask" || t === "permission_request" || t === "question") m.asks++;
		if (t === "error" || e.isError === true) m.errors++;
		const u = e.usage ?? e.message?.usage;
		if (u) {
			m.tokensIn += u.input ?? u.inputTokens ?? u.promptTokens ?? 0;
			m.tokensOut += u.output ?? u.outputTokens ?? u.completionTokens ?? 0;
		}
		if (e.stopReason) m.stopReason = e.stopReason;
	}
	return m;
}

/** Every provider/turn errorMessage in a --mode json stream, first line only. */
function collectRunErrors(stdout) {
	const out = [];
	for (const line of stdout.split("\n")) {
		const s = line.trim();
		if (!s.startsWith("{")) continue;
		let e;
		try { e = JSON.parse(s); } catch { continue; }
		const msg = e?.message?.errorMessage ?? e?.errorMessage;
		if (msg) out.push(String(msg).split("\n")[0].slice(0, 200));
	}
	return out;
}

async function runOne(task, opts, stateDir) {
	const ws = mkdtempSync(join(tmpdir(), `eval-${task.id}-`));
	const agentDir = stateDir ?? mkdtempSync(join(tmpdir(), `eval-state-${task.id}-`));
	mkdirSync(agentDir, { recursive: true });

	// Credentials live in the agent-state dir, so a fully isolated dir has no auth at all.
	// Copy ONLY auth.json + settings.json forward: the run stays hermetic for memories, skills,
	// prompt notes and the failure ledger — the things being measured — while still being able
	// to reach a provider. Without this every task fails with 0 turns and scores as a task failure.
	for (const f of ["auth.json", "settings.json"]) {
		const src = join(process.env.HOME ?? "", ".prime", "agent", f);
		const dst = join(agentDir, f);
		if (existsSync(src) && !existsSync(dst)) cpSync(src, dst);
	}

	// --seed-harness copies the REAL global harness (memories, skills, prompt notes) into the
	// isolated state. Without it, a task that exists to test a stored memory cannot pass in any
	// phase, because the memory under test was never present. This turns "did the study phase
	// learn?" into the separate, answerable question "does an existing memory change behaviour?"
	if (opts.seedHarness) {
		const src = join(process.env.HOME ?? "", ".prime", "agent", "harness");
		const dst = join(agentDir, "harness");
		if (existsSync(src) && !existsSync(dst)) cpSync(src, dst, { recursive: true });
	}
	// --seed-from DIR copies an arbitrary prepared state (e.g. a single prompt note) into the
	// isolated agent dir. Lets a candidate harness fix be A/B tested without editing the product.
	if (opts.seedFrom) {
		const src = resolve(opts.seedFrom);
		if (existsSync(src) && !existsSync(join(agentDir, "harness"))) cpSync(src, agentDir, { recursive: true });
	}

	const env = {
		...process.env,
		PRIME_AGENT_CODING_AGENT_DIR: agentDir,
		PI_CODING_AGENT_DIR: agentDir,
		GIT_AUTHOR_NAME: "eval", GIT_AUTHOR_EMAIL: "eval@local",
		GIT_COMMITTER_NAME: "eval", GIT_COMMITTER_EMAIL: "eval@local",
		CI: "1",
	};

	if (existsSync(join(task.dir, "setup.sh"))) {
		const r = await sh(`bash ${JSON.stringify(join(task.dir, "setup.sh"))}`, ws, env, 120_000);
		if (r.code !== 0) {
			if (!opts.keep) rmSync(ws, { recursive: true, force: true });
			return { id: task.id, pass: false, phase: opts.phase, error: `setup failed: ${r.stderr.slice(-400)}` };
		}
	}

	const modelArg = opts.model ? ` --model ${JSON.stringify(opts.model)}` : "";
	const cmd = `${opts.bin} --print --mode json${modelArg} ${JSON.stringify(task.prompt)}`;
	// A task may declare `followups: [...]`: further prompts delivered with --continue into the
	// SAME session. This is what a real operator correction looks like, and a correction is one
	// of the few things that actually triggers the refinement loop. A study task with no
	// followup teaches the harness nothing, because a quiet success has no trigger.
	const followups = Array.isArray(task.followups) ? task.followups : [];

	// A 429 is transient and says nothing about the harness; a 401 is structural. Retry the whole
	// task on rate limits with exponential backoff, and only give up after the last attempt. The
	// agent's own auto-retry is 3 x 2s, far too fast for an account-level limit.
	const RETRYABLE = /rate limit|429|overloaded|529|ETIMEDOUT|ECONNRESET/i;
	let run, metrics, wallMs;
	const backoff = [30_000, 60_000, 120_000, 240_000];
	for (let attempt = 0; ; attempt++) {
		const t0 = Date.now();
		run = await sh(cmd, ws, env, task.timeoutMs ?? 600_000);
		wallMs = Date.now() - t0;
		metrics = foldEvents(run.stdout);
		const transient = collectRunErrors(run.stdout).some((m) => RETRYABLE.test(m));
		if (!transient || attempt >= backoff.length) break;
		const wait = backoff[attempt];
		process.stdout.write(`(rate limited, retry in ${Math.round(wait / 1000)}s) `);
		await new Promise((r) => setTimeout(r, wait));
	}

	// Expose the run to the verifier so a task can assert on BEHAVIOUR (which tools were called,
	// whether a command was repeated, whether a skill was used) and not only on the end state.
	const evalDir = join(ws, ".eval");
	mkdirSync(evalDir, { recursive: true });
	writeFileSync(join(evalDir, "events.jsonl"), run.stdout);
	writeFileSync(join(evalDir, "metrics.json"), JSON.stringify(metrics, null, 1));
	// OBSERVER EFFECT: .eval/ is written into the workspace before verify.sh runs, so a task that
	// asserts `git status --porcelain` is empty sees `?? .eval/` and fails a run that actually
	// passed. Exclude it locally (.git/info/exclude is untracked, so it cannot dirty the tree
	// itself). Without this the instrument fails the very behaviour it is measuring.
	const gitInfo = join(ws, ".git", "info");
	if (existsSync(join(ws, ".git"))) {
		mkdirSync(gitInfo, { recursive: true });
		const ex = join(gitInfo, "exclude");
		const prev = existsSync(ex) ? readFileSync(ex, "utf8") : "";
		if (!prev.includes(".eval/")) writeFileSync(ex, `${prev}\n.eval/\n`);
	}

	const v = await sh(`bash ${JSON.stringify(join(task.dir, "verify.sh"))}`, ws, env, 120_000);

	// A run that could not happen must never score as a run that happened and failed. An auth
	// failure, a missing key or a crashed process produces 0 turns and 0 tokens; recording that
	// as pass:false poisons the baseline, and the control arm cannot catch it because the control
	// breaks the same way.
	// Deliver followup prompts into the same session; fold their events into the same metrics.
	let followupOut = "";
	for (const fu of followups) {
		const fcmd = `${opts.bin} --print --mode json --continue${modelArg} ${JSON.stringify(fu)}`;
		const fr = await sh(fcmd, ws, env, task.timeoutMs ?? 600_000);
		followupOut += fr.stdout;
		const fm = foldEvents(fr.stdout);
		metrics.turns += fm.turns;
		metrics.toolCalls += fm.toolCalls;
		metrics.tokensIn += fm.tokensIn;
		metrics.tokensOut += fm.tokensOut;
		metrics.asks += fm.asks;
		metrics.events += fm.events;
		metrics.stopReason = fm.stopReason ?? metrics.stopReason;
		// A followup IS an intervention: the operator had to say something beyond the first prompt.
		metrics.interventions = (metrics.interventions ?? 0) + 1;
		wallMs += 0;
	}
	run = { ...run, stdout: run.stdout + followupOut };

	const runErrors = collectRunErrors(run.stdout);
	// A run that recovered from a transient error and still did work is VALID. Only a run that
	// produced nothing, or whose process failed, or whose LAST turn ended in error, is invalid.
	const noProviderOutput = metrics.turns === 0 && metrics.toolCalls === 0 && metrics.tokensIn === 0 && metrics.tokensOut === 0;
	const endedInError = metrics.stopReason === "error";
	const invalid = run.code !== 0 || noProviderOutput || endedInError;

	const result = {
		id: task.id,
		phase: opts.phase,
		invalid,
		invalidReason: invalid ? (runErrors[runErrors.length - 1] ?? (run.code !== 0 ? `agent exit ${run.code}` : "no provider output: 0 turns, 0 tokens")) : null,
		recoveredErrors: runErrors.length,
		pass: v.code === 0 && !invalid,
		timedOut: run.killed,
		wallMs,
		...metrics,
		// Interventions beyond the single opening prompt. Headless means the true count is asks;
		// a run that stops needing them is the thing we are trying to buy.
		humanInterventions: metrics.asks + (metrics.interventions ?? 0),
		verifyStdout: v.stdout.trim().slice(0, 600),
		verifyStderr: v.stderr.trim().slice(0, 600),
		agentExit: run.code,
	};
	if (!opts.keep) {
		rmSync(ws, { recursive: true, force: true });
		if (!stateDir) rmSync(agentDir, { recursive: true, force: true });
	} else {
		result.workspace = ws;
	}
	return result;
}

async function main() {
	const opts = parseArgs(process.argv.slice(2));
	if (opts.help) {
		console.log(`usage: node evals/run.mjs [--phase cold|study|warm] [--state DIR] [--out FILE]
                          [--only ID|TAG] [--repeat N] [--model ID] [--bin prime-agent] [--keep]`);
		return;
	}
	const tasks = loadTasks(opts.only);
	if (!tasks.length) { console.error("no tasks found in evals/tasks/"); process.exit(1); }

	// cold => isolated state per task. study/warm => one shared, persistent state dir.
	let stateDir = null;
	if (opts.phase !== "cold") {
		if (!opts.state) { console.error("--state DIR is required for phase study|warm"); process.exit(1); }
		stateDir = resolve(opts.state);
		mkdirSync(stateDir, { recursive: true });
	}

	console.log(`phase=${opts.phase} tasks=${tasks.length} repeat=${opts.repeat} state=${stateDir ?? "(isolated per task)"}`);
	const results = [];
	for (let r = 0; r < opts.repeat; r++) {
		for (const task of tasks) {
			process.stdout.write(`  [${r + 1}/${opts.repeat}] ${task.id} ... `);
			const res = await runOne(task, opts, stateDir);
			res.rep = r;
			results.push(res);
			if (opts.delay > 0) await new Promise((r) => setTimeout(r, opts.delay));
			const mark = res.invalid ? "INVALID" : res.pass ? "PASS" : "FAIL";
			console.log(`${mark}  turns=${res.turns ?? "?"} tok=${(res.tokensIn ?? 0) + (res.tokensOut ?? 0)} asks=${res.humanInterventions ?? "?"} ${Math.round((res.wallMs ?? 0) / 1000)}s${res.invalidReason ? `  (${res.invalidReason})` : res.error ? `  (${res.error})` : ""}`);
		}
	}
	const payload = { phase: opts.phase, startedAt: new Date().toISOString(), tasks: tasks.length, results };
	if (opts.out) {
		mkdirSync(dirname(resolve(opts.out)), { recursive: true });
		writeFileSync(resolve(opts.out), JSON.stringify(payload, null, 1));
		console.log(`\nwrote ${opts.out}`);
	}
	const passed = results.filter((x) => x.pass).length;
	const invalid = results.filter((x) => x.invalid).length;
	console.log(`\n${passed}/${results.length} passed${invalid ? `, ${invalid} INVALID` : ""}`);
	if (invalid) {
		// Refuse to hand back a scoreable baseline built on runs that never happened.
		console.error(`\n${invalid} run(s) did not execute. This is not a measurement; treat the file as void.`);
		console.error(`first reason: ${results.find((x) => x.invalid)?.invalidReason}`);
		process.exit(2);
	}
}

main().catch((e) => { console.error(e); process.exit(1); });
