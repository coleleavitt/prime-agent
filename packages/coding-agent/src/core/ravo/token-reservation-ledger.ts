/**
 * Admission ledger for the RAVO token budget.
 *
 * The controller reserves a floor of tokens before it starts a child call and
 * settles the actual usage when the child returns. The ledger is pure and
 * synchronous: it never touches the clock, the network, or the archive, so the
 * budget arithmetic can be exercised exhaustively in isolation.
 *
 * Invariants (each is asserted by `test/ravo-token-reservation-ledger.test.ts`):
 *
 * 1. The gate admits an estimate only when `spent + reserved + estimate <= budget`.
 *    There is no saturating subtraction: once the ledger is over-committed, even a
 *    zero estimate is refused. (A saturating `budget - (spent + reserved)` reads as
 *    `0` and would admit `0 <= 0`; a Rocq proof found exactly that hole in an
 *    earlier ledger, so the comparison is written on the sum, not the difference.)
 * 2. Two admissions that both pass reserved their estimates against the same
 *    `remaining()` because the gate takes its reservation synchronously.
 * 3. `release` after `gate` restores the previous snapshot exactly.
 * 4. `settle` may record more than the estimate (the reservation is only an
 *    admission floor; a well-behaved child spends at most `available`, which it
 *    receives as its token budget) but spend is monotone and an actual that is
 *    not a non-negative safe integer is rejected before any state changes.
 * 5. `remaining()` after settling a reservation is never larger than it was before
 *    the reservation was admitted.
 */

export interface TokenReservationAdmission {
	readonly ok: true;
	readonly reservationId: number;
	/** The estimate this reservation holds until it is settled or released. */
	readonly estimate: number;
	/**
	 * Tokens that were unclaimed when the reservation was admitted, including
	 * this reservation's own estimate. It is the ceiling a well-behaved child may
	 * spend without pushing the ledger past its budget.
	 */
	readonly available: number;
}
export interface TokenReservationRefusal {
	readonly ok: false;
	readonly reason: string;
}
export type TokenReservationGate = TokenReservationAdmission | TokenReservationRefusal;

export interface TokenReservationLedgerSnapshot {
	readonly budget: number;
	readonly spent: number;
	readonly reserved: number;
	/** `budget - spent - reserved`; negative once a child overspent its ceiling. */
	readonly remaining: number;
	readonly openReservations: number;
}

export class TokenReservationLedger {
	readonly budget: number;
	#spent: number;
	#reserved = 0;
	#nextId = 1;
	readonly #open = new Map<number, number>();

	/**
	 * @param budget total tokens the run may spend.
	 * @param spent tokens already recorded (used to rebuild the ledger from a
	 * checkpoint; may exceed `budget` if an earlier child overspent, in which case
	 * every further admission is refused).
	 */
	constructor(budget: number, spent = 0) {
		if (!Number.isSafeInteger(budget) || budget < 0)
			throw new RangeError("token budget must be a non-negative safe integer");
		if (!Number.isSafeInteger(spent) || spent < 0)
			throw new RangeError("spent tokens must be a non-negative safe integer");
		this.budget = budget;
		this.#spent = spent;
	}

	/** Admit `estimate` tokens if the sum of spend, open reservations and the estimate fits the budget. */
	gate(estimate: number): TokenReservationGate {
		if (!Number.isSafeInteger(estimate) || estimate < 0)
			throw new RangeError("token estimate must be a non-negative safe integer");
		const committed = this.#spent + this.#reserved;
		if (committed + estimate > this.budget)
			return {
				ok: false,
				reason: `reservation of ${estimate} exceeds remaining budget (${this.budget - committed} of ${this.budget})`,
			};
		const available = this.budget - committed;
		const reservationId = this.#nextId++;
		this.#open.set(reservationId, estimate);
		this.#reserved += estimate;
		return { ok: true, reservationId, estimate, available };
	}

	/** Release the reservation and record what the child actually spent. */
	settle(reservationId: number, actualTokens: number): void {
		if (!Number.isSafeInteger(actualTokens) || actualTokens < 0)
			throw new RangeError("actual token usage must be a non-negative safe integer");
		const estimate = this.#take(reservationId);
		this.#reserved -= estimate;
		this.#spent += actualTokens;
	}

	/** Release the reservation without recording any spend. */
	release(reservationId: number): void {
		this.#reserved -= this.#take(reservationId);
	}

	/** Tokens neither spent nor reserved. Negative once a child overspent its ceiling. */
	remaining(): number {
		return this.budget - this.#spent - this.#reserved;
	}

	get spent(): number {
		return this.#spent;
	}

	snapshot(): TokenReservationLedgerSnapshot {
		return {
			budget: this.budget,
			spent: this.#spent,
			reserved: this.#reserved,
			remaining: this.remaining(),
			openReservations: this.#open.size,
		};
	}

	#take(reservationId: number): number {
		const estimate = this.#open.get(reservationId);
		if (estimate === undefined) throw new Error(`unknown or already closed token reservation ${reservationId}`);
		this.#open.delete(reservationId);
		return estimate;
	}
}
