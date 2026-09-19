import { type ChildProcess, spawn } from "node:child_process";
import { rmSync } from "node:fs";
import { mkdtemp, rm } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { withSpan } from "@earendil-works/pi-ai";
import { recordOrphanProcessState } from "../orphan-process-journal.js";
import { resolveKernelPython } from "../refinement/skill-dry-run.js";
import type { FailureObservation, FailureRecord, ReplayVerification } from "./failure-ledger.js";
import {
	inheritedPythonEnvironment,
	sanitizedPythonEnvironment,
	skillImportEnvironment,
} from "./python-environment.js";
import {
	type RefereeVerdict,
	type RefereeVerdictStatus,
	type ReplayCase,
	type ReplayOutcome,
	refereeVerdict,
	refereeVerdictIsEvidence,
	replayAppliesToSkillImports,
	replayCasesOf,
	replayProbeOf,
	verdictFromOutcome,
} from "./referee.js";

/**
 * Executes replay cases. This is the only place in the refinement loop that
 * produces a signal the proposal did not write.
 *
 * A replay case is derived from text a tool printed, so the interpreter is
 * given as little as possible to act on: isolated mode without bytecode
 * writes (`-I -B`), the program on stdin so `sys.argv` carries nothing, a fresh
 * temporary working directory removed afterwards, and the sanitized
 * environment (`sanitizedPythonEnvironment`: PATH, HOME, LANG and the explicit
 * sys.path roots, re-applied by the wrapper because `-I` implies `-E`).
 * Adjudication asks whether a skill's import resolves, so it replays in the
 * environment and kind of working directory the dry-run screen probes that
 * import in (`skillImportEnvironment`). The interpreter leads its own process
 * group, and the whole group is killed when the run ends, or when this process
 * exits or gets SIGINT, SIGTERM, SIGHUP or SIGQUIT, so a grandchild cannot
 * outlive a timeout or an interrupted worker. On that exit or signal the run's
 * temporary working directory is removed too, since its own cleanup may never
 * get to run. A live group is also recorded in the orphan process journal, so
 * supervisor recovery reaps it when the worker dies without running any
 * listener (SIGKILL, OOM); the directory is then left behind. An unrunnable
 * probe is `unrunnable`, never a pass (see `refereeOpponentPassed`).
 */

export const DEFAULT_REPLAY_TIMEOUT_MS = 10_000;
const MAX_DETAIL_CHARS = 1000;
const MAX_CAPTURE_CHARS = MAX_DETAIL_CHARS * 2;
const RAISED_MARKER = "RAISED ";
const CLEAN_MARKER = "CLEAN";
const CLEANUP_SIGNALS = ["SIGINT", "SIGTERM", "SIGHUP", "SIGQUIT"] as const;

function replayProgram(source: string): string {
	return [
		"import os, sys",
		"sys.path[0:0] = [p for p in os.environ.get('PYTHONPATH', '').split(os.pathsep) if p]",
		`source = ${JSON.stringify(source)}`,
		"try:",
		"    exec(compile(source, '<replay-case>', 'exec'), {'__name__': '__replay__'})",
		"except BaseException as exc:",
		`    sys.stdout.write('${RAISED_MARKER}' + type(exc).__name__ + '\\n' + str(exc)[:1000])`,
		"    sys.stdout.flush()",
		"    raise SystemExit(3)",
		`sys.stdout.write('${CLEAN_MARKER}')`,
		"",
	].join("\n");
}

export interface RefereeRunOptions {
	/** Interpreter override; defaults to the kernel python (`resolveKernelPython`). */
	pythonPath?: string;
	timeoutMs?: number;
	/** Extra sys.path roots, prepended before the case's own. */
	sysPath?: readonly string[];
	signal?: AbortSignal;
	now?: () => string;
}

/**
 * The environment a run gets (`python-environment.ts`): `sanitized` for a probe
 * derived from tool output, `skill-import` for adjudicating a skill's import,
 * `inherited` for a program the model wrote to run in the user's environment.
 */
export type ReplayEnvironment = "sanitized" | "skill-import" | "inherited";

export interface ReplayRunOptions extends RefereeRunOptions {
	/**
	 * Working directory for a program its caller authored (the toolforge gate).
	 * Omitted, the run gets a fresh temporary directory that is removed
	 * afterwards. The referee's own entry points never pass one.
	 */
	cwd?: string;
	/** Default `sanitized`. The referee's entry points choose their own. */
	environment?: ReplayEnvironment;
}

