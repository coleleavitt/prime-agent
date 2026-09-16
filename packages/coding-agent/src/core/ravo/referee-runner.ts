import { spawn } from "node:child_process";
import path from "node:path";
import { withSpan } from "@earendil-works/pi-ai";
import { resolveKernelPython } from "../refinement/skill-dry-run.js";
import type { FailureObservation, FailureRecord } from "./failure-ledger.js";
import {
	deriveReplayCase,
	type RefereeVerdict,
	type ReplayCase,
	type ReplayOutcome,
	refereeVerdict,
	replayCaseOf,
	verdictFromOutcome,
} from "./referee.js";

/**
 * Executes replay cases. This is the only place in the refinement loop that
 * produces a signal the proposal did not write.
 *
 * The spawn shape is the one `skill-dry-run.ts` `probeOne` already uses: the
 * kernel python in isolated mode (`-I -c`), the program in argv so nothing
 * touches the filesystem, extra sys.path roots passed as one argv entry, and
 * PYTHONPATH re-applied explicitly because `-I` implies `-E`. What differs is
 * the polarity of failure: an unrunnable probe is `unrunnable`, never a pass
 * (see `refereeOpponentPassed`).
 */

export const DEFAULT_REPLAY_TIMEOUT_MS = 10_000;
const MAX_DETAIL_CHARS = 1000;
const RAISED_MARKER = "RAISED ";
const CLEAN_MARKER = "CLEAN";

const REPLAY_PROGRAM = [
	"import os, sys",
	"source, extra = sys.argv[1], sys.argv[2]",
	"paths = [p for p in extra.split(os.pathsep) if p] + [p for p in os.environ.get('PYTHONPATH', '').split(os.pathsep) if p]",
	"sys.path[0:0] = paths",
	"try:",
	"    exec(compile(source, '<replay-case>', 'exec'), {'__name__': '__replay__'})",
	"except BaseException as exc:",
	`    sys.stdout.write('${RAISED_MARKER}' + type(exc).__name__ + '\\n' + str(exc)[:1000])`,
	"    sys.stdout.flush()",
	"    raise SystemExit(3)",
	`sys.stdout.write('${CLEAN_MARKER}')`,
].join("\n");

