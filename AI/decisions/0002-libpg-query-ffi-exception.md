# ADR 0002 — Reuse `libpg_query` for SQL parsing (FFI exception to §3.6)

**Status:** Accepted
**Date:** 2026-05-24

## Context

§3.6 commits to a memory-safe Rust foundation. The PG SQL parser is ~10k lines of Bison grammar with decades of accumulated dialect quirks. A native rewrite would burn person-years and still drift from PG behavior on edge cases that real ORMs hit. `libpg_query` is the upstream PG parser exposed as a stateless C library, PostgreSQL-licensed, and already in production at pganalyze, Squawk, etc.

## Decision

Reuse `libpg_query` via FFI for the SQL parser. Every other component on the critical path (planner, executor, type system, catalog, wire protocol, storage, runtime) remains pure Rust.

The FFI surface is isolated in a pair of crates:

- `cascade-pg-parser-sys` — raw bindings, `unsafe`, narrow API surface.
- `cascade-pg-parser` — safe Rust wrapper, owns memory-context lifetimes, exposes a Rust-idiomatic parse-tree API.

No other crate may FFI into the parser. No other crate may FFI into PG C code at all without a follow-up workspace ADR.

## Consequences

- §3.6 is updated to acknowledge the exception: *"Rust where we own the critical path; vetted dependencies where reuse beats reinvention."*
- Phase 4 ships months sooner with byte-exact PG dialect fidelity.
- CI gains a C toolchain dependency.
- Re-vendoring required on new PG releases (PG 18, 19, …).
- Memory-safety story includes a clearly bounded FFI surface that auditors can review independently.

## Alternatives considered

- **`sqlparser-rs`.** Pure Rust, but PG dialect coverage lags PG and will silently fail on ORM queries. Long-term compatibility risk outweighs the purity gain.
- **Hand-rolled parser.** Many months of work; will still drift from PG.
- **Embed full Postgres binary.** Defeats the entire premise of Cascade DB.

## Amendment (2026-10-26) — binding implementation: vendored C, hand-written bindings

The two-crate isolation (`cascade-pg-parser-sys` + `cascade-pg-parser`) is
unchanged and in force. The *binding mechanism* resolved differently from
the original sketch (bindgen over in-tree sources): bindgen requires
libclang on every build machine, which makes every fresh clone of the
workspace depend on a heavy toolchain for one C header. Instead:

- The libpg_query C sources (17-latest, from the pinned `pg_query` 6.2.1
  crates.io package, sha256 recorded) are vendored in-tree under
  `crates/07-sql/cascade-pg-parser-sys/libpg_query/`.
- The C API surface used is one parse entry point + its free function;
  the bindings are hand-written `#[repr(C)]` mirrors of `pg_query.h` —
  as the "alternatives considered" already anticipated ("hand-written is
  small enough to maintain").
- The protobuf tree types are checked-in prost-generated code
  (`src/pb.rs`), regenerated on re-vendoring.
- Provenance and the re-vendoring procedure: `VENDORED.md` next to the
  sources. Re-vendoring remains a deliberate, reviewed action (invariant 4).

Consequence update: CI needs a C compiler (gcc/clang), *not* libclang or
protobuf tooling.

Ruling re-confirmed 2026-10-25 (perf-demo fairness work): cascade parses
with PG's real grammar so the frontend parse cost is identical to
PostgreSQL's in comparisons; parse-only ≠ planner, stated rather than
implied wherever the claim is made.
