# cascade-error — AI Design Context

## Role

Foundation crate. Top-level `Error` and `Result` for the workspace.

## Pillar Reservations

- **§4.1 (Wire protocol):** errors must be convertible to PG `ErrorResponse` messages with correct SQLSTATE codes. Reserve a `pub fn sqlstate(&self) -> &'static str` method on the top-level error.

## Hard Invariants

1. **Per-crate local errors first.** A crate must not stuff its variants into `cascade-error` directly. It defines its own error and provides `From` / `Into` at the boundary.
2. **No `panic!` for recoverable conditions.** All recoverable failures flow through this `Result`.
3. **No swallowing.** Every variant must preserve the source error chain.

## Out of Scope

- Subsystem-specific errors (live in their own crates).
- Diagnostic *formatting* for end users — handled at the wire-protocol boundary, which knows the locale and verbosity context.

## Open Design Questions

- Do we want `miette`-style rich diagnostics for human-facing CLI errors? Probably yes for the `cascade-server` binary, but not pulled into every library.
- SQLSTATE coverage: which subset of PG SQLSTATE codes do we commit to in v1?
