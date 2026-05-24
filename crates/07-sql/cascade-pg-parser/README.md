# cascade-pg-parser

Safe Rust wrapper over `cascade-pg-parser-sys` (which wraps `libpg_query`).

## Role

Layer **07-sql**. The only crate that downstream code (planner, executor, wire protocol) is allowed to depend on for SQL parsing. The `unsafe` FFI is contained in this crate's `*-sys` peer.

## Public API (initial)

- `parse(sql: &str) -> Result<ParseTree>` — the one function callers need.
- `ParseTree` — a Rust-idiomatic AST. Owned, dropable, `Send`.
- `ParseError` — surfaced via `cascade-error`.

## Why this split

`*-sys` crates are a Rust ecosystem convention: raw bindings live in `foo-sys`, the safe API lives in `foo`. The split lets `cargo audit`, IDE indexing, and code reviewers immediately spot where FFI / `unsafe` lives.

## See also

- [AI/CONTEXT.md](AI/CONTEXT.md)
- [ADR 0002](../../../AI/decisions/0002-libpg-query-ffi-exception.md)
- README §3.6, §4.3.
