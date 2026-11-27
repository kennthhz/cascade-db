# cascade-pagestore — AI Design Context

## Role

The page store (storage-layer-design.md §6, landed as M7 slice 1): it
ingests each attached scope's log, applies the records to pages through
`cascade-redo`, writes pages to disk through a doublewrite area, and serves
pages exactly as of an LSN. It implements `PageSource` and `PageScopes`
from `cascade-storage-api`, sees the log only through `LogSource`, and
commits never wait for it.

One object per process, shared by every core (`Send + Sync`); its futures
stay on the core that made them (S3). Built with `PageStore::open`.

## Hard invariants

1. **On-disk pages are *exactly* the replay of the log below the manifest's
   materialized LSN** — not "at least". A heap insert names a segment, not
   a page, so where it goes is decided by the segment's open page; an open
   page ahead of the manifest would send a record the store replays to the
   next page, twice. Three things keep it: a batch is one snapshot (dirty
   pages and the ingested LSN under one lock hold, at a record boundary);
   the manifest advances only after the batch's pages are synced in their
   home files; and recovery *finishes* an interrupted batch before serving.
2. **Recovery moves both counters.** Completing a batch advances
   `materialized` *and* `ingested` to the batch's LSN. A scope whose
   `ingested` stayed behind replays the log onto pages that already hold it
   (the first read after restart re-ingested the whole log).
3. **No non-durable record is ever applied.** Durability is learned only
   from the chunks the log source returns: a request above the durable LSN
   is `BeyondDurable`, and a chunk that reaches past the durable LSN it
   reports is refused as damage.
4. **Ingest stops exactly where a read asks.** The log source serves whole
   records up to *its* durable LSN, which is often ahead of the LSN a
   reader asked for, so a chunk runs past the request. A record that
   *begins* at the requested LSN ends past it too, and is not the request's:
   everything below the requested LSN is applied, nothing at or above. (The
   first version tested only `ends past`, and refused every boundary below
   the current durable LSN as `NotRecordBoundary`.)
5. **Never a silently newer page.** Only the newest version of a page is
   held; a request older than it is `TooOld`. `oldest_readable_lsn` is the
   *ingested* LSN, so anything at or above it is servable (§4.5 promise 5).
6. **A record the store cannot apply is refused, never skipped.** A record
   whose cell is not a cell; a record type this build does not know; a
   `LegacyInsert`; the reserved bulk-load and unlogged-marker families,
   which are not page-neutral by nature and whose payload shapes do not
   exist yet. Ingest stops at that record's start, the scope's ingest fails
   for the life of the instance, and every later read of the scope — even
   one below the LSN it reached — returns that failure.
7. **A failed sync is permanent (S19).** Any I/O error during a flush marks
   the scope's flush failed for this instance; later flushes return it
   rather than build on unknown state. `open` refuses to build on a file
   whose contents are unknown.
8. **`open` refuses rather than guess.** Page files with no manifest (the
   LSN they are a replay through is unknown); a listed segment missing
   either file (the log that would rebuild it may be gone — zeros would be
   silent loss); a listed segment's open page that does not read back.
9. **The state lock is never held across an `await`.** Enforced by review
   and by clippy's `await_holding_lock`.

## On-disk layouts (one-way doors — pinned byte for byte in `layout.rs`)

All little-endian; every structure ends in a CRC32 over what precedes it;
every decoder refuses rather than guesses. A `BlockDevice` namespace is
flat, so the scope is part of each name.

| File | Holds |
| :--- | :--- |
| `pg-MANIFEST` | one 4 KiB block, replaced by write → sync → rename → sync namespace: the holder id, and per scope its key reference, its materialized LSN, and **the list of segments that have pages** |
| `pg-dblwrite` | the one batch in flight: a header region of whole 4 KiB blocks (8 by default, describing up to 1363 pages), then the page images |
| `pg-<scope>-<segment>` | page images, **verbatim**: page `n` at `n × 8 KiB` |
| `pgm-<scope>-<segment>` | one 64-byte record per page: page LSN, image CRC, and room for §7's key version, nonce and tag |

