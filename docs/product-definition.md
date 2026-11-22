# Cascade DB: Product Definition

**Status:** Draft for review, revised on 2026-11-22 (second revision the same day: single-box production deployments, separate log and page stores, followers and read replicas; third revision: the single-box edition is a standalone, free and open-source server, and high availability belongs to the managed service). This document defines what Cascade DB is, who it is for, and which problems it solves. How Cascade DB delivers it is described in [`architecture-design.md`](architecture-design.md), which builds on the storage design ([`storage-subsystem-hld-v2.md`](storage-subsystem-hld-v2.md)) and the runtime design ([`runtime-architecture.md`](runtime-architecture.md)) and lists, in its Section 1.1, where it changes them. Where this document and the architecture disagree on mechanism, the architecture wins and this document should be corrected. It was previously named `README.md`. The previous version of this document is kept at [`history/README-2026-11-21-before-revision.md`](history/README-2026-11-21-before-revision.md).

---

## 1. Executive Summary

Cascade DB is a PostgreSQL-compatible relational database for transactional (OLTP) workloads, built from the ground up in Rust for the cloud. Applications connect to it with their existing PostgreSQL drivers, ORMs and tools, and they keep the SQL dialect, data types and core extensions they already depend on. Our goal is that most PostgreSQL applications can move to Cascade DB without rewriting application code.

What changes is the engine underneath. Developers consistently say they love PostgreSQL's product surface (its SQL, its types, its extensions and its ecosystem) and dislike its engine internals (its storage format, its process model, its replication and its upgrades). Cascade DB keeps the surface and replaces the internals. It makes four promises to the people who run it:

1. **No maintenance tax.** Cascade DB does not accumulate table bloat, does not need autovacuum tuning on its default storage engine, and cannot suffer transaction ID wraparound, because its transaction IDs are 63 bits wide.
2. **Committed data is never lost, and the managed service has high availability built in.** Cascade DB separates log durability from page storage in the style of Amazon Aurora. It comes in two editions built from the same engine: a free, open-source single server with local disks, and a managed service on disaggregated storage. In both, a transaction is acknowledged only after its log record is durable. In the managed service, durability comes from a quorum of replicas, and a failed node is replaced by a warm follower within seconds, without data loss and without assembling external failover tooling.
3. **Databases and tenants scale without application sharding.** Every database already has its own independent log, recovery and resource accounting, which suits platforms that run thousands of tenant databases. The same mechanism, called a writing scope, is designed so that a single large database can later spread its write load across several machines without changing the application.
4. **Thousands of connections work without a pooler.** A thread-per-core runtime serves tens of thousands of connections directly, so features that break behind transaction-mode poolers, such as session-level prepared statements and `LISTEN`/`NOTIFY`, keep working.

Underneath these promises are engineering choices that matter to us but are not, on their own, reasons for a customer to switch: a thread-per-core runtime on `io_uring`, a Rust codebase with a single audited C dependency (the PostgreSQL parser), and a storage layer where the log is the database. Sections 4 and 5 describe them.

---

## 2. The Problems We Solve

We ranked what practitioners most often say they love and hate about PostgreSQL, using developer surveys, practitioner blogs, public postmortems and our own review of competitors. The full ranking with sources is in the project doc "What people love and hate about Postgres". The table below lists the ten most cited complaints in order, what Cascade DB does about each one, and how far upstream PostgreSQL has moved on its own.

| Rank | Complaint about PostgreSQL | What Cascade DB does | Status in upstream PostgreSQL (through PG 19) |
| :--- | :--- | :--- | :--- |
| 1 | VACUUM, table bloat and autovacuum tuning | The default index-organized engine updates rows in place and keeps old versions in an undo log, so there is no vacuum to tune. The heap engine, kept for compatibility, vacuums incrementally one page at a time. (§4.1) | Eased but not removed. PG 19 adds `REPACK (CONCURRENTLY)` and parallel index vacuuming, but bloat is still inherent to the storage format. |
| 2 | Extra writes on every update (write amplification) | In-place updates write only the changed columns to undo. Secondary indexes point at the primary key, so they are rewritten only when an indexed column changes. (§4.1) | Largely unchanged. HOT updates help only when no indexed column changes and the new version fits on the same page. |
| 3 | One operating-system process per connection | One worker thread per CPU core, serving tens of thousands of connections without an external pooler. (§4.6) | Unchanged. A multithreaded PostgreSQL is still being explored and is years away. |
| 4 | Transaction ID wraparound | Transaction IDs are 63-bit counters per writing scope, and commit order uses a 64-bit hybrid logical clock, so nothing can wrap. (§4.1) | Mitigated by better freezing, but transaction IDs are still 32 bits. |
| 5 | Painful major-version upgrades | Compute is stateless and storage formats are versioned, so Cascade DB upgrades roll through a managed-service cluster without dump-and-restore, and the open-source edition needs only a restart. The PostgreSQL behavior a database presents is a setting, not an on-disk format. (§4.8) | Improved in PG 17 and 18, but a major upgrade still needs downtime or a logical replication project. |
| 6 | No built-in high availability or automatic failover | A quorum-replicated log, fenced ownership and warm followers make failover a built-in feature of the managed service that takes seconds and loses no acknowledged commits. (§4.3) | Still requires external tools such as Patroni and a proxy. |
| 7 | Replication limits (whole-cluster physical replication, no DDL in logical replication) | Each database has its own log, so replication and recovery are per database. Schema changes are logged, so the logical change stream can carry DDL. (§4.4, §8.3) | Logical replication improved in PG 17, 18 and 19, but DDL is still not replicated. |
| 8 | Unpredictable query plans and no hints | The planner captures plans, lets operators pin them, detects plan regressions, and accepts `pg_hint_plan` hint syntax. (§4.7) | PG 19 adds the `pg_plan_advice` extension. Core hints are still not supported. |
| 9 | I/O inefficiency (double buffering, checkpoint spikes) | Compute never writes data pages and never checkpoints. Storage builds pages from the log in the background. (§4.2) | PG 18 added asynchronous I/O, and PG 19 made its worker pool self-sizing. |
| 10 | Operational tuning burden | No vacuum settings on the default engine, a self-sizing page cache, and a goal of zero required tuning settings for a standard deployment. (§4.1) | Slowly improving defaults. |

