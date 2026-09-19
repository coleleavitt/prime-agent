import { withSpan } from "@earendil-works/pi-ai";
import { applyReplayVerifications, type FailureRecord, type ReplayVerification } from "../ravo/failure-ledger.js";
import {
	type ReplayCase,
	replayAppliesToSkillImports,
	replayProbeOf,
	skillImportsOf,
	verifiedReplayCasesOf,
} from "../ravo/referee.js";
import { adjudicateFailureClaims, type RefereeRunOptions } from "../ravo/referee-runner.js";
import { toolforgeSrcRoots } from "../toolforge/ledger.js";
import {
	type HarnessTrustWindow,
	type HarnessTrustWindows,
	MAX_TRUST_ADJUDICATION_RUNS,
	parseHarnessEntryRef,
	type TrustWindowEvidence,
} from "./harness-trust.js";
import type { HarnessScope, HarnessState } from "./refinement.js";

/**
 * Post-commit referee replays: the only path by which a committed refinement
 * loses trust.
 *
 * A window's claimed failure recurring is not by itself evidence against any
 * entry the commit wrote: a fingerprint folds every missing module into one, a
 * memory or prompt fix cannot be probed at all, and a skill may have been
 * rewritten since. A replay is planned only for a skill entry the window
 * touched whose imports are still the ones the commit recorded, only on the
 * newest overlapping window that wrote them, and only when the recurrence's
 * own derived case probes one of those imports. It runs the same verified
 * probes, in the same `skill-import` environment and toolforge roots, as the
 * gate's referee.
 */

export const MAX_TRUST_ADJUDICATION_JOBS = 8;

export interface TrustRecurrence {
	proposalId: string;
	fingerprintId: string;
	ordinal: number;
	/** Valid probes derived from this batch's actionable occurrences of the fingerprint, distinct by source. */
	observedCases: ReplayCase[];
}

export interface TrustAdjudicationJob {
	scope: HarnessScope;
	proposalId: string;
	/** The touched `skill:<id>` the replay runs for. */
	entry: string;
	fingerprintId: string;
	ordinal: number;
	/** The imports the window recorded for the entry, which are also its current ones. */
	skillImports: string[];
	record: FailureRecord;
	triggerTraceId?: string;
}

/** A job whose observed case has not reproduced yet; released when the self-check verifies one of `sources`. */
export interface AwaitingTrustAdjudication {
	job: TrustAdjudicationJob;
	sources: string[];
}

export type ScopedTrustEvidence = Extract<TrustWindowEvidence, { type: "adjudication" }> & { scope: HarnessScope };

function inRange(window: Pick<HarnessTrustWindow, "committedTurn" | "untilTurn">, ordinal: number): boolean {
	return ordinal >= window.committedTurn && ordinal <= window.untilTurn;
}

function sameImports(left: readonly string[], right: readonly string[]): boolean {
	const set = new Set(left);
	return set.size === new Set(right).size && right.every((module) => set.has(module));
}

/**
 * The open windows `ordinal` falls in that claimed a fingerprint which recurred
 * in this batch, sorted by proposal then fingerprint. Only valid probes are
 * carried as observed cases.
 */
export function findTrustWindowRecurrences(
	windows: HarnessTrustWindows | undefined,
	recurred: ReadonlyMap<string, readonly ReplayCase[]>,
	ordinal: number,
): TrustRecurrence[] {
	const recurrences: TrustRecurrence[] = [];
	for (const window of Object.values(windows ?? {})) {
		if (window.outcome !== "open" || !inRange(window, ordinal)) continue;
		for (const fingerprintId of window.claimedFingerprints) {
			const cases = recurred.get(fingerprintId);
			if (cases === undefined) continue;
			const bySource = new Map<string, ReplayCase>();
			for (const replay of cases) {
				if (replayProbeOf(replay) && !bySource.has(replay.source)) bySource.set(replay.source, replay);
			}
			recurrences.push({
				proposalId: window.proposalId,
				fingerprintId,
				ordinal,
				observedCases: [...bySource.values()],
			});
		}
	}
	return recurrences.sort(
		(left, right) =>
			left.proposalId.localeCompare(right.proposalId) || left.fingerprintId.localeCompare(right.fingerprintId),
	);
}

export function trustAdjudicationKey(
	job: Pick<TrustAdjudicationJob, "scope" | "proposalId" | "entry" | "fingerprintId">,
): string {
	return `${job.scope} ${job.proposalId} ${job.entry} ${job.fingerprintId}`;
}

