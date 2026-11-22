//! [`MemKeyProvider`] (§4.10): an in-memory key chain per key id —
//! `current` hands out the latest version, `get` an exact one. Key
//! material lives in `ScopeKey`, which zeroes on drop.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use cascade_types::LocalBoxFuture;

use crate::error::KeyError;
use crate::key_provider::{KeyProvider, KeyRef, KeyVersion, ScopeKey};
use crate::OpCtx;

#[derive(Default)]
struct ChainInner {
    versions: Vec<(KeyVersion, Vec<u8>)>,
}

/// In-memory provider: `rotate` seals with a new version, which is how
/// rotation tests drive it (§7).
#[derive(Clone, Default)]
pub struct MemKeyProvider {
    chains: Arc<Mutex<HashMap<u64, ChainInner>>>,
}

impl MemKeyProvider {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a new current version for `key` (32 bytes for AES-256) and
    /// returns its version number.
    pub fn rotate(&self, key: KeyRef, material: Vec<u8>) -> KeyVersion {
        let mut chains = self.chains.lock().expect("key lock");
        let chain = chains.entry(key.key_id).or_default();
        let version = KeyVersion(chain.versions.len() as u32 + 1);
        chain.versions.push((version, material));
        version
    }
}

impl KeyProvider for MemKeyProvider {
    fn current<'a>(
        &'a self,
        _ctx: &'a OpCtx<'_>,
        key: KeyRef,
    ) -> LocalBoxFuture<'a, Result<(KeyVersion, ScopeKey), KeyError>> {
        Box::pin(async move {
            let chains = self.chains.lock().expect("key lock");
            let chain = chains.get(&key.key_id).ok_or(KeyError::UnknownKey)?;
            let (version, material) = chain.versions.last().ok_or(KeyError::UnknownKey)?;
            Ok((*version, ScopeKey::new(material.clone())))
        })
    }

    fn get<'a>(
        &'a self,
        _ctx: &'a OpCtx<'_>,
        key: KeyRef,
        version: KeyVersion,
    ) -> LocalBoxFuture<'a, Result<ScopeKey, KeyError>> {
        Box::pin(async move {
            let chains = self.chains.lock().expect("key lock");
            let chain = chains.get(&key.key_id).ok_or(KeyError::UnknownKey)?;
            let material = chain
                .versions
                .iter()
                .find(|(v, _)| *v == version)
                .map(|(_, m)| m)
                .ok_or(KeyError::UnknownVersion)?;
            Ok(ScopeKey::new(material.clone()))
        })
    }
}