Two lessons from this ranking shape the rest of the document. First, items 1 to 3 remain unsolved upstream, and they are the same problems that pushed OpenAI to move new write-heavy workloads off its main PostgreSQL deployment ([OpenAI, January 2026](https://openai.com/index/scaling-postgresql/)). Second, item 9 is the only complaint that upstream is closing quickly, so we treat our I/O design as a means to the other promises rather than as a headline feature.

The drop-in promise also has to protect what people love. The most loved qualities after reliability are the extension ecosystem, the rich SQL dialect, the type system and the fact that every driver and tool speaks PostgreSQL. Section 5 explains how we protect them and where we fall short.

---

## 3. Target Customers and Deployment Model

**Deployment model.** Cascade DB comes in two editions, built from the same components:

- **The managed service (disaggregated mode).** Compute nodes, log store nodes and page store nodes run on separate machines, as in Amazon Aurora. Storage scales independently of compute, and customers never operate the storage layer. This edition has a control plane, which places databases on nodes, detects failures and promotes followers, and it is the only edition with quorum durability, warm followers, read replicas and automatic failover.
- **The open-source edition (single-box mode).** One server runs compute, a log store and a page store on its own local disks, like a conventional database server. It is free and open source, downloadable, and needs no control plane. It suits development, small production deployments and customers who run Cascade DB on their own hardware or in their own cloud account. It has no followers or failover: if the server fails, its databases are unavailable until it restarts, and it then recovers from its local disk without losing any acknowledged commit.

The managed service comes first, but the open-source edition is a supported product for production use, not a development convenience. Both editions give the same SQL behavior, storage engines and durability rule: a commit is acknowledged only after its log record is durable. Customers who need high availability use the managed service. A customer-run cluster of single servers is not part of the product.

**Primary customer: multi-tenant SaaS platforms.** These are companies that run hundreds to thousands of tenant databases, one per customer. Today they struggle because PostgreSQL shares one write-ahead log, one replication stream and one recovery process across every database in a cluster. Cascade DB gives each database its own log, its own point-in-time recovery, its own branches and its own resource accounting, so one tenant's write spike or restore does not affect any other tenant.

**Secondary customer: write-heavy OLTP applications that have outgrown a single PostgreSQL primary.** These teams suffer from bloat, vacuum tuning, connection limits and replica lag, and they want to avoid a multi-year sharding project. Cascade DB removes the maintenance tax and the pooler today, and it is designed to let a large database spread its writes across machines later without application changes.

**Emerging customer: developer and AI-agent platforms.** These platforms create many small, short-lived databases, often programmatically. When Databricks agreed to acquire Neon in 2025, it reported that over 80% of the databases on Neon were created by AI agents rather than by people ([Databricks](https://databricks.com/company/newsroom/press-releases/databricks-agrees-acquire-neon-help-developers-deliver-ai-systems)). These customers need databases that are created in seconds, branched cheaply and restored to any point in time. Cascade DB's log-based storage makes branching and restore inexpensive (§4.5).

**Migration-minded teams.** Any of the customers above may arrive with an existing PostgreSQL deployment. They need a safe path in, which Section 6 describes.

---

## 4. Product Pillars

### 4.1. No Maintenance Tax

Cascade DB has two storage engines behind one table access interface, and both share the same log, page cache, transaction manager and storage service.

**The index-organized engine is the default.** A table that has a primary key is stored as a B-tree ordered by that key, with each row in a leaf of the tree. When a row is updated, the engine first writes the previous values of the changed columns to an undo log and then overwrites the row in place. Readers that need an older version follow the undo chain backwards until they find the version their snapshot should see. Undo space is reclaimed in whole segments once no running transaction can need it. Because rows are never copied to new locations, tables do not bloat, and there is no vacuum process to schedule or tune.

**The heap engine exists for compatibility.** Tables without a primary key, and tables whose applications depend on PostgreSQL's heap behavior, use a PostgreSQL-style heap in which each update writes a new row version. Unlike PostgreSQL, Cascade DB cleans up heap tables continuously and incrementally. Pages are pruned when they are accessed and running low on free space, and a background process vacuums one page at a time with no table-wide pause. The heap engine therefore still performs cleanup work, but operators never need to run `VACUUM FULL` or plan around a freeze.

**Updates write less.** On the index-organized engine, secondary index entries point at the primary key rather than at a physical row location. An update that does not change an indexed column therefore writes nothing to any secondary index, and the undo record contains only the columns that changed. Index entries carry their own visibility information, so index-only scans work on both engines without a separate visibility map.

**Transaction IDs cannot wrap around.** Each writing scope numbers its transactions with a 63-bit counter, and commit order comes from a 64-bit hybrid logical clock rather than from transaction IDs. There is no freezing and no emergency anti-wraparound vacuum on either engine.

**Undo growth is bounded and visible.** An undo log has its own failure mode, which PostgreSQL users may know from Oracle as "snapshot too old" and from MySQL as a growing history list. A transaction that stays open for hours forces the engine to keep every older row version it might read. Cascade DB reports undo retention per database, warns when a long-running transaction is holding back cleanup, and lets operators set a maximum transaction age and a maximum undo size per database. The exact behavior when a limit is reached (cancel the oldest transaction or fail the reader) is an open design decision.

**Fewer settings to tune.** With no vacuum settings on the default engine and a page cache that sizes itself to the machine, our goal is that a standard deployment needs no tuning settings at all. Settings that remain exist for limits and policy, not for keeping the database healthy.

**Showcase workload: PostgreSQL as a job queue.** Many teams use PostgreSQL as a queue with `SELECT ... FOR UPDATE SKIP LOCKED`, and this is where heap bloat hurts most, because every job row is inserted, updated and deleted within minutes. We will publish queue benchmarks alongside standard OLTP benchmarks to demonstrate the difference.

### 4.2. Storage Where the Log Is the Database

Cascade DB follows the model that Amazon Aurora made popular: compute nodes send only log records to storage, and storage builds the data pages.

**Compute never writes data pages.** When a transaction changes a row, the compute node appends a log record, applies the same change to its cached copy of the page, and moves on. The only durable write on the commit path is the log. The page cache in compute memory is a cache of the log and is never flushed back to storage.

**There are no checkpoints on compute.** Storage has two separate parts. The **log store** makes log records durable and keeps them in order. The **page store** reads the log from the log store, folds the changes into data pages in the background one page at a time, and serves pages to compute. When a compute node needs a page it does not have, the page store applies any outstanding changes and returns the current image. The two parts are separate components in both editions: on separate machines in the managed service, and in the same process on one server in the open-source edition. Because no component ever needs to flush all dirty pages at once, there are no checkpoint latency spikes, and there is no double buffering between our page cache and the operating system's file cache on the compute node.

**Durability comes from the log.** In the managed service, every log record is sent to several log store replicas, and a write is acknowledged once a quorum has it. The number of replicas and the quorum size are configurable, with a default of three replicas and a quorum of two, on dedicated log store nodes in different failure domains. In the open-source edition, a write is acknowledged once its log record is flushed to the server's own disk, and backups to an archive protect against losing that disk. In both editions, commits never wait for the page store.

**Data is encrypted at rest.** Pages and log records are encrypted with one key per writing scope, which the page store uses when it applies changes to pages.

**Why this pillar matters to customers.** On its own, this design is an implementation detail. It matters because it is what makes the other promises possible: predictable latency without checkpoint spikes (§4.6), failover without data loss (§4.3), and cheap branching and restore (§4.5).

### 4.3. Built-in High Availability and Failover

High availability is part of the managed service, not something the customer assembles from Patroni, etcd and a proxy. The open-source edition is a single server: it never loses an acknowledged commit, but it is unavailable while it restarts, and it has no followers or read replicas. Everything below describes the managed service.

**No acknowledged commit is ever lost.** A transaction is reported as committed only after its commit record is durable on a quorum of log store replicas. A compute failure at any moment therefore loses no acknowledged work, which means the recovery point objective (RPO) for acknowledged commits is zero.

**Failover is a handoff, not a restore.** Each writing scope is owned by exactly one compute node at a time, under an ownership number called an epoch. Every scope also has at least one **follower**: a warm replica on another node that receives the log from the leader as it is written, keeps its page cache and transaction state current, and serves reads. When the leader fails, a follower takes ownership with a higher epoch, and the log store rejects any further log records from the old owner. The new owner confirms where the durable log ends, rolls back any transaction that was still in flight when the old node failed, and starts serving with a cache that is already warm. There is no data to copy and no log to replay before it can serve, which is what makes failover take seconds. Every node is active: each one leads some scopes and follows others, so a failed node's load spreads across the survivors.

**Read replicas scale reads without loading the leader.** Beyond its few followers, a scope can have any number of **read replicas**. Followers receive the log directly from the leader, because they must be ready to take over. Read replicas instead read the log from the log store and fetch pages from the page store, so adding them puts no extra load on the leader. This avoids the problem large PostgreSQL deployments hit when one primary must stream its entire log to dozens of replicas. Followers and read replicas both serve a read at a given timestamp once the leader has promised that no later commit can appear before that timestamp.

**Targets.** We will publish a target recovery time objective (RTO) for failover and measure it continuously. The number will be set once the failover design is complete.

### 4.4. Isolated Databases and Writing Scopes

PostgreSQL does not support transactions that span databases, and Cascade DB uses that fact to isolate databases completely.

**Every database is an independent unit.** Each database has its own log, its own log sequence numbers, its own transaction ID space, its own catalog (including its roles), its own encryption key, and its own log and pages in the log store and page store. A heavy write spike in one database does not grow another database's log or delay another database's replication. A database can be restored, branched or moved to another node without touching any other database in the cluster.

**A writing scope is the unit of single-writer ownership.** In version 1, each database is one writing scope, and its catalog lives in that same scope, so a transaction can change both the schema and the data, as in PostgreSQL. The design also allows a scope to be a single table or a shard of a table, so that one large database can spread its write load across several compute nodes in a later release. All ordering, conflict detection and logging stay local to a scope, which is why Cascade DB does not need distributed transactions for ordinary writes. Reads that span scopes use a single timestamp from a hybrid logical clock so that they see a consistent state.

**Limits that apply once a database is split into several scopes.** These limits do not apply in version 1, where a database's tables share one scope and behave exactly like PostgreSQL. They apply only to tables that an operator explicitly splits into separate scopes in a later release, and we state them now so that nobody is surprised:

- A transaction that writes data can write to only one scope. A transaction that writes to two tables in different scopes is rejected.
- Schema changes (DDL) are the one exception. Once a database is split, its catalog moves to a separate catalog scope, and DDL coordinates across scopes using two-phase commit with the catalog scope as the coordinator.
- Sharded tables are planned to have local secondary indexes only, and a unique index on a sharded table must include the shard key. This is pending confirmation.
- Foreign keys are planned not to be enforced across scopes. This is also pending confirmation.

### 4.5. Branching and Point-in-Time Restore per Database

Because the log is the database, a past state of a database is just its pages at an earlier point in the log, and a branch is a new database that starts from that point and writes its own log from there.

- **Point-in-time restore per database.** Operators can restore one database to any point within its retention window without restoring the rest of the cluster.
- **Copy-on-write branches.** A branch shares unchanged pages with its parent, so creating it does not copy data, and it costs storage only for the pages it changes. Branches suit development, testing, schema-migration rehearsals and AI-agent workflows.
- **Fast database creation.** Creating a new empty database requires only a catalog entry and a new scope in the log store and page store, so it is designed to take seconds rather than minutes.

The restore window is kept in an archive in object storage, which receives each database's log and periodic page images, so that long windows do not occupy the storage layer's low-latency disks. Because a database may later span several scopes, a restore point is named by a timestamp, so that all of the database's scopes are restored to the same consistent moment. The retention window and the tooling around restore are not yet designed.

### 4.6. Connections at Scale Without a Pooler

**The problem.** PostgreSQL starts one operating-system process for every connection. Each process uses its own memory, and thousands of them cause heavy context switching, so almost every production deployment adds a connection pooler such as PgBouncer. Transaction-mode pooling then breaks session-level features, including session prepared statements, `LISTEN`/`NOTIFY`, temporary tables, session advisory locks and `SET` commands.

**How Cascade DB works instead.** Cascade DB runs one worker thread pinned to each CPU core, and each worker has its own `io_uring` ring for asynchronous I/O. Each database is given a set of cores that grows and shrinks with its load, and a very busy table can be given cores of its own. A new connection is assigned to the least-loaded core in its database's set when it is accepted, and stays on that core for its whole life, so per-connection state never needs locks or atomic operations. Within a database, the shared parts of the engine, such as the page cache, the log and the transaction manager, are shared-memory structures designed for low contention, so that a single database can use many cores. Thread-per-core applies to connections and sessions; a database that needs only one core never contends at all.

**What customers get.** A single node handles tens of thousands of concurrent connections, and every session-level PostgreSQL feature keeps working because there is no pooler in between. Teams no longer need to deploy, monitor and tune PgBouncer or Pgpool.

**Choosing latency or throughput.** Two settings let operators decide how a machine's cores are used:

- The cluster-wide SMT setting is either `physical`, which runs one worker per physical core and leaves hyperthread siblings idle for the most predictable tail latency (the default), or `logical`, which runs one worker per virtual CPU for the highest throughput.
- The per-database NUMA setting is either `single`, which keeps a database's memory, log and workers on one NUMA node for the most predictable latency (the default), or `cross`, which lets a database use every NUMA node for the highest throughput. Different databases in the same cluster can use different settings.

**Fairness between short and long queries.** Long-running queries yield the core at batch boundaries and whenever they exceed a CPU time budget, so short transactional queries keep a flat tail latency even when analytical queries share the same core. A stall detector logs any task that runs too long without yielding. The full runtime design is in [`runtime-architecture.md`](runtime-architecture.md).

### 4.7. Predictable Query Plans

Cascade DB's planner is a new implementation written in Rust. That gives us the chance to fix a long-standing PostgreSQL complaint, and it also creates our biggest migration risk: a new planner will sometimes choose a different plan from PostgreSQL for the same query, and a worse plan is the most likely way a migrated application gets slower.

We address both sides with the following features:

- **Plan capture.** The plans of frequently run and slow queries are recorded, together with their run-time statistics, and can be queried with SQL.
- **Plan pinning.** An operator can pin a known-good plan to a query so that it does not change when statistics change.
- **Regression detection.** When a query's plan changes and its latency gets worse, Cascade DB flags the change and can fall back to the previous plan.
- **Hint compatibility.** Cascade DB accepts the hint comment syntax of the widely used `pg_hint_plan` extension, and it will track the `pg_plan_advice` format that PostgreSQL 19 introduced.
- **Migration comparison.** During a migration, the same queries can be explained on the source PostgreSQL database and on Cascade DB so that plan differences are found before cutover.

### 4.8. Online Upgrades and Version Independence

**Upgrading Cascade DB.** Compute nodes hold no durable state, and every log record and page format carries a version number that the page store must support for as long as data in that format exists. In the managed service, a Cascade DB release can therefore be rolled out one node at a time. The open-source edition, which has no follower to take over, needs a short restart. In the managed service, each writing scope sees only a brief pause in writes while its ownership moves to an upgraded node. No dump-and-restore and no offline `pg_upgrade` step is ever required.

**Choosing PostgreSQL behavior.** The PostgreSQL version whose behavior a database presents, including its SQL dialect, functions and catalog contents, is designed to be a per-database setting that does not depend on the on-disk format. Moving a database from PostgreSQL 17 behavior to PostgreSQL 18 behavior is therefore a setting change, which can be tested on a branch first (§4.5), rather than a data migration.

### 4.9. A Memory-Safe Foundation in Rust

Every component on the critical path, including storage, the page cache, the log, transactions, indexes, the executor, the wire protocol and the runtime, is written in Rust.

- **Memory safety without garbage collection.** Rust removes whole classes of bugs, such as use-after-free, buffer overflows and data races, without the pauses that a garbage collector would add to tail latency.
- **Invariants checked by the compiler.** Transaction states, page latches, catalog versions and wire-protocol messages are encoded in types, so many mistakes are caught at compile time rather than in code review.
- **Safe concurrency.** The borrow checker is what makes the thread-per-core model, with its strictly per-core connection state, practical to build and refactor.

We see Rust as a way to move faster with fewer defects. It is not a criticism of PostgreSQL, whose C codebase has a strong security record.

**One deliberate exception.** Cascade DB reuses `libpg_query`, which packages the upstream PostgreSQL parser as a C library, so that it parses SQL exactly as PostgreSQL does. Re-creating the grammar by hand would take years and would still drift from PostgreSQL on the edge cases that real ORMs produce. The C code is isolated in one audited pair of crates (`cascade-pg-parser-sys` for the raw bindings and `cascade-pg-parser` for the safe Rust interface), so that it stays contained and replaceable. No other PostgreSQL C code is reused.

---

## 5. PostgreSQL Compatibility

Our compatibility goal is that most existing PostgreSQL applications run unmodified. We treat compatibility as a product requirement, and we measure it publicly.

**Baseline version (proposed, to be confirmed).** Version 1 targets the behavior of PostgreSQL 17. PostgreSQL 18 and 19 have since been released, so this choice should be revisited against the versions that `libpg_query` supports. Later releases will add newer PostgreSQL versions as selectable behaviors (§4.8).

### 5.1. Wire Protocol

Cascade DB implements the PostgreSQL wire protocol on port 5432. This includes the simple and extended query protocols, prepared statements, portals, `COPY`, cursors and `LISTEN`/`NOTIFY`. It supports SCRAM-SHA-256 and MD5 password authentication and TLS. Standard clients and drivers, including `psql`, `pgAdmin`, `pg_dump`, `pg_restore`, `libpq`, JDBC, `pgx`, `node-postgres` and `psycopg`, work without changes.

### 5.2. Data Types

Cascade DB implements the PostgreSQL type system with text and binary wire formats that match PostgreSQL byte for byte, so drivers decode values exactly as they do today. This covers numeric types (`smallint`, `integer`, `bigint`, `numeric`, `real`, `double precision` and the `serial` family), character types (`text`, `varchar`, `char` and `citext`), date and time types with PostgreSQL's time zone rules (`date`, `time`, `timestamp`, `timestamptz` and `interval`), `bytea` and `uuid`, structured types (`json`, `jsonb` with full operator and path support, `hstore`, arrays, ranges, composite types and enums), and network and geometric types (`inet`, `cidr`, `macaddr`, `point`, `line`, `polygon` and others).

### 5.3. SQL Dialect

Cascade DB targets the full PostgreSQL dialect that applications and ORMs actually produce, not a reduced ANSI SQL subset.

- **Parsing** uses `libpg_query` (§4.9), so the grammar matches PostgreSQL exactly. Planning, execution and everything after parsing are written in Rust.
- **Data changes** include `SELECT`, `INSERT`, `UPDATE`, `DELETE` and `MERGE`, with PostgreSQL clauses such as `RETURNING`, `ON CONFLICT ... DO UPDATE` and `DO NOTHING`, and `FOR UPDATE` or `FOR SHARE` with `SKIP LOCKED` or `NOWAIT`.
- **Query features** include common table expressions (recursive and `MATERIALIZED`), window functions, lateral joins, `DISTINCT ON`, `GROUPING SETS`, `ROLLUP`, `CUBE` and full-text search with `tsvector` and `tsquery`.
- **Schema definition** covers tables, indexes (B-tree, hash, GIN, GiST and BRIN), views, materialized views, schemas, sequences, constraints and triggers.
- **Transactions** support savepoints, the Read Committed, Repeatable Read and Serializable isolation levels (Read Uncommitted behaves as Read Committed, as in PostgreSQL), and advisory locks.
- **Stored code** uses PL/pgSQL for functions, procedures and triggers.
- **System catalogs.** `pg_catalog` and `information_schema` are real, queryable catalogs rather than canned responses, so ORMs, migration tools (Flyway, Liquibase, Alembic and Prisma Migrate) and monitoring tools can introspect the database correctly.

### 5.4. Extensions

Many PostgreSQL applications are really "PostgreSQL plus an extension" applications. PostgreSQL extensions written in C call deep into PostgreSQL's internals, including its memory management, buffer manager and table and index interfaces, so a new engine cannot load them unchanged. Other PostgreSQL-compatible databases show the trade-off: YugabyteDB keeps extension support by reusing PostgreSQL's own code, while CockroachDB and Aurora DSQL support few or no extensions.

Cascade DB therefore commits to **native reimplementations** of the most depended-on extensions, in this priority order:

1. **`pgvector`** for vector similarity search, which is essential for AI and retrieval workloads. Because Cascade DB's indexes carry their own visibility information, vector indexes can be fully consistent with transactions, and filtered vector search is a design goal.
2. **`pg_trgm`** for trigram similarity and fuzzy search.
3. **`pg_stat_statements`**, built in as a system view so that no extension needs to be loaded.
4. **`pgcrypto`** and **`uuid-ossp`** for cryptographic functions and UUID generation.
5. **`hstore`** and **`citext`**, which are part of the type system (§5.2).

Two items are research rather than commitments: **PostGIS**, which is large enough that a native version is a major project, and a **compatibility layer for hosting unmodified C extensions**. We will publish an extension compatibility matrix that marks each extension as native, planned, under research or out of scope, so that teams can check before migrating.

### 5.5. Known Differences from PostgreSQL

A drop-in replacement must be honest about where it is not a drop-in. The known differences are:

- **Physical row identity.** On the index-organized engine, rows have no stable `ctid`, so applications that read `ctid` need to use the heap engine. On both engines, the system columns `xmin` and `xmax` cannot carry PostgreSQL's 32-bit transaction IDs, so applications that read them, for example for optimistic locking, need to change.
- **Maintenance commands.** `VACUUM`, `VACUUM FULL`, `CLUSTER` and `REPACK` are accepted for compatibility. They do nothing on the index-organized engine and trigger incremental cleanup on the heap engine.
- **Physical replication and file-level tools.** PostgreSQL physical replication, `pg_basebackup`, `pg_upgrade` and other tools that read PostgreSQL's data files do not apply, because Cascade DB's storage format is different. Backup, replicas and upgrades are built in instead (§4.3, §4.5, §4.8).
- **C extensions.** Extensions that are not reimplemented natively are not available (§5.4).
- **Multi-scope tables.** The limits in §4.4 apply once a table is split into its own writing scope in a later release.

### 5.6. How We Measure Compatibility

We will publish, for each release, the pass rate on the PostgreSQL regression test suite, the pass rates on the test suites of major ORMs and frameworks (such as Rails, Django, Prisma, Hibernate and SQLAlchemy) and migration tools (such as Flyway and Liquibase), and the extension compatibility matrix.

---

## 6. Migrating into Cascade DB

A drop-in replacement needs a safe way in, so migration support is part of version 1 rather than a later feature.

1. **Assess.** A compatibility check connects to the source PostgreSQL database and reports unsupported extensions, uses of `ctid` or `xmin`, and other known differences (§5.5).
2. **Copy and follow.** Cascade DB subscribes to a PostgreSQL publication on the source database, copies the existing data, and then applies ongoing changes through PostgreSQL logical replication, so that the source keeps serving traffic during the copy.
3. **Compare.** Queries can be run and explained against both databases to find plan and result differences before cutover (§4.7).
4. **Cut over.** The application switches its connection string once replication has caught up. Because the source keeps running, rolling back means switching the connection string back.

For smaller databases, `pg_dump` and `pg_restore` also work, as described in §5.1.

---

## 7. How Cascade DB Compares

Customers do not compare Cascade DB against plain PostgreSQL. They compare it against managed PostgreSQL services and other PostgreSQL-compatible databases. The table below summarizes the main alternatives as of November 2026 and where Cascade DB differs.

| Alternative | What it is | Where Cascade DB differs |
| :--- | :--- | :--- |
| Amazon Aurora PostgreSQL | The PostgreSQL engine on Amazon's log-based distributed storage. | Aurora keeps PostgreSQL's heap, VACUUM and process-per-connection model, and it has one writer per cluster unless the customer adopts Aurora Limitless Database, which shards. Cascade DB uses the same storage idea but replaces the engine above it, and it isolates each database. |
| Google AlloyDB | The PostgreSQL engine on Google's disaggregated storage, with a columnar engine for analytics. | Same difference as Aurora: the PostgreSQL engine and its maintenance model are unchanged. |
| Azure HorizonDB | A PostgreSQL service with a new distributed storage layer and large scale-out compute. | HorizonDB targets very large single deployments. Cascade DB focuses on removing the maintenance tax and on isolating many tenant databases. |
| Neon (Databricks Lakebase) | PostgreSQL compute on separate storage, with branching and scale-to-zero. | Neon shows the demand for branching and fast creation, which Cascade DB matches (§4.5). Neon still runs the unchanged PostgreSQL engine. |
| Supabase with OrioleDB and Multigres | OrioleDB is an undo-based storage engine for PostgreSQL in public beta. Multigres adds pooling, failover and sharding and is in private alpha. | OrioleDB validates the undo-log approach. Cascade DB builds undo, high availability and connection handling into one engine rather than adding them to PostgreSQL. |
| PlanetScale Postgres and Neki | PostgreSQL on local NVMe drives, with the Neki project adding sharding. | PlanetScale scales through sharding layers. Cascade DB aims to scale writes through writing scopes without an application sharding project. |
| YugabyteDB and CockroachDB | Distributed SQL databases that speak the PostgreSQL protocol. | These systems spread data and transactions across nodes by default, so many transactions coordinate between machines, and CockroachDB has notable compatibility gaps. Cascade DB keeps every transaction on a single writer and stays closer to PostgreSQL behavior. |
| Amazon Aurora DSQL | A serverless, distributed database with a PostgreSQL-compatible interface. | DSQL removes triggers, PL/pgSQL and temporary tables and limits transaction size. Cascade DB keeps these features. |

---

## 8. Later Pillars (Reserved Design Space)

The following capabilities are committed product directions but are not part of version 1. Each one lists the capability, an initial sketch, and the reservations that version 1 code must respect so that the capability can be added later without breaking changes.

### 8.1. Multi-Tenant Resource Governance

**Capability.** Per-database and per-role limits on CPU, memory, I/O bandwidth, connections, statement memory and statement time, enforced inside the engine without proxies or sidecars. This directly serves the multi-tenant SaaS customer (§3).

**Initial sketch (PostgreSQL-style DDL):**

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

-- Live usage compared with limits, per database
SELECT * FROM cascade_resource_usage;
```

**Version 1 reservations.**

- Every operation that consumes CPU, including parsing, planning, execution, page cache eviction and log writes, runs in a context that carries the database's identity. In version 1 the identity is recorded even though limits are not enforced.
- Page cache usage is accounted per database from the first release, so adding a limit later is a single check.
- Every `io_uring` submission carries a tenant tag, so I/O can later be partitioned or weighted per database.
- Connection objects carry their database context through the whole request.
- Limits that are eventually enforced appear as rows in `pg_settings` where that makes sense.

### 8.2. First-Class Observability

**Capability.** OpenTelemetry traces, metrics and structured logs out of the box, with no extensions or exporters to install. This includes `pg_stat_statements`-compatible views, persistent capture of slow-query plans (shared with §4.7), and a Prometheus scrape endpoint.

**Initial sketch.**

- Traces and metrics are sent over OTLP (gRPC) to an endpoint set by the `cascade.otel_endpoint` setting.
- A Prometheus scrape endpoint is served on a separate administration port.
- Trace context is passed through `application_name`, so existing PostgreSQL client tracing libraries work without code changes.
- Captured plans can be queried with SQL:

```sql
SELECT * FROM cascade_query_plans
  WHERE duration_ms > 100
  ORDER BY captured_at DESC LIMIT 50;
```

**Version 1 reservations.**

- Every function on the hot path takes a tracing span parameter from the first release, so connecting an exporter later does not change any function signature.
- A central metrics registry exists in version 1, and modules record counters and histograms even when no scrape endpoint is running.
- The query lifecycle has stable, named phases (`parse`, `plan`, `execute`, `wait_io`, `wait_lock` and `serialize`), so trace structure stays the same across releases.
- Logs are structured JSON from the first release, with reserved fields for `trace_id`, `span_id`, `database_id` and `query_id`.

### 8.3. Logical Replication and Change Data Capture (Outbound)

Inbound logical replication from PostgreSQL is part of version 1 because it is the migration path (§6). This section covers the outbound direction.

**Capability.** A per-database stream of `INSERT`, `UPDATE` and `DELETE` events with full row images, ordered by commit, for downstream consumers such as Debezium-style sinks, search indexers, audit pipelines and data lakes. Because schema changes are logged, the stream can also carry DDL, which PostgreSQL's logical replication still cannot do. Cascade DB will support PostgreSQL's `PUBLICATION` and `SUBSCRIPTION` commands and a native streaming endpoint.

**Initial sketch.** The PostgreSQL-compatible path:

```sql
CREATE PUBLICATION orders_changes FOR TABLE orders, line_items;
CREATE SUBSCRIPTION replica_a CONNECTION '...' PUBLICATION orders_changes;
```

A native streaming endpoint over gRPC, which is more efficient than the PostgreSQL protocol:

```proto
service CascadeCDC {
  rpc Subscribe(SubscribeRequest) returns (stream ChangeEvent);
}
message ChangeEvent {
  uint64 lsn          = 1;
  uint64 commit_ts    = 2;     // hybrid logical clock commit timestamp
  string database     = 3;
  string schema       = 4;
  string table        = 5;
  Op     op           = 6;     // INSERT, UPDATE, DELETE or DDL
  Row    before       = 7;     // empty for INSERT
  Row    after        = 8;     // empty for DELETE
}
```

**Version 1 reservations.**

- Log records carry full row images for inserts, updates and deletes, including the previous image where needed. This was settled during the log format design, because it is the hardest decision to change later.
- A single log-reader interface is the only way to consume the log. Recovery is one consumer, and logical decoding will be another.
- Commit order can be reconstructed from the log alone, without relying on in-memory state.
- Schema changes are logged, so a decoder can interpret historical row formats correctly.

### 8.4. Analytics on Operational Data

**Capability.** Lightweight analytics, such as in-product dashboards and reporting, on transactional data without a separate ETL pipeline. The market has moved toward connecting PostgreSQL to lakehouse formats such as Apache Iceberg, so we plan this in two steps.

1. **Export first.** Continuously export tables to Apache Iceberg through the change stream (§8.3), so that existing analytics engines can query fresh operational data.
2. **Built-in columnar storage later.** An optional columnar copy of selected tables or columns (for example, enabled with `ALTER TABLE ... ENABLE COLUMNAR`), kept consistent with the row store under the same snapshots. It would use compressed column segments with per-segment minimum and maximum values for pruning, a vectorized execution path, and planner routing between row and columnar access. The runtime's cooperative yielding (§4.6) already protects transactional latency when analytical queries share a core.

---

## 9. Open Product Decisions

The following decisions are still open. This document states a proposal for some of them, and each should be confirmed or changed explicitly.

| Decision | Current proposal |
| :--- | :--- |
| Deployment model: managed service, customer-run software, or an engine sold to platforms | Decided: two editions, with the managed service first (§3). The managed service runs in disaggregated mode with a control plane and high availability. The open-source edition is a standalone single server with no control plane. |
| Whether splitting a database into several writing scopes is a version 1 feature | Not in version 1. Version 1 has one scope per database, which also holds its catalog (§4.4). |
| Whether built-in columnar storage is in version 1 | Not in version 1. Iceberg export comes first (§8.4). |
| Which engine new tables use by default | The index-organized engine for tables with a primary key, and the heap engine otherwise (§4.1). |
| PostgreSQL behavior baseline for version 1 | PostgreSQL 17 (§5). |
| Behavior when undo limits are reached | Not yet proposed (§4.1). |
| Product targets: tail latency under load, connections per node, failover time, branch creation time and restore time | To be set once the related designs are complete. |
| License | Partly decided: the single-server edition is free and open source (§3). The specific open-source license is not yet chosen, and neither is how the managed service's control plane is licensed. Practitioners rank open source and community ownership among PostgreSQL's most loved qualities, so this choice affects adoption. |

---

## 10. Roadmap

The phase list that used to be here is out of date. Progress is tracked in the repository history and in the work packages of the storage design ([`storage-subsystem-hld-v2.md`](storage-subsystem-hld-v2.md), §18). The architecture is in [`architecture-design.md`](architecture-design.md), and the order in which it is built is in [`architecture-implementation-phases.md`](architecture-implementation-phases.md).

---

## Appendix: Section Changes from the Previous Version

This document was previously named `README.md`. Older design documents refer to sections of its previous version. This table maps the old section numbers to the new ones.

| Previous section | Topic | New section |
| :--- | :--- | :--- |
| §3.1 | Kernel-bypass I/O | §4.2 and §4.6 |
| §3.2 | Thread-per-core execution | §4.6 |
| §3.3 | In-place MVCC without VACUUM | §4.1 |
| §3.4 | Per-database WAL | §4.4 |
| §3.5 | Buffer pool manager | §4.2 (now a page cache that is never written back) |
| §3.6 | Rust foundation | §4.9 |
| §3.7 | Columnar analytics | §8.4 |
| §4 | PostgreSQL compatibility | §5 |
| §5 | Competitive differentiation | §7 |
| §6.1 | Resource governance | §8.1 |
| §6.2 | Observability | §8.2 |
| §6.3 | Logical replication and CDC | §8.3 (outbound) and §6 (inbound) |
| §6.4 | Disaggregated storage | §4.2, §4.3 and §4.5 |
| §7 | Engineering roadmap | §10 |
