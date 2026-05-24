# Cascade DB — Workspace AI Context

> Always-loaded, workspace-wide design context. Per-crate `AI/CONTEXT.md` files refine this for individual components.

## Product in one paragraph

Cascade DB is a high-performance, predictable-latency relational database written in Rust, designed as a drop-in replacement for PostgreSQL. It is wire-, type-, and SQL-compatible with PG (most applications run unmodified, including those using popular extensions), but the kernel underneath is a fundamentally rewritten modern storage engine: `O_DIRECT` + `io_uring`, in-place MVCC with undo log, per-database WAL, thread-per-core execution, and a native columnar HTAP path.

See [`README.md`](../README.md) for the full product spec. This file is the *operating manual* for AI design sessions.

## Architectural pillars (committed for v1)

| § | Pillar | Where it lives |
|---|---|---|
| 3.1 | Kernel-bypass storage I/O (`O_DIRECT` + `io_uring`) | `02-storage/cascade-storage` |
| 3.2 | Thread-per-core execution | `09-runtime/cascade-runtime` |
| 3.3 | In-place MVCC + undo log | `04-txn/cascade-mvcc`, `04-txn/cascade-undo` |
| 3.4 | Per-database WAL | `02-storage/cascade-wal` |
| 3.5 | Userspace buffer pool | `03-memory/cascade-bpm` |
| 3.6 | Memory-safe Rust foundation (FFI exception: PG parser) | workspace-wide; FFI confined to `07-sql/cascade-pg-parser{,-sys}` |
| 3.7 | Native HTAP / columnar | `10-columnar/cascade-columnar` |

## Future pillars (reserved design space — see README §6)

| § | Future capability | v1 reservation hook |
|---|---|---|
| 6.1 | Multi-tenant resource governance | `09-runtime/cascade-tenant` (context propagation), `12-future/cascade-governance` (stub) |
| 6.2 | First-class observability (OTel) | `01-foundation/cascade-telemetry` (`Span`, `MetricsRegistry`) |
| 6.3 | Logical replication / CDC | `02-storage/cascade-wal` (logical-decoding-ready record format), `12-future/cascade-cdc` (stub) |
| 6.4 | Disaggregated / tiered storage | `02-storage/cascade-storage` (`PageStore`/`WalStore` traits) |

## Layering rule

Crates are grouped under `crates/NN-layer/cascade-foo/`. Layer `N` may depend on layers `1..N`, never on `N+1..`. This is a convention, not enforced by Cargo — code reviewers and AI sessions must respect it.

The dependency graph itself is encoded in each crate's `Cargo.toml`, not in the directory tree. Nesting `cascade-storage` inside `cascade-bpm` would falsely imply ownership; instead, `cascade-bpm` *depends on* `cascade-storage` via Cargo.

## Cross-cutting invariants

These apply to **every** crate. Each crate's `AI/CONTEXT.md` may add more, but cannot weaken these.

1. **No raw I/O above storage.** No layer above `cascade-storage` / `cascade-wal` may hold a `RawFd` or `File` handle. All I/O flows through `PageStore` / `WalStore`.
2. **Backend-agnostic page IDs.** `PageId = (DatabaseId, SegmentId, PageNo)`. Never `(file_path, offset)`.
3. **Tenant context everywhere.** Every operation that consumes CPU, memory, or I/O runs in a context that carries `DatabaseId` (and eventually `TenantId`). Even if enforcement is a no-op in v1, the *data* must be present.
4. **Span on hot paths.** Every public function on a hot path takes `&Span` (or `Span::noop()`). Wiring an OTel exporter later must not require changing call signatures.
5. **WAL records carry full row images** where logical decoding will need them. This is the single hardest reservation to retrofit — get it right in Phase 2.
6. **No unsafe outside `*-sys` crates** except where strictly justified and audited. `cascade-pg-parser-sys` is the documented exception (FFI).
7. **No PG C code reuse beyond the parser.** Don't slip in FFI for "just this one helper" without a workspace-level decision.

## Glossary

See [`GLOSSARY.md`](GLOSSARY.md) for `PageId`, `Lsn`, `Xid`, `DatabaseId`, `TenantId`, `AlignedBuf`, and other shared terms.

## Decisions

See [`decisions/`](decisions/) for cross-cutting ADRs (workspace layout, FFI policy, language version, etc.).

## When in doubt

- Architecture question: re-read `README.md` §3 and §6.
- Layering question: re-read this file's "Layering rule".
- Compatibility question: re-read `README.md` §4.
- Per-crate decision: read that crate's `AI/CONTEXT.md` first.
