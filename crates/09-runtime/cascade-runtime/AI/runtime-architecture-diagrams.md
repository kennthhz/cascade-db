# Cascade DB — Runtime Architecture Diagrams

Visual companion to [runtime-architecture.md](runtime-architecture.md); the spec remains
authoritative. Section references (§) point into the spec.

Each figure has two representations that must be kept in sync:

- **Mermaid source** below — the editable original; renders natively on GitHub and in
  Mermaid-enabled Markdown previews.
- **Rendered figure** in [assets/](assets/) (PNG + SVG) — hand-laid-out for viewers
  without Mermaid support. When a diagram changes, update both.

## A. Runtime components & topology (§2–§4)

Rendered: [PNG](assets/runtime-components.png) · [SVG](assets/runtime-components.svg)

```mermaid
flowchart TB
    CMD["Kernel cmdline — isolcpus / nohz_full / rcu_nocbs → workers + idle HT siblings · irqaffinity → housekeepers (§2.4)"]

    subgraph HK["Housekeeper — cores 0..H (CFS-scheduled, §2.1)"]
        direction TB
        LISTEN["PG listening socket"]
        ROUTE["Connection router — affinity-aware least-loaded (§5.2)"]
        CW["Tier C writer — serialized cluster_state (§6.4)"]
        BG["Background orchestrator — checkpointer · WAL recycler · stats · reaper · stall sampler (§9)"]
        LISTEN --> ROUTE
    end

    subgraph N0["NUMA node 0"]
        direction TB
        subgraph W0["Worker reactor — glommio executor (§3)"]
            direction TB
            TA0["Tier A — connections · operators · batches (!Send)"]
            SNAP0["Tier C replica — ArcSwap&lt;ClusterSnapshot&gt; pointer-load reads (§6.3)"]
        end
        BPM0["BPM pool 0 — frames + page table (§8.1)"]
        WAL0["Arc&lt;WalWriter&gt; — per-DB writers homed to this NUMA (§8.2)"]
    end

    subgraph N1["NUMA node 1"]
        direction TB
        subgraph W1["Worker reactor — glommio executor (§3)"]
            direction TB
            TA1["Tier A — connections · operators · batches (!Send)"]
            SNAP1["Tier C replica — ArcSwap&lt;ClusterSnapshot&gt; pointer-load reads (§6.3)"]
        end
        BPM1["BPM pool 1 — frames + page table (§8.1)"]
        WAL1["Arc&lt;WalWriter&gt; — per-DB writers homed to this NUMA (§8.2)"]
    end

    FILES["PageStore data files · WAL segment files"]

    style HK stroke-width:3px
    style W0 stroke-width:3px
    style W1 stroke-width:3px

    CMD -.->|"constrains"| HK
    CMD -.->|"isolates"| N0
    CMD -.->|"isolates"| N1
    ROUTE -->|"RawFd handoff (§5.1)"| TA0
    ROUTE -->|"RawFd handoff (§5.1)"| TA1
    CW -->|"submit_to → ArcSwap::store"| SNAP0
    CW -->|"submit_to → ArcSwap::store"| SNAP1
    BG -->|"submit_to in-pool work (§9)"| W0
    BG -->|"submit_to in-pool work (§9)"| W1
    TA0 -->|"BPM lookup"| BPM0
    TA0 -->|"append_wal / flush_wal"| WAL0
    TA1 -->|"BPM lookup"| BPM1
    TA1 -->|"append_wal / flush_wal"| WAL1
    TA0 -.->|"cross DB: any pool via hash(PageId) (§2.5.1)"| BPM1
    W0 -->|"page-in · writeback · fdatasync (own ring)"| FILES
    W1 -->|"page-in · writeback · fdatasync (own ring)"| FILES
```

## B. OLTP hot-path data flow (§7–§8)

Rendered: [PNG](assets/runtime-hotpath.png) · [SVG](assets/runtime-hotpath.svg)

