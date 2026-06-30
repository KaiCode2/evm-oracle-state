use thiserror::Error;

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
#[derive(Debug, Error)]
pub enum OracleError {
    /// Provider read failed.
    #[error("provider read failed: {0}")]
    Provider(String),
    /// Event decode failed.
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

impl From<String> for OracleError {
    fn from(value: String) -> Self {
        Self::Provider(value)
    }
}

impl From<&str> for OracleError {
    fn from(value: &str) -> Self {
        Self::Provider(value.to_string())
    }
}
