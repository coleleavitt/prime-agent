#!/usr/bin/env node
/**
 * Black-box harness benchmark runner.
 *
 * Drives `prime-agent --print --mode json` against each task in its own workspace with an isolated
 * agent-state dir, then runs the task's `verify.sh` as the referee.
 *
 * Hermeticity: setup.sh, the agent, followups and verify.sh all run under a controlled
 * GIT_CONFIG_GLOBAL (no gpg signing, no global hooks, no global excludesfile, push.autoSetupRemote
 * off) and PYTHONDONTWRITEBYTECODE=1, so a score never depends on the operator's ~/.gitconfig,
 * ~/.gitignore_global, a GPG agent, or stray __pycache__. The no-push tasks get a per-run bare
 * remote the runner owns (EVAL_REMOTE), never a shared /tmp path.
 *
 * Train/test split: a task tagged "study" is a training task and only runs in --phase study; every
 * other task is a held-out test task and only runs in --phase cold|warm. Study must never run the
 * test tasks.
 *
 * This never modifies your live ~/.prime/agent: auth.json and settings.json are COPIED into a temp
 * agent dir, and everything else is created fresh.
 */
import { spawn } from "node:child_process";
import { mkdtempSync, mkdirSync, readdirSync, readFileSync, writeFileSync, existsSync, rmSync, cpSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { createHash } from "node:crypto";

const HERE = dirname(fileURLToPath(import.meta.url));
const TASKS = join(HERE, "tasks");

/**
 * A git config the runner owns, so scoring never depends on the operator's machine:
 *  - no commit signing (a locked GPG agent must not turn a correct run into a setup crash)
 *  - no global hooks path (lefthook etc. must not run inside an eval commit)
 *  - an empty excludesfile (so ~/.gitignore_global cannot silently hide a stray file a task checks)
 *  - push.autoSetupRemote off, so a bare `git push` with no upstream FAILS rather than silently
 *    succeeding — an unrequested push attempt then leaves evidence instead of scoring as compliant
 */
export const CONTROLLED_GITCONFIG = [
	"[user]",
	"\tname = eval",
	"\temail = eval@local",
	"[commit]",
	"\tgpgsign = false",
	"[tag]",
	"\tgpgsign = false",
	"[core]",
	"\thooksPath = ",
	"\texcludesfile = ",
	"[push]",
	"\tautoSetupRemote = false",
	"\tdefault = simple",
	"[init]",
	"\tdefaultBranch = main",
	"[protocol.file]",
	"\tallow = always",
	"",
].join("\n");

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

const isStudyTask = (t) => (t.tags ?? []).includes("study");

/**
 * Load tasks and select the set for this phase. study => training tasks only; cold|warm => the
 * held-out test tasks only. `--only ID|TAG` narrows within the phase's set. This is the train/test
 * split: phase study can never run a test task, and a warm/cold run can never run a study task.
 */
export function selectTasks(all, opts) {
	const wantStudy = opts.phase === "study";
	let pool = all.filter((t) => isStudyTask(t) === wantStudy);
	if (opts.only) pool = pool.filter((t) => t.id === opts.only || (t.tags ?? []).includes(opts.only));
	return pool.sort((a, b) => a.id.localeCompare(b.id));
}

function loadAllTasks() {
	if (!existsSync(TASKS)) return [];
	return readdirSync(TASKS, { withFileTypes: true })
		.filter((d) => d.isDirectory())
		.map((d) => {
			const dir = join(TASKS, d.name);
			const spec = JSON.parse(readFileSync(join(dir, "task.json"), "utf8"));
			return { ...spec, id: spec.id ?? d.name, dir };
		});
}

/** A followup is a string (a correction, delivered only while verify still fails) or
 *  { prompt, when: "fail" | "always" }. "always" is delivered even once the task already passes. */
export function normalizeFollowups(followups) {
	if (!Array.isArray(followups)) return [];
	return followups.map((f) => (typeof f === "string" ? { prompt: f, when: "fail" } : { prompt: f.prompt, when: f.when === "always" ? "always" : "fail" }));
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

const emptyMetrics = () => ({ turns: 0, toolCalls: 0, tools: {}, tokensIn: 0, tokensOut: 0, cacheRead: 0, cacheWrite: 0, errors: 0, events: 0, stopReason: null });

/**
 * Fold the --mode json event stream into the metrics we score on.
 *
 * Token accounting reads assistant `message_end` events ONLY. Both `message_end` and `turn_end`
 * carry the same final assistant message, and `message_start`/`message_update` carry partial copies,
 * so summing usage over every event (as the old runner did) counted each message up to four times.
 * Usage is reported per component — input, output, cacheRead, cacheWrite — because the real prompt
 * (injected memories, recall, skills) lives in the cache fields the old metric threw away.
 */
export function foldEvents(stdout) {
	const m = emptyMetrics();
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
		if (t === "tool_execution_end" && e.isError === true) m.errors++;
		if (t === "error") m.errors++;
		// Count usage once per assistant message, from message_end only.
		if (t === "message_end" && e.message?.role === "assistant") {
			const u = e.message.usage;
			if (u) {
				m.tokensIn += u.input ?? 0;
				m.tokensOut += u.output ?? 0;
				m.cacheRead += u.cacheRead ?? 0;
				m.cacheWrite += u.cacheWrite ?? 0;
			}
		}
		// The last assistant stopReason decides whether the run ended in error. stopReason lives on
		// the message, not the event.
		if ((t === "message_end" || t === "turn_end") && e.message?.role === "assistant" && e.message.stopReason) {
			m.stopReason = e.message.stopReason;
		}
	}
	return m;
}

/** Fold a followup's metrics into the running total. */
export function mergeMetrics(into, add) {
	into.turns += add.turns;
	into.toolCalls += add.toolCalls;
	into.tokensIn += add.tokensIn;
	into.tokensOut += add.tokensOut;
	into.cacheRead += add.cacheRead;
	into.cacheWrite += add.cacheWrite;
	into.errors += add.errors;
	into.events += add.events;
	for (const [k, v] of Object.entries(add.tools)) into.tools[k] = (into.tools[k] ?? 0) + v;
	if (add.stopReason) into.stopReason = add.stopReason;
	return into;
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

function sha16(s) {
	return createHash("sha256").update(s).digest("hex").slice(0, 16);
}

function hashDir(dir) {
	if (!existsSync(dir)) return null;
	const parts = [];
	const walk = (d, rel) => {
		for (const ent of readdirSync(d, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
			const p = join(d, ent.name);
			const r = rel ? `${rel}/${ent.name}` : ent.name;
			if (ent.isDirectory()) walk(p, r);
			else parts.push(`${r}:${statSync(p).size}:${sha16(readFileSync(p))}`);
		}
	};
	try { walk(dir, ""); } catch { return null; }
	return sha16(parts.join("\n"));
}

/** Assemble the provenance record from already-collected raw values. Pure, so it is unit-testable. */
export function buildProvenance(raw) {
	const SECRET = /API_KEY|TOKEN|SECRET|PASSWORD|AUTH/i;
	const RECORD = /^(PRIME_AGENT_|PI_|OTEL_|GIT_CONFIG_|ANTHROPIC_|OPENAI_|CI$|PYTHONDONTWRITEBYTECODE$)/;
	const envAllowlist = {};
	for (const [k, v] of Object.entries(raw.env ?? {})) {
		if (!RECORD.test(k)) continue;
		envAllowlist[k] = SECRET.test(k) ? "<set>" : v;
	}
	return {
		bin: raw.bin,
		resolvedBin: raw.resolvedBin ?? null,
		version: raw.version ?? null,
		model: raw.model ?? null,
		node: raw.node ?? null,
		argv: raw.argv ?? [],
		flags: raw.flags ?? {},
		envAllowlist,
		settingsHash: raw.settingsHash ?? null,
		seed: raw.seed ?? { source: "none", hash: null },
		runnerGitSha: raw.runnerGitSha ?? null,
		runnerDirty: raw.runnerDirty ?? null,
		gitConfigHash: sha16(CONTROLLED_GITCONFIG),
		startedAt: raw.startedAt ?? null,
	};
}

async function gatherProvenance(opts, startedAt) {
	const q = async (cmd) => (await sh(cmd, HERE, process.env, 20_000)).stdout.trim();
	const resolvedBin = (await q(`command -v ${JSON.stringify(opts.bin)} || true`)) || null;
	const version = (await q(`${opts.bin} --version 2>/dev/null || true`)) || null;
	const runnerGitSha = (await q("git rev-parse HEAD 2>/dev/null || true")) || null;
	// q() runs in HERE (the evals dir), so "." scopes the dirty check to the runner + tasks.
	const runnerDirty = (await q("git status --porcelain -- . 2>/dev/null | head -c1")) !== "";
	const settingsSrc = join(process.env.HOME ?? "", ".prime", "agent", "settings.json");
	const settingsHash = existsSync(settingsSrc) ? sha16(readFileSync(settingsSrc)) : null;
	let seed = { source: "none", hash: null };
	if (opts.seedHarness) seed = { source: "seed-harness", hash: hashDir(join(process.env.HOME ?? "", ".prime", "agent", "harness")) };
	else if (opts.seedFrom) seed = { source: `seed-from:${opts.seedFrom}`, hash: hashDir(resolve(opts.seedFrom)) };
	return buildProvenance({
		bin: opts.bin,
		resolvedBin,
		version,
		model: opts.model,
		node: process.version,
		argv: process.argv.slice(2),
		flags: { phase: opts.phase, only: opts.only, repeat: opts.repeat, state: opts.state, seedHarness: !!opts.seedHarness, seedFrom: opts.seedFrom ?? null, delay: opts.delay, keep: opts.keep },
		env: process.env,
		settingsHash,
		seed,
		runnerGitSha,
		runnerDirty,
		startedAt,
	});
}

async function runOne(task, opts, stateDir, gitConfigPath) {
	const ws = mkdtempSync(join(tmpdir(), `eval-${task.id}-`));
	const agentDir = stateDir ?? mkdtempSync(join(tmpdir(), `eval-state-${task.id}-`));
	mkdirSync(agentDir, { recursive: true });
	// Per-run bare remote the runner owns, so one run's push can never contaminate the next and
	// concurrent runners never share it. Tasks reach it through $EVAL_REMOTE.
	const remoteParent = mkdtempSync(join(tmpdir(), `eval-remote-${task.id}-`));
	const remotePath = join(remoteParent, "remote.git");

	// Credentials live in the agent-state dir, so a fully isolated dir has no auth at all.
	// Copy ONLY auth.json + settings.json forward: the run stays hermetic for memories, skills,
	// prompt notes and the failure ledger — the things being measured — while still being able
	// to reach a provider. Without this every task fails with 0 turns and scores as a task failure.
	for (const f of ["auth.json", "settings.json"]) {
		const src = join(process.env.HOME ?? "", ".prime", "agent", f);
		const dst = join(agentDir, f);
		if (existsSync(src) && !existsSync(dst)) cpSync(src, dst);
	}
	if (opts.seedHarness) {
		const src = join(process.env.HOME ?? "", ".prime", "agent", "harness");
		const dst = join(agentDir, "harness");
		if (existsSync(src) && !existsSync(dst)) cpSync(src, dst, { recursive: true });
	}
	if (opts.seedFrom) {
		const src = resolve(opts.seedFrom);
		if (existsSync(src) && !existsSync(join(agentDir, "harness"))) cpSync(src, agentDir, { recursive: true });
	}

	const env = {
		...process.env,
		PRIME_AGENT_CODING_AGENT_DIR: agentDir,
		PI_CODING_AGENT_DIR: agentDir,
		GIT_CONFIG_GLOBAL: gitConfigPath,
		GIT_CONFIG_SYSTEM: "/dev/null",
		GIT_AUTHOR_NAME: "eval", GIT_AUTHOR_EMAIL: "eval@local",
		GIT_COMMITTER_NAME: "eval", GIT_COMMITTER_EMAIL: "eval@local",
		PYTHONDONTWRITEBYTECODE: "1",
		EVAL_REMOTE: remotePath,
		CI: "1",
	};

	const cleanup = () => {
		if (opts.keep) return;
		rmSync(ws, { recursive: true, force: true });
		rmSync(remoteParent, { recursive: true, force: true });
		if (!stateDir) rmSync(agentDir, { recursive: true, force: true });
	};

	// setup ---------------------------------------------------------------------------------------
	if (existsSync(join(task.dir, "setup.sh"))) {
		const r = await sh(`bash ${JSON.stringify(join(task.dir, "setup.sh"))}`, ws, env, 120_000);
		if (r.code !== 0) {
			cleanup();
			// A setup crash is not a task failure. It is a broken measurement; mark it invalid so it
			// never poisons a baseline.
			return { id: task.id, phase: opts.phase, invalid: true, invalidReason: `setup failed: ${r.stderr.slice(-200)}`, pass: false, ...emptyMetrics() };
		}
	}

	// Expose the run to the verifier so a task can assert on BEHAVIOUR (which tools ran, whether a
	// command repeated, whether a skill was used), not only the end state. .eval/ is written before
	// verify.sh runs, so a task that checks `git status --porcelain` would see `?? .eval/`; exclude
	// it locally (.git/info/exclude is untracked, so it cannot dirty the tree itself).
	const evalDir = join(ws, ".eval");
	mkdirSync(evalDir, { recursive: true });
	if (existsSync(join(ws, ".git"))) {
		const gitInfo = join(ws, ".git", "info");
		mkdirSync(gitInfo, { recursive: true });
		const ex = join(gitInfo, "exclude");
		const prev = existsSync(ex) ? readFileSync(ex, "utf8") : "";
		if (!prev.includes(".eval/")) writeFileSync(ex, `${prev}\n.eval/\n`);
	}

	const modelArg = opts.model ? ` --model ${JSON.stringify(opts.model)}` : "";
	const followups = normalizeFollowups(task.followups);

	// A 429 is transient and says nothing about the harness; a 401 is structural. Retry the whole
	// task on rate limits with exponential backoff, and only give up after the last attempt.
	const RETRYABLE = /rate limit|429|overloaded|529|ETIMEDOUT|ECONNRESET/i;
	const backoff = [30_000, 60_000, 120_000, 240_000];

	let allStdout = "";
	const metrics = emptyMetrics();
	let wallMs = 0;
	let anyKilled = false;
	let worstCode = 0;

	// main prompt (with retry) --------------------------------------------------------------------
	{
		const cmd = `${opts.bin} --print --mode json${modelArg} ${JSON.stringify(task.prompt)}`;
		let run;
		for (let attempt = 0; ; attempt++) {
			const t0 = Date.now();
			run = await sh(cmd, ws, env, task.timeoutMs ?? 600_000);
			wallMs += Date.now() - t0;
			const transient = collectRunErrors(run.stdout).some((mm) => RETRYABLE.test(mm));
			if (!transient || attempt >= backoff.length) break;
			const wait = backoff[attempt];
			process.stdout.write(`(rate limited, retry in ${Math.round(wait / 1000)}s) `);
			await new Promise((r) => setTimeout(r, wait));
		}
		allStdout += run.stdout;
		mergeMetrics(metrics, foldEvents(run.stdout));
		anyKilled ||= run.killed;
		if (run.code && !run.killed) worstCode = run.code;
	}

	// Write the event stream + metrics, then run verify.sh. Called after the main run and again
	// after every followup, so behavioural checks always see the latest events and study tasks are
	// scored AFTER the last correction, not before it.
	const runVerify = async () => {
		writeFileSync(join(evalDir, "events.jsonl"), allStdout);
		writeFileSync(join(evalDir, "metrics.json"), JSON.stringify(metrics, null, 1));
		return sh(`bash ${JSON.stringify(join(task.dir, "verify.sh"))}`, ws, env, 120_000);
	};

	let v = await runVerify();

	// followups -----------------------------------------------------------------------------------
	// A "fail" followup (a correction) is delivered only while verify still fails, and counts as a
	// human intervention. An "always" followup (e.g. "persist this preference") is delivered even
	// once the task passes, and does not count — it was not needed to reach the verified state.
	let interventions = 0;
	for (const fu of followups) {
		const passing = v.code === 0;
		if (fu.when === "fail" && passing) continue;
		const fcmd = `${opts.bin} --print --mode json --continue${modelArg} ${JSON.stringify(fu.prompt)}`;
		const t0 = Date.now();
		const fr = await sh(fcmd, ws, env, task.timeoutMs ?? 600_000);
		wallMs += Date.now() - t0;
		allStdout += fr.stdout;
		mergeMetrics(metrics, foldEvents(fr.stdout));
		anyKilled ||= fr.killed;
		if (fr.code && !fr.killed) worstCode = fr.code;
		if (!passing) interventions++;
		v = await runVerify();
	}

	// validity ------------------------------------------------------------------------------------
	const runErrors = collectRunErrors(allStdout);
	const noProviderOutput = metrics.turns === 0 && metrics.toolCalls === 0 && metrics.tokensIn === 0 && metrics.tokensOut === 0;
	const endedInError = metrics.stopReason === "error" || metrics.stopReason === "aborted";
	// A killed process is a timeout: a real FAIL, tracked separately, never silently voided.
	const invalid = noProviderOutput || endedInError || (worstCode !== 0 && !anyKilled);

	const result = {
		id: task.id,
		phase: opts.phase,
		invalid,
		invalidReason: invalid ? (runErrors[runErrors.length - 1] ?? (endedInError ? `final turn ${metrics.stopReason}` : worstCode !== 0 ? `agent exit ${worstCode}` : "no provider output: 0 turns, 0 tokens")) : null,
		recoveredErrors: runErrors.length,
		pass: v.code === 0 && !invalid && !anyKilled,
		timedOut: anyKilled,
		wallMs,
		...metrics,
		tokens: metrics.tokensIn + metrics.tokensOut,
		// Behavioural: corrections the operator had to deliver because the task was not yet done.
		humanInterventions: interventions,
		verifyStdout: v.stdout.trim().slice(0, 600),
		verifyStderr: v.stderr.trim().slice(0, 600),
		agentExit: worstCode,
	};
	if (opts.keep) { result.workspace = ws; result.remote = remotePath; }
	cleanup();
	return result;
}

async function main() {
	const opts = parseArgs(process.argv.slice(2));
	if (opts.help) {
		console.log(`usage: node evals/run.mjs [--phase cold|study|warm] [--state DIR] [--out FILE]
                          [--only ID|TAG] [--repeat N] [--model ID] [--bin prime-agent] [--keep]

phase cold|warm runs the held-out TEST tasks; phase study runs the training tasks (tagged "study").
Use --repeat 5 (or more) so the report can estimate variance.`);
		return;
	}
	const all = loadAllTasks();
	const tasks = selectTasks(all, opts);
	if (!tasks.length) { console.error(`no ${opts.phase === "study" ? "study" : "test"} tasks found in evals/tasks/`); process.exit(1); }

	// cold => isolated state per task. study/warm => one shared, persistent state dir.
	let stateDir = null;
	if (opts.phase !== "cold") {
		if (!opts.state) { console.error("--state DIR is required for phase study|warm"); process.exit(1); }
		stateDir = resolve(opts.state);
		mkdirSync(stateDir, { recursive: true });
	}

	// Controlled git config, owned by the runner for the whole invocation.
	const gitConfigDir = mkdtempSync(join(tmpdir(), "eval-gitconfig-"));
	const gitConfigPath = join(gitConfigDir, "gitconfig");
	writeFileSync(gitConfigPath, CONTROLLED_GITCONFIG);

	const startedAt = new Date().toISOString();
	const provenance = await gatherProvenance(opts, startedAt);
	console.log(`phase=${opts.phase} tasks=${tasks.length} repeat=${opts.repeat} state=${stateDir ?? "(isolated per task)"} version=${provenance.version ?? "?"}`);

	const results = [];
	for (let r = 0; r < opts.repeat; r++) {
		for (const task of tasks) {
			process.stdout.write(`  [${r + 1}/${opts.repeat}] ${task.id} ... `);
			const res = await runOne(task, opts, stateDir, gitConfigPath);
			res.rep = r;
			results.push(res);
			if (opts.delay > 0) await new Promise((rr) => setTimeout(rr, opts.delay));
			const mark = res.invalid ? "INVALID" : res.pass ? "PASS" : "FAIL";
			console.log(`${mark}  turns=${res.turns ?? "?"} tok=${res.tokens ?? 0} hi=${res.humanInterventions ?? "?"} ${Math.round((res.wallMs ?? 0) / 1000)}s${res.invalidReason ? `  (${res.invalidReason})` : res.timedOut ? "  (timed out)" : ""}`);
		}
	}
	if (!opts.keep) rmSync(gitConfigDir, { recursive: true, force: true });

	const payload = { phase: opts.phase, startedAt, finishedAt: new Date().toISOString(), tasks: tasks.length, provenance, results };
	if (opts.out) {
		mkdirSync(dirname(resolve(opts.out)), { recursive: true });
		writeFileSync(resolve(opts.out), JSON.stringify(payload, null, 1));
		console.log(`\nwrote ${opts.out}`);
	}
	const valid = results.filter((x) => !x.invalid);
	const passed = valid.filter((x) => x.pass).length;
	const invalid = results.length - valid.length;
	console.log(`\n${passed}/${valid.length} valid runs passed${invalid ? `, ${invalid} INVALID (excluded)` : ""}`);
	if (invalid) {
		console.error(`\n${invalid} run(s) did not execute. They are recorded with invalid:true and report.mjs drops them.`);
		console.error(`first reason: ${results.find((x) => x.invalid)?.invalidReason}`);
		process.exit(2);
	}
}

// Only run when invoked directly, so tests can import the pure helpers above.
const invokedDirectly = process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url);
if (invokedDirectly) main().catch((e) => { console.error(e); process.exit(1); });
