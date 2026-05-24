# cascade-types — AI Design Context

## Role

Foundation crate. Defines shared value types referenced by multiple layers. Has zero internal dependencies; every other crate may depend on it.

## Pillar Reservations

- **§6.4 (Disaggregated storage):** `PageId` is `(DatabaseId, SegmentId, PageNo)` — backend-agnostic. Never `(file_path, offset)`. This must remain true.
- **§6.1 (Multi-tenant governance):** `TenantId` exists in v1 (may equal `DatabaseId`); types that flow through hot paths must already carry it where applicable.

## Hard Invariants

1. **No I/O, no logic, no async.** This crate contains data types only. No method that could block, allocate non-trivially, or fail.
2. **No `unsafe`** — `AlignedBuf` is the one expected exception; gate it carefully and audit.
3. **`PageId` shape is fixed.** Adding new fields breaks every downstream crate. Extend via a new type (`PageIdV2`) before mutating.
4. **Identifiers are newtypes, not raw integers.** `pub struct DatabaseId(pub u32)`, not `type DatabaseId = u32`. Prevents accidental swaps in function signatures.

## Out of Scope

- Error types (in `cascade-error`).
- Trait definitions (`PageStore`, `WalStore`, etc.) — those belong to the crate that owns the abstraction.
- Serialization formats / on-disk encodings.

## Open Design Questions

- Should `Lsn` be a `u64` or distinguish global from per-DB sequence? Current call: per-DB `Lsn(u64)`, with the per-DB scoping understood from context (each database's WAL has its own LSN space, §3.4).
- `TenantId` vs `DatabaseId` — for v1 a SaaS tenant = one database; if we later support multi-DB tenants, `TenantId` becomes distinct. The newtype exists now to reserve the API.
- `AlignedBuf` ownership model — owned by BPM, lent to storage. Implementation TBD; the type signature must support move-in/move-out semantics needed by `tokio-uring`.

## Decisions

See [`decisions/`](decisions/).
