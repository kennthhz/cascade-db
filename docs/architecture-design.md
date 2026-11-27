# Cascade DB: Architecture, Components and Contracts

**Status:** Draft v0.10.2, 2026-11-22, for iteration with Kenneth. v0.10.2 fixes clear errors found in a [third review](reviews/architecture-v0.10.1-third-review.md), mostly places that still assumed a quorum on a single server. v0.10.1 added a fairness rule for shared log flushes (Section 2.4), from the second outside review. v0.10 applied a product decision: single-box mode is the free, open-source edition, a standalone server with no quorum, no followers and no control plane, while disaggregated mode is the managed service and the only mode with a control plane, high availability and read replicas (Section 2.2, D11). v0.9.1 fixed clear errors found in a [second outside review](reviews/architecture-v0.9-second-review.md) and lists where this document changes the designs of record (Section 1.1). v0.9 applied the first six findings of the [outside principal review](reviews/architecture-v0.8.1-principal-review.md): the catalog lives in the main scope in version 1, scope density is a design requirement, a scope is a shared-memory engine within its node, read-write sessions are proxied whole to the leader, the failure model and design budgets are stated, and a new archive component supports restore and branching. v0.8 was a top-down review: it stated the document's purpose, gave every component a stated responsibility (Section 3.2), traced every product definition requirement to the components that deliver it (Section 3.3), and assigned the remaining requirements to their owners for detailed design (Section 4.12). Earlier versions, and what each one changed, are kept in [`history/`](history/), with reviews in [`reviews/`](reviews/).

**What this document is for.** It describes the **target architecture** of Cascade DB at the level needed to deliver the product definition in [`product-definition.md`](product-definition.md). Its main purpose is to show three things: which components are needed, what each one is responsible for, and how they connect to deliver each product definition requirement. It does not describe the order in which things get built; phasing, simplified first deployments and the migration from today's code are in [`architecture-implementation-phases.md`](architecture-implementation-phases.md).

**What this document is not.** The API sketches in Section 4 are a starting point that shows the shape of each contract and the promises it must keep. They are not the final API. The detailed design of each component, including its final API, data types, error cases and internal structure, belongs in that component's own implementation design document. A detailed design may change an API sketch freely, as long as it keeps the component's responsibility, its layer and its connections as described here. Changing any of those three requires changing this document first.

**The central goal.** Every component can be built and fully tested on its own, without starting the rest of the system. A component may talk to another component only through that component's published API and data types. Hidden, implicit or shared-state coupling between components is a design defect.

**Sources this document builds on.** The product definition is [`product-definition.md`](product-definition.md). The storage design of record is [`storage-subsystem-hld-v2.md`](storage-subsystem-hld-v2.md), and the table access API is the [access layer design](../crates/05-access/AI/access-layer-design.md), which replaced the retired WP5 document. The runtime design is [`runtime-architecture.md`](runtime-architecture.md).

---

## 1. Design Rules

These rules apply to every component. Section 9 describes how they are enforced.

**Rule 1. A component is a crate with a published contract.** The contract consists of the traits the component implements, the data types that cross its boundary, the errors it can return, and a written list of promises that callers may rely on. Anything not in the contract is private and may change without notice.

**Rule 2. Components depend on contracts, not on implementations.** When component A needs component B, A depends on B's contract and receives an implementation as a parameter. A never constructs B's implementation and never names B's concrete types. Where a contract has more than one implementation, or where its implementation pulls in heavy dependencies, the contract lives in its own small crate named `cascade-<name>-api`.

**Rule 3. Composition roots choose implementations and hold no logic.** The server binary and the test harnesses are the only crates that pick concrete implementations and connect them. They contain no lifecycle, recovery or routing logic. Anything that happens while the server runs, such as opening a scope or promoting a replica, belongs to a component with its own contract and tests (§4.8).

**Rule 4. Time, randomness, threads, network and disk are injected.** No component reads the system clock, spawns threads, opens files or sockets, or calls the async runtime directly. It receives a clock, an executor handle, a network transport and storage contracts as parameters. This is what makes deterministic simulation possible (§8).

**Rule 5. Every byte format has exactly one owner.** On-disk and on-wire formats are encoded and decoded in exactly one crate, which freezes them with golden-byte tests. Where several teams own parts of one format family, such as the redo records of different engines, each part is a separate module with a named owner inside the single owning crate.

**Rule 6. Every contract ships a conformance suite and a reference fake.** The contract crate provides a test suite, written once as generic functions over the trait, that checks every promise in the contract, plus a simple in-memory fake. The real implementation and the fake must both pass the suite. Consumers test against the fake.

**Rule 7. Layering is enforced by a machine.** A CI check fails the build when any crate depends on a crate it is not allowed to depend on.

**Rule 8. Shared context travels explicitly.** Tenant and database identity, tracing spans and deadlines travel in an explicit operation context, never in thread-local or global state. The only sanctioned globals are the runtime adapter's own per-thread reactor state and the replaceable telemetry log sink. Both are documented exceptions, and components reach them only through contracts.

**Rule 9. Contract types are designed to evolve.** Public contract enums and structs are `#[non_exhaustive]` unless they are meant to be matched exhaustively. A change to a contract is reviewed as an API change, and its conformance suite changes in the same commit.

**Rule 10. Thread affinity is part of a contract.** Whether a contract's implementation may be shared across cores (`Send + Sync`) or is owned by one core (`!Send`) is stated in the contract and checked by its conformance suite. It cannot be hidden behind a trait, because callers are written differently in each case.

**Rule 11. Shared structures on the commit path are model-checked.** Any `Send + Sync` structure that the commit path uses, such as the log writer's reservation, the transaction status table, the lock manager and the page cache's latches, is tested with a model checker that explores thread interleavings (for example `loom`), in addition to the simulator. The simulator runs all cores on one thread, so it cannot find races between atomic operations.

### 1.1. Changes to the Designs of Record

This document builds on HLD v2 and the runtime design, and in a few places it changes them. Where they disagree, this document wins, and the older design should be updated to match.

| Design of record | What it says | What this document decides | Where |
| :--- | :--- | :--- | :--- |
| HLD v2 §4 | Each database has a separate catalog scope, and DDL uses two-phase commit with it | In version 1 the catalog lives in the database's own scope; a catalog scope appears only when a database is split | §4.7, D10 |
| HLD v2 §8 | One page cache per core, owned by the scope's core, with cross-core access by message | A database or table runs on a set of cores, so a scope's data is a shared-memory engine and the page cache is shared per NUMA node, as the runtime design's shared state tier already allows | §4.4, §4.8, D1, Rule 11 |
| HLD v2 §9 | One combined PageStore service replicates the log and builds pages | Two separate components: the log store and the page store | §2.1, §4.3 |
| Runtime design D6 and §5.2 | Each connection goes to the least-loaded core at accept | Each connection goes to the least-loaded core in its database's core set, or is proxied whole to the leader node | §4.8, §5.1 |

---

## 2. Deployment Modes and Cluster Topology

Before looking inside one node, this section describes the deployment modes and how nodes work together, because storage, replication and failover shape several components.

### 2.1. Two Storage Roles, Always Separate

Storage has two jobs, and each is its own component in every deployment:

- **The log store** makes log records durable and keeps them in order. It decides the durable LSN, enforces epochs (fencing), and serves log ranges to anyone who needs to read the log. Every commit waits on it, so it is built for low, predictable latency.
- **The page store** reads the log from the log store, merges log records into data pages using `cascade-redo`, writes those pages to disk, and serves a page as of a given LSN. Commits never wait on it. If it falls behind, a page request applies the outstanding log for that page on demand.

Compute nodes never write data pages in either mode. They send log records to the log store and read pages from the page store.

### 2.2. Two Deployment Modes

The same components run in both modes. Only where they run, and how they are connected, differs.

| | Single-box mode: the open-source edition | Disaggregated mode: the managed service |
| :--- | :--- | :--- |
| Shape | One server runs compute, a log store and a page store on its own local disks, like a conventional database server | Compute nodes, log store nodes and page store nodes run on separate machines, like Aurora or SQL Server Hyperscale |
| How compute reaches storage | In-process calls through the same contracts | Network calls through the same contracts |
| Log durability | The local log store alone (N = 1). A commit is durable once its log record is flushed to the server's own disk, with no quorum involved | A group of log store nodes chosen per scope from a shared pool, so that many scopes share each log store node (default N = 3, W = 2) |
| Page storage | The local page store materializes pages from the local log | Page store nodes materialize pages from the log store nodes. Their replication factor is chosen separately from the log's |
| Metadata and control | A local metadata store on the server's own disk, and a local controller that creates and drops databases on this server. There is no control plane | The control plane: a Raft metadata group, the cluster controller and the failover controller |
| High availability | None built in. If the server fails, its databases are unavailable until it restarts. It then recovers from its local log and pages without losing any acknowledged commit, provided its disk survives. Backups through the archive protect against losing the disk | Warm followers and failover in seconds, plus any number of read replicas |
| Typical use | The free, downloadable edition, run by the customer: development, small production and on-premises deployments | The managed cloud service |

**The product boundary is the control plane.** Single-box mode is the free and open-source edition, and it runs without a control plane. Disaggregated mode is the managed service, and everything that coordinates more than one machine belongs to its control plane: the Raft metadata group (`cascade-meta`), the cluster controller (`cascade-cluster`), the failover controller (`cascade-failover`), the network storage clients and servers (`cascade-storage-remote`), and the log store and page store server binaries. Followers, read replicas and log shipping (`cascade-logship`) exist only in the managed service. A single server instead wires local implementations of the same contracts: a metadata store kept on its own disk, a local controller for creating and dropping databases, and the log store and page store called in process. Everything else, including SQL, both storage engines, transactions, core sets and the governor, the archive and the temporary store, is the same code in both editions. Because the boundary falls on contracts that already exist, the open-source edition is a different wiring of the same components, not a separate product to maintain. This architecture does not include a customer-run cluster of single servers; customers who need high availability use the managed service.

### 2.3. Active-Active Compute with Warm Followers (Managed Service)

This section applies to disaggregated mode only. A single server has one compute node, leads every scope it holds, and has no followers or read replicas.

**Kinds of nodes.** *Compute nodes* run SQL and transactions and hold page caches. *Log store nodes* and *page store nodes* hold the two storage roles on their own machines. A small *metadata group* (a Raft group) records which compute node owns each writing scope, under which epoch, and which log store and page store instances hold each scope's data.

**Every compute node is active.** There is no idle standby. Each writing scope (a database, a table or a table shard, per HLD v2 §4) has exactly one **leader**, which is the only node that writes it. It also has one or more **followers**, which are warm replicas. A compute node is typically the leader for some scopes and a follower for others, and the cluster controller (`cascade-cluster`) spreads leaders so that all nodes carry write load. Followers serve reads for their scopes. "Active-active" in this document means all nodes are active at once. It does not mean two nodes write the same scope at the same time, which the single-writer rule of HLD v2 forbids.

**Read replicas.** Beyond its followers, a scope may have any number of **read replicas**. Followers are few (default one, D3) because the leader pushes the log to each of them, and they must be ready to take over. Read replicas exist only to add read capacity, so they do not take the log from the leader. They subscribe to it from the log store through `LogSource::subscribe` and fetch pages from the page store, which means adding read replicas puts no load on the leader. They run the same follower code as followers, apply only durable log for the same reason, and serve reads under the same closed-timestamp rule, using the closed-timestamp heartbeat records in the log. A read replica may lag slightly more than a follower, because the log reaches it through the log store. A read replica is not a failover candidate unless the controller first turns it into a follower.

**Two layers of replication.** They have different jobs and must not be confused:

| | Layer 1: durability replication | Layer 2: log shipping to followers |
| :--- | :--- | :--- |
| From and to | The leader's log writer to the scope's log store quorum | The leader's log writer to the scope's followers |
| Purpose | No acknowledged commit is ever lost | Followers keep a warm cache and current transaction state, so they can serve reads and take over in seconds |
| Synchrony | Synchronous. A commit is acknowledged only after a write quorum of log stores has the commit record | Asynchronous. A commit never waits for a follower's cache to apply it |
| Quorum | Configurable as copies (N) and write quorum (W). The default is N = 3 and W = 2. Aurora's N = 6 and W = 4 is also a valid setting | Not a quorum. Every follower receives the stream. Read replicas do not; they read the log from the log store |
| Catch-up | A log store that falls behind repairs itself from its peers | A follower that falls behind or reconnects reads the missing log from the log store, then rejoins the stream |

