import type { RecallClaimVerdict } from "./claims.js";
import {
	type AbsentSkipWorktree,
	captureWorkspace,
	committedChangesBetween,
	digestRecallPath,
	isFullyVerifiable,
	isRecallExcluded,
	pathsMatchingCommit,
	RECALL_ABSENT_DIGEST,
	RECALL_MAX_HASHED_BYTES,
	RECALL_UNVERIFIABLE,
	type RecallCaptureFailure,
	type WorkspaceSnapshot,
	workspaceDigest,
} from "./mark.js";
import { markState, type RecallMarkFile, recallRepoKey } from "./store.js";

/**
 * The witness: every digest in a prior mark recomputed against the live
 * filesystem. A path is reported unchanged only when both sides were hashed
 * and agree, when it is absent on both sides, or when a path present at the
 * mark and newly tagged is still the blob at an unmoved HEAD; anything that
 * could not be compared is unverifiable and is never counted as unchanged.
 * An absent skip-worktree entry compares as a deletion, so a path a sparse
 * checkout removes or restores is changed.
 */

export const RECALL_COMMITS_UNLISTED = "commits between the mark and HEAD could not be listed";
export const RECALL_MARK_PREDATES_PRESENCE = "the mark predates recording which skip-worktree paths are absent";
export const RECALL_PRESENCE_CHANGED = "sparse checkout changed: skip-worktree paths appeared or disappeared";

export interface RecallWitnessReport {
	repoRoot: string;
	markWrittenAt: string;
	markHead: string | null;
	head: string | null;
	headMoved: boolean;
	trackedTreeChanged: boolean;
	/** Sorted. Only the paths that could be compared when `changedUnknownReason` is set. */
	changed: string[];
	/** Set when the full set of changed paths cannot be established. */
	changedUnknownReason?: string;
	/** Sorted. */
	unverifiable: string[];
	/** Skip-worktree and assume-unchanged paths too many to hash or list now; counted, never listed. */
	unhashedTagged: number;
	/**
	 * Every path that could not be compared, listed or not: `unverifiable`,
	 * `unhashedTagged`, and as many paths the mark left unrecorded as the paths
	 * reported without a mark digest leave over.
	 */
	uncomparedCount: number;
	/**
	 * Set when reported paths without a mark digest were taken off a partial
	 * mark's unrecorded count. Such a path may be new rather than unrecorded, so
	 * the unrecorded remainder in `uncomparedCount` is only a lower bound: up to
	 * this many (the mark's whole unrecorded count) may be reported nowhere.
	 */
	unrecordedUpTo?: number;
	/** Undefined when the set of unchanged paths cannot be established; see `unchangedUnknownReason`. */
	unchangedCount: number | undefined;
	unchangedUnknownReason?: string;
	claims: RecallClaimVerdict[];
}

export interface CompareWorkspaceInputs {
	mark: RecallMarkFile;
	snapshot: WorkspaceSnapshot;
	/** Paths changed between the mark's HEAD and the current HEAD; undefined with a moved HEAD means git could not list them. */
	committedChanges?: ReadonlySet<string>;
	/** Current digests for paths dirty or absent at the mark that git status no longer reports and that are not absent now. */
	cleanNowDigests: ReadonlyMap<string, string>;
	/**
	 * Tagged paths with no digest in the mark whose working content is the blob at an unmoved HEAD. Only a path
	 * the mark shows was on disk, not listed absent and not under a mark that recorded no absence, is unchanged by it.
	 */
	taggedMatchingHead?: ReadonlySet<string>;
}

type PathClass = "changed" | "unchanged" | "unverifiable";

