# Product Specification: Cascade DB

## 1. Executive Summary
**Cascade DB** is a high-performance, predictable-latency relational database designed as a drop-in replacement for PostgreSQL. It combines comprehensive PostgreSQL compatibility — across the **wire protocol**, **data type system**, and **SQL dialect** — with a fundamentally rewritten, modern storage kernel built in Rust. The goal is that the vast majority of existing PostgreSQL applications, including those depending on popular extensions, can migrate to Cascade DB **without any application rewrite**.

Cascade DB is designed to eradicate the legacy architectural bottlenecks of PostgreSQL: **VACUUM bloat** (via In-Place MVCC), **Double Buffering** (via `O_DIRECT`), **Checkpoint Latency Spikes** (via `io_uring`), and **Connection Limits** (via a Thread-per-Core architecture). Furthermore, it introduces a **Per-Database WAL architecture**, solving the "noisy neighbor" replication and recovery bottlenecks inherent in Postgres's global cluster design.

## 2. Target Market & Use Cases
* **High-Throughput OLTP:** Applications constrained by Postgres write-amplification and checkpoint stalling.
* **Multi-Tenant SaaS:** Platforms managing thousands of distinct databases that require isolated replication streams and independent Point-in-Time Recovery (PITR).
* **Ecosystem Migrations:** Teams wanting modern database performance without rewriting their application code, drivers, ORMs, or migrating off the PostgreSQL extensions (pgvector, PostGIS, pg_trgm, etc.) they already depend on.

---

## 3. Core Architectural Pillars

### 3.1. Kernel-Bypass Storage I/O (`io_uring` + `O_DIRECT`)
* **Zero Double-Buffering:** Bypasses the Linux Page Cache entirely using `O_DIRECT`. Data exists in only one place in RAM (the Userspace Buffer Pool), effectively doubling the usable memory capacity for hot data.
* **Syscall Eradication:** Utilizes `io_uring` to batch I/O submissions and completions asynchronously. Reduces CPU context-switch overhead to near-zero during heavy I/O.
* **Predictable Latency:** Eliminates the need for blocking `fsync` storms. Dirty pages are trickle-flushed asynchronously, keeping P99 latencies flat during checkpoints.

### 3.2. Thread-per-Core Execution Model
* **The Problem:** PostgreSQL uses a legacy process-per-connection model. Spawning a heavy OS process for every client consumes massive amounts of RAM and causes severe CPU context-switching overhead under load, forcing users to deploy external proxies like PgBouncer.
* **The Cascade DB Solution:** Implements a modern thread-per-core async architecture: **one worker thread pinned per CPU core**, each with a private `io_uring` ring and `!Send` per-connection query state. The shared data substrate (buffer pool, WAL, MVCC, catalog) uses lock-free primitives so a single database can scale across many cores.
* **Latency / Throughput configuration.** Two coordinated knobs — one cluster-wide, one per-database — let operators choose explicitly:
  * **SMT topology** (cluster-wide): `physical` (one worker per physical core, HT siblings idle; default, P99-optimized) or `logical` (one worker per vCPU; throughput-optimized).
  * **NUMA affinity** (per-database): `single` (DB bound to one NUMA node; pages, WAL, and workers all NUMA-local; default, P99-optimized) or `cross` (DB spans all NUMA nodes; cross-NUMA traffic accepted; throughput-optimized).
  * The combined "**latency stack**" (default: `physical` + `single` + NUMA-local arenas + kernel isolation) gives flat P99. The "**throughput stack**" (`logical` + `cross`) gives maximum aggregate CPU utilization. Mixed: some DBs on the same cluster can be `single`, others `cross`.
