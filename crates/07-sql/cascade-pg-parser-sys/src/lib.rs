//! FFI boundary to libpg_query — PostgreSQL's real parser as a C library
//! (ADR 0002, the documented §3.6 exception).
//!
//! ## Vendoring and bindings
//!
//! The C sources live in-tree under [`libpg_query`](./libpg_query/) —
//! PostgreSQL 17's parser as extracted by pganalyze's libpg_query
//! 17-latest, taken from the pinned `pg_query` 6.2.1 crates.io package
//! (provenance and re-vendoring steps: `VENDORED.md`). `build.rs` compiles
//! them with the `cc` crate; a fresh clone needs only a C compiler.
//!
//! The C API surface this crate touches is small and stable — one parse
//! entry point and its free function — so the bindings below are
//! hand-written `#[repr(C)]` mirrors of `libpg_query/pg_query.h` (the
//! original ADR's plan). `links = "pg_query"` enforces the single-copy
//! rule; this crate is the workspace's *only* consumer of the C library.
//!
//! ## The safe surface
//!
//! Everything `pub` is safe Rust: [`parse`] copies the protobuf-encoded
//! tree out of C memory, frees the C side immediately, and hands back
//! owned data. `unsafe` is contained to the one block in [`parse`], laid
//! out to satisfy the header's contract exactly: the result struct is
//! valid until the free function is called; `data`/`message` live only
//! that long and are copied before the free.
//!
//! The protobuf tree types ([`pb`]) are checked-in prost-generated
//! bindings of `libpg_query/protobuf/pg_query.proto` (`src/pb.rs`,
//! provenance in its header) — pure Rust, `Send + Sync`.

use std::ffi::{CStr, CString};

use prost::Message as _;

pub mod pb;

/// A parse failure: PG's own message text plus, when the parser located
/// the problem, the 1-based character position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SysError {
    pub message: String,
    pub cursorpos: Option<i32>,
}

/// Parses `sql` with PostgreSQL's grammar and returns the protobuf-encoded
/// parse tree, decoded into the owned [`pb`] types.
pub fn parse(sql: &str) -> Result<pb::ParseResult, SysError> {
    let input = CString::new(sql)
        .map_err(|_| SysError { message: "query string contains an interior NUL".into(), cursorpos: None })?;

    // SAFETY: `input` is a valid C string for the call's duration. The
    // header's contract: `pg_query_parse_protobuf` returns a struct whose
    // `parse_tree.data` (length `parse_tree.len`) and `error` (when
    // non-null, with its `message`) are valid *until*
    // `pg_query_free_protobuf_parse_result` is called. Everything borrowed
    // below is copied out (decode / to_string) before the free.
    let raw = unsafe { pg_query_parse_protobuf(input.as_ptr()) };
    let outcome = if raw.error.is_null() {
        let bytes =
            unsafe { std::slice::from_raw_parts(raw.parse_tree.data as *const u8, raw.parse_tree.len) };
        pb::ParseResult::decode(bytes)
            .map_err(|e| SysError { message: format!("protobuf decode failed: {e}"), cursorpos: None })
    } else {
        let e = unsafe { &*raw.error };
        let message = unsafe { CStr::from_ptr(e.message) }.to_string_lossy().into_owned();
        Err(SysError {
            message,
            cursorpos: (e.cursorpos > 0).then_some(e.cursorpos),
        })
    };
    unsafe { pg_query_free_protobuf_parse_result(raw) };
    outcome
}

/// `#[repr(C)]` mirrors of `libpg_query/pg_query.h` (the subset this crate
/// uses). Field order must match the header exactly.
mod raw {
    #[repr(C)]
    pub struct PgQueryError {
        pub message: *mut std::os::raw::c_char,
        pub funcname: *mut std::os::raw::c_char,
        pub filename: *mut std::os::raw::c_char,
        pub lineno: std::os::raw::c_int,
        pub cursorpos: std::os::raw::c_int,
        pub context: *mut std::os::raw::c_char,
    }

    #[repr(C)]
    pub struct PgQueryProtobuf {
        pub len: usize,
        pub data: *mut std::os::raw::c_char,
    }

    #[repr(C)]
    pub struct PgQueryProtobufParseResult {
        pub parse_tree: PgQueryProtobuf,
        pub stderr_buffer: *mut std::os::raw::c_char,
        pub error: *mut PgQueryError,
    }
}

unsafe extern "C" {
    fn pg_query_parse_protobuf(input: *const std::os::raw::c_char)
        -> raw::PgQueryProtobufParseResult;
    fn pg_query_free_protobuf_parse_result(result: raw::PgQueryProtobufParseResult);
}
