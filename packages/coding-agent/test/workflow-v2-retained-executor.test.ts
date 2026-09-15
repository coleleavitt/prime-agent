import { describe, expect, it } from "vitest";
import {
	type Slice3DispatchBinding,
	type Slice3DispatchClaim,
	type Slice3DispatchFacts,
	type Slice3DispatchJournal,
	Slice3RetainedDispatchExecutor,
} from "../src/core/workflow-v2-retained-executor.js";

const BINDING: Slice3DispatchBinding = {
	rlmChildId: "sub-abc123",
	turnId: "turn-1",
	admissionReceiptDigest: `sha256:${"a".repeat(64)}`,
};

/**
 * In-memory stand-in for the per-worker retained journal. State persists across
 * executor instances so a "restart" (new executor over the same journal) reaches
 * the exact same at-most-once decision. claimAndCommitDispatching is synchronous
 * and atomic, exactly like the real generation-fenced transaction.
 */
class FakeDispatchJournal implements Slice3DispatchJournal {
	captureArmed = true;
	outboxClaimed = false;
	dispatching = false;
	providerEntered = false;
	claimAttempts = 0;

	readDispatchFacts(_binding: Slice3DispatchBinding): Slice3DispatchFacts {
		return {
			captureArmed: this.captureArmed,
			outboxUnclaimed: !this.outboxClaimed,
			hasDispatching: this.dispatching,
			hasProviderEntered: this.providerEntered,
		};
	}

	claimAndCommitDispatching(_binding: Slice3DispatchBinding): Slice3DispatchClaim {
		this.claimAttempts += 1;
		if (this.dispatching || this.providerEntered) {
			return { ok: false, code: "DISPATCHING_ALREADY_COMMITTED" };
		}
		if (this.outboxClaimed) {
			return { ok: false, code: "OUTBOX_ALREADY_CLAIMED" };
		}
		this.outboxClaimed = true;
		this.dispatching = true;
		return { ok: true };
	}

	commitProviderEntered(_binding: Slice3DispatchBinding): void {
		this.providerEntered = true;
	}
}

function spyCall(): { fn: () => Promise<string>; calls: () => number } {
	let n = 0;
	return {
		fn: async () => {
			n += 1;
			return "ok";
		},
		calls: () => n,
	};
}