/**
 * The window a replay fact about (`entry`, `fingerprintId`) at `ordinal` is
 * charged to: the newest by commit ordinal (ties: proposal id, descending) of
 * the windows that claimed the fingerprint, touched the entry with the same
 * recorded imports, and hold the ordinal. Any older overlapping one, such as
 * the window a regression repair replaced, is superseded.
 */
function newestAttributableWindow(
	windows: HarnessTrustWindows,
	entry: string,
	fingerprintId: string,
	imports: readonly string[],
	ordinal: number,
): HarnessTrustWindow | undefined {
	let newest: HarnessTrustWindow | undefined;
	for (const window of Object.values(windows)) {
		if (window.outcome === "unmeasured" || !inRange(window, ordinal)) continue;
		if (!window.claimedFingerprints.includes(fingerprintId) || !window.touched.includes(entry)) continue;
		const recorded = window.skillImports?.[entry];
		if (!recorded || !sameImports(recorded, imports)) continue;
		if (
			!newest ||
			window.committedTurn > newest.committedTurn ||
			(window.committedTurn === newest.committedTurn && window.proposalId.localeCompare(newest.proposalId) > 0)
		) {
			newest = window;
		}
	}
	return newest;
}

/**
 * Plan the replays a batch of recurrences warrants in one scope. A job exists
 * for a (window, touched skill entry, fingerprint) only when the entry still
 * imports what the window recorded, the window is the newest that wrote those
 * imports over the ordinal, it is not faulted, has no upheld verdict and fewer
 * than {@link MAX_TRUST_ADJUDICATION_RUNS} runs for the pair, and a case this
 * batch observed probes one of the imports. It is queued when the record
 * already holds a verified case probing them, and otherwise awaits the
 * self-check of the observed sources. Memory, prompt and subagent entries never
 * get a job. Each list is capped at {@link MAX_TRUST_ADJUDICATION_JOBS}.
 */
export function planTrustAdjudications(input: {
	scope: HarnessScope;
	windows: HarnessTrustWindows | undefined;
	recurrences: readonly TrustRecurrence[];
	entries: HarnessState["entries"];
	recordOf: (fingerprintId: string) => FailureRecord | undefined;
	triggerTraceId?: string;
}): { jobs: TrustAdjudicationJob[]; awaiting: AwaitingTrustAdjudication[] } {
	const jobs: TrustAdjudicationJob[] = [];
	const awaiting: AwaitingTrustAdjudication[] = [];
	const windows = input.windows ?? {};
	const planned = new Set<string>();
	for (const recurrence of input.recurrences) {
		if (!Object.hasOwn(windows, recurrence.proposalId)) continue;
		const window = windows[recurrence.proposalId];
		const { fingerprintId, ordinal } = recurrence;
		if (window.outcome === "faulted" || window.outcome === "unmeasured") continue;
		if (!window.claimedFingerprints.includes(fingerprintId) || !inRange(window, ordinal)) continue;
		for (const entry of window.touched) {
			const parsed = parseHarnessEntryRef(entry);
			if (parsed?.kind !== "skill") continue;
			const recorded = window.skillImports?.[entry];
			if (!recorded || recorded.length === 0) continue;
			const skill = Object.hasOwn(input.entries.skill, parsed.id) ? input.entries.skill[parsed.id] : undefined;
			if (!skill) continue;
			const current = skillImportsOf([{ kind: "skill", action: "update", reference: skill.reference }]);
			if (!sameImports(current, recorded)) continue;
			if (newestAttributableWindow(windows, entry, fingerprintId, recorded, ordinal) !== window) continue;
			const adjudication = window.adjudications?.find(
				(item) => item.entry === entry && item.fingerprintId === fingerprintId,
			);
			if (adjudication?.status === "upheld" || (adjudication?.runs.length ?? 0) >= MAX_TRUST_ADJUDICATION_RUNS) {
				continue;
			}
			const sources = [
				...new Set(
					recurrence.observedCases
						.filter((replay) => replayAppliesToSkillImports(replay, recorded))
						.map((replay) => replay.source),
				),
			].sort();
			if (sources.length === 0) continue;
			const record = input.recordOf(fingerprintId);
			if (!record) continue;
			const job: TrustAdjudicationJob = {
				scope: input.scope,
				proposalId: window.proposalId,
				entry,
				fingerprintId,
				ordinal,
				skillImports: [...recorded],
				record,
				...(input.triggerTraceId === undefined ? {} : { triggerTraceId: input.triggerTraceId }),
			};
			const key = trustAdjudicationKey(job);
			if (planned.has(key)) continue;
			planned.add(key);
			if (verifiedReplayCasesOf(record).some((replay) => replayAppliesToSkillImports(replay, recorded))) {
				if (jobs.length < MAX_TRUST_ADJUDICATION_JOBS) jobs.push(job);
			} else if (awaiting.length < MAX_TRUST_ADJUDICATION_JOBS) {
				awaiting.push({ job, sources });
			}
		}
	}
	return { jobs, awaiting };
}

