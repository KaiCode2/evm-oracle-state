use evm_fork_cache::reactive::HookSignal;

use crate::{
    AggregatorChange, OracleHookEvent, OraclePriceConfirmed, OraclePriceCorrected,
    OraclePriceStale, OraclePriceUpdate, OracleSignalDecodeError, OracleUpdate,
};

/// Namespace used by every oracle hook signal.
pub const ORACLE_SIGNAL_NAMESPACE: &str = "evm-oracle-state";

/// Legacy event-update hook kind retained for compatibility.
pub const ORACLE_LEGACY_ANSWER_UPDATED_KIND: &str = "oracle.answer_updated";

/// Strongly typed oracle hook signal kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OracleSignalKind {
    /// Immediate event-derived price update.
    PriceUpdate,
    /// Event price matched the authoritative proxy read.
    PriceConfirmed,
    /// Event price was corrected by the authoritative proxy read.
    PriceCorrected,
    /// Authoritative proxy read was stale under policy.
    PriceStale,
    /// Proxy now points at a different aggregator.
    AggregatorChanged,
}

impl OracleSignalKind {
    /// Hook signal kind string.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PriceUpdate => "oracle.price_update",
            Self::PriceConfirmed => "oracle.price_confirmed",
            Self::PriceCorrected => "oracle.price_corrected",
            Self::PriceStale => "oracle.price_stale",
            Self::AggregatorChanged => "oracle.aggregator_changed",
        }
    }

    /// Parse an oracle hook signal kind.
    pub fn parse(kind: &str) -> Option<Self> {
        match kind {
            "oracle.price_update" => Some(Self::PriceUpdate),
            "oracle.price_confirmed" => Some(Self::PriceConfirmed),
            "oracle.price_corrected" => Some(Self::PriceCorrected),
            "oracle.price_stale" => Some(Self::PriceStale),
            "oracle.aggregator_changed" => Some(Self::AggregatorChanged),
            _ => None,
        }
    }
}

/// Borrowed typed view over an oracle hook signal payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OracleSignal<'a> {
    /// Immediate event-derived price update.
    PriceUpdate(&'a OraclePriceUpdate),
    /// Event price matched the authoritative proxy read.
    PriceConfirmed(&'a OraclePriceConfirmed),
    /// Event price was corrected by the authoritative proxy read.
    PriceCorrected(&'a OraclePriceCorrected),
    /// Authoritative proxy read was stale under policy.
    PriceStale(&'a OraclePriceStale),
    /// Proxy now points at a different aggregator.
    AggregatorChanged(&'a AggregatorChange),
}

impl<'a> OracleSignal<'a> {
    /// Decode a generic reactive hook into a typed oracle signal.
    pub fn from_hook(signal: &'a HookSignal) -> Result<Option<Self>, OracleSignalDecodeError> {
        if signal.namespace.as_ref() != ORACLE_SIGNAL_NAMESPACE {
            return Ok(None);
        }

        let Some(kind) = OracleSignalKind::parse(signal.kind.as_ref()) else {
            return Ok(None);
        };
        let payload =
            signal
                .payload
                .as_ref()
                .ok_or_else(|| OracleSignalDecodeError::MissingPayload {
                    kind: signal.kind.to_string(),
                })?;

        if let Some(event) = payload.downcast_ref::<OracleHookEvent>() {
            return event
                .as_signal()
                .ok_or_else(|| OracleSignalDecodeError::WrongPayloadType {
                    kind: signal.kind.to_string(),
                })
                .map(Some);
        }

        match kind {
            OracleSignalKind::PriceUpdate => payload
                .downcast_ref::<OraclePriceUpdate>()
                .map(Self::PriceUpdate),
            OracleSignalKind::PriceConfirmed => payload
                .downcast_ref::<OraclePriceConfirmed>()
                .map(Self::PriceConfirmed),
            OracleSignalKind::PriceCorrected => payload
                .downcast_ref::<OraclePriceCorrected>()
                .map(Self::PriceCorrected),
            OracleSignalKind::PriceStale => payload
                .downcast_ref::<OraclePriceStale>()
                .map(Self::PriceStale),
            OracleSignalKind::AggregatorChanged => payload
                .downcast_ref::<AggregatorChange>()
                .map(Self::AggregatorChanged),
        }
        .ok_or_else(|| OracleSignalDecodeError::WrongPayloadType {
            kind: signal.kind.to_string(),
        })
        .map(Some)
    }

    /// Own this borrowed signal as an `OracleHookEvent`.
    pub fn to_event(self) -> OracleHookEvent {
        match self {
            Self::PriceUpdate(update) => OracleHookEvent::PriceUpdate(update.clone()),
            Self::PriceConfirmed(confirmed) => OracleHookEvent::PriceConfirmed(confirmed.clone()),
            Self::PriceCorrected(corrected) => OracleHookEvent::PriceCorrected(corrected.clone()),
            Self::PriceStale(stale) => OracleHookEvent::PriceStale(stale.clone()),
            Self::AggregatorChanged(change) => OracleHookEvent::AggregatorChanged(change.clone()),
        }
    }
}

impl OracleHookEvent {
    /// Return this event's signal kind.
    pub fn signal_kind(&self) -> OracleSignalKind {
        match self {
            Self::PriceUpdate(_) => OracleSignalKind::PriceUpdate,
            Self::PriceConfirmed(_) => OracleSignalKind::PriceConfirmed,
            Self::PriceCorrected(_) => OracleSignalKind::PriceCorrected,
            Self::PriceStale(_) => OracleSignalKind::PriceStale,
            Self::AggregatorChanged(_) => OracleSignalKind::AggregatorChanged,
        }
    }

    /// Return this event's hook kind string.
    pub fn kind_str(&self) -> &'static str {
        self.signal_kind().as_str()
    }

    pub(crate) fn as_signal(&self) -> Option<OracleSignal<'_>> {
        match self {
            Self::PriceUpdate(update) => Some(OracleSignal::PriceUpdate(update)),
            Self::PriceConfirmed(confirmed) => Some(OracleSignal::PriceConfirmed(confirmed)),
            Self::PriceCorrected(corrected) => Some(OracleSignal::PriceCorrected(corrected)),
            Self::PriceStale(stale) => Some(OracleSignal::PriceStale(stale)),
            Self::AggregatorChanged(change) => Some(OracleSignal::AggregatorChanged(change)),
        }
    }
}

impl OracleUpdate {
    /// Legacy hook kind for this payload.
    pub const KIND: &'static str = ORACLE_LEGACY_ANSWER_UPDATED_KIND;
}
