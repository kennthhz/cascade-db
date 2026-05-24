# cascade-governance

Multi-tenant resource governance.

## Status

**Reserved design space — v1 hooks only.** Full implementation lands in Phase 7+ (see [README §6.1](../../../README.md#61-multi-tenant-resource-governance)).

## Role

Layer **12-future**. The enforcement side of §6.1. Reads per-DB counters maintained throughout the rest of the workspace (CPU usage in `cascade-runtime`, page residency in `cascade-bpm`, I/O bandwidth in `cascade-storage`, connection counts in `cascade-pgwire`) and applies `RESOURCE GROUP` quotas.

The v1 deliverable is the API surface — `ResourceGroup`, the SQL DDL hooks, the accounting *interfaces* — plus no-op enforcement. The runtime / BPM / storage / pgwire crates do the real bookkeeping; this crate will eventually do the throttling.

## See also

- [AI/CONTEXT.md](AI/CONTEXT.md)
- README §6.1.
