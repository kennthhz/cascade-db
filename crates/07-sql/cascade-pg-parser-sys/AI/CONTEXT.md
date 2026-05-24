# cascade-pg-parser-sys — AI Design Context

## Role

The *only* FFI crate in the workspace. Raw bindings to `libpg_query` (the PostgreSQL parser exposed as a C library).

## Pillar Reservations

- **§3.6 (Rust foundation):** documented exception. This crate is `unsafe`-heavy by design. The safe API lives in `cascade-pg-parser`.

## Hard Invariants

1. **No safe Rust API here.** Everything `pub` is `unsafe extern "C"`. The safe wrapper lives in the next crate up.
2. **No Rust drop types cross the boundary.** Memory allocated by `libpg_query` is freed by `libpg_query`. Rust just holds raw pointers.
3. **Memory ownership documented per function.** Each binding's doc-comment states whether the caller or callee owns returned pointers.
4. **Vendor libpg_query at a known SHA.** Reproducible builds. Re-vendoring on new PG releases is a deliberate, recorded action.

## Out of Scope

- Anything Rust-idiomatic. `cascade-pg-parser` exists for that.
- Parsing logic. We're just calling C.

## Open Design Questions

- Vendor strategy: git submodule, vendored sources in-tree, or fetch-at-build. Vendored-in-tree is most reproducible; submodule is easier to update.
- bindgen vs hand-written bindings. bindgen is less work but pulls in libclang. Hand-written is small enough to maintain (`libpg_query` has a stable, small C API).
- Whether to expose protobuf-decoded parse trees (libpg_query returns protobuf) or plain JSON. Protobuf is faster.
