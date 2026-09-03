import { createHash } from "node:crypto";

export type ContextAtomKind = "current_task" | "champion" | "constraint" | "archive" | "artifact" | "lineage";

export interface DurableArtifactRef {
	/** Stable, durable identifier. This is not a URL containing credentials. */
	id: string;
	uri: string;
	mediaType?: string;
	byteLength: number;
	sha256: string;
	redactions?: readonly string[];
}

export interface LineageSummary {
	id: string;
	summary: string;
	parent?: LineageSummary;
}

export interface ContextAtom {
	id: string;
	kind: ContextAtomKind;
	text?: string;
	artifact?: DurableArtifactRef;
	lineage?: LineageSummary;
	priority?: number;
	createdAt?: number;
	redactions?: readonly string[];
}

export interface ContextArchive {
	currentTask: ContextAtom;
	champion?: ContextAtom;
	constraints?: readonly ContextAtom[];
	items?: readonly ContextAtom[];
}

export interface ContextViewLimits {
	maxTokens: number;
	maxBytes: number;
	maxItems: number;
	lineageDepth: number;
	maxArtifactBytesPerItem: number;
}

export type ContextOmissionReason =
	| "max_items"
	| "max_bytes"
	| "max_tokens"
	| "artifact_too_large"
	| "lineage_depth"
	| "invalid_atom";

export interface ContextViewItem {
	id: string;
	kind: ContextAtomKind;
	content?: string;
	artifact?: DurableArtifactRef;
	lineage?: LineageSummary;
	redactions: readonly string[];
	truncated: boolean;
	reasons: readonly ContextOmissionReason[];
	sha256: string;
	bytes: number;
	tokens: number;
}

export interface ContextOmission {
	reason: ContextOmissionReason;
	count: number;
}

export interface BoundedContextView {
	items: readonly ContextViewItem[];
	omissions: readonly ContextOmission[];
	usage: { bytes: number; tokens: number; items: number };
	limits: ContextViewLimits;
	sha256: string;
}

export interface ContextViewOptions {
	/** Defaults to one token per UTF-8 byte: conservative for normal text tokenizers. */
	countTokens?: (text: string) => number;
}

const utf8 = new TextEncoder();
const bytes = (value: string): number => utf8.encode(value).byteLength;
const hash = (value: string): string => createHash("sha256").update(value).digest("hex");

function validateLimits(limits: ContextViewLimits): void {
	for (const [name, value] of Object.entries(limits)) {
		if (!Number.isSafeInteger(value) || value < 0)
			throw new RangeError(`${name} must be a non-negative safe integer`);
	}
}

/** Slice on Unicode code-point boundaries and never exceed a UTF-8 byte budget. */
export function truncateUtf8(value: string, maxBytes: number): { value: string; truncated: boolean } {
	if (bytes(value) <= maxBytes) return { value, truncated: false };
	let result = "";
	let used = 0;
	for (const point of value) {
		const size = bytes(point);
		if (used + size > maxBytes) break;
		result += point;
		used += size;
	}
	return { value: result, truncated: true };
}

/** Encode a lineage as a bounded summary, replacing the unvisited tail with an explicit marker. */
export function encodeLineage(
	lineage: LineageSummary,
	depth: number,
	maxFieldBytes = Number.MAX_SAFE_INTEGER,
): { lineage?: LineageSummary; omitted: boolean } {
	if (depth <= 0) return { omitted: true };
	const encoded: LineageSummary = {
		id: truncateUtf8(lineage.id, maxFieldBytes).value,
		summary: truncateUtf8(lineage.summary, maxFieldBytes).value,
	};
	if (!lineage.parent) return { lineage: encoded, omitted: false };
	const parent = encodeLineage(lineage.parent, depth - 1, maxFieldBytes);
	if (parent.lineage) encoded.parent = parent.lineage;
	return { lineage: encoded, omitted: parent.omitted };
}

function canonical(value: unknown): string {
	if (Array.isArray(value)) return `[${value.map(canonical).join(",")}]`;
	if (value && typeof value === "object") {
		return `{${Object.entries(value as Record<string, unknown>)
			.filter(([, item]) => item !== undefined)
			.sort(([a], [b]) => a.localeCompare(b))
			.map(([key, item]) => `${JSON.stringify(key)}:${canonical(item)}`)
			.join(",")}}`;
	}
	return JSON.stringify(value);
}

function orderedAtoms(archive: ContextArchive): ContextAtom[] {
	const required = [archive.currentTask, archive.champion, ...(archive.constraints ?? [])].filter(
		(atom): atom is ContextAtom => atom !== undefined,
	);
	const optional = [...(archive.items ?? [])].sort(
		(a, b) =>
			(b.priority ?? 0) - (a.priority ?? 0) || (b.createdAt ?? 0) - (a.createdAt ?? 0) || a.id.localeCompare(b.id),
	);
	return [...required, ...optional];
}

