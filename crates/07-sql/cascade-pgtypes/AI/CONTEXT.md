# cascade-pgtypes — AI Design Context

## Role

PG type system kernel. Values, coercion, on-wire encoding. Since T1.4 also
the **key contract** (WP1 F4 / WP5 D13): `KeyBytes`/`KeyRange` — the
order-preserving normalized encoding every index and scan range is defined
on — and the logical index-entry shape (`IndexEntry`/`Locator`).

## The key contract (T1.4, WP1 F4 — one-way door)

- Non-null keys are `[tag u8][payload]`; NULL is a bare `[0xFF]` — that
  marker is what makes **NULLS LAST** (the global rule) hold bytewise, no
  per-type rule. Tags today: `0x01` INT (all widths canonicalized to i64,
  sign-flipped big-endian), `0x02` TEXT (bytewise/C collation, UTF-8 bytes
  + `0x00` terminator), `0x03` BOOL. Tags are frozen bytes: extend, never
  renumber — same law as the WAL's type bytes.
- Equality and ordering are defined **on the normalized bytes** (invariant
  7), never on decoded values. Multi-column keys are concatenations;
  prefix scans are ranges over the concatenation (`KeyRange::prefix_bytes`,
  `KeyRange::text_prefix` — the prefix uppers are constructed so a
  NULL-valued continuation stays *inside* the range).
- A type whose normalization is not order-preserving may not join an index
  until an encoding lands here (currently: NUMERIC, temporal, UUID, JSON —
  their value types don't exist yet; non-C text collations are gated the
  same way).
- `IndexEntry` is the *logical* shape `[key][locator][create word][delete
  word]` (F1 version words raw, LE); the B-tree page layout is
  cascade-btree's (WP6/WP8), deliberately unpinned here.

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
