/**
 * Slice 4 crash-window child (test-only). Drives the store to admission_bound
 * (each command commits), then enters ingestHostEvent and is SIGKILLed mid
 * transaction — after the host_inbox INSERT has executed but BEFORE COMMIT —
 * via the clock hook. The parent reopens and asserts zero partial effect.
 */
import { WorkflowV2Store } from "../src/core/workflow-v2-store.js";
import { ctrlEvent, EVIDENCE, hostEvent, validDefinition } from "./workflow-v2-slice4-fixtures.js";

const root = process.argv[2];
const scope = process.argv[3];
let armed = false;
let armedCount = 0;
const clock = (): string => {
	if (armed) {
		armedCount += 1;
		// 2nd armed call lands inside ingestHostEvent's BEGIN IMMEDIATE, after the
		// host_inbox INSERT and before COMMIT — die there.
		if (armedCount === 2) process.kill(process.pid, "SIGKILL");
	}
	return new Date(Date.UTC(2026, 8, 15, 0, 0, armedCount % 60)).toISOString();
};

const store = WorkflowV2Store.open({ root, rootScopeDigest: scope, clock });
store.acquireWriter(1);
const runId = "run-1";
function post(action: string, commandId: string, revision: number): Record<string, unknown> {
	return {
		protocol: "prime.workflow.request/v2",
		requestId: `req-${commandId}`,
		action,
		runId,
		commandId,
		expectedRevision: revision,
		expectedControllerEpoch: 1,
		expectedCancelEpoch: 0,
	};
}
store.applyCommand(
	{ protocol: "prime.workflow.request/v2", requestId: "c-create", action: "create", definition: validDefinition() },
	{ definition: validDefinition(), events: [ctrlEvent(runId, 1, "RunAdmitted", { evidenceDigest: EVIDENCE })] },
);
store.applyCommand(post("start", "s", 1), {
	events: [ctrlEvent(runId, 2, "RunStarted", { evidenceDigest: EVIDENCE })],
});
store.applyCommand(post("start", "r", 2), {
	events: [ctrlEvent(runId, 3, "NodeBecameReady", { nodeId: "n1", evidenceDigest: EVIDENCE })],
});
store.applyCommand(post("start", "p", 3), {
	events: [ctrlEvent(runId, 4, "AttemptPrepared", { nodeId: "n1", attemptId: "a1", evidenceDigest: EVIDENCE })],
});
store.applyCommand(post("start", "d", 4), {
	events: [
		ctrlEvent(runId, 5, "AttemptDispatchCommitted", { nodeId: "n1", attemptId: "a1", evidenceDigest: EVIDENCE }),
	],
});
store.applyCommand(post("start", "b", 5), {
	events: [
		ctrlEvent(runId, 6, "AttemptAdmissionBound", {
			nodeId: "n1",
			attemptId: "a1",
			operationId: "op1",
			rlmChildId: "child1",
			turnId: "turn1",
			evidenceDigest: EVIDENCE,
		}),
	],
});
process.stdout.write("BOUND\n");
armed = true;
store.ingestHostEvent(
	runId,
	hostEvent("he-kill", "hc-kill", "TurnStarted", {
		requestId: "req-a1",
		rlmChildId: "child1",
		turnId: "turn1",
		evidenceDigest: EVIDENCE,
	}),
);
process.stdout.write("UNREACHABLE\n");
