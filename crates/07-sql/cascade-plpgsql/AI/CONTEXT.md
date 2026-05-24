# cascade-plpgsql — AI Design Context

## Role

PL/pgSQL interpreter.

## Pillar Reservations

- **§4.3:** PL/pgSQL is part of the PG SQL surface ORMs assume. Migrations, triggers, audit functions are all written in it.
- **§3.6 (Rust):** pure-Rust interpreter — no FFI.

## Hard Invariants

1. **PG-compatible semantics** for control flow, exception handling, `RETURN NEXT`, cursors, dynamic SQL (`EXECUTE`).
2. **Statements within a function go through the normal planner/executor** — no shortcut path, no special privileges.
3. **Variables have type identity** from `cascade-pgtypes`.
4. **Sandboxing.** PL/pgSQL is *not* sandboxed in PG; we follow that for compatibility (security model is via roles, not language).

## Out of Scope

- Other PLs (PL/Python, PL/V8) — possible future, not v1.
- JIT compilation of PL/pgSQL bodies — interpreter only in v1.

## Open Design Questions

- Whether to AST-walk or compile to a bytecode for execution. AST-walk is simpler; bytecode is faster.
- Exception block semantics — PG's `EXCEPTION WHEN ... THEN` requires savepoint-on-block-entry. Need to confirm savepoint integration with `cascade-mvcc`.