- **Why the image is verbatim and its checksum is in a sidecar.** The old
  `CoreStorage` kept a CRC in the first four bytes of the page slot — which
  in `cascade-page`'s layout are the format version and page kind, so a
  page read back through it looked never-written. The §10 differential law
  holds on the disk bytes themselves, and the record beside the image
  carries what the image cannot: a CRC over the *whole* image, the page LSN
  in a form-independent place, and §7's fields without `cascade-page`
  giving up a byte. (This resolves §11 item 3's need without a page-format
  change; the design doc's "CRC at the page's start, as today" is
  superseded.)
- **Two CRCs per record.** `record_crc` says "this record is what was
  written"; `image_crc` says "the image beside it is the one it describes".
  An all-zero record is a page that was never written.
- **The batch header records its ingest LSN.** That field is the whole
  point: once the batch's pages are in their home files, on-disk pages are
  the replay of the log below it.
- **The batch header says how long it is.** `header_blocks` sits in the
  first block, and recovery reads the region in *that* length. Taking the
  length from the current configuration instead would put the checksum
  somewhere else after a configuration change across a crash, read a batch
  in flight as never synced, and leave its half-written home pages ahead of
  the manifest.
- **The manifest is canonical.** Scopes, and each scope's segments, are in
  ascending order and the decoder holds them to it: a repeated scope would
  silently replace its twin when state is seeded from the manifest.

## The flush protocol (§6.3)

```text
write batch → pg-dblwrite          sync
write images + records → home      sync          (pages durable)
write manifest (materialized = batch LSN)        (restart point durable)
report the retention holder, if configured        (§4.4 promise 5)
clear pg-dblwrite                  sync
```

- A crash leaves the previous consistent state, the batch half-installed
  with its doublewrite copy intact, or the batch complete. Recovery tells
  these apart by checksum, never by position: a batch whose images do not
  match the checksums its header holds was never synced, so its home copies
  were never begun.
- **Recovery is the same protocol, resumed.** It repairs the batch's home
  copies, writes the manifest, and only then clears the batch. That order
  is what makes a crash *during* recovery harmless: the batch is still
  there, and the next `open` redoes only the unfinished part. Clearing
  first loses the batch.
- **One batch per flush.** Ingest flushes itself when `max_dirty_pages`
  pages are dirty — at a record boundary — so a flush snapshot always fits
  one batch. Splitting a snapshot would put pages on disk ahead of any LSN
  the manifest could honestly record; a snapshot that exceeds the capacity
  is a broken invariant and refuses loudly.
- `flush` reports `Clean` (nothing to do), `Advanced` (no dirty page, but
  records since the last flush touched no page: the restart point moves
  without a write) or `Flushed`.
- **A doublewrite file's first creation is made durable** with
  `sync_namespace` before its batch's sync is relied on (§4.8: a create is
  durable only then). The in-memory fake keeps unsynced creations, so no
  test can show this one missing — only this reasoning stands behind it.
- **A manifest is written only from measured files.** When recovery
  completes a batch, it lists and measures the page files *before* writing
  the manifest: `install` records only the segments of pages it rewrote,
  and a segment whose batch pages all survived the crash must still be
  listed. The "file the manifest does not list" check compares against what
  the durable manifest said, captured before any of this.

## M7 slice 1: what is pull-shaped

The log store's first slice flushed inline and left its flusher task for a
later slice; this makes the same trade. The contract laws are identical
either way and the suites pin them.

| §6 says | This slice does |
| :--- | :--- |
| a subscription per scope, driven from a shared scheduler (§6.2) | catch-up on demand: `get_page` pulls with `read_log` until it reaches `at_lsn`; `catch_up` ingests without a read |
| per-page redo chains, applied at read time (§6.2, §6.4) | pages are applied eagerly into a bounded per-scope cache; clean pages are evicted and re-read from disk |
| a background materializer (§6.3) | `flush`, called by the housekeeper — or by ingest at the dirty bound |

Still to land: the subscription-driven ingest, per-page redo chains with
bounded memory and ingest lag reporting, background tasks through `Rt`,
page versions (§6.5), a per-core handle cache (a handle is made per
device phase today).

**What ingest costs.** Per heap-insert record: the open page is copied out
and validated whole, the target page is copied out, `apply_record`
validates it twice more and appends, the result is copied back, and
`ingested` is updated under the state lock. That is four 8 KiB copies,
three full-page validations and one lock round trip per record — correct
and deliberately simple, not fast. If a profile cares, the open page is the
same page as the target in the common case, and `ingested` could advance
once per chunk.

