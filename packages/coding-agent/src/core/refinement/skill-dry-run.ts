import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { readFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { getKernelVenvDir } from "../kernel/bootstrap.js";
import { countValidRefinementEdits, type RefinementEdit, type RefinementProposal } from "./refinement.js";

/**
 * Deterministic skill dry-run: the RAVO fast screen for skill edits.
 *
 * For every create/update skill edit the proposal carries, spawn the kernel
 * python in isolated mode (`-I -c`) with a tiny program that imports
 * `reference.import` and resolves `reference.callable` via getattr. The
 * callable is never invoked. Import errors, missing attributes, malformed
 * references, and timeouts all count as a failed screen for that edit.
 */

export interface SkillDryRunResult {
	editIndex: number;
	ok: boolean;
	detail: string;
	durationMs: number;
}

export interface SkillDryRunOptions {
	pythonPath: string;
	/** Per-edit wall-clock bound; the interpreter is killed when exceeded. Default 5000. */
	timeoutMs?: number;
	cwd?: string;
	signal?: AbortSignal;
	/** Extra environment for the probe; merged over process.env. */
	env?: NodeJS.ProcessEnv;
	/** Extra directories prepended to sys.path in the probe (e.g. a skill package root). */
	sysPath?: readonly string[];
}

export const DEFAULT_SKILL_DRY_RUN_TIMEOUT_MS = 5000;
export const SKILL_DRY_RUN_CONCURRENCY = 4;
const MAX_DETAIL_CHARS = 2000;
const OK_MARKER = "OK";

// The probe never calls the callable. `-I` drops cwd/user-site and ignores
// PYTHON* env vars, so PYTHONPATH is re-applied explicitly from os.environ to
// match what the real kernel process sees; extra sys.path entries arrive via argv.
const PROBE_PROGRAM = [
	"import importlib, os, sys",
	"mod, attr, extra = sys.argv[1], sys.argv[2], sys.argv[3]",
	"paths = [p for p in extra.split(os.pathsep) if p] + [p for p in os.environ.get('PYTHONPATH', '').split(os.pathsep) if p]",
	"sys.path[0:0] = paths",
	"try:",
	"    obj = importlib.import_module(mod)",
	"    for part in attr.split('.'):",
	"        obj = getattr(obj, part)",
	"    if not callable(obj):",
	"        raise TypeError(f'{mod}.{attr} is not callable ({type(obj).__name__})')",
	"except BaseException as exc:",
	`    sys.stdout.write(f'{type(exc).__name__}: {exc}')`,
	"    sys.stdout.flush()",
	"    raise SystemExit(1)",
	`sys.stdout.write('${OK_MARKER}')`,
].join("\n");

interface ParsedReference {
	moduleName: string;
	callableName: string;
}

interface ReferenceCheck {
	parsed?: ParsedReference;
	error?: string;
}

function nonEmptyString(value: unknown): value is string {
	return typeof value === "string" && value.trim().length > 0;
}

function isPlainObject(value: unknown): value is Record<string, unknown> {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}

const MODULE_PATTERN = /^[A-Za-z_][A-Za-z0-9_]*(?:\.[A-Za-z_][A-Za-z0-9_]*)*$/;

/**
 * Derive the callable name from a `call_pattern` such as `await my_skill.run(...)`
 * when `callable` is absent. The leading module segment is dropped when it
 * matches the import (or its last component).
 */
function callableFromCallPattern(pattern: string, moduleName: string): string | undefined {
	const match = /([A-Za-z_][A-Za-z0-9_]*(?:\.[A-Za-z_][A-Za-z0-9_]*)*)\s*\(/.exec(pattern.replace(/^\s*await\s+/, ""));
	if (!match) return undefined;
	const parts = match[1].split(".");
	const moduleLast = moduleName.split(".").at(-1);
	if (parts.length > 1 && (parts[0] === moduleName || parts[0] === moduleLast)) {
		parts.shift();
	}
	if (parts.length > 1 && match[1].startsWith(`${moduleName}.`)) {
		return match[1].slice(moduleName.length + 1);
	}
	return parts.join(".");
}

function checkReference(edit: RefinementEdit): ReferenceCheck {
	if (!isPlainObject(edit.arguments)) {
		return { error: "skill edit arguments must be a JSON object" };
	}
	const reference = edit.reference;
	if (!isPlainObject(reference)) {
		return { error: "skill edit requires a python reference object" };
	}
	if (reference.type !== "python") {
		return { error: `skill reference.type must be "python" (got ${JSON.stringify(reference.type ?? null)})` };
	}
	const rawImport = nonEmptyString(reference.import)
		? reference.import
		: nonEmptyString(reference.python_import)
			? reference.python_import
			: undefined;
	if (!rawImport) {
		return { error: "skill reference requires a non-empty import" };
	}
	const moduleName = rawImport.trim();
	if (!MODULE_PATTERN.test(moduleName)) {
		return { error: `skill reference import is not a dotted module name: ${JSON.stringify(moduleName)}` };
	}
	const callableName = nonEmptyString(reference.callable)
		? reference.callable.trim()
		: nonEmptyString(reference.call_pattern)
			? callableFromCallPattern(reference.call_pattern, moduleName)
			: undefined;
	if (!callableName) {
		return { error: "skill reference requires a callable (or a call_pattern naming one)" };
	}
	if (!MODULE_PATTERN.test(callableName)) {
		return { error: `skill reference callable is not a dotted identifier: ${JSON.stringify(callableName)}` };
	}
	return { parsed: { moduleName, callableName } };
}

function isSkillWriteEdit(edit: RefinementEdit): boolean {
	return edit.kind === "skill" && (edit.action === "create" || edit.action === "update");
}

function trimDetail(text: string): string {
	const trimmed = text.trim();
	return trimmed.length > MAX_DETAIL_CHARS ? `${trimmed.slice(0, MAX_DETAIL_CHARS)}…` : trimmed;
}

function probeOne(
	parsed: ParsedReference,
	options: SkillDryRunOptions,
	timeoutMs: number,
): Promise<{ ok: boolean; detail: string }> {
	return new Promise((resolve) => {
		const started = Date.now();
		let settled = false;
		let stdout = "";
		let stderr = "";
		const finish = (ok: boolean, detail: string) => {
			if (settled) return;
			settled = true;
			clearTimeout(timer);
			options.signal?.removeEventListener("abort", onAbort);
			resolve({ ok, detail });
		};
		const label = `${parsed.moduleName}.${parsed.callableName}`;
		let child: ReturnType<typeof spawn>;
		try {
			child = spawn(
				options.pythonPath,
				[
					"-I",
					"-c",
					PROBE_PROGRAM,
					parsed.moduleName,
					parsed.callableName,
					(options.sysPath ?? []).join(path.delimiter),
				],
				{
					cwd: options.cwd,
					env: { ...process.env, ...options.env },
					stdio: ["ignore", "pipe", "pipe"],
				},
			);
		} catch (error) {
			finish(
				false,
				`spawn failed for ${options.pythonPath}: ${error instanceof Error ? error.message : String(error)}`,
			);
			return;
		}
		const killChild = () => {
			if (child.exitCode === null && child.signalCode === null) {
				child.kill("SIGKILL");
			}
		};
		const timer = setTimeout(() => {
			killChild();
			finish(false, `timeout after ${timeoutMs}ms importing ${label}`);
		}, timeoutMs);
		const onAbort = () => {
			killChild();
			finish(false, `aborted while importing ${label}`);
		};
		if (options.signal?.aborted) {
			onAbort();
			return;
		}
		options.signal?.addEventListener("abort", onAbort, { once: true });
		child.stdout?.on("data", (chunk: Buffer) => {
			if (stdout.length < MAX_DETAIL_CHARS * 2) stdout += chunk.toString();
		});
		child.stderr?.on("data", (chunk: Buffer) => {
			if (stderr.length < MAX_DETAIL_CHARS * 2) stderr += chunk.toString();
		});
		child.on("error", (error) => {
			finish(false, `spawn failed for ${options.pythonPath}: ${error.message}`);
		});
		child.on("close", (code, signal) => {
			const elapsed = Date.now() - started;
			if (code === 0 && stdout.trim() === OK_MARKER) {
				finish(true, `imported ${label} in ${elapsed}ms`);
				return;
			}
			const reported = trimDetail(stdout) || trimDetail(stderr);
			const exit = signal ? `signal ${signal}` : `exit ${code ?? "unknown"}`;
			finish(false, reported ? `${reported} (${exit})` : `${label} probe failed with ${exit}`);
		});
	});
}

/**
 * Dry-run every create/update skill edit of the proposal. Results are returned
 * in edit order; edits of other kinds (and skill deletes) are not included.
 * Malformed references fail without spawning an interpreter. At most
 * SKILL_DRY_RUN_CONCURRENCY probes run at once.
 */
export async function dryRunSkillEdits(
	proposal: RefinementProposal,
	options: SkillDryRunOptions,
): Promise<SkillDryRunResult[]> {
	const timeoutMs = options.timeoutMs ?? DEFAULT_SKILL_DRY_RUN_TIMEOUT_MS;
	const results: SkillDryRunResult[] = [];
	const pending: Array<{ editIndex: number; parsed: ParsedReference }> = [];
	for (const [editIndex, edit] of proposal.edits.entries()) {
		if (!isSkillWriteEdit(edit)) continue;
		const check = checkReference(edit);
		if (check.parsed) {
			pending.push({ editIndex, parsed: check.parsed });
		} else {
			results.push({ editIndex, ok: false, detail: check.error ?? "malformed skill reference", durationMs: 0 });
		}
	}

	let cursor = 0;
	const runWorker = async () => {
		while (cursor < pending.length) {
			const job = pending[cursor];
			cursor += 1;
			const started = Date.now();
			const outcome = await probeOne(job.parsed, options, timeoutMs);
			results.push({
				editIndex: job.editIndex,
				ok: outcome.ok,
				detail: outcome.detail,
				durationMs: Date.now() - started,
			});
		}
	};
	await Promise.all(Array.from({ length: Math.min(SKILL_DRY_RUN_CONCURRENCY, pending.length) }, () => runWorker()));

	results.sort((a, b) => a.editIndex - b.editIndex);
	return results;
}

/**
 * Structural valid count minus the skill edits that failed dry-run. A failed
 * edit is only subtracted when it counted as structurally valid, so an edit
 * cannot be removed twice. Never negative.
 */
export function screenValidEdits(
	proposal: RefinementProposal,
	structuralValid: number,
	dryRun: readonly SkillDryRunResult[],
): number {
	let failed = 0;
	const seen = new Set<number>();
	for (const result of dryRun) {
		if (result.ok || seen.has(result.editIndex)) continue;
		seen.add(result.editIndex);
		const edit = proposal.edits[result.editIndex];
		if (!edit || !isSkillWriteEdit(edit)) continue;
		if (countValidRefinementEdits({ ...proposal, edits: [edit] }) === 1) {
			failed += 1;
		}
	}
	return Math.max(0, structuralValid - failed);
}

function expandHome(filePath: string): string {
	if (filePath === "~") return os.homedir();
	if (filePath.startsWith("~/")) return path.join(os.homedir(), filePath.slice(2));
	return filePath;
}

/**
 * Locate Prime's kernel python without bootstrapping anything: the
 * PRIME_AGENT_KERNEL_PYTHON override first, then the kernel venv interpreter
 * (getKernelVenvDir, then the XDG fallback used by bootstrap). Returns undefined
 * when no interpreter exists on disk. Callers must treat undefined as "screen
 * skipped" (ok=true, detail "dry-run skipped: no kernel python"), never as a
 * failed screen, so sessions without a kernel keep today's behavior.
 */
export function resolveKernelPython(): string | undefined {
	const override = process.env.PRIME_AGENT_KERNEL_PYTHON;
	if (override) {
		const resolved = path.resolve(expandHome(override));
		return existsSync(resolved) ? resolved : undefined;
	}
	const dataHome = process.env.XDG_DATA_HOME
		? path.resolve(expandHome(process.env.XDG_DATA_HOME))
		: path.join(os.homedir(), ".local", "share");
	const candidates = [
		path.join(getKernelVenvDir(), "bin", "python"),
		path.join(dataHome, "prime", "agent", "kernel-venv", "bin", "python"),
	];
	return candidates.find((candidate) => existsSync(candidate));
}

/** Result used by the call site when no kernel python is available. */
export function skippedSkillDryRun(proposal: RefinementProposal): SkillDryRunResult[] {
	return proposal.edits.flatMap((edit, editIndex) =>
		isSkillWriteEdit(edit)
			? [{ editIndex, ok: true, detail: "dry-run skipped: no kernel python", durationMs: 0 }]
			: [],
	);
}

/**
 * The RAVO fast screen for one proposal: structural validity minus skill edits
 * that fail the import dry-run. Without a kernel python the dry-run is skipped
 * (never fail-closed), so the count equals countValidRefinementEdits. This is
 * the Rocq S13 `fastDry` instance: a false screen can only cause a rejection.
 */
export async function screenRefinementProposal(
	proposal: RefinementProposal,
	opts: { signal?: AbortSignal; cwd?: string; timeoutMs?: number } = {},
): Promise<{ validEdits: number; dryRun: SkillDryRunResult[] }> {
	const structural = countValidRefinementEdits(proposal);
	const pythonPath = resolveKernelPython();
	const dryRun = pythonPath ? await dryRunSkillEdits(proposal, { pythonPath, ...opts }) : skippedSkillDryRun(proposal);
	return { validEdits: screenValidEdits(proposal, structural, dryRun), dryRun };
}

// ---------------------------------------------------------------------------
// Refereed counter-examples: the RAVO referee opponent's executable evidence.
// ---------------------------------------------------------------------------

export type SkillCounterexampleOutcome =
	| "passed"
	| "failed"
	| "invalid"
	| "timeout"
	| "aborted"
	| "skipped"
	| "spawn_failed";

export interface SkillCounterexampleRun {
	outcome: SkillCounterexampleOutcome;
	/** `outcome === "passed"`: the interpreter exited 0, so the counter-example did not demonstrate a flaw. */
	passed: boolean;
	exitCode: number | null;
	stdout: string;
	stderr: string;
	detail: string;
	durationMs: number;
}

export interface SkillCounterexampleOptions {
	/** Interpreter; defaults to {@link resolveKernelPython}. Without one the run is `skipped`. */
	pythonPath?: string;
	/** Wall-clock bound; the interpreter is killed when exceeded. Default {@link DEFAULT_SKILL_COUNTEREXAMPLE_TIMEOUT_MS}. */
	timeoutMs?: number;
	cwd?: string;
	signal?: AbortSignal;
	env?: NodeJS.ProcessEnv;
	sysPath?: readonly string[];
}

export interface SkillModuleSource {
	/** Module file the kernel resolves `reference.import` to; absent when it is not a plain file. */
	origin?: string;
	/** Source text, clipped to `maxChars`. */
	source?: string;
	truncated: boolean;
	detail: string;
}

export interface SkillModuleSourceOptions extends SkillCounterexampleOptions {
	/** Default {@link DEFAULT_SKILL_SOURCE_MAX_CHARS}. */
	maxChars?: number;
}

/** A counter-example may call the skill, so it gets three import-probe budgets. */
export const DEFAULT_SKILL_COUNTEREXAMPLE_TIMEOUT_MS = 3 * DEFAULT_SKILL_DRY_RUN_TIMEOUT_MS;
export const DEFAULT_SKILL_SOURCE_MAX_CHARS = 16_000;
/** Exit status the harness uses when the test source itself does not compile. */
export const COUNTEREXAMPLE_INVALID_EXIT_CODE = 96;
const COUNTEREXAMPLE_INVALID_MARKER = "COUNTEREXAMPLE_INVALID:";
const MAX_CAPTURE_CHARS = MAX_DETAIL_CHARS * 4;

// Same sys.path preparation as PROBE_PROGRAM, then the test source arrives on
// stdin and runs as `__main__`. A test that does not compile is not evidence
// and exits with COUNTEREXAMPLE_INVALID_EXIT_CODE; any other nonzero exit
// (assertion, uncaught exception, sys.exit) is a failed counter-example.
const COUNTEREXAMPLE_PROGRAM = [
	"import os, sys",
	"mod, attr, extra = sys.argv[1], sys.argv[2], sys.argv[3]",
	"paths = [p for p in extra.split(os.pathsep) if p] + [p for p in os.environ.get('PYTHONPATH', '').split(os.pathsep) if p]",
	"sys.path[0:0] = paths",
	"source = sys.stdin.read()",
	"try:",
	"    code = compile(source, '<counterexample>', 'exec')",
	"except (SyntaxError, ValueError) as exc:",
	`    sys.stderr.write(f'${COUNTEREXAMPLE_INVALID_MARKER} {type(exc).__name__}: {exc}\\n')`,
	`    raise SystemExit(${COUNTEREXAMPLE_INVALID_EXIT_CODE})`,
	"namespace = {'__name__': '__main__', '__file__': '<counterexample>', 'SKILL_MODULE': mod, 'SKILL_CALLABLE': attr}",
	"exec(code, namespace)",
].join("\n");

// find_spec never executes the target module itself (parents of a dotted
// name are imported, exactly as the probe would).
const SOURCE_PROGRAM = [
	"import importlib.util, os, sys",
	"mod, extra = sys.argv[1], sys.argv[2]",
	"paths = [p for p in extra.split(os.pathsep) if p] + [p for p in os.environ.get('PYTHONPATH', '').split(os.pathsep) if p]",
	"sys.path[0:0] = paths",
	"try:",
	"    spec = importlib.util.find_spec(mod)",
	"except BaseException as exc:",
	"    sys.stdout.write(f'{type(exc).__name__}: {exc}')",
	"    raise SystemExit(1)",
	"origin = getattr(spec, 'origin', None) if spec is not None else None",
	"if not origin or not os.path.isfile(origin):",
	"    raise SystemExit(2)",
	"sys.stdout.write(origin)",
].join("\n");

interface PythonRun {
	exitCode: number | null;
	signal: NodeJS.Signals | null;
	stdout: string;
	stderr: string;
	terminal?: "timeout" | "aborted" | "spawn_failed";
	error?: string;
}

/** Spawn `pythonPath -I -c program argv...` with bounded output capture, a kill timer, and abort relay. */
function spawnPython(
	program: string,
	argv: readonly string[],
	stdin: string | undefined,
	options: SkillCounterexampleOptions & { pythonPath: string },
	timeoutMs: number,
): Promise<PythonRun> {
	return new Promise((resolve) => {
		let settled = false;
		let stdout = "";
		let stderr = "";
		const finish = (run: PythonRun) => {
			if (settled) return;
			settled = true;
			clearTimeout(timer);
			options.signal?.removeEventListener("abort", onAbort);
			resolve({ ...run, stdout: trimDetail(stdout), stderr: trimDetail(stderr) });
		};
		let child: ReturnType<typeof spawn>;
		try {
			child = spawn(options.pythonPath, ["-I", "-c", program, ...argv], {
				cwd: options.cwd,
				env: { ...process.env, ...options.env },
				stdio: [stdin === undefined ? "ignore" : "pipe", "pipe", "pipe"],
			});
		} catch (error) {
			finish({
				exitCode: null,
				signal: null,
				stdout,
				stderr,
				terminal: "spawn_failed",
				error: error instanceof Error ? error.message : String(error),
			});
			return;
		}
		const killChild = () => {
			if (child.exitCode === null && child.signalCode === null) child.kill("SIGKILL");
		};
		const timer = setTimeout(() => {
			killChild();
			finish({ exitCode: null, signal: null, stdout, stderr, terminal: "timeout" });
		}, timeoutMs);
		const onAbort = () => {
			killChild();
			finish({ exitCode: null, signal: null, stdout, stderr, terminal: "aborted" });
		};
		if (options.signal?.aborted) {
			onAbort();
			return;
		}
		options.signal?.addEventListener("abort", onAbort, { once: true });
		child.stdout?.on("data", (chunk: Buffer) => {
			if (stdout.length < MAX_CAPTURE_CHARS) stdout += chunk.toString();
		});
		child.stderr?.on("data", (chunk: Buffer) => {
			if (stderr.length < MAX_CAPTURE_CHARS) stderr += chunk.toString();
		});
		child.on("error", (error) => {
			finish({ exitCode: null, signal: null, stdout, stderr, terminal: "spawn_failed", error: error.message });
		});
		child.on("close", (code, signal) => {
			finish({ exitCode: code, signal, stdout, stderr });
		});
		if (stdin !== undefined && child.stdin) {
			child.stdin.on("error", () => {
				// The interpreter may exit before consuming stdin; the close handler reports that.
			});
			child.stdin.end(stdin);
		}
	});
}

function counterexampleResult(
	outcome: SkillCounterexampleOutcome,
	detail: string,
	started: number,
	run: Partial<PythonRun> = {},
): SkillCounterexampleRun {
	return {
		outcome,
		passed: outcome === "passed",
		exitCode: run.exitCode ?? null,
		stdout: run.stdout ?? "",
		stderr: run.stderr ?? "",
		detail,
		durationMs: Date.now() - started,
	};
}

/**
 * Run a referee's counter-example against one create/update skill edit. The
 * test source runs in the kernel python exactly as the dry-run probe imports
 * the skill (`-I`, PYTHONPATH re-applied, extra sys.path entries), so
 * `import <reference.import>` inside the test resolves the same module.
 *
 * Mechanistic verdict material: `outcome === "failed"` iff the interpreter
 * exited nonzero for a reason other than the test not compiling (assertion,
 * uncaught exception, `sys.exit(n)`). A test that does not compile, times
 * out, is aborted, or cannot be spawned is NOT a demonstrated flaw; callers
 * must not treat those outcomes as evidence. Without a kernel python the run
 * is `skipped` (never fail-closed), matching {@link screenRefinementProposal}.
 */
export async function runSkillCounterexample(
	edit: RefinementEdit,
	testSource: string,
	options: SkillCounterexampleOptions = {},
): Promise<SkillCounterexampleRun> {
	const started = Date.now();
	if (!isSkillWriteEdit(edit)) {
		return counterexampleResult("invalid", "counter-example target is not a create/update skill edit", started);
	}
	const check = checkReference(edit);
	if (!check.parsed) {
		return counterexampleResult("invalid", check.error ?? "malformed skill reference", started);
	}
	if (!testSource.trim()) {
		return counterexampleResult("invalid", "counter-example test source is empty", started);
	}
	const pythonPath = options.pythonPath ?? resolveKernelPython();
	if (!pythonPath) {
		return counterexampleResult("skipped", "counter-example skipped: no kernel python", started);
	}
	const timeoutMs = options.timeoutMs ?? DEFAULT_SKILL_COUNTEREXAMPLE_TIMEOUT_MS;
	const label = `${check.parsed.moduleName}.${check.parsed.callableName}`;
	const run = await spawnPython(
		COUNTEREXAMPLE_PROGRAM,
		[check.parsed.moduleName, check.parsed.callableName, (options.sysPath ?? []).join(path.delimiter)],
		testSource,
		{ ...options, pythonPath },
		timeoutMs,
	);
	if (run.terminal === "spawn_failed") {
		return counterexampleResult(
			"spawn_failed",
			`spawn failed for ${pythonPath}: ${run.error ?? "unknown"}`,
			started,
			run,
		);
	}
	if (run.terminal === "timeout") {
		return counterexampleResult(
			"timeout",
			`timeout after ${timeoutMs}ms running counter-example for ${label}`,
			started,
			run,
		);
	}
	if (run.terminal === "aborted") {
		return counterexampleResult("aborted", `aborted while running counter-example for ${label}`, started, run);
	}
	if (run.exitCode === 0) {
		return counterexampleResult("passed", `counter-example for ${label} passed (exit 0)`, started, run);
	}
	if (run.exitCode === COUNTEREXAMPLE_INVALID_EXIT_CODE && run.stderr.includes(COUNTEREXAMPLE_INVALID_MARKER)) {
		const reason = run.stderr
			.slice(run.stderr.indexOf(COUNTEREXAMPLE_INVALID_MARKER) + COUNTEREXAMPLE_INVALID_MARKER.length)
			.trim();
		return counterexampleResult(
			"invalid",
			`counter-example does not compile: ${reason.split("\n")[0]}`,
			started,
			run,
		);
	}
	const exit = run.signal ? `signal ${run.signal}` : `exit ${run.exitCode ?? "unknown"}`;
	const lastLine = run.stderr.trim().split("\n").at(-1) || run.stdout.trim().split("\n").at(-1) || "";
	return counterexampleResult(
		"failed",
		lastLine
			? `counter-example for ${label} failed (${exit}): ${lastLine}`
			: `counter-example for ${label} failed (${exit})`,
		started,
		run,
	);
}

/**
 * Locate and read the module a skill edit's `reference.import` resolves to in
 * the kernel python, so a referee can read the real implementation rather
 * than guess from prose. Best effort: any resolution failure yields no source
 * with a detail, never a thrown error. Text is clipped to `maxChars`.
 */
export async function readSkillModuleSource(
	edit: RefinementEdit,
	options: SkillModuleSourceOptions = {},
): Promise<SkillModuleSource> {
	const check = checkReference(edit);
	if (!check.parsed) return { truncated: false, detail: check.error ?? "malformed skill reference" };
	const pythonPath = options.pythonPath ?? resolveKernelPython();
	if (!pythonPath) return { truncated: false, detail: "no kernel python" };
	const timeoutMs = options.timeoutMs ?? DEFAULT_SKILL_DRY_RUN_TIMEOUT_MS;
	const run = await spawnPython(
		SOURCE_PROGRAM,
		[check.parsed.moduleName, (options.sysPath ?? []).join(path.delimiter)],
		undefined,
		{ ...options, pythonPath },
		timeoutMs,
	);
	if (run.terminal) return { truncated: false, detail: `${run.terminal}${run.error ? `: ${run.error}` : ""}` };
	if (run.exitCode !== 0 || !run.stdout.trim()) {
		return {
			truncated: false,
			detail:
				run.exitCode === 2
					? `${check.parsed.moduleName} has no file origin`
					: run.stdout || run.stderr || `exit ${run.exitCode ?? "unknown"}`,
		};
	}
	const origin = run.stdout.trim();
	try {
		const text = await readFile(origin, "utf8");
		const maxChars = options.maxChars ?? DEFAULT_SKILL_SOURCE_MAX_CHARS;
		const truncated = text.length > maxChars;
		return { origin, source: truncated ? text.slice(0, maxChars) : text, truncated, detail: `read ${origin}` };
	} catch (error) {
		return {
			origin,
			truncated: false,
			detail: `cannot read ${origin}: ${error instanceof Error ? error.message : String(error)}`,
		};
	}
}
