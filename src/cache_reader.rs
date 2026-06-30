use std::{
    cell::RefCell,
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(feature = "aave")]
use alloy_primitives::I256;
use alloy_primitives::{Address, U256};
use alloy_sol_types::sol;
use evm_fork_cache::cache::EvmCache;

use crate::{
    ChainlinkFeedProvider, Feed, FeedConfig, FeedId, OracleError, OracleRegistry, OracleTracker,
    ProviderFuture, RoundData,
};

sol! {
    interface AggregatorProxyInterface {
        function latestRoundData() external view returns (
            uint80 roundId,
            int256 answer,
            uint256 startedAt,
            uint256 updatedAt,
            uint80 answeredInRound
        );
        function decimals() external view returns (uint8);
        function description() external view returns (string);
        function version() external view returns (uint256);
        function aggregator() external view returns (address);
        function typeAndVersion() external view returns (string);
    }
}

use AggregatorProxyInterface::{
    aggregatorCall, decimalsCall, descriptionCall, latestRoundDataCall, typeAndVersionCall,
    versionCall,
};

#[cfg(feature = "aave")]
sol! {
    interface AaveOracleInterface {
        function getSourceOfAsset(address asset) external view returns (address);
    }

    interface AavePriceCapAdapterStableInterface {
        function ASSET_TO_USD_AGGREGATOR() external view returns (address);
        function getPriceCap() external view returns (int256);
        function decimals() external view returns (uint8);
        function description() external view returns (string);
        function latestAnswer() external view returns (int256);
    }
}

#[cfg(feature = "aave")]
use AaveOracleInterface::getSourceOfAssetCall;
#[cfg(feature = "aave")]
use AavePriceCapAdapterStableInterface::{
    ASSET_TO_USD_AGGREGATORCall, decimalsCall as aaveDecimalsCall,
    descriptionCall as aaveDescriptionCall, getPriceCapCall, latestAnswerCall,
};

#[cfg(feature = "aave")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AavePriceCapStableSource {
    pub(crate) underlying_proxy: Address,
    pub(crate) price_cap: I256,
    pub(crate) decimals: u8,
    pub(crate) description: String,
    pub(crate) latest_answer: I256,
}

/// Chainlink view reader backed by an `evm-fork-cache` fork cache.
pub struct EvmCacheChainlinkReader<'a> {
    cache: RefCell<&'a mut EvmCache>,
}

impl<'a> EvmCacheChainlinkReader<'a> {
    /// Create a reader around an existing mutable EVM cache.
    pub fn new(cache: &'a mut EvmCache) -> Self {
        Self {
            cache: RefCell::new(cache),
        }
    }

    /// Register a typed feed into an existing oracle registry.
    pub async fn register_feed(
        &self,
        registry: &mut OracleRegistry,
        feed: Feed,
    ) -> Result<FeedId, OracleError> {
        let config = FeedConfig::try_from(feed)?;
        registry.register_chainlink_feed(self, config).await
    }

    /// Read `decimals()` from a Chainlink-compatible proxy.
    pub fn read_decimals(&self, proxy: Address) -> Result<u8, OracleError> {
        self.cache
            .borrow_mut()
            .call_sol(proxy, decimalsCall {})
            .map_err(provider_error)
    }

    /// Read `description()` from a Chainlink-compatible proxy.
    pub fn read_description(&self, proxy: Address) -> Result<String, OracleError> {
        self.cache
            .borrow_mut()
            .call_sol(proxy, descriptionCall {})
            .map_err(provider_error)
    }

    /// Read `version()` from a Chainlink-compatible proxy.
    pub fn read_version(&self, proxy: Address) -> Result<U256, OracleError> {
        self.cache
            .borrow_mut()
            .call_sol(proxy, versionCall {})
            .map_err(provider_error)
    }

    /// Read `latestRoundData()` from a Chainlink-compatible proxy.
    pub fn read_latest_round_data(&self, proxy: Address) -> Result<RoundData, OracleError> {
        let round = self
            .cache
            .borrow_mut()
            .call_sol(proxy, latestRoundDataCall {})
            .map_err(provider_error)?;
        Ok(RoundData {
            round_id: U256::from(round.roundId),
            answer: round.answer,
            started_at: u64_from_u256(round.startedAt, "startedAt")?,
            updated_at: u64_from_u256(round.updatedAt, "updatedAt")?,
            answered_in_round: U256::from(round.answeredInRound),
        })
    }

