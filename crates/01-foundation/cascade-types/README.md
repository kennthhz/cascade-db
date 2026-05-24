# cascade-types

Shared primitive types for Cascade DB.

## Role

Layer **01-foundation**. The definitive home for value types that more than one layer references — identifiers, sequence numbers, aligned I/O buffers. By convention, every other crate may depend on `cascade-types`; `cascade-types` depends on nothing internal.

## What lives here

| Type | Purpose |
|---|---|
| `DatabaseId`, `TenantId`, `SegmentId`, `PageNo` | Identifier newtypes. |
| `PageId` | `(DatabaseId, SegmentId, PageNo)` — globally unique, backend-agnostic. |
| `Lsn` | Log Sequence Number — monotonic byte offset within a database's WAL. |
| `Xid` | Transaction ID. |
| `AlignedBuf` | 4 KB-aligned 8 KB buffer required for `O_DIRECT`. |

## What does *not* live here

- Errors → `cascade-error`.
- Trait definitions (`PageStore`, `WalStore`, etc.) → the crate that owns the abstraction.
- Anything specific to one layer.

## Dependencies

None (foundation layer).

## See also

- [AI/CONTEXT.md](AI/CONTEXT.md) — design context and invariants.
- Workspace [GLOSSARY](../../../AI/GLOSSARY.md) — human-readable type reference.
