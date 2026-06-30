use std::time::{SystemTime, UNIX_EPOCH};

use alloy_primitives::{Address, U256};
use evm_fork_cache::cache::EvmCache;

use crate::{
    AssetId, Denomination, EvmCacheChainlinkReader, Feed, FeedConfig, FeedId, FeedMetadata,
    FeedSource, OracleAdapterBuildReport, OracleAdapterFeedSkip, OracleAdapterSkipReason,
    OracleError, OracleRegistry, OracleTracker, RoundData, StalenessPolicy,
    cache_reader::AavePriceCapStableSource,
};

/// Declarative Aave V3 asset source registration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AaveAsset {
    asset: Address,
    id: Option<FeedId>,
    label: Option<String>,
    base: Option<AssetId>,
    quote: Option<Denomination>,
    staleness: StalenessPolicy,
}

impl AaveAsset {
    /// Create an Aave asset registration for `AaveOracle.getSourceOfAsset(asset)`.
    pub fn new(asset: Address) -> Self {
        Self {
            asset,
            id: None,
            label: None,
            base: None,
            quote: None,
            staleness: StalenessPolicy::default(),
        }
    }

    /// Return the Aave asset address.
    pub fn asset(&self) -> Address {
        self.asset
    }

    /// Set a stable feed id for the discovered source.
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(FeedId::new(id));
        self
    }

    /// Set a stable feed id for the discovered source.
    pub fn feed_id(mut self, id: FeedId) -> Self {
        self.id = Some(id);
        self
    }

    /// Set a human-readable feed label.
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Set the base asset.
    pub fn base(mut self, base: AssetId) -> Self {
        self.base = Some(base);
        self
    }

    /// Set the quote denomination.
    pub fn quote(mut self, quote: Denomination) -> Self {
        self.quote = Some(quote);
        self
    }

    /// Set a max-age staleness policy.
    pub fn max_age_secs(mut self, max_age_secs: u64) -> Self {
        self.staleness = StalenessPolicy::max_age(max_age_secs);
        self
    }

    /// Set the full staleness policy.
    pub fn staleness(mut self, staleness: StalenessPolicy) -> Self {
        self.staleness = staleness;
        self
    }

    fn feed_for_source(&self, source: Address) -> Feed {
        let mut feed = Feed::proxy(source);
        if let Some(id) = self.id {
            feed = feed.feed_id(id);
        }
        if let Some(label) = &self.label {
            feed = feed.label(label.clone());
        }
        if let Some(base) = &self.base {
            feed = feed.base(base.clone());
        }
        if let Some(quote) = &self.quote {
            feed = feed.quote(quote.clone());
        }
        feed.staleness(self.staleness)
    }

    fn config_for_source(&self, source: Address) -> Result<FeedConfig, OracleError> {
        self.feed_for_source(source).try_into()
    }
}

/// Cache-backed Aave V3 oracle source discovery adapter.
#[derive(Clone, Debug, Default)]
pub struct AaveV3OracleAdapter {
    oracle: Address,
    assets: Vec<AaveAsset>,
    now_timestamp: Option<u64>,
}

impl AaveV3OracleAdapter {
    /// Create an adapter for an Aave V3 `AaveOracle`.
    pub fn new(oracle: Address) -> Self {
        Self {
            oracle,
            assets: Vec::new(),
            now_timestamp: None,
        }
    }

    /// Return the Aave oracle address.
    pub fn oracle(&self) -> Address {
        self.oracle
    }

    /// Return configured assets.
    pub fn assets(&self) -> &[AaveAsset] {
        &self.assets
    }

    /// Add one asset whose source should be discovered from the Aave oracle.
    pub fn asset(mut self, asset: AaveAsset) -> Self {
        self.assets.push(asset);
        self
    }

    /// Add multiple Aave assets.
    pub fn assets_iter(mut self, assets: impl IntoIterator<Item = AaveAsset>) -> Self {
        self.assets.extend(assets);
        self
    }

    /// Set a fixed timestamp for deterministic registration status classification.
    pub fn now_timestamp(mut self, now_timestamp: u64) -> Self {
        self.now_timestamp = Some(now_timestamp);
        self
    }

