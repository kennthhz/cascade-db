//! Random-access page I/O for Cascade DB.
//!
//! See `README.md` and `AI/CONTEXT.md` for the design contract.

pub mod traits;
pub mod core_storage;

pub use traits::{PageStore, StorageError, StorageConfig, StorageManager};
pub use core_storage::CoreStorage;