/** The run options a referee entry point forwards: never a working directory or an environment. */
function refereeRunOptions(options: RefereeRunOptions): RefereeRunOptions {
	return {
		...(options.pythonPath === undefined ? {} : { pythonPath: options.pythonPath }),
		...(options.timeoutMs === undefined ? {} : { timeoutMs: options.timeoutMs }),
		...(options.sysPath === undefined ? {} : { sysPath: options.sysPath }),
		...(options.signal === undefined ? {} : { signal: options.signal }),
	};
}

function trimDetail(text: string): string {
	const trimmed = text.trim();
	return trimmed.length > MAX_DETAIL_CHARS ? `${trimmed.slice(0, MAX_DETAIL_CHARS)}…` : trimmed;
}

function parseOutcome(stdout: string, stderr: string, code: number | null, signalCode: string | null): ReplayOutcome {
	if (signalCode) return { kind: "unrunnable", detail: `replay case killed by ${signalCode}` };
	if (code === 0 && stdout.trim() === CLEAN_MARKER) {
		return { kind: "clean", detail: "replay case completed without raising" };
	}
	if (code === 3 && stdout.startsWith(RAISED_MARKER)) {
		const body = stdout.slice(RAISED_MARKER.length);
		const newline = body.indexOf("\n");
		const exceptionClass = (newline === -1 ? body : body.slice(0, newline)).trim();
		const message = newline === -1 ? "" : body.slice(newline + 1).trim();
		if (exceptionClass) {
			return {
				kind: "raised",
				exceptionClass,
				detail: trimDetail(message ? `${exceptionClass}: ${message}` : exceptionClass),
			};
		}
	}
	const reported = trimDetail(stderr) || trimDetail(stdout);
	return { kind: "unrunnable", detail: reported || `replay case exited ${code ?? "unknown"} without a verdict` };
}

function replayEnvironment(environment: ReplayEnvironment, roots: readonly string[]): NodeJS.ProcessEnv {
	switch (environment) {
		case "sanitized":
			return sanitizedPythonEnvironment(roots);
		case "skill-import":
			return skillImportEnvironment(roots);
		case "inherited":
			return inheritedPythonEnvironment(roots);
	}
}

/**
 * Process groups of the replays still running, each with the temporary working
 * directory its run owns (none for a caller's `cwd`). If this process exits or
 * is signalled first, the group is killed and the directory removed
 * synchronously, since the run's own async cleanup may never get to run. A
 * group is journaled (`recordOrphanProcessState`) for as long as it is tracked.
 */
const liveReplayGroups = new Map<number, string | undefined>();
let replayCleanupInstalled = false;

function killLiveReplayGroups(): void {
	for (const [pid, workdir] of liveReplayGroups) {
		try {
			process.kill(-pid, "SIGKILL");
		} catch {
			// The group is already gone.
		}
		recordOrphanProcessState(pid, false);
		if (workdir === undefined) continue;
		try {
			rmSync(workdir, { recursive: true, force: true });
		} catch {
			// The run's own cleanup tries again if this process survives the signal.
		}
	}
	liveReplayGroups.clear();
}

/**
 * Kill the replay groups, then step aside so the signal does exactly what it
 * would have done without this listener. It is prepended, so it removes itself
 * before any other listener looks: a `once` handler still runs, and
 * `signal-exit`, which re-raises only when it counts no listener but its own,
 * still does. Only when nothing else listens does it deliver the signal again,
 * to the default action it displaced.
 */
function onReplayCleanupSignal(signal: NodeJS.Signals): void {
	killLiveReplayGroups();
	uninstallReplayCleanup();
	if (process.listenerCount(signal) === 0) process.kill(process.pid, signal);
}

function installReplayCleanup(): void {
	replayCleanupInstalled = true;
	process.on("exit", killLiveReplayGroups);
	for (const signal of CLEANUP_SIGNALS) process.prependListener(signal, onReplayCleanupSignal);
}

function uninstallReplayCleanup(): void {
	replayCleanupInstalled = false;
	process.removeListener("exit", killLiveReplayGroups);
	for (const signal of CLEANUP_SIGNALS) process.removeListener(signal, onReplayCleanupSignal);
}

function trackReplayGroup(pid: number, workdir: string | undefined): void {
	liveReplayGroups.set(pid, workdir);
	recordOrphanProcessState(pid, true);
	if (!replayCleanupInstalled) installReplayCleanup();
}

function untrackReplayGroup(pid: number): void {
	if (!liveReplayGroups.delete(pid)) return;
	recordOrphanProcessState(pid, false);
	if (liveReplayGroups.size === 0) uninstallReplayCleanup();
}

