// `proper-lockfile` for the golden generator: one process, no contention.
export function lockSync(): () => void {
	return () => undefined;
}
