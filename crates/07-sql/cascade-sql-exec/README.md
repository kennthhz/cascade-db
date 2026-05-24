# cascade-sql-exec

Row-store SQL executor.

## Role

Layer **07-sql**. Executes physical plans from [`cascade-sql-planner`](../cascade-sql-planner/) against the row store (heap + B+Tree + MVCC). The vectorized columnar execution path lives in [`cascade-columnar`](../../10-columnar/cascade-columnar/); the planner decides which path to dispatch to.

## Public API (initial)

- `Executor` — drives a `PhysicalPlan` to completion.
- Operator implementations (scan, join, agg, sort, …).
- `RowSet` — execution-time row representation.

## Dependencies

- `cascade-heap`, `cascade-btree` — scan + index access.
- `cascade-mvcc` — snapshot/visibility.
- `cascade-pgtypes` — value computations.
- `cascade-sql-planner` — `PhysicalPlan` consumer.

## See also

- [AI/CONTEXT.md](AI/CONTEXT.md)
