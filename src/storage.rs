use std::sync::Arc;

use alloy_primitives::{Address, I256, U256, keccak256};
use evm_fork_cache::{StateUpdate, StateView};
use thiserror::Error;

use crate::{FeedRegistration, OraclePriceUpdate};

const OCR2_HOT_VARS_SLOT: u64 = 11;
const OCR2_TRANSMISSIONS_SLOT: u64 = 12;
const OCR2_LATEST_EPOCH_AND_ROUND_OFFSET_BITS: usize = 8;
const OCR2_LATEST_AGGREGATOR_ROUND_ID_OFFSET_BITS: usize = 6 * 8;
const OCR2_TRANSMISSION_OBSERVATIONS_TIMESTAMP_OFFSET_BITS: usize = 24 * 8;
const OCR2_TRANSMISSION_TIMESTAMP_OFFSET_BITS: usize = 28 * 8;
const OCR1_HOT_VARS_SLOT: u64 = 43;
const OCR1_TRANSMISSIONS_SLOT: u64 = 44;
const OCR1_LATEST_EPOCH_AND_ROUND_OFFSET_BITS: usize = 16 * 8;
const OCR1_LATEST_AGGREGATOR_ROUND_ID_OFFSET_BITS: usize = 22 * 8;
const OCR1_TRANSMISSION_TIMESTAMP_OFFSET_BITS: usize = 24 * 8;
const UINT32_BITS: usize = 32;
const UINT40_BITS: usize = 40;
const INT192_BITS: usize = 192;

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

impl OracleStorageError {
    fn adapter(adapter: &'static str, message: impl Into<String>) -> Self {
        Self::Adapter {
            adapter,
            message: message.into(),
        }
    }
}

/// Result of attempting event-derived oracle storage sync.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OracleStorageEffect {
    /// A layout adapter emitted direct cache state updates.
    StateUpdates {
        /// Adapter that produced the updates.
        adapter: &'static str,
        /// State updates to apply.
        updates: Vec<StateUpdate>,
    },
    /// No layout adapter supported the feed; callers should use purge/refetch.
    FallbackPurge,
}

/// OCR2 transmission data that can be translated into exact aggregator storage writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ocr2TransmissionStorageUpdate {
    /// Emitting aggregator address.
    pub aggregator: Address,
    /// Aggregator-local round id.
    pub aggregator_round_id: U256,
    /// OCR2 median answer.
    pub answer: I256,
    /// OCR2 observations timestamp, stored as `startedAt`.
    pub observations_timestamp: u64,
    /// On-chain transmission timestamp, stored as `updatedAt`.
    pub transmission_timestamp: u64,
    /// OCR2 packed epoch and round.
    pub epoch_and_round: u64,
}

/// OCR1 transmission data that can be translated into exact aggregator storage writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ocr1TransmissionStorageUpdate {
    /// Emitting aggregator address.
    pub aggregator: Address,
    /// Aggregator-local round id.
    pub aggregator_round_id: U256,
    /// OCR1 median answer.
    pub answer: I256,
    /// On-chain transmission timestamp, stored as both `startedAt` and `updatedAt`.
    pub transmission_timestamp: u64,
    /// OCR1 packed epoch and round.
    pub epoch_and_round: u64,
}

/// Adapter that can translate oracle events into direct EVM cache state updates.
///
/// Implementations must be deployment-layout specific. Returning `Ok(None)`
/// means the adapter does not support the feed/update and the caller should try
/// the next adapter or fall back to conservative invalidation.
pub trait OracleStorageAdapter: Send + Sync {
    /// Stable adapter name for diagnostics.
    fn name(&self) -> &'static str;

    /// Storage slots that should be warmed before live event handling.
    ///
    /// Direct storage adapters may need a packed word to be present before they
    /// can safely emit masked writes. Returning these slots lets callers bulk
    /// load them once through `evm-fork-cache` before entering the reactive loop.
    fn warm_slots_for_registration(
        &self,
        _registration: &FeedRegistration,
    ) -> Vec<(Address, U256)> {
        Vec::new()
    }

