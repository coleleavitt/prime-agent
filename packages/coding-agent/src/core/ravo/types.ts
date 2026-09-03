export const RAVO_EVENT_TYPES = ["run", "proposal", "evaluation", "pressure", "accept", "reject", "stop"] as const;

export type RavoEventType = (typeof RAVO_EVENT_TYPES)[number];
export type JsonPrimitive = string | number | boolean | null;
export type JsonValue = JsonPrimitive | JsonValue[] | { [key: string]: JsonValue };

export interface RavoEvent {
	seq: number;
	type: RavoEventType;
	timestamp: string;
	payload: { [key: string]: JsonValue };
	prevDigest: string | null;
	digest: string;
}

export interface RavoState {
	revision: number;
	championDigest: string | null;
	eventCount: number;
	headDigest: string | null;
}

export interface ChampionCas {
	revision: number;
	championDigest: string | null;
}

export type RavoFaultPoint =
	| "after-log-write"
	| "after-log-fsync"
	| "after-state-temp-write"
	| "after-state-temp-fsync"
	| "after-state-rename"
	| "after-blob-temp-write"
	| "after-blob-temp-fsync"
	| "after-blob-rename";
