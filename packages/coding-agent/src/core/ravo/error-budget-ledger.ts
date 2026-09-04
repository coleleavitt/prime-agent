import { probability, Rational } from "./rational.js";

/**
 * A conditional calibration assumption for one evaluator at one observable history.
 *
 * `falsePassUpperBound` means P(evaluator passes an incorrect candidate | history)
 * is at most this value. It is an assumption supplied by the caller, not something
 * this ledger estimates or proves. A separate record is required for each history.
 */
export interface CalibrationAssumption {
	id: string;
	evaluatorId: string;
	historyKey: string;
	falsePassUpperBound: Rational;
	basis: string;
}

export interface DecisionAllocation {
	decisionId: string;
	historyKey: string;
	evaluatorId: string;
	calibrationId: string;
	delta: Rational;
}

export interface EvaluatorResult {
	decisionId: string;
	passed: boolean;
	calibrationId?: string;
}

export interface EvaluationRecord {
	decisionId: string;
	passed: boolean;
	probabilisticallyAccepted: boolean;
	deltaSpent: Rational;
	calibrationId?: string;
	rejectionReason?: "uncalibrated";
}

export interface ErrorBudgetLedgerSnapshot {
	schemaVersion: 1;
	familyDelta: string;
	allocatedDelta: string;
	spentDelta: string;
	calibrations: Array<{
		id: string;
		evaluatorId: string;
		historyKey: string;
		falsePassUpperBound: string;
		basis: string;
	}>;
	allocations: Array<{
		decisionId: string;
		historyKey: string;
		evaluatorId: string;
		calibrationId: string;
		delta: string;
	}>;
	evaluations: Array<{
		decisionId: string;
		passed: boolean;
		probabilisticallyAccepted: boolean;
		deltaSpent: string;
		calibrationId?: string;
		rejectionReason?: "uncalibrated";
	}>;
}

export interface UnionBoundCertificate {
	method: "adaptive-union-bound";
	independenceAssumed: false;
	familyDelta: Rational;
	allocatedDelta: Rational;
	spentDelta: Rational;
	falsePassRiskBoundAcrossHorizon: Rational;
	allocationWithinFamily: true;
	claim: string;
	notAConvergenceClaim: true;
}

/**
 * Tracks a family-wise false-pass budget for adaptively selected evaluations.
 *
 * The certificate uses only a union bound over conditional, history-specific
 * calibration assumptions. It deliberately makes no independence claim. Budget
 * allocation must happen before an evaluator result is recorded.
 */
export class ErrorBudgetLedger {
	readonly familyDelta: Rational;
	readonly #calibrations = new Map<string, CalibrationAssumption>();
	readonly #allocations = new Map<string, DecisionAllocation>();
	readonly #evaluations: EvaluationRecord[] = [];
	#allocatedDelta = Rational.of(0);
	#spentDelta = Rational.of(0);

	constructor(familyDelta: Rational) {
		this.familyDelta = probability(familyDelta, "familyDelta");
	}

	static deserialize(serialized: string): ErrorBudgetLedger {
		let value: unknown;
		try {
			value = JSON.parse(serialized);
		} catch {
			throw new TypeError("invalid error-budget ledger JSON");
		}
		return ErrorBudgetLedger.fromSnapshot(value);
	}

	static fromSnapshot(value: unknown): ErrorBudgetLedger {
		const snapshot = parseSnapshot(value);
		const ledger = new ErrorBudgetLedger(Rational.parse(snapshot.familyDelta));
		for (const calibration of snapshot.calibrations) {
			ledger.registerCalibration({
				...calibration,
				falsePassUpperBound: Rational.parse(calibration.falsePassUpperBound),
			});
		}
		for (const allocation of snapshot.allocations) {
			ledger.allocate({ ...allocation, delta: Rational.parse(allocation.delta) });
		}
		for (const evaluation of snapshot.evaluations) {
			const record = ledger.recordEvaluation({
				decisionId: evaluation.decisionId,
				passed: evaluation.passed,
				...(evaluation.calibrationId === undefined ? {} : { calibrationId: evaluation.calibrationId }),
			});
			if (
				record.probabilisticallyAccepted !== evaluation.probabilisticallyAccepted ||
				record.deltaSpent.toString() !== evaluation.deltaSpent ||
				record.rejectionReason !== evaluation.rejectionReason
			) {
				throw new TypeError("error-budget evaluation record is inconsistent");
			}
		}
		if (
			ledger.allocatedDelta.toString() !== snapshot.allocatedDelta ||
			ledger.spentDelta.toString() !== snapshot.spentDelta
		) {
			throw new TypeError("error-budget totals are inconsistent");
		}
		return ledger;
	}