    /// Best-effort read of the proxy's current aggregator.
    pub fn read_aggregator(&self, proxy: Address) -> Result<Option<Address>, OracleError> {
        let aggregator = self
            .cache
            .borrow_mut()
            .call_sol(proxy, aggregatorCall {})
            .map_err(provider_error)?;
        Ok(Some(aggregator))
    }

    /// Best-effort read of an aggregator implementation `typeAndVersion()`.
    pub fn read_type_and_version(
        &self,
        aggregator: Address,
    ) -> Result<Option<String>, OracleError> {
        let type_and_version = self
            .cache
            .borrow_mut()
            .call_sol(aggregator, typeAndVersionCall {})
            .map_err(provider_error)?;
        Ok(Some(type_and_version))
    }

    #[cfg(feature = "aave")]
    pub(crate) fn read_aave_source(
        &self,
        oracle: Address,
        asset: Address,
    ) -> Result<Address, OracleError> {
        self.cache
            .borrow_mut()
            .call_sol(oracle, getSourceOfAssetCall { asset })
            .map_err(provider_error)
    }

    #[cfg(feature = "aave")]
    pub(crate) fn read_aave_price_cap_stable(
        &self,
        source: Address,
    ) -> Result<AavePriceCapStableSource, OracleError> {
        let underlying_proxy = self
            .cache
            .borrow_mut()
            .call_sol(source, ASSET_TO_USD_AGGREGATORCall {})
            .map_err(provider_error)?;
        let price_cap = self
            .cache
            .borrow_mut()
            .call_sol(source, getPriceCapCall {})
            .map_err(provider_error)?;
        let decimals = self
            .cache
            .borrow_mut()
            .call_sol(source, aaveDecimalsCall {})
            .map_err(provider_error)?;
        let description = self
            .cache
            .borrow_mut()
            .call_sol(source, aaveDescriptionCall {})
            .map_err(provider_error)?;
        let latest_answer = self
            .cache
            .borrow_mut()
            .call_sol(source, latestAnswerCall {})
            .map_err(provider_error)?;

        Ok(AavePriceCapStableSource {
            underlying_proxy,
            price_cap,
            decimals,
            description,
            latest_answer,
        })
    }
}

impl ChainlinkFeedProvider for EvmCacheChainlinkReader<'_> {
    fn decimals(&self, proxy: Address) -> ProviderFuture<'_, u8> {
        let result = self.read_decimals(proxy);
        Box::pin(async move { result })
    }

    fn description(&self, proxy: Address) -> ProviderFuture<'_, String> {
        let result = self.read_description(proxy);
        Box::pin(async move { result })
    }

    fn version(&self, proxy: Address) -> ProviderFuture<'_, U256> {
        let result = self.read_version(proxy);
        Box::pin(async move { result })
    }

    fn latest_round_data(&self, proxy: Address) -> ProviderFuture<'_, RoundData> {
        let result = self.read_latest_round_data(proxy);
        Box::pin(async move { result })
    }

    fn aggregator(&self, proxy: Address) -> ProviderFuture<'_, Option<Address>> {
        let result = self.read_aggregator(proxy).or(Ok(None));
        Box::pin(async move { result })
    }

    fn aggregator_type_and_version(
        &self,
        aggregator: Address,
    ) -> ProviderFuture<'_, Option<String>> {
        let result = self.read_type_and_version(aggregator).or(Ok(None));
        Box::pin(async move { result })
    }
}

/// Cache-native oracle adapter entry point.
pub struct OracleAdapter;

impl OracleAdapter {
    /// Start building a cache-backed oracle tracker.
    pub fn builder() -> OracleAdapterBuilder {
        OracleAdapterBuilder::default()
    }
}

/// Builder for cache-backed oracle tracker registration.
#[derive(Clone, Debug, Default)]
pub struct OracleAdapterBuilder {
    feeds: Vec<Feed>,
    now_timestamp: Option<u64>,
}