**The durable LSN is defined by the log store, not by any compute node.** For each scope, the durable LSN is the end of the contiguous log prefix that a write quorum of log stores has acknowledged. The leader learns it from those acknowledgements, and the leader includes it in the stream it pushes to followers.

**Followers apply only durable log.** A follower receives log records as soon as the leader writes them, but it applies a record only once the durable LSN has passed it. A follower that applied non-durable records could, after a failover, hold changes that never committed. Aurora applies the same rule.

**Followers serve consistent reads.** A follower serves a read at timestamp `read_ts` only after it has applied everything up to the leader's closed timestamp at or beyond `read_ts`, which is HLD v2's read rule. It applies each group of records that forms one atomic page change (a mini-transaction, such as a B-tree split) all at once, so readers never see half of a structure change. A follower applies records only to pages that are already in its cache. It drops records for pages it does not hold, because it can fetch those pages from the page store later at the right LSN.

![Replication and failover: active-active compute nodes, a log store quorum feeding separate page stores, and the failover sequence](assets/replication.svg)

*The diagram* shows the managed service in disaggregated mode, without read replicas: three compute nodes, each leading one scope and following another, the two replication layers (① synchronous durability replication to a log store quorum, ② asynchronous log shipping to followers), page stores fed from the log store, and the failover sequence when node A fails. A PNG copy is at [`assets/replication.png`](assets/replication.png).

### 2.4. Scope Density

The primary customer runs thousands of tenant databases, and each database is at least one writing scope. The architecture therefore treats the cost of an idle scope as a design requirement, not an optimization. The target is 10,000 open scopes per node, most of them idle, and failover of a node that leads thousands of them within the failover budget in Section 6.

The following rules apply to every component that keeps per-scope state:

- **Shared log segments.** The log store writes many scopes into shared segment files and makes them durable with one shared flush, while each scope keeps its own LSN space, epoch and durable LSN.
- **Fair sharing of the log.** Shared flushes are what make density affordable, but they also let one tenant's burst of large writes delay the commits of every tenant that shares the flush, which would break product definition §4.4's promise that a write spike in one database does not slow another. The log store therefore bounds each scope's share of every flush and of its log bandwidth, so that a scope writing more than its share has the excess carried into later flushes instead of making the whole flush larger and slower. Quiet scopes keep their commit latency, and the heavy scope is slowed, not the others. The log store reports each scope's log volume as usage, and a scope that stays heavy is isolated further: the log store can give it its own segment stream, and in the managed service `cascade-cluster` can move its log group to other log store nodes. The exact sharing policy, such as weighted fair queuing, and its thresholds are left to the log store's detailed design.
- **Batched streams.** Log shipping, closed-timestamp heartbeats and watermark reports are batched per pair of nodes, not sent per scope. An idle scope sends nothing.
- **Batched ownership changes.** When a node fails, the failover controller acquires the new epochs for all of its scopes in batched metadata writes, and the log stores fence them in batches, so that failover time grows with the number of nodes involved, not with the number of scopes.
- **No task per scope.** Per-scope background work, such as purge, heartbeats and page store catch-up, runs from shared schedulers. An idle scope holds memory for its metadata only, and no tasks, timers or connections.

**Failover in outline.** Section 6 gives the full sequence. When a leader fails, the failure detector in the metadata group notices that its lease has expired. The failover controller picks a follower for each affected scope. That follower acquires the scope with a higher epoch, which fences the old leader at the log store. It confirms the durable LSN with the log store and rolls back unfinished transactions. Then it starts accepting writes with a cache that is already warm. Scopes fail over independently and in parallel, so the load of a failed node spreads across the survivors.

---

## 3. The Layer Map

Layers are numbered from the bottom. A crate may depend on contracts in lower layers and in its own layer, never in a higher layer. The composition roots are the only crates that depend on implementations from many layers. The map shows every component and its main connections; Section 3.2 lists each component's responsibility, and Section 3.3 shows which components deliver each product definition requirement.

![Cascade DB layer map v0.10](assets/layer-map.svg)

*How to read the map.* Thin black arrows show call direction: an arrow from A to B means A calls B's contract. Dashed arrows with open heads show which implementation fulfills which contract. The thick orange arrows are log records: into the log store (durability replication), from the log store to the page store, and across to followers (log shipping). The thick blue arrow is page images returning from the page store into the page cache. The arrow from the session to the data plane is connection placement, which happens once when a connection is established, and the arrow from the governor to the data plane is the feedback that changes core assignments (Section 5.1). The purple arrow shows that only composition roots wire implementations. The map shows the full set of components used by the managed service; the components that a single server does not run are drawn with grey dashed outlines and are also marked in Section 3.2. A PNG copy is at [`assets/layer-map.png`](assets/layer-map.png).

### 3.1. Layers

| Layer | Name | What the layer is for |
| :--- | :--- | :--- |
| L9 | Front end | Talking to clients and to source databases |
| L8 | SQL | Turning SQL into plans and running them |
| L7 | Scope host and data plane | Deciding where scopes and work run, and changing that at run time |
| L6 | Catalog | Schema, statistics and permissions |
| L5 | Access | Tables, indexes and sequences |
| L4 | Transactions | Ordering, visibility, rollback and waiting |
| L3 | Log, cache and replication | Writing the log, caching pages, and shipping the log to replicas |
| L2 | Durability and cluster | Storing log and pages, archiving them, cluster metadata, and local temporary space |
| L1 | Formats | Byte formats and the redo functions, with no I/O |
| L0 | Foundation | Identifiers, context, errors, telemetry, and the runtime and network contracts |

### 3.2. Component Responsibilities

Every component in the architecture is listed here with its single responsibility. Section 4 describes each in more detail.

| Layer | Component | Responsibility |
| :--- | :--- | :--- |
| L9 | `cascade-pgwire` | The PostgreSQL wire protocol: framing, startup, authentication, TLS, query protocols, `COPY` and notifications. |
| L9 | `cascade-session` | Per-connection state, and the acceptor that runs the startup handshake and hands each connection to its core. |
| L9 | `cascade-notify` | `LISTEN` and `NOTIFY` across sessions, cores and nodes, in commit order. |
| L9 | `cascade-subscriber` | Inbound logical replication from PostgreSQL, which is the migration path. |
| L8 | `cascade-query-api` | The `QueryService` contract used by sessions, PL/pgSQL and the subscriber, including nested execution. |
| L8 | `cascade-query` | Running each statement through the SQL pipeline, managing prepared statements, and starting and ending transactions. |
| L8 | `cascade-pg-parser(-sys)` | Parsing SQL exactly as PostgreSQL does, through `libpg_query`. |
| L8 | `cascade-sql-analyzer` | Resolving names and types, and checking privileges, to produce a bound query. |
| L8 | `cascade-sql-planner` | Choosing a physical plan without I/O, splitting it into per-scope fragments, and applying hints and pinned plans. |
| L8 | `cascade-sql-exec` | Running a plan, sending fragments through the router, and running the operators above them. |
| L8 | `cascade-plan-store` | Caching plans, and capturing, pinning and comparing plans for plan stability. |
| L8 | `cascade-plpgsql` | Running PL/pgSQL functions, procedures and triggers. |
| L8 | `cascade-pgtypes` | SQL-level type behavior: casts, operators, functions and wire formats. |
| L8 | `cascade-sysview` | The registry of system views, and scanning them for the executor. |
| L7 | `cascade-scope` | Opening, promoting, demoting and closing scopes on a node, in leader, follower or read-replica role. |
| L7 | `cascade-dataplane` | Placement strategies, core sets, placing connections, and routing plan fragments to the right core or node. |
| L7 | `cascade-governor` | Measuring resource use per database and table, driving placement, and later enforcing limits. |
| L7 | `cascade-failover` | Detecting a failed leader and promoting a follower within seconds. Managed service only. |
| L7 | `cascade-cluster` | Creating, dropping, moving and balancing databases and scopes across nodes, including planned handoffs. A local controller handles creating and dropping databases on a single server. |
| L6 | `cascade-catalog-api` | Contracts for reading and writing the catalog and for credentials. |
| L6 | `cascade-catalog` | Storing the catalog as tables, caching schema per core, and schema leases. |
| L6 | `cascade-stats` | `ANALYZE` and automatic statistics collection. |
| L6 | `cascade-authz` | Privilege checks and row-level security policies. |
| L5 | `cascade-access-api` | The table access contract that every engine implements, and table descriptors. |
| L5 | `cascade-btree-core` | A B-tree over the page cache that knows nothing about transactions. |
| L5 | `cascade-iot` | The default index-organized engine, with in-place updates and undo. |
| L5 | `cascade-heap` | The PostgreSQL-compatible heap engine, with incremental pruning and vacuum. |
| L5 | `cascade-index` | Secondary indexes of every kind, each carrying its own visibility, for both engines. |
| L5 | `cascade-sequence` | Sequences, `serial` and identity values that are never reused after a failover. |
| L5 | `cascade-maintenance` | Scheduling undo purge, heap pruning and vacuum within resource budgets, and enforcing undo limits. |
| L5 | `cascade-temptable` | Session-private temporary tables. |
| L4 | `cascade-txn-api` | Transaction contracts at both levels, snapshots, and the visibility rule shared by engines and indexes. |
| L4 | `cascade-scope-txn` | Per-scope transactions: IDs, commit and abort, status, isolation levels, the closed timestamp, and leader and follower roles. |
| L4 | `cascade-txn-coord` | Session-level transactions: one read timestamp across scopes, the single-scope write rule, and two-phase commit for DDL once a database has several scopes. |
| L4 | `cascade-undo` | Undo records and chains for old versions, rollback and savepoints. |
| L4 | `cascade-lock` | Waiting: table and advisory locks, waits on row writers, `NOWAIT`, `SKIP LOCKED` and deadlock detection. |
| L4 | `cascade-watermark` | The oldest read timestamp still in use anywhere, which bounds cleanup. |
| L3 | `cascade-log-api`, `cascade-log` | The per-scope log writer: LSN reservation, group commit, waiting for durability, and the tap for log shipping. |
| L3 | `cascade-pagecache` | The compute page cache, which is never written back, and mini-transactions as the only way to change a page. |
| L3 | `cascade-logship` | Pushing a leader's log to its followers, and the feeds through which followers and read replicas apply it. Managed service only. |
| L2 | `cascade-storage-api` | The `LogSink`, `LogSource` and `PageSource` contracts, identical in both deployment modes. |
| L2 | `cascade-logstore` | Durable, ordered and fenced storage of each scope's log, and serving it to readers. |
| L2 | `cascade-pagestore` | Merging the log into pages in the background, and serving a page as of an LSN. |
| L2 | `cascade-storage-remote` | Network clients and servers for the storage contracts, including the log quorum client. Managed service only. |
| L2 | `cascade-meta-api`, `cascade-meta` | Cluster metadata: scope ownership and epochs, replica locations, leases and membership. A Raft group in the managed service, and a local store on a single server. |
| L2 | `cascade-archive` | Copying log and page images to object storage, and serving them for restore, branching and rebuilds. |
| L2 | `cascade-tempstore` | Compute-local, non-durable disk space for temporary tables and spill files. |
| L1 | `cascade-datum` | In-memory values, row encoding and order-preserving key encoding. |
| L1 | `cascade-page` | Every page layout and its typed views. |
| L1 | `cascade-wal-format` | Encoding and decoding of every log record. |
| L1 | `cascade-redo` | Deterministic application of a log record to a page, shared by compute and storage. |
| L0 | `cascade-types` | Shared identifiers and value types, and the operation context. |
| L0 | `cascade-error` | Mapping every contract's errors to PostgreSQL SQLSTATE codes. |
| L0 | `cascade-telemetry` | Spans, metrics, structured logs, and the usage reports that the governor reads. |
| L0 | `cascade-sysview-api` | The contract through which any component publishes live state as a system view. |
| L0 | `cascade-rt-api` | The executor handle: per-core tasks, cross-core work, connection hand-off, clocks and cooperative yielding. |
| L0 | `cascade-net-api` | Messaging between nodes, and outbound client connections. |
| Adapter | `cascade-runtime` | The thread-per-core runtime on `io_uring`, which implements `cascade-rt-api` and `cascade-net-api`. |
| Root | `bin/cascade-server` | Wiring a compute node. In single-box mode it also wires the local log store, page store, metadata store and controller into the same process. |
| Root | `bin/cascade-logstore-server`, `bin/cascade-pagestore-server` | Wiring log store nodes and page store nodes in disaggregated mode. |
| Root | `cascade-testkit`, `cascade-sim` | Reference fakes, conformance suites, and the deterministic simulator. |

