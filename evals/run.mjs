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
	const out = { phase: "cold", out: null, state: null, only: null, repeat: 1, keep: false, bin: "prime-agent", model: null };
	for (let i = 0; i < argv.length; i++) {
		const a = argv[i];
		if (a === "--phase") out.phase = argv[++i];
		else if (a === "--out") out.out = argv[++i];
		else if (a === "--state") out.state = argv[++i];
		else if (a === "--only") out.only = argv[++i];
		else if (a === "--repeat") out.repeat = Number(argv[++i]);
		else if (a === "--bin") out.bin = argv[++i];
		else if (a === "--model") out.model = argv[++i];
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

async function runOne(task, opts, stateDir) {
	const ws = mkdtempSync(join(tmpdir(), `eval-${task.id}-`));
	const agentDir = stateDir ?? mkdtempSync(join(tmpdir(), `eval-state-${task.id}-`));
	mkdirSync(agentDir, { recursive: true });

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
	const t0 = Date.now();
	const run = await sh(cmd, ws, env, task.timeoutMs ?? 600_000);
	const wallMs = Date.now() - t0;

	const metrics = foldEvents(run.stdout);

	// Expose the run to the verifier so a task can assert on BEHAVIOUR (which tools were called,
	// whether a command was repeated, whether a skill was used) and not only on the end state.
	const evalDir = join(ws, ".eval");
	mkdirSync(evalDir, { recursive: true });
	writeFileSync(join(evalDir, "events.jsonl"), run.stdout);
	writeFileSync(join(evalDir, "metrics.json"), JSON.stringify(metrics, null, 1));

	const v = await sh(`bash ${JSON.stringify(join(task.dir, "verify.sh"))}`, ws, env, 120_000);

	const result = {
		id: task.id,
		phase: opts.phase,
		pass: v.code === 0,
		timedOut: run.killed,
		wallMs,
		...metrics,
		// Interventions beyond the single opening prompt. Headless means the true count is asks;
		// a run that stops needing them is the thing we are trying to buy.
		humanInterventions: metrics.asks,
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
			console.log(`${res.pass ? "PASS" : "FAIL"}  turns=${res.turns ?? "?"} tok=${(res.tokensIn ?? 0) + (res.tokensOut ?? 0)} asks=${res.humanInterventions ?? "?"} ${Math.round((res.wallMs ?? 0) / 1000)}s${res.error ? `  (${res.error})` : ""}`);
		}
	}
	const payload = { phase: opts.phase, startedAt: new Date().toISOString(), tasks: tasks.length, results };
	if (opts.out) {
		mkdirSync(dirname(resolve(opts.out)), { recursive: true });
		writeFileSync(resolve(opts.out), JSON.stringify(payload, null, 1));
		console.log(`\nwrote ${opts.out}`);
	}
	const passed = results.filter((x) => x.pass).length;
	console.log(`\n${passed}/${results.length} passed`);
}

main().catch((e) => { console.error(e); process.exit(1); });
