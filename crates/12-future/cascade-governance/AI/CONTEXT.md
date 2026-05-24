# cascade-governance — AI Design Context

## Status

**v1 = API surface + no-op enforcement.** Implementation in Phase 7+.

## Role

Resource-quota enforcement layer for §6.1.

## Pillar Reservations

- **§6.1:** the entire reason this crate exists.
- Coordinates with: `cascade-tenant` (carrier), `cascade-runtime` (CPU), `cascade-bpm` (memory), `cascade-storage` (I/O), `cascade-pgwire` (connections).

## Hard Invariants

1. **Enforcement decisions are cheap.** Hot-path checks are a single atomic load.
2. **Counters live where the resource is used** — not in this crate. Governance *reads* counters; it does not write them.
3. **No silent throttling.** Throttle decisions surface as PG errors or backpressure, never as random latency spikes.

## v1 Hooks That Must Exist

- `cascade-tenant::TenantContext` propagated end-to-end.
- Per-DB page residency counter in `cascade-bpm`.
- Per-DB I/O submission tag in `cascade-storage`.
- Per-DB connection count in `cascade-pgwire`.

## Open Design Questions

- DDL shape — the README sketch uses `CREATE RESOURCE GROUP`. Confirm.
- Hierarchical groups (parent / child quotas) — useful for SaaS tiers but adds enforcement complexity.
- Cross-core CPU accounting — naive per-core counters with periodic aggregation vs. lock-free global counter.
