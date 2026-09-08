import { getLogger } from "@earendil-works/pi-ai";
import { shutdownInstalledOtlpExporter } from "./otlp-export.js";

const log = getLogger("process");

export type FatalCrashKind = "uncaught_exception" | "unhandled_rejection" | "top_level_rejection";

export function fatalCrashFields(kind: FatalCrashKind, error: unknown): Record<string, unknown> {
	if (error instanceof Error) {
		return {
			kind,
			errorName: error.name,
			errorMessage: error.message,
			stack: error.stack,
		};
	}
	return { kind, errorMessage: String(error) };
}

export function reportFatalCrash(kind: FatalCrashKind, error: unknown): void {
	log.error("fatal_crash", fatalCrashFields(kind, error));
}

/** Install last-resort handlers for client processes. Returns a cleanup function for tests/embedders. */
export function installFatalCrashHandlers(): () => void {
	let handling = false;
	const terminate = (kind: FatalCrashKind, error: unknown) => {
		if (handling) return;
		handling = true;
		reportFatalCrash(kind, error);
		console.error(error instanceof Error ? (error.stack ?? error.message) : String(error));
		void shutdownInstalledOtlpExporter().finally(() => process.exit(1));
	};
	const onException = (error: Error) => terminate("uncaught_exception", error);
	const onRejection = (reason: unknown) => terminate("unhandled_rejection", reason);
	process.on("uncaughtException", onException);
	process.on("unhandledRejection", onRejection);
	return () => {
		process.off("uncaughtException", onException);
		process.off("unhandledRejection", onRejection);
	};
}
