# cascade-columnar

Native columnar secondary storage + vectorized execution.

## Role

Layer **10-columnar**. Implements pillar §3.7 — opt-in per-table columnar projection alongside the row store, with MVCC-consistent propagation and a vectorized execution path. The planner (in `cascade-sql-planner`) routes scan/aggregate-heavy queries here.

## Public API (initial)

- `ColumnarProjection` — handle to a columnar projection of a table.
- Segment format: compressed, zone-mapped, SIMD-friendly column batches.
- Vectorized scan / filter / aggregate operators.
- Row→columnar propagation hook (subscribes to mutations from `cascade-heap`).

## Dependencies

- `cascade-storage`, `cascade-bpm` — segment pages live in the same pool as row pages.
- `cascade-mvcc` — visibility under shared XID/snapshot rules.
- `cascade-pgtypes` — column types match the row store.

## See also

- [AI/CONTEXT.md](AI/CONTEXT.md)
- README §3.7.
