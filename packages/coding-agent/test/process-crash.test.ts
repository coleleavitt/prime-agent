import { describe, expect, it } from "vitest";
import { fatalCrashFields } from "../src/core/process-crash.js";

describe("fatal crash diagnostics", () => {
	it("serializes errors with a stable structured shape", () => {
		const error = new TypeError("broken");
		expect(fatalCrashFields("top_level_rejection", error)).toMatchObject({
			kind: "top_level_rejection",
			errorName: "TypeError",
			errorMessage: "broken",
			stack: expect.stringContaining("TypeError: broken"),
		});
	});

	it("serializes non-Error rejection reasons", () => {
		expect(fatalCrashFields("unhandled_rejection", "nope")).toEqual({
			kind: "unhandled_rejection",
			errorMessage: "nope",
		});
	});
});
