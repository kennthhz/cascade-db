# cascade-ext-abi

Extension hosting ABI.

## Role

Layer **11-extension**. Implements §4.4 — the loadable-extension surface that hosts pgvector, PostGIS, pg_trgm, pg_stat_statements, and friends.

## Two modes of extension

1. **Native (Rust).** Reimplemented against the Cascade kernel. Examples: pg_trgm, citext, hstore — small enough to rewrite cleanly.
2. **Hosted (upstream C).** The upstream extension is loaded via a PG-compatible ABI. Examples: PostGIS (too large to rewrite), pgvector (initially — may move to native later).

## Public API

- Extension manifest format.
- Type registration → `cascade-pgtypes`.
- Function registration → `cascade-catalog`.
- Operator class registration.

## See also

- [AI/CONTEXT.md](AI/CONTEXT.md)
- README §4.4.