    /// Produce direct state updates for a detected oracle layout.
    fn state_updates_for_answer(
        &self,
        registration: &FeedRegistration,
        update: &OraclePriceUpdate,
        state: &dyn StateView,
    ) -> Result<Option<Vec<StateUpdate>>, OracleStorageError>;

    /// Produce direct state updates for a detected OCR2 `NewTransmission` event.
    fn state_updates_for_ocr2_transmission(
        &self,
        _registration: &FeedRegistration,
        _update: &Ocr2TransmissionStorageUpdate,
        _state: &dyn StateView,
    ) -> Result<Option<Vec<StateUpdate>>, OracleStorageError> {
        Ok(None)
    }

    /// Produce direct state updates for a detected OCR1 `NewTransmission` event.
    fn state_updates_for_ocr1_transmission(
        &self,
        _registration: &FeedRegistration,
        _update: &Ocr1TransmissionStorageUpdate,
        _state: &dyn StateView,
    ) -> Result<Option<Vec<StateUpdate>>, OracleStorageError> {
        Ok(None)
    }

    /// Return true when this adapter wants OCR2 `NewTransmission` as the primary event.
    fn prefers_ocr2_new_transmission(&self, _registration: &FeedRegistration) -> bool {
        false
    }

    /// Return true when this adapter wants OCR1 `NewTransmission` as the primary event.
    fn prefers_ocr1_new_transmission(&self, _registration: &FeedRegistration) -> bool {
        false
    }
}

/// Verified direct-storage adapter for Chainlink OCR2 aggregators.
///
/// This adapter targets the `AccessControlledOCR2Aggregator 1.0.0` storage
/// layout:
///
/// - `s_hotVars` at slot 11, with `latestAggregatorRoundId` packed at byte
///   offset 6 and `latestEpochAndRound` packed at byte offset 1.
/// - `s_transmissions` at slot 12, keyed by the aggregator-local `uint32` round
///   id.
///
/// The adapter is enabled by registration-time layout detection. If the packed
/// hot-vars slot is cold in `StateView`, the adapter declines the event and the
/// caller should fall back to purge/refetch.
#[derive(Clone, Debug, Default)]
pub struct ChainlinkOcr2StorageAdapter;

impl ChainlinkOcr2StorageAdapter {
    const NAME: &'static str = "chainlink-ocr2";

    /// Create an OCR2 storage adapter.
    pub fn new() -> Self {
        Self
    }

    /// Storage slot for OCR2 `s_hotVars`.
    pub fn hot_vars_slot() -> U256 {
        U256::from(OCR2_HOT_VARS_SLOT)
    }

    /// Mask selecting the packed `s_hotVars.latestAggregatorRoundId` field.
    pub fn latest_aggregator_round_id_mask() -> U256 {
        uint_mask(UINT32_BITS) << OCR2_LATEST_AGGREGATOR_ROUND_ID_OFFSET_BITS
    }

    /// Mask selecting the packed `s_hotVars.latestEpochAndRound` field.
    pub fn latest_epoch_and_round_mask() -> U256 {
        uint_mask(UINT40_BITS) << OCR2_LATEST_EPOCH_AND_ROUND_OFFSET_BITS
    }

    /// Masked value for `s_hotVars.latestAggregatorRoundId`.
    pub fn latest_aggregator_round_id_value(round_id: u32) -> U256 {
        U256::from(round_id) << OCR2_LATEST_AGGREGATOR_ROUND_ID_OFFSET_BITS
    }

    /// Masked value for `s_hotVars.latestEpochAndRound`.
    pub fn latest_epoch_and_round_value(epoch_and_round: u64) -> Result<U256, OracleStorageError> {
        if epoch_and_round > uint_mask_u64(UINT40_BITS) {
            return Err(OracleStorageError::adapter(
                Self::NAME,
                format!("OCR2 epochAndRound {epoch_and_round} does not fit uint40"),
            ));
        }

        Ok(U256::from(epoch_and_round) << OCR2_LATEST_EPOCH_AND_ROUND_OFFSET_BITS)
    }

    /// Mapping slot for OCR2 `s_transmissions[round_id]`.
    pub fn transmission_slot(round_id: u32) -> U256 {
        mapping_slot(U256::from(round_id), U256::from(OCR2_TRANSMISSIONS_SLOT))
    }

