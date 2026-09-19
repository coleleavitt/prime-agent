/**
 * Build claims: "this command exited 0 on exactly this workspace". A claim is
 * never served as an answer. It is rendered CURRENT only while the workspace
 * digest it was recorded against still matches a fresh recomputation, and
 * EXPIRED with a reason otherwise.
 */

export const RECALL_MAX_CLAIMS = 8;
export const RECALL_MAX_CLAIM_COMMAND_CHARS = 500;

export interface RecallClaim {
	command: string;
	exitCode: number;
	/** ISO timestamp the claim was recorded. */
	at: string;
	/** workspaceDigest() of the workspace the command ran against. */
	digestAtClaim: string;
}

export type RecallClaimStatus = "CURRENT" | "EXPIRED";

export interface RecallClaimVerdict {
	claim: RecallClaim;
	status: RecallClaimStatus;
	reason?: string;
}

const BUILD_SEGMENT_PATTERNS: readonly RegExp[] = [
	/^(?:(?:npx|pnpm\s+exec|pnpm\s+dlx|yarn|bunx)\s+)?(?:tsgo|tsc|vue-tsc)(?:\s|$)/,
	/^(?:(?:npx|pnpm\s+exec|pnpm\s+dlx|yarn|bunx)\s+)?(?:@biomejs\/)?biome\s+(?:check|ci|lint)(?:\s|$)/,
	/^cargo\s+(?:\+\S+\s+)?(?:build|test|check|clippy|nextest\s+run)(?:\s|$)/,
	/^(?:npm|pnpm|yarn|bun)\s+(?:test|t)(?:\s|$)/,
	/^(?:npm|pnpm|yarn|bun)\s+run\s+(?:build|check|test|typecheck|lint)(?:[:\s]|$)/,
	/^(?:pnpm|yarn)\s+(?:build|check|typecheck)(?:\s|$)/,
	/^(?:(?:python3?|uv\s+run|poetry\s+run)\s+(?:-m\s+)?)?pytest(?:\s|$)/,
	/^go\s+(?:build|test|vet)(?:\s|$)/,
	/^make(?:\s|$)/,
];

const BUILD_COMMAND_MENTION =
	/\b(?:tsgo|tsc|pytest|make)\b|\bcargo\s+(?:\+\S+\s+)?(?:build|test|check|clippy|nextest)\b|\b(?:npm|pnpm|yarn|bun)\s+(?:test|t|run\s+(?:build|check|test|typecheck|lint))\b|\b(?:pnpm|yarn)\s+(?:build|check|typecheck)\b|\bgo\s+(?:build|test|vet)\b|\bbiome\s+(?:check|ci|lint)\b/;

const ENV_ASSIGNMENT = /^[A-Za-z_][A-Za-z0-9_]*=\S*$/;

/**
 * True when cell source names a build or test command anywhere. A cheap
 * pre-filter: only such cells pay for a workspace digest before they run.
 */
export function mentionsBuildCommand(source: string): boolean {
	return BUILD_COMMAND_MENTION.test(source);
}

function stripEnvAssignments(segment: string): string {
	const words = segment.split(/\s+/);
	let index = 0;
	while (index < words.length && ENV_ASSIGNMENT.test(words[index]!)) index++;
	return words.slice(index).join(" ");
}

/**
 * True for a command whose exit code 0 is evidence the build or tests passed.
 * `&&` chains of build commands and `cd` qualify; pipes, `||`, `;`, background
 * jobs, and substitutions do not, because each can report 0 when the build failed.
 */
export function isBuildClaimCommand(command: string): boolean {
	const trimmed = command.trim();
	if (!trimmed || trimmed.length > RECALL_MAX_CLAIM_COMMAND_CHARS) return false;
	if (/[\n\r;`]|\$\(|\|\||(?<![&>])&(?![&>])|\|/.test(trimmed)) return false;
	let sawBuild = false;
	for (const rawSegment of trimmed.split("&&")) {
		const segment = stripEnvAssignments(rawSegment.trim().replace(/\s+(?:\d?>>?|&>)\s*\S+/g, ""));
		if (!segment) return false;
		if (/^cd\s+\S+$/.test(segment)) continue;
		if (!BUILD_SEGMENT_PATTERNS.some((pattern) => pattern.test(segment))) return false;
		sawBuild = true;
	}
	return sawBuild;
}

function claimTime(claim: RecallClaim): number {
	const time = Date.parse(claim.at);
	return Number.isFinite(time) ? time : 0;
}

/** Existing claims plus new ones, one per command (the newest wins), bounded to the RECALL_MAX_CLAIMS newest. */
export function mergeRecallClaims(existing: readonly RecallClaim[], added: readonly RecallClaim[]): RecallClaim[] {
	const byCommand = new Map<string, RecallClaim>();
	for (const claim of [...existing, ...added]) {
		const held = byCommand.get(claim.command);
		if (!held || claimTime(claim) >= claimTime(held)) byCommand.set(claim.command, claim);
	}
	return [...byCommand.values()].sort((a, b) => claimTime(a) - claimTime(b)).slice(-RECALL_MAX_CLAIMS);
}

export function isRecallClaim(value: unknown): value is RecallClaim {
	if (typeof value !== "object" || value === null) return false;
	const claim = value as Partial<RecallClaim>;
	return (
		typeof claim.command === "string" &&
		typeof claim.exitCode === "number" &&
		Number.isInteger(claim.exitCode) &&
		typeof claim.at === "string" &&
		typeof claim.digestAtClaim === "string"
	);
}
