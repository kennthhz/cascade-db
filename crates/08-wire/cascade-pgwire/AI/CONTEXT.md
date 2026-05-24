# cascade-pgwire — AI Design Context

## Role

PG wire protocol terminator. Where bytes-on-a-socket become query intent.

## Pillar Reservations

- **§4.1:** the entire reason this crate exists.
- **§6.1 (Tenant):** every connection carries a `DatabaseId` (and future `TenantId`) from the authentication handshake forward.
- **§6.2 (Observability):** every query starts a parent span.

## Hard Invariants

1. **Byte-exact wire compatibility.** Test against `libpq` and `psycopg` golden vectors.
2. **Extended-query state machine correctness.** Parse / Bind / Describe / Execute lifecycle. ORMs depend on subtle behavior here.
3. **Errors emitted as PG `ErrorResponse`** with correct SQLSTATE.
4. **Connection context = `DatabaseId` from handshake.** Propagated through the request lifecycle (§6.1 reservation).

## Out of Scope

- Authentication backends beyond the standard set (LDAP, GSSAPI, custom) — Phase 5+.
- Replication-protocol messages (Phase 7 / §6.3).

## Open Design Questions

- Reuse existing Rust pgwire crate (e.g., `pgwire`, `convergence`) vs hand-roll. Reuse saves work but may not fit the thread-per-core model — needs investigation.
- TLS termination: `rustls` (pure Rust) — clear choice over OpenSSL FFI.
