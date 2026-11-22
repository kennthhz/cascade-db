//! `KeyProvider`: keys for encryption at rest (§4.7). Bytes cross the
//! storage contracts in plaintext (S11) — encryption happens *inside* the
//! components, which fetch key material here. Two planned implementations:
//! a local key file (open-source single server, no control plane) and an
//! adapter to an external KMS (managed service).

use cascade_types::LocalBoxFuture;

use crate::error::KeyError;
use crate::OpCtx;

/// Identifies a scope's key chain (§4.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct KeyRef {
    pub key_id: u64,
}

/// A key version: new writes use the current version; each extent and page
/// records the version it was written with, so old data stays readable
/// through rotation (§7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KeyVersion(pub u32);

/// Key material for one scope. Zeroes its memory on drop and never
/// appears in logs or debug output (§4.7).
pub struct ScopeKey {
    bytes: Vec<u8>,
}

impl ScopeKey {
    /// Wraps raw key material (32 bytes for AES-256).
    pub fn new(bytes: Vec<u8>) -> Self {
        debug_assert!(!bytes.is_empty(), "a scope key is never empty");
        ScopeKey { bytes }
    }

    /// The raw material. Callers use it to seal/unseal in place and drop
    /// their view immediately; the key itself zeroes on drop.
    pub fn material(&self) -> &[u8] {
        &self.bytes
    }

    /// The length of the material in bytes.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Always false — a scope key is never empty (debug_assert guards).
    pub fn is_empty(&self) -> bool {
        false
    }
}

impl std::fmt::Debug for ScopeKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Key material never appears in logs or debug output (§4.7).
        f.debug_struct("ScopeKey")
            .field("len", &self.bytes.len())
            .finish_non_exhaustive()
    }
}

impl Drop for ScopeKey {
    fn drop(&mut self) {
        for b in self.bytes.iter_mut() {
            *b = 0;
        }
    }
}

/// The key contract (§4.7).
pub trait KeyProvider: Send + Sync {
    /// The current version and material for `key`. Writers seal with this.
    fn current<'a>(
        &'a self,
        ctx: &'a OpCtx<'_>,
        key: KeyRef,
    ) -> LocalBoxFuture<'a, Result<(KeyVersion, ScopeKey), KeyError>>;

    /// Material for an exact version — reads of old extents and pages.
    fn get<'a>(
        &'a self,
        ctx: &'a OpCtx<'_>,
        key: KeyRef,
        version: KeyVersion,
    ) -> LocalBoxFuture<'a, Result<ScopeKey, KeyError>>;
}
