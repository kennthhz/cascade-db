# cascade-pg-parser — AI Design Context

## Role

The safe Rust API over `libpg_query`. The boundary between the FFI exception (§3.6) and the rest of the engine.

## Pillar Reservations

- **§3.6:** the documented FFI exception is *contained* here. Callers must not depend on `cascade-pg-parser-sys` directly.
- **§4.3:** delivers byte-exact PG dialect compatibility for parsing.

## Hard Invariants

1. **No `unsafe` leaks.** Every `unsafe` block is internal and audited. The public API is safe Rust.
2. **No raw pointers in public types.** `ParseTree` and its children are owned Rust types.
3. **Memory context lifetimes contained.** When `ParseTree` drops, all C-owned memory it referenced is freed. No double-free, no leak.
4. **`Send + Sync` where possible.** Parse trees should be movable across threads to support the runtime layer (§3.2).

## Out of Scope

- Planning / planning rewrites — `cascade-sql-planner`.
- Type checking — `cascade-pgtypes` + `cascade-catalog`.
- Anything that requires resolving identifiers — that's a planner concern.

## Resolved Design Questions

- Tree representation: **native Rust enums, syntactic view** — `parse`
  lowers the protobuf tree into owned `Stmt`/`Select`/`Insert` types that
  mirror what the grammar saw, resolving nothing. Anything parsed but
  unmodeled comes back as `Unsupported` (0A000 downstream), so the bound
  view never masquerades as a syntax error. The full protobuf tree stays
  reachable through `cascade-pg-parser-sys::pb` if a future layer needs
  more than the view.
- Caching: still a Phase 4 follow-up (fingerprint-keyed, per the perf-demo
  design), not needed for the demo slice.

## Slice State (2026-10-26)

Implemented and live in the runtime's query path: SELECT (projection +
`WHERE col = const`), INSERT (single-row VALUES), BEGIN/COMMIT/ROLLBACK,
debug functions. Parse errors carry PG's own message text.
