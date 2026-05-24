# cascade-runtime — AI Design Context

## Role

The shared-nothing thread-per-core executor. Glues the kernel-bypass I/O model (§3.1) to user-level scheduling.

## Pillar Reservations

- **§3.2:** the pillar this crate *is*.
- **§3.1:** each worker owns one `io_uring` ring. No cross-core ring sharing.
- **§6.1 (Tenant):** every task runs in a `TenantContext` (from `cascade-tenant`) so future quotas can throttle.

## Hard Invariants

1. **One worker, one core, one ring.** No work-stealing across cores in v1.
2. **No shared mutable state across workers.** Communication via channels or per-worker shards.
3. **CPU pinning is real** (`sched_setaffinity` on Linux).
4. **Connection sharding is deterministic** — a connection lives on exactly one worker for its lifetime.

## Out of Scope

- Connection migration across cores (would require shutting down the lock-free assumptions).
- Multi-machine clustering.

## Open Design Questions

- Runtime choice: `tokio-uring`, `glommio`, `monoio`. Current code uses `tokio-uring`. Glommio is more thread-per-core-native; monoio is newer. Worth a benchmark before committing.
- Connection sharding policy: round-robin, least-loaded, hash-on-DB. Hash-on-DB has nice locality properties for the BPM.
