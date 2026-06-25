use std::sync::Arc;

use evm_fork_cache::{StateUpdate, StateView};
use thiserror::Error;

use crate::{FeedRegistration, OraclePriceUpdate};

/// Errors returned by oracle storage adapters.
#[derive(Debug, Error)]
pub enum OracleStorageError {
    /// Adapter-specific error.
    #[error("oracle storage adapter `{adapter}` failed: {message}")]
    Adapter {
        /// Adapter name.
        adapter: &'static str,
        /// Error message.
        message: String,
    },
}

/// Result of attempting event-derived oracle storage sync.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OracleStorageEffect {
    /// A verified adapter emitted direct cache state updates.
    StateUpdates {
        /// Adapter that produced the updates.
        adapter: &'static str,
        /// State updates to apply.
        updates: Vec<StateUpdate>,
    },
    /// No verified adapter supported the feed; callers should use purge/refetch.
    FallbackPurge,
}

/// Adapter that can translate oracle events into direct EVM cache state updates.
///
/// Implementations must be deployment-layout specific. Returning `Ok(None)`
/// means the adapter does not support the feed/update and the caller should try
/// the next adapter or fall back to conservative invalidation.
pub trait OracleStorageAdapter: Send + Sync {
    /// Stable adapter name for diagnostics.
    fn name(&self) -> &'static str;

    /// Produce direct state updates for a verified oracle layout.
    fn state_updates_for_answer(
        &self,
        registration: &FeedRegistration,
        update: &OraclePriceUpdate,
        state: &dyn StateView,
    ) -> Result<Option<Vec<StateUpdate>>, OracleStorageError>;
}

/// Ordered collection of verified oracle storage adapters.
#[derive(Clone, Default)]
pub struct OracleStorageSync {
    adapters: Vec<Arc<dyn OracleStorageAdapter>>,
}

impl std::fmt::Debug for OracleStorageSync {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OracleStorageSync")
            .field("adapters", &self.adapters.len())
            .finish()
    }
}

impl OracleStorageSync {
    /// Create an empty storage sync registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a storage sync registry with one adapter appended.
    pub fn with_adapter(mut self, adapter: Arc<dyn OracleStorageAdapter>) -> Self {
        self.adapters.push(adapter);
        self
    }

    /// Append one adapter.
    pub fn push_adapter(&mut self, adapter: Arc<dyn OracleStorageAdapter>) {
        self.adapters.push(adapter);
    }

    /// Return true when no storage adapters are configured.
    pub fn is_empty(&self) -> bool {
        self.adapters.is_empty()
    }

    /// Try adapters in order, falling back to purge/refetch if none supports the feed.
    pub fn state_effect_for_answer(
        &self,
        registration: &FeedRegistration,
        update: &OraclePriceUpdate,
        state: &dyn StateView,
    ) -> Result<OracleStorageEffect, OracleStorageError> {
        for adapter in &self.adapters {
            if let Some(updates) = adapter.state_updates_for_answer(registration, update, state)? {
                return Ok(OracleStorageEffect::StateUpdates {
                    adapter: adapter.name(),
                    updates,
                });
            }
        }

        Ok(OracleStorageEffect::FallbackPurge)
    }
}
