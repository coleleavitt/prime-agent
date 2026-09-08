import { startSpan } from "@earendil-works/pi-ai";
import { installOtlpExporterFromEnv } from "../../src/core/otlp-export.js";

const installed = installOtlpExporterFromEnv({ version: "fixture-version" });
if (!installed) throw new Error("OTLP exporter was not installed");
startSpan(process.argv[2] ?? "fixture-span").end();

let shuttingDown = false;
async function shutdown(): Promise<void> {
	if (shuttingDown) return;
	shuttingDown = true;
	await installed?.shutdown();
}

let keepAlive: NodeJS.Timeout | undefined;
for (const signal of ["SIGINT", "SIGTERM"] as NodeJS.Signals[]) {
	process.once(signal, () => {
		if (keepAlive) clearInterval(keepAlive);
		void shutdown().then(() => process.exit(0));
	});
}

if (process.argv.includes("--wait-for-signal")) {
	keepAlive = setInterval(() => {}, 60_000);
	console.log("READY");
} else {
	await shutdown();
}