    /// Discover and register every compatible Aave source.
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
                "Aave asset {} at {:?} was skipped: {}",
                label, skipped.proxy, skipped.reason
            )));
        }
        Ok(report.tracker)
    }

    /// Discover compatible sources and report unsupported variants instead of panicking.
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

        let oracle = self.oracle;
        for asset in self.assets {
            if let Some(skip) = Self::register_asset(oracle, &reader, &mut registry, asset).await? {
                skipped.push(skip);
            }
        }

        Ok(OracleAdapterBuildReport {
            tracker: OracleTracker::new(registry),
            skipped,
        })
    }

    async fn register_asset(
        oracle: Address,
        reader: &EvmCacheChainlinkReader<'_>,
        registry: &mut OracleRegistry,
        asset: AaveAsset,
    ) -> Result<Option<OracleAdapterFeedSkip>, OracleError> {
        let source = match reader.read_aave_source(oracle, asset.asset()) {
            Ok(source) if source != Address::ZERO => source,
            Ok(_) => {
                return Ok(Some(aave_skip(
                    &asset,
                    asset.asset(),
                    "Aave oracle returned the zero source address",
                )));
            }
            Err(error) => {
                return Ok(Some(aave_skip(
                    &asset,
                    asset.asset(),
                    format!("getSourceOfAsset failed: {error}"),
                )));
            }
        };

        let feed = asset.feed_for_source(source);
        match reader.register_feed(registry, feed.clone()).await {
            Ok(_) => return Ok(None),
            Err(OracleError::Provider(chainlink_error)) => {
                return Self::try_register_price_cap_stable(
                    reader,
                    registry,
                    asset,
                    source,
                    chainlink_error,
                )
                .await;
            }
            Err(error) => return Err(error),
        }
    }

    async fn try_register_price_cap_stable(
        reader: &EvmCacheChainlinkReader<'_>,
        registry: &mut OracleRegistry,
        asset: AaveAsset,
        source: Address,
        chainlink_error: String,
    ) -> Result<Option<OracleAdapterFeedSkip>, OracleError> {
        let capped = match reader.read_aave_price_cap_stable(source) {
            Ok(capped) => capped,
            Err(error) => {
                return Ok(Some(aave_skip(
                    &asset,
                    source,
                    format!(
                        "source is neither Chainlink-compatible ({chainlink_error}) nor Aave PriceCapAdapterStable ({error})"
                    ),
                )));
            }
        };

        let registration_source =
            FeedSource::aave_price_cap_stable(source, capped.underlying_proxy, capped.price_cap);
        let (underlying_round, current_aggregator) = match read_underlying_chainlink(
            reader, &capped,
        ) {
            Ok(values) => values,
            Err(error) => {
                return Ok(Some(aave_skip(
                    &asset,
                    source,
                    format!(
                        "Aave PriceCapAdapterStable underlying Chainlink source is unsupported: {error}"
                    ),
                )));
            }
        };

        let mut seeded_round = underlying_round;
        seeded_round.answer = registration_source.normalize_answer(capped.latest_answer);
        let metadata = FeedMetadata {
            decimals: capped.decimals,
            description: capped.description,
            version: U256::ZERO,
        };

        registry
            .register_discovered_feed(
                reader,
                asset.config_for_source(source)?,
                registration_source,
                metadata,
                seeded_round,
                Some(current_aggregator),
            )
            .await?;

        Ok(None)
    }
}

fn read_underlying_chainlink(
    reader: &EvmCacheChainlinkReader<'_>,
    capped: &AavePriceCapStableSource,
) -> Result<(RoundData, Address), OracleError> {
    reader.read_decimals(capped.underlying_proxy)?;
    reader.read_description(capped.underlying_proxy)?;
    reader.read_version(capped.underlying_proxy)?;
    let round = reader.read_latest_round_data(capped.underlying_proxy)?;
    let aggregator = reader
        .read_aggregator(capped.underlying_proxy)?
        .ok_or_else(|| {
            OracleError::Provider("underlying aggregator() returned none".to_string())
        })?;
    Ok((round, aggregator))
}

fn aave_skip(
    asset: &AaveAsset,
    proxy: Address,
    reason: impl Into<String>,
) -> OracleAdapterFeedSkip {
    OracleAdapterFeedSkip {
        feed: asset.feed_for_source(proxy),
        proxy,
        reason: OracleAdapterSkipReason::UnsupportedAaveSource {
            error: reason.into(),
        },
    }
}

fn provider_error(error: impl std::fmt::Debug) -> OracleError {
    OracleError::Provider(format!("{error:?}"))
}
