# cascade-pg-parser-sys — AI Design Context

## Role

The *only* FFI crate in the workspace. Raw bindings to `libpg_query` (the PostgreSQL parser exposed as a C library).

## Pillar Reservations

- **§3.6 (Rust foundation):** documented exception. This crate is `unsafe`-heavy by design. The safe API lives in `cascade-pg-parser`.

## Hard Invariants

1. **One safe entry point; `unsafe` contained.** The public [`parse`] is
   safe Rust — it copies the tree out of C memory and frees the C side
   before returning. The `unsafe` lives in that one block plus `mod raw`
   (the `#[repr(C)]` header mirrors). The safe wrapper one crate up sees
   no pointers at all.
2. **No C memory escapes.** Nothing C-allocated outlives the call; the
   protobuf types (`src/pb.rs`) are owned Rust.
3. **`mod raw` mirrors `pg_query.h` exactly.** Any re-vendoring diffs the
   header against `mod raw` (see VENDORED.md step 4).
4. **Vendor libpg_query at a known SHA.** Sources are in-tree under
   `libpg_query/` (taken from the pinned `pg_query` 6.2.1 package —
   sha256 in VENDORED.md). Re-vendoring is a deliberate, recorded action.

## Out of Scope

- Anything Rust-idiomatic. `cascade-pg-parser` exists for that.
- Parsing logic. We're just calling C.

## Resolved Design Questions

- Vendor strategy: **vendored in-tree** (`libpg_query/`), because the
  pg_query crate's bindgen path needs libclang on every build machine;
  in-tree C needs only a C compiler. Provenance: VENDORED.md.
- bindgen vs hand-written: **hand-written** — the used C API is one
  function pair; `mod raw` mirrors the header.
- Tree exposure: **protobuf** (`src/pb.rs`, checked-in prost generation of
  `pg_query.proto`) — faster than JSON and pure Rust.
