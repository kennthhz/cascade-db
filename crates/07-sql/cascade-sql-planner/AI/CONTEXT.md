# cascade-sql-planner — AI Design Context

## Role

Native Rust query planner. Converts PG parse trees into executable plans.

## Pillar Reservations

- **§3.7 (HTAP):** the planner is where row-vs-columnar routing happens. Costs from both `cascade-heap` and `cascade-columnar` feed the decision.
- **§4.3 (PG SQL dialect):** must handle the full PG-supported feature set the parser accepts — CTEs, window functions, lateral joins, `RETURNING`, `ON CONFLICT`, etc.

## Hard Invariants

1. **Pure Rust.** No FFI. The PG planner is not portable to our storage; reuse via FFI would create more problems than it solves.
2. **Catalog reads via `cascade-catalog` only** — no shortcuts.
3. **Cost model is auditable** — costs are real (estimated tuples, page reads), not magic constants.

## Out of Scope

- Parse-tree manipulation (lives in `cascade-pg-parser` or sits in the planner's parse-tree-rewrite passes — TBD where).
- Execution itself (`cascade-sql-exec`).
- PL/pgSQL — that's `cascade-plpgsql`, which invokes the planner per-statement.

## Open Design Questions

- IR shape: Cascades-style (memo + groups + transformations) vs. classic Volcano-style. Cascades scales better for complex plans but is a bigger up-front investment.
- Reuse opportunities from DataFusion's planner? Worth investigating; DataFusion's planner is MIT/Apache and has window functions, joins, aggregates.
- Statistics — sampled vs full, where stored, when refreshed.
