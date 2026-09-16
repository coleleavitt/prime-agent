import type { FailureFingerprint, FailureRecord } from "./failure-ledger.js";

/**
 * The referee: mechanical adjudication of a proposal's claim to have fixed a
 * recorded failure.
 *
 * Every other gate in the refinement loop reduces to the proposal grading
 * itself — the failure opponent passed iff the proposal (or the judge reading
 * it) listed the fingerprint in `addressedFingerprints`, with nothing re-run to
 * check that the failure actually stopped. The referee is the one signal
 * written by something other than the thing being judged: a failure record
 * carries a REPLAY CASE, and the claim is upheld or refuted by re-executing it
 * in a subprocess (`referee-runner.ts`).
 *
 * This mirrors Rocq `Ravo.v` Section 16: a challenge is a transcript of prose
 * plus at most one executable test; the verdict is `FlawUpheld` iff the test
 * runs and fails, `NoFlawFound` otherwise (Thm 16.2 `verdict_upheld_iff`,
 * Thm 16.3 `prose_is_not_evidence`, `no_test_no_flaw`). The referee "joins the
 * pool as one more opponent, which can only tighten the gate": `referee:<fp>`
 * is a criterion alongside `failure:<fp>`, adjudicated only for fingerprints
 * the proposal actually claims, so an unclaimed fingerprint is charged exactly
 * what it is charged today and nothing is ever excused that was not excused
 * before.
 */

export const REFEREE_OPPONENT_PREFIX = "referee:";

/** Language of a replay case. Python only: the kernel is the runtime the harness can re-enter. */
export type ReplayLanguage = "python";

export interface ReplayCase {
	language: ReplayLanguage;
	/**
	 * Program that MUST raise the recorded exception while the failure is live.
	 * Executed with the interpreter in isolated mode; it never imports the
	 * agent, and it is only ever derived from the recorded traceback.
	 */
	source: string;
	/** Exception class the recorded failure raised; the verdict compares against it. */
	exceptionClass?: string;
	/** Extra sys.path roots the case needs (e.g. a skill package root). */
	sysPath?: string[];
	/**
	 * When the self-check saw this case reproduce the recorded failure. A case
	 * that has never reproduced is NOT evidence: a clean run of it says nothing
	 * about whether the failure stopped, so the referee abstains instead of
	 * clearing the claim.
	 */
	verifiedAt?: string;
}

export type ReplayOutcome =
	| { kind: "raised"; exceptionClass: string; detail: string }
	| { kind: "clean"; detail: string }
	/** The verification itself could not be performed. Never a pass; see `refereeOpponentPassed`. */
	| { kind: "unrunnable"; detail: string };

export type RefereeVerdictStatus =
	/** The case ran and the recorded exception recurred: the claim is refuted. */
	| "upheld"
	/** The case ran clean and had previously reproduced: the claim is corroborated. */
	| "cleared"
	/** The case exists but could not be run, or raised something else. Conservative reject. */
	| "unverifiable"
	/** No executable case, or one that never reproduced. Rocq `no_test_no_flaw`. */
	| "no_evidence";

export interface RefereeVerdict {
	fingerprintId: string;
	status: RefereeVerdictStatus;
	detail: string;
}

const MAX_REPLAY_SOURCE_CHARS = 600;
const MODULE_PATTERN = /^[A-Za-z_][A-Za-z0-9_]*(?:\.[A-Za-z_][A-Za-z0-9_]*)*$/;
const IDENTIFIER_PATTERN = /^[A-Za-z_][A-Za-z0-9_]*$/;
const NO_MODULE_NAMED = /no module named ['"]([^'"]+)['"]/i;
const CANNOT_IMPORT_NAME = /cannot import name ['"]([^'"]+)['"] from ['"]([^'"]+)['"]/i;
const MODULE_HAS_NO_ATTRIBUTE = /module ['"]([^'"]+)['"] has no attribute ['"]([^'"]+)['"]/i;

export function refereeOpponentId(fingerprint: FailureFingerprint | string): string {
	const id = typeof fingerprint === "string" ? fingerprint : fingerprint.id;
	return id.startsWith(REFEREE_OPPONENT_PREFIX) ? id : `${REFEREE_OPPONENT_PREFIX}${id}`;
}

export function isRefereeOpponentId(criterionId: string): boolean {
	return criterionId.startsWith(REFEREE_OPPONENT_PREFIX) && criterionId.length > REFEREE_OPPONENT_PREFIX.length;
}

export function refereeOpponentFingerprint(criterionId: string): string | undefined {
	return isRefereeOpponentId(criterionId) ? criterionId.slice(REFEREE_OPPONENT_PREFIX.length) : undefined;
}

/**
 * Derive an executable reproduction from the recorded traceback. Only failure
 * classes with a side-effect-free, deterministic reproduction qualify: a
 * missing module, a missing name in a module, and a missing module attribute.
 * They are the ones where re-running the case cannot itself damage anything,
 * and `module ? has no attribute ?` is the second largest fingerprint class in
 * the corpus. Everything else yields no case, and the gate falls back to the
 * claim rather than inventing evidence.
 *
 * The normalized fingerprint message has had quoted strings replaced by `?`,
 * so the module name survives only in the raw excerpt.
 */