    /// Pack OCR2 transmission from a legacy `AnswerUpdated` timestamp.
    ///
    /// `AnswerUpdated` exposes one timestamp, so this compatibility path can
    /// only seed `startedAt` and `updatedAt` with the same value. The preferred
    /// OCR2 path is `NewTransmission`, which carries both timestamps.
    pub fn pack_transmission_from_event(
        answer: I256,
        updated_at: u64,
    ) -> Result<U256, OracleStorageError> {
        let timestamp = u32::try_from(updated_at).map_err(|_| {
            OracleStorageError::adapter(
                Self::NAME,
                format!("OCR2 timestamp {updated_at} does not fit uint32"),
            )
        })?;
        Self::pack_transmission(answer, timestamp, timestamp)
    }

    /// Pack an OCR2 transmission word with explicit observation/transmission timestamps.
    pub fn pack_transmission(
        answer: I256,
        observations_timestamp: u32,
        transmission_timestamp: u32,
    ) -> Result<U256, OracleStorageError> {
        let answer = encode_int192(answer)?;
        Ok(answer
            | (U256::from(observations_timestamp)
                << OCR2_TRANSMISSION_OBSERVATIONS_TIMESTAMP_OFFSET_BITS)
            | (U256::from(transmission_timestamp) << OCR2_TRANSMISSION_TIMESTAMP_OFFSET_BITS))
    }
}

impl OracleStorageAdapter for ChainlinkOcr2StorageAdapter {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn warm_slots_for_registration(&self, registration: &FeedRegistration) -> Vec<(Address, U256)> {
        let Some(aggregator) = registration.current_aggregator else {
            return Vec::new();
        };
        if registration_supports_ocr2(registration, aggregator) {
            vec![(aggregator, Self::hot_vars_slot())]
        } else {
            Vec::new()
        }
    }

    fn state_updates_for_answer(
        &self,
        registration: &FeedRegistration,
        update: &OraclePriceUpdate,
        state: &dyn StateView,
    ) -> Result<Option<Vec<StateUpdate>>, OracleStorageError> {
        let Some(aggregator) = registration.current_aggregator else {
            return Ok(None);
        };
        if update.aggregator != aggregator || !registration_supports_ocr2(registration, aggregator)
        {
            return Ok(None);
        }
        if state.storage(aggregator, Self::hot_vars_slot()).is_none() {
            return Ok(None);
        }

        let round_id = u32::try_from(update.event_round_id).map_err(|_| {
            OracleStorageError::adapter(
                Self::NAME,
                format!(
                    "OCR2 round id {} does not fit aggregator-local uint32",
                    update.event_round_id
                ),
            )
        })?;
        let transmission =
            Self::pack_transmission_from_event(update.raw_answer, update.updated_at)?;

        Ok(Some(vec![
            StateUpdate::slot_masked(
                aggregator,
                Self::hot_vars_slot(),
                Self::latest_aggregator_round_id_mask(),
                Self::latest_aggregator_round_id_value(round_id),
            ),
            StateUpdate::slot(aggregator, Self::transmission_slot(round_id), transmission),
        ]))
    }

