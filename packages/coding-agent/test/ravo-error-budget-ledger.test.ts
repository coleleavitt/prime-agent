import { describe, expect, it } from "vitest";
import { ErrorBudgetLedger, Rational } from "../src/core/ravo/index.js";

const r = (numerator: bigint | number, denominator: bigint | number = 1) => Rational.of(numerator, denominator);

function calibration(id: string, evaluatorId: string, historyKey: string, bound: Rational) {
	return {
		id,
		evaluatorId,
		historyKey,
		falsePassUpperBound: bound,
		basis: `conditional validation for ${historyKey}`,
	};
}

describe("RAVO adaptive error-budget ledger", () => {
	it("mirrors a geometric recurrence and certifies its union-bound sum", () => {
		const ledger = new ErrorBudgetLedger(r(1, 4));
		for (let step = 0; step < 3; step++) {
			const delta = r(1, 2 ** (step + 3));
			const id = `c${step}`;
			ledger.registerCalibration(calibration(id, "judge", `h${step}`, delta));
			ledger.allocate({
				decisionId: `d${step}`,
				evaluatorId: "judge",
				historyKey: `h${step}`,
				calibrationId: id,
				delta,
			});
			ledger.recordEvaluation({ decisionId: `d${step}`, passed: true, calibrationId: id });
		}
		const certificate = ledger.certificate();
		expect(certificate.allocatedDelta.toString()).toBe("7/32");
		expect(certificate.spentDelta.toString()).toBe("7/32");
		expect(certificate.falsePassRiskBoundAcrossHorizon.toString()).toBe("7/32");
		expect(certificate.familyDelta.toString()).toBe("1/4");
		expect(certificate.independenceAssumed).toBe(false);
		expect(certificate.notAConvergenceClaim).toBe(true);
	});

	it("requires calibration for the actual adaptive history", () => {
		const ledger = new ErrorBudgetLedger(r(1, 10));
		ledger.registerCalibration(calibration("left", "judge", "answer=left", r(1, 100)));
		expect(() =>
			ledger.allocate({
				decisionId: "adaptive",
				evaluatorId: "judge",
				historyKey: "answer=right",
				calibrationId: "left",
				delta: r(1, 100),
			}),
		).toThrow("does not apply");
		ledger.registerCalibration(calibration("right", "judge", "answer=right", r(1, 100)));
		ledger.allocate({
			decisionId: "adaptive",
			evaluatorId: "judge",
			historyKey: "answer=right",
			calibrationId: "right",
			delta: r(1, 100),
		});
	});

	it("rejects budget exhaustion before seeing a decision result", () => {
		const ledger = new ErrorBudgetLedger(r(1, 10));
		ledger.registerCalibration(calibration("a", "judge", "h1", r(3, 50)));
		ledger.registerCalibration(calibration("b", "judge", "h2", r(1, 20)));
		ledger.allocate({
			decisionId: "d1",
			evaluatorId: "judge",
			historyKey: "h1",
			calibrationId: "a",
			delta: r(3, 50),
		});
		expect(() =>
			ledger.allocate({
				decisionId: "d2",
				evaluatorId: "judge",
				historyKey: "h2",
				calibrationId: "b",
				delta: r(1, 20),
			}),
		).toThrow("exhausted");
		expect(ledger.allocatedDelta.toString()).toBe("3/50");
	});

	it("does not probabilistically accept an uncalibrated evaluator pass", () => {
		const ledger = new ErrorBudgetLedger(r(1, 10));
		ledger.registerCalibration(calibration("expected", "judge", "h", r(1, 100)));
		ledger.allocate({
			decisionId: "d",
			evaluatorId: "judge",
			historyKey: "h",
			calibrationId: "expected",
			delta: r(1, 100),
		});
		const missing = ledger.recordEvaluation({ decisionId: "d", passed: true });
		expect(missing).toMatchObject({
			passed: true,
			probabilisticallyAccepted: false,
			rejectionReason: "uncalibrated",
		});
		expect(ledger.spentDelta.toString()).toBe("1/100");
	});

	it("validates exact rational and probability ranges", () => {
		expect(Rational.of(2, 4).toString()).toBe("1/2");
		expect(() => Rational.of(1, 0)).toThrow("denominator");
		expect(() => new ErrorBudgetLedger(r(101, 100))).toThrow("[0, 1]");
		const ledger = new ErrorBudgetLedger(r(1, 2));
		expect(() => ledger.registerCalibration(calibration("bad", "judge", "h", r(-1, 10)))).toThrow("[0, 1]");
	});

	it("serializes deterministically with fixed rational strings", () => {
		const build = () => {
			const ledger = new ErrorBudgetLedger(r(1, 5));
			ledger.registerCalibration(calibration("c", "judge", "h", r(1, 20)));
			ledger.allocate({
				decisionId: "d",
				evaluatorId: "judge",
				historyKey: "h",
				calibrationId: "c",
				delta: r(1, 20),
			});
			ledger.recordEvaluation({ decisionId: "d", passed: false, calibrationId: "c" });
			return ledger.serialize();
		};
		expect(build()).toBe(build());
		expect(JSON.parse(build())).toMatchObject({ familyDelta: "1/5", allocatedDelta: "1/20", spentDelta: "1/20" });
	});
});
