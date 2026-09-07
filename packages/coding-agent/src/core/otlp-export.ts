import { addSpanSink, createOtlpSpanExporter, type OtlpSpanExporter } from "@earendil-works/pi-ai";

export const OTLP_ENDPOINT_ENV = "OTEL_EXPORTER_OTLP_ENDPOINT";
export const OTLP_HEADERS_ENV = "OTEL_EXPORTER_OTLP_HEADERS";
export const DEFAULT_OTLP_SHUTDOWN_TIMEOUT_MS = 1_000;

export interface OtlpRuntimeEnvironment {
	[OTLP_ENDPOINT_ENV]?: string;
	[OTLP_HEADERS_ENV]?: string;
}

export interface InstalledOtlpExporter {
	exporter: OtlpSpanExporter;
	shutdown(): Promise<void>;
}

export interface InstallOtlpExporterOptions {
	version: string;
	env?: OtlpRuntimeEnvironment;
	shutdownTimeoutMs?: number;
	createExporter?: typeof createOtlpSpanExporter;
}

/** Parse the standard comma-separated `key=value` OTLP header environment variable. */
export function parseOtlpHeaders(value: string | undefined): Record<string, string> | undefined {
	if (!value?.trim()) return undefined;
	const headers: Record<string, string> = {};
	for (const item of value.split(",")) {
		const separator = item.indexOf("=");
		if (separator <= 0) continue;
		const key = item.slice(0, separator).trim();
		const encodedValue = item.slice(separator + 1).trim();
		let headerValue = encodedValue;
		try {
			headerValue = decodeURIComponent(encodedValue);
		} catch {
			// Preserve malformed values verbatim rather than failing startup.
		}
		if (key) headers[key] = headerValue;
	}
	return Object.keys(headers).length > 0 ? headers : undefined;
}

function boundedShutdown(exporter: OtlpSpanExporter, timeoutMs: number): Promise<void> {
	return new Promise((resolve) => {
		let settled = false;
		const finish = () => {
			if (settled) return;
			settled = true;
			clearTimeout(timer);
			resolve();
		};
		const timer = setTimeout(() => {
			exporter.abort();
			finish();
		}, timeoutMs);
		void exporter.shutdown().then(finish, finish);
	});
}

/**
 * Install the OTLP adapter only when `OTEL_EXPORTER_OTLP_ENDPOINT` is set.
 * Call this after the file log sink is installed so export remains additive.
 */
export function installOtlpExporterFromEnv(options: InstallOtlpExporterOptions): InstalledOtlpExporter | undefined {
	const env = options.env ?? process.env;
	const endpoint = env[OTLP_ENDPOINT_ENV]?.trim();
	if (!endpoint) return undefined;

	const createExporter = options.createExporter ?? createOtlpSpanExporter;
	const exporter = createExporter({
		endpoint,
		headers: parseOtlpHeaders(env[OTLP_HEADERS_ENV]),
		serviceName: "prime-agent",
		serviceVersion: options.version,
	});
	const unsubscribe = addSpanSink(exporter.sink);
	let shutdownPromise: Promise<void> | undefined;
	return {
		exporter,
		shutdown() {
			shutdownPromise ??= (async () => {
				unsubscribe();
				await boundedShutdown(exporter, options.shutdownTimeoutMs ?? DEFAULT_OTLP_SHUTDOWN_TIMEOUT_MS);
			})();
			return shutdownPromise;
		},
	};
}