### 3.3. How the Components Deliver the Product Definition

Each product definition requirement is listed with the components that deliver it, in the order a request or event passes through them. Where an owner's contract does not yet cover a requirement, the last column points to Section 4.12, which leaves the API to that component's detailed design.

| Product definition requirement | Delivered by | Detailed design pending |
| :--- | :--- | :--- |
| No vacuum on the default engine (§4.1) | `cascade-iot` updates in place and writes old values through `cascade-undo`; `cascade-maintenance` purges undo below the `cascade-watermark` | |
| Continuous heap cleanup (§4.1) | `cascade-heap` prunes on access; `cascade-maintenance` vacuums page by page | |
| Fewer index writes on update (§4.1) | `cascade-iot` with `cascade-index` entries keyed by primary key | |
| No transaction ID wraparound (§4.1) | 63-bit `TxnId` and 64-bit `Hlc` in `cascade-types`, assigned by `cascade-scope-txn` | |
| Bounded, visible undo (§4.1) | `cascade-watermark` finds the holder; `cascade-maintenance` enforces limits; `cascade-sysview` reports | §4.12 |
| Self-sizing page cache, no tuning (§4.1) | `cascade-pagecache` | §4.12 |
| Compute never writes pages, no checkpoints (§4.2) | Mini-transactions in `cascade-pagecache` → `cascade-log` → `LogSink`; `cascade-pagestore` builds pages with `cascade-redo` | |
| Quorum durability, N = 3 and W = 2 by default (§4.2), in the managed service | `cascade-log` → log quorum client in `cascade-storage-remote` → `cascade-logstore` replicas | |
| Local durability on a single server (§3, §4.2) | `cascade-log` → the local `cascade-logstore` (N = 1); `cascade-archive` for backups | |
| Encryption at rest, one key per scope (§4.2, §4.4) | `cascade-logstore`, `cascade-pagestore`, `cascade-storage-api` | §4.12 |
| No acknowledged commit lost (§4.3) | `cascade-scope-txn` commit waits on `LogSink::wait_durable` | |
| Failover in seconds with warm followers (§4.3), in the managed service | `cascade-failover` watches `cascade-meta`; `cascade-scope` promotes; `cascade-logship` keeps the follower's `cascade-pagecache` and `cascade-scope-txn` warm; `LogSink::open_writer` fences | |
| Read replicas without load on the leader (§4.3), in the managed service | Read-replica feed in `cascade-logship` over `LogSource`; read-only sessions placed on replicas by `cascade-dataplane` | |
| Isolated databases and writing scopes (§4.4) | Every component above L2 works per scope; `cascade-meta` records ownership; `cascade-cluster` places scopes | |
| Version 1 databases behave exactly like PostgreSQL, including DDL and DML in one transaction (§4.4) | One scope per database that also holds its catalog, so every transaction is single-scope; `cascade-txn-coord` | |
| Single-scope writes and DDL by two-phase commit once a database is split (§4.4) | `cascade-txn-coord` | |
| Point-in-time restore and branches per database (§4.5) | `cascade-archive` holds the window in object storage, or in a directory on a single server; `cascade-storage-api`, `cascade-pagestore` and `cascade-meta` provide branch reads and lineage; `cascade-cluster` or the local controller orchestrates | §4.12 |
| Database creation in seconds (§4.5) | `cascade-cluster` → `cascade-meta` → `cascade-scope` on the chosen nodes; on a single server, the local controller → the local metadata store → `cascade-scope` | |
| Thousands of connections without a pooler (§4.6) | `cascade-runtime` accepts; `cascade-session` and `cascade-pgwire` hold session state on one core; `cascade-dataplane` places the connection | |
| One database or hot table using many cores (§4.6) | `cascade-governor` measures; `cascade-dataplane` strategies assign core sets; `cascade-log`, `cascade-scope-txn` and `cascade-lock` are shared by a scope's write cores | |
| SMT and NUMA settings (§4.6) | `cascade-runtime` topology; `cascade-dataplane` strategies respect NUMA affinity | |
| Fair sharing between short and long queries (§4.6) | `Rt::should_yield` in `cascade-sql-exec`; the stall detector in `cascade-runtime` | |
| Plan capture, pinning, regression detection and hints (§4.7) | `cascade-sql-planner`, `cascade-plan-store`, views through `cascade-sysview` | |
| Rolling upgrades without downtime (§4.8), in the managed service | Planned handoff in `cascade-cluster`; old record versions in `cascade-redo`; mixed-version messaging in `cascade-net-api` | §4.12 |
| PostgreSQL behavior chosen per database (§4.8) | Behavior version in `cascade-catalog`, read by `cascade-sql-analyzer` and `cascade-pgtypes` | §4.12 |
| Memory safety and one audited C dependency (§4.9) | Every component is Rust; `cascade-pg-parser-sys` is the only C code, behind `cascade-pg-parser` | |
| Wire protocol, TLS and authentication (§5.1) | `cascade-pgwire`, `cascade-session`, `cascade-notify`; credentials from `cascade-catalog-api` | §4.12 (TLS) |
| PostgreSQL data types (§5.2) | `cascade-datum` for values and keys; `cascade-pgtypes` for behavior and wire formats | |
| SQL dialect, PL/pgSQL and triggers (§5.3) | `cascade-pg-parser`, `cascade-sql-analyzer`, `cascade-sql-planner`, `cascade-sql-exec`, `cascade-plpgsql` | |
| All index kinds (§5.3) | `cascade-index`, with formats in `cascade-page` and redo in `cascade-redo` | §4.12 |
| Isolation levels, savepoints, advisory locks (§5.3) | `cascade-scope-txn`, `cascade-undo`, `cascade-lock` | §4.12 (Serializable, savepoints) |
| Temporary tables (§4.6, §5.3) | `cascade-temptable` over `cascade-tempstore` | |
| Real system catalogs and statistics views (§5.3) | `cascade-catalog` for catalog tables; `cascade-sysview` for live views | |
| Roles, privileges and row-level security (§4.4, §5.3) | `cascade-authz`, called by `cascade-sql-analyzer` and `cascade-sql-planner` | |
| Native `pgvector`, `pg_trgm`, `pg_stat_statements`, `pgcrypto` (§5.4) | `cascade-index` for the index types; `cascade-sysview` with `cascade-plan-store` for statements; `cascade-pgtypes` for functions | §4.12 (index types) |
| Migration by logical replication (§6) | `cascade-subscriber` → `QueryService` | |
| Resource governance reservations (§8.1) | `OpCtx` in `cascade-types`; `UsageSink` in `cascade-telemetry`; `cascade-governor`; per-database accounting in `cascade-pagecache`; tenant tags on I/O | §4.12 |
| Observability reservations (§8.2) | `cascade-telemetry` spans and metrics; `cascade-sysview` | |
| Change data capture reservations (§8.3) | Full row images in `cascade-wal-format`; `LogSource` as the only log reader | |
| Analytics later (§8.4) | Reserved: cooperative yielding now, and Appendix A.3 | |

---

## 4. Components and Their Contracts

Each component lists its responsibility and promises, what it depends on, and how it is tested in isolation. Where an API is shown, it is a **starting sketch** for the component's implementation design document, not the final API. All async traits use `async fn` in traits. Each contract states its thread affinity (Rule 10).

### 4.1. L0 Foundation

**`cascade-types`.** Plain value types, including `DatabaseId`, `TenantId`, `ScopeId`, `CoreId`, `RoleId`, `Epoch`, `NodeId`, `PageId`, `Lsn`, `TxnId` (63-bit), `Hlc` (48-bit physical milliseconds plus a 16-bit logical counter), `VersionWord`, `ObjectUuid`, `AlignedBuf`, and the operation context:

```rust
pub struct OpCtx<'a> {
    pub tenant: TenantContext,      // DatabaseId plus optional TenantId
    pub span: &'a Span,
    pub deadline: Option<Instant>,
}
```

**`cascade-error`.** Each contract defines its own error type. This crate provides the shared mapping from errors to PostgreSQL SQLSTATE codes.

**`cascade-telemetry`.** Spans, metrics, the structured log sink, and the `UsageSink` contract through which components report per-database and per-table resource use to the governor (Section 4.8).

**`cascade-rt-api`: the executor contract.** It is a small concrete handle type rather than a generic trait, so that components do not become generic over the runtime. Work is passed as boxed closures and futures. The cost of boxing on hot paths is to be measured.

```rust
pub struct Rt { /* handle to the current core's executor; !Send */ }
impl Rt {
    pub fn core(&self) -> CoreId;
    pub fn now(&self) -> Instant;                    // monotonic
    pub fn wall_ms(&self) -> u64;                    // for the HLC
    pub fn spawn_local(&self, f: LocalBoxFuture<'static, ()>) -> TaskHandle;
    pub async fn submit_to<T: Send + 'static>(&self, core: CoreId,
        f: Box<dyn FnOnce() -> LocalBoxFuture<'static, T> + Send>) -> Result<T, SubmitError>;
    pub fn hand_off(&self, conn: AcceptedConn, core: CoreId) -> Result<(), SubmitError>; // move a connection to its home core
    pub async fn sleep(&self, d: Duration);
    pub async fn yield_now(&self);
    pub fn should_yield(&self) -> bool;
}
```

**`cascade-net-api`: the network contract (new).** Message streams between nodes, addressed by `NodeId`, and outbound client connections for the migration subscriber. Log shipping, storage replication, remote data-plane calls and metadata traffic all use it. In tests, the simulator implements it with seeded delays, reordering, drops and partitions.

**`cascade-sysview-api`: the system view contract (new).** Many PostgreSQL views show live state that lives inside one component: `pg_stat_activity` in the sessions, `pg_locks` in the lock manager, `pg_stat_statements` and captured plans in the plan store, undo retention in maintenance, resource usage in the governor. This contract lets each component publish its own views without anyone reaching into its internals. It sits in L0 so that a component in any layer can implement it.

```rust
pub trait SystemViewProvider: Send + Sync {
    fn views(&self) -> &[ViewDef];                                   // names and column types
    fn scan(&self, ctx: &OpCtx<'_>, view: ViewId) -> RowBatchStream;  // a point-in-time snapshot of live state
}
```

- *Promises.* A scan never blocks the provider's hot path, returns rows for the caller's database only unless the caller has a monitoring role, and may be slightly stale. Views over live state are not transactional, as in PostgreSQL.
- *Tests.* A conformance suite checks that each provider's rows match its declared columns and respect the database filter.

### 4.2. L1 Formats (pure, no I/O)

**`cascade-datum`: values, rows and keys (new).** It owns the in-memory value representation, the row encoding and an order-preserving key encoding that includes collation-aware string keys. Engines handle only encoded rows and keys from this crate, so they never depend on the SQL type system. Tested with golden bytes and with property tests that key order matches value order.

**`cascade-page`: the page format authority.** It owns page layouts: the common header and slot directory (implemented today), and typed views for index-organized leaves, heap tuples, index entries, undo pages, the free space map and transaction status pages. Cells carry the HLD v2 version word. Tested with golden bytes and property tests.

**`cascade-wal-format`: the log record encoding.** Every record carries its LSN, length, type, version, scope epoch, optional transaction ID and checksum. Records are grouped by family: page redo, transaction (commit, abort, HLC high-water, closed-timestamp heartbeat), catalog and DDL, two-phase commit for DDL, and compensation records. Records carry full row images for change data capture.

**`cascade-redo`: the redo apply functions.** `apply(page, record, lsn)` is deterministic, depends only on the page and the record, and does nothing if the record's LSN is not above the page's LSN. Leaders, followers and page stores all use this one crate, so all three produce byte-identical pages.

- *Structure.* Redo is organized as one module per record family, each with a named owner, behind a dispatch table keyed by record type and version. This crate necessarily knows every page format, and it must keep applying every record version it ever accepted. That is the deliberate price of the redo contract.
- *Tests.* Property tests for determinism and idempotence, golden tests per record version, and a check that every historical version still applies.

