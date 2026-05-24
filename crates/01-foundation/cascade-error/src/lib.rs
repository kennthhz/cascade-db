//! Unified error type for Cascade DB.
//!
//! Each subsystem crate defines its own local error and provides `Into<Error>`
//! at the boundary. See `AI/CONTEXT.md`.

use thiserror::Error;

/// Top-level Cascade DB error.
#[derive(Debug, Error)]
pub enum Error {
    #[error("storage error: {0}")]
    Storage(String),

    #[error("wal error: {0}")]
    Wal(String),

    #[error("transaction error: {0}")]
    Txn(String),

    #[error("catalog error: {0}")]
    Catalog(String),

    #[error("sql error: {0}")]
    Sql(String),

    #[error("wire protocol error: {0}")]
    Wire(String),

    #[error("internal error: {0}")]
    Internal(String),
}

/// Workspace `Result` alias.
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// PostgreSQL SQLSTATE code for this error (reserved for §4.1).
    ///
    /// v1 placeholder — actual mapping will be filled in alongside the wire-protocol crate.
    pub fn sqlstate(&self) -> &'static str {
        match self {
            Error::Storage(_) | Error::Wal(_) => "58000", // system_error
            Error::Txn(_)                     => "40000", // transaction_rollback (family)
            Error::Catalog(_)                 => "42000", // syntax_error_or_access_rule_violation
            Error::Sql(_)                     => "42601", // syntax_error
            Error::Wire(_)                    => "08000", // connection_exception
            Error::Internal(_)                => "XX000", // internal_error
        }
    }
}
