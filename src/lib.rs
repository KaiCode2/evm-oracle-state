//! Chainlink-style oracle state tracking over `evm-fork-cache`.
//!
//! Events are treated as low-latency hints. Proxy reads through
//! `latestRoundData()` are the authoritative state and should be used for
//! startup, periodic reconciliation, and repair after gaps or reorgs.

#[cfg(feature = "aave")]
mod aave;
mod adapter;
mod cache_reader;
mod code;
mod config;
mod error;
#[cfg(feature = "euler")]
mod euler;
mod events;
mod feed;
mod layout;
#[cfg(feature = "morpho")]
mod morpho;
mod overlay;
mod price;
mod provider;
#[cfg(feature = "pyth")]
mod pyth;
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
pub use adapter::{
    AdapterFuture, OracleAdapterId, OracleAdapterPlugin, OracleDiscoveredFeed,
    OracleDiscoveryContext, OracleDiscoveryReport, OracleSourceDescriptor,
    OracleTransformDescriptor,
};
pub use cache_reader::{
    EvmCacheChainlinkReader, OracleAdapter, OracleAdapterBuildReport, OracleAdapterBuilder,
    OracleAdapterFeedSkip, OracleAdapterSkipReason,
};
pub use code::{
    OracleCodeInstall, OracleCodeInstallError, OracleCodeInstallMode, OracleCodeMismatch,
    OracleCodeRegistry, OracleCodeSeed, OracleCodeUnverifiable, OracleCodeWarmupPolicy,
    OracleCodeWarmupReport,
};
pub use config::{FeedConfig, StalenessPolicy};
pub use error::{ChainlinkEventDecodeError, OracleError, OracleSignalDecodeError};
#[cfg(feature = "euler")]
pub use euler::{EulerOracleAdapter, EulerQuotePair};
pub use events::{
    ANSWER_UPDATED_TOPIC, AnswerUpdated, NEW_ROUND_TOPIC, NewRound, OCR1_NEW_TRANSMISSION_TOPIC,
    OCR2_NEW_TRANSMISSION_TOPIC, Ocr1NewTransmission, Ocr2NewTransmission, decode_answer_updated,
    decode_new_round, decode_ocr1_new_transmission, decode_ocr2_new_transmission,
};
#[cfg(feature = "pyth")]
pub use events::{
    PYTH_PRICE_FEED_UPDATE_TOPIC, PythPriceFeedUpdate, decode_pyth_price_feed_update,
};
pub use feed::{Feed, FeedAccess, FeedBuilder, FeedBuilderState, FeedReady};
pub use layout::{
    AggregatorLayout, AggregatorLayoutConfidence, AggregatorLayoutEvidence,
    classify_type_and_version,
};
#[cfg(feature = "morpho")]
pub use morpho::{MorphoBlueMarket, MorphoBlueOracleAdapter, MorphoMarketParams};
pub use overlay::OracleReadOverlay;
pub use price::{CheckedPrice, OraclePrice, PricePolicy};
pub use provider::{ChainlinkFeedProvider, OracleBlockRef, ProviderFuture};
#[cfg(feature = "pyth")]
pub use pyth::{PythFeed, PythOracleAdapter, PythReactiveHandler};
pub use reactive::OracleReactiveHandler;
pub use registry::{
    AggregatorChange, EulerQuoteLeg, FeedId, FeedRegistration, FeedSource, MorphoChainlinkFeed,
    MorphoChainlinkFeedRole, OracleDependency, OracleRegistry,
};
pub use runtime::{
    ChainlinkFeed, OracleCacheRuntimeBuildReport, OracleCacheRuntimeBuilder,
    OracleColdStartWarmupReport, OracleReactiveInstallReport, OracleReactiveRefreshReport,
    OracleReactiveUninstallReport, OracleRuntime, OracleRuntimeBuilder,
    OracleRuntimeMutationReport, OracleStorageWarmupFailure, OracleStorageWarmupMode,
    OracleStorageWarmupReport,
};
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
