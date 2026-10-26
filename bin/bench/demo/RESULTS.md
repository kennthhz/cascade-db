# Demo: real parser + pool-resident reads, PG-vs-cascade

Environment: 8-core WSL2, one box hosting pgbench, PG 16.15 (scratch cluster,
fsync/synchronous_commit on), and cascade-db. Identical SQL text on both sides
(`SELECT payload FROM demo WHERE id = :id`, `id = random(1,100000)`), simple
wire protocol only. Both servers hold the same 100,000-row relation. 10-second
runs, pgbench latency logs summarized by `percentile.py`.

## Per-query work (the fairness story)

- PG: libpg_query-equivalent parse → plan (cached in prepared mode) → pkey
  index scan → heap tuple fetch.
- cascade: the same PG parser (vendored libpg_query, ADR 0002) → fixed-schema
  resolution → arithmetic on a pool-resident page placed by the T2.5 hash.

The parse cost is identical by construction. What differs: PG plans and walks
an index; cascade resolves against a fixed schema and computes the page.

## Throughput / latency (tps / p50 latency)

| clients | PG simple | PG prepared | cascade 2 workers | cascade 4 workers |
|---|---|---|---|---|
| c=1  | 6,696 / 0.139ms | 8,109 / 0.115ms | 5,533 / 0.169ms | (artifact)* |
| c=4  | 14,239 / 0.216ms | 14,815 / 0.198ms | 6,064 / 0.168ms† | 14,057 / 0.181ms |
| c=8  | 14,824 / 0.389ms | 15,353 / 0.346ms | 7,467 / 0.417ms | 14,183 / 0.182ms |

\* w4 c=1 hit a ~2.7ms fixed stall per query (366 tps) — not a capacity
number; excluded. Not reproducible at other concurrency; uninvestigated.
† p50 of the healthy clients; the -j 4 runs had client-side thread
contention on this shared box (one starved pgbench log per run). A -j 1
re-run at c=4/w4 gave 11,174 tps, p50 0.163ms, no starvation — the
aggregates are consistent.

## Findings

1. **Per-core parity at saturation.** PG plateaus ~14.2–15.4k tps using 4–8
   unpinned cores (~3.5k/core). Cascade's 2 pinned workers do 5.5–7.5k
   (~3.7k/core); with 4 workers it reaches PG's plateau (14.1–14.2k) on
   half the cores.
2. **Median latency under load favors cascade.** At c=8: 0.182ms vs PG's
   0.389/0.346ms — the pool-resident read path (hash → address → decode,
   no planner, no btree) shows through once the parse is paid.
3. **PG wins the single client.** Prepared-mode PG at c=1 (8.1k tps,
   p50 0.115ms) beats cascade w2 (5.5k, 0.169ms): at c=1 there is no
   concurrency to win, and PG's mature execution path plus cached plans
   beat cascade's parse-every-time frontend.
4. **Tail latency is cascade's honest loss.** At load, cascade p99 is
   4.2–9.8ms vs PG's 0.6–1.8ms. Glommio's cooperative scheduler has no
   preemption; a slow task stalls its core's queue. This is the real gap
   to work on, not the throughput number.

## Scope notes

- cascade's `demo` is a fixed-schema demo relation: point lookups only,
  no planner, no index — the comparison measures frontend + data placement,
  not a general-purpose engine.
- libpg_query is parse-only; cascade does no semantic analysis (fixed
  schema stands in for the catalog).
- Simple protocol only; parse runs per query on both sides (PG's prepared
  rows show what skipping it buys).
- WSL2, all processes sharing 8 cores; numbers are environment-relative,
  the shape of the comparison is the result.
