use std::collections::BTreeMap;

use alloy_primitives::{Address, I256, U256};
use evm_fork_cache::{StateUpdate, StateView, cache::EvmOverlay};
use thiserror::Error;

use crate::{
    FeedId, FeedRegistration, OracleError, OraclePriceUpdate, OracleRuntime, OracleSnapshot,
    OracleSnapshotStatus, OracleStorageEffect, OracleStorageError,
};

/// A requested oracle price mock.
///
/// The price is expressed in the feed's raw answer units, matching
/// `latestRoundData().answer`. For an 8-decimal ETH/USD feed, `2_500_00000000`
/// means `2500.00000000`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OraclePriceMock {
    /// Raw oracle answer to expose.
    pub raw_answer: I256,
    /// Aggregator-local round id. Defaults to the next local round inferred from
    /// the current snapshot.
    pub round_id: Option<U256>,
    /// `startedAt` timestamp. Defaults to `updated_at`.
    pub started_at: Option<u64>,
    /// `updatedAt` timestamp. Defaults to overlay block timestamp or the current
    /// snapshot timestamp plus one second.
    pub updated_at: Option<u64>,
}

impl OraclePriceMock {
    /// Create a price mock from a raw feed answer.
    pub fn new(raw_answer: I256) -> Self {
        Self {
            raw_answer,
            round_id: None,
            started_at: None,
            updated_at: None,
        }
    }

    /// Set the aggregator-local round id to write.
    pub fn round_id(mut self, round_id: U256) -> Self {
        self.round_id = Some(round_id);
        self
    }

    /// Set the `startedAt` timestamp.
    pub fn started_at(mut self, started_at: u64) -> Self {
        self.started_at = Some(started_at);
        self
    }

    /// Set the `updatedAt` timestamp.
    pub fn updated_at(mut self, updated_at: u64) -> Self {
        self.updated_at = Some(updated_at);
        self
    }

    fn to_update(
        &self,
        registration: &FeedRegistration,
        snapshot: &OracleSnapshot,
        overlay_timestamp: Option<u64>,
    ) -> OraclePriceUpdate {
        let updated_at = self
            .updated_at
            .or(overlay_timestamp)
            .unwrap_or_else(|| snapshot.round.updated_at.saturating_add(1));
        let started_at = self.started_at.unwrap_or(updated_at);
        let round_id = self
            .round_id
            .unwrap_or_else(|| next_local_round_id(snapshot.round.round_id));
        let aggregator = registration
            .current_aggregator
            .or(snapshot.aggregator)
            .unwrap_or(registration.proxy);

        OraclePriceUpdate {
            id: registration.id,
            proxy: registration.proxy,
            aggregator,
            label: registration.label.clone(),
            base: registration.base.clone(),
            quote: registration.quote.clone(),
            raw_answer: self.raw_answer,
            decimals: registration.metadata.decimals,
            event_round_id: round_id,
            started_at,
            updated_at,
            block_number: None,
            block_hash: None,
            log_index: None,
            confidence: OracleSnapshotStatus::EventPending,
        }
    }
}

impl From<I256> for OraclePriceMock {
    fn from(raw_answer: I256) -> Self {
        Self::new(raw_answer)
    }
}

/// Result of applying an overlay-scoped oracle mock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OracleMockReport {
    /// Feed that was requested.
    pub feed_id: FeedId,
    /// User-facing oracle proxy/source address.
    pub proxy: Address,
    /// Current underlying aggregator, when known.
    pub aggregator: Option<Address>,
    /// Whether storage writes were applied to the overlay.
    pub applied: bool,
    /// Storage adapter that produced the writes.
    pub adapter: Option<&'static str>,
    /// State updates translated into overlay slot overrides.
    pub updates: Vec<StateUpdate>,
}

impl OracleMockReport {
    /// Return true when the overlay was updated.
    pub fn applied(&self) -> bool {
        self.applied
    }
}