    fn state_updates_for_ocr2_transmission(
        &self,
        registration: &FeedRegistration,
        update: &Ocr2TransmissionStorageUpdate,
        state: &dyn StateView,
    ) -> Result<Option<Vec<StateUpdate>>, OracleStorageError> {
        let Some(aggregator) = registration.current_aggregator else {
            return Ok(None);
        };
        if update.aggregator != aggregator || !registration_supports_ocr2(registration, aggregator)
        {
            return Ok(None);
        }
        if state.storage(aggregator, Self::hot_vars_slot()).is_none() {
            return Ok(None);
        }

        let round_id = u32::try_from(update.aggregator_round_id).map_err(|_| {
            OracleStorageError::adapter(
                Self::NAME,
                format!(
                    "OCR2 round id {} does not fit aggregator-local uint32",
                    update.aggregator_round_id
                ),
            )
        })?;
        let observations_timestamp =
            u32::try_from(update.observations_timestamp).map_err(|_| {
                OracleStorageError::adapter(
                    Self::NAME,
                    format!(
                        "OCR2 observations timestamp {} does not fit uint32",
                        update.observations_timestamp
                    ),
                )
            })?;
        let transmission_timestamp =
            u32::try_from(update.transmission_timestamp).map_err(|_| {
                OracleStorageError::adapter(
                    Self::NAME,
                    format!(
                        "OCR2 transmission timestamp {} does not fit uint32",
                        update.transmission_timestamp
                    ),
                )
            })?;
        let hot_vars_mask =
            Self::latest_epoch_and_round_mask() | Self::latest_aggregator_round_id_mask();
        let hot_vars_value = Self::latest_epoch_and_round_value(update.epoch_and_round)?
            | Self::latest_aggregator_round_id_value(round_id);
        let transmission = Self::pack_transmission(
            update.answer,
            observations_timestamp,
            transmission_timestamp,
        )?;

        Ok(Some(vec![
            StateUpdate::slot_masked(
                aggregator,
                Self::hot_vars_slot(),
                hot_vars_mask,
                hot_vars_value,
            ),
            StateUpdate::slot(aggregator, Self::transmission_slot(round_id), transmission),
        ]))
    }

    fn prefers_ocr2_new_transmission(&self, registration: &FeedRegistration) -> bool {
        registration
            .current_aggregator
            .is_some_and(|aggregator| registration_supports_ocr2(registration, aggregator))
    }
}

fn registration_supports_ocr2(registration: &FeedRegistration, aggregator: Address) -> bool {
    registration
        .aggregator_layout
        .as_ref()
        .is_some_and(|layout| layout.aggregator == aggregator && layout.is_chainlink_ocr2_v1())
}

/// Verified direct-storage adapter for Chainlink OCR1 aggregators.
///
/// This adapter targets the common `AccessControlledOffchainAggregator`
/// 2.x/3.x/4.x storage layout:
///
/// - `s_hotVars` at slot 43, with `latestEpochAndRound` packed at byte offset
///   16 and `latestAggregatorRoundId` packed at byte offset 22.
/// - `s_transmissions` at slot 44, keyed by the aggregator-local `uint32`
///   round id, storing `Transmission { int192 answer; uint64 timestamp; }`.
///
/// OCR1 `NewTransmission` does not carry a separate observations timestamp, so
/// the block timestamp is used for both `startedAt` and `updatedAt`, matching
/// the OCR1 `latestRoundData()` implementation.
#[derive(Clone, Debug, Default)]
pub struct ChainlinkOcr1StorageAdapter;

impl ChainlinkOcr1StorageAdapter {
    const NAME: &'static str = "chainlink-ocr1";

    /// Create an OCR1 storage adapter.
    pub fn new() -> Self {
        Self
    }

    /// Storage slot for OCR1 `s_hotVars`.
    pub fn hot_vars_slot() -> U256 {
        U256::from(OCR1_HOT_VARS_SLOT)
    }

    /// Mask selecting the packed `s_hotVars.latestAggregatorRoundId` field.
    pub fn latest_aggregator_round_id_mask() -> U256 {
        uint_mask(UINT32_BITS) << OCR1_LATEST_AGGREGATOR_ROUND_ID_OFFSET_BITS
    }

    /// Mask selecting the packed `s_hotVars.latestEpochAndRound` field.
    pub fn latest_epoch_and_round_mask() -> U256 {
        uint_mask(UINT40_BITS) << OCR1_LATEST_EPOCH_AND_ROUND_OFFSET_BITS
    }

    /// Masked value for `s_hotVars.latestAggregatorRoundId`.
    pub fn latest_aggregator_round_id_value(round_id: u32) -> U256 {
        U256::from(round_id) << OCR1_LATEST_AGGREGATOR_ROUND_ID_OFFSET_BITS
    }

    /// Masked value for `s_hotVars.latestEpochAndRound`.
    pub fn latest_epoch_and_round_value(epoch_and_round: u64) -> Result<U256, OracleStorageError> {
        if epoch_and_round > uint_mask_u64(UINT40_BITS) {
            return Err(OracleStorageError::adapter(
                Self::NAME,
                format!("OCR1 epochAndRound {epoch_and_round} does not fit uint40"),
            ));
        }

        Ok(U256::from(epoch_and_round) << OCR1_LATEST_EPOCH_AND_ROUND_OFFSET_BITS)
    }

