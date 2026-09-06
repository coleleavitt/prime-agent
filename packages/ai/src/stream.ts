import "./providers/register-builtins.js";

import { getApiProvider } from "./api-registry.js";
import { runWithTraceContext, type SpanAttributes, startSpan } from "./trace-context.js";
import type {
	Api,
	AssistantMessage,
	AssistantMessageEventStream,
	Context,
	Model,
	ProviderStreamOptions,
	SimpleStreamOptions,
	StreamOptions,
} from "./types.js";

export { getEnvApiKey } from "./env-api-keys.js";

function resolveApiProvider(api: Api) {
	const provider = getApiProvider(api);
	if (!provider) {
		throw new Error(`No API provider registered for api: ${api}`);
	}
	return provider;
}

function llmRequestAttrs<TApi extends Api>(model: Model<TApi>): SpanAttributes {
	return {
		"llm.provider": model.provider,
		"llm.api": model.api,
		"llm.model": model.id,
		"llm.base_url": model.baseUrl,
	};
}

/**
 * Run a provider call inside an `llm.request` span. The span is the active
 * context while the provider builds and issues the request (so provider log
 * lines inherit the ids) and it ends when the stream settles.
 */
function tracedProviderCall<TApi extends Api>(
	model: Model<TApi>,
	call: () => AssistantMessageEventStream,
): AssistantMessageEventStream {
	const span = startSpan("llm.request", llmRequestAttrs(model));
	let result: AssistantMessageEventStream;
	try {
		result = runWithTraceContext(span.context, call);
	} catch (error) {
		span.recordError(error);
		span.end();
		throw error;
	}
	result.result().then(
		(message) => {
			span.setAttributes({
				"llm.stop_reason": message.stopReason,
				"llm.usage.input": message.usage?.input,
				"llm.usage.output": message.usage?.output,
			});
			if (message.stopReason === "error") span.recordError(message.errorMessage ?? "provider error");
			span.end();
		},
		(error: unknown) => {
			span.recordError(error);
			span.end();
		},
	);
	return result;
}

export function stream<TApi extends Api>(
	model: Model<TApi>,
	context: Context,
	options?: ProviderStreamOptions,
): AssistantMessageEventStream {
	const provider = resolveApiProvider(model.api);
	return tracedProviderCall(model, () => provider.stream(model, context, options as StreamOptions));
}

export async function complete<TApi extends Api>(
	model: Model<TApi>,
	context: Context,
	options?: ProviderStreamOptions,
): Promise<AssistantMessage> {
	const s = stream(model, context, options);
	return s.result();
}

export function streamSimple<TApi extends Api>(
	model: Model<TApi>,
	context: Context,
	options?: SimpleStreamOptions,
): AssistantMessageEventStream {
	const provider = resolveApiProvider(model.api);
	return tracedProviderCall(model, () => provider.streamSimple(model, context, options));
}

export async function completeSimple<TApi extends Api>(
	model: Model<TApi>,
	context: Context,
	options?: SimpleStreamOptions,
): Promise<AssistantMessage> {
	const s = streamSimple(model, context, options);
	return s.result();
}
