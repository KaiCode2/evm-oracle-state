use std::time::{SystemTime, UNIX_EPOCH};

use alloy_primitives::{Address, I256, U256};
use evm_fork_cache::cache::EvmCache;

use crate::{
    AssetId, Denomination, EvmCacheChainlinkReader, Feed, FeedConfig, FeedId, FeedMetadata,
    FeedSource, OracleAdapterBuildReport, OracleAdapterFeedSkip, OracleAdapterSkipReason,
    OracleError, OracleRegistry, OracleTracker, RoundData, StalenessPolicy,
    cache_reader::{AaveFixedPriceSource, AaveRatioCapSource},
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
        if let Some(id) = self.id.clone() {
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
            return Err(OracleError::FeedSkipped(Box::new(
                crate::OracleFeedSkip::from_adapter_skip(skipped),
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
                .map_err(crate::error::clock_error)?
                .as_secs(),
        };
        let reader = EvmCacheChainlinkReader::new(cache);
        let mut registry = OracleRegistry::new_at_timestamp(now_timestamp);
        let mut skipped = Vec::new();

        let oracle = self.oracle;
        for asset in self.assets {
            if let Some(skip) =
                Self::register_asset(oracle, &reader, &mut registry, asset, now_timestamp).await?
            {
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
        now_timestamp: u64,
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
            Ok(_) => Ok(None),
            // Chainlink-probe read failures and value-range decode failures
            // both mean "not a plain Chainlink proxy": fall through to the
            // Aave-specific source probes.
            Err(OracleError::Provider(chainlink_error)) => {
                Self::try_register_aave_derived_source(
                    reader,
                    registry,
                    asset,
                    source,
                    chainlink_error,
                    now_timestamp,
                )
                .await
            }
            Err(error @ OracleError::Decode(_)) => {
                Self::try_register_aave_derived_source(
                    reader,
                    registry,
                    asset,
                    source,
                    error.to_string(),
                    now_timestamp,
                )
                .await
            }
            Err(error) => Err(error),
        }
    }

    async fn try_register_aave_derived_source(
        reader: &EvmCacheChainlinkReader<'_>,
        registry: &mut OracleRegistry,
        asset: AaveAsset,
        source: Address,
        chainlink_error: String,
        now_timestamp: u64,
    ) -> Result<Option<OracleAdapterFeedSkip>, OracleError> {
        let mut errors = vec![format!("Chainlink-compatible ({chainlink_error})")];
        match Self::register_price_cap_stable_source(reader, registry, &asset, source).await {
            Ok(()) => return Ok(None),
            Err(error) => errors.push(format!("Aave PriceCapAdapterStable ({error})")),
        }
        match Self::register_ratio_cap_source(reader, registry, &asset, source).await {
            Ok(()) => return Ok(None),
            Err(error) => errors.push(format!("Aave PriceCapAdapterBase/CAPO ({error})")),
        }
        match Self::register_synchronicity_peg_to_base_source(reader, registry, &asset, source)
            .await
        {
            Ok(()) => return Ok(None),
            Err(error) => errors.push(format!("Aave CLSynchronicity PegToBase ({error})")),
        }
        match Self::register_fixed_price_source(reader, registry, &asset, source, now_timestamp)
            .await
        {
            Ok(()) => return Ok(None),
            Err(error) => errors.push(format!("Aave fixed-price ({error})")),
        }

        Ok(Some(aave_skip(
            &asset,
            source,
            format!(
                "unsupported Aave source; probes failed: {}",
                errors.join("; ")
            ),
        )))
    }

    async fn register_price_cap_stable_source(
        reader: &EvmCacheChainlinkReader<'_>,
        registry: &mut OracleRegistry,
        asset: &AaveAsset,
        source: Address,
    ) -> Result<(), OracleError> {
        let capped = match reader.read_aave_price_cap_stable(source) {
            Ok(capped) => capped,
            Err(error) => return Err(error),
        };

        let registration_source =
            FeedSource::aave_price_cap_stable(source, capped.underlying_proxy, capped.price_cap);
        let (underlying_round, current_aggregator) =
            read_underlying_chainlink(reader, capped.underlying_proxy)?;

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

        Ok(())
    }

    async fn register_ratio_cap_source(
        reader: &EvmCacheChainlinkReader<'_>,
        registry: &mut OracleRegistry,
        asset: &AaveAsset,
        source: Address,
    ) -> Result<(), OracleError> {
        let ratio = reader.read_aave_ratio_cap(source)?;
        let (base_round, current_aggregator) =
            read_underlying_chainlink(reader, ratio.base_to_usd_proxy)?;
        let max_ratio = ratio_cap_max_ratio(&ratio, base_round.answer);
        let registration_source = FeedSource::aave_ratio_cap(
            source,
            ratio.base_to_usd_proxy,
            ratio.ratio_provider,
            ratio.current_ratio,
            max_ratio,
            ratio.ratio_decimals,
        );

        let mut seeded_round = base_round;
        seeded_round.answer = ratio.latest_answer;
        let metadata = FeedMetadata {
            decimals: ratio.decimals,
            description: ratio.description,
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

        Ok(())
    }

    async fn register_synchronicity_peg_to_base_source(
        reader: &EvmCacheChainlinkReader<'_>,
        registry: &mut OracleRegistry,
        asset: &AaveAsset,
        source: Address,
    ) -> Result<(), OracleError> {
        let synchronicity = reader.read_aave_synchronicity_peg_to_base(source)?;
        let (asset_round, asset_aggregator) =
            read_underlying_chainlink(reader, synchronicity.asset_to_peg_proxy)?;
        let (peg_round, peg_aggregator) =
            read_underlying_chainlink(reader, synchronicity.peg_to_base_proxy)?;
        let asset_decimals = reader.read_decimals(synchronicity.asset_to_peg_proxy)?;
        let peg_decimals = reader.read_decimals(synchronicity.peg_to_base_proxy)?;
        let registration_source = FeedSource::aave_synchronicity_peg_to_base(
            source,
            synchronicity.asset_to_peg_proxy,
            asset_aggregator,
            asset_round.answer,
            asset_decimals,
            synchronicity.peg_to_base_proxy,
            peg_aggregator,
            peg_round.answer,
            peg_decimals,
            synchronicity.decimals,
        );

        let mut seeded_round = asset_round;
        seeded_round.answer = synchronicity.latest_answer;
        seeded_round.updated_at = seeded_round.updated_at.max(peg_round.updated_at);
        let metadata = FeedMetadata {
            decimals: synchronicity.decimals,
            description: synchronicity.description,
            version: U256::ZERO,
        };

        registry
            .register_discovered_feed(
                reader,
                asset.config_for_source(source)?,
                registration_source,
                metadata,
                seeded_round,
                Some(asset_aggregator),
            )
            .await?;

        Ok(())
    }

    async fn register_fixed_price_source(
        reader: &EvmCacheChainlinkReader<'_>,
        registry: &mut OracleRegistry,
        asset: &AaveAsset,
        source: Address,
        now_timestamp: u64,
    ) -> Result<(), OracleError> {
        let fixed = reader.read_aave_fixed_price(source)?;
        let seeded_round = fixed_price_round(&fixed, now_timestamp);
        let registration_source = FeedSource::aave_fixed_price(source, fixed.latest_answer);
        let metadata = FeedMetadata {
            decimals: fixed.decimals,
            description: fixed.description,
            version: U256::ZERO,
        };
        registry
            .register_discovered_feed(
                reader,
                asset.config_for_source(source)?,
                registration_source,
                metadata,
                seeded_round,
                None,
            )
            .await?;

        Ok(())
    }
}

fn read_underlying_chainlink(
    reader: &EvmCacheChainlinkReader<'_>,
    proxy: Address,
) -> Result<(RoundData, Address), OracleError> {
    reader.read_decimals(proxy)?;
    reader.read_description(proxy)?;
    reader.read_version(proxy)?;
    let round = reader.read_latest_round_data(proxy)?;
    let aggregator = reader.read_aggregator(proxy)?.ok_or_else(|| {
        OracleError::Unsupported("underlying aggregator() returned none".to_string())
    })?;
    Ok((round, aggregator))
}

fn ratio_cap_max_ratio(ratio: &AaveRatioCapSource, base_answer: I256) -> I256 {
    if !ratio.is_capped || base_answer == I256::ZERO {
        return ratio.current_ratio;
    }
    ratio
        .latest_answer
        .saturating_mul(decimal_scale(ratio.ratio_decimals))
        / base_answer
}

fn fixed_price_round(fixed: &AaveFixedPriceSource, now_timestamp: u64) -> RoundData {
    RoundData {
        round_id: U256::ZERO,
        answer: fixed.latest_answer,
        started_at: now_timestamp,
        updated_at: now_timestamp,
        answered_in_round: U256::ZERO,
    }
}

fn decimal_scale(decimals: u8) -> I256 {
    let mut scale = I256::unchecked_from(1_i8);
    for _ in 0..decimals {
        scale = scale.saturating_mul(I256::unchecked_from(10_i8));
    }
    scale
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