describe("Slice3RetainedDispatchExecutor — at-most-once physical dispatch", () => {
	it("dispatches the single physical provider call exactly once", async () => {
		const journal = new FakeDispatchJournal();
		const spy = spyCall();
		const out = await new Slice3RetainedDispatchExecutor(journal).dispatchOnce(BINDING, spy.fn);
		expect(out.outcome).toBe("dispatched");
		expect(spy.calls()).toBe(1);
		expect(journal.dispatching).toBe(true);
		expect(journal.providerEntered).toBe(true);
	});

	it("no relaunch after DISPATCHING: a committed dispatching fact blocks any further physical call", async () => {
		const journal = new FakeDispatchJournal();
		// A prior attempt committed dispatching (e.g., before crashing).
		journal.outboxClaimed = true;
		journal.dispatching = true;
		const spy = spyCall();
		// A fresh executor (restart) must NOT relaunch.
		const out = await new Slice3RetainedDispatchExecutor(journal).dispatchOnce(BINDING, spy.fn);
		expect(out.outcome).toBe("already_dispatched");
		expect(spy.calls()).toBe(0);
	});

	it("D2: crash after dispatching, before the network call → execution_unknown, dispatch count stays 0 across retries", async () => {
		const journal = new FakeDispatchJournal();
		const spy = spyCall();
		// Fence fails exactly at the provider-effect boundary (after dispatching commits).
		let fenceOk = false;
		const executor = new Slice3RetainedDispatchExecutor(journal, () => {
			if (!fenceOk) throw new Error("worker generation stale");
		});
		const first = await executor.dispatchOnce(BINDING, spy.fn);
		expect(first.outcome).toBe("fence_lost_after_dispatch");
		expect(spy.calls()).toBe(0);
		expect(journal.dispatching).toBe(true); // dispatching is durable
		// Unlimited restarts/retries keep the physical dispatch count at 0 (never prompted).
		fenceOk = true;
		for (let i = 0; i < 5; i++) {
			const retry = await new Slice3RetainedDispatchExecutor(journal, () => {}).dispatchOnce(BINDING, spy.fn);
			expect(retry.outcome).toBe("already_dispatched");
		}
		expect(spy.calls()).toBe(0);
	});

	it("at-most-once across many relaunch attempts: exactly one physical call total", async () => {
		const journal = new FakeDispatchJournal();
		const spy = spyCall();
		const outcomes = [];
		for (let i = 0; i < 10; i++) {
			outcomes.push(await new Slice3RetainedDispatchExecutor(journal).dispatchOnce(BINDING, spy.fn));
		}
		expect(spy.calls()).toBe(1);
		expect(outcomes.filter((o) => o.outcome === "dispatched")).toHaveLength(1);
		expect(outcomes.filter((o) => o.outcome === "already_dispatched")).toHaveLength(9);
	});

	it("concurrent dispatch attempts perform exactly one physical call", async () => {
		const journal = new FakeDispatchJournal();
		let n = 0;
		let release!: () => void;
		const gate = new Promise<void>((r) => {
			release = r;
		});
		const physical = async () => {
			n += 1;
			await gate;
			return "ok";
		};
		const executor = new Slice3RetainedDispatchExecutor(journal);
		const p1 = executor.dispatchOnce(BINDING, physical);
		const p2 = executor.dispatchOnce(BINDING, physical);
		release();
		const [o1, o2] = await Promise.all([p1, p2]);
		expect(n).toBe(1);
		const outcomes = [o1.outcome, o2.outcome].sort();
		expect(outcomes).toEqual(["already_dispatched", "dispatched"]);
	});

	it("provider error still counts as one dispatch and never relaunches", async () => {
		const journal = new FakeDispatchJournal();
		let n = 0;
		const boom = async () => {
			n += 1;
			throw new Error("provider 500");
		};
		const first = await new Slice3RetainedDispatchExecutor(journal).dispatchOnce(BINDING, boom);
		expect(first.outcome).toBe("provider_error");
		expect(n).toBe(1);
		expect(journal.providerEntered).toBe(true);
		// A retry after a provider error must not call the provider again.
		const retry = await new Slice3RetainedDispatchExecutor(journal).dispatchOnce(BINDING, boom);
		expect(retry.outcome).toBe("already_dispatched");
		expect(n).toBe(1);
	});

	it("refuses to dispatch when not armed (no physical call)", async () => {
		const journal = new FakeDispatchJournal();
		journal.captureArmed = false;
		const spy = spyCall();
		const out = await new Slice3RetainedDispatchExecutor(journal).dispatchOnce(BINDING, spy.fn);
		expect(out).toEqual({ outcome: "not_ready", reason: "not_armed" });
		expect(spy.calls()).toBe(0);
		expect(journal.dispatching).toBe(false);
	});

	it("reports claim_lost without a physical call when the atomic claim is lost (TOCTOU)", async () => {
		// readDispatchFacts reports an unclaimed outbox, but a concurrent writer
		// claims it before this executor's claim lands.
		const spy = spyCall();
		const racing: Slice3DispatchJournal = {
			readDispatchFacts: () => ({
				captureArmed: true,
				outboxUnclaimed: true,
				hasDispatching: false,
				hasProviderEntered: false,
			}),
			claimAndCommitDispatching: () => ({ ok: false, code: "OUTBOX_ALREADY_CLAIMED" }),
			commitProviderEntered: () => {
				throw new Error("should not be called");
			},
		};
		const out = await new Slice3RetainedDispatchExecutor(racing).dispatchOnce(BINDING, spy.fn);
		expect(out).toEqual({ outcome: "claim_lost", code: "OUTBOX_ALREADY_CLAIMED" });
		expect(spy.calls()).toBe(0);
	});
});
