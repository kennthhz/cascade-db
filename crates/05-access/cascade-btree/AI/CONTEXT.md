# cascade-btree — AI Design Context

## Role

B+Tree with Optimistic Lock Coupling.

## Pillar Reservations

- **§3.2 (Thread-per-core):** OLC is the concurrency primitive that makes the index scale under shared-nothing async.
- **§3.6 (Rust):** the borrow checker is doing real work here — version-validated reads, latch handoff during splits — these are *very* easy to get wrong in C.

## Hard Invariants

1. **Readers never block.** OLC version validation, retry on conflict.
2. **Writers latch one page at a time** during traversal; coupling rules per Graefe (2010).
3. **No node visible to readers until fully constructed** — splits publish the new page atomically.
4. **Page format owned by `cascade-page`** — this crate doesn't redefine layout.

## Out of Scope

- Other index kinds (GIN, GiST, BRIN, hash) — separate crates / variants in later phases.
- Locking above the index (row-level locks live in the txn layer).
- Bulk-load path — initial implementation is point insert; bulk load is a Phase 3 follow-up.

## Open Design Questions

- Key-prefix compression at the leaf level — meaningful win for text PKs, complicates split logic.
- Suffix truncation in internal pages (Graefe key normalization).
- Whether to ship a hash variant as a sibling crate or as a tagged enum within this one.