**What the store needs from the log source.** `read_log` is called once
per 1 MiB chunk, so its cost must not grow with the log. The log store's
`read_log` used to clone the scope's whole state — including its recent-log
cache, unbounded in its slice, so the whole log — on every call, which made
catch-up quadratic; it now hands readers a view (`ReadView`) holding only
the durable LSN, the floor and the index entries that can overlap the range.
The cache itself is still unbounded in the log store (its own deferred
slice), so memory, not time, is what grows.

## Decisions worth knowing

- **`report_retention` is off by default.** On, the store reports its
  materialized LSN as its retention value — only after the manifest naming
  it is durable (pinned at every cut point of a flush). It is off because
  once the page store is a holder the log below its value reads as
  `Truncated`, and runtime recovery still reads the log from 0 to rebuild
  transaction state. M8 turns it on with that consumer in mind.
- **A restarted store reports its holder again.** The log store keeps
  holders in memory and writes them only with its next manifest, so nobody
  can be trusted to remember them on the page store's behalf.
- **`detach` deletes the scope's page files.** Order: release the holder,
  write the manifest without the scope, forget the scope's gate, remove
  the files. A crash between the last two leaves orphans, which `open`
  sweeps; scope ids are never reused (S18), so they are nobody's pages.
- **The redo plan is injected** (`RedoPlan`), and the store is blind to
  what a page means. The contract crate's conformance suite drives cells of
  a test shape (`cascade-storage-api` never depends on
  `cascade-redo`, §4.10); with the plan injected, the *same* store code
  runs that suite. The shipped plan is `HeapInsertRedo`.
- **The fill cursor is derived, never stored.** `HeapInsertRedo` asks the
  store for the segment's highest page and reads that page's header. The
  open page on disk *is* the cursor, which is what makes routing right
  after a restart. It asks `SegmentFill::would_route`, the pure question;
  `route` is for an engine that keeps a cursor (heap design H9). It reads
  the open page through `cascade-page`'s full validation: a damaged
  `free_offset` would otherwise route every later insert to a fresh page.
- **`Slots` are block-granular.** A segment's record count on disk is its
  record file's length in 64-byte records — whole 4 KiB blocks. A page past
  the last record in the final block reads as never written, which is the
  right answer. (The scan for a segment's highest page does not assume the
  count is a whole number of blocks.)

## Lock order

```text
ingest gate (per scope) → flush gate → home gate
```

- The **ingest gate** keeps a record from being applied twice: two cores
  catching one scope up would both route the same record, and the second
  would find an open page already past it.
- The **flush gate** serializes flushes: the doublewrite area holds one
  batch, and a scope's batches must land in order.
