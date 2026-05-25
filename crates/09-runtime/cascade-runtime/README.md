# cascade-runtime

Thread-per-core async scheduler over Glommio.

## Role

Layer **09-runtime**. Implements pillar §3.2 — pins one worker thread per CPU core, drives a private `io_uring` ring per worker, and routes accepted connections to the least-loaded reactor.

## Full design

**See [`docs/runtime-architecture.md`](../../../docs/runtime-architecture.md) for the complete spec** — hardware partitioning, the Glommio reactor model, the Tier A/B/C state model, connection routing, cooperative yielding contract, cross-core API, shared-substrate primitives, and cold-start/shutdown choreography.

## Public API (initial)

- `Runtime::start(config)` — spawns N workers (one per pinned core), housekeeper on core 0.
- `submit_to(core, async fn) -> T` — cross-reactor coordinator primitive for Tier C writes and background work. Never used on the OLTP hot path.
- `current_core_id()` / `current_reactor()` — context queries for the active reactor.
- `yield_if_needed().await` — cooperative yield, no-op unless the CPU deadline has been exceeded.
- Connection accept + dispatch (housekeeper-side) and the per-reactor handoff.

## Dependencies

- `cascade-tenant` — context propagation.
- `cascade-telemetry` — `Span`, reactor-stall metrics.
- `glommio` (TBD version) — runtime + `io_uring`.
- `libc` — `sched_setaffinity`, NUMA syscalls.

## See also

- [AI/CONTEXT.md](AI/CONTEXT.md) — design context, invariants, open questions.
- [`docs/runtime-architecture.md`](../../../docs/runtime-architecture.md) — authoritative design spec.
- Root [README §3.2](../../../README.md) — pillar.
