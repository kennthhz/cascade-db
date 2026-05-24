# cascade-error

Unified error type and `Result` alias for Cascade DB.

## Role

Layer **01-foundation**. Provides the top-level `Error` enum that crates compose their layer-specific errors into, plus a workspace `Result<T>` alias.

## Design

Each layer (`cascade-storage`, `cascade-bpm`, `cascade-wal`, …) defines its **own** error type local to that crate. Those local errors implement `Into<cascade_error::Error>` for the boundary case where they bubble up to a caller that doesn't care about the specific subsystem.

This avoids the "one giant enum imports the whole world" antipattern while still giving the wire protocol and binary a single error type to convert into a PG error message.

## What lives here

- `Error` — top-level enum.
- `Result<T>` — alias for `std::result::Result<T, Error>`.
- (Future) PG SQLSTATE mapping for §4.1 error reporting.

## Dependencies

- `thiserror` (only).

## See also

- [AI/CONTEXT.md](AI/CONTEXT.md)