	restore(value: unknown): void {
		const restored = ErrorBudgetLedger.fromSnapshot(value);
		if (restored.familyDelta.toString() !== this.familyDelta.toString()) {
			throw new Error("error-budget familyDelta mismatch");
		}
		this.#calibrations.clear();
		this.#allocations.clear();
		this.#evaluations.length = 0;
		for (const [id, assumption] of restored.#calibrations) this.#calibrations.set(id, assumption);
		for (const [id, allocation] of restored.#allocations) this.#allocations.set(id, allocation);
		this.#evaluations.push(...restored.#evaluations);
		this.#allocatedDelta = restored.#allocatedDelta;
		this.#spentDelta = restored.#spentDelta;
	}

	get allocatedDelta(): Rational {
		return this.#allocatedDelta;
	}
	get spentDelta(): Rational {
		return this.#spentDelta;
	}

	registerCalibration(assumption: CalibrationAssumption): void {
		requireNonEmpty(assumption.id, "calibration id");
		requireNonEmpty(assumption.evaluatorId, "evaluator id");
		requireNonEmpty(assumption.historyKey, "history key");
		requireNonEmpty(assumption.basis, "calibration basis");
		probability(assumption.falsePassUpperBound, "falsePassUpperBound");
		if (this.#calibrations.has(assumption.id)) throw new Error(`duplicate calibration: ${assumption.id}`);
		this.#calibrations.set(assumption.id, Object.freeze({ ...assumption }));
	}

	allocate(allocation: DecisionAllocation): void {
		requireNonEmpty(allocation.decisionId, "decision id");
		const delta = probability(allocation.delta, "decision delta");
		if (delta.isZero()) throw new RangeError("decision delta must be greater than zero");
		if (this.#allocations.has(allocation.decisionId)) throw new Error(`duplicate decision: ${allocation.decisionId}`);
		const calibration = this.#calibrations.get(allocation.calibrationId);
		if (!calibration) throw new Error(`unknown calibration: ${allocation.calibrationId}`);
		if (calibration.evaluatorId !== allocation.evaluatorId || calibration.historyKey !== allocation.historyKey) {
			throw new Error("calibration does not apply to this evaluator and history");
		}
		if (!calibration.falsePassUpperBound.lte(delta)) {
			throw new Error("decision delta is smaller than the calibration false-pass bound");
		}
		const next = this.#allocatedDelta.add(delta);
		if (!next.lte(this.familyDelta)) throw new Error("family error budget exhausted");
		this.#allocations.set(allocation.decisionId, Object.freeze({ ...allocation }));
		this.#allocatedDelta = next;
	}

	recordEvaluation(result: EvaluatorResult): EvaluationRecord {
		if (this.#evaluations.some((record) => record.decisionId === result.decisionId)) {
			throw new Error(`evaluation already recorded: ${result.decisionId}`);
		}
		const allocation = this.#allocations.get(result.decisionId);
		if (!allocation) throw new Error(`decision was not preallocated: ${result.decisionId}`);
		const calibrated = result.calibrationId === allocation.calibrationId;
		const record: EvaluationRecord = Object.freeze({
			decisionId: result.decisionId,
			passed: result.passed,
			probabilisticallyAccepted: result.passed && calibrated,
			deltaSpent: allocation.delta,
			...(result.calibrationId === undefined ? {} : { calibrationId: result.calibrationId }),
			...(calibrated ? {} : { rejectionReason: "uncalibrated" as const }),
		});
		this.#spentDelta = this.#spentDelta.add(allocation.delta);
		this.#evaluations.push(record);
		return record;
	}

	certificate(): UnionBoundCertificate {
		return Object.freeze({
			method: "adaptive-union-bound",
			independenceAssumed: false,
			familyDelta: this.familyDelta,
			allocatedDelta: this.#allocatedDelta,
			spentDelta: this.#spentDelta,
			falsePassRiskBoundAcrossHorizon: this.#spentDelta,
			allocationWithinFamily: true,
			claim: "Given every recorded conditional calibration assumption, the probability of one or more false passes across the evaluated horizon is at most spentDelta by the union bound.",
			notAConvergenceClaim: true,
		});
	}

	toJSON(): ErrorBudgetLedgerSnapshot {
		return {
			schemaVersion: 1,
			familyDelta: this.familyDelta.toString(),
			allocatedDelta: this.#allocatedDelta.toString(),
			spentDelta: this.#spentDelta.toString(),
			calibrations: [...this.#calibrations.values()].map((value) => ({
				...value,
				falsePassUpperBound: value.falsePassUpperBound.toString(),
			})),
			allocations: [...this.#allocations.values()].map((value) => ({
				...value,
				delta: value.delta.toString(),
			})),
			evaluations: this.#evaluations.map((value) => ({
				...value,
				deltaSpent: value.deltaSpent.toString(),
			})),
		};
	}

	serialize(): string {
		return JSON.stringify(this);
	}
}

