# cascade-server

The Cascade DB server binary.

## Role

Top-level binary. Boots the routed runtime (`cascade-runtime::Runtime::boot_routed`: housekeeper accept/auth/route + one worker reactor per configured core), serves the simple-query protocol until SIGTERM/SIGINT, then prints one JSON observables line — reactor stall totals and the WAL writer's watermarks and fdatasync barrier count — for the bench harness to collect. Does not contain database logic itself — only composition.

## Layout

- `src/main.rs` — entry point: parse core/port/`--wal-dir` args, boot, wait on a shutdown flag, print observables.
- `tests/smoke.rs` — end-to-end: boots the real binary, drives INSERT/txn/SELECT through `bench::Client`, asserts durable commits and clean shutdown observables.

## See also

- [AI/CONTEXT.md](AI/CONTEXT.md)
- [../bench](../bench/) — the architecture A/B this binary serves in
- Workspace [README](../../README.md)
