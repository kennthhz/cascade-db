# cascade-sql-planner

Native-Rust query planner.

## Role

Layer **07-sql**. Consumes a parse tree from [`cascade-pg-parser`](../cascade-pg-parser/), resolves identifiers against [`cascade-catalog`](../../06-catalog/cascade-catalog/), and emits a physical plan for [`cascade-sql-exec`](../cascade-sql-exec/).

**Pure Rust.** No PG C code (unlike the parser). The PG planner is too entangled with PG's storage to transplant — we own this code.

## Public API (initial)

- `Planner` — top-level entry; `plan(parse_tree, catalog, session) -> PhysicalPlan`.
- `LogicalPlan`, `PhysicalPlan` — internal IRs.
- Cost-based row-vs-columnar routing (§3.7): the planner decides whether a scan goes through the heap or the columnar projection.

## Dependencies

- `cascade-pg-parser` — input.
- `cascade-catalog` — identifier resolution.
- `cascade-pgtypes` — type coercion / inference.

## See also

- [AI/CONTEXT.md](AI/CONTEXT.md)
- README §3.7, §4.3.
