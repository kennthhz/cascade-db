# Vendored C: libpg_query (PostgreSQL 17 parser)

## What is here

`libpg_query/` holds the C sources of [libpg_query](https://github.com/pganalyze/libpg_query)
(17-latest — PostgreSQL 17's parser extracted as a stateless C library,
PostgreSQL licence), plus vendored `protobuf-c` and `xxhash` under
`libpg_query/vendor/`. `build.rs` compiles them with the `cc` crate;
`src/pb.rs` is the prost-generated Rust binding of
`libpg_query/protobuf/pg_query.proto`; the hand-written `#[repr(C)]`
bindings live in `src/lib.rs` (`mod raw`).

## Provenance

- Taken from the `pg_query` **6.2.1** crates.io package (pganalyze), which
  vendors libpg_query at its pinned upstream commit.
- Package sha256:
  `da43168013d137b3ca49b0044c1108ec09ff596a0626d4db89209c0a1d450cd2`
- Vendored here: 2026-10-26 (perf-demo work; ADR 0002).

## Why vendored in-tree

The `pg_query` crate generates its Rust bindings with bindgen at build
time, which requires libclang on every machine that builds the workspace.
ADR 0002's original plan — vendored-in-tree C + hand-written bindings for
the (small, stable) C API — needs only a C compiler, keeps the workspace
self-contained, and satisfies the ADR's invariant directly: sources at a
known, recorded hash, upgraded only by a deliberate re-vendoring.

## Re-vendoring (deliberate, reviewed action)

1. Pick the new `pg_query` crate version (it tracks libpg_query releases;
   each major tracks one PostgreSQL major).
2. Verify its package hash from crates.io, extract `libpg_query/` into
   this directory (keep `pg_query.h`, `postgres_deparse.h`, `src/`,
   `protobuf/`, `vendor/`; the `test/` tree is not needed to build).
3. Refresh `src/pb.rs` from the package's `src/protobuf.rs` (same schema,
   matching prost version) and update its provenance header.
4. Diff `pg_query.h` against `src/lib.rs`'s `mod raw` — mirror any change.
5. Run the full workspace test suite; update this file and the ADR.