/**
 * Construct a deterministic child view. The archive may grow without changing
 * any configured bound. Large artifacts remain opaque durable references.
 */
export function buildBoundedContextView(
	archive: ContextArchive,
	limits: ContextViewLimits,
	options: ContextViewOptions = {},
): BoundedContextView {
	validateLimits(limits);
	const countTokens = options.countTokens ?? ((text: string) => bytes(text));
	const items: ContextViewItem[] = [];
	const omissionCounts = new Map<ContextOmissionReason, number>();
	const omit = (reason: ContextOmissionReason): void => {
		omissionCounts.set(reason, (omissionCounts.get(reason) ?? 0) + 1);
	};
	let usedBytes = 0;
	let usedTokens = 0;

	for (const atom of orderedAtoms(archive)) {
		if (!atom.id || (!atom.text && !atom.artifact && !atom.lineage)) {
			omit("invalid_atom");
			continue;
		}
		if (items.length >= limits.maxItems) {
			omit("max_items");
			continue;
		}

		const remainingBytes = limits.maxBytes - usedBytes;
		const remainingTokens = limits.maxTokens - usedTokens;
		if (remainingBytes <= 0 || remainingTokens <= 0) {
			omit(remainingBytes <= 0 ? "max_bytes" : "max_tokens");
			continue;
		}

		const reasons: ContextOmissionReason[] = [];
		let content = atom.text;
		const artifact = atom.artifact;
		if (artifact && artifact.byteLength > limits.maxArtifactBytesPerItem) {
			// The reference is retained. Its target is deliberately never dereferenced or inlined.
			reasons.push("artifact_too_large");
		}
		const lineageResult = atom.lineage
			? encodeLineage(atom.lineage, limits.lineageDepth, limits.maxArtifactBytesPerItem)
			: undefined;
		if (lineageResult?.omitted) reasons.push("lineage_depth");

		if (content) {
			const byteCut = truncateUtf8(content, Math.min(remainingBytes, limits.maxArtifactBytesPerItem));
			content = byteCut.value;
			if (byteCut.truncated)
				reasons.push(remainingBytes <= limits.maxArtifactBytesPerItem ? "max_bytes" : "artifact_too_large");
			while (content && countTokens(content) > remainingTokens) {
				content = truncateUtf8(content, Math.max(0, bytes(content) - 1)).value;
				if (!reasons.includes("max_tokens")) reasons.push("max_tokens");
			}
		}

		// Accounting covers inline text. Opaque refs and lineage are metadata, not archive payload.
		const itemBytes = content ? bytes(content) : 0;
		const itemTokens = content ? countTokens(content) : 0;
		if (itemBytes > remainingBytes || itemTokens > remainingTokens) {
			omit(itemBytes > remainingBytes ? "max_bytes" : "max_tokens");
			continue;
		}
		const boundedArtifact = artifact
			? {
					...artifact,
					id: truncateUtf8(artifact.id, limits.maxArtifactBytesPerItem).value,
					uri: truncateUtf8(artifact.uri, limits.maxArtifactBytesPerItem).value,
					mediaType: artifact.mediaType
						? truncateUtf8(artifact.mediaType, limits.maxArtifactBytesPerItem).value
						: undefined,
					redactions: artifact.redactions
						?.slice(0, limits.maxItems)
						.map((value) => truncateUtf8(value, limits.maxArtifactBytesPerItem).value),
				}
			: undefined;
		const base = {
			id: truncateUtf8(atom.id, limits.maxArtifactBytesPerItem).value,
			kind: atom.kind,
			content: content || undefined,
			artifact: boundedArtifact,
			lineage: lineageResult?.lineage,
			redactions: [...(atom.redactions ?? []), ...(artifact?.redactions ?? [])]
				.slice(0, limits.maxItems)
				.map((value) => truncateUtf8(value, limits.maxArtifactBytesPerItem).value),
			truncated: reasons.length > 0,
			reasons,
			bytes: itemBytes,
			tokens: itemTokens,
		};
		items.push({ ...base, sha256: hash(canonical(base)) });
		usedBytes += itemBytes;
		usedTokens += itemTokens;
	}

	const omissions = [...omissionCounts.entries()]
		.sort(([a], [b]) => a.localeCompare(b))
		.map(([reason, count]) => ({ reason, count }));
	const body = { items, omissions, usage: { bytes: usedBytes, tokens: usedTokens, items: items.length }, limits };
	return { ...body, sha256: hash(canonical(body)) };
}
