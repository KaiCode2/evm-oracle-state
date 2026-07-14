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

/// Lifecycle readiness of a registered oracle feed.
///
/// This is a feed/runtime readiness signal, not a price freshness signal. A
/// feed can be [`Ready`](Self::Ready) while its current round is
/// [`Stale`](OracleRoundStatus::Stale).
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum OracleFeedStatus {
    /// Registration was requested but has not been initialized.
    #[default]
    Pending,
    /// Metadata or registration exists, but usable state/warmup is incomplete.
    Cold,
    /// Feed is structurally usable.
    Ready,
    /// Feed remains registered, but a repair, reconciliation, source, layout, or warmup path failed.
    Degraded,
    /// Feed was explicitly disabled by the caller/runtime.
    Disabled,
    /// Feed source/layout/protocol is not supported.
    Unsupported,
}

/// Classified usability of an oracle round under the feed policy.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OracleRoundStatus {
    /// Round is usable under its configured policy.
    Fresh,
    /// Round is older than the configured max age.
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
    /// Round validity is not currently known.
    Unknown,
}

/// Reconciliation lifecycle for an oracle value.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OracleValueStatus {
    /// Snapshot was derived from an event and still needs proxy reconciliation.
    EventPending,
    /// Value came from the authoritative source path without needing event correction.
    ///
    /// This does not imply the round is fresh; inspect [`OracleRoundStatus`].
    Confirmed,
    /// Event-derived value was superseded by an authoritative source read.
    ///
    /// This does not imply the corrected round is fresh; inspect [`OracleRoundStatus`].
    Corrected,
    /// Value/cache path is not trusted as final and needs authoritative repair.
    ///
    /// Payloads may still carry a best-available diagnostic value, but
    /// [`crate::PricePolicy`] rejects this status by default.
    RequiresRepair,
    /// Value lifecycle is not currently known.
    Unknown,
}

/// Source that produced an oracle value.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OracleValueSource {
    /// Value came from the authoritative proxy/source path.
    Proxy,
    /// Value came from an oracle event and must be reconciled before final trust.
    Event,
    /// Value was derived from one or more dependency feeds.
    Derived,
    /// Value came from a caller-controlled mock override.
    Mock,
    /// Value source is not currently trusted or known.
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
    /// Current round validity status.
    pub round_status: OracleRoundStatus,
    /// Current value reconciliation lifecycle.
    pub value_status: OracleValueStatus,
    /// State source.
    pub source: OracleValueSource,
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
        let round_status = classify_round(&round, now_timestamp, staleness);
        Self {
            id,
            proxy,
            aggregator,
            metadata,
            round,
            round_status,
            value_status: OracleValueStatus::Confirmed,
            source: OracleValueSource::Proxy,
            block_number: None,
            block_hash: None,
            requires_reconciliation: false,
        }
    }

    pub(crate) fn event(input: EventSnapshotInput<'_>) -> Self {
        let round_status = classify_round(&input.round, input.now_timestamp, input.staleness);
        Self {
            id: input.id,
            proxy: input.proxy,
            aggregator: input.aggregator,
            metadata: input.metadata,
            round: input.round,
            round_status,
            value_status: input.value_status,
            source: input.source,
            block_number: input.block_number,
            block_hash: input.block_hash,
            requires_reconciliation: matches!(
                input.value_status,
                OracleValueStatus::EventPending
                    | OracleValueStatus::RequiresRepair
                    | OracleValueStatus::Unknown
            ),
        }
    }

    pub(crate) fn mark_unknown(&mut self) {
        self.round_status = OracleRoundStatus::Unknown;
        self.value_status = OracleValueStatus::RequiresRepair;
        self.source = OracleValueSource::Unknown;
        self.requires_reconciliation = true;
    }

    pub(crate) fn set_value_status(&mut self, value_status: OracleValueStatus) {
        self.value_status = value_status;
        self.requires_reconciliation = matches!(
            value_status,
            OracleValueStatus::EventPending
                | OracleValueStatus::RequiresRepair
                | OracleValueStatus::Unknown
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
    pub(crate) value_status: OracleValueStatus,
    pub(crate) source: OracleValueSource,
}

pub(crate) fn classify_round(
    round: &RoundData,
    now_timestamp: u64,
    staleness: &StalenessPolicy,
) -> OracleRoundStatus {
    if round.updated_at == 0 || round.answered_in_round < round.round_id {
        return OracleRoundStatus::IncompleteRound;
    }

    if !staleness.allows_zero_or_negative_answer() && round.answer <= I256::ZERO {
        return OracleRoundStatus::InvalidAnswer;
    }

    if let Some(max_age_secs) = staleness.max_age_secs() {
        let age_secs = now_timestamp.saturating_sub(round.updated_at);
        if age_secs > max_age_secs {
            return OracleRoundStatus::Stale {
                age_secs,
                max_age_secs,
            };
        }
    }

    OracleRoundStatus::Fresh
}
