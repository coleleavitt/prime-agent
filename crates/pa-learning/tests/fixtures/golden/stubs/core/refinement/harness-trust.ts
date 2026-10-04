// The real `parseHarnessEntryRef` (the only value the trajectory index imports).
const ENTRY_REF_SEPARATOR = ":";
export function parseHarnessEntryRef(ref: string): { kind: string; id: string } | undefined {
	const separator = ref.indexOf(ENTRY_REF_SEPARATOR);
	if (separator <= 0 || separator === ref.length - 1) return undefined;
	return { kind: ref.slice(0, separator), id: ref.slice(separator + 1) };
}
