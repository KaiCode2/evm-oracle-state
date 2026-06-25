use alloy_primitives::{Address, I256, U256};

use crate::{
    FeedRegistration, FeedStatus, OracleError, OracleSnapshot, OracleSnapshotStatus, RoundData,
    SnapshotSource,
};

/// Consumer-facing typed oracle price.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OraclePrice {
    /// Stable feed id.
    pub id: crate::FeedId,
    /// User-facing proxy address.
    pub proxy: Address,
    /// Best-known aggregator.
    pub aggregator: Option<Address>,
    /// Optional human-readable label.
    pub label: Option<String>,
    /// Optional base symbol.
    pub base: Option<String>,
    /// Optional quote symbol.
    pub quote: Option<String>,
    /// Raw signed answer.
    pub raw_answer: I256,
    /// Feed decimals.
    pub decimals: u8,
    /// Current round id.
    pub round_id: U256,
    /// Current `updatedAt` timestamp.
    pub updated_at: u64,
    /// Full current round data.
    pub round: RoundData,
    /// Round validity classification.
    pub status: FeedStatus,
    /// Snapshot lifecycle confidence.
    pub confidence: OracleSnapshotStatus,
    /// Source that produced the current snapshot.
    pub source: SnapshotSource,
}

impl OraclePrice {
    pub(crate) fn from_snapshot(
        snapshot: &OracleSnapshot,
        registration: &FeedRegistration,
    ) -> Self {
        Self {
            id: snapshot.id,
            proxy: snapshot.proxy,
            aggregator: snapshot.aggregator,
            label: registration.label.clone(),
            base: registration.base.clone(),
            quote: registration.quote.clone(),
            raw_answer: snapshot.round.answer,
            decimals: snapshot.metadata.decimals,
            round_id: snapshot.round.round_id,
            updated_at: snapshot.round.updated_at,
            round: snapshot.round.clone(),
            status: snapshot.status.clone(),
            confidence: snapshot.confidence,
            source: snapshot.source,
        }
    }

    /// Return true when the price is fresh and allowed for immediate decisions.
    pub fn is_actionable(&self) -> bool {
        self.status == FeedStatus::Fresh
            && matches!(
                self.confidence,
                OracleSnapshotStatus::EventPending
                    | OracleSnapshotStatus::Confirmed
                    | OracleSnapshotStatus::Corrected
            )
    }

    /// Return true when the price came from an event awaiting reconciliation.
    pub fn is_event_pending(&self) -> bool {
        self.confidence == OracleSnapshotStatus::EventPending
    }

    /// Return true when the price was confirmed by a proxy read.
    pub fn is_confirmed(&self) -> bool {
        self.confidence == OracleSnapshotStatus::Confirmed
    }

    /// Return true when the price was corrected by a proxy read.
    pub fn is_corrected(&self) -> bool {
        self.confidence == OracleSnapshotStatus::Corrected
    }

    /// Return the corresponding latest round tuple.
    pub fn round_data(&self) -> RoundData {
        self.round.clone()
    }

    /// Scale `raw_answer` to `target_decimals`.
    pub fn scaled_to(&self, target_decimals: u8) -> Result<I256, OracleError> {
        scale_i256(self.raw_answer, self.decimals, target_decimals)
    }
}

fn scale_i256(value: I256, from_decimals: u8, to_decimals: u8) -> Result<I256, OracleError> {
    if from_decimals == to_decimals {
        return Ok(value);
    }

    let diff = from_decimals.abs_diff(to_decimals);
    let factor = I256::unchecked_from(10_i64)
        .checked_pow(U256::from(diff))
        .ok_or_else(|| OracleError::Provider("decimal scale factor overflowed".to_string()))?;

    if to_decimals > from_decimals {
        value
            .checked_mul(factor)
            .ok_or_else(|| OracleError::Provider("scaled oracle price overflowed".to_string()))
    } else {
        value
            .checked_div(factor)
            .ok_or_else(|| OracleError::Provider("scaled oracle price division failed".to_string()))
    }
}
