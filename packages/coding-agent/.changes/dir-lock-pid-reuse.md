Stop a recycled pid from holding a lock whose owner has died.

`tryAcquireDirLock` judged an owner by pid alone, so once the owner exited and the OS handed
its pid to an unrelated process, the lock read as held. For the kernel bootstrap lock that
meant waiting out the bounded timeout and failing; for the supervisor launch lock it meant a
worker could silently never relaunch a supervisor that had actually died.

A lock now records the owner's process start identity beside its pid, and both callers judge
the pair through `isProcessIdentityAlive`, the check session leases already used. It fails
safe toward alive when no identity was recorded or the current one cannot be read. The pid
and identity are newline-separated because the macOS and BSD identity is `ps` lstart output
and contains spaces; a pid-only lock written by older code is still read.
