# cascade-tenant

`TenantContext` propagation.

## Role

Layer **09-runtime**. Implements the §6.1 reservation: every operation that consumes CPU, memory, or I/O must run in a context that carries `DatabaseId` (and future `TenantId`). Even if enforcement is a no-op in v1, the *data* must be present so future quotas (in `cascade-governance`) can throttle without rewriting call sites.

## Public API

- `TenantContext { db: DatabaseId, tenant: Option<TenantId> }`.
- Task-local accessor: `TenantContext::current()` returns the active context (or a sentinel "system" context for background work).
- `with_context(ctx, async { ... })` — runs a future under a given context.

## See also

- [AI/CONTEXT.md](AI/CONTEXT.md)
- README §6.1.