* **Built-in Connection Scaling:** Can natively handle tens of thousands of concurrent connections without memory bloat. Connections are routed least-loaded at accept (within the DB's NUMA constraint), and pinned to their reactor for their lifetime — the borrow checker guarantees zero atomics on per-connection state.
* **Cooperative Fairness:** Cooperative yielding at vectorized batch boundaries plus a CPU-deadline yield keeps OLTP p99 flat even when OLAP queries share the core. A reactor-stall detector logs any task that runs too long without yielding.
* **Result:** Maximizes CPU cache locality, drastically improves query performance, and entirely eliminates the operational complexity of running external connection poolers (PgBouncer/Pgpool).
* **Full design:** see [`docs/runtime-architecture.md`](docs/runtime-architecture.md).

### 3.3. The "Anti-VACUUM" Engine (In-Place MVCC)
* **In-Place Updates:** Modifies records directly within their original 8KB data pages, preventing table bloat and write-amplification.
* **Undo-Log Rollback Segments:** Preserves MVCC isolation by writing the pre-update row state to a separate, sequential Undo Log.
* **Result:** Eradicates the need for a background VACUUM daemon or table-locking `VACUUM FULL` operations.

### 3.4. Isolated Durability (Per-Database WAL & LSN)
Exploiting the fact that Postgres does not natively support cross-database transactions, Cascade DB physically isolates operational logging.
* **Per-Database Write-Ahead Log (WAL):** Each database maintains its own WAL, LSN sequence, and Transaction ID (XID) space.
* **Isolated Replication:** A massive write spike in Database A has zero impact on the replication stream or WAL size of Database B.
* **Granular Recovery:** Enables trivial, instantaneous Point-in-Time Recovery (PITR) for a single database without requiring a full cluster restore.
* **Sharding Readiness:** Databases are self-contained physical units, making cross-node migration and sharding significantly easier than legacy Postgres.

### 3.5. Userspace Buffer Pool Manager (BPM)
* **Direct Memory Control:** A highly optimized Rust concurrency layer managing a pre-allocated pool of `AlignedBuf` 8KB pages.
* **Eviction:** Utilizes modern eviction algorithms (e.g., LRU-K or CLOCK-sweep) optimized for B+Tree index traversal and sequential scans.

### 3.6. Memory-Safe Systems Foundation (Built in Rust)
Cascade DB is implemented in **Rust** — every component on the critical path (storage, BPM, WAL, MVCC, B+Tree, executor, wire protocol, runtime) is pure Rust. This is not an incidental implementation choice but a foundational architectural pillar.
* **Memory Safety Without GC:** Eliminates the class of use-after-free, buffer overflow, and data-race bugs that have historically dominated CVEs in C/C++-based database engines — without paying the latency tax of a garbage collector.
* **Async-Native I/O:** A Rust-native async runtime integrated with `io_uring` (e.g., `glommio`, `monoio`, or `tokio-uring`) makes the kernel-bypass I/O model (§3.1) idiomatic rather than bolted-on.
* **Zero-Cost Abstractions:** High-level page, index, and catalog APIs compile down to tight, branch-predictable machine code on the hot path; no virtual dispatch or hidden allocations in the critical path.
* **Invariants in the Type System:** Transaction states, page latches, catalog versions, and wire-protocol message types are encoded at compile time — invariants legacy engines enforce only by convention and code review.
* **Fearless Concurrency:** The borrow checker makes the shared-nothing thread-per-core model (§3.2) and lock-free index structures (e.g., Optimistic Lock Coupling) safe to write and refactor at scale.

**Pragmatic FFI exception.** The guiding principle is *"Rust where we own the critical path; vetted dependencies where reuse beats reinvention."* The **PostgreSQL SQL parser** is the one deliberate FFI exception: Cascade DB reuses **`libpg_query`** (the upstream PG parser exposed as a stateless C library) to guarantee byte-exact dialect compatibility that would otherwise take person-years to reproduce and would still drift from PG behavior on edge cases real ORMs hit. The FFI surface is isolated in a single audited wrapper crate pair (`cascade-pg-parser-sys` for raw bindings, `cascade-pg-parser` for the safe Rust API) so it remains replaceable, contained, and does not leak C lifetimes or memory contexts into the rest of the engine. Every other component — planner, executor, type system, catalog, wire protocol, storage, runtime — is pure Rust.

### 3.7. Native Lightweight Analytics (Columnar Secondary Storage / HTAP)
Cascade DB is fundamentally a row-store OLTP engine, but ships with a built-in **columnar secondary storage format** so lightweight analytical queries on operational data run natively — with **no external ETL, no separate warehouse, no FDW gymnastics**.
* **Opt-In Per Table/Column:** `ALTER TABLE ... ENABLE COLUMNAR` (or equivalent DDL) lights up a columnar-backed projection alongside the primary row store. Storage cost is paid only where analytics actually matter.
* **Transactionally Consistent:** Changes propagate from the row store to columnar segments under the same XID / MVCC snapshot rules (§3.3), so analytical queries see consistent, up-to-date state without lock-step replication or stale-read windows.
* **Modern Compression & Pruning:** Columnar segments use dictionary encoding, RLE, bit-packing, and per-segment **zone maps** (min/max, bloom filters) for aggressive predicate pushdown — scanning only the segments that can possibly match.
* **Vectorized Execution:** Scan-heavy and aggregation-heavy queries are routed through a SIMD-friendly vectorized execution path that operates on column batches, not row-at-a-time tuples.
* **Cost-Based Routing:** The planner transparently chooses between the row-store path (point lookups, OLTP) and the columnar path (scans, aggregations, BI) — no application-side hinting required.
* **Use Cases:** in-product dashboards, BI rollups, ad-hoc analytics, time-series rollups — all co-located with transactional data and queried with standard PostgreSQL SQL.

---

## 4. PostgreSQL Compatibility

Cascade DB's compatibility goal is explicit and aggressive: **most existing PostgreSQL applications should run unmodified.** Compatibility is treated as a product requirement, not a façade — it spans four layers:

### 4.1. Wire Protocol Compatibility
* Implements the PostgreSQL TCP wire protocol (`pgwire`) on port 5432, including the simple and extended query protocols, prepared statements, portals, `COPY`, cursors, and `NOTIFY`/`LISTEN`.
* Supports standard authentication methods (`SCRAM-SHA-256`, `MD5`, TLS/SSL).
* Works transparently with `psql`, `pgAdmin`, `pg_dump`/`pg_restore`, and every driver in the ecosystem (`libpq`, JDBC, `pgx`, `node-postgres`, `psycopg`, etc.) without driver-side changes.

### 4.2. Data Type Compatibility
Implements the PostgreSQL type system with binary and text representations that match Postgres byte-for-byte on the wire — so client drivers decode values the same way they always have.
* **Numerics:** `smallint`, `integer`, `bigint`, `numeric`/`decimal`, `real`, `double precision`, `serial` family.
* **Character:** `text`, `varchar`, `char`, `citext`.
* **Date/Time:** `date`, `time`, `timestamp`, `timestamptz`, `interval` (including Postgres's timezone semantics).
* **Binary & UUID:** `bytea`, `uuid`.
* **Structured:** `json`, `jsonb` (with full operator/path support), `hstore`, arrays of any base type, ranges, composite/row types, `enum`.
* **Network & Geometric:** `inet`, `cidr`, `macaddr`, `point`, `line`, `polygon`, etc.

### 4.3. SQL Dialect Compatibility (PG SQL, not just ANSI SQL)
Targets the full PostgreSQL SQL dialect that real applications and ORMs actually emit — not a reduced ANSI-SQL subset.
* **Parser:** Reuses `libpg_query` via FFI (see §3.6) to guarantee byte-exact compatibility with the PG SQL grammar — the same parser real applications and ORMs are written against. Planner, executor, and everything downstream of the parse tree is pure Rust.
* **DML:** `SELECT`, `INSERT`, `UPDATE`, `DELETE`, `MERGE`, with PG-specific clauses (`RETURNING`, `ON CONFLICT ... DO UPDATE/NOTHING`, `FOR UPDATE/SHARE ... SKIP LOCKED/NOWAIT`).
* **Query features:** CTEs (including recursive and `WITH ... AS MATERIALIZED`), window functions, lateral joins, `DISTINCT ON`, `GROUPING SETS`/`ROLLUP`/`CUBE`, full-text search (`tsvector`/`tsquery`).
* **DDL:** `CREATE/ALTER/DROP TABLE`, indexes (B-tree, hash, GIN, GiST, BRIN), views, materialized views, schemas, sequences, constraints, triggers.
* **Transactions:** `BEGIN`/`COMMIT`/`ROLLBACK`, savepoints, all four standard isolation levels, advisory locks.
* **Procedural:** `PL/pgSQL` for functions, stored procedures, and triggers — the dialect ORMs and existing applications already depend on.
* **Real catalog:** `pg_catalog` and `information_schema` are implemented as **real, query-backed system catalogs** (not mock responses), so arbitrary introspection queries from ORMs, migration tools (Flyway, Liquibase, Alembic, Prisma Migrate), and observability tools work correctly.

### 4.4. Extension Compatibility
A meaningful share of "PostgreSQL applications" are really "PostgreSQL + extension X" applications. Cascade DB will ship first-class support for the most depended-on extensions, either by re-implementing them natively against the Cascade kernel or by hosting the upstream extension via a compatible extension ABI.

Initial target set (priority order):
* **`pgvector`** — vector similarity search; table-stakes for any modern AI/RAG workload.
* **`PostGIS`** — geospatial types, indexes, and functions.
* **`pg_trgm`** — trigram similarity / fuzzy search, commonly paired with GIN.
* **`pg_stat_statements`** — query performance telemetry; expected by virtually every production deployment.
* **`uuid-ossp` / `pgcrypto`** — UUID generation and cryptographic primitives.
* **`hstore`** and **`citext`** — included at the type-system layer (§4.2) for zero-friction compatibility.
* **`postgres_fdw`** — foreign data wrapper for federation back to existing Postgres clusters during migration.

A formal **Extension Compatibility Matrix** (Supported / Native / Planned / Out-of-scope) will be maintained alongside the release notes so operators can verify compatibility before migrating.

---

## 5. Competitive Differentiation

| Feature / Bottleneck | PostgreSQL (Legacy) | Cascade DB |
| :--- | :--- | :--- |
| **Concurrency Model**| Process-per-Connection (Heavy) | **Thread-per-Core** (Async, Lightweight) |
| **Connection Pooling** | Requires External Proxy (PgBouncer) | **Native** (Handles 10k+ connections) |
| **Memory Architecture**| Double Buffered (OS Cache + Shared Buffers) | Zero-Copy Userspace Buffer Pool |
| **Checkpointing** | Blocking `fsync` storms (Latency Spikes) | Continuous Async Flushing (`io_uring`) |
| **MVCC Model** | Append-Only (Causes Bloat) | In-Place Updates + Undo Log |
| **Maintenance** | Requires Autovacuum Tuning | **Zero VACUUM** |
| **WAL & LSN** | Global / Cluster-wide | **Per-Database** (Isolated) |
| **Point-in-Time Recovery**| Cluster-level only | Granular (Per-Database) |
| **Analytical Queries** | Row-only (needs cstore_fdw / Citus / external warehouse) | **Native Row + Columnar HTAP** (vectorized) |
| **Implementation Language** | C (manual memory management, frequent CVEs) | **Rust** (memory-safe, no GC) |

---

## 6. Future Architectural Pillars (Reserved Design Space)

The following pillars are committed product directions but are **explicitly out of scope for v1**. They are documented here — with initial API surfaces and the v1 hooks that preserve their design space — so that current implementation work does not foreclose them. For each pillar this section describes: **(a)** the future capability, **(b)** its initial API / architecture sketch, and **(c)** the **v1 reservations** — invariants Phase 1–6 code must respect so that the future capability can be added without breaking changes.

### 6.1. Multi-Tenant Resource Governance

**(a) Capability.** Native per-database (and per-role) quotas for CPU, memory, I/O bandwidth, connection count, and per-statement memory / timeout — enforced inside the kernel, without external proxies or sidecars. Directly supports the multi-tenant SaaS use case in §2.

**(b) Initial API sketch (PG-compatible DDL):**
```sql
CREATE RESOURCE GROUP saas_tier_silver
  WITH (
    cpu_shares        = 200,
    memory_limit      = '4GB',
    io_bandwidth      = '200MB/s',
    max_connections   = 500,
    statement_timeout = '30s',
    statement_memory  = '256MB'
  );

ALTER DATABASE tenant_42 SET RESOURCE_GROUP = saas_tier_silver;

-- Live introspection
SELECT * FROM cascade_resource_usage;   -- per-DB live usage vs. limits
```

**(c) v1 reservations.**
* Every CPU-consuming operation (parse, plan, execute, BPM eviction sweep, WAL flush) runs in a context that carries a `TenantId` / `DatabaseId`. The data is present in v1; enforcement may be a no-op.
* BPM page accounting is **per-database from day one**, even with no global cap enforced. Adding enforcement later is a single accounting check.
* `io_uring` submission queues are partitioned (or weighted) by database — at minimum, every submission carries an opaque tenant tag.
* Connection objects carry database context through the entire request lifecycle.
* All limits ultimately enforced are surfaced as `pg_settings`-compatible rows where reasonable.

### 6.2. First-Class Observability

**(a) Capability.** OpenTelemetry-native traces, metrics, and structured logs out-of-the-box — no extensions, no exporters to install. `pg_stat_statements`-compatible views, persistent EXPLAIN-ANALYZE-style plan capture, native Prometheus scrape endpoint.

**(b) Initial API sketch.**
* Outbound: standard OTLP (gRPC) for traces + metrics; endpoint configured via `cascade.otel_endpoint` GUC.
* Inbound: Prometheus scrape endpoint on a separate admin port.
* Trace context propagation via `application_name` so existing PG client tracing libraries (Datadog, Honeycomb, etc.) work without code changes.
* Persistent slow-query plan capture:
```sql
SELECT * FROM cascade_query_plans
  WHERE duration_ms > 100
  ORDER BY captured_at DESC LIMIT 50;
```
* `pg_stat_statements` available as a built-in view — no extension load required.

**(c) v1 reservations.**
* Every hot-path function takes a `&Span` (or `Span::noop()`) parameter from day one. Wiring an exporter later does not require changing call signatures.
* A central `MetricsRegistry` exists in v1 — modules emit counters / histograms even if no scrape endpoint is up.
* The query lifecycle has **clearly named phases** (`parse`, `plan`, `execute`, `wait_io`, `wait_lock`, `serialize`) so span hierarchies remain stable across releases.
* Logs are structured (JSON) from day one with reserved fields: `trace_id`, `span_id`, `database_id`, `query_id`.

### 6.3. Logical Replication & Change Data Capture (CDC)

**(a) Capability.** Per-database logical event stream — `INSERT` / `UPDATE` / `DELETE` events with full row images, ordered by commit, suitable for downstream consumers (Debezium-style sinks, read replicas, audit pipelines, search indexers). PG-compatible `PUBLICATION` / `SUBSCRIPTION` DDL plus a native streaming endpoint. The per-database WAL (§3.4) is a structural advantage: each logical stream maps 1:1 to one database's WAL with no cross-database event interleaving.

**(b) Initial API sketch.**

PG-compatible path:
```sql
CREATE PUBLICATION orders_changes FOR TABLE orders, line_items;
CREATE SUBSCRIPTION replica_a CONNECTION '...' PUBLICATION orders_changes;
```

Native CDC streaming endpoint (gRPC/protobuf, more efficient than the PG protocol):
```proto
service CascadeCDC {
  rpc Subscribe(SubscribeRequest) returns (stream ChangeEvent);
}
message ChangeEvent {
  uint64 lsn          = 1;
  uint64 xid          = 2;
  string database     = 3;
  string schema       = 4;
  string table        = 5;
  Op     op           = 6;     // INSERT / UPDATE / DELETE
  Row    before       = 7;     // null for INSERT
  Row    after        = 8;     // null for DELETE
}
```

**(c) v1 reservations.**
* The WAL record format carries enough information to reconstruct logical events: full column values on `INSERT` / `UPDATE` / `DELETE`, including the pre-image where needed. **This is the single hardest decision to change later — it must be settled in Phase 2.**
* A `WalReader` trait abstracts WAL consumption. Physical recovery is one consumer; logical decoding will be another. No code outside the WAL module reads raw WAL bytes.
* XID / commit ordering is reconstructable from WAL alone (no reliance on in-memory state).
* Schema changes (DDL) are themselves logged as WAL records so the logical decoder can correctly interpret historical row formats.

### 6.4. Disaggregated / Tiered Storage

**(a) Capability.** Pluggable storage backend so Cascade DB can run against local NVMe (v1), object storage (S3 / GCS / Azure Blob), or a Cascade-specific page service (Aurora / Neon style) — without changing higher layers. Enables compute / storage separation, cheap snapshots, branching, and pay-for-what-you-read economics.

**(b) Initial API sketch (Rust trait):**
```rust
pub trait PageStore: Send + Sync {
    async fn read_page(&self, id: PageId, lsn_at_least: Lsn) -> Result<AlignedBuf>;
    async fn write_page(&self, id: PageId, buf: &AlignedBuf, lsn: Lsn) -> Result<()>;
    async fn allocate_extent(&self, db: DatabaseId, size: u64) -> Result<ExtentId>;
    async fn flush(&self, lsn: Lsn) -> Result<()>;

    // Reserved for future backends — v1 implementations may return Unsupported.
    async fn snapshot(&self, db: DatabaseId, lsn: Lsn) -> Result<SnapshotId>;
    async fn branch(&self, from: SnapshotId, into: DatabaseId)  -> Result<()>;
}

pub trait WalStore: Send + Sync { /* analogous, separate from PageStore */ }
```

**(c) v1 reservations.**
* **All** page I/O in v1 — even though it goes only to local `O_DIRECT` files — flows through the `PageStore` trait. No layer above storage may hold raw `RawFd` or `File` handles.
* `PageId` is **globally unique and backend-agnostic**: `(DatabaseId, SegmentId, PageNo)`. Never `(file_path, offset)`.
* WAL storage is a **separate trait** from data page storage — they may live in different backends (e.g., WAL on fast local NVMe, pages on S3).
* The BPM is unaware of where pages come from; eviction and read-through go through `PageStore`.
* `snapshot` / `branch` methods exist on the trait in v1 (returning `Unsupported`) so callers can be written against them today.

---

## 7. Engineering Roadmap

* **Phase 1: Storage Foundation (Complete)** * `O_DIRECT` + `io_uring` page allocation and disk I/O verification (`storage.rs`).
* **Phase 2: Memory & Durability (Next)**
  * Userspace Buffer Pool Manager (BPM) — with **per-database page accounting** (§6.1 reservation).
  * Per-Database Write-Ahead Log (WAL) implementation — with **logical-decoding-ready record format** (§6.3 reservation).
  * `PageStore` / `WalStore` trait abstractions in place from day one (§6.4 reservation).
* **Phase 3: Data Structures**
  * Page layout design (Headers, Slot Arrays, Checksums).
  * B+Tree implementation with Optimistic Lock Coupling.
  * Undo-Log segment manager.
* **Phase 4: Compute & Compatibility**
  * Async Thread-per-Core network listener integration — connections carry `DatabaseId` / tenant context (§6.1 reservation).
  * Postgres Wire Protocol integration (simple + extended query, COPY, prepared statements).
  * PostgreSQL SQL dialect parser via `libpg_query` FFI (isolated in `cascade-pg-parser-sys` / `cascade-pg-parser`); native-Rust planner for PG SQL (not ANSI-only) — including `RETURNING`, `ON CONFLICT`, CTEs, window functions, lateral joins.
  * PostgreSQL type system implementation with wire-format parity (numerics, `jsonb`, arrays, ranges, UUID, timestamptz, etc.).
  * Real `pg_catalog` / `information_schema` backed by Cascade's own metadata, sufficient for ORM and migration-tool introspection.
  * `PL/pgSQL` procedural language runtime.
  * Query lifecycle instrumented with named span phases and `MetricsRegistry` (§6.2 reservation).
* **Phase 5: Extension Compatibility**
  * Extension ABI / hosting layer.
  * Tier-1 extensions: `pgvector`, `PostGIS`, `pg_trgm`, `pg_stat_statements`, `pgcrypto`, `uuid-ossp`, `postgres_fdw`.
  * Published Extension Compatibility Matrix.
* **Phase 6: Native HTAP (Columnar Secondary Storage)**
  * Columnar segment format (compressed, zone-mapped).
  * Row → columnar propagation pipeline under MVCC.
  * Vectorized scan / aggregation execution path.
  * Cost-based planner routing between row and columnar storage.
* **Phase 7+: Future Pillars (per §6)** — enforcement and exporters for capabilities whose hooks already exist in v1:
  * Multi-Tenant Resource Governance (§6.1) — turn accounting into enforcement; ship `RESOURCE GROUP` DDL.
  * First-Class Observability (§6.2) — wire OTLP exporter, Prometheus endpoint, `pg_stat_statements` view, slow-query plan capture.
  * Logical Replication & CDC (§6.3) — logical decoder, PG `PUBLICATION` / `SUBSCRIPTION`, native gRPC CDC stream.
  * Disaggregated / Tiered Storage (§6.4) — S3-backed `PageStore`, snapshot / branch, page-service mode.
