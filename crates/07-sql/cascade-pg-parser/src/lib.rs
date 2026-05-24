//! Safe Rust wrapper over libpg_query.
//!
//! See `README.md`, `AI/CONTEXT.md`, and
//! [ADR 0002](../../../AI/decisions/0002-libpg-query-ffi-exception.md).

#![allow(dead_code)]

/// A parsed SQL statement. Owns all memory backing the tree.
pub struct ParseTree {
    _placeholder: (),
}

/// Parse a SQL string. Implementation lands in Phase 4.
pub fn parse(_sql: &str) -> Result<ParseTree, ParseError> {
    todo!("Phase 4: link libpg_query, wrap pg_query_parse")
}

#[derive(Debug)]
pub enum ParseError {
    Syntax { message: String, position: u32 },
    Internal(String),
}