function classifyPath(
	markDigest: string | undefined,
	currentDigest: string | undefined,
	cleanNowDigest: string | undefined,
	markMayOmitPath: boolean,
	matchesHead: boolean,
): PathClass {
	if (markDigest === RECALL_UNVERIFIABLE || currentDigest === RECALL_UNVERIFIABLE) return "unverifiable";
	if (markDigest !== undefined && currentDigest !== undefined) {
		return markDigest === currentDigest ? "unchanged" : "changed";
	}
	if (markDigest !== undefined) {
		if (cleanNowDigest === undefined || cleanNowDigest === RECALL_UNVERIFIABLE) return "unverifiable";
		return cleanNowDigest === markDigest ? "unchanged" : "changed";
	}
	// Past the mark's recorded window, or possibly absent at a mark that did not record absence: nothing to compare with.
	if (markMayOmitPath) return "unverifiable";
	// Clean at the mark and tagged since, with the content HEAD still holds: only the index bits moved.
	if (matchesHead) return "unchanged";
	// Clean at the mark and dirty or absent now, or committed between the two HEADs: the content moved.
	return "changed";
}

function compareStrings(a: string, b: string): number {
	return a < b ? -1 : a > b ? 1 : 0;
}

function sameAbsence(mark: AbsentSkipWorktree | null, now: AbsentSkipWorktree | null): boolean {
	if (mark === null || now === null) return mark === now;
	return mark.count === now.count && mark.digest === now.digest;
}

function formatClaimPaths(paths: readonly string[]): string {
	const shown = paths.slice(0, 3).join(", ");
	return paths.length > 3 ? `${shown} +${paths.length - 3} more` : shown;
}

function cannotVerify(count: number): string {
	return `cannot verify: ${count} unverifiable path${count === 1 ? "" : "s"}`;
}

