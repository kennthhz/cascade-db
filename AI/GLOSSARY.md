# Cascade DB — Shared Glossary

Types and concepts that recur across multiple crates. Definitive definitions live in `cascade-types`; this file is the human-readable reference.

## Identifiers

| Term | Type | Meaning |
|---|---|---|
| `DatabaseId` | `u32` | Logical database. Each has its own WAL, XID space, and (eventually) resource group. The atomic unit of replication, PITR, and migration. |
| `TenantId` | (TBD) | Reserved for §6.1 governance. May be `= DatabaseId` in v1; broader if we later support multi-DB tenants. |
| `SegmentId` | `u32` | A physical storage segment within a database (table, index, undo segment). |
| `PageId` | `(DatabaseId, SegmentId, PageNo)` | Globally unique, backend-agnostic identifier for an 8 KB page. |
| `PageNo` | `u32` | Page offset within a segment. |

## Time / Ordering

| Term | Type | Meaning |
|---|---|---|
| `Lsn` | `u64` | Log Sequence Number. Monotonic byte offset within a database's WAL. Per-DB (see README §3.4). |
| `Xid` | `u64` | Transaction ID. Per-DB (see README §3.4). Wide enough to avoid wraparound. |
| `Snapshot` | struct | MVCC visibility — set of committed XIDs visible to a transaction. |

## Memory

| Term | Type | Meaning |
|---|---|---|
| `AlignedBuf` | struct | 4 KB-aligned, 8 KB-sized buffer required for `O_DIRECT`. Owned by the BPM, lent to storage. |
| Page | concept | 8 KB unit of storage. Row pages, index pages, undo pages, columnar segment pages all use this size. |

## I/O

| Term | Trait | Meaning |
|---|---|---|
| `PageStore` | trait | Random-access page I/O. v1 impl: local `O_DIRECT`. Future: S3 page service (§6.4). |
| `WalStore` | trait | Sequential WAL append/flush/truncate. v1 impl: local files. Future: object-store WAL. |

## Observability (reserved for §6.2)

| Term | Type | Meaning |
|---|---|---|
| `Span` | trait | OpenTelemetry-compatible span handle. v1 default impl is `Span::noop()`. |
| `MetricsRegistry` | struct | Central counter/histogram registry. v1 stores metrics; export comes later. |

## Tenancy (reserved for §6.1)

| Term | Type | Meaning |
|---|---|---|
| `TenantContext` | struct | Carries `DatabaseId` (+ future `TenantId`) through the request lifecycle. Required at every layer; enforcement may be a no-op in v1. |
