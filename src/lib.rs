//! Chainlink-style oracle state tracking over `evm-fork-cache`.
//!
//! Events are treated as low-latency hints. Proxy reads through
//! `latestRoundData()` are the authoritative state and should be used for
//! startup, periodic reconciliation, and repair after gaps or reorgs.

#[cfg(feature = "aave")]
mod aave;
mod cache_reader;
mod config;
mod error;
mod events;
mod feed;
mod layout;
mod overlay;
mod price;
mod provider;
mod reactive;
mod registry;
mod runtime;
mod signal;
mod state;
mod storage;
mod tracker;
mod types;

#[cfg(feature = "aave")]
pub use aave::{AaveAsset, AaveV3OracleAdapter};
pub use cache_reader::{
    EvmCacheChainlinkReader, OracleAdapter, OracleAdapterBuildReport, OracleAdapterBuilder,
    OracleAdapterFeedSkip, OracleAdapterSkipReason,
};
pub use config::{FeedConfig, StalenessPolicy};
pub use error::{ChainlinkEventDecodeError, OracleError, OracleSignalDecodeError};
pub use events::{
    ANSWER_UPDATED_TOPIC, AnswerUpdated, NEW_ROUND_TOPIC, NewRound, OCR1_NEW_TRANSMISSION_TOPIC,
    OCR2_NEW_TRANSMISSION_TOPIC, Ocr1NewTransmission, Ocr2NewTransmission, decode_answer_updated,
    decode_new_round, decode_ocr1_new_transmission, decode_ocr2_new_transmission,
};
pub use feed::{Feed, FeedAccess, FeedBuilder, FeedBuilderState, FeedReady};
pub use layout::{
    AggregatorLayout, AggregatorLayoutConfidence, AggregatorLayoutEvidence,
    classify_type_and_version,
};
pub use overlay::OracleReadOverlay;
pub use price::{CheckedPrice, OraclePrice, PricePolicy};
pub use provider::{ChainlinkFeedProvider, OracleBlockRef, ProviderFuture};
pub use reactive::OracleReactiveHandler;
pub use registry::{AggregatorChange, FeedId, FeedRegistration, FeedSource, OracleRegistry};
pub use runtime::{ChainlinkFeed, OracleRuntime, OracleRuntimeBuilder};
pub use signal::{
    ORACLE_LEGACY_ANSWER_UPDATED_KIND, ORACLE_SIGNAL_NAMESPACE, OracleSignal, OracleSignalKind,
};
pub use state::{
    FeedMetadata, FeedStatus, OracleSnapshot, OracleSnapshotStatus, RoundData, SnapshotSource,
};
pub use storage::{
    ChainlinkOcr1StorageAdapter, ChainlinkOcr2StorageAdapter, Ocr1TransmissionStorageUpdate,
    Ocr2TransmissionStorageUpdate, OracleStorageAdapter, OracleStorageEffect, OracleStorageError,
    OracleStorageSync,
};
pub use tracker::{
    OracleAggregatorChanged, OracleHookEvent, OraclePriceConfirmed, OraclePriceCorrected,
    OraclePriceStale, OraclePriceUpdate, OracleReconciler, OracleReconciliationRequest,
    OracleReconciliationResult, OracleTracker, OracleUpdate, ReconcileReport,
};
pub use types::{AssetId, Denomination, TokenAmount, ValuedAmount};
