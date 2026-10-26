# cascade-server — AI Design Context

## Role

The binary. Composition only — no business logic. Everything reusable lives in library crates.

## Hard Invariants

1. **No domain logic here.** If a function does anything other than parse args, wire components, or handle signals, it belongs in a library crate.
2. **Single binary.** Cascade DB ships one server process. Admin tools live in `tools/`.
3. **Observable shutdown.** SIGTERM/SIGINT flip one async-signal-safe flag; the main thread prints the FR-26/FR-27 observables JSON (stall totals, WAL watermarks + sync count) before exit. Connection draining and WAL quiesce arrive with the catalog/buffer-pool phases.
4. **Bench parity.** `tests/smoke.rs` and `bin/bench/tests/parity.rs` pin this binary's wire behavior and WAL shapes against the pgmodel twin — changes here must keep both green.

## Current shape (M1)

Args: `--port`, `--hk-cores`, `--worker-cores`, `--wal-dir`, `--batch-rows`. Placement is explicit operator cores; the /sys topology provider and TOML config land in later phases.

## Open Design Questions

- Config format: TOML, YAML, env, all of the above. `figment` or `config` crate.
- Logging output for the binary: structured JSON to stdout (`cascade-telemetry` defaults).
