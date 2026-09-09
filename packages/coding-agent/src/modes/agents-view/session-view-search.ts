import type { FuzzyMatch } from "@earendil-works/pi-tui";

export interface ParsedSearchQuery {
	mode: "tokens" | "regex";
	tokens: { kind: "fuzzy" | "phrase"; value: string }[];
	regex: RegExp | null;
	/** If set, parsing failed and we should treat query as non-matching. */
	error?: string;
}

export interface CompiledSearchQuery {
	mode: "tokens" | "regex";
	tokens: readonly { kind: "fuzzy" | "phrase"; value: string; lowerValue: string; normalizedValue: string }[];
	regex: RegExp | null;
	/** If set, parsing failed and we should treat query as non-matching. */
	error?: string;
}

export interface SearchTextCorpus {
	text: string;
	lowerText: string;
	normalizedText: string;
}

export interface MatchResult {
	matches: boolean;
	/** Lower is better; only meaningful when matches === true */
	score: number;
}

function normalizeWhitespaceLower(text: string): string {
	return text.toLowerCase().replace(/\s+/g, " ").trim();
}

/** Join arbitrary session fields into the common search corpus. */
export function createSessionSearchText(parts: readonly (string | undefined | null)[]): string {
	return parts.filter((part): part is string => typeof part === "string" && part.length > 0).join(" ");
}

export function parseSearchQuery(query: string): ParsedSearchQuery {
	const trimmed = query.trim();
	if (!trimmed) {
		return { mode: "tokens", tokens: [], regex: null };
	}

	// Regex mode: re:<pattern>
	if (trimmed.startsWith("re:")) {
		const pattern = trimmed.slice(3).trim();
		if (!pattern) {
			return { mode: "regex", tokens: [], regex: null, error: "Empty regex" };
		}
		try {
			return { mode: "regex", tokens: [], regex: new RegExp(pattern, "i") };
		} catch (err) {
			const msg = err instanceof Error ? err.message : String(err);
			return { mode: "regex", tokens: [], regex: null, error: msg };
		}
	}

	// Token mode with quote support.
	// Example: foo "node cve" bar
	const tokens: { kind: "fuzzy" | "phrase"; value: string }[] = [];
	let buf = "";
	let inQuote = false;
	let hadUnclosedQuote = false;

	const flush = (kind: "fuzzy" | "phrase"): void => {
		const v = buf.trim();
		buf = "";
		if (!v) return;
		tokens.push({ kind, value: v });
	};

	for (let i = 0; i < trimmed.length; i++) {
		const ch = trimmed[i]!;
		if (ch === '"') {
			if (inQuote) {
				flush("phrase");
				inQuote = false;
			} else {
				flush("fuzzy");
				inQuote = true;
			}
			continue;
		}

		if (!inQuote && /\s/.test(ch)) {
			flush("fuzzy");
			continue;
		}

		buf += ch;
	}

	if (inQuote) {
		hadUnclosedQuote = true;
	}

	// If quotes were unbalanced, fall back to plain whitespace tokenization.
	if (hadUnclosedQuote) {
		return {
			mode: "tokens",
			tokens: trimmed
				.split(/\s+/)
				.map((t) => t.trim())
				.filter((t) => t.length > 0)
				.map((t) => ({ kind: "fuzzy" as const, value: t })),
			regex: null,
		};
	}

	flush(inQuote ? "phrase" : "fuzzy");

	return { mode: "tokens", tokens, regex: null };
}

/** Parse and normalize a query once so it can be reused across a session rebuild. */
export function compileSearchQuery(query: string): CompiledSearchQuery {
	return compileParsedSearchQuery(parseSearchQuery(query));
}

function compileParsedSearchQuery(parsed: ParsedSearchQuery): CompiledSearchQuery {
	return {
		mode: parsed.mode,
		tokens: parsed.tokens.map((token) => ({
			...token,
			lowerValue: token.value.toLowerCase(),
			normalizedValue: normalizeWhitespaceLower(token.value),
		})),
		regex: parsed.regex,
		...(parsed.error ? { error: parsed.error } : {}),
	};
}

/** Prepare both corpus forms once before evaluating any query tokens. */
export function createSearchTextCorpus(text: string): SearchTextCorpus {
	const lowerText = text.toLowerCase();
	return { text, lowerText, normalizedText: lowerText.replace(/\s+/g, " ").trim() };
}