export function compareWorkspace(inputs: CompareWorkspaceInputs): RecallWitnessReport {
	const { mark, snapshot, cleanNowDigests } = inputs;
	const taggedMatchingHead = inputs.taggedMatchingHead ?? new Set<string>();
	const excluded = snapshot.excludedPath;
	const markDirty = new Map(Object.entries(mark.dirty).filter(([path]) => !isRecallExcluded(path, excluded)));
	const committedChanges =
		inputs.committedChanges &&
		new Set([...inputs.committedChanges].filter((path) => !isRecallExcluded(path, excluded)));
	const absentAtMark = new Set(
		(mark.absentSkipWorktree?.paths ?? []).filter((path) => !isRecallExcluded(path, excluded)),
	);
	const absentNow = new Set(snapshot.absentSkipWorktree?.paths ?? []);
	const markDigestOf = (path: string) =>
		markDirty.get(path) ?? (absentAtMark.has(path) ? RECALL_ABSENT_DIGEST : undefined);
	const headMoved = mark.head !== snapshot.head;
	const trackedTreeChanged = mark.trackedTreeDigest !== snapshot.trackedTreeDigest;
	const commitsUnlisted = headMoved && committedChanges === undefined;
	const markPartial = mark.dirtyOverflow > 0;
	const presenceUnknown = mark.absentSkipWorktree === undefined;
	const presenceChanged =
		!presenceUnknown && !sameAbsence(mark.absentSkipWorktree ?? null, snapshot.absentSkipWorktree);
	const overflowNow = new Set(snapshot.overflowPaths);

	const candidates = new Set<string>([
		...markDirty.keys(),
		...absentAtMark,
		...snapshot.dirty.keys(),
		...absentNow,
		...(committedChanges ?? []),
	]);
	const changed: string[] = [];
	const unverifiable = new Set<string>(overflowNow);
	for (const path of candidates) {
		if (overflowNow.has(path)) continue;
		const committed = committedChanges?.has(path) ?? false;
		// A mark that did not record absence cannot say whether a tagged or absent path was on disk then.
		const presenceAtMarkUnknown = presenceUnknown && (snapshot.taggedPaths.has(path) || absentNow.has(path));
		const pathClass = classifyPath(
			markDigestOf(path),
			snapshot.dirty.get(path) ?? (absentNow.has(path) ? RECALL_ABSENT_DIGEST : undefined),
			cleanNowDigests.get(path),
			!committed && (markPartial || presenceAtMarkUnknown),
			taggedMatchingHead.has(path),
		);
		if (pathClass === "changed") changed.push(path);
		else if (pathClass === "unverifiable") unverifiable.add(path);
	}
	const unhashedTagged = snapshot.unhashedTaggedPaths.filter((path) => !candidates.has(path)).length;
	// The mark's unrecorded paths are unnamed; count only as many as the paths already reported without a mark digest leave over.
	let unverifiableWithoutMarkDigest = 0;
	for (const path of unverifiable) {
		if (markDigestOf(path) === undefined) unverifiableWithoutMarkDigest++;
	}
	let changedWithoutMarkDigest = 0;
	for (const path of changed) {
		if (markDigestOf(path) === undefined) changedWithoutMarkDigest++;
	}
	const unaccounted = markPartial ? mark.dirtyOverflow - unhashedTagged - unverifiableWithoutMarkDigest : 0;
	const markUnrecorded = Math.max(0, unaccounted - changedWithoutMarkDigest);
	const unrecordedUpTo = markPartial && markUnrecorded < mark.dirtyOverflow ? mark.dirtyOverflow : 0;
	const uncomparedCount = unverifiable.size + unhashedTagged + markUnrecorded;

	let unchangedCount: number | undefined;
	let unchangedUnknownReason: string | undefined;
	if (commitsUnlisted) {
		unchangedUnknownReason = "HEAD moved and the commits in between could not be listed";
	} else if (markPartial) {
		unchangedUnknownReason = `the mark left ${mark.dirtyOverflow} path${mark.dirtyOverflow === 1 ? "" : "s"} unrecorded`;
	} else if (presenceUnknown) {
		unchangedUnknownReason = RECALL_MARK_PREDATES_PRESENCE;
	} else {
		const universe = new Set<string>([...snapshot.trackedPaths, ...candidates, ...overflowNow]);
		unchangedCount = universe.size - changed.length - unverifiable.size - unhashedTagged;
	}

	const sortedChanged = changed.sort(compareStrings);
	const sortedUnverifiable = [...unverifiable].sort(compareStrings);
	const currentDigest = workspaceDigest(snapshot);
	const currentVerifiable = isFullyVerifiable(snapshot);
	const markDigest = workspaceDigest(markState(mark));
	const retaggedDigest =
		taggedMatchingHead.size > 0
			? workspaceDigest({
					...snapshot,
					dirty: new Map([...snapshot.dirty].filter(([path]) => !taggedMatchingHead.has(path))),
				})
			: undefined;
	const claims: RecallClaimVerdict[] = mark.claims.map((claim) => {
		// The claim's own digest covers the whole workspace; what the mark failed to record cannot expire an exact match.
		if (claim.digestAtClaim === currentDigest) {
			if (currentVerifiable) return { claim, status: "CURRENT" };
			return { claim, status: "EXPIRED", reason: cannotVerify(unverifiableInSnapshot(snapshot)) };
		}
		let reason: string;
		if (claim.digestAtClaim !== markDigest) {
			reason = "workspace changed after the claim, before the mark";
		} else if (headMoved) {
			reason = commitsUnlisted ? `HEAD moved; ${RECALL_COMMITS_UNLISTED}` : "HEAD moved";
		} else if (trackedTreeChanged) {
			reason = "tracked tree changed";
		} else if (presenceChanged) {
			reason = RECALL_PRESENCE_CHANGED;
		} else if (sortedChanged.length > 0) {
			reason = `changed paths: ${formatClaimPaths(sortedChanged)}`;
		} else if (claim.digestAtClaim === retaggedDigest) {
			reason = "skip-worktree or assume-unchanged bits changed";
		} else if (presenceUnknown) {
			reason = `cannot verify: ${RECALL_MARK_PREDATES_PRESENCE}`;
		} else if (uncomparedCount > 0) {
			reason = cannotVerify(uncomparedCount);
		} else {
			reason = "workspace digest changed";
		}
		return { claim, status: "EXPIRED", reason };
	});

	return {
		repoRoot: snapshot.repoRoot,
		markWrittenAt: mark.writtenAt,
		markHead: mark.head,
		head: snapshot.head,
		headMoved,
		trackedTreeChanged,
		changed: sortedChanged,
		...(commitsUnlisted
			? { changedUnknownReason: RECALL_COMMITS_UNLISTED }
			: presenceUnknown
				? { changedUnknownReason: RECALL_MARK_PREDATES_PRESENCE }
				: {}),
		unverifiable: sortedUnverifiable,
		unhashedTagged,
		uncomparedCount,
		...(unrecordedUpTo > 0 ? { unrecordedUpTo } : {}),
		unchangedCount,
		...(unchangedUnknownReason ? { unchangedUnknownReason } : {}),
		claims,
	};
}