function requireNonEmpty(value: string, name: string): void {
	if (value.trim() === "") throw new TypeError(`${name} must not be empty`);
}

function parseSnapshot(value: unknown): ErrorBudgetLedgerSnapshot {
	if (!isRecord(value) || value.schemaVersion !== 1) throw new TypeError("unsupported error-budget schema");
	for (const key of ["familyDelta", "allocatedDelta", "spentDelta"] as const) requireRational(value[key], key);
	if (!Array.isArray(value.calibrations) || !Array.isArray(value.allocations) || !Array.isArray(value.evaluations)) {
		throw new TypeError("invalid error-budget collections");
	}
	for (const item of value.calibrations) {
		if (!isRecord(item)) throw new TypeError("invalid calibration");
		for (const key of ["id", "evaluatorId", "historyKey", "basis"] as const) requireString(item[key], key);
		requireRational(item.falsePassUpperBound, "falsePassUpperBound");
	}
	for (const item of value.allocations) {
		if (!isRecord(item)) throw new TypeError("invalid allocation");
		for (const key of ["decisionId", "historyKey", "evaluatorId", "calibrationId"] as const)
			requireString(item[key], key);
		requireRational(item.delta, "delta");
	}
	for (const item of value.evaluations) {
		if (!isRecord(item)) throw new TypeError("invalid evaluation");
		requireString(item.decisionId, "decisionId");
		if (typeof item.passed !== "boolean" || typeof item.probabilisticallyAccepted !== "boolean")
			throw new TypeError("invalid evaluation flags");
		requireRational(item.deltaSpent, "deltaSpent");
		if (item.calibrationId !== undefined) requireString(item.calibrationId, "calibrationId");
		if (item.rejectionReason !== undefined && item.rejectionReason !== "uncalibrated")
			throw new TypeError("invalid rejectionReason");
	}
	return value as unknown as ErrorBudgetLedgerSnapshot;
}

function isRecord(value: unknown): value is Record<string, unknown> {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}
function requireString(value: unknown, name: string): asserts value is string {
	if (typeof value !== "string" || value.trim() === "") throw new TypeError(`${name} must be a non-empty string`);
}
function requireRational(value: unknown, name: string): asserts value is string {
	if (typeof value !== "string") throw new TypeError(`${name} must be a rational string`);
	Rational.parse(value);
}
