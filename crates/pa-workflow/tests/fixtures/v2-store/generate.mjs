// Regenerates the TS-written Workflow V2 store goldens by running the TS
// fork's own slice-4 store (commit 5e2b7f2fe) under node.
//
//   C=5e2b7f2fe P=packages/coding-agent; SRC=$(mktemp -d)
//   for f in src/core/workflow-v2-wire src/core/workflow-v2-reducer \
//            src/core/workflow-v2-store test/workflow-v2-slice4-fixtures; do
//     git show $C:$P/$f.ts > "$SRC/$(basename $f).ts"; done
//   node crates/pa-workflow/tests/fixtures/v2-store/generate.mjs "$SRC" \
//     crates/pa-workflow/tests/fixtures/v2-store
//
//   # reverse direction: the TS store opens a Rust-written database
//   node crates/pa-workflow/tests/fixtures/v2-store/generate.mjs "$SRC" --verify <workflows-dir>
//
// Node's type stripping runs the sources as they are, except for TS
// parameter properties, which it refuses: `prepare` expands them into
// plain parameters plus `this.x = x;` (no behaviour change) and points the
// relative imports at the copied `.ts` files.
//
// Outputs: `scenario.json` (the steps, every step's TS result, the final
// aggregates), `dump.json` (every table row as SQL `quote()` literals, the
// schema, the pragmas), and `ts-v2.sqlite` (the closed, checkpointed
// database file).
import { copyFileSync, mkdtempSync, readFileSync, rmSync, writeFileSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { DatabaseSync } from "node:sqlite";
import { createHash } from "node:crypto";

const [source, ...rest] = process.argv.slice(2);
const SCOPE = `sha256:${"c".repeat(64)}`;

function expandParameterProperties(src) {
	let out = "";
	let at = 0;
	for (;;) {
		const start = src.indexOf("constructor(", at);
		if (start < 0) return out + src.slice(at);
		let depth = 0;
		let end = start + "constructor".length;
		for (; end < src.length; end++) {
			if (src[end] === "(") depth++;
			else if (src[end] === ")" && --depth === 0) break;
		}
		const params = src.slice(start + "constructor(".length, end);
		const names = [];
		const cleaned = params.replace(/(?:(?:private|public|protected|readonly)\s+)+([A-Za-z_]\w*)/g, (_, n) => {
			names.push(n);
			return n;
		});
		let bodyAt = src.indexOf("{", end) + 1;
		const sup = /^\s*super\([^;]*\);/.exec(src.slice(bodyAt));
		if (sup) bodyAt += sup[0].length;
		out += src.slice(at, start) + "constructor(" + cleaned + src.slice(end, bodyAt);
		if (names.length) out += ` ${names.map((n) => `this.${n} = ${n};`).join(" ")}`;
		at = bodyAt;
	}
}

function prepare(dir) {
	const work = mkdtempSync(join(tmpdir(), "wv2-ts-src-"));
	for (const name of ["workflow-v2-wire", "workflow-v2-reducer", "workflow-v2-store", "workflow-v2-slice4-fixtures"]) {
		const text = readFileSync(join(dir, `${name}.ts`), "utf8");
		const rewritten = expandParameterProperties(text).replace(
			/from "\.\.?\/(?:src\/core\/)?(workflow-v2-[a-z0-9-]+)\.js"/g,
			'from "./$1.ts"',
		);
		writeFileSync(join(work, `${name}.ts`), rewritten);
	}
	return work;
}

const work = prepare(source);
const { WorkflowV2Store } = await import(join(work, "workflow-v2-store.ts"));
const F = await import(join(work, "workflow-v2-slice4-fixtures.ts"));

if (rest[0] === "--verify") {
	// Open a Rust-written store with the TS store at a higher epoch, hydrate
	// every run, and print the aggregates (the caller diffs them).
	const store = WorkflowV2Store.open({ root: rest[1], rootScopeDigest: SCOPE });
	store.acquireWriter(Number(rest[2] ?? 2));
	const out = {};
	for (const runId of ["run-1", "run-2", "run-3"]) out[runId] = JSON.parse(JSON.stringify(store.loadAggregate(runId)));
	store.close();
	process.stdout.write(`${JSON.stringify(out)}\n`);
	process.exit(0);
}

const out = rest[0];
const EVIDENCE = F.EVIDENCE;
const OTHER = `sha256:${"b".repeat(64)}`;

// ---- the scenario: data the Rust test replays step by step ---------------

const steps = [];
const step = (op, args = {}) => steps.push({ op, ...args });

function createReq(runId, requestId = `req-create-${runId}`) {
	return { protocol: "prime.workflow.request/v2", requestId, action: "create", definition: F.validDefinition() };
}
function postReq(action, runId, commandId, revision, extra = {}) {
	return {
		protocol: "prime.workflow.request/v2",
		requestId: `req-${action}-${commandId}`,
		action,
		runId,
		commandId,
		expectedRevision: revision,
		expectedControllerEpoch: 1,
		expectedCancelEpoch: 0,
		...extra,
	};
}
const ev = (runId, seq, type, data) => F.ctrlEvent(runId, seq, type, data);
const create = (runId) =>
	step("command", { request: createReq(runId), effect: { definition: F.validDefinition(), events: [ev(runId, 1, "RunAdmitted", { evidenceDigest: EVIDENCE })] } });

function driveToAdmissionBound(runId, attemptId, op, child, turn, hostReq) {
	create(runId);
	step("command", { request: postReq("start", runId, `${runId}-start`, 1), effect: { events: [ev(runId, 2, "RunStarted", { evidenceDigest: EVIDENCE })] } });
	step("command", { request: postReq("start", runId, `${runId}-ready`, 2), effect: { events: [ev(runId, 3, "NodeBecameReady", { nodeId: "n1", evidenceDigest: EVIDENCE })] } });
	step("command", {
		request: postReq("start", runId, `${runId}-prep`, 3),
		effect: { events: [ev(runId, 4, "AttemptPrepared", { nodeId: "n1", attemptId, evidenceDigest: EVIDENCE })] },
	});
	step("command", {
		request: postReq("start", runId, `${runId}-disp`, 4),
		effect: {
			events: [ev(runId, 5, "AttemptDispatchCommitted", { nodeId: "n1", attemptId, evidenceDigest: EVIDENCE })],
			operations: [
				{
					operationId: op,
					attemptId,
					kind: "deliver",
					requestId: hostReq,
					canonicalRequest: { protocol: "prime.workflow.retained-request/v2", operation: "child.admit", requestId: hostReq, prompt: "zeta é   \"q\"" },
				},
			],
		},
	});
	step("command", {
		request: postReq("start", runId, `${runId}-bind`, 5),
		effect: {
			events: [ev(runId, 6, "AttemptAdmissionBound", { nodeId: "n1", attemptId, operationId: op, rlmChildId: child, turnId: turn, evidenceDigest: EVIDENCE })],
			bindings: [{ attemptId, workflowChildId: `wfc-${attemptId}`, rlmChildId: child, turnId: turn, requestId: hostReq, canonicalDigest: EVIDENCE }],
		},
	});
}
const host = (runId, id, cursor, type, data) => step("ingest", { runId, event: F.hostEvent(id, cursor, type, data) });

step("open", { epoch: 1 });
// run-1: the full succeeded path.
driveToAdmissionBound("run-1", "a1", "op1", "child1", "turn1", "hostreq-1");
const turn1 = { requestId: "hostreq-1", rlmChildId: "child1", turnId: "turn1", evidenceDigest: EVIDENCE };
host("run-1", "he-start", "hc-1", "TurnStarted", turn1);
const settled1 = { ...turn1, settlement: F.turnSettlement({ nodeId: "n1", attemptId: "a1", rlmChildId: "child1", turnId: "turn1" }) };
host("run-1", "he-settle", "hc-2", "TurnSettled", settled1);
host("run-1", "he-settle", "hc-9", "TurnSettled", settled1); // duplicate: no-op
step("command", {
	request: postReq("start", "run-1", "run-1-obs", 6),
	effect: { events: [ev("run-1", 7, "AttemptSettlementObserved", { nodeId: "n1", attemptId: "a1", settlementDigest: EVIDENCE, outcome: "completed" })] },
});
step("command", {
	request: postReq("start", "run-1", "run-1-acc", 7),
	effect: {
		events: [ev("run-1", 8, "AttemptAccepted", { nodeId: "n1", attemptId: "a1", evidenceDigest: EVIDENCE })],
		acceptance: [{ nodeId: "n1", attemptId: "a1", decision: "accepted", evidenceDigest: EVIDENCE }],
	},
});
step("command", { request: postReq("start", "run-1", "run-1-drain", 8), effect: { events: [ev("run-1", 9, "RunDraining", { evidenceDigest: EVIDENCE })] } });
step("command", {
	request: postReq("start", "run-1", "run-1-term", 9),
	effect: { events: [ev("run-1", 10, "RunTerminalized", { evidenceDigest: EVIDENCE, outcome: "succeeded" })] },
});
step("claim", { owner: "w", leaseMs: 60_000, limit: 10 });
step("claim", { owner: "w", leaseMs: 60_000, limit: 10 }); // lease held: nothing
step("advance", { ms: 120_000 });
step("claim", { owner: "w", leaseMs: 60_000, limit: 10 }); // expired: same identity
step("ack", { operationId: "op1", owner: "intruder", hostRequestId: "hostreq-1", outcome: "succeeded", receipt: { ok: true } });
step("ack", { operationId: "op1", owner: "w", hostRequestId: "hostreq-1", outcome: "succeeded", receipt: { ok: true, n: 1 } });
// idempotency: identical replay, then a changed body under the same id.
step("command", { request: createReq("run-1"), effect: { definition: F.validDefinition(), events: [ev("run-1", 1, "RunAdmitted", { evidenceDigest: EVIDENCE })] } });
step("command", {
	request: { ...createReq("run-x", "req-create-run-1"), definition: F.validDefinition({ maxTotalTokens: 999 }) },
	effect: { definition: F.validDefinition({ maxTotalTokens: 999 }), events: [ev("run-x", 1, "RunAdmitted", { evidenceDigest: EVIDENCE })] },
});
// run-2: cancellation with host cancel facts, a cancelled settlement,
// a host cursor fact, tombstone and acceptance evidence, two outbox ops.
driveToAdmissionBound("run-2", "a2", "op2", "child2", "turn2", "hostreq-2");
const turn2 = { requestId: "hostreq-2", rlmChildId: "child2", turnId: "turn2", evidenceDigest: EVIDENCE };
host("run-2", "he2-start", "hc2-1", "TurnStarted", turn2);
step("command", { request: postReq("start", "run-2", "run-2-stale", 99), effect: { events: [] } }); // stale fence
step("command", {
	request: postReq("cancel", "run-2", "run-2-cancel", 6, { reason: "operator stop" }),
	effect: {
		events: [
			ev("run-2", 7, "RunCancellationRequested", { evidenceDigest: EVIDENCE }),
			ev("run-2", 8, "AttemptCancellationRequested", { nodeId: "n1", attemptId: "a2", evidenceDigest: EVIDENCE }),
		],
		operations: [{ operationId: "op2-cancel", attemptId: "a2", kind: "cancel", requestId: "hostreq-2c", canonicalRequest: { operation: "child.cancel", requestId: "hostreq-2c" } }],
	},
});
host("run-2", "he2-cr", "hc2-2", "CancelRequested", turn2);
host("run-2", "he2-ca", "hc2-3", "CancelActuated", turn2);
const cancelled = {
	...F.turnSettlement({ nodeId: "n1", attemptId: "a2", rlmChildId: "child2", turnId: "turn2", cancelActuated: true, quiescent: false, usageFinal: false, outcome: "cancelled" }),
	result: { kind: "none", reason: "cancelled" },
	error: { code: "CANCELLED", message: "cancelled by the controller", retryable: false },
	settlementDigest: OTHER,
};
host("run-2", "he2-settle", "hc2-4", "TurnSettled", { ...turn2, settlement: cancelled });
host("run-2", "he2-q", "hc2-5", "ChildQuiescent", { requestId: "hostreq-2", rlmChildId: "child2", evidenceDigest: EVIDENCE });
host("run-2", "he2-tomb", "hc2-6", "ChildTombstoned", { requestId: "hostreq-2", rlmChildId: "child2", evidenceDigest: EVIDENCE });
step("command", { request: postReq("start", "run-2", "run-2-gap", 8), effect: { events: [ev("run-2", 11, "HostCursorAdvanced", { hostCursor: "hc2-6", evidenceDigest: EVIDENCE })] } }); // gap
step("command", {
	request: postReq("cancel", "run-2", "run-2-obs", 8, { reason: "observe" }),
	effect: {
		events: [
			ev("run-2", 9, "AttemptSettlementObserved", { nodeId: "n1", attemptId: "a2", settlementDigest: OTHER, outcome: "cancelled" }),
			ev("run-2", 10, "HostCursorAdvanced", { hostCursor: "hc2-6", evidenceDigest: EVIDENCE }),
		],
		acceptance: [{ nodeId: "n1", attemptId: null, decision: "not_evaluated", evidenceDigest: null }],
		tombstones: [{ rlmChildId: "child2", requestId: "hostreq-2", tombstoneDigest: OTHER }],
	},
});
step("claim", { owner: "w2", leaseMs: 1_000, limit: 1 });
step("claim", { owner: "w2", leaseMs: 1_000, limit: 10 });
step("ack", { operationId: "op2", owner: "w2", hostRequestId: "hostreq-2", outcome: "failed", receipt: { error: "x" } });
step("ack", { operationId: "op2-cancel", owner: "w2", hostRequestId: "hostreq-2c", outcome: "ambiguous", receipt: {} });
// run-3: quarantined straight from created.
create("run-3");
step("command", { request: postReq("start", "run-3", "run-3-q", 1), effect: { events: [ev("run-3", 2, "RunQuarantined", { evidenceDigest: EVIDENCE })] } });
// retention on run-1.
step("erase", { runId: "run-2", policyBasis: "retention-30d" }); // nonterminal: refused
step("erase", { runId: "run-1", policyBasis: "retention-30d" });
step("compact", { runId: "run-2", throughSequence: 3, snapshotDigest: EVIDENCE }); // run-2 ops terminal: allowed
step("compact", { runId: "run-1", throughSequence: 5, snapshotDigest: OTHER });
step("listEvents", { runId: "run-1", afterSequence: 0, limit: 500 }); // SNAPSHOT_REQUIRED
step("listEvents", { runId: "run-1", afterSequence: 5, limit: 3 });
step("reconcile", { runId: "run-1" });
step("reconcile", { runId: "run-2" });
// restart: the writer reopens at a higher epoch.
step("reopen", { epoch: 2 });
step("aggregate", { runId: "run-1" });
step("aggregate", { runId: "run-2" });
step("aggregate", { runId: "run-3" });
step("hostCursor", { runId: "run-2" });
step("close");

// ---- run it through the TS store ------------------------------------------

const base = mkdtempSync(join(tmpdir(), "wv2-golden-"));
const root = join(base, "workflows");
let nowMs = Date.UTC(2026, 8, 15, 0, 0, 0);
const clock = () => new Date(nowMs).toISOString();
let store;
const results = [];
const settle = (fn) => {
	try {
		const value = fn();
		return value === undefined ? null : JSON.parse(JSON.stringify(value));
	} catch (error) {
		return { error: error.code ?? String(error) };
	}
};
for (const s of steps) {
	switch (s.op) {
		case "open":
		case "reopen":
			if (store) store.close();
			store = WorkflowV2Store.open({ root, rootScopeDigest: SCOPE, clock });
			results.push(settle(() => store.acquireWriter(s.epoch)));
			break;
		case "command":
			results.push(settle(() => store.applyCommand(s.request, s.effect)));
			break;
		case "ingest":
			results.push(settle(() => store.ingestHostEvent(s.runId, s.event)));
			break;
		case "claim":
			results.push(settle(() => store.claimOutbox(s.owner, s.leaseMs, s.limit)));
			break;
		case "ack":
			results.push(settle(() => store.acknowledgeOutbox(s)));
			break;
		case "advance":
			nowMs += s.ms;
			results.push(null);
			break;
		case "erase":
			results.push(settle(() => store.eraseRunText(s.runId, s.policyBasis)));
			break;
		case "compact":
			results.push(settle(() => store.compactEvents(s.runId, s.throughSequence, s.snapshotDigest)));
			break;
		case "listEvents":
			results.push(settle(() => store.listEvents(s.runId, s.afterSequence, s.limit)));
			break;
		case "reconcile":
			results.push(settle(() => store.reconcileTerminalMismatch(s.runId)));
			break;
		case "aggregate":
			results.push(settle(() => store.loadAggregate(s.runId)));
			break;
		case "hostCursor":
			results.push(settle(() => store.getHostCursor(s.runId)));
			break;
		case "close":
			store.close();
			store = undefined;
			results.push(null);
			break;
		default:
			throw new Error(`unknown step ${s.op}`);
	}
}

// ---- dump ------------------------------------------------------------------

export function dump(path) {
	const db = new DatabaseSync(path, { readOnly: true });
	const scalar = (sql) => Object.values(db.prepare(sql).get())[0];
	const schema = db.prepare("SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY type, name").all().map((r) => ({ ...r }));
	const tables = {};
	for (const { name } of db.prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name").all()) {
		const cols = db.prepare(`PRAGMA table_info(${name})`).all().map((c) => c.name);
		const select = cols.map((c) => `quote(${c})`).join(", ");
		tables[name] = db
			.prepare(`SELECT ${select} FROM ${name} ORDER BY rowid`)
			.all()
			.map((row) => Object.fromEntries(cols.map((c, i) => [c, Object.values(row)[i]])));
	}
	for (const row of tables.store_migrations ?? []) row.applied_at = "<masked>";
	const out = {
		applicationId: Number(scalar("PRAGMA application_id")),
		userVersion: Number(scalar("PRAGMA user_version")),
		schema,
		tables,
	};
	db.close();
	return out;
}

const dbPath = join(root, "v2.sqlite");
if (existsSync(`${dbPath}-wal`)) throw new Error("the closed store left a WAL behind");
writeFileSync(join(out, "scenario.json"), `${JSON.stringify({ scope: SCOPE, steps, results }, null, "\t")}\n`);
writeFileSync(join(out, "dump.json"), `${JSON.stringify(dump(dbPath), null, "\t")}\n`);
copyFileSync(dbPath, join(out, "ts-v2.sqlite"));
process.stdout.write(
	`ts-v2.sqlite sha256:${createHash("sha256").update(readFileSync(dbPath)).digest("hex")} steps=${steps.length}\n`,
);
rmSync(base, { recursive: true, force: true });
rmSync(work, { recursive: true, force: true });