/**
 * Run one replay case in a subprocess. Resolves to `unrunnable` rather than
 * throwing: every path out of here is a verdict the gate can charge.
 */
export function runReplayCase(replay: ReplayCase, options: ReplayRunOptions = {}): Promise<ReplayOutcome> {
	const timeoutMs = options.timeoutMs ?? DEFAULT_REPLAY_TIMEOUT_MS;
	return withSpan(
		"ravo.replay_case",
		{
			"referee.language": replay.language,
			"referee.timeout_ms": timeoutMs,
			"referee.environment": options.environment ?? "sanitized",
		},
		async (span): Promise<ReplayOutcome> => {
			const pythonPath = options.pythonPath ?? resolveKernelPython();
			if (!pythonPath) {
				const outcome: ReplayOutcome = {
					kind: "unrunnable",
					detail: "no kernel python: the replay case could not be executed",
				};
				span.setAttributes({ "referee.outcome": outcome.kind, "referee.python": false });
				return outcome;
			}
			span.setAttributes({ "referee.python": true });
			const outcome = await spawnReplay(replay, pythonPath, timeoutMs, options);
			span.setAttributes({
				"referee.outcome": outcome.kind,
				...(outcome.kind === "raised" ? { "referee.exception_class": outcome.exceptionClass } : {}),
			});
			return outcome;
		},
	);
}

async function spawnReplay(
	replay: ReplayCase,
	pythonPath: string,
	timeoutMs: number,
	options: ReplayRunOptions,
): Promise<ReplayOutcome> {
	let workdir: string | undefined;
	if (options.cwd === undefined) {
		try {
			workdir = await mkdtemp(path.join(os.tmpdir(), "prime-agent-replay-"));
		} catch (error) {
			return {
				kind: "unrunnable",
				detail: `no working directory for the replay case: ${error instanceof Error ? error.message : String(error)}`,
			};
		}
	}
	try {
		return await runInterpreter(replayProgram(replay.source), pythonPath, timeoutMs, {
			cwd: options.cwd ?? workdir,
			workdir,
			env: replayEnvironment(options.environment ?? "sanitized", [
				...(options.sysPath ?? []),
				...(replay.sysPath ?? []),
			]),
			...(options.signal ? { signal: options.signal } : {}),
		});
	} finally {
		if (workdir) await rm(workdir, { recursive: true, force: true }).catch(() => undefined);
	}
}

function runInterpreter(
	program: string,
	pythonPath: string,
	timeoutMs: number,
	options: { cwd: string | undefined; workdir: string | undefined; env: NodeJS.ProcessEnv; signal?: AbortSignal },
): Promise<ReplayOutcome> {
	return new Promise((resolve) => {
		let settled = false;
		let stdout = "";
		let stderr = "";
		let child: ChildProcess | undefined;
		let timer: ReturnType<typeof setTimeout> | undefined;
		const killGroup = () => {
			if (child?.pid === undefined) return;
			if (process.platform === "win32") {
				if (child.exitCode === null && child.signalCode === null) child.kill("SIGKILL");
				return;
			}
			try {
				process.kill(-child.pid, "SIGKILL");
			} catch {
				// The group is already gone.
			}
			untrackReplayGroup(child.pid);
		};
		const onAbort = () => finish({ kind: "unrunnable", detail: "replay case aborted" });
		const finish = (outcome: ReplayOutcome) => {
			if (settled) return;
			settled = true;
			if (timer) clearTimeout(timer);
			options.signal?.removeEventListener("abort", onAbort);
			killGroup();
			resolve(outcome);
		};
		if (options.signal?.aborted) {
			onAbort();
			return;
		}
		try {
			child = spawn(pythonPath, ["-I", "-B", "-"], {
				cwd: options.cwd,
				env: options.env,
				stdio: ["pipe", "pipe", "pipe"],
				detached: process.platform !== "win32",
			});
		} catch (error) {
			finish({
				kind: "unrunnable",
				detail: `spawn failed for ${pythonPath}: ${error instanceof Error ? error.message : String(error)}`,
			});
			return;
		}
		if (child.pid !== undefined && process.platform !== "win32") trackReplayGroup(child.pid, options.workdir);
		timer = setTimeout(
			() => finish({ kind: "unrunnable", detail: `replay case timed out after ${timeoutMs}ms` }),
			timeoutMs,
		);
		options.signal?.addEventListener("abort", onAbort, { once: true });
		child.stdout?.setEncoding("utf8");
		child.stderr?.setEncoding("utf8");
		child.stdout?.on("data", (chunk: string) => {
			if (stdout.length < MAX_CAPTURE_CHARS) stdout = (stdout + chunk).slice(0, MAX_CAPTURE_CHARS);
		});
		child.stderr?.on("data", (chunk: string) => {
			if (stderr.length < MAX_CAPTURE_CHARS) stderr = (stderr + chunk).slice(0, MAX_CAPTURE_CHARS);
		});
		child.on("error", (error) => {
			finish({ kind: "unrunnable", detail: `spawn failed for ${pythonPath}: ${error.message}` });
		});
		child.on("close", (code, signalCode) => {
			finish(parseOutcome(stdout, stderr, code, signalCode));
		});
		child.stdin?.on("error", () => {
			// The interpreter exited before reading its program; `close` reports why.
		});
		child.stdin?.end(program);
	});
}