### 4.3. L2 Durability and Cluster

The two storage roles are two components, `cascade-logstore` and `cascade-pagestore`, reached through three contracts. The contracts are the same in single-box and disaggregated mode, so every component above L2 is tested once and runs unchanged in both modes. The components that exist only in the managed service are named as such below.

**`cascade-storage-api`: the storage contracts.**

```rust
/// Compute → log store. Makes log bytes durable for a scope.            Send + Sync
pub trait LogSink {
    async fn open_writer(&self, scope: ScopeId, epoch: Epoch) -> Result<Lsn, StoreError>;  // fences older epochs; returns durable LSN
    async fn append(&self, scope: ScopeId, epoch: Epoch, start: Lsn, bytes: &[u8]) -> Result<(), StoreError>;
    fn durable_lsn(&self, scope: ScopeId) -> Lsn;
    async fn wait_durable(&self, scope: ScopeId, lsn: Lsn) -> Result<(), StoreError>;
}

/// Log store → its readers (page store, follower catch-up, change data capture).   Send + Sync
pub trait LogSource {
    async fn read_log(&self, scope: ScopeId, from: Lsn, max_bytes: usize) -> Result<LogChunk, StoreError>;  // durable records only
    fn subscribe(&self, scope: ScopeId, from: Lsn) -> LogSubscription;   // ordered stream of durable records
    async fn set_retained_from(&self, scope: ScopeId, holder: RetentionHolder, lsn: Lsn) -> Result<(), StoreError>;
}

/// Compute → page store. Serves pages as of an LSN.                     Send + Sync
pub trait PageSource {
    async fn get_page(&self, scope: ScopeId, page: PageId, at_lsn: Lsn) -> Result<PageImage, StoreError>;
    fn materialized_lsn(&self, scope: ScopeId) -> Lsn;    // every page reflects the log up to here on disk
}
```

- *Retention.* The log store may discard log only below the lowest LSN still needed by any retention holder. Holders include each page store (up to what it has written to disk), followers that are catching up, change data capture, and the archive, which holds log until it has copied it to object storage. This is how the page store and the log store stay independent without losing log the page store still needs.
- *Promises.* The durable LSN never moves backwards. Appends from an epoch older than the scope's current epoch are rejected. `LogSource` returns only durable records, in LSN order, with no gaps. A page returned at `at_lsn` reflects exactly the scope's records for that page below `at_lsn`. No record above the durable LSN is ever applied to a page.
- *Tests.* One conformance suite per contract, run against the in-memory fakes, the local implementations and the remote implementations. It covers fencing, durable-LSN monotonicity, ordering and gaps, retention, page-at-LSN correctness (using `cascade-redo` as the reference), and crash-restart.
- *Left to detailed design (encryption, product definition §4.2 and §4.4; HLD v2 §15).* The storage contracts and both storage components own encryption at rest, with one key per scope covering its log and its pages. The page store must decrypt and re-encrypt when it applies redo. The detailed design adds a key provider contract, key rotation, and where encryption sits in the log and page formats. The key provider needs an implementation that works without a control plane, such as a local key file or an external key management service, for the open-source edition.
- *Left to detailed design (branching and point-in-time restore, product definition §4.5).* The storage contracts own reading a database as of an earlier point and creating a branch that shares unchanged pages with its parent. The detailed design adds branch semantics to `PageSource` (a parent scope and fork LSN), a mapping from restore timestamp to LSN in the log store, and branch lineage in the metadata store, with `cascade-cluster` orchestrating restore and branch creation.

**`cascade-logstore`: the log store.**

- *What it does.* It persists log segments for its scopes, enforces epochs, tracks its own persisted LSN per scope, serves log ranges and subscriptions, honors retention holders, and repairs gaps from peer log stores. Many scopes share segment files and flushes, and each scope's share of a flush and of log bandwidth is bounded (Section 2.4).
- *Implements.* `LogSink` for one replica, and `LogSource`. In single-box mode the compute node uses it directly, as its only log store (N = 1). In disaggregated mode the compute node uses the quorum client in `cascade-storage-remote`, which presents the quorum as a single `LogSink`.
- *Disk access.* Through an injected block-device contract, so that crash consistency can be tested by cutting writes at random points. Today's `O_DIRECT` and `io_uring` code in `cascade-storage`, and the segment and recycle logic in `cascade-wal`, become its disk layer.

**`cascade-pagestore`: the page store (the component that merges log into pages).**

- *What it does.* It subscribes to the log through `LogSource`, applies each record to its page with `cascade-redo`, writes pages to disk in the background, and serves `get_page` at an LSN. If a requested page is behind the requested LSN, it applies the outstanding records for that page before returning it. It reports how far it has written pages to disk as its retention holder, so the log store keeps everything it still needs.
- *Implements.* `PageSource`.
- *Depends on.* `LogSource`, `cascade-redo`, `cascade-page` and an injected block-device contract. It never depends on the log store's implementation, so it can run next to the log store, on another machine, or in the same process.
- *Tests.* Against a fake `LogSource`: materialization order, on-demand catch-up, retention reporting, and crash-restart from its last written point, plus differential tests that its pages equal pages built by replaying the log through `cascade-redo`.

**`cascade-storage-remote`: network clients and servers (managed service only).** Disaggregated mode uses all of them. Single-box mode uses none of them.

- The **log quorum client** implements `LogSink` and `LogSource` over N log store nodes. It sends each append to all N, advances the durable LSN once W have acknowledged a contiguous prefix, and performs a quorum read when a new leader opens a scope.
- The **page client** implements `PageSource` over the page store nodes for a scope, routing each read to a node that has materialized far enough, or that can catch up on demand.
- The **servers** expose a local `cascade-logstore` or `cascade-pagestore` over `cascade-net-api`. They add transport only, with no storage logic.
- *Tests.* Against simulated nodes over the simulated network: lost and reordered acknowledgements, slow and failed nodes, quorum loss, and epoch fencing during a leader change. The same conformance suites from `cascade-storage-api` run through the clients.

**`cascade-meta-api` and `cascade-meta`: cluster metadata.** The metadata group records, for every scope, its leader, epoch, followers, read replicas, log store replicas and page stores. It also provides leases and membership for failure detection. There are two production implementations of `MetaStore`. The managed service uses a Raft group, which is part of its control plane. A single server uses a local store kept on its own disk; with only one node there is nothing to agree on and no lease can be lost, so it needs no consensus.

```rust
pub trait MetaStore: Send + Sync {
    async fn acquire(&self, scope: ScopeId, expected: Epoch, node: NodeId) -> Result<Epoch, MetaError>;
    async fn acquire_many(&self, claims: &[(ScopeId, Epoch)], node: NodeId) -> Result<Vec<Epoch>, MetaError>;  // batched, for failover
    async fn lookup(&self, scope: ScopeId) -> Result<ScopePlacement, MetaError>;
    async fn renew_lease(&self, node: NodeId) -> Result<LeaseExpiry, MetaError>;
    fn watch(&self, filter: WatchFilter) -> WatchStream;              // placement, membership and lease events, in order
}
```

- *Promises.* At most one node holds a scope at any epoch, epochs only increase, and watch events arrive in commit order.
- *Tests.* Conformance on the in-memory, local and Raft implementations, plus simulated partitions for the Raft implementation.

**`cascade-archive`: the archive tier (new).** It copies each scope's durable log, and periodic page images from the page store, to object storage, and serves them back for point-in-time restore, branching and rebuilding page stores. Keeping a restore window of days or weeks on the log store's low-latency disks would be too expensive, so the archive is where that window lives.

- *What it does.* It subscribes to the log through `LogSource` and uploads it in segments, and it is a retention holder, so the log store discards log only after the archive has it. The managed service always runs it. On a single server it is optional: when no archive target is configured, it does not run and holds no log back, and the server then has no backups or point-in-time restore beyond the log it still keeps. It asks the page store for page images at chosen LSNs, so that a restore starts from a recent image instead of replaying the whole log. It serves both back to the page store, to `cascade-cluster` for restore and branch creation, and to a page store being rebuilt.
- *Depends on.* `LogSource`, `PageSource`, and an injected `ObjectStore` contract, so that tests use an in-memory object store and single-box deployments can use a local or network directory instead of a cloud object store.
- *Promises.* Archived log is complete and gap-free from the start of the retention window to the archive's high-water LSN. Every archived object is checksummed and encrypted with the scope's key.
- *Tests.* Against fakes of `LogSource`, `PageSource` and `ObjectStore`: upload ordering, retention handoff to the log store, restore to an exact LSN, and recovery after an interrupted upload.

**`cascade-tempstore`: compute-local temporary storage (new).** Compute never writes data pages, but some work still needs local disk: temporary tables that outgrow memory, and sorts, hash joins and aggregations that spill. This component provides that space and nothing else.

```rust
pub trait TempStore: Send + Sync {
    fn create(&self, ctx: &OpCtx<'_>, owner: TempOwner) -> Result<TempFile, TempError>;  // owner: a session or a query
}
pub trait TempFile {                                                                    // !Send: used by one core
    async fn write_at(&self, off: u64, buf: &AlignedBuf) -> Result<(), TempError>;
    async fn read_at(&self, off: u64, len: usize) -> Result<AlignedBuf, TempError>;
}
```

- *Promises.* Nothing in temporary storage is logged, replicated or recovered. Every file is deleted when its owner ends, and all files are deleted when the node restarts. Space is accounted per database through `OpCtx`, so the governor can limit it later.
- *Disk access.* Through the same injected block-device contract as the log store and page store, on a local disk of the compute node. In single-box mode it is a separate directory on the server.
- *Tests.* Conformance against an in-memory fake: ownership cleanup, per-database accounting, and cleanup after a simulated crash.

### 4.4. L3 Log, Cache and Replication

