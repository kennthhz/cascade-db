# cascade-cdc — AI Design Context

## Status

**v1 = trait surface + no-op.** Implementation in Phase 7+.

## Role

Logical decoder for §6.3. Reads WAL via `WalReader`, emits `ChangeEvent`s.

## Pillar Reservations

- **§6.3:** the entire reason this crate exists.
- **§3.4 (Per-DB WAL):** structural advantage — one decoder per database, no cross-DB interleaving.

## Hard Invariants

1. **Decoder is a consumer of `WalReader`** — never reads raw WAL bytes itself.
2. **DDL is decoded** so the consumer can interpret row formats across schema changes.
3. **Event order = commit order** within a database.
4. **No back-pressure into the writer.** A slow CDC consumer must not stall the OLTP path.

## v1 Hooks That Must Exist

- `WalRecord` variants carry enough state for decoding (full row images, DDL records).
- `WalReader` trait in `cascade-wal`.
- Per-database streaming endpoint architecture sketched (gRPC suggested).

## Open Design Questions

- Output format: protobuf `ChangeEvent` is in the README sketch. Worth confirming before Phase 7.
- Catalog snapshots — the decoder needs schema as of each WAL position. Approach: replay DDL records to maintain a shadow catalog.
- Resume semantics: replication slots à la PG. Need to confirm slot management before slot LSN format ossifies.