```mermaid
flowchart LR
    Q["SQL statement<br/>(Tier A, !Send)"] --> EX["Executor —<br/>vectorized operators"]
    EX -->|"every batch boundary"| YP["yield_point()<br/>500 µs deadline (§7.1)"]
    EX --> LK["BPM lookup<br/>single: BPMs[k]<br/>cross: hash(PageId) mod num_numas"]
    LK -->|"hit"| PIN["pin frame<br/>(PageGuard RAII)"]
    LK -->|"miss"| IO["PageStore read —<br/>this reactor's ring"]
    IO --> PIN
    PIN --> ROWS["rows → batches → results"]
    EX --> TX["COMMIT — append_wal:<br/>reserve LSN → fill record (§8.2)"]
    TX --> FL["flush_wal —<br/>leader election via CAS"]
    FL --> FS["io_uring fdatasync<br/>on leader's own ring"]
    FS --> ACK["durability ≥ filled watermark<br/>→ commit ack"]
    TX -.->|"I7 WAL-before-data:<br/>dirty pages flush only after WAL durable"| WB["page writeback<br/>(shard-home core)"]
```

## C. Workflow — connection lifecycle, query, commit (§5, §7, §8.2)

Rendered: [PNG](assets/runtime-seq-connection.png) · [SVG](assets/runtime-seq-connection.svg)

```mermaid
sequenceDiagram
    autonumber
    participant C as PG client
    participant HK as Housekeeper (core 0..H)
    participant R as Worker reactor (bound NUMA)
    participant B as BPM pool (this NUMA)
    participant W as WalWriter (Arc, per DB)

    C->>HK: connect + PG startup
    HK->>HK: auth · resolve DatabaseId (Tier C snapshot)
    HK->>HK: affinity → candidate set → least-loaded (§5.2)
    HK->>R: hand off RawFd (§5.1)
    R->>R: construct connection object (Tier A, !Send — pinned for life)
    C->>R: query
    R->>B: BPMs[k].lookup(PageId) — pin frame
    alt page miss
        B->>B: PageStore read on R's io_uring ring
    end
    B-->>R: page data
    R->>R: execute batches — yield_point() per batch (§7)
    R-->>C: results
    C->>R: COMMIT
    R->>W: append_wal — reserve LSN, fill record
    R->>W: flush_wal(up_to_lsn)
    W->>W: leader CAS → fdatasync (own ring)
    W-->>R: durable ≤ filled watermark → notify waiters
    R-->>C: commit ack
    C->>R: disconnect
    R->>R: abort in-flight txn · release pins · drop statements (§5.4)
```

## D. Workflow — Tier C catalog write (§6.4, §8.4)

Rendered: [PNG](assets/runtime-seq-tierc.png) · [SVG](assets/runtime-seq-tierc.svg)

```mermaid
sequenceDiagram
    autonumber
    participant R as Worker reactor
    participant HK as Housekeeper
    participant A as All reactors

    R->>HK: submit_to(housekeeper) — catalog change (e.g. CREATE ROLE)
    HK->>HK: cluster_state.apply() — serialized, single writer
    HK->>HK: build immutable ClusterSnapshot
    loop every reactor
        HK->>A: submit_to → ArcSwap::store
    end
    HK-->>R: ack (old snapshot dropped when last reader finishes)
```

## Notes

- **Border weight encodes what is a thread** (Figure A): a **thick** border marks the only
  real threads — the housekeeper band (H threads, one per core) and each worker-reactor box
  (one pinned thread; the box stands for ×W reactors per NUMA). Thin-bordered boxes —
  Tier A, Tier C replica, BPM pools, WAL writers, files — are memory or data owned or shared
  by those threads, not threads themselves. The Mermaid source mirrors this with
  `style … stroke-width:3px` on the same three subgraphs.
- `Arc<WalWriter>` and `ArcSwap<ClusterSnapshot>` are HTML-escaped (`&lt;`/`&gt;`) inside
  Mermaid **flowchart** labels because Mermaid strips unknown HTML tags; the sequence
  diagrams sidestep this with participant aliases (`WalWriter (Arc, per DB)`).
- Figures A and C show one NUMA node pair and one representative `cross`-affinity edge
  (`TA0 -.-> BPM1`); the node-1 mirror images are omitted for readability. The
  `single`-affinity routing-refusal path (§5.2) is prose-only.
- Boot/shutdown (§10) and eviction/checkpoint internals (§8.1, §9) are deliberately not
  diagrammed — they are orchestration detail best read in the spec text.