- The **home gate** keeps a lazy page read from meeting a flush rewriting a
  record block it shares (64 records per 4 KiB block). Its cost: a page
  that has to come from disk waits for a flush's home phase, syncs
  included. A page in the batch itself is in memory and is served without
  the gate (§6.4's rule). Per-segment gates would narrow this if a profile
  cared.

Ingest takes the flush gate when backpressure makes it flush; scope
administration takes all three; a plain `flush` takes the last two.

**The gates are fair** (`gate.rs`). A release hands the gate to the
longest-waiting waiter and it stays spoken for until that waiter claims it;
a newcomer may take a free gate only when nobody is queued. This is a
liveness requirement, not politeness. A parked waiter runs microseconds
after it is woken, while a thread hot-looping on the gate re-takes it in
well under one — so with "wake everyone and re-contend" a waiter can be
starved for as long as the looping goes on. That is an argument from
scheduling latency, not something observed in the store; what *was*
observed is the fault the first version also had, a lost wakeup after a
release. Cancelling is safe at every point — a queued waiter that is
dropped leaves the queue, and a waiter dropped after being handed the gate
passes it on. See `a_latecomer_cannot_overtake_a_waiter_the_gate_was_handed_to`.

**The state lock is coarse, deliberately, in this slice.** One
`std::sync::Mutex` guards every scope's page map: a cache hit copies the
page out under it, and a flush copies its snapshot (up to
`max_dirty_pages` images) under it. It is the checklist's "coarse mutex
on the hot path" (I11), and it is acceptable only while the page store
sits on the *miss* path: once M8 routes hot reads through the buffer pool,
nothing here is on a hot path. Sharding it per scope or per NUMA is the
work to do if a profile disagrees.

## Tests, and what each pins

| Suite | Pins |
| :--- | :--- |
| `src/gate.rs` unit tests | the fair hand-off: held for its waiter until claimed; latecomers queue behind; FIFO; cancellation before and after the hand-off |
| `src/layout.rs` unit tests | every on-disk byte layout, golden; damage refusal; the header's own length; canonical manifest order; capacity (1009 segments per manifest block, 1363 pages per batch) |
| `src/io.rs` unit tests | the highest-page scan at a record count that is not a whole number of blocks |
| `src/lib.rs` unit tests | §4.5's promises at their edges, including a read below the durable LSN stopping exactly where asked; v2-format records replaying like v3; housekeeping records advancing the restart point with no page write; a chunk past the durable LSN it reports refused; the dirty bound; cache eviction after a flush; sticky ingest and flush failures; `detach` and the orphan sweep; `open`'s refusals; config bounds |
| `tests/conformance.rs` | the contract crate's `page_source_suite`, against the real store over the real log store, with and without retention reporting |
| `tests/differential.rs` | §10: every served page equals an independent replay of the same log, at 12 checkpoints, with a three-page cache; as-of history at every checkpoint; restart resumption; `flush`'s outcomes |
| `tests/crash.rs` | §10: a crash at **every** page-store operation of a flush, before and after it — the first flush and a later one that overwrites existing pages — each followed by another crash; a crash at every second operation *of recovery*; a batch in flight across a changed header size; a 240-seed soak with cuts inside backpressure flushes; a crash in one scope costing another nothing; retention reported only after its manifest is durable, at every cut; S19 end to end |
| `tests/concurrency.rs` | real threads on one store — one log writer, four readers, a flusher in as tight a loop as it can manage — no path fails, panics, hangs, or leaves the store off the replay; and **two scopes under threads** (writer, reader and a shared flusher each), which is where scope-keyed bookkeeping mistakes would show |
| `tests/real_device.rs` | the whole stack on the real glommio `O_DIRECT` device: materialize, verify, reopen through recovery, verify, continue writing — alignment, the record-block read-modify-write, and the file namespace against a real filesystem. Crash cuts stay with the fake (see T5.7's storage note) |

**The reference replay is written in the page format's own terms**
(`tests/common/mod.rs`): each cell is appended with `PageMut::append_cell`,
and a new page is opened whenever the page says it is full. Neither the
store nor `cascade-redo` is asked to vouch for itself, so a defect in
either — a page LSN stamped wrong, free space counted wrong, a cell on the
wrong page — is a difference. (The first version replayed through
`apply_record`, which would have cancelled such a defect out.) Every
verification in the crash suite also checks that the pages past each
segment's highest are still unwritten: garbage on a page the replay never
had is otherwise invisible.

The crash suite's cut points are enumerated, not sampled: it first counts
the operations in one flush, then crashes at each. Defects have been shown
to fail these suites by mutation before being fixed: routing every insert
to page 0, stamping a page with its record's start, reading the batch
header in a length not its own, clearing the doublewrite before the
manifest, and the pre-fix ingest loop.

## For M8 (the buffer pool onto `PageSource`)

- The pool's miss path calls `get_page(scope, key, at_lsn = durable LSN)`.
  A page no record touched is zeros with page LSN 0. A request below the
  current durable LSN is fine: the store ingests to it and stops there.
- `at_lsn` must be a record boundary; one inside a record is
  `NotRecordBoundary`, and nothing of that record is applied.
- A reader whose requests do not move forward — asking for an LSN older
  than one it or another reader already caused to be ingested, for a page
  changed in between — gets `TooOld`. That is the documented answer
  without page versions; a leader's requests move forward with its writes.
- The pool's write-back and the checkpointer go away; the housekeeper calls
  `flush(scope)` (or relies on the dirty bound) and `catch_up` ahead of
  read misses.
- Logs holding `LegacyInsert` (type 4) are refused by this store: their
  pages are not materialized here. Fixtures that still write type 4 need
  rewriting before they run against pages.