export interface RefereeRunOptions {
	/** Interpreter override; defaults to the kernel python (`resolveKernelPython`). */
	pythonPath?: string;
	timeoutMs?: number;
	cwd?: string;
	env?: NodeJS.ProcessEnv;
	/** Extra sys.path roots, prepended before the case's own. */
	sysPath?: readonly string[];
	signal?: AbortSignal;
	now?: () => string;
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

/**
 * Run one replay case in a subprocess. Resolves to `unrunnable` rather than
 * throwing: every path out of here is a verdict the gate can charge.
 */
export function runReplayCase(replay: ReplayCase, options: RefereeRunOptions = {}): Promise<ReplayOutcome> {
	const timeoutMs = options.timeoutMs ?? DEFAULT_REPLAY_TIMEOUT_MS;
	return withSpan(
		"ravo.replay_case",
		{ "referee.language": replay.language, "referee.timeout_ms": timeoutMs },
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

function spawnReplay(
	replay: ReplayCase,
	pythonPath: string,
	timeoutMs: number,
	options: RefereeRunOptions,
): Promise<ReplayOutcome> {
	return new Promise((resolve) => {
		let settled = false;
		let stdout = "";
		let stderr = "";
		const finish = (outcome: ReplayOutcome) => {
			if (settled) return;
			settled = true;
			clearTimeout(timer);
			options.signal?.removeEventListener("abort", onAbort);
			resolve(outcome);
		};
		const sysPath = [...(options.sysPath ?? []), ...(replay.sysPath ?? [])].join(path.delimiter);
		let child: ReturnType<typeof spawn>;
		try {
			child = spawn(pythonPath, ["-I", "-c", REPLAY_PROGRAM, replay.source, sysPath], {
				cwd: options.cwd,
				env: { ...process.env, ...options.env },
				stdio: ["ignore", "pipe", "pipe"],
			});
		} catch (error) {
			finish({
				kind: "unrunnable",
				detail: `spawn failed for ${pythonPath}: ${error instanceof Error ? error.message : String(error)}`,
			});
			return;
		}
		const killChild = () => {
			if (child.exitCode === null && child.signalCode === null) child.kill("SIGKILL");
		};
		const timer = setTimeout(() => {
			killChild();
			finish({ kind: "unrunnable", detail: `replay case timed out after ${timeoutMs}ms` });
		}, timeoutMs);
		const onAbort = () => {
			killChild();
			finish({ kind: "unrunnable", detail: "replay case aborted" });
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
			finish({ kind: "unrunnable", detail: `spawn failed for ${pythonPath}: ${error.message}` });
		});
		child.on("close", (code, signalCode) => {
			finish(parseOutcome(stdout, stderr, code, signalCode));
		});
	});
}

/**
 * Adjudicate the fingerprints a proposal claims to have addressed. Records the
 * proposal does not claim are not adjudicated at all: there is no claim to
 * refute, so the gate charges them exactly what it charges today.
 *
 * Cases run one at a time. A claimed recurring fingerprint set is small, and
 * serializing keeps the referee off the critical path of everything else.
 */
export function adjudicateFailureClaims(
	records: readonly FailureRecord[],
	claimedFingerprintIds: readonly string[],
	options: RefereeRunOptions = {},
): Promise<RefereeVerdict[]> {
	const claimed = new Set(claimedFingerprintIds);
	const adjudicable = records.filter((record) => claimed.has(record.fingerprint.id));
	if (adjudicable.length === 0) return Promise.resolve([]);
	return withSpan("ravo.referee", { "referee.claimed": adjudicable.length }, async (span) => {
		const verdicts: RefereeVerdict[] = [];
		for (const record of adjudicable) {
			const replay = replayCaseOf(record);
			if (!replay) {
				verdicts.push(
					refereeVerdict(record.fingerprint.id, "no_evidence", "no replay case recorded for this fingerprint"),
				);
				continue;
			}
			const outcome = await runReplayCase(replay, options);
			const status = verdictFromOutcome(replay, outcome);
			verdicts.push(refereeVerdict(record.fingerprint.id, status, `${status}: ${outcome.detail}`));
		}
		const count = (status: string) => verdicts.filter((verdict) => verdict.status === status).length;
		span.setAttributes({
			"referee.upheld": count("upheld"),
			"referee.cleared": count("cleared"),
			"referee.unverifiable": count("unverifiable"),
			"referee.no_evidence": count("no_evidence"),
		});
		return verdicts;
	});
}

/**
 * The capture-time self-check: derive a replay case for a fresh observation and
 * keep it ONLY if it reproduces the exception that was just recorded. A case
 * that never reproduced is not an oracle — a later clean run of it would say
 * nothing — so an unverified derivation is dropped rather than stored.
 */
export async function captureReplayCase(
	observation: FailureObservation,
	options: RefereeRunOptions = {},
): Promise<ReplayCase | undefined> {
	const derived = deriveReplayCase(observation.fingerprint, observation.excerpt);
	if (!derived) return undefined;
	const outcome = await runReplayCase(derived, options);
	if (verdictFromOutcome(derived, outcome) !== "upheld") return undefined;
	const now = options.now ?? (() => new Date().toISOString());
	return { ...derived, verifiedAt: now() };
}

/** Capture-time self-check for a batch of observations, in observation order. */
export async function captureReplayCases(
	observations: readonly FailureObservation[],
	options: RefereeRunOptions = {},
): Promise<FailureObservation[]> {
	const captured: FailureObservation[] = [];
	for (const observation of observations) {
		if (observation.replayCase) {
			captured.push(observation);
			continue;
		}
		const replayCase = await captureReplayCase(observation, options);
		captured.push(replayCase ? { ...observation, replayCase } : observation);
	}
	return captured;
}
