import { describe, expect, it } from "vitest";
import {
	type TokenReservationAdmission,
	TokenReservationLedger,
	type TokenReservationLedgerSnapshot,
} from "../src/core/ravo/token-reservation-ledger.js";

/** Deterministic PRNG (mulberry32) so a failing sequence can be replayed from its seed. */
function rng(seed: number): () => number {
	let state = seed >>> 0;
	return () => {
		state = (state + 0x6d2b79f5) >>> 0;
		let t = state;
		t = Math.imul(t ^ (t >>> 15), t | 1);
		t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
		return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
	};
}
function pick(random: () => number, max: number): number {
	return Math.floor(random() * (max + 1));
}

/** Reference model: the invariants every reachable ledger state must satisfy. */
function assertInvariants(ledger: TokenReservationLedger, snapshot: TokenReservationLedgerSnapshot): void {
	expect(snapshot.budget).toBe(ledger.budget);
	expect(Number.isSafeInteger(snapshot.spent) && snapshot.spent >= 0).toBe(true);
	expect(Number.isSafeInteger(snapshot.reserved) && snapshot.reserved >= 0).toBe(true);
	expect(snapshot.remaining).toBe(snapshot.budget - snapshot.spent - snapshot.reserved);
	expect(ledger.remaining()).toBe(snapshot.remaining);
	expect(ledger.spent).toBe(snapshot.spent);
	if (snapshot.openReservations === 0) expect(snapshot.reserved).toBe(0);
}

