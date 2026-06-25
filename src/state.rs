use alloy_primitives::{Address, B256, I256, U256};

use crate::{FeedId, StalenessPolicy};

/// Static metadata read from a Chainlink-compatible proxy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedMetadata {
    /// Number of decimals used by the answer.
    pub decimals: u8,
    /// Human-readable feed description.
    pub description: String,
    /// Feed contract version.
    pub version: U256,
}

/// Round tuple returned by `latestRoundData()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoundData {
    /// Proxy or aggregator round id, depending on the source.
    pub round_id: U256,
    /// Signed answer.
    pub answer: I256,
    /// `startedAt` timestamp.
    pub started_at: u64,
    /// `updatedAt` timestamp.
    pub updated_at: u64,
    /// `answeredInRound`.
    pub answered_in_round: U256,
}

/// Classified usability of a feed snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FeedStatus {
    /// Snapshot is usable under its configured policy.
    Fresh,
    /// Snapshot is older than the configured max age.
    Stale {
        /// Observed age in seconds.
        age_secs: u64,
        /// Configured max age in seconds.
        max_age_secs: u64,
    },
    /// Round data is incomplete.
    IncompleteRound,
    /// Answer is disallowed by policy.
    InvalidAnswer,
    /// State is not currently trusted and needs reconciliation.
    Unknown,
}

/// Lifecycle confidence for an oracle snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OracleSnapshotStatus {
    /// Snapshot was derived from an event and still needs proxy reconciliation.
    EventPending,
    /// Snapshot was confirmed by an authoritative proxy read.
    Confirmed,
    /// Snapshot was corrected by an authoritative proxy read after an event mismatch.
    Corrected,
    /// Snapshot was read from the proxy but is stale under the feed policy.
    Stale,
    /// Snapshot was read from the proxy but is invalid under the feed policy.
    Invalid,
    /// Snapshot is not currently trusted.
    Unknown,
}

/// Source that produced an oracle snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SnapshotSource {
    /// Snapshot came from an authoritative proxy read.
    ProxyRead,
    /// Snapshot came from an aggregator event and must be reconciled.
    Event,
    /// Snapshot was intentionally marked untrusted.
    Unknown,
}

/// Typed current oracle state for one registered proxy feed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OracleSnapshot {
    /// Registered feed id.
    pub id: FeedId,
    /// User-facing proxy address.
    pub proxy: Address,
    /// Best-known current aggregator address.
    pub aggregator: Option<Address>,
    /// Feed metadata read from the proxy.
    pub metadata: FeedMetadata,
    /// Current round data.
    pub round: RoundData,
    /// Current status.
    pub status: FeedStatus,
    /// Snapshot lifecycle confidence.
    pub confidence: OracleSnapshotStatus,
    /// State source.
    pub source: SnapshotSource,
    /// Block number associated with the observation, when known.
    pub block_number: Option<u64>,
    /// Block hash associated with the observation, when known.
    pub block_hash: Option<B256>,
    requires_reconciliation: bool,
}

impl OracleSnapshot {
    pub(crate) fn proxy_read(
        id: FeedId,
        proxy: Address,
        aggregator: Option<Address>,
        metadata: FeedMetadata,
        round: RoundData,
        now_timestamp: u64,
        staleness: &StalenessPolicy,
    ) -> Self {
        let status = classify_round(&round, now_timestamp, staleness);
        let confidence = confidence_from_feed_status(&status);
        Self {
            id,
            proxy,
            aggregator,
            metadata,
            round,
            status,
            confidence,
            source: SnapshotSource::ProxyRead,
            block_number: None,
            block_hash: None,
            requires_reconciliation: false,
        }
    }

    pub(crate) fn event(input: EventSnapshotInput<'_>) -> Self {
        let status = classify_round(&input.round, input.now_timestamp, input.staleness);
        Self {
            id: input.id,
            proxy: input.proxy,
            aggregator: input.aggregator,
            metadata: input.metadata,
            round: input.round,
            status,
            confidence: OracleSnapshotStatus::EventPending,
            source: SnapshotSource::Event,
            block_number: input.block_number,
            block_hash: input.block_hash,
            requires_reconciliation: true,
        }
    }

    pub(crate) fn mark_unknown(&mut self) {
        self.status = FeedStatus::Unknown;
        self.confidence = OracleSnapshotStatus::Unknown;
        self.source = SnapshotSource::Unknown;
        self.requires_reconciliation = true;
    }

    pub(crate) fn set_confidence(&mut self, confidence: OracleSnapshotStatus) {
        self.confidence = confidence;
        self.requires_reconciliation = matches!(
            confidence,
            OracleSnapshotStatus::EventPending | OracleSnapshotStatus::Unknown
        );
    }

    /// Return true when callers should repair this snapshot via proxy read.
    pub fn requires_reconciliation(&self) -> bool {
        self.requires_reconciliation
    }
}

pub(crate) struct EventSnapshotInput<'a> {
    pub(crate) id: FeedId,
    pub(crate) proxy: Address,
    pub(crate) aggregator: Option<Address>,
    pub(crate) metadata: FeedMetadata,
    pub(crate) round: RoundData,
    pub(crate) now_timestamp: u64,
    pub(crate) staleness: &'a StalenessPolicy,
    pub(crate) block_number: Option<u64>,
    pub(crate) block_hash: Option<B256>,
}

pub(crate) fn classify_round(
    round: &RoundData,
    now_timestamp: u64,
    staleness: &StalenessPolicy,
) -> FeedStatus {
    if round.updated_at == 0 || round.answered_in_round < round.round_id {
        return FeedStatus::IncompleteRound;
    }

    if !staleness.allows_zero_or_negative_answer() && round.answer <= I256::ZERO {
        return FeedStatus::InvalidAnswer;
    }

    if let Some(max_age_secs) = staleness.max_age_secs() {
        let age_secs = now_timestamp.saturating_sub(round.updated_at);
        if age_secs > max_age_secs {
            return FeedStatus::Stale {
                age_secs,
                max_age_secs,
            };
        }
    }

    FeedStatus::Fresh
}

fn confidence_from_feed_status(status: &FeedStatus) -> OracleSnapshotStatus {
    match status {
        FeedStatus::Fresh => OracleSnapshotStatus::Confirmed,
        FeedStatus::Stale { .. } => OracleSnapshotStatus::Stale,
        FeedStatus::IncompleteRound | FeedStatus::InvalidAnswer => OracleSnapshotStatus::Invalid,
        FeedStatus::Unknown => OracleSnapshotStatus::Unknown,
    }
}
