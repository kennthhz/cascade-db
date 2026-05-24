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

## Open Design Questions

- Should we re-emit the AST as native Rust enums (cleaner but ties us to libpg_query's tree shape) or expose the protobuf parse tree directly (more flexible, more annoying to traverse)?
- Caching of parse results — fingerprint-based, sharing across sessions. Phase 4 follow-up.
