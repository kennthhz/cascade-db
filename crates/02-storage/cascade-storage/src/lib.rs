//! Random-access page I/O for Cascade DB.
//!
//! See `README.md` and `AI/CONTEXT.md` for the design contract.

pub mod core_storage;
pub mod traits;

pub use core_storage::CoreStorage;
pub use traits::{PageStore, StorageConfig, StorageError, StorageManager};
