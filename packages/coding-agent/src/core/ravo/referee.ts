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
 *
 * A replay case probes the executable environment (`import x`), so it is
 * evidence only about a claim a proposal's skill writes can speak to: a skill
 * whose python reference imports the module (or distribution) a missing-module
 * probe names. A memory or prompt fix leaves the probe raising after a correct
 * fix, and a skill registers a reference without changing any other import, so
 * every other claim is `not_applicable` and the provisional window is its
 * referee.
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
	 * about whether the failure stopped.
	 */
	verifiedAt?: string;
}

/** Distinct cases a record keeps per fingerprint; the oldest is evicted first. */
export const MAX_REPLAY_CASES = 8;

export type ReplayOutcome =
	| { kind: "raised"; exceptionClass: string; detail: string }
	| { kind: "clean"; detail: string }
	/** The verification itself could not be performed. Never a pass; see `refereeOpponentPassed`. */
	| { kind: "unrunnable"; detail: string };

export type RefereeVerdictStatus =
	/** A verified case ran and the recorded exception recurred: the claim is refuted. */
	| "upheld"
	/** Every verified case ran clean: the claim is corroborated. */
	| "cleared"
	/** A verified case could not be run, or raised something else. Conservative reject. */
	| "unverifiable"
	/**
	 * The failure is replay-derivable but no case ever reproduced. Evidence of
	 * this kind is expected and missing, so the claim fails closed.
	 */
	| "no_evidence"
	/**
	 * A replay cannot speak to this claim: no skill the proposal writes imports
	 * what a missing-module or missing-distribution probe of the fingerprint
	 * names (a memory or prompt fix leaves `import x; x.y` raising), or the
	 * failure has no derivable reproduction at all. The claim stands on the
	 * provisional window instead.
	 */
	| "not_applicable";

export interface RefereeVerdict {
	fingerprintId: string;
	status: RefereeVerdictStatus;
	detail: string;
}

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

const MAX_REPLAY_SOURCE_CHARS = 600;
const MODULE_PATTERN = /^[A-Za-z_][A-Za-z0-9_]*(?:\.[A-Za-z_][A-Za-z0-9_]*)*$/;
const NO_MODULE_NAMED = /no module named ['"]([^'"]+)['"]/i;
const NO_PACKAGE_METADATA = /no package metadata was found for ['"]?([^\s'"]+?)['"]?\s*$/im;
const PACKAGE_NOT_FOUND_LINE = /PackageNotFoundError: ['"]?([^\s'"]+?)['"]?\s*$/m;
const DISTRIBUTION_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._-]*$/;

/** Top-level modules whose import alone acts (a browser, a GUI, a CLI, pip, a debugger, a server); never replayed. */
export const REPLAY_MODULE_DENYLIST: ReadonlySet<string> = new Set([
	"antigravity",
	"this",
	"idlelib",
	"turtledemo",
	"turtle",
	"tkinter",
	"webbrowser",
	"venv",
	"ensurepip",
	"pip",
	"pydoc",
	"zipapp",
	"site",
	"sitecustomize",
	"usercustomize",
	"runpy",
	"code",
	"pdb",
	"cProfile",
	"profile",
	"trace",
	"timeit",
	"doctest",
	"unittest",
	"http",
	"xmlrpc",
	"smtpd",
	"ftplib",
	"telnetlib",
	"socketserver",
	"wsgiref",
]);

/**
 * The side-effect-free probes a replay case may run: exactly the kinds
 * adjudication can apply to a skill write (`replayAppliesToSkillImports`). A
 * case's source is always rendered from one of these (`replayProbeSource`),
 * and only a source that parses back to a valid probe (`replayProbeOf`) is ever
 * kept by the failure ledger, executed, listed as evidence, or made a referee
 * opponent. A case of a retired kind (a module attribute, a name imported from
 * a module, an executable) is dropped wherever the failure ledger builds a case
 * list (`mergeReplayCase`).
 */
