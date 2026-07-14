use thiserror::Error;

use crate::{OracleAdapterId, OracleCodePolicyViolation, OracleFeedSkip, PricePolicyViolation};

/// Errors returned by Chainlink event decoding.
#[non_exhaustive]
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ChainlinkEventDecodeError {
    /// The log did not contain the expected event signature topic.
    #[error("wrong event topic: expected {expected:?}, got {actual:?}")]
    WrongTopic {
        /// Expected topic0.
        expected: alloy_primitives::B256,
        /// Actual topic0, if present.
        actual: Option<alloy_primitives::B256>,
    },
    /// The log had the wrong number of topics.
    #[error("wrong topic count: expected {expected}, got {actual}")]
    WrongTopicCount {
        /// Expected topic count.
        expected: usize,
        /// Actual topic count.
        actual: usize,
    },
    /// The data payload had the wrong byte length.
    #[error("wrong data length: expected {expected}, got {actual}")]
    WrongDataLength {
        /// Expected byte length.
        expected: usize,
        /// Actual byte length.
        actual: usize,
    },
    /// A uint256 event field did not fit into the crate's typed Rust field.
    #[error("event field `{field}` value {value} does not fit in u64")]
    Uint64Overflow {
        /// Field name.
        field: &'static str,
        /// Encoded value.
        value: alloy_primitives::U256,
    },
    /// ABI decoding failed for a dynamic or typed event payload.
    #[error("failed to decode event `{event}`: {message}")]
    AbiDecode {
        /// Event name.
        event: &'static str,
        /// Decoder error message.
        message: String,
    },
}

/// Errors returned by registry, reconciliation, and report application.
#[non_exhaustive]
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum OracleError {
    /// A chain/provider read failed.
    ///
    /// This covers only genuine on-chain read failures: RPC transport errors,
    /// reverted or undecodable view calls (`call_sol`), and
    /// [`crate::EvmCacheChainlinkReader`] read failures. Programming errors,
    /// policy rejections, and unsupported-source classifications use the
    /// dedicated variants below.
    #[error("provider read failed: {0}")]
    Provider(String),
    /// Event or typed-field decode failed.
    ///
    /// Covers Chainlink log decoding and value-range failures where a chain
    /// value does not fit the crate's typed representation (for example a
    /// `latestRoundData()` timestamp that overflows `u64`, reported as
    /// [`ChainlinkEventDecodeError::Uint64Overflow`]).
    #[error(transparent)]
    Decode(#[from] ChainlinkEventDecodeError),
    /// Hook signal decode failed.
    #[error(transparent)]
    SignalDecode(#[from] OracleSignalDecodeError),
    /// Feed id already exists.
    #[error("feed id `{0}` is already registered")]
    DuplicateFeedId(String),
    /// Proxy already exists.
    #[error("proxy `{0:?}` is already registered")]
    DuplicateProxy(alloy_primitives::Address),
    /// A feed was not found.
    #[error("feed was not found")]
    FeedNotFound,
    /// A configured price policy rejected a value or result.
    ///
    /// Covers [`crate::PricePolicy`] rejections (stale, pending, invalid, or
    /// wrong-denomination prices) and valuation failures while producing a
    /// [`crate::ValuedAmount`]. The payload classifies the exact violation.
    /// Code warmup policy failures use [`OracleError::CodePolicy`].
    #[error("policy rejected: {0}")]
    Policy(#[from] PricePolicyViolation),
    /// The configured [`crate::OracleCodeWarmupPolicy`] rejected a code
    /// warmup report.
    ///
    /// The boxed payload lists exactly the [`crate::OracleCodeWarmupReport`]
    /// entries that violated the enforced policy toggles.
    #[error("{0}")]
    CodePolicy(Box<OracleCodePolicyViolation>),
    /// Caller or programming error.
    ///
    /// The payload classifies the failure: a request that references an
    /// adapter that is not installed
    /// ([`OracleConfigError::AdapterNotInstalled`]), a system clock before
    /// the UNIX epoch ([`OracleConfigError::Clock`]), a malformed or
    /// mismatched overlay/reconciliation request
    /// ([`OracleConfigError::InvalidRequest`]), or another invalid builder
    /// configuration or internal invariant violation
    /// ([`OracleConfigError::Other`]).
    #[error("configuration error: {0}")]
    Config(#[from] OracleConfigError),
    /// A source family, layout, or value shape was recognized but is not
    /// supported by this crate.
    ///
    /// Covers unsupported adapter variants discovered at runtime (for example
    /// an unknown Euler adapter name, a nested `CrossAdapter`, or a
    /// base/quote direction mismatch) and chain values that cannot be
    /// represented in the requested ABI or fixed-point shape.
    #[error("unsupported source or value: {0}")]
    Unsupported(String),
    /// An adapter feed skip was surfaced as a fail-fast build error.
    ///
    /// The boxed payload carries the complete skip description (feed
    /// id/label, proxy, and classified [`crate::OracleAdapterSkipReason`]).
    /// Use the `build_report` methods to receive skips as data instead of an
    /// error.
    #[error("{0}")]
    FeedSkipped(Box<OracleFeedSkip>),
    /// A reactive-engine operation failed.
    ///
    /// Covers handler install/uninstall, subscriber interest registration,
    /// and batch ingestion errors surfaced through the
    /// [`evm_fork_cache::reactive`] runtime.
    #[error("reactive engine error: {0}")]
    Reactive(String),
}

/// Errors returned when decoding oracle hook signals.
#[non_exhaustive]
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum OracleSignalDecodeError {
    /// The signal kind is known but the payload is absent.
    #[error("oracle signal `{kind}` did not include a typed payload")]
    MissingPayload {
        /// Signal kind.
        kind: String,
    },
    /// The signal kind is known but the payload type does not match.
    #[error("oracle signal `{kind}` had an unexpected payload type")]
    WrongPayloadType {
        /// Signal kind.
        kind: String,
    },
}

/// Caller or programming errors carried by [`OracleError::Config`].
#[non_exhaustive]
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum OracleConfigError {
    /// A request referenced an oracle adapter that is not installed in the
    /// runtime.
    #[error("oracle adapter `{adapter}` is not installed in this runtime")]
    AdapterNotInstalled {
        /// Adapter id the request referenced.
        adapter: OracleAdapterId,
    },
    /// The system clock could not produce a UNIX timestamp.
    #[error("system clock is before the UNIX epoch: {detail}")]
    Clock {
        /// Stringified [`std::time::SystemTimeError`]. Kept as a string
        /// because the source error is neither `Clone` nor `PartialEq`.
        detail: String,
    },
    /// A malformed or mismatched overlay or reconciliation request, for
    /// example calldata that does not decode or a quote request that does
    /// not match any registered pair.
    #[error("{0}")]
    InvalidRequest(String),
    /// Any other invalid builder configuration or internal invariant
    /// violation.
    #[error("{0}")]
    Other(String),
}

/// Classify a `SystemTime::duration_since(UNIX_EPOCH)` failure.
pub(crate) fn clock_error(error: std::time::SystemTimeError) -> OracleError {
    OracleError::Config(OracleConfigError::Clock {
        detail: error.to_string(),
    })
}
