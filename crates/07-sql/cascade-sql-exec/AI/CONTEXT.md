# cascade-sql-exec — AI Design Context

## Role

Row-store executor. Volcano-style operator tree for OLTP-shaped queries.

## Pillar Reservations

- **§3.2 (Thread-per-core):** operators are async; cooperative cancellation; no blocking primitives.
- **§4.3 (PG SQL dialect):** must implement PG-compatible behavior for `RETURNING`, `ON CONFLICT`, CTEs, window functions, lateral joins.
- **§6.2 (Observability):** every operator emits a span; aggregated for slow-query plan capture.

## Hard Invariants

1. **PG-compatible semantics, not "close enough."** NULL handling, division-by-zero, integer overflow, timezone math, locale-sensitive sorts.
2. **Errors are recoverable.** A failing operator aborts the txn cleanly.
3. **No blocking calls** in operator code — async all the way down.
4. **Snapshot visibility honored** — all reads use the txn's snapshot via `cascade-mvcc`.

## Out of Scope

- Vectorized columnar execution (`cascade-columnar`).
- PL/pgSQL execution (`cascade-plpgsql`).
- Query plan caching (could live here, but is a Phase 5 follow-up).

## Open Design Questions

- Operator dispatch: trait-object Volcano vs. compiled (LLVM/cranelift) per-query codegen. Volcano is far simpler; codegen wins on tight CPU-bound workloads but is a much bigger investment.
- Memory accounting per operator — needed for §6.1 statement-memory limits.