export type ReplayProbe =
	/** `import X`: a missing module. */
	| { kind: "module"; module: string }
	/** `importlib.metadata.version("d")`: a missing distribution. */
	| { kind: "distribution"; distribution: string };

/**
 * A module path a replay may import. A private segment (`_x`, `__main__`) can
 * run a package's entry point, and a denylisted top-level module acts on
 * import, so neither is ever derived or executed.
 */
export function isReplayableModulePath(module: string): boolean {
	if (!MODULE_PATTERN.test(module)) return false;
	const segments = module.split(".");
	return !REPLAY_MODULE_DENYLIST.has(segments[0]) && segments.every((segment) => !segment.startsWith("_"));
}

function validProbe(probe: ReplayProbe): boolean {
	switch (probe.kind) {
		case "module":
			return isReplayableModulePath(probe.module);
		case "distribution":
			return DISTRIBUTION_PATTERN.test(probe.distribution);
	}
}

/** Render a probe as Python. String operands are JSON literals, which are valid Python string literals. */
export function replayProbeSource(probe: ReplayProbe): string {
	switch (probe.kind) {
		case "module":
			return `import ${probe.module}`;
		case "distribution":
			return `import importlib.metadata\nimportlib.metadata.version(${JSON.stringify(probe.distribution)})`;
	}
}

const PROBE_PARSERS: ReadonlyArray<(source: string) => ReplayProbe | undefined> = [
	(source) => {
		const match = /^import (\S+)$/.exec(source);
		return match ? { kind: "module", module: match[1] } : undefined;
	},
	(source) => {
		const match = /^import importlib\.metadata\nimportlib\.metadata\.version\("([^"\\]*)"\)$/.exec(source);
		return match ? { kind: "distribution", distribution: match[1] } : undefined;
	},
];

/**
 * The probe a stored case runs, or undefined when its source is not exactly a
 * valid rendered probe. A ledger is a file on disk; only sources this module
 * would itself have derived are ever executed from one.
 */
export function replayProbeOf(replay: Pick<ReplayCase, "source">): ReplayProbe | undefined {
	for (const parse of PROBE_PARSERS) {
		const probe = parse(replay.source);
		if (probe && validProbe(probe) && replayProbeSource(probe) === replay.source) return probe;
	}
	return undefined;
}

/**
 * Derive an executable reproduction from the recorded traceback. Only the
 * environment failures a skill write can fix qualify, each with a
 * side-effect-free, deterministic probe (`ReplayProbe`): a missing module and
 * a missing distribution. Module paths with a private segment or a denylisted
 * top level (`REPLAY_MODULE_DENYLIST`) are refused, so re-running a case cannot
 * do anything beyond an ordinary import.
 *
 * Everything else yields no case on purpose: a missing file path is user data;
 * a missing module attribute, or a name `from X import n` cannot find in a
 * module X that imports, is a guessed API that a skill registering a reference
 * does not change; and a missing program is something no skill installs. No
 * adjudication could ever apply such a case.
 *
 * The normalized fingerprint message has had quoted strings replaced by `?`,
 * so the names survive only in the raw excerpt. Callers pass only the kernel's
 * own traceback for a cell that raised (see `extractFailures`).
 */
export function deriveReplayCase(fingerprint: FailureFingerprint, excerpt: string): ReplayCase | undefined {
	if (fingerprint.kind !== "python_exception") return undefined;
	const derived = deriveReplayProbe(fingerprint, excerpt);
	if (!derived || !validProbe(derived.probe)) return undefined;
	const source = replayProbeSource(derived.probe);
	if (source.length > MAX_REPLAY_SOURCE_CHARS) return undefined;
	const exceptionClass = derived.exceptionClass ?? fingerprint.exceptionClass;
	return {
		language: "python",
		source,
		...(exceptionClass === undefined ? {} : { exceptionClass }),
	};
}

