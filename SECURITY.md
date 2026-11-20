# Security and dependency notes

These releases are pre-1.0 and have not received an independent security audit.

## Alloy LRU advisory scope

`RUSTSEC-2026-0253` concerns `lru::LruCache::pop` when a stored key's destructor
panics and execution subsequently continues after catching that panic. The
remaining `lru 0.16.4` copy comes only from `alloy-provider 1.6.3`. Its block
stream uses `BlockNumber` (`u64`) keys and its cache layer uses `B256` keys;
neither has a custom destructor. This dependency remains affected as a library,
but the key-destructor trigger is absent from these selected Alloy uses.
Reassess on an Alloy or key-type change. The fix is in lru 0.18.2, outside
Alloy's 0.16 dependency range; no registry patch is hidden in this release.