describe("TokenReservationLedger", () => {
	it("refuses a zero estimate on an over-committed ledger (the saturating-subtraction hole)", () => {
		// jfc's ledger.rs computed `remaining = budget.saturating_sub(spent + reserved)`
		// and admitted `estimate <= remaining`; with spent > budget that reads 0 <= 0.
		const ledger = new TokenReservationLedger(10);
		const first = ledger.gate(10);
		expect(first.ok).toBe(true);
		if (!first.ok) throw new Error("unreachable");
		ledger.settle(first.reservationId, 25);
		expect(ledger.snapshot()).toMatchObject({ spent: 25, reserved: 0, remaining: -15, openReservations: 0 });
		expect(ledger.gate(0)).toMatchObject({ ok: false });
		expect(ledger.gate(1)).toMatchObject({ ok: false });
		// The same hole from reservations alone: a rebuilt ledger already over budget.
		const resumed = new TokenReservationLedger(10, 11);
		expect(resumed.gate(0)).toMatchObject({ ok: false });
		// And exactly at the boundary the sum comparison still admits.
		const exact = new TokenReservationLedger(10, 10);
		expect(exact.gate(0)).toMatchObject({ ok: true, available: 0 });
		expect(exact.gate(1)).toMatchObject({ ok: false });
	});

	it("closes the TOCTOU window: two passing gates fit together in the remaining budget", () => {
		const ledger = new TokenReservationLedger(10);
		const before = ledger.remaining();
		const a = ledger.gate(6);
		const b = ledger.gate(4);
		const c = ledger.gate(1);
		expect(a.ok && b.ok).toBe(true);
		expect(c).toMatchObject({ ok: false });
		if (!a.ok || !b.ok) throw new Error("unreachable");
		expect(a.estimate + b.estimate).toBeLessThanOrEqual(before);
		expect(a.available).toBe(10);
		expect(b.available).toBe(4);
		expect(ledger.snapshot()).toMatchObject({ spent: 0, reserved: 10, remaining: 0, openReservations: 2 });
	});

	it("release after gate restores the snapshot exactly", () => {
		const ledger = new TokenReservationLedger(7, 2);
		const before = ledger.snapshot();
		const admission = ledger.gate(3);
		if (!admission.ok) throw new Error("expected admission");
		expect(ledger.snapshot()).toEqual({
			...before,
			reserved: 3,
			remaining: before.remaining - 3,
			openReservations: 1,
		});
		ledger.release(admission.reservationId);
		expect(ledger.snapshot()).toEqual(before);
		expect(() => ledger.release(admission.reservationId)).toThrow(/unknown or already closed/);
		expect(() => ledger.settle(admission.reservationId, 1)).toThrow(/unknown or already closed/);
		expect(ledger.snapshot()).toEqual(before);
	});

	it("settles more than the estimate but never less than monotone, and fails closed on bad actuals", () => {
		const ledger = new TokenReservationLedger(100);
		const admission = ledger.gate(5);
		if (!admission.ok) throw new Error("expected admission");
		for (const bad of [-1, 1.5, Number.NaN, Number.POSITIVE_INFINITY, Number.MAX_SAFE_INTEGER + 1]) {
			expect(() => ledger.settle(admission.reservationId, bad)).toThrow(RangeError);
			// A rejected settle leaves the reservation open and the spend untouched.
			expect(ledger.snapshot()).toMatchObject({ spent: 0, reserved: 5, openReservations: 1 });
		}
		ledger.settle(admission.reservationId, 40);
		expect(ledger.snapshot()).toMatchObject({ spent: 40, reserved: 0, remaining: 60, openReservations: 0 });
		const second = ledger.gate(5);
		if (!second.ok) throw new Error("expected admission");
		ledger.settle(second.reservationId, 0);
		expect(ledger.spent).toBe(40);
	});

	it("validates its constructor and estimates", () => {
		for (const bad of [-1, 0.5, Number.NaN, Number.MAX_SAFE_INTEGER + 1]) {
			expect(() => new TokenReservationLedger(bad)).toThrow(RangeError);
			expect(() => new TokenReservationLedger(10, bad)).toThrow(RangeError);
			expect(() => new TokenReservationLedger(10).gate(bad)).toThrow(RangeError);
		}
		expect(new TokenReservationLedger(0).gate(0)).toMatchObject({ ok: true, available: 0 });
	});

	it("holds every invariant across random operation sequences", () => {
		const seeds = Array.from({ length: 200 }, (_, i) => i + 1);
		for (const seed of seeds) {
			const random = rng(seed);
			const budget = pick(random, 50);
			const ledger = new TokenReservationLedger(budget, pick(random, 5));
			const open: TokenReservationAdmission[] = [];
			let previous = ledger.snapshot();
			assertInvariants(ledger, previous);
			for (let step = 0; step < 60; step += 1) {
				const op = random();
				if (op < 0.45 || open.length === 0) {
					const estimate = pick(random, 12);
					const before = ledger.snapshot();
					const gate = ledger.gate(estimate);
					const after = ledger.snapshot();
					if (before.spent + before.reserved + estimate > budget) {
						// Invariant 1: refused, and refusal is a no-op.
						expect(gate.ok, `seed ${seed} step ${step}`).toBe(false);
						expect(after).toEqual(before);
					} else {
						expect(gate.ok, `seed ${seed} step ${step}`).toBe(true);
						if (!gate.ok) throw new Error("unreachable");
						expect(gate.available).toBe(before.remaining);
						expect(gate.estimate).toBe(estimate);
						expect(after.spent + after.reserved).toBeLessThanOrEqual(budget);
						expect(after.reserved).toBe(before.reserved + estimate);
						expect(after.openReservations).toBe(before.openReservations + 1);
						open.push(gate);
					}
				} else if (op < 0.6) {
					// Invariant 3: release ∘ gate = identity, checked against a fresh gate.
					const before = ledger.snapshot();
					const gate = ledger.gate(pick(random, 12));
					if (gate.ok) ledger.release(gate.reservationId);
					expect(ledger.snapshot(), `seed ${seed} step ${step}`).toEqual(before);
				} else {
					const index = pick(random, open.length - 1);
					const admission = open.splice(index, 1)[0];
					if (!admission) throw new Error("unreachable");
					const before = ledger.snapshot();
					if (random() < 0.2) {
						ledger.release(admission.reservationId);
						const after = ledger.snapshot();
						expect(after.spent).toBe(before.spent);
						expect(after.reserved).toBe(before.reserved - admission.estimate);
					} else {
						// A well-behaved child spends at most `available`, an over-eager one
						// may exceed it; both are recorded truthfully (invariant 4).
						const actual = random() < 0.85 ? pick(random, admission.available) : pick(random, 80);
						ledger.settle(admission.reservationId, actual);
						const after = ledger.snapshot();
						expect(after.spent).toBe(before.spent + actual);
						expect(after.reserved).toBe(before.reserved - admission.estimate);
						// Invariant 5: settling never returns more than the reservation took, so
						// `remaining()` minus the released floor is non-increasing across settles.
						expect(after.remaining, `seed ${seed} step ${step}`).toBeLessThanOrEqual(
							before.remaining + admission.estimate,
						);
						// Unspent tokens (`budget - spent`) are monotone non-increasing across settles.
						expect(after.remaining + after.reserved).toBeLessThanOrEqual(before.remaining + before.reserved);
					}
				}
				const snapshot = ledger.snapshot();
				assertInvariants(ledger, snapshot);
				// Invariant 4/5: spend is monotone; a rejected gate never changes state.
				expect(snapshot.spent, `seed ${seed} step ${step}`).toBeGreaterThanOrEqual(previous.spent);
				expect(snapshot.openReservations).toBe(open.length);
				expect(snapshot.reserved).toBe(open.reduce((sum, a) => sum + a.estimate, 0));
				// Invariant 1 restated for every state: whenever a reservation is open, the
				// gate that admitted it saw the sum fit; the ledger never admits past budget.
				expect(ledger.gate(Math.max(0, budget - snapshot.spent - snapshot.reserved + 1)).ok).toBe(false);
				previous = snapshot;
			}
		}
	});

	it("keeps spent + reserved within budget whenever children honor their ceiling", () => {
		for (const seed of Array.from({ length: 100 }, (_, i) => 1000 + i)) {
			const random = rng(seed);
			const budget = pick(random, 40);
			const ledger = new TokenReservationLedger(budget);
			const open: TokenReservationAdmission[] = [];
			for (let step = 0; step < 40; step += 1) {
				if (random() < 0.5 || open.length === 0) {
					const gate = ledger.gate(pick(random, 10));
					if (gate.ok) open.push(gate);
				} else {
					const admission = open.splice(pick(random, open.length - 1), 1)[0];
					if (!admission) throw new Error("unreachable");
					// Concurrent children share `available`; each stays within the unclaimed
					// tokens at the moment it settles plus its own reservation.
					const unclaimed = ledger.remaining() + admission.estimate;
					ledger.settle(admission.reservationId, pick(random, Math.min(admission.available, unclaimed)));
				}
				const snapshot = ledger.snapshot();
				expect(snapshot.spent + snapshot.reserved, `seed ${seed} step ${step}`).toBeLessThanOrEqual(budget);
				expect(snapshot.remaining).toBeGreaterThanOrEqual(0);
			}
		}
	});
});