/// Best-effort cache-native oracle registration report.
#[derive(Clone, Debug)]
pub struct OracleAdapterBuildReport {
    /// Tracker containing every successfully registered feed.
    pub tracker: OracleTracker,
    /// Feeds that were skipped because the source could not be registered.
    pub skipped: Vec<OracleAdapterFeedSkip>,
}

/// Feed skipped during best-effort cache-native registration.
#[derive(Clone, Debug)]
pub struct OracleAdapterFeedSkip {
    /// Feed definition supplied by the caller.
    pub feed: Feed,
    /// Proxy/source address that failed registration.
    pub proxy: Address,
    /// Classified skip reason.
    pub reason: OracleAdapterSkipReason,
}

/// Classified reason for skipping a feed in best-effort registration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OracleAdapterSkipReason {
    /// The source did not expose the Chainlink proxy methods required by this crate.
    NotChainlinkCompatible {
        /// Underlying read/decode error.
        error: String,
    },
    /// The Aave oracle source variant is not supported by this MVP.
    UnsupportedAaveSource {
        /// Classified discovery or compatibility error.
        error: String,
    },
}

impl OracleAdapterBuilder {
    /// Add one typed feed registration.
    pub fn feed(mut self, feed: Feed) -> Self {
        self.feeds.push(feed);
        self
    }

    /// Add multiple typed feed registrations.
    pub fn feeds(mut self, feeds: impl IntoIterator<Item = Feed>) -> Self {
        self.feeds.extend(feeds);
        self
    }

    /// Set a fixed timestamp for deterministic registration status classification.
    pub fn now_timestamp(mut self, now_timestamp: u64) -> Self {
        self.now_timestamp = Some(now_timestamp);
        self
    }

    /// Register feeds from cache reads and return a typed tracker.
    pub async fn build(self, cache: &mut EvmCache) -> Result<OracleTracker, OracleError> {
        let report = self.build_report(cache).await?;
        if let Some(skipped) = report.skipped.first() {
            let label = skipped
                .feed
                .label()
                .map(str::to_string)
                .or_else(|| skipped.feed.id().map(|id| id.to_string()))
                .unwrap_or_else(|| "unknown".to_string());
            return Err(OracleError::Provider(format!(
                "feed {} at {:?} was skipped: {}",
                label, skipped.proxy, skipped.reason
            )));
        }
        Ok(report.tracker)
    }

    /// Register every compatible feed and return skipped feeds instead of failing fast.
    pub async fn build_report(
        self,
        cache: &mut EvmCache,
    ) -> Result<OracleAdapterBuildReport, OracleError> {
        let now_timestamp = match self.now_timestamp {
            Some(now_timestamp) => now_timestamp,
            None => SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(provider_error)?
                .as_secs(),
        };
        let reader = EvmCacheChainlinkReader::new(cache);
        let mut registry = OracleRegistry::new_at_timestamp(now_timestamp);
        let mut skipped = Vec::new();
        for feed in self.feeds {
            let proxy = feed.proxy().unwrap_or_default();
            let skipped_feed = feed.clone();
            match reader.register_feed(&mut registry, feed).await {
                Ok(_) => {}
                Err(OracleError::Provider(error)) => {
                    skipped.push(OracleAdapterFeedSkip {
                        feed: skipped_feed,
                        proxy,
                        reason: OracleAdapterSkipReason::NotChainlinkCompatible { error },
                    });
                }
                Err(error) => return Err(error),
            }
        }
        Ok(OracleAdapterBuildReport {
            tracker: OracleTracker::new(registry),
            skipped,
        })
    }
}

impl std::fmt::Display for OracleAdapterSkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotChainlinkCompatible { error } => {
                write!(f, "not Chainlink-compatible ({error})")
            }
            Self::UnsupportedAaveSource { error } => {
                write!(f, "unsupported Aave oracle source ({error})")
            }
        }
    }
}

fn u64_from_u256(value: U256, field: &'static str) -> Result<u64, OracleError> {
    u64::try_from(value).map_err(|_| OracleError::Provider(format!("{field} does not fit in u64")))
}

fn provider_error(error: impl std::fmt::Debug) -> OracleError {
    OracleError::Provider(format!("{error:?}"))
}
