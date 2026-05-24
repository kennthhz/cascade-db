# cascade-pgtypes — AI Design Context

## Role

PG type system kernel. Values, coercion, on-wire encoding.

## Pillar Reservations

- **§4.1 (Wire protocol):** binary and text wire formats must match PG byte-for-byte. Test against `libpq` and `psycopg`.
- **§4.2 (Data types):** breadth coverage of PG types.
- **§4.4 (Extensions):** extension types (`vector` from pgvector, `geometry` from PostGIS) plug in here via a registration API.

## Hard Invariants

1. **Wire-format parity.** Off-by-one in `timestamptz` epoch handling, NaN/Inf encoding in numerics, JSON whitespace canonicalization — all of these break clients silently. Test vectors against real PG.
2. **PG-compatible behavior for edge cases.** `NULL <> NULL` is `NULL`, not `true`. Integer division truncates toward zero. `timestamp` without `tz` does not implicitly carry session TZ.
3. **Allocator-friendly.** Hot-path operations on `int4`/`int8`/`numeric` should be branch-light and avoid allocation.
4. **Extension types are real type-system citizens** — same registration mechanism, same coercion rules, no second-class status.

## Out of Scope

- Type *parsing* (in `cascade-pg-parser` / planner — converting `'2024-01-01'::date` literals).
- Storage of values on disk (in `cascade-heap` / `cascade-page` row encoding).
- Vectorized batch encoding (`cascade-columnar` has its own codecs).

## Open Design Questions

- `numeric`: rewrite vs. wrap `rust_decimal` / `bigdecimal`. PG `numeric` has subtle precision-propagation rules; need test vectors.
- Date/time: `chrono` vs. `jiff` (newer, better timezone handling). Lean `jiff`.
- `jsonb` operators (`@>`, `?`, `#>>`, `jsonb_path_query`): how much of the path engine do we ship in v1?
- Composite/row types: significant ORM dependency. Coverage worth verifying with target ORMs.
