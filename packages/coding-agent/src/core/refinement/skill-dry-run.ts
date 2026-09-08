import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
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
