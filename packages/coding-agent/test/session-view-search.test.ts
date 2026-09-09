import { fuzzyMatch } from "@earendil-works/pi-tui";
import { describe, expect, it } from "vitest";
import {
	filterUnifiedSessionsBySearchQuery,
	type UnifiedSessionRecord,
} from "../src/modes/agents-view/agents-view-state.js";
import {
	compileSearchQuery,
	createSessionSearchText,
	matchCompiledSearchText,
	matchesSearchText,
} from "../src/modes/agents-view/session-view-search.js";

describe("session view search", () => {
	it("matches fuzzy tokens, normalized phrases, and case-insensitive regexes", () => {
		const text = createSessionSearchText(["Release Planner", "/work/widget", "fixed the node\n  CVE"]);
		expect(matchesSearchText(text, "rls plnr")).toBe(true);
		expect(matchesSearchText(text, '"node cve"')).toBe(true);
		expect(matchesSearchText(text, "re:/WORK/\\w+")).toBe(true);
		expect(matchesSearchText(text, "re:(")).toBe(false);
	});

	it("rejects noisy fuzzy matches that only scatter across the corpus", () => {
		const text = createSessionSearchText(["Release Planner", "/work/widget", "fixed the node\n  CVE"]);
		expect(matchesSearchText(text, "planner")).toBe(true);
		expect(matchesSearchText(text, "rwfxce")).toBe(false);
	});

	it("reuses a compiled query across corpus matches", () => {
		let trimCalls = 0;
		const observableQuery = {
			trim: () => {
				trimCalls++;
				return 'release "node cve"';
			},
		} as unknown as string;

		const compiled = compileSearchQuery(observableQuery);
		expect(trimCalls).toBe(1);
		expect(matchCompiledSearchText("Release notes include node\n CVE details", compiled).matches).toBe(true);
		expect(matchCompiledSearchText("Release notes include node CVE details", compiled).matches).toBe(true);
		expect(trimCalls).toBe(1);
	});

	it("compiles a query once for a unified-session rebuild", () => {
		let trimCalls = 0;
		const observableQuery = {
			trim: () => {
				trimCalls++;
				return "needle";
			},
		} as unknown as string;
		const records: UnifiedSessionRecord[] = [
			{ identity: "one", identityAliases: ["one"], section: "idle", searchableText: "needle one" },
			{ identity: "two", identityAliases: ["two"], section: "idle", searchableText: "needle two" },
		];

		expect(filterUnifiedSessionsBySearchQuery(records, observableQuery)).toEqual(records);
		expect(trimCalls).toBe(1);
	});

	it("normalizes each corpus once even when multiple fuzzy tokens need scoring", () => {
		let lowerCalls = 0;
		const observableText = {
			toLowerCase: () => {
				lowerCalls++;
				return "release planner widget";
			},
		} as unknown as string;
		const compiled = compileSearchQuery("rls plnr wdgt");

		expect(matchCompiledSearchText(observableText, compiled).matches).toBe(true);
		expect(lowerCalls).toBe(1);
	});
	it("preserves pi-tui fuzzy scoring for compiled queries", () => {
		const texts = ["Release Planner", "node-cve 123", "/work/widget", "abc123", "123abc", "mixed CASE"];
		const queries = ["rls", "plnr", "nc", "abc123", "123abc", "mxdcse", "missing"];
		for (const text of texts) {
			for (const query of queries) {
				const direct = fuzzyMatch(query, text);
				const compiled = matchCompiledSearchText(text, compileSearchQuery(query));
				expect(compiled.matches).toBe(
					text.toLowerCase().includes(query.toLowerCase()) || (direct.matches && direct.score <= 25),
				);
			}
		}
	});
});
