# cascade-runtime

Thread-per-core async scheduler.

## Role

Layer **09-runtime**. Implements pillar §3.2 — pins lightweight worker threads to physical CPU cores, drives `io_uring` reactors, and routes accepted connections to a core.

## Public API (initial)

- `Runtime::start(config)` — spawns N workers (one per pinned core).
- `Worker` — the per-core executor; owns its `io_uring` ring, BPM shard, storage handle.
- Connection accept + sharding to a worker.

## Dependencies

- `cascade-tenant` — context propagation.
- `tokio-uring`, `libc`.

## See also

- [AI/CONTEXT.md](AI/CONTEXT.md)
- README §3.2.
