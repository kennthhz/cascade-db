# cascade-bpm

Userspace Buffer Pool Manager.

## Role

Layer **03-memory**. Implements pillar §3.5 — owns the pre-allocated `AlignedBuf` pool, page pin/unpin, eviction policy, and the WAL-before-data flush invariant.

## Public API (initial)

- `BufferPool` — handle returned to consumers; pin / unpin / mark-dirty.
- `PageGuard` — RAII pin handle (drop = unpin).
- Eviction policy plug-in (`LruK`, `ClockSweep` candidates).
- Per-`DatabaseId` page accounting hooks (§6.1 reservation).

## Dependencies

- `cascade-storage` (for the `PageStore` it reads from / writes to)
- `cascade-types`, `cascade-error`, `cascade-telemetry`

## v1 reservations honored

- **§6.1:** every pin / fetch operation tags pages with their `DatabaseId`; the registry keeps per-DB residency counts. Enforcement is a no-op now; the counters are real.
- **§6.4:** the BPM speaks to `PageStore`, never to raw files.

## See also

- [AI/CONTEXT.md](AI/CONTEXT.md)
- README §3.5, §6.1.