/// Errors returned by oracle mocking helpers.
#[derive(Debug, Error)]
pub enum OracleMockError {
    /// Requested feed id is not registered.
    #[error("oracle feed `{0}` is not registered")]
    FeedNotFound(String),
    /// A registration exists but has no current snapshot.
    #[error("oracle feed `{0}` has no current snapshot")]
    MissingSnapshot(String),
    /// Storage adapter failed while preparing the mock.
    #[error(transparent)]
    Storage(#[from] OracleStorageError),
    /// A storage adapter emitted an update kind that overlay mocking cannot apply.
    #[error("oracle mock cannot apply {kind} update to an overlay")]
    UnsupportedStateUpdate {
        /// Unsupported update kind.
        kind: &'static str,
    },
    /// A masked packed-slot write required a current slot value that was not cached.
    #[error("oracle mock needs hot slot {address:?}[{slot}] for masked write")]
    ColdMaskedSlot {
        /// Contract address.
        address: Address,
        /// Storage slot.
        slot: U256,
    },
}

impl From<OracleMockError> for OracleError {
    fn from(value: OracleMockError) -> Self {
        Self::Provider(value.to_string())
    }
}

/// Extension trait for overlay-scoped oracle price mocks.
///
/// This mirrors `evm-fork-cache`'s ERC20 `mock_balance` style: the mock is
/// written only into the supplied [`EvmOverlay`], and dropping or resetting the
/// overlay discards it. Pass the cache as `state` so packed Chainlink hot slots
/// can preserve their unrelated bits.
pub trait OracleOverlayMockExt {
    /// Mock one registered oracle price in this overlay.
    fn mock_oracle_price<P, F, M>(
        &mut self,
        state: &dyn StateView,
        runtime: &OracleRuntime<P>,
        feed_id: F,
        mock: M,
    ) -> Result<OracleMockReport, OracleMockError>
    where
        F: AsRef<str>,
        M: Into<OraclePriceMock>;
}

impl OracleOverlayMockExt for EvmOverlay {
    fn mock_oracle_price<P, F, M>(
        &mut self,
        state: &dyn StateView,
        runtime: &OracleRuntime<P>,
        feed_id: F,
        mock: M,
    ) -> Result<OracleMockReport, OracleMockError>
    where
        F: AsRef<str>,
        M: Into<OraclePriceMock>,
    {
        runtime.mock_price(self, state, feed_id, mock)
    }
}

impl<P> OracleRuntime<P> {
    /// Mock one registered oracle price in an [`EvmOverlay`].
    ///
    /// The cache/runtime are not mutated. For Chainlink OCR1/OCR2 feeds with a
    /// detected layout and warmed hot slot, this writes the same storage slots a
    /// live oracle event would update. If the feed cannot be direct-written, the
    /// report returns `applied = false`.
    pub fn mock_price<F, M>(
        &self,
        overlay: &mut EvmOverlay,
        state: &dyn StateView,
        feed_id: F,
        mock: M,
    ) -> Result<OracleMockReport, OracleMockError>
    where
        F: AsRef<str>,
        M: Into<OraclePriceMock>,
    {
        let feed_id_ref = feed_id.as_ref();
        let registration = self
            .tracker()
            .registrations()
            .find(|registration| registration.id.as_str() == feed_id_ref)
            .ok_or_else(|| OracleMockError::FeedNotFound(feed_id_ref.to_string()))?;
        let snapshot = self
            .tracker()
            .latest(registration.proxy)
            .ok_or_else(|| OracleMockError::MissingSnapshot(feed_id_ref.to_string()))?;
        let update = mock
            .into()
            .to_update(&registration, snapshot, overlay.timestamp());
        let effect = self
            .storage_sync()
            .state_effect_for_answer(&registration, &update, state)?;

        let mut report = OracleMockReport {
            feed_id: registration.id,
            proxy: registration.proxy,
            aggregator: registration.current_aggregator,
            applied: false,
            adapter: None,
            updates: Vec::new(),
        };

        let OracleStorageEffect::StateUpdates { adapter, updates } = effect else {
            return Ok(report);
        };

        let mut staged = OverlayWriteState::new(state);
        for update in &updates {
            staged.stage(update)?;
        }
        staged.apply_to(overlay);

        report.applied = true;
        report.adapter = Some(adapter);
        report.updates = updates;
        Ok(report)
    }
}

struct OverlayWriteState<'a> {
    base: &'a dyn StateView,
    writes: BTreeMap<(Address, U256), U256>,
}

impl<'a> OverlayWriteState<'a> {
    fn new(base: &'a dyn StateView) -> Self {
        Self {
            base,
            writes: BTreeMap::new(),
        }
    }

    fn stage(&mut self, update: &StateUpdate) -> Result<(), OracleMockError> {
        match update {
            StateUpdate::Slot {
                address,
                slot,
                value,
            } => {
                self.writes.insert((*address, *slot), *value);
                Ok(())
            }
            StateUpdate::SlotMasked {
                address,
                slot,
                mask,
                value,
            } => {
                let current =
                    self.storage(*address, *slot)
                        .ok_or(OracleMockError::ColdMaskedSlot {
                            address: *address,
                            slot: *slot,
                        })?;
                let next = (current & !*mask) | (*value & *mask);
                self.writes.insert((*address, *slot), next);
                Ok(())
            }
            StateUpdate::SlotDelta { .. } => {
                Err(OracleMockError::UnsupportedStateUpdate { kind: "slot delta" })
            }
            StateUpdate::BalanceDelta { .. } => Err(OracleMockError::UnsupportedStateUpdate {
                kind: "balance delta",
            }),
            StateUpdate::Account { .. } => Err(OracleMockError::UnsupportedStateUpdate {
                kind: "account patch",
            }),
            StateUpdate::AccountUpsert { .. } => Err(OracleMockError::UnsupportedStateUpdate {
                kind: "account upsert",
            }),
            StateUpdate::Purge { .. } => {
                Err(OracleMockError::UnsupportedStateUpdate { kind: "purge" })
            }
            _ => Err(OracleMockError::UnsupportedStateUpdate { kind: "unknown" }),
        }
    }

    fn apply_to(self, overlay: &mut EvmOverlay) {
        for ((address, slot), value) in self.writes {
            overlay.override_slot(address, slot, value);
        }
    }
}

impl StateView for OverlayWriteState<'_> {
    fn storage(&self, address: Address, slot: U256) -> Option<U256> {
        self.writes
            .get(&(address, slot))
            .copied()
            .or_else(|| self.base.storage(address, slot))
    }
}

fn next_local_round_id(round_id: U256) -> U256 {
    let local = round_id & U256::from(u64::MAX);
    if local == U256::ZERO {
        U256::from(1_u64)
    } else {
        local.saturating_add(U256::from(1_u64))
    }
}
