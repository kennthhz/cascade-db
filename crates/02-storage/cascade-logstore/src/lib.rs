//! The log store (storage-layer-design.md §5) — placeholder while the M4
//! red tests are written; frame format lands first, then segments,
//! manifest, flusher, retention, recovery.

pub mod drr;
pub mod frame;
pub mod manifest;