export interface AdjudicationOptions extends RefereeRunOptions {
	/**
	 * Modules the proposal's skill writes import (`skillImportsOf`). Required:
	 * a replay speaks only to a claim one of them can fix, and defaulting either
	 * way would silently excuse or silently charge every other proposal.
	 */
	skillImports: readonly string[];
}

/**
 * Adjudicate the fingerprints a proposal claims to have addressed. Records the
 * proposal does not claim are not adjudicated at all: there is no claim to
 * refute, so the gate charges them exactly what it charges today.
 *
 * Applicability is decided per fingerprint. Its cases that apply are the
 * missing-module and missing-distribution probes naming something a skill of
 * the proposal imports (`replayAppliesToSkillImports`); with none, the claim is
 * `not_applicable` and nothing runs. With some but none verified there is no
 * evidence. Otherwise ONLY the applicable verified cases run, in the
 * environment the skill dry-run screens imports in (`skillImportEnvironment`,
 * with `sysPath` as further roots): any reproduction upholds the failure, any
 * case that cannot run (or raises something else) is unverifiable, and only an
 * all-clean run clears it.
 *
 * Cases run one at a time. A claimed recurring fingerprint set is small, and
 * serializing keeps the referee off the critical path of everything else.
 */
export function adjudicateFailureClaims(
	records: readonly FailureRecord[],
	claimedFingerprintIds: readonly string[],
	options: AdjudicationOptions,
): Promise<RefereeVerdict[]> {
	const claimed = new Set(claimedFingerprintIds);
	const adjudicable = records.filter((record) => claimed.has(record.fingerprint.id));
	if (adjudicable.length === 0) return Promise.resolve([]);
	const skillImports = [...options.skillImports];
	const runOptions = refereeRunOptions(options);
	return withSpan(
		"ravo.referee",
		{ "referee.claimed": adjudicable.length, "referee.skill_imports": skillImports.length },
		async (span) => {
			const verdicts: RefereeVerdict[] = [];
			// An unverifiable verdict reached with the signal aborted is a run cut short, not a measurement: the
			// span does not count it, though the caller still gets it and decides what an abort means.
			const interrupted = new Set<RefereeVerdict>();
			for (const record of adjudicable) {
				const verdict = await adjudicateRecord(record, skillImports, runOptions);
				if (verdict.status === "unverifiable" && runOptions.signal?.aborted) interrupted.add(verdict);
				verdicts.push(verdict);
			}
			const measured = verdicts.filter((verdict) => !interrupted.has(verdict));
			const count = (status: RefereeVerdictStatus) => measured.filter((verdict) => verdict.status === status).length;
			span.setAttributes({
				"referee.adjudicated": measured.filter(refereeVerdictIsEvidence).length,
				"referee.upheld": count("upheld"),
				"referee.cleared": count("cleared"),
				"referee.unverifiable": count("unverifiable"),
				"referee.no_evidence": count("no_evidence"),
				"referee.not_applicable": count("not_applicable"),
				"referee.aborted": runOptions.signal?.aborted === true,
			});
			return verdicts;
		},
	);
}

