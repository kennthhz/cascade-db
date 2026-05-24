# cascade-columnar — AI Design Context

## Role

The HTAP engine. Columnar secondary storage with vectorized execution.

## Pillar Reservations

- **§3.7:** the defining pillar.
- **§3.3 (MVCC):** columnar reads use the same snapshot/XID system as the row store — no separate consistency model.
- **§6.3 (CDC):** columnar propagation is driven by WAL events; the CDC stream is unaffected by columnar.

## Hard Invariants

1. **Row store is authoritative.** Columnar is a secondary projection. On disagreement, row wins; columnar is invalidated and rebuilt.
2. **MVCC-consistent reads.** Analytical queries see the same snapshot OLTP queries see. No "eventually consistent" gap.
3. **Opt-in per table** — never enabled implicitly. Storage cost is paid where analytics actually matter.
4. **Same page size.** 8 KB segments to match BPM and storage; eases buffer pool sharing.

## Out of Scope

- Cross-table joins planned entirely in columnar — Phase 6 starts with single-table scans and aggs.
- Distributed analytics / sharding — out of v1.

## Open Design Questions

- Compression mix: dictionary + RLE + bit-packing + delta — which per type? Worth benchmarking against typical OLTP data.
- Zone-map granularity (page vs segment).
- Async propagation: how do we bound the lag between a row write and the columnar update? Strict commit-time write would hurt OLTP latency; async risks long lag.
- Vectorized execution engine: build from scratch vs reuse Arrow + DataFusion's vectorized operators?
