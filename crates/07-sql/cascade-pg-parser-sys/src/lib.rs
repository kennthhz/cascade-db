//! Raw FFI bindings to libpg_query.
//!
//! See `README.md` and [ADR 0002](../../../AI/decisions/0002-libpg-query-ffi-exception.md).
//!
//! Every public item here is `unsafe`. The safe wrapper lives in `cascade-pg-parser`.

#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

// Bindings will be generated / hand-written in Phase 4. Placeholder declarations
// below show the *shape* of what this crate exports.

/// Opaque parse-tree pointer returned by libpg_query.
pub type PgQueryParseResult = *mut std::ffi::c_void;

extern "C" {
    // Placeholders — actual signatures land in Phase 4.
    // pub fn pg_query_parse(input: *const std::os::raw::c_char) -> PgQueryParseResult;
    // pub fn pg_query_free_parse_result(result: PgQueryParseResult);
}