async function adjudicateRecord(
	record: FailureRecord,
	skillImports: readonly string[],
	options: RefereeRunOptions,
): Promise<RefereeVerdict> {
	const fingerprintId = record.fingerprint.id;
	const cases = replayCasesOf(record).filter((replay) => replayProbeOf(replay) !== undefined);
	const applicable = cases.filter((replay) => replayAppliesToSkillImports(replay, skillImports));
	if (applicable.length === 0) {
		return refereeVerdict(
			fingerprintId,
			"not_applicable",
			cases.length > 0
				? "not_applicable: no skill the proposal writes imports what this failure's replay cases probe"
				: "not_applicable: no replay case derivable for this failure",
		);
	}
	const verified = applicable.filter((replay) => replay.verifiedAt !== undefined);
	if (verified.length === 0) {
		return refereeVerdict(
			fingerprintId,
			"no_evidence",
			"no_evidence: no replay case probing an import of the proposal's skills ever reproduced this failure",
		);
	}
	const results: Array<{ status: RefereeVerdictStatus; detail: string }> = [];
	for (const replay of verified) {
		const outcome = await runReplayCase(replay, { ...options, environment: "skill-import" });
		results.push({ status: verdictFromOutcome(replay, outcome), detail: outcome.detail });
	}
	const status: RefereeVerdictStatus = results.some((result) => result.status === "upheld")
		? "upheld"
		: results.every((result) => result.status === "cleared")
			? "cleared"
			: "unverifiable";
	const decisive = results.find((result) => result.status === status) ?? results[0];
	const suffix = verified.length > 1 ? ` (${verified.length} verified cases)` : "";
	return refereeVerdict(fingerprintId, status, `${status}: ${decisive.detail}${suffix}`);
}

/**
 * The capture-time self-check for a batch: run every UNVERIFIED case once, in
 * the sanitized environment (the host's PYTHONPATH never decides whether an
 * observed failure reproduces), and return the ones that reproduced the
 * exception recorded with them. Accepts
 * fresh observations (their single derived case) or ledger records (every
 * unverified case they carry); a (fingerprint, source) pair runs at most once,
 * and a source that is not a valid probe (`replayProbeOf`) never runs. Apply
 * the result with `applyReplayVerifications`.
 */
export async function verifyObservedReplayCases(
	items: readonly (FailureRecord | FailureObservation)[],
	options: RefereeRunOptions = {},
): Promise<ReplayVerification[]> {
	const pending = new Map<string, { fingerprintId: string; replay: ReplayCase }>();
	for (const item of items) {
		const cases = "count" in item ? replayCasesOf(item) : item.replayCase ? [item.replayCase] : [];
		for (const replay of cases) {
			if (replay.verifiedAt || !replayProbeOf(replay)) continue;
			const key = `${item.fingerprint.id}\u0000${replay.source}`;
			if (!pending.has(key)) pending.set(key, { fingerprintId: item.fingerprint.id, replay });
		}
	}
	if (pending.size === 0) return [];
	const now = options.now ?? (() => new Date().toISOString());
	const runOptions = refereeRunOptions(options);
	return withSpan("ravo.replay_verify", { "referee.cases": pending.size }, async (span) => {
		const verifications: ReplayVerification[] = [];
		let ran = 0;
		for (const { fingerprintId, replay } of pending.values()) {
			if (options.signal?.aborted) break;
			const outcome = await runReplayCase(replay, runOptions);
			ran += 1;
			if (verdictFromOutcome(replay, outcome) === "upheld") {
				verifications.push({ fingerprintId, source: replay.source, verifiedAt: now() });
			}
		}
		span.setAttributes({ "referee.ran": ran, "referee.verified": verifications.length });
		return verifications;
	});
}

/**
 * Self-check one observation's derived case inline: returns it stamped
 * `verifiedAt` when it reproduces the exception recorded with it, and undefined
 * otherwise. It never derives a case itself, so it can only run what
 * `extractFailures` derived from a kernel cell's own traceback.
 *
 * The session does not call this. It records derived cases unverified and
 * verifies them off the turn path (`verifyObservedReplayCases`, then
 * `applyReplayVerifications`); an unverified case stays in the ledger but is
 * never evidence.
 */
export async function captureReplayCase(
	observation: FailureObservation,
	options: RefereeRunOptions = {},
): Promise<ReplayCase | undefined> {
	const derived = observation.replayCase;
	if (!derived || !replayProbeOf(derived)) return undefined;
	if (derived.verifiedAt) return derived;
	const outcome = await runReplayCase(derived, refereeRunOptions(options));
	if (verdictFromOutcome(derived, outcome) !== "upheld") return undefined;
	const now = options.now ?? (() => new Date().toISOString());
	return { ...derived, verifiedAt: now() };
}

/**
 * `captureReplayCase` over a batch, in observation order: an observation whose
 * case reproduced carries it verified, and every other observation is returned
 * unchanged (an unverified case is kept, as the ledger keeps it).
 */
export async function captureReplayCases(
	observations: readonly FailureObservation[],
	options: RefereeRunOptions = {},
): Promise<FailureObservation[]> {
	const captured: FailureObservation[] = [];
	for (const observation of observations) {
		const replayCase = await captureReplayCase(observation, options);
		captured.push(replayCase ? { ...observation, replayCase } : observation);
	}
	return captured;
}
