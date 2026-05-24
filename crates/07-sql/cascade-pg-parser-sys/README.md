# cascade-pg-parser-sys

Raw, `unsafe` FFI bindings to [`libpg_query`](https://github.com/pganalyze/libpg_query).

## Role

Layer **07-sql**. The single FFI boundary in the entire Cascade DB workspace. See [ADR 0002](../../../AI/decisions/0002-libpg-query-ffi-exception.md) for the why.

**Do not depend on this crate directly.** Higher layers depend on [`cascade-pg-parser`](../cascade-pg-parser/), which is the safe Rust wrapper.

## What lives here

- `extern "C"` declarations for `libpg_query` entry points (`pg_query_parse`, `pg_query_free_parse_result`, …).
- `build.rs` that vendors / builds the C source.
- Nothing else. No higher-level types, no Rust API. Every `pub` symbol is FFI.

## Invariants

1. Every public function is `unsafe`.
2. No Rust type with a destructor crosses this boundary except via raw pointers.
3. Memory ownership rules of `libpg_query` (palloc / `MemoryContext`) are documented at every call site.

## See also

- [AI/CONTEXT.md](AI/CONTEXT.md)
- [ADR 0002](../../../AI/decisions/0002-libpg-query-ffi-exception.md)
- README §3.6.
