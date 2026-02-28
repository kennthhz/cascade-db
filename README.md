# Product Specification: Cascade DB

## 1. Executive Summary
**Cascade DB** is a high-performance, predictable-latency relational database designed as a drop-in replacement for PostgreSQL. It combines 100% Postgres wire compatibility with a fundamentally rewritten, modern storage kernel built in Rust. 

Cascade DB is designed to eradicate the three "original sins" of legacy PostgreSQL architecture: **VACUUM bloat** (via In-Place MVCC), **Double Buffering** (via `O_DIRECT`), and **Checkpoint Latency Spikes** (via `io_uring`). Furthermore, it introduces a **Per-Database WAL architecture**, solving the "noisy neighbor" replication and recovery bottlenecks inherent in Postgres's global cluster design.

## 2. Target Market & Use Cases
* **High-Throughput OLTP:** Applications constrained by Postgres write-amplification and checkpoint stalling.
* **Multi-Tenant SaaS:** Platforms managing thousands of distinct databases that require isolated replication streams and independent Point-in-Time Recovery (PITR).
* **Ecosystem Migrations:** Teams wanting modern database performance without rewriting their application code, drivers, or replacing their ORMs.

---

## 3. Core Architectural Pillars

### 3.1. Kernel-Bypass Storage I/O (`io_uring` + `O_DIRECT`)
* **Zero Double-Buffering:** Bypasses the Linux Page Cache entirely using `O_DIRECT`. Data exists in only one place in RAM (the Userspace Buffer Pool), effectively doubling the usable memory capacity for hot data.
* **Syscall Eradication:** Utilizes `io_uring` to batch I/O submissions and completions asynchronously. Reduces CPU context-switch overhead to near-zero during heavy I/O.
* **Predictable Latency:** Eliminates the need for blocking `fsync` storms. Dirty pages are trickle-flushed asynchronously, keeping P99 latencies flat during checkpoints.

### 3.2. The "Anti-VACUUM" Engine (In-Place MVCC)
* **In-Place Updates:** Modifies records directly within their original 8KB data pages, preventing table bloat and write-amplification.
* **Undo-Log Rollback Segments:** Preserves MVCC isolation by writing the pre-update row state to a separate, sequential Undo Log.
* **Result:** Eradicates the need for a background VACUUM daemon or table-locking `VACUUM FULL` operations.

### 3.3. Isolated Durability (Per-Database WAL & LSN)
Exploiting the fact that Postgres does not natively support cross-database transactions, Cascade DB physically isolates operational logging.
* **Per-Database Write-Ahead Log (WAL):** Each database maintains its own WAL, LSN sequence, and Transaction ID (XID) space.
* **Isolated Replication:** A massive write spike in Database A has zero impact on the replication stream or WAL size of Database B.
* **Granular Recovery:** Enables trivial, instantaneous Point-in-Time Recovery (PITR) for a single database without requiring a full cluster restore.
* **Sharding Readiness:** Databases are self-contained physical units, making cross-node migration and sharding significantly easier than legacy Postgres.

### 3.4. Userspace Buffer Pool Manager (BPM)
* **Direct Memory Control:** A highly optimized Rust concurrency layer managing a pre-allocated pool of `AlignedBuf` 8KB pages.
* **Eviction:** Utilizes modern eviction algorithms (e.g., LRU-K or CLOCK-sweep) optimized for B+Tree index traversal and sequential scans.

---

## 4. The "Postgres Illusion" (Compatibility Layer)

To achieve zero-friction onboarding, Cascade DB presents an identical surface area to standard PostgreSQL tools (psql, pgAdmin) and ORMs (Prisma, Hibernate, Django).

* **Wire Protocol:** Implements the Postgres TCP wire protocol (`pgwire`) on port 5432.
* **Catalog Facade:** Intercepts complex ORM introspection queries directed at `pg_catalog` and `information_schema`. Returns hardcoded, structurally correct mock metadata, avoiding the need to build and maintain Postgres's convoluted internal catalog architecture.
* **Compute Head:** Leverages an embedded, high-performance SQL execution engine (e.g., Apache DataFusion) restricted to core ANSI SQL (the 80/20 rule: SELECT, INSERT, UPDATE, DELETE, JOIN, Aggregates) tailored to the most common application workloads.

---

## 5. Competitive Differentiation

| Feature / Bottleneck | PostgreSQL (Legacy) | Cascade DB |
| :--- | :--- | :--- |
| **Memory Architecture** | Double Buffered (OS Cache + Shared Buffers) | Zero-Copy Userspace Buffer Pool |
| **Checkpointing** | Blocking `fsync` storms (Latency Spikes) | Continuous Async Flushing (`io_uring`) |
| **MVCC Model** | Append-Only (Causes Bloat) | In-Place Updates + Undo Log |
| **Maintenance** | Requires Autovacuum Tuning | **Zero VACUUM** |
| **WAL & LSN** | Global / Cluster-wide | **Per-Database** (Isolated) |
| **Point-in-Time Recovery**| Cluster-level only | Granular (Per-Database) |

---

## 6. Engineering Roadmap

* **Phase 1: Storage Foundation (Complete)** * `O_DIRECT` + `io_uring` page allocation and disk I/O verification (`storage.rs`).
* **Phase 2: Memory & Durability (Next)**
  * Userspace Buffer Pool Manager (BPM).
  * Per-Database Write-Ahead Log (WAL) implementation.
* **Phase 3: Data Structures**
  * Page layout design (Headers, Slot Arrays, Checksums).
  * B+Tree implementation with Optimistic Lock Coupling.
  * Undo-Log segment manager.
* **Phase 4: Compute & Compatibility**
  * Postgres Wire Protocol integration.
  * SQL Parser / DataFusion integration.
  * Catalog Facade implementation.
