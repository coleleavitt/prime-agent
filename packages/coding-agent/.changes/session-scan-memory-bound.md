Keep oversized session scans inside the retained-usage memory bound.

The session scan evicts a file's accumulated state once retained usage entries pass the
bound, so that file pays a full rescan next time. The persisted catalog cache was then
populated unconditionally, which answered the next read warm and kept that session's info
alive, full search corpus included, however far past the bound it was. The cache is now
filled only for scans whose state survived eviction.
