import type { RecallClaimVerdict } from "./claims.js";
import type { RecallWitnessReport } from "./witness.js";

export const RECALL_BLOCK_MAX_BYTES = 2048;
export const RECALL_MAX_LISTED_PATHS = 20;

const OPEN_TAG = "<workspace_recall>";
const CLOSE_TAG = "</workspace_recall>";

interface RenderLimits {
	listedPaths: number;
	pathChars: number;
	commandChars: number;
	listedClaims: number;
}

/**
 * Paths and commands come from the filesystem and the model; escape anything
 * that could end a line or the block early.
 */
function displayText(text: string, maxChars: number): string {
	const escaped = JSON.stringify(text).slice(1, -1).replace(/</g, "\\u003c").replace(/>/g, "\\u003e");
	if (escaped.length <= maxChars) return escaped;
	const keep = Math.max(1, maxChars - 3);
	const head = Math.ceil(keep / 2);
	return `${escaped.slice(0, head)}...${escaped.slice(escaped.length - (keep - head))}`;
}

function plural(count: number, noun: string): string {
	return `${count} ${noun}${count === 1 ? "" : "s"}`;
}

function shortHead(head: string | null): string {
	return head ? head.slice(0, 12) : "no commit";
}

function pathList(label: string, paths: readonly string[], limits: RenderLimits): string[] {
	if (paths.length === 0) return [`${label}: none.`];
	const shown = paths.slice(0, limits.listedPaths);
	const lines = [`${label} (${paths.length}):`, ...shown.map((path) => `  ${displayText(path, limits.pathChars)}`)];
	if (paths.length > shown.length) lines.push(`  +${paths.length - shown.length} more`);
	return lines;
}

function claimLine(verdict: RecallClaimVerdict, limits: RenderLimits): string {
	const command = displayText(verdict.claim.command, limits.commandChars);
	const outcome = `\`${command}\` exited ${verdict.claim.exitCode} at ${displayText(verdict.claim.at, 40)}`;
	return verdict.status === "CURRENT"
		? `- CURRENT: ${outcome}`
		: `- EXPIRED (${displayText(verdict.reason ?? "workspace changed", limits.pathChars)}): ${outcome}`;
}

function renderWithLimits(report: RecallWitnessReport, limits: RenderLimits): string {
	const lines = [
		OPEN_TAG,
		`Recomputed against the workspace mark written ${displayText(report.markWrittenAt, 40)}. Digests only: nothing below is a cached answer.`,
		report.headMoved
			? `HEAD moved: ${shortHead(report.markHead)} -> ${shortHead(report.head)}.`
			: `HEAD unchanged at ${shortHead(report.head)}.`,
	];
	if (report.trackedTreeChanged) lines.push("The index (tracked tree) changed since the mark.");
	if (report.changedUnknownReason === undefined) {
		if (report.changed.length === 0 && report.uncomparedCount > 0) {
			lines.push(`Changed: none detected (${plural(report.uncomparedCount, "path")} could not be compared).`);
		} else {
			lines.push(...pathList("Changed", report.changed, limits));
		}
	} else {
		lines.push(`Changed: unknown — ${displayText(report.changedUnknownReason, limits.pathChars)}.`);
		if (report.changed.length > 0) lines.push(...pathList("Changed among compared paths", report.changed, limits));
	}
	lines.push(
		report.unchangedCount === undefined
			? `Unchanged since the mark: not reported, because ${report.unchangedUnknownReason ?? "the comparison is incomplete"}.`
			: `${report.unchangedCount} unchanged since the mark (unverifiable paths are never counted).`,
	);
	const unrecorded = Math.max(0, report.uncomparedCount - report.unverifiable.length - report.unhashedTagged);
	const unrecordedUpTo = report.unrecordedUpTo ?? 0;
	if (report.unverifiable.length > 0 || (report.uncomparedCount === 0 && unrecordedUpTo === 0)) {
		lines.push(...pathList("Unverifiable", report.unverifiable, limits));
	}
	if (report.unhashedTagged > 0) {
		lines.push(
			`Unverifiable, not listed: ${plural(report.unhashedTagged, "skip-worktree or assume-unchanged path")}, too many to hash or list.`,
		);
	}
	if (unrecorded > 0 && unrecordedUpTo > 0) {
		lines.push(
			`Unverifiable, not listed: ${plural(unrecorded, "path")} the mark left unrecorded (up to ${unrecordedUpTo}; some may be among the listed paths).`,
		);
	} else if (unrecorded > 0) {
		lines.push(`Unverifiable, not listed: ${plural(unrecorded, "path")} the mark left unrecorded.`);
	} else if (unrecordedUpTo > 0) {
		lines.push(
			`Unverifiable, not listed: up to ${plural(unrecordedUpTo, "path")} the mark left unrecorded (some may be among the listed paths).`,
		);
	}
	if (report.claims.length > 0) {
		lines.push("Build claims (re-run an EXPIRED one before relying on it):");
		const newestFirst = [...report.claims].reverse();
		const shown = newestFirst.slice(0, limits.listedClaims);
		lines.push(...shown.map((verdict) => claimLine(verdict, limits)));
		if (newestFirst.length > shown.length) lines.push(`- +${newestFirst.length - shown.length} more`);
	}
	lines.push(CLOSE_TAG);
	return lines.join("\n");
}

function byteLength(text: string): number {
	return Buffer.byteLength(text, "utf8");
}

/** The `<workspace_recall>` block for a witness report, never longer than RECALL_BLOCK_MAX_BYTES. */
export function renderRecallBlock(report: RecallWitnessReport, maxBytes: number = RECALL_BLOCK_MAX_BYTES): string {
	const limits: RenderLimits = {
		listedPaths: RECALL_MAX_LISTED_PATHS,
		pathChars: 160,
		commandChars: 160,
		listedClaims: 8,
	};
	for (let attempt = 0; attempt < 24; attempt++) {
		const text = renderWithLimits(report, limits);
		if (byteLength(text) <= maxBytes) return text;
		if (limits.listedPaths > 2) limits.listedPaths = Math.floor(limits.listedPaths / 2);
		else if (limits.pathChars > 40) limits.pathChars = Math.floor(limits.pathChars / 2);
		else if (limits.commandChars > 40) limits.commandChars = Math.floor(limits.commandChars / 2);
		else if (limits.listedClaims > 1) limits.listedClaims = Math.floor(limits.listedClaims / 2);
		else if (limits.listedPaths > 0) limits.listedPaths = 0;
		else break;
	}
	const minimal = [
		OPEN_TAG,
		`Recomputed against the workspace mark written ${displayText(report.markWrittenAt, 40)}.`,
		report.headMoved ? "HEAD moved." : "HEAD unchanged.",
		`Changed: ${report.changedUnknownReason === undefined ? report.changed.length : "unknown"}. Unverifiable: ${report.uncomparedCount}${report.unrecordedUpTo ? ` (up to ${report.unrecordedUpTo} unrecorded)` : ""}. Unchanged: ${report.unchangedCount ?? "not reported"}.`,
		`Build claims: ${report.claims.filter((verdict) => verdict.status === "CURRENT").length} CURRENT, ${report.claims.filter((verdict) => verdict.status === "EXPIRED").length} EXPIRED.`,
		CLOSE_TAG,
	].join("\n");
	return byteLength(minimal) <= maxBytes ? minimal : `${OPEN_TAG}\n${CLOSE_TAG}`;
}
