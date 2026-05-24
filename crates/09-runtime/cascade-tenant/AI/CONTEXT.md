# cascade-tenant — AI Design Context

## Role

`TenantContext` data type + task-local propagation primitives. Pure plumbing for §6.1.

## Pillar Reservations

- **§6.1:** the carrier type. Enforcement (`cascade-governance`) reads these counters; this crate just ensures they're populated.

## Hard Invariants

1. **Context is always set.** Background work runs under a synthetic "system" context, never a missing one. Avoids `Option<TenantContext>` everywhere.
2. **Cheap to clone.** Designed to be copied freely across async tasks.
3. **No I/O, no logic.** Plumbing only.

## Out of Scope

- Quota / limit values (live in `cascade-governance`).
- Enforcement (live in `cascade-governance`).

## Open Design Questions

- Propagation mechanism: thread-local + explicit-arg hybrid vs pure explicit-arg. Thread-local is less invasive but interacts subtly with `Future` polling on async runtimes. Explicit-arg is verbose but unambiguous.
- `TenantId` vs `DatabaseId` semantics — see [GLOSSARY](../../../AI/GLOSSARY.md).
