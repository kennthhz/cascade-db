# cascade-btree — AI Design Context

## Role

B+Tree access method — the IOT primary tree (HLD v2 §11: newest row version in the leaf, MVCC via undo) and secondary index trees (§13). Detailed design pending under `AI/`.

## Pillar Reservations

- **Storage foundations (crates/02-storage/AI/storage-layer-design.md §12) are the source of truth** for this crate: one writer per scope (§12.1 principle 1, §12.2), the redo contract (§12.4), page cache and cross-core reader access (§12.5); leaf rows and index entries are the per-component designs (docs/components/, pending).

## Hard Invariants

1. **Single mutator:** all mutations to one tree are serialized on the core that owns its scope (HLD v2 §2 principle 1).
2. **No node visible to readers until fully constructed** — SMOs publish through WAL redo that is self-contained, deterministic, and idempotent by LSN (§9).
3. **Page formats live under the cascade-page layer contract** (prologue + `page_kind` registry): the layer owns the prologue and the rules; this crate owns the btree kinds' bytes and their redo apply functions.

## Out of Scope

- Other index kinds (GIN, GiST, BRIN, hash) — separate crates / variants in later phases.
- Locking above the index (row-level locks live in the txn layer).
- Bulk-load path — initial implementation is point insert; bulk load is a Phase 3 follow-up.

## Open Design Questions

- Key-prefix compression at the leaf level — meaningful win for text PKs, complicates split logic.
- Suffix truncation in internal pages (Graefe key normalization).
- Whether to ship a hash variant as a sibling crate or as a tagged enum within this one.
