# WP2 — Txn manager: the HLC, commit/abort state machine, status table, closed ts

Status: ACCEPTED (decisions D1–D5; implementation T2.1–T2.6) · 2026-11-21
Upstream: docs/storage-subsystem-hld-v2.md §4 (scope/epoch), §5 (WAL watermarks — landed), §6 (clock, txn ids, commit/abort, status table), §7 (closed ts, heartbeats; the snapshot/isolation half is WP9's). Upstream §18 scopes WP2 as: LSN reservation and fill; durable LSN tracking; HLC; commit/abort state machine; status table pages; closed ts and heartbeats — the first two items are already real (cascade-wal writer, FR-25/26/27), this doc covers the rest.
Consumers: WP5 table access API (D3 status-table wait, D4 one visibility rule), the MVCC milestone's visibility layer, WP9 snapshots, recovery.

## Responsibility

Give every txn an identity, give every commit a timestamp ordered with its commit-record LSN, keep the per-scope txn status table as pure derived state rebuilt from the WAL, and publish the scope's closed ts — the read gate every future snapshot waits on.

## Key decisions

### D1 — The commit section (confirms §6's proposal)

§6 proposes assigning commit_ts "inside the same critical section as the commit-record reservation"; confirmed. Two reactors sharing one writer can otherwise interleave ts assignment and reservation and mint a commit_ts that decreases where the commit-record LSN increases — breaking §6's invariant that within a scope commit_ts increases with commit-record LSN.

Mechanism: a per-DB **commit-section mutex** in the runtime's txn manager, guarding exactly two steps, never held across an await:

```text
lock → ts = clock.issue() → commit = writer.append_wal(&CommitTxn { xid, commit_ts: ts }) → unlock
```

Fill and flush stay outside the section (watermark machinery is already race-safe). The section is contended only by concurrent committers on one DB; it holds no lock the WAL writer itself takes, so no ordering hazard with the pending/leader locks. Cost accepted: one uncontended-≈ mutex hop per commit; the group-commit window dwarfs it.

### D2 — The status table is derived state, built only from the WAL

Per-scope in-memory map `Xid → TxnEntry`, entries:

```text
InProgress                      — inserted when the txn's Xid is assigned (first write)
Committed { commit_ts: Hlc }    — set only after the durable LSN covers the commit record
Aborted                         — set only after the durable LSN covers the abort record
```

The law (principle 2: the WAL is the only durable write): **nothing in the table is ever persisted, and every entry's existence and outcome is reconstructible from WAL records alone.** Live transitions obey the same order replay does:

- `InProgress` at Xid assignment (before the first record of the txn is reserved — replay sees the BEGIN/first insert at the same point),
- `Committed`/`Aborted` in the ack path, after `flush_wal` covers the outcome record and before the client ack — §6's commit protocol steps 3–4 verbatim.

`Committed` carries the commit_ts the COMMIT record named, so a reader resolves a `VersionWord::uncommitted(xid)` word with one lookup: `InProgress` → invisible to others, `Committed{ts}` → visible iff `ts ≤ read_ts` (the comparison itself is WP9/D4's), `Aborted` → invisible, prune/vacuum food. In-place version-word stamping (rewrite txn_id → commit_ts in the cell) is **rejected for this era**: the current heap slice never rewrites cells (T6.4/T1.3 contract), and §6 lists stamping timing as an open question — the table lookup is the stamping-free answer, revisited only if replay-era read amplification ever justifies it.

Structure: `Mutex<BTreeMap<Xid, TxnEntry>>` — ordered so the future cleanup-watermark scan (retire entries below the oldest live read_ts) is a prefix drain; short critical sections, never across an await. Retention itself is deferred to the vacuum era: until a watermark exists, entries live for the process lifetime (bounded by txn count; fine for the single-node slices).

### D3 — The HLC: boot seeding, high-water records, and the reissue rule

`HlcClock` lands in **cascade-runtime's `txnmgr` module** — cascade-types stays data-only (its hard invariant 1: no logic, no failure), so the stateful clock cannot live beside the packed `Hlc` it issues; the encoding, its guards (`Hlc::from_parts` panicking past `PHYSICAL_MAX_MS`, the same constructor-guard class as `VersionWord::committed`), and accessors (`physical_ms`/`logical`) are cascade-types'. The clock: `issue() → Hlc` with `ts = max(prev_issued + 1, pack(now_ms_since_epoch))` — logical +1 on every call, physical part snaps forward when the wall moves (logical carry into the next ms is the HLD rule, not an error); `observe(ts)` folds a foreign timestamp in (remote-token era, WP9), refusing overflow as a `Result`; the epoch-contract overflow itself (wall at year ~6488) panics — a clock that cannot issue without corrupting version words must stop, not lie.

**Boot seeding:** the clock starts at `prev_issued = max(replayed max COMMIT commit_ts, replayed max HLC_HIGH_WATER max_issued, wall-derived floor)`. Recovery already decodes every COMMIT record in order (T3.3 prepare walk); the plan gains the running max.

**High-water emission** (the record persists *issued* ts, covering ts that outlive their txn): graceful shutdown always writes `HlcHighWater { max_issued }`; the housekeeping tick writes one only when `max_issued` advanced past the last recorded value (a quiet system writes nothing). With no checkpoint, replay's max-over-records is the seed — high-water records are the only records that can raise it beyond committed history.

**The reissue rule (why crash-losable ts are safe):** a ts issued but never committed may be lost in a crash (no record carried it). Reissuing it is sound: it never appeared in a COMMIT record, so no committed word, status entry, or read rule ever observed it — uniqueness of *committed* commit_ts is what §6's invariant protects. The high-water record exists to shrink that lossy window to ≤ one housekeeping tick, not to close it.

### D4 — Closed ts: advance points and the heartbeat gate

Per-scope `closed_ts` (atomic packed Hlc). Advance points, in one place (the ack path):

1. **Commit:** when the durable LSN covers a COMMIT record, closed_ts = max(closed_ts, its commit_ts) — at the same moment the status table flips to `Committed`. "Never passes a commit that is not yet durable" (§7) holds by construction: the advance uses the ts of a just-confirmed-durable record.
2. **Heartbeat:** an idle scope must still advance closed ts (§7 — readers at fresh read_ts must not block forever behind a silent writer). Emitted from the housekeeping tick as `ClosedTsHeartbeat { closed_ts }`, **only when the writer is fully drained (`reserved == durable`)**: if any reservation is in flight, its commit_ts may already be assigned (D1) and sit below the wall clock — advancing closed_ts past it would promise more than durability backs. A drained writer has no assigned-but-undurable ts, so `closed_ts = clock.peek()` (the highest *issued* ts — every future `issue()` returns strictly more) is sound: with `reserved == durable`, every issued ts's commit record is already durable, so the promise is durability-backed by construction. The record itself then goes through the normal reserve/fill/flush path (its own durability is what makes the promise survive a restart, and replay raises closed_ts from heartbeat records too).

`wait_closed(read_ts)` resolves when `closed_ts ≥ read_ts` — the same register-then-recheck waker pattern the WAL writer uses. In the current single-scope slices every read trivially passes (the owner's own commits advanced it); the gate is landed now so WP9's cross-scope rule and the replicas reuse one mechanism.

### D5 — The abort path

The runtime emits `AbortTxn { xid }` when a session rolls back a txn that wrote (Xid assigned): explicit ROLLBACK, and error-path teardown of an `InTxn` session. A read-only rollback emits nothing — no Xid was ever assigned, so no status entry exists to resolve (§6: read-only txns get no txn_id). The abort record takes the normal reserve/fill/flush path; the status table flips to `Aborted` when durability covers it (D2's rule covers aborts by the same argument §6 makes: heap cleanup is the status table's invisibility plus later pruning — no undo work).

## Invariants

1. Within a scope, commit_ts is unique and increases with commit-record LSN (D1's section; §6).
2. A txn is committed iff its commit record is in the durable WAL; no client ack before that (existing FR-27 discipline + D2's transition point).
3. The status table contains nothing that the WAL does not imply; a fresh boot rebuilds it exactly (D2).
4. `clock.issue()` is strictly monotonic across restarts, modulo D3's reissue rule (committed ts never reused).
5. closed_ts never exceeds what durability backs: commit advances use just-durable commit_ts; heartbeats fire only on a drained writer (D4).
6. The status table lock and the commit-section lock are never held across an await, and never nested (the commit section touches the clock and the writer only; status transitions happen later, in the ack path).

## Interfaces

Lands in `cascade-types` (T2.1): `Hlc::{from_parts (panicking guard), physical_ms, logical, LOGICAL_BITS, PHYSICAL_MAX_MS}` — the F1 encoding, data-only. Lands in `cascade-runtime::txnmgr` (T2.1): `HlcClock::{new, new_seeded(floor), with_source, issue, peek, observe}` — `issue() -> Hlc` is strictly monotonic and panics on epoch-contract overflow; `observe(foreign_ts) -> Result<(), HlcOverflow>` takes remote input and refuses loudly; `peek()` returns the highest issued ts without advancing; `with_source` injects the physical-ms source (tests, and future replica clocks). Lands in `cascade-runtime` `txnmgr` module (T2.2): `TxnStatusTable::{begin(xid), commit_durable(xid, ts), abort_durable(xid), status(xid)}`, `TxnEntry`, built into the session env next to the `Wal` wiring; recovery's `RecoveryPlan` gains the replay-derived table (T2.2) and the clock seed `max_issued_ts` (T2.3). Commit path (T2.3): `durable_commit` takes the commit section (D1), stamps a real `HlcClock::issue()` ts, the `NEXT_COMMIT_TS` stand-in dies (the wp1 doc's "until WP2's HLC" resolved); shutdown/housekeeper emit `HlcHighWater`/`ClosedTsHeartbeat` per D3/D4. `WalRecord::{HlcHighWater, ClosedTsHeartbeat}` payloads exist since T1.2 (F3 pinned `[closed_ts u64]` / `[max_issued u64]`) — T2.3/T2.4 add only emission and replay interpretation, no format change.

## Open questions

| Question | From | Needed before |
| :---- | :---- | :---- |
| Status-table retention below the cleanup watermark (§7's GC watermark) | D2 | vacuum era — the BTreeMap prefix drain is reserved for it |
| In-place version-word stamping (rewrite txn_id → commit_ts) | §6 | only if table-lookup read amplification is measured; never before MVCC visibility exists |
| Per-table scopes: one status table per scope, one clock per scope | §4 | catalog / sharding era (today: one DB = one scope) |
| Heartbeat interval + high-water tick tuning | D3/D4 | bare-metal gate (T5.7 discipline); defaults chosen conservatively |
| SSI read-write dependency tracking hooks | §7 | WP9 |

## Provenance

Decided 2026-11-21 from HLD v2 §6/§7 read against the landed writer (FR-25/26/27), the T1.2 F3 record table (types 6/7 pre-reserved), and the runtime's T1.2-era stand-ins (`NEXT_XID`, `NEXT_COMMIT_TS` — the latter's own comment names its replacement). D1 confirms §6's "proposed" commit_ts/reservation critical section, as that section invites. The wp1 doc's cross-reference to "T2.3" for the HLC is repointed to T2.1 (the clock task) in the same change; the session.rs stand-in comment's "replaces it at T2.3" becomes true as written (T2.3 is the commit-path migration). The clock's home is cascade-runtime::txnmgr, not cascade-types: a first draft put it beside the packed `Hlc`, but cascade-types' hard invariant 1 (data only — no logic, no failure) rules the clock out; `04-txn`'s existing crates (cascade-mvcc/undo) are pre-pivot stubs awaiting WP6/WP9 redesign, so the module joins its consumers (durable_commit, recovery, the T2.2 status table) in the runtime.
