import { defineConfig } from "vitest/config";

export default defineConfig({
	test: {
		globals: true,
		environment: "node",
		testTimeout: 30000, // 30 seconds for API calls
		coverage: {
			provider: "v8",
			reporter: ["lcov"],
			reportsDirectory: process.env.COVERAGE_DIR ?? "coverage",
			include: ["src/**/*.ts"],
		},
	},
});