function unverifiableInSnapshot(snapshot: WorkspaceSnapshot): number {
	let count = snapshot.dirtyOverflow;
	for (const digest of snapshot.dirty.values()) {
		if (digest === RECALL_UNVERIFIABLE) count++;
	}
	return count;
}

export interface WitnessWorkspaceOptions {
	agentDir?: string;
	/** Aborting kills outstanding git processes and stops hashing. */
	signal?: AbortSignal;
}

export type WitnessWorkspaceResult =
	| { ok: true; report: RecallWitnessReport; repoKey: string }
	| { ok: false; failure: RecallCaptureFailure };

/** Recompute `mark` against the live workspace. Fails when git cannot describe the workspace now. */
export async function witnessWorkspace(
	repoRoot: string,
	mark: RecallMarkFile,
	options: WitnessWorkspaceOptions = {},
): Promise<WitnessWorkspaceResult> {
	const { agentDir, signal } = options;
	const captured = await captureWorkspace(repoRoot, { agentDir, signal });
	if (!captured.ok) return captured;
	const { snapshot } = captured;
	let committedChanges: Set<string> | undefined;
	if (mark.head !== snapshot.head && mark.head !== null && snapshot.head !== null) {
		committedChanges = await committedChangesBetween(repoRoot, mark.head, snapshot.head, {
			signal,
			excludedPath: snapshot.excludedPath,
		});
	}
	const cleanNowDigests = new Map<string, string>();
	const overflowNow = new Set(snapshot.overflowPaths);
	const absentNow = new Set(snapshot.absentSkipWorktree?.paths ?? []);
	const absentAtMark = mark.absentSkipWorktree?.paths;
	const comparedAtMark = [
		...Object.entries(mark.dirty)
			.filter(([, digest]) => digest !== RECALL_UNVERIFIABLE)
			.map(([path]) => path),
		...(absentAtMark ?? []),
	];
	const budget = { remainingBytes: RECALL_MAX_HASHED_BYTES };
	for (const path of new Set(comparedAtMark)) {
		if (snapshot.dirty.has(path) || absentNow.has(path) || overflowNow.has(path)) continue;
		if (isRecallExcluded(path, snapshot.excludedPath)) continue;
		if (signal?.aborted) return { ok: false, failure: "aborted" };
		cleanNowDigests.set(path, await digestRecallPath(repoRoot, path, true, budget));
	}
	if (signal?.aborted) return { ok: false, failure: "aborted" };
	let taggedMatchingHead: Set<string> | undefined;
	// A tagged path the mark neither has a digest for nor lists as absent was on disk and clean then, so with
	// HEAD unmoved it is unchanged iff it is still HEAD's blob. compareWorkspace gives a path the mark lists
	// as absent the absent digest, and a mark that recorded no absence no inference, so neither reaches this.
	if (snapshot.head !== null && mark.head === snapshot.head && mark.dirtyOverflow === 0) {
		const retagged = [...snapshot.dirty]
			.filter(
				([path, digest]) =>
					snapshot.taggedPaths.has(path) &&
					digest !== RECALL_UNVERIFIABLE &&
					digest !== RECALL_ABSENT_DIGEST &&
					!Object.hasOwn(mark.dirty, path),
			)
			.map(([path]) => path);
		if (retagged.length > 0)
			taggedMatchingHead = await pathsMatchingCommit(repoRoot, snapshot.head, retagged, signal);
	}
	if (signal?.aborted) return { ok: false, failure: "aborted" };
	const report = compareWorkspace({ mark, snapshot, committedChanges, cleanNowDigests, taggedMatchingHead });
	return { ok: true, report, repoKey: recallRepoKey(repoRoot, agentDir) };
}