/**
 * Release an awaiting job against verifications the self-check just landed.
 * `matched` says one of them verified an awaited source; the job comes back,
 * its record carrying the verifications, only when a verified case now probes
 * its imports.
 */
export function releaseAwaitingTrustAdjudication(
	awaiting: AwaitingTrustAdjudication,
	verifications: readonly ReplayVerification[],
): { matched: boolean; job?: TrustAdjudicationJob } {
	const { job } = awaiting;
	const matches = verifications.filter(
		(verification) =>
			verification.fingerprintId === job.fingerprintId && awaiting.sources.includes(verification.source),
	);
	if (matches.length === 0) return { matched: false };
	const record = applyReplayVerifications(
		{ schema: 1, failures: { [job.fingerprintId]: job.record }, lastScannedEntryIndex: 0 },
		matches,
	).failures[job.fingerprintId];
	if (
		!record ||
		!verifiedReplayCasesOf(record).some((replay) => replayAppliesToSkillImports(replay, job.skillImports))
	) {
		return { matched: true };
	}
	return { matched: true, job: { ...job, record } };
}

/**
 * Run a batch of planned replays serially under one `harness.trust.adjudicate`
 * span and return the verdicts as evidence scoped to the window and entry each
 * ran for. A job aborted before or during its run yields nothing, and neither
 * does a verdict that is not a replay result (`no_evidence`, `not_applicable`),
 * so neither consumes a run. The span ends ok whatever the verdicts: a refuted
 * claim is a measurement, not a failure of the adjudication.
 */
export async function adjudicateTrustRecurrences(
	jobs: readonly TrustAdjudicationJob[],
	options: RefereeRunOptions & { sessionId?: string } = {},
): Promise<ScopedTrustEvidence[]> {
	if (jobs.length === 0) return [];
	const triggers = new Set(jobs.map((job) => job.triggerTraceId));
	const [trigger] = triggers;
	return withSpan(
		"harness.trust.adjudicate",
		{
			"session.id": options.sessionId,
			"trigger.trace_id": triggers.size === 1 ? trigger : undefined,
			"trust.jobs": jobs.length,
			"trust.windows": new Set(jobs.map((job) => `${job.scope} ${job.proposalId}`)).size,
		},
		async (span) => {
			const evidence: ScopedTrustEvidence[] = [];
			const counts = { ran: 0, upheld: 0, cleared: 0, unverifiable: 0, skipped: 0 };
			let aborted = false;
			const { signal } = options;
			try {
				for (const job of jobs) {
					if (signal?.aborted) {
						aborted = true;
						break;
					}
					const verdicts = await adjudicateFailureClaims([job.record], [job.fingerprintId], {
						skillImports: job.skillImports,
						sysPath: options.sysPath ?? toolforgeSrcRoots(),
						...(options.pythonPath === undefined ? {} : { pythonPath: options.pythonPath }),
						...(options.timeoutMs === undefined ? {} : { timeoutMs: options.timeoutMs }),
						...(signal === undefined ? {} : { signal }),
					});
					if (signal?.aborted) {
						aborted = true;
						break;
					}
					counts.ran += 1;
					const status = verdicts.find((verdict) => verdict.fingerprintId === job.fingerprintId)?.status;
					if (status === "upheld" || status === "cleared" || status === "unverifiable") {
						counts[status] += 1;
						evidence.push({
							type: "adjudication",
							scope: job.scope,
							proposalId: job.proposalId,
							entry: job.entry,
							fingerprintId: job.fingerprintId,
							status,
							ordinal: job.ordinal,
							at: options.now?.() ?? new Date().toISOString(),
						});
					} else {
						counts.skipped += 1;
					}
				}
			} finally {
				span.setAttributes({
					"trust.ran": counts.ran,
					"trust.upheld": counts.upheld,
					"trust.cleared": counts.cleared,
					"trust.unverifiable": counts.unverifiable,
					"trust.skipped": counts.skipped,
					"trust.aborted": aborted,
				});
			}
			return evidence;
		},
	);
}