**`cascade-log-api` and `cascade-log`: the per-scope log writer (built from today's `WalWriter`).**

- *Promise.* `append` reserves a contiguous LSN range. The writer streams filled bytes to the `LogSink` and publishes them to log shipping through a tap. `wait_durable(lsn)` resolves only when the `LogSink` reports `lsn` durable: once a write quorum of log stores has acknowledged it in the managed service, or once the local log store has flushed it on a single server. A sink failure or a fencing error stops the writer, and every waiter receives an error.
- *API sketch.*

```rust
pub trait Log {                                      // Send + Sync on the leader node: shared by the scope's write cores
    fn scope(&self) -> ScopeId;
    fn append(&self, rec: &LogRecord) -> Reservation<'_>;
    fn durable_lsn(&self) -> Lsn;
    async fn wait_durable(&self, lsn: Lsn) -> Result<(), LogError>;
    fn tap(&self) -> LogTap;                         // ordered stream of filled bytes plus durable-LSN updates
}
```

- *Thread affinity.* Any core in the scope's write core set (Section 4.8) may append. Reservation is one atomic step per record, as HLD v2 §5 specifies, so adding cores to a scope does not serialize its writers on one core. Group commit batches the appends of all those cores.
- *Tests.* The existing writer tests, run against the in-memory `LogSink`, plus fault injection and concurrent appends from several simulated cores.

**`cascade-pagecache`: the compute page cache (built from today's `cascade-bpm`).**

- *Promise.* A cached page's LSN equals the LSN of the last record applied to it. A page whose LSN is above the durable LSN is never evicted. A miss is filled from `PageSource` at the scope's durable LSN on a leader, and at the applied LSN on a follower or read replica. The cache never writes pages to storage.
- *Mini-transactions are the only way to change a page.* The cache's public contract offers reads and latches but no `apply` method. Pages change only through the cache's mini-transaction type, which reserves one contiguous LSN range in the `Log`, applies the engine's redo records to the latched pages through `cascade-redo`, and then hands the sealed group to the `Log`; no page leaves the cache before its records are durable. (This order, and engines supplying records that the seam applies, is the access layer's proposal A10, pending decision D-5 in the [access decision register](../crates/05-access/AI/decision-register.md).) Only the mini-transaction module can create the capability that permits a page change, so a change that skips the log does not compile.
- *Follower mode.* On a follower or read replica, the cache accepts record groups from its `FollowerFeed` in `cascade-logship` instead of from local mini-transactions. It applies them only to resident pages, only up to the durable LSN, and one whole group at a time.
- *Its dependency on durability.* The cache defines a narrow `DurableLsn` contract, the successor of today's `WalDurability` gate. The log implements it on a leader, and log shipping implements it on a follower.
- *Thread affinity (open decision D1).* The implemented buffer pool is shared per NUMA node (ADR-0003), while HLD v2 §8 has one cache per core owned by the scope's core. Because a database or table may now run on a set of cores (Section 4.8), the cache must be shareable by every core in that set, which favors the shared per-NUMA design. Under Rule 10 this choice is part of the contract and must be made before the contract is frozen (§10, D1).
- *Tests.* The existing pool tests, plus conformance tests for the durable-LSN eviction rule, refetch at the durable LSN, mini-transaction atomicity under a simulated crash, and follower-mode application, all against in-memory fakes.

**`cascade-logship`: log shipping to followers (new).**

- *Promise.* For each scope, the leader pushes an ordered stream of log record groups to every follower, together with the current durable LSN and closed timestamp. A follower that detects a gap, or that reconnects, fills it from `LogSource::read_log` and then rejoins the live stream. The stream never delays a commit.
- *API sketch.*

```rust
pub trait LogShipper {                       // leader side, fed by Log::tap
    fn add_follower(&self, scope: ScopeId, node: NodeId, from: Lsn);
    fn remove_follower(&self, scope: ScopeId, node: NodeId);
}
pub trait FollowerFeed {                     // follower and read-replica side; two implementations
    async fn next(&mut self) -> Result<ShipBatch, ShipError>;   // record groups, durable LSN, closed ts
    fn applied_lsn(&self) -> Lsn;
}
```

- *Batching.* Shipping streams and closed-timestamp heartbeats for all scopes between two nodes travel in one batched stream, and idle scopes send nothing (Section 2.4).
- *Two feeds.* `FollowerFeed` has two implementations behind one contract: the pushed feed from the leader, used by followers, and a feed built on `LogSource::subscribe`, used by read replicas. The page cache and transaction manager cannot tell them apart, so read replicas need no code of their own above this point.
- *Depends on.* `cascade-net-api` for transport, `LogSource` for catch-up and for the read-replica feed, and `cascade-wal-format` for record group boundaries.
- *Tests.* Leader and follower over the simulated network with drops, reordering, duplicates and partitions. A differential check confirms that a follower's resident pages equal the leader's pages at the same LSN.

### 4.5. L4 Transactions

**`cascade-txn-api`: the transaction contracts.** It defines the two transaction levels: per-scope transactions, implemented by `cascade-scope-txn`, and session-level transactions, implemented by `cascade-txn-coord`. It also defines `Snapshot` and the visibility rule, `Snapshot::visible(version_word)`, which both engines and every index kind use, so that visibility is decided in one place.

**`cascade-scope-txn`: per-scope transactions (HLD v2 §6 and §7).**

- *Promise.* A transaction is committed if and only if its commit record is durable, as reported by the `LogSink`: on a write quorum of log stores in the managed service, or on the local disk on a single server. Commit timestamps are unique and increase with the commit record's LSN within a scope. The scope's closed timestamp never passes a commit that is not yet durable.
- *Leader and follower roles.* On a leader it assigns transaction IDs, appends commit and abort records, maintains the status table and advances the closed timestamp. On a follower it rebuilds the same state from applied log records, including the set of in-flight transactions. On promotion it switches roles without scanning the log, which is what makes failover fast.
- *API sketch.*

```rust
pub trait ScopeTxn {                                 // Send + Sync on the leader node: shared by the scope's write cores
    fn role(&self) -> Role;                          // Leader or Follower
    fn begin_write(&self, ctx: &OpCtx<'_>, iso: Isolation) -> ScopeTxnCtx;
    async fn commit(&self, txn: ScopeTxnCtx) -> Result<Hlc, CommitError>;
    async fn abort(&self, txn: ScopeTxnCtx) -> Result<(), TxnError>;
    fn status(&self, id: TxnId) -> TxnStatus;
    fn closed_ts(&self) -> Hlc;
    async fn wait_closed(&self, ts: Hlc) -> Result<(), TxnError>;
    fn in_flight(&self) -> Vec<TxnId>;               // maintained on followers too
    async fn promote(&self, epoch: Epoch) -> Result<(), TxnError>;
}
```

- *Depends on.* `Log`, `Rt` for the clock, and an `UndoRollback` contract implemented by the engines, so that the transaction manager can roll back without knowing about engines.
- *Tests.* The commit protocol under log failure, the status table rebuilt from a log in follower mode, promotion with in-flight transactions, and closed-timestamp waiting with a virtual clock.
- *Left to detailed design (Serializable isolation and savepoints, product definition §5.3; HLD v2 §7).* `cascade-scope-txn` owns Serializable isolation, using PostgreSQL-style serializable snapshot isolation with read-write dependency tracking, and savepoints. Savepoint rollback marks the subtransaction ids allocated since the savepoint aborted in the status table, which is enough on the heap engine; the index-organized engine also restores its leaf rows from `cascade-undo`. (Subtransaction ids: access layer design §6.6, accepted by Kenneth on 2026-11-27, register D-11.) The detailed design defines how reads are tracked for dependency detection, how much of that state followers keep for promotion, and the savepoint operations in the `ScopeTxn` contract.

**`cascade-txn-coord`: session-level transactions (new).**

- *Promise.* It chooses one read timestamp for all scopes a transaction reads, carries the session's HLC token for read-your-writes, and rejects a write transaction that touches a second scope. Once a database has been split into several scopes, it also runs two-phase commit for DDL, with the scope that holds the catalog as coordinator. In version 1 a database is one scope that also holds its catalog, so DDL and DML can be mixed freely in one transaction, as in PostgreSQL, and two-phase commit is never used.
- *Tests.* Against fake `ScopeTxn` implementations: read-timestamp selection with clock skew, the single-scope write rule, and DDL two-phase commit with a participant failing at each step.

**`cascade-undo`.** Undo records and chains for the index-organized engine (HLD v2 §11), used to read older versions, to roll back a transaction, and to roll back to a savepoint. Every undo record is in the log no later than the leaf change that depends on it, and purge never frees undo newer than the cleanup watermark. Tested with in-memory log and cache.

**`cascade-lock`: the lock manager (new).** Table-level locks for DDL, advisory locks, and wait queues for row writes that hit an in-progress writer, with `NOWAIT`, `SKIP LOCKED` and deadlock detection. Row locks: on the heap engine, exclusive row locks live in version words and shared ones (`FOR SHARE`, `FOR KEY SHARE`) in the lock manager; on the index-organized engine, every row lock lives in the lock manager, behind a per-row flag so unlocked rows cost no lookup. The lock manager also handles waiting and wake-up, including the speculative-insertion wait. (Proposed by the access layer design, A11, pending decision D-5; it replaces "row locks themselves live in version words".) It is per scope and shared by the cores in the scope's write core set. Tests cover wait and wake ordering, timeouts and deadlock cycles.

**`cascade-watermark`: the cleanup watermark (new).** It computes the oldest read timestamp still in use across a scope's leader, followers, read replicas and open sessions, and publishes it to purge and vacuum. Followers report their oldest active read through log shipping acknowledgements. Read replicas receive no stream from the leader, so they report it in a small periodic message to the leader over `cascade-net-api`. Tests use simulated readers on several nodes.

### 4.6. L5 Access

**`cascade-access-api`: the unified table access contract (see the [access layer design](../crates/05-access/AI/access-layer-design.md) §6).** An enum, `Table`, with one arm per engine (heap, index-organized, temporary), plus a test-only trait for the conformance suite (access layer A13, accepted by Kenneth on 2026-11-27 through decision D-5; it replaces the earlier trait `TableAccessMethod`), with an opaque `RowLocator`, `WriteOutcome` that reports conflicts and unique violations as values, and separate scan entry points. Two additions:

- *`TableDescriptor`.* An engine instance is constructed with a descriptor that lists columns, key columns and indexes. Engines never call the catalog. The scope host builds descriptors from the catalog, and the catalog's own system tables use hard-coded bootstrap descriptors.
- *Conformance and fake.* One suite runs against every engine, including temporary tables. A simple in-memory multi-version table implements the contract with no pages, for testing the catalog, executor and data plane.

**`cascade-btree-core`.** A B-tree over the page cache with byte keys and values. It changes pages only through mini-transactions and knows nothing about transactions or visibility.

**`cascade-iot`, `cascade-heap` and `cascade-index`.** Both table engines use `cascade-index`, and neither depends on the other.

- *`cascade-iot` (access layer §11.3; HLD v2 §11 is retired).* The default engine. It stores rows in the leaves of a B-tree ordered by primary key, updates them in place, writes the previous values of changed columns to undo, and finds older versions by following the undo chain.
- *`cascade-heap` (the heap design, `crates/05-access/cascade-heap/AI/heap-design.md`; HLD v2 §12 is retired).* The compatibility engine for tables without a primary key or that need heap behavior. Each update writes a new version; pages are pruned when accessed, and `cascade-maintenance` vacuums one page at a time.
- *`cascade-index` (access layer §11.4; HLD v2 §13 is retired).* Secondary indexes for both engines. Every entry carries its own version word, so index-only scans need no visibility map. On the index-organized engine, entries point at the primary key, so updates that do not change an indexed column write no index entries.
- *Left to detailed design (index types, product definition §5.3 and §5.4).* `cascade-index` owns every secondary index kind, not only B-tree: hash, GIN, GiST, BRIN, and the native `pgvector` and `pg_trgm` indexes. The detailed design defines an index access method contract inside `cascade-index`, so that each kind is its own module or crate behind it, and adds each kind's page formats to `cascade-page` and its redo records to `cascade-redo` under Rule 5. Every kind must carry its own visibility, as B-tree entries do.

**`cascade-sequence` (new).** Sequences for `serial` and identity columns, with cached ranges per scope and logged allocation, so values are never reused after a failover.

**`cascade-maintenance` (new).** It schedules undo purge, heap pruning and vacuum within resource budgets, using the cleanup watermark, and enforces the per-database undo limits, whose exact behavior is left to detailed design (Section 4.12). Tested with a fake watermark and fake engines.

**`cascade-temptable`: the temporary table engine (new).** PostgreSQL's `CREATE TEMPORARY TABLE`, which the product definition promises keeps working without a pooler. It is the third arm of `Table`, so the executor treats it like any other table. Its rows are visible only to the owning session, so it needs no version words, no log and no undo beyond what a savepoint or rollback within the session requires, which it keeps in memory. It holds rows in session memory and moves them to `TempStore` when they exceed a budget. Temporary tables never belong to a scope, never cross nodes, and disappear when the session ends. Writes to a temporary table therefore do not count against the rule that a transaction writes to only one scope. Tested with the access conformance suite against a fake `TempStore`.

### 4.7. L6 Catalog

**`cascade-catalog-api`.** `CatalogReader` (definitions at a timestamp, functions and statistics), `CatalogWriter` (DDL changes inside a transaction) and `CredentialSource` (role verifiers for authentication). Reads are consistent at the given timestamp.

**`cascade-catalog`.** It stores catalog rows as index-organized tables through `cascade-access-api`, keeps a per-core schema cache, and holds schema leases (HLD v2 §14). In version 1 the catalog tables live in the database's own scope, so a transaction can change both the schema and the data. A separate catalog scope is created only when a database is split into several scopes in a later release (decision D10). Cache invalidation comes from a catalog change feed that the scope host provides from the log of the scope that holds the catalog. The catalog does not read the log directly.

**`cascade-stats` (new).** `ANALYZE` and automatic statistics collection, which write statistics through `CatalogWriter` and are read by the planner through `CatalogReader`.

**`cascade-authz`: authorization (new).** Authentication only proves who a client is. This component decides what that role may do, as PostgreSQL does with `GRANT`, object ownership, default privileges and row-level security.

```rust
pub trait Authorizer {
    fn check(&self, ctx: &OpCtx<'_>, role: RoleId, obj: ObjectRef, privilege: Privilege) -> Result<(), AuthzError>;
    fn row_policies(&self, ctx: &OpCtx<'_>, role: RoleId, table: ObjectUuid, cmd: CommandKind) -> Vec<PolicyExpr>;
}
```

- *Callers.* The analyzer calls `check` for every object a statement names, and the planner adds the expressions from `row_policies` to every scan of a table with row-level security. The executor never checks privileges itself, so a check cannot be skipped by a plan that is cached and reused under another role: plan cache entries record the role they were checked for.
- *Depends on.* `CatalogReader` for roles, memberships, privileges and policies, read at the statement's timestamp.
- *Tests.* Against a fake catalog: inheritance through role membership, `PUBLIC`, ownership, column privileges, and policy combination for permissive and restrictive policies, checked against cases taken from the PostgreSQL regression suite.

### 4.8. L7 Scope Host and Data Plane

This layer owns everything that happens to scopes while the server is running.

**`cascade-scope`: the scope host (new).**

- *Promise.* It opens, promotes, demotes and closes scopes on this node, and gives other components a `ScopeHandle` for each open scope. A handle provides the scope's log (on a leader), transaction manager, lock manager, table registry and catalog change feed.
- *Leader open.* Acquire the scope from the metadata store, open the log writer with `LogSink::open_writer` (which fences older epochs and returns the durable LSN), rebuild transaction state from the log tail, roll back in-flight transactions through undo, and start per-scope background work.
- *Follower open.* Register with the leader's log shipper from the current durable LSN, start the follower feed, and run the transaction manager in follower mode.
- *Read-replica open.* The same as a follower, except that the feed subscribes to the log store instead of registering with the leader.
- *Promotion.* Described in Section 6.
- *Table registry.* It maps a table's `ObjectUuid` to an open engine instance and its descriptor, built from the catalog.
- *Tests.* Entirely against fakes: the metadata store, `LogSink`, `LogSource`, `PageSource`, log shipping and the engines. Promotion and recovery are tested here, not only in the simulator.

**`cascade-dataplane`: placement and routing (new).** This crate decides where work runs. Placement is a pluggable strategy, not a fixed rule, so that the same router serves a node with thousands of small tenant databases and a node with one very busy database.

*Core sets.* The unit of placement is a **core set**: the cores of one node on which a database's work may run. Every node that holds a database's scope, as leader, follower or read replica, keeps its own assignment for that database. A database starts with the core set its strategy gives it, which for a small tenant may be a single core shared with other small databases. A hot database can be given more cores, and a hot table inside it can be given its own core set, so that its fragments run on dedicated cores while the rest of the database stays where it was. A core set never spans nodes. When a NUMA affinity of `single` applies, a core set stays within the database's NUMA node. A scope's **write cores** are the union of the core sets of its database and of its tables; the scope's log, transaction manager and lock manager are shared by exactly those cores.

*What this means for the engine.* Thread-per-core applies to connections and sessions: each one lives on one core and its state needs no locks. It does not apply to the data of a scope. Within its leader node, a scope is a **shared-memory engine** whose writers are its write cores, in the same way as InnoDB or SQL Server: the log writer, transaction manager, lock manager, status table and page cache are shared structures with atomics and latches. A scope that needs only one core behaves like the thread-per-core design, because its structures are never contended. The contention budget is that a commit uses at most one contended atomic operation and takes no lock, and Rule 11 requires model-checked tests for these structures. (The access layer design proposes one exception, a short commit-section mutex that issues the commit timestamp and LSN together, pending its decision register.)

```rust
/// The current assignment of databases and tables to core sets on this node. Versioned and
/// swapped atomically, so the request path reads it without locks.
pub struct CoreAssignment { /* version; DatabaseId → CoreSet; (DatabaseId, table) → CoreSet */ }

/// The pluggable policy. It runs on the governor's schedule, never on the request path.
pub trait PlacementStrategy: Send + Sync {
    fn plan(&self, load: &LoadReport, current: &CoreAssignment) -> CoreAssignment;
}

/// Request path, step 1: called once per connection, after the startup handshake.
pub trait ConnectionPlacer {
    fn place(&self, db: DatabaseId, intent: SessionIntent) -> Placement;   // Local(core) or Proxy(leader node)
}

/// Request path, step 2: called for each plan fragment.
pub trait Router {
    async fn run_fragment(&self, scope: ScopeId, ts: ReadTs, frag: PlanFragment) -> Result<RowStream, RouteError>;
    async fn call(&self, scope: ScopeId, op: DataOp) -> Result<DataReply, RouteError>;  // only where no fragment can be formed
}
```

- *Strategies.* The crate ships three, and the composition root or a database setting selects one. **Packed** places many small databases on shared cores and grows a database's set only when it becomes busy; it is the default for multi-tenant nodes. **Spread** gives a database the whole NUMA node, or the whole machine with `cross` affinity; it suits a node dedicated to one large database. **Pinned** uses core sets that an operator sets explicitly. New strategies implement `PlacementStrategy` and pass the same conformance suite.
- *Feedback.* The resource governor calls `plan` on a fixed interval with a fresh `LoadReport`, and publishes the result. A strategy must change assignments gradually: it grows a set when load stays above a threshold for a minimum time, shrinks it only after a longer quiet period, and never moves the same database twice within a minimum dwell time, so that assignments do not flap.
- *Connection placement.* The PostgreSQL protocol cannot redirect a client to another node, so which node a client reaches is decided by its connection string or a load balancer. Cascade DB publishes each database's leader and replica endpoints so that drivers and load balancers can send read-write sessions to the leader and read-only sessions to a replica. On the node the client reached, the placer decides in one of two ways. If this node leads the database, or the session is read-only and this node holds a follower or read replica, the connection is placed on the least-loaded core in the database's core set. Otherwise the session is read-write on a node that does not lead the database, and the whole session is **proxied** to the leader node: the acceptor forwards the connection's protocol messages to the leader, where the session, its transactions and its locks all live. A transaction therefore never has its session on one node and its writes on another. Placement happens once, at accept. A session is read-only when the client asks for it at startup, through `default_transaction_read_only` or a read-only endpoint; otherwise it is treated as read-write. A read-only session that tries to write receives PostgreSQL's read-only transaction error, as on a PostgreSQL hot standby.
- *Routing.* For each fragment, the router checks the current assignment. If the connection's core is in the fragment's core set (the table's own set if it has one, otherwise the database's set), the fragment runs locally with no queue and no copy. Otherwise the router sends it as one unit to the least-loaded core in that set on the same node through `Rt::submit_to`. Fragments never cross nodes for writes, because read-write sessions always run on the leader node. Read-only fragments may run on another node only when a later release splits a database into scopes led by different nodes. The unit that crosses cores is always a fragment, so a statement costs at most one hand-off rather than one per row.
- *Promises.* Every fragment runs on a core in its current core set on a node that holds its scope in the right role. A change of assignment affects new connections and new fragments at once, and never interrupts a running fragment. Existing connections stay on their core (open decision D9). If that core later leaves the database's set, the connection keeps working, and the router sends its fragments to a core that is still in the set.
- *Tests.* Each strategy is tested as a pure function from load reports to assignments, including hysteresis and dwell time. The placer and router are tested with a fake runtime, network and metadata store, in which several cores and nodes are simulated on one thread: local fast path, routing to a table's dedicated set, routing after a set shrinks, and proxying a read-write session to a leader on another node.

**`cascade-governor`: the resource governor (new).** It measures how much each database and table uses, feeds placement, and later enforces limits.

- *Measurement.* Components report usage through the `UsageSink` contract in `cascade-telemetry`, tagged from `OpCtx` with the database and, where known, the table: CPU time per fragment, connections, page cache residency, I/O submissions and log bytes. The governor aggregates these into a `LoadReport` per node on a fixed interval.
- *Placement feedback.* On each interval it calls the configured `PlacementStrategy` with the report and publishes the new `CoreAssignment` to the data plane. It detects hot databases and hot tables this way, and it is the only component that changes an assignment.
- *Limits (product definition §8.1).* The same data later drives admission control and per-database limits on CPU, memory, I/O and connections. In version 1 it records usage and publishes it as a system view, without enforcing limits.
- *Tests.* Synthetic usage streams on a virtual clock: hot-spot detection, interaction with each strategy, and the absence of flapping under oscillating load.

**`cascade-failover`: the failover controller (new, managed service only).** It watches leases and membership from the metadata store. When a leader's lease expires, it chooses a follower for each affected scope, groups the scopes by chosen node, and has each node acquire its group in one batch (Section 2.4), preferring the one with the highest applied LSN, and asks that node's scope host to promote. It does nothing else: restoring the number of followers afterwards is the job of `cascade-cluster`. Tested against a fake metadata store, including competing controllers and flapping nodes.

**`cascade-cluster`: the cluster controller (new).** It owns the life of databases and scopes across the cluster: where each one lives, and how that changes. The data plane places work on cores within a node; this component places scopes on nodes. It provides two implementations of `ClusterControl`: the cluster controller of the managed service, and a local controller for single-box mode that creates and drops databases on the server itself, orchestrates restore and branch creation wherever the open-source edition offers them, and does not support moving scopes or adding replicas. Which features each edition offers is a product scoping question, not an architectural one: the components support either answer.

```rust
pub trait ClusterControl {
    async fn create_database(&self, spec: DatabaseSpec) -> Result<DatabaseId, ClusterError>;   // its scopes, storage and first leader
    async fn drop_database(&self, db: DatabaseId) -> Result<(), ClusterError>;
    async fn move_scope(&self, scope: ScopeId, to: NodeId) -> Result<(), ClusterError>;         // planned handoff, no failure involved
    async fn set_replicas(&self, scope: ScopeId, followers: u8, read_replicas: u8) -> Result<(), ClusterError>;
}
```

- *Who calls it.* SQL `CREATE DATABASE` and `DROP DATABASE` reach it through `cascade-query`, in both editions. In the managed service, the service's own operator API also calls it; that API is not yet described in this document.
- *What it does.* Creating a database allocates its scope, which in version 1 also holds its catalog, assigns its log store replicas and page stores, picks a leader and followers, and asks those nodes' scope hosts to open them. This is what makes database creation take seconds (product definition §4.5). It also spreads leaders so that every node carries write load, moves scopes when a node is drained, and adds or removes followers and read replicas.
- *Relation to failover.* `cascade-failover` reacts to failures within seconds and only promotes. `cascade-cluster` makes planned, slower changes and restores the desired layout afterwards. Both act only by writing to the metadata store and asking scope hosts to act, and the metadata store's epochs keep them from conflicting.
- *Depends on.* `MetaStore`, the scope hosts' remote interface over `cascade-net-api`, and load reports from each node's governor.
- *Tests.* Against a fake metadata store and fake nodes: creation and drop with failures at each step, leader balancing, draining a node, and interaction with a concurrent failover.

### 4.9. L8 SQL

The SQL layer is a pipeline of transformations, each with its own data contract.

```
SQL text ─parser─▶ ParseTree ─analyzer─▶ BoundQuery ─planner─▶ PhysicalPlan ─executor─▶ RowBatch stream
```

- **`cascade-query-api` (new)** defines `QueryService`, which the session, PL/pgSQL and the migration subscriber use. Nested calls, from triggers, functions and constraint checks, take a `NestedCtx` that carries the outer transaction, a command counter so that nested statements see earlier changes, and a depth limit.
- **`cascade-query` (new)** implements `QueryService` by running the pipeline, managing prepared statements and calling the transaction coordinator. Statements that act on whole databases, such as `CREATE DATABASE` and `DROP DATABASE`, are passed to `ClusterControl` instead of running in a transaction, as in PostgreSQL.
- **`cascade-pg-parser(-sys)`** turns SQL text into a safe parse tree using `libpg_query` (implemented today).
- **`cascade-sql-analyzer`** resolves names and types against `CatalogReader`, checks privileges through `cascade-authz`, and produces a `BoundQuery`.
- **`cascade-sql-planner`** turns a `BoundQuery` into a `PhysicalPlan` using only `CatalogReader`. It does no I/O. Plans are serializable and have stable fingerprints. The planner groups each subtree that touches only one scope, such as a scan with its filters and projections, a partial aggregate, or a single-scope insert, update or delete, into a `PlanFragment` that can run on a core in its core set as one unit. It adds row-level security filters from `cascade-authz`, and applies hints and pinned plans from `cascade-plan-store`.
- **`cascade-plan-store` (new)** caches plans for prepared statements and stores captured and pinned plans for the product definition's plan stability features (product definition §4.7).
- **`cascade-sql-exec`** runs a `PhysicalPlan` and produces `RowBatch` streams, yielding through `Rt::should_yield`. It runs each fragment through `Router::run_fragment`, which takes the local fast path when the connection's core is in the fragment's core set. Operators above the fragments, such as joins across scopes, final sorts and final aggregation, run on the connection's core.
- **`cascade-pgtypes`** owns SQL-level type behavior: casts, operators, functions, and text and binary wire formats. Values and keys themselves are in `cascade-datum` (L1).
- **`cascade-plpgsql`** runs PL/pgSQL through `QueryService` with nested contexts.
- **`cascade-sysview` (new)** is the registry of system views. The composition root registers every `SystemViewProvider`, the analyzer resolves view names such as `pg_stat_activity` through the registry, and the executor scans them like tables. Views that gather state from several cores or nodes, such as `pg_stat_activity` for a whole node, are assembled here through `cascade-dataplane`. Catalog tables such as `pg_class` are not system views in this sense: they are real tables in the catalog.

### 4.10. L9 Front End

**`cascade-pgwire`.** The PostgreSQL protocol: framing, startup, authentication (SCRAM-SHA-256, today's `scram.rs`), simple and extended query, `COPY` and notifications. It works over any `AsyncRead + AsyncWrite` stream and asks `CredentialSource` for verifiers. Tested with in-memory streams and recorded client traces.

- *Left to detailed design (transport security, product definition §5.1).* `cascade-pgwire` owns TLS for client connections and MD5 password authentication. TLS between nodes is owned by `cascade-net-api` and its runtime adapter. Both are specified in the detailed designs of those components.

**`cascade-session`.** Per-connection state: prepared statements, portals, settings, the current transaction and notification subscriptions. It calls only `QueryService` once the connection is running. Tested against a scripted fake.

**Connection setup.** The runtime accepts each socket on a housekeeper core and passes it to the session acceptor in `cascade-session`. The acceptor runs the `cascade-pgwire` startup handshake, which authenticates the client and names the database, and asks `ConnectionPlacer::place` where the session belongs. For a local placement it calls `Rt::hand_off`, and the connection runs on that core for its lifetime. For a proxy placement it forwards the connection's protocol messages to the leader node's acceptor over `cascade-net-api`, and the session runs there. If the leader later moves, a proxied session ends as it would in any failover, and the client reconnects. The handshake is the earliest point at which the database is known, so it is also the earliest point at which the connection can be placed.

**`cascade-notify` (new).** `LISTEN` and `NOTIFY` across sessions, cores and nodes. Notifications are delivered after the notifying transaction commits, in commit order.

**`cascade-subscriber`: inbound logical replication (new).** The migration path in product definition §6. It connects to a source PostgreSQL server as a logical replication client, using the `pgoutput` protocol, copies the initial data of the published tables, and then applies the ongoing change stream.

- *How it applies changes.* Through `QueryService`, as an ordinary session would, so that constraints, triggers (if enabled), indexes and replication to followers all happen exactly as for application writes. It applies each source transaction as one Cascade DB transaction, in source commit order, and records the source LSN it has applied in the same transaction, so that it resumes exactly after a restart.
- *Commands.* `CREATE SUBSCRIPTION`, `ALTER SUBSCRIPTION` and `DROP SUBSCRIPTION`, stored in the catalog. Progress is published as a system view, matching `pg_stat_subscription`.
- *Depends on.* `QueryService`, `CatalogReader`, and outbound connections through `cascade-net-api`.
- *Tests.* Against recorded `pgoutput` streams and a fake `QueryService`: initial copy, ordering, restart from a recorded position, and type conversion edge cases.

### 4.11. Adapters and Composition Roots

**`cascade-runtime`.** The thread-per-core runtime: topology, SMT and NUMA configuration, reactor boot, the accept loop and connection hand-off (the choice of core comes from `ConnectionPlacer`), `submit_to`, cooperative-yield accounting, the stall detector, the reaper and shutdown. It implements `cascade-rt-api` and `cascade-net-api`.

**`bin/cascade-server`, `bin/cascade-logstore-server` and `bin/cascade-pagestore-server`.** Production composition roots. `bin/cascade-server` runs a compute node. In single-box mode it also wires the local log store, page store, metadata store and controller into the same process, and wires no failover controller, log shipping or storage network clients. The other two binaries run log store nodes and page store nodes in disaggregated mode. Each binary reads configuration, constructs implementations and connects them. `bin/cascade-server` passes them to the scope host, data plane, governor, cluster controller and front end, and in disaggregated mode to the failover controller. The two storage binaries connect a log store or page store to its server in `cascade-storage-remote`. None of them contains any other logic (Rule 3).

**`cascade-testkit` and `cascade-sim`.** The reference fakes and conformance suites, and the deterministic simulator (Section 8).

### 4.12. Requirements Left to Component Detailed Design

For the requirements below, the owning component already exists, but its contract in this document does not yet cover the feature. This document only assigns ownership. Each owner's detailed design specifies the API and promises, and must not change the layering or move the responsibility to another component without changing this document first.

| Product definition requirement | Owner | Notes on where it appears |
| :--- | :--- | :--- |
| Encryption at rest, one key per scope (§4.2, §4.4) | `cascade-storage-api`, `cascade-logstore`, `cascade-pagestore` | Section 4.3 |
| TLS and MD5 for clients, and TLS between nodes (§5.1) | `cascade-pgwire`, `cascade-net-api` | Sections 4.10 and 4.1 |
| Branching and point-in-time restore per database (§4.5) | `cascade-storage-api`, `cascade-pagestore`, `cascade-archive`, `cascade-meta`, orchestrated by `cascade-cluster` | Section 4.3 |
| Hash, GIN, GiST, BRIN, `pgvector` and `pg_trgm` indexes (§5.3, §5.4) | `cascade-index`, with formats in `cascade-page` and redo in `cascade-redo` | Section 4.6 |
| Serializable isolation and savepoints (§5.3) | `cascade-scope-txn`, with `cascade-undo` for savepoint rollback | Section 4.5 |
| Rolling upgrades: planned handoff of a healthy leader, and mixed-version nodes during an upgrade (§4.8) | `cascade-cluster` (`move_scope`), `cascade-scope` (demote and promote), `cascade-net-api` (message version negotiation); version gates are Appendix A.1 | Section 4.8 |
| PostgreSQL behavior version chosen per database (§4.8) | `cascade-catalog` stores it; `cascade-sql-analyzer` and `cascade-pgtypes` honor it | Section 4.7 |
| Maximum transaction age and undo size per database, and reporting what holds undo back (§4.1) | `cascade-watermark` and `cascade-maintenance`, reporting through `cascade-sysview-api` | Section 4.5 |
| A page cache that sizes itself to the machine (§4.1) | `cascade-pagecache` | Section 4.4 |
| Tenant tag on every I/O submission and per-database page cache accounting from version 1 (§8.1) | The block-device contract used by the storage components, `cascade-pagecache`, and `cascade-runtime` | Sections 4.3 and 4.4 |

---

## 5. How the Components Interact

Every step below is a call on a published contract.

### 5.1. Where Work Is Routed

Routing happens at two points, and a feedback loop adjusts both.

1. **At connection time.** As soon as the startup handshake has named the database, `ConnectionPlacer` puts the connection on the least-loaded core in that database's core set on the node the client reached. From then on, the session, parser, analyzer, planner and executor run on that core.
2. **Per fragment, only when needed.** For each plan fragment, the router runs it locally if the connection's core is in the fragment's core set. Otherwise it sends the whole fragment once to a core in that set on the same node. This happens when a hot table has its own core set, or when a database's set has changed since the connection was placed. A read-write session that reached a node other than the leader was already proxied whole to the leader in step 1, so writes never cross nodes inside a transaction.
3. **Feedback.** The resource governor measures usage per database and per table, and periodically asks the placement strategy for a new assignment. A database that becomes hot is given more cores, a hot table can be given dedicated cores, and quiet databases are packed back together. No database is limited to one core by design.

### 5.2. Request Paths

**Write path for one row update on the index-organized engine (on the scope's leader).**

1. `cascade-session` calls `QueryService::execute`. `cascade-query` runs the pipeline, and `cascade-txn-coord` opens a write transaction on the scope.
2. The connection was placed on a core in the database's core set when it was established, so the executor runs the update fragment through `Router::run_fragment`, which calls the engine directly on the same core. If the table had its own core set, the router would first send the fragment to a core in that set.
3. The engine opens a mini-transaction, has `cascade-undo` append an undo record in it, and adds a leaf update record.
4. The mini-transaction appends both records to the `Log` as one range and applies them to the latched pages through `cascade-redo`.
5. `cascade-log` streams the bytes to its `LogSink`, which is the quorum client in the managed service or the local log store on a single server. In the managed service it also taps them to `cascade-logship`, which pushes them to followers.
6. At commit, `ScopeTxn::commit` appends a commit record and waits until it is durable: acknowledged by a write quorum of log stores, or flushed to the local disk on a single server. The client is then acknowledged. In the managed service, followers apply the records once the durable LSN they receive has passed them.
7. The page store receives the durable records through `LogSource::subscribe` and merges them into the page in the background through `cascade-redo`. No compute node ever writes the page.

**Read path on a follower.** The coordinator picks a read timestamp. The router sends the read to the local follower, whose transaction manager waits until the applied closed timestamp reaches the read timestamp. The engine reads pages from the follower's cache, fetching misses from the page store at the follower's applied LSN, and decides visibility through `Snapshot::visible`.

---

## 6. Failover and Failure Model

The architecture must make failover take seconds. The exact timeouts and the remaining design details will be settled in the failover detailed design. This section fixes the sequence and the properties that make seconds achievable.

**Sequence, per affected scope.**

1. **Detect.** The leader stops renewing its lease in the metadata group. When the lease expires, `cascade-failover` sees the event through `MetaStore::watch`.
2. **Choose.** The controller picks the follower with the highest applied LSN and asks its scope host to promote.
3. **Fence.** The follower calls `MetaStore::acquire` with the next epoch, then `LogSink::open_writer`. The log stores now reject any append from the old epoch, so even if the old leader is still running, it cannot make another write durable.
4. **Confirm the durable LSN.** `open_writer` returns the durable LSN, established by a quorum read. The follower has already applied everything up to its last received durable LSN. It discards any received records beyond the confirmed durable LSN and fetches any it is missing from the log store.
5. **Finish transactions.** Transactions that were in flight at the durable LSN are rolled back through undo. The follower already tracks this set, so no log scan is needed.
6. **Serve.** The transaction manager switches to leader role, advances the HLC past the persisted high-water mark, and starts accepting writes. The router learns the new placement from the metadata watch.
7. **Restore redundancy.** `cascade-cluster` notices that the scope is short of followers and asks another node to open a new one.

**What makes seconds possible.**

- No data is copied and nothing is replayed onto compute, because the log and page stores hold the data and the follower's cache is already warm.
- Followers keep transaction status, the in-flight set and the closed timestamp current, so promotion does not scan the log.
- Fencing at the log store makes aggressive lease timeouts safe. A false suspicion costs a needless failover, never a split brain.
- Scopes fail over independently and in parallel across surviving nodes.

**Single-box mode has no failover.** A single server leads every scope it holds, so there is no follower to promote. If the server fails, its databases are unavailable until it restarts. On restart, each scope opens its local log store, and the page store applies any log beyond the point it had written to disk, so recovery time depends on how far page writing had fallen behind, not on the size of the database. No acknowledged commit is lost, because a commit is acknowledged only after its log record is flushed to the local disk. If the disk itself is lost, the database is restored from the archive, and changes made after the last archived log are lost. High availability is a feature of the managed service.

**What clients see.** Connections to a failed node drop, and clients reconnect, as with any PostgreSQL failover. Connections on surviving nodes keep working. Their transactions that were writing to a failed scope receive an error and can be retried. Transparent session migration is a possible later feature and is not part of this design.

**Open details for the failover design.** Lease length and renewal interval, how follower placement handles availability zones, and how the router handles requests that arrive during a promotion.

### Failure model

The table lists the failures the managed service is designed to tolerate, with the default configuration of N = 3 and W = 2 log store replicas, one follower per scope, two page store replicas, and a metadata group of three members in three availability zones.

| Failure | Reads | Writes | Recovery |
| :--- | :--- | :--- | :--- |
| One compute node | Continue on followers and replicas elsewhere. Sessions on the failed node drop | Scopes it led stop until a follower is promoted, within the failover budget | Failover (this section), then `cascade-cluster` restores followers |
| One log store replica of a scope | Unaffected | Unaffected, since two of three still acknowledge | The replica repairs from its peers, or `cascade-cluster` replaces it |
| Two log store replicas of a scope | Continue up to the durable LSN | Stall for that scope, with no data lost | Resume when a replica returns or is replaced |
| One page store replica | Served by the other replica | Unaffected | Rebuilt from the log store and the archive |
| All page store replicas of a scope | Cache hits continue; misses stall | Commits continue, since they never wait for page stores | Rebuilt from the archive's page images and the log |
| Metadata group loses its quorum | Unaffected | Continue on current leaders, which fencing keeps safe | No failover, creation or moves until the quorum returns. Leases that lapsed only because the metadata group was down do not trigger failover when it returns |
| A leader partitioned from its log stores | Its followers keep serving up to the durable LSN | Stall on the old leader, which then loses its lease | A follower is promoted, and the old leader is fenced |
| One availability zone | Continue in the other zones | Continue, since each scope's log stores, followers and metadata members are spread across zones | Followers in other zones are promoted for scopes led in the lost zone |
| The server in single-box mode | None until restart | None until restart | Restart and recover from the local log and pages, with no acknowledged commit lost. If the disk is lost, restore from the archive |

Beyond these, for example the loss of two zones at once, the architecture favors consistency: affected scopes stop accepting writes rather than risk losing an acknowledged commit.

### Design budgets

These are provisional numbers that the architecture is designed to meet. The failover, cross-zone and follower budgets apply to the managed service; the others apply to both editions. The final product targets are set in product definition §9 after benchmarking.

| Budget | Target | What it depends on |
| :--- | :--- | :--- |
| Commit latency, p99, log stores in one zone | 2 ms | One round trip to the second-fastest log store, one flush, and group commit |
| Commit latency, p99, log stores across zones | One cross-zone round trip plus 1 ms | Zone placement of log stores (D5) |
| Failover, leader failure to writes accepted | Under 10 seconds, for a node leading up to 10,000 scopes | Lease length, batched epoch acquisition (Section 2.4), warm followers |
| Database creation | Under 5 seconds | `cascade-cluster`, one batched metadata write, no data copied |
| Connections per node | 50,000 | Thread-per-core sessions in `cascade-runtime` and `cascade-session` |
| Open scopes per node | 10,000, most idle | The density rules in Section 2.4 |
| Follower lag behind the durable LSN, steady state | Under 100 ms | Log shipping and follower apply |

---

## 7. Storage Details

- **Log store quorum.** In disaggregated mode, each scope's log is stored on N log stores, configurable with a default of N = 3 and W = 2, on log store nodes in different failure domains. In single-box mode N = 1: the server's own log store, with no quorum.
- **Writes.** The log writer's `LogSink` sends each append to all N log stores. The durable LSN advances when W have acknowledged a contiguous prefix.
- **Log repair.** A log store that missed records fetches them from its peers. A replaced log store rebuilds its segments from its peers.
- **Page stores.** In disaggregated mode, each scope's pages are held by page store nodes with their own replication factor, chosen for read capacity and rebuild time rather than for durability. A lost page store is rebuilt from the log store and older page images, without any risk to committed data. In single-box mode, the server's page store builds pages from its local log.
- **Reads.** The page client reads each page from a page store that has materialized far enough, or that can catch up on demand. A read quorum is needed only on the log side, when a new leader establishes the durable LSN.
- **Retention and archive.** The log store keeps log back to the lowest LSN needed by any retention holder: page stores that have not yet written pages to disk, followers that are catching up, change data capture, and the archive. The restore and branch window lives in the archive's object storage, not on the log store.

---

## 8. Testing Strategy

| Level | What it checks | How |
| :--- | :--- | :--- |
| Pure unit and property tests | Formats, redo, keys, planner rules, SCRAM | Golden bytes, round trips and property tests, with no async runtime. |
| Contract conformance | Every promise of a contract | One generic suite per contract, run against the fake and every implementation. |
| Component tests with fakes | A component's own logic | The component under test is real, and everything it depends on is a fake. |
| Differential tests | Two implementations agree | A leader's cache, a follower's cache and a page store produce identical pages for the same log. Both engines agree on observable results. |
| Deterministic simulation | Concurrency, crashes, partitions, failover and clock skew | Several compute nodes, log store nodes and page store nodes in one process on `cascade-sim`, with simulated runtime and network, seeded fault injection, and invariant checks after every step: no lost acknowledged commit, at most one writer per scope per epoch, followers never ahead of the durable LSN, and identical pages everywhere. Any failing seed replays exactly. |
| Noisy neighbor | One tenant cannot slow another's commits | One tenant floods the log with large writes while the commit latency of quiet tenants on the same log store is measured against the design budget in Section 6, in the simulator and in system benchmarks. |
| Crash consistency | The log store's and page store's own disk handling | Writes cut at random points through the injected block device, followed by the conformance suites. |
| Mode equivalence | Single-box and disaggregated wiring behave the same | The full component test suites, and every simulation that does not involve followers or failover, run under both wirings: local log and page stores in process, and remote ones over the simulated network. |
| System tests | Wiring, compatibility and performance | Real binaries against the PostgreSQL regression suite, ORM suites, benchmarks, and a timed failover test. |

---

## 9. Enforcing the Rules

- **Allowed dependency edges.** A checked-in file lists the crates each crate may depend on. A CI step compares it with `cargo metadata` and fails on any other edge.
- **Implementations stay out of libraries.** Only composition roots may depend on another component's implementation crate.
- **Banned APIs.** Lints forbid the system clock, thread spawning, `std::fs`, `std::net` and direct `glommio` use outside the runtime adapter, the block-device adapters of the log store, page store and temporary store, the archive's object store adapter, and the composition roots.
- **Conformance is mandatory.** A contract without a conformance suite and a fake does not pass review.
- **Existing invariants.** The cross-cutting invariants in `AI/CONTEXT.md` remain in force.

---

## 10. Open Decisions

| # | Decision | Recommendation |
| :--- | :--- | :--- |
| D1 | Page cache thread affinity: one cache per core (HLD v2 §8) or one shared cache per NUMA node (ADR-0003, implemented) | The shared cache per NUMA node. Core sets let several cores read and write one database, so its pages must be reachable from all of them without messaging. The shared cache already exists and is measured. |
| D2 | Separate `-api` crates or contract modules | Separate crates where a contract has several implementations or heavy dependencies. Contract modules elsewhere. |
| D3 | Default number of followers per scope | One, so every scope can fail over. Configurable per database. Read capacity beyond that comes from read replicas, which cost the leader nothing. |
| D4 | Whether a follower must also be ready to serve reads at all times, or may be a cache-only standby for some tiers | All followers serve reads, which is what "active-active" implies. |
| D5 | Placement of log store quorum members across availability zones with N = 3 | One copy per zone where three zones exist. |
| D6 | In a single-box cluster, whether followers' boxes must also hold a copy of every page, or only the log | Obsolete since v0.10: single-box mode has no followers and no cluster (D11). |
| D7 | Replication factor for page store nodes in disaggregated mode | Two, so a page store failure never forces every cache miss onto on-demand log replay. |
| D8 | The default placement strategy and its thresholds | Packed for multi-tenant nodes, Spread for nodes dedicated to one database. Thresholds and dwell times to be tuned by benchmark. A database that outgrows a whole node is split into several scopes in a later release (product definition §4.4), which the router already supports. |
| D9 | Whether connections move to a new core after their database's core set changes or its leader moves | Not in v1, since the router sends their fragments to cores in the new set. A proxied session whose leader moves ends and reconnects. Moving a connection at a transaction boundary is a later optimization, together with the transparent session migration in Section 6. The governor would use it to rebalance connections when a database's core set shrinks. |
| D10 | Where a database's catalog lives | Decided: in version 1, in the database's own scope, so transactions can mix DDL and DML as in PostgreSQL. A separate catalog scope is created when a database is first split into several scopes. This changes HLD v2 §4, which should be updated. |
| D11 | What separates the open-source edition from the managed service | Decided: single-box mode is the free, open-source edition, a standalone server with N = 1, no followers or read replicas, no failover, and no control plane. Disaggregated mode is the managed service, and only it has the control plane (the Raft metadata group, `cascade-cluster`, `cascade-failover` and `cascade-storage-remote`), followers, read replicas and failover. The boundary falls on existing contracts, so both editions run the same components with different wiring (Section 2.2). |

---

## Appendix A. Design Patterns Kept for the Future

These patterns came out of the [industry comparison](reviews/architecture-v0.3-industry-comparison.md). They are not part of the current design. Each is recorded here with the point at which it should be reconsidered, so that current work does not close it off.

**A.1. Cluster version gates for rolling upgrades.** A node running new software keeps writing the old log, page and message formats until every node in the cluster has been upgraded, and the cluster switches to the new formats only after an explicit "finalize" step, as CockroachDB does. *Reconsider* before the first release that changes a log record or page format. *Keep open now* by making every versioned format writer ask for the version to write through a contract, rather than always writing its newest compiled-in version.

**A.2. A wait-event contract.** Every operation that can block reports why it is waiting, for example on a lock, a page, log durability or the network, as Oracle's wait interface and PostgreSQL's wait events do. *Reconsider* when the first observability work starts (product definition §8.2). *Keep open now* by routing every blocking call through contract methods, so that a wait report can be added in one place per contract.

**A.3. Morsel-driven parallel query with exchange operators.** Large scans and aggregations are split into small chunks that idle cores pick up and that yield between chunks, as in HyPer and DuckDB, with exchange operators joining the results. *Reconsider* when parallel query is scheduled. *Keep open now* by keeping executor operators restartable at batch boundaries, and by routing all cross-core work through `cascade-dataplane`.

**A.4. Bounded-staleness follower reads.** A query may read data up to a stated age from the nearest follower without waiting for the leader's closed timestamp, as Spanner and CockroachDB allow. *Reconsider* when read scaling on followers is measured. *Keep open now* by keeping the choice of read timestamp inside `cascade-txn-coord`, so that a staleness option is one new input there.

---

## Appendix B. Version History

| Version | Change |
| :--- | :--- |
| v0.10.2 | Third-review fixes: durability promises stated for both editions, the archive optional on a single server, `CREATE DATABASE` routed to `ClusterControl`, a local key provider, and the composition roots clarified. The layer map marks the components that only the managed service runs. |
| v0.10.1 | Fair sharing of shared log flushes between scopes, and a noisy-neighbor test. |
| v0.10 | Single-box mode becomes the standalone open-source edition, with no quorum, followers, failover or control plane; the control plane, high availability and read replicas belong to the managed service (D11). D6 is obsolete. |
| v0.9.1 | Fixes from a second outside review: the designs-of-record changes listed in §1.1, log store groups drawn from a shared pool, how a session is marked read-only, and lease handling after a metadata outage. |
| v0.9 | Applied findings 1 to 6 of the outside principal review: catalog in the main scope for version 1, scope density, the shared-memory engine within a scope and Rule 11, whole-session proxying to the leader, the failure model and design budgets, and `cascade-archive`. |
| v0.8.1 | Readability, consistency and correctness pass: connection placement limited to the node the client reached, failover and cluster duties separated, read replicas included in the cleanup watermark, and review tags removed from headings. |
| v0.8 | Top-down review: the document's purpose, a responsibility for every component, a trace from product definition requirements to components, and owners for the remaining requirements. |
| v0.7.1 | Encryption, branching and restore, index kinds, and Serializable isolation assigned to their owning components for detailed design. |
| v0.7 | Added `cascade-cluster`, `cascade-tempstore`, `cascade-temptable`, `cascade-authz`, `cascade-sysview-api`, `cascade-sysview` and `cascade-subscriber`. |
| v0.6.1 | Read replicas fed from the log store; small fixes. |
| v0.6 | Placement as a pluggable strategy with core sets, driven by the new `cascade-governor`. |
| v0.5 | Connection placement at accept, and routing of whole plan fragments. |
| v0.4 | Separate log store and page store; single-box and disaggregated deployment modes; Appendix A. |
| v0.3 | Replication and failover; changes from the adversarial review of v0.2. |
| v0.2 | Contract-per-component design and the layer map. |
