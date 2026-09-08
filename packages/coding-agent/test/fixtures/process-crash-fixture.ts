import { installFileLogSink } from "../../src/core/logging.js";
import { installFatalCrashHandlers } from "../../src/core/process-crash.js";

installFileLogSink({ fixture: "process-crash" });
installFatalCrashHandlers();

const kind = process.argv[2];
if (kind === "uncaughtException") {
	setImmediate(() => {
		throw new Error("fixture uncaught exception");
	});
} else if (kind === "unhandledRejection") {
	void Promise.reject(new Error("fixture unhandled rejection"));
} else {
	throw new Error(`unknown crash fixture kind: ${kind}`);
}
