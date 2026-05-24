# cascade-cdc

Logical decoder + Change Data Capture stream.

## Status

**Reserved design space — v1 hooks only.** Full implementation lands in Phase 7+ (see [README §6.3](../../../README.md#63-logical-replication--change-data-capture-cdc)).

## Role

Layer **12-future**. Consumes WAL records from [`cascade-wal`](../../02-storage/cascade-wal/) and emits logical events (`INSERT` / `UPDATE` / `DELETE` with full row images, DDL changes) for downstream consumers — read replicas, Debezium-style sinks, audit pipelines.

The v1 deliverable from this crate is the **trait surface** plus a no-op implementation. The actual decoder and the gRPC stream land later. What makes that possible is the WAL record format reservation enforced by `cascade-wal`.

## See also

- [AI/CONTEXT.md](AI/CONTEXT.md)
- README §6.3.