    /// Mapping slot for OCR1 `s_transmissions[round_id]`.
    pub fn transmission_slot(round_id: u32) -> U256 {
        mapping_slot(U256::from(round_id), U256::from(OCR1_TRANSMISSIONS_SLOT))
    }

    /// Pack an OCR1 transmission word.
    pub fn pack_transmission(
        answer: I256,
        transmission_timestamp: u64,
    ) -> Result<U256, OracleStorageError> {
        let answer = encode_int192_with_adapter(answer, Self::NAME, "OCR1")?;
        Ok(
            answer
                | (U256::from(transmission_timestamp) << OCR1_TRANSMISSION_TIMESTAMP_OFFSET_BITS),
        )
    }
}

impl OracleStorageAdapter for ChainlinkOcr1StorageAdapter {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn warm_slots_for_registration(&self, registration: &FeedRegistration) -> Vec<(Address, U256)> {
        let Some(aggregator) = registration.current_aggregator else {
            return Vec::new();
        };
        if registration_supports_ocr1(registration, aggregator) {
            vec![(aggregator, Self::hot_vars_slot())]
        } else {
            Vec::new()
        }
    }

    fn state_updates_for_answer(
        &self,
        _registration: &FeedRegistration,
        _update: &OraclePriceUpdate,
        _state: &dyn StateView,
    ) -> Result<Option<Vec<StateUpdate>>, OracleStorageError> {
        Ok(None)
    }

    fn state_updates_for_ocr1_transmission(
        &self,
        registration: &FeedRegistration,
        update: &Ocr1TransmissionStorageUpdate,
        state: &dyn StateView,
    ) -> Result<Option<Vec<StateUpdate>>, OracleStorageError> {
        let Some(aggregator) = registration.current_aggregator else {
            return Ok(None);
        };
        if update.aggregator != aggregator || !registration_supports_ocr1(registration, aggregator)
        {
            return Ok(None);
        }
        if state.storage(aggregator, Self::hot_vars_slot()).is_none() {
            return Ok(None);
        }

        let round_id = u32::try_from(update.aggregator_round_id).map_err(|_| {
            OracleStorageError::adapter(
                Self::NAME,
                format!(
                    "OCR1 round id {} does not fit aggregator-local uint32",
                    update.aggregator_round_id
                ),
            )
        })?;
        let hot_vars_mask =
            Self::latest_epoch_and_round_mask() | Self::latest_aggregator_round_id_mask();
        let hot_vars_value = Self::latest_epoch_and_round_value(update.epoch_and_round)?
            | Self::latest_aggregator_round_id_value(round_id);
        let transmission = Self::pack_transmission(update.answer, update.transmission_timestamp)?;

        Ok(Some(vec![
            StateUpdate::slot_masked(
                aggregator,
                Self::hot_vars_slot(),
                hot_vars_mask,
                hot_vars_value,
            ),
            StateUpdate::slot(aggregator, Self::transmission_slot(round_id), transmission),
        ]))
    }

    fn prefers_ocr1_new_transmission(&self, registration: &FeedRegistration) -> bool {
        registration
            .current_aggregator
            .is_some_and(|aggregator| registration_supports_ocr1(registration, aggregator))
    }
}

fn registration_supports_ocr1(registration: &FeedRegistration, aggregator: Address) -> bool {
    registration
        .aggregator_layout
        .as_ref()
        .is_some_and(|layout| layout.aggregator == aggregator && layout.is_chainlink_ocr1())
}