export function deriveReplayCase(fingerprint: FailureFingerprint, excerpt: string): ReplayCase | undefined {
	if (fingerprint.kind !== "python_exception") return undefined;
	const source = deriveReplaySource(excerpt);
	if (!source || source.length > MAX_REPLAY_SOURCE_CHARS) return undefined;
	return {
		language: "python",
		source,
		...(fingerprint.exceptionClass === undefined ? {} : { exceptionClass: fingerprint.exceptionClass }),
	};
}

function deriveReplaySource(excerpt: string): string | undefined {
	const missingModule = NO_MODULE_NAMED.exec(excerpt);
	if (missingModule && MODULE_PATTERN.test(missingModule[1])) {
		return `import ${missingModule[1]}`;
	}
	const missingName = CANNOT_IMPORT_NAME.exec(excerpt);
	if (missingName && IDENTIFIER_PATTERN.test(missingName[1]) && MODULE_PATTERN.test(missingName[2])) {
		return `from ${missingName[2]} import ${missingName[1]}`;
	}
	const missingAttribute = MODULE_HAS_NO_ATTRIBUTE.exec(excerpt);
	if (missingAttribute && MODULE_PATTERN.test(missingAttribute[1]) && IDENTIFIER_PATTERN.test(missingAttribute[2])) {
		return `import ${missingAttribute[1]}\ngetattr(${missingAttribute[1]}, ${JSON.stringify(missingAttribute[2])})`;
	}
	return undefined;
}

export function normalizeReplayCase(value: unknown): ReplayCase | undefined {
	if (typeof value !== "object" || value === null || Array.isArray(value)) return undefined;
	const raw = value as Record<string, unknown>;
	if (raw.language !== "python") return undefined;
	if (typeof raw.source !== "string" || !raw.source.trim() || raw.source.length > MAX_REPLAY_SOURCE_CHARS) {
		return undefined;
	}
	const sysPath = Array.isArray(raw.sysPath)
		? raw.sysPath.filter((item): item is string => typeof item === "string" && item.length > 0)
		: [];
	return {
		language: "python",
		source: raw.source,
		...(typeof raw.exceptionClass === "string" ? { exceptionClass: raw.exceptionClass } : {}),
		...(sysPath.length > 0 ? { sysPath } : {}),
		...(typeof raw.verifiedAt === "string" && raw.verifiedAt ? { verifiedAt: raw.verifiedAt } : {}),
	};
}

/** The replay case the referee may adjudicate against, if the record carries one. */
export function replayCaseOf(record: FailureRecord): ReplayCase | undefined {
	return record.replayCase;
}

/** Bare class name of a possibly dotted exception class (`requests.exceptions.ConnectionError`). */
function bareClass(name: string): string {
	return name.split(".").at(-1) ?? name;
}

/**
 * Rocq Def 16.1 `verdictWith`, specialized to a recorded exception: the verdict
 * is a function of the case and its run alone. Prose — the proposal's summary,
 * its rationale, the judge's paragraph — never enters.
 */
export function verdictFromOutcome(replay: ReplayCase, outcome: ReplayOutcome): RefereeVerdictStatus {
	if (outcome.kind === "unrunnable") return "unverifiable";
	if (outcome.kind === "raised") {
		const expected = replay.exceptionClass;
		if (expected === undefined || bareClass(expected) === bareClass(outcome.exceptionClass)) return "upheld";
		// A different exception means the case no longer probes what it was
		// recorded for; it cannot be read as the failure having stopped.
		return "unverifiable";
	}
	return replay.verifiedAt ? "cleared" : "no_evidence";
}

export function refereeVerdict(fingerprintId: string, status: RefereeVerdictStatus, detail: string): RefereeVerdict {
	return { fingerprintId, status, detail };
}

/**
 * `failure:<fp>`: the claim stays necessary, and executable evidence makes it
 * insufficient. Absent a verdict this is exactly the predicate that shipped
 * (the proposal's own claim), so an unclaimed or unverifiable fingerprint is
 * charged no less than before.
 */
export function failureOpponentPassed(claimed: boolean, verdict: RefereeVerdict | undefined): boolean {
	if (!claimed) return false;
	if (!verdict || verdict.status === "no_evidence") return true;
	return verdict.status === "cleared";
}

/**
 * `referee:<fp>`: the second opponent the Rocq development adds to the pool.
 * It only ever adjudicates a claim, so a proposal that claims nothing is
 * charged nothing. A claim the referee refutes, and a claim whose verification
 * could not be run, both fail.
 *
 * FAIL CLOSED on `unverifiable`, deliberately unlike `skill-dry-run.ts`, whose
 * screen skips (ok=true) when no kernel python exists. That is sound there: a
 * false screen can only cause a false REJECTION (Lean `bestScore_screenedStep`),
 * so skipping preserves the safety direction. Here the polarity is inverted —
 * the referee's output is the only thing standing between an unchecked claim
 * and a commit, so a verification that cannot be performed must not be read as
 * a verification that succeeded.
 */
export function refereeOpponentPassed(claimed: boolean, verdict: RefereeVerdict | undefined): boolean {
	if (!claimed || !verdict) return true;
	return verdict.status === "cleared" || verdict.status === "no_evidence";
}

export function refereeDetail(verdict: RefereeVerdict | undefined, claimed: boolean): string {
	if (!claimed) return "no claim to adjudicate";
	if (!verdict) return "no replay case recorded for this fingerprint";
	return verdict.detail;
}