function deriveReplayProbe(
	fingerprint: FailureFingerprint,
	excerpt: string,
): { probe: ReplayProbe; exceptionClass?: string } | undefined {
	const missingModule = NO_MODULE_NAMED.exec(excerpt);
	if (missingModule) {
		return { probe: { kind: "module", module: missingModule[1] } };
	}
	const exceptionClass = fingerprint.exceptionClass === undefined ? undefined : bareClass(fingerprint.exceptionClass);
	const distribution =
		NO_PACKAGE_METADATA.exec(excerpt)?.[1] ??
		(exceptionClass === "PackageNotFoundError" ? PACKAGE_NOT_FOUND_LINE.exec(excerpt)?.[1] : undefined);
	if (distribution) {
		return { probe: { kind: "distribution", distribution }, exceptionClass: "PackageNotFoundError" };
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

/**
 * Normalize a stored case list. A record written before `replayCases` existed
 * carries a single `replayCase`; it becomes the list's oldest entry. A case that
 * is not a valid probe is dropped before the bound applies.
 */
export function normalizeReplayCases(value: unknown, legacy?: unknown): ReplayCase[] {
	const raw: unknown[] = [...(legacy === undefined ? [] : [legacy]), ...(Array.isArray(value) ? value : [])];
	let cases: ReplayCase[] = [];
	for (const item of raw) {
		const replay = normalizeReplayCase(item);
		if (replay && replayProbeOf(replay) !== undefined) cases = foldReplayProbe(cases, replay);
	}
	return cases;
}

/**
 * Fold one case into a list kept distinct by source, oldest first and bounded
 * to `MAX_REPLAY_CASES`. Only valid probes (`replayProbeOf`) are kept: an
 * incoming case that is not one is ignored and a stored one is dropped, so the
 * bound counts runnable cases only. A re-observed source moves to the newest
 * slot, and a case that already reproduced keeps its `verifiedAt`: a fresh
 * derivation that has not been self-checked never downgrades evidence.
 */
export function mergeReplayCase(cases: readonly ReplayCase[], incoming: ReplayCase): ReplayCase[] {
	const live = cases.filter((replay) => replayProbeOf(replay) !== undefined);
	return replayProbeOf(incoming) === undefined ? live : foldReplayProbe(live, incoming);
}

/** `mergeReplayCase` for a list and an incoming case already checked to be probes. */
function foldReplayProbe(probes: readonly ReplayCase[], incoming: ReplayCase): ReplayCase[] {
	const existing = probes.find((replay) => replay.source === incoming.source);
	const merged: ReplayCase =
		existing?.verifiedAt && !incoming.verifiedAt ? { ...incoming, verifiedAt: existing.verifiedAt } : incoming;
	const next = [...probes.filter((replay) => replay.source !== incoming.source), merged];
	return next.length > MAX_REPLAY_CASES ? next.slice(next.length - MAX_REPLAY_CASES) : next;
}

/** Every case a record carries, oldest first, reading a legacy single `replayCase` too. */
export function replayCasesOf(record: FailureRecord): ReplayCase[] {
	if (record.replayCases !== undefined) return [...record.replayCases];
	return record.replayCase ? [record.replayCase] : [];
}

/**
 * A fingerprint's replay evidence: only the cases that have reproduced the
 * recorded failure and that are a probe adjudication can apply. Storage never
 * keeps a case of a retired probe kind (`mergeReplayCase`); one held in memory
 * is still not evidence of anything.
 */
export function verifiedReplayCasesOf(record: FailureRecord): ReplayCase[] {
	return replayCasesOf(record).filter(
		(replay) => replay.verifiedAt !== undefined && replayProbeOf(replay) !== undefined,
	);
}

/** The newest verified case, if the record carries any evidence at all. */
export function replayCaseOf(record: FailureRecord): ReplayCase | undefined {
	return verifiedReplayCasesOf(record).at(-1);
}

/**
 * The modules a proposal's skill writes (creates and updates) import: each
 * python reference's `import` (or legacy `python_import`), as the skill dry-run
 * reads it. Deletes and malformed references contribute nothing.
 */
export function skillImportsOf(
	edits: readonly { readonly kind?: unknown; readonly action?: unknown; readonly reference?: unknown }[],
): string[] {
	const imports = new Set<string>();
	for (const edit of edits) {
		if (edit.kind !== "skill" || (edit.action !== "create" && edit.action !== "update")) continue;
		const reference = edit.reference;
		if (typeof reference !== "object" || reference === null || Array.isArray(reference)) continue;
		const { type, import: moduleImport, python_import: legacyImport } = reference as Record<string, unknown>;
		if (type !== "python") continue;
		const raw = typeof moduleImport === "string" && moduleImport.trim() ? moduleImport : legacyImport;
		const module = typeof raw === "string" ? raw.trim() : "";
		if (MODULE_PATTERN.test(module)) imports.add(module);
	}
	return [...imports];
}

function modulePathsOverlap(left: string, right: string): boolean {
	return left === right || left.startsWith(`${right}.`) || right.startsWith(`${left}.`);
}

/** Distribution and import names compared under PEP 503 normalization, one a segment prefix of the other. */
function distributionOverlapsModule(distribution: string, module: string): boolean {
	const dist = distribution.toLowerCase().split(/[-_.]+/);
	const mod = module.toLowerCase().split(/[-_.]+/);
	return dist.slice(0, Math.min(dist.length, mod.length)).every((segment, index) => segment === mod[index]);
}

/**
 * Whether a replay case can speak to a proposal whose skills import
 * `skillImports`: it is a missing-module (`import X`) or missing-distribution
 * probe, and what it names equals a skill import or is a dotted prefix of one
 * (or the reverse). A source that is not a valid probe, including one of a
 * retired kind, never applies.
 */
export function replayAppliesToSkillImports(
	replay: Pick<ReplayCase, "source">,
	skillImports: readonly string[],
): boolean {
	const probe = replayProbeOf(replay);
	if (!probe) return false;
	switch (probe.kind) {
		case "module":
			return skillImports.some((module) => modulePathsOverlap(probe.module, module));
		case "distribution":
			return skillImports.some((module) => distributionOverlapsModule(probe.distribution, module));
	}
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
 * Whether a verdict is executable evidence that belongs in the opponent pool as
 * `referee:<fp>`. `no_evidence` and `not_applicable` add nothing a replay
 * actually adjudicated, so they never introduce a referee criterion (a
 * persisted one still charges `no_evidence`; see `refereeOpponentPassed`).
 */
export function refereeVerdictIsEvidence(verdict: RefereeVerdict | undefined): verdict is RefereeVerdict {
	return verdict !== undefined && verdict.status !== "no_evidence" && verdict.status !== "not_applicable";
}

/**
 * `failure:<fp>`: the claim stays necessary, and executable evidence makes it
 * insufficient. With no verdict, or one a replay cannot speak to, the claim
 * alone passes and the provisional window is its referee. A derivable failure
 * that never reproduced FAILS CLOSED: the evidence the gate expects is missing.
 */
export function failureOpponentPassed(claimed: boolean, verdict: RefereeVerdict | undefined): boolean {
	if (!claimed) return false;
	if (!verdict || verdict.status === "not_applicable") return true;
	return verdict.status === "cleared";
}

/**
 * `referee:<fp>`: the second opponent the Rocq development adds to the pool.
 * It only ever adjudicates a claim, so a proposal that claims nothing is
 * charged nothing. A claim the referee refutes, a claim whose verification
 * could not be run, and a claim on a criterion whose evidence has since gone
 * missing all fail.
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
	if (!claimed) return true;
	if (!verdict || verdict.status === "not_applicable") return true;
	return verdict.status === "cleared";
}

export function refereeDetail(verdict: RefereeVerdict | undefined, claimed: boolean): string {
	if (!claimed) return "no claim to adjudicate";
	if (!verdict) return "no replay case recorded for this fingerprint";
	return verdict.detail;
}