/// Ordered collection of oracle storage adapters.
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

    /// Create a storage sync registry with built-in Chainlink layout adapters.
    pub fn chainlink_defaults() -> Self {
        Self::new()
            .with_adapter(Arc::new(ChainlinkOcr2StorageAdapter::new()))
            .with_adapter(Arc::new(ChainlinkOcr1StorageAdapter::new()))
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

    /// Return the de-duplicated storage slots needed for direct event writes.
    pub fn warm_slots_for_registrations<'a>(
        &self,
        registrations: impl IntoIterator<Item = &'a FeedRegistration>,
    ) -> Vec<(Address, U256)> {
        let mut slots = Vec::new();
        for registration in registrations {
            for adapter in &self.adapters {
                for slot in adapter.warm_slots_for_registration(registration) {
                    if !slots.contains(&slot) {
                        slots.push(slot);
                    }
                }
            }
        }
        slots
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

    /// Try adapters in order for an OCR2 transmission, falling back to purge/refetch.
    pub fn state_effect_for_ocr2_transmission(
        &self,
        registration: &FeedRegistration,
        update: &Ocr2TransmissionStorageUpdate,
        state: &dyn StateView,
    ) -> Result<OracleStorageEffect, OracleStorageError> {
        for adapter in &self.adapters {
            if let Some(updates) =
                adapter.state_updates_for_ocr2_transmission(registration, update, state)?
            {
                return Ok(OracleStorageEffect::StateUpdates {
                    adapter: adapter.name(),
                    updates,
                });
            }
        }

        Ok(OracleStorageEffect::FallbackPurge)
    }

    /// Try adapters in order for an OCR1 transmission, falling back to purge/refetch.
    pub fn state_effect_for_ocr1_transmission(
        &self,
        registration: &FeedRegistration,
        update: &Ocr1TransmissionStorageUpdate,
        state: &dyn StateView,
    ) -> Result<OracleStorageEffect, OracleStorageError> {
        for adapter in &self.adapters {
            if let Some(updates) =
                adapter.state_updates_for_ocr1_transmission(registration, update, state)?
            {
                return Ok(OracleStorageEffect::StateUpdates {
                    adapter: adapter.name(),
                    updates,
                });
            }
        }

        Ok(OracleStorageEffect::FallbackPurge)
    }

    /// Return true when any adapter wants OCR2 `NewTransmission` for this feed.
    pub fn prefers_ocr2_new_transmission(&self, registration: &FeedRegistration) -> bool {
        self.adapters
            .iter()
            .any(|adapter| adapter.prefers_ocr2_new_transmission(registration))
    }

    /// Return true when any adapter wants OCR1 `NewTransmission` for this feed.
    pub fn prefers_ocr1_new_transmission(&self, registration: &FeedRegistration) -> bool {
        self.adapters
            .iter()
            .any(|adapter| adapter.prefers_ocr1_new_transmission(registration))
    }
}

fn mapping_slot(key: U256, base_slot: U256) -> U256 {
    let mut preimage = [0_u8; 64];
    preimage[..32].copy_from_slice(&key.to_be_bytes::<32>());
    preimage[32..].copy_from_slice(&base_slot.to_be_bytes::<32>());
    U256::from_be_slice(keccak256(preimage).as_slice())
}

fn encode_int192(value: I256) -> Result<U256, OracleStorageError> {
    encode_int192_with_adapter(value, ChainlinkOcr2StorageAdapter::NAME, "OCR2")
}

fn encode_int192_with_adapter(
    value: I256,
    adapter: &'static str,
    family: &'static str,
) -> Result<U256, OracleStorageError> {
    let raw = value.into_raw();
    let encoded = raw & uint_mask(INT192_BITS);
    let sign_bit_set = (encoded & (U256::from(1_u8) << (INT192_BITS - 1))) != U256::ZERO;
    let high = raw >> INT192_BITS;
    let expected_high = if sign_bit_set {
        uint_mask(256 - INT192_BITS)
    } else {
        U256::ZERO
    };

    if high != expected_high {
        return Err(OracleStorageError::adapter(
            adapter,
            format!("answer {value} does not fit {family} int192"),
        ));
    }

    Ok(encoded)
}

fn uint_mask(bits: usize) -> U256 {
    debug_assert!(bits <= 256);
    match bits {
        0 => U256::ZERO,
        256 => U256::MAX,
        bits => (U256::from(1_u8) << bits) - U256::from(1_u8),
    }
}

fn uint_mask_u64(bits: usize) -> u64 {
    debug_assert!(bits < 64);
    (1_u64 << bits) - 1
}