const STRICT_FUZZY_MAX_TOKEN_SCORE = 25;

/** Match a prepared corpus against a compiled query without further normalization. */
export function matchSearchTextCorpus(corpus: SearchTextCorpus, compiled: CompiledSearchQuery): MatchResult {
	if (compiled.error) return { matches: false, score: 0 };
	if (compiled.mode === "regex") {
		if (!compiled.regex) {
			return { matches: false, score: 0 };
		}
		const idx = corpus.text.search(compiled.regex);
		if (idx < 0) return { matches: false, score: 0 };
		return { matches: true, score: idx * 0.1 };
	}

	if (compiled.tokens.length === 0) {
		return { matches: true, score: 0 };
	}

	let totalScore = 0;
	for (const token of compiled.tokens) {
		const needle = token.normalizedValue;
		if (!needle) continue;
		const idx = corpus.normalizedText.indexOf(needle);
		if (idx >= 0) {
			totalScore += idx * 0.1;
			continue;
		}
		if (token.kind === "phrase") return { matches: false, score: 0 };
		const match = fuzzyMatchLower(token.lowerValue, corpus.lowerText);
		if (!match.matches || match.score > STRICT_FUZZY_MAX_TOKEN_SCORE) return { matches: false, score: 0 };
		totalScore += match.score;
	}

	return { matches: true, score: totalScore };
}

/** Match raw text with a compiled query, preparing the corpus exactly once. */
export function matchCompiledSearchText(text: string, compiled: CompiledSearchQuery): MatchResult {
	return matchSearchTextCorpus(createSearchTextCorpus(text), compiled);
}

/** Match any precomputed search corpus using the resume picker's query language. */
export function matchSearchText(text: string, parsed: ParsedSearchQuery): MatchResult {
	return matchCompiledSearchText(text, compileParsedSearchQuery(parsed));
}

/** Backwards-compatible convenience API for one-off searches. */
export function matchesSearchText(text: string, query: string): boolean {
	return matchCompiledSearchText(text, compileSearchQuery(query)).matches;
}

// Equivalent to pi-tui's fuzzyMatch after its lowercasing step. Accepting
// normalized inputs avoids lowercasing the same session corpus for every token.
function fuzzyMatchLower(queryLower: string, textLower: string): FuzzyMatch {
	const matchQuery = (normalizedQuery: string): FuzzyMatch => {
		if (normalizedQuery.length === 0) return { matches: true, score: 0 };
		if (normalizedQuery.length > textLower.length) return { matches: false, score: 0 };

		let queryIndex = 0;
		let score = 0;
		let lastMatchIndex = -1;
		let consecutiveMatches = 0;
		for (let i = 0; i < textLower.length && queryIndex < normalizedQuery.length; i++) {
			if (textLower[i] !== normalizedQuery[queryIndex]) continue;
			const isWordBoundary = i === 0 || /[\s\-_./:]/.test(textLower[i - 1]!);
			if (lastMatchIndex === i - 1) {
				consecutiveMatches++;
				score -= consecutiveMatches * 5;
			} else {
				consecutiveMatches = 0;
				if (lastMatchIndex >= 0) score += (i - lastMatchIndex - 1) * 2;
			}
			if (isWordBoundary) score -= 10;
			score += i * 0.1;
			lastMatchIndex = i;
			queryIndex++;
		}
		if (queryIndex < normalizedQuery.length) return { matches: false, score: 0 };
		if (normalizedQuery === textLower) score -= 100;
		return { matches: true, score };
	};

	const primaryMatch = matchQuery(queryLower);
	if (primaryMatch.matches) return primaryMatch;
	const alphaNumericMatch = queryLower.match(/^(?<letters>[a-z]+)(?<digits>[0-9]+)$/);
	const numericAlphaMatch = queryLower.match(/^(?<digits>[0-9]+)(?<letters>[a-z]+)$/);
	const swappedQuery = alphaNumericMatch
		? `${alphaNumericMatch.groups?.digits ?? ""}${alphaNumericMatch.groups?.letters ?? ""}`
		: numericAlphaMatch
			? `${numericAlphaMatch.groups?.letters ?? ""}${numericAlphaMatch.groups?.digits ?? ""}`
			: "";
	if (!swappedQuery) return primaryMatch;
	const swappedMatch = matchQuery(swappedQuery);
	return swappedMatch.matches ? { matches: true, score: swappedMatch.score + 5 } : primaryMatch;
}
