use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use alloy_network::Ethereum;
use alloy_primitives::{Address, I256, U256};
use alloy_sol_types::sol;
use evm_fork_cache::{cache::EvmCache, reactive::ReactiveHandler};

use crate::{
    AdapterFuture, AssetId, Denomination, EulerQuoteLeg, EvmCacheChainlinkReader, FeedId,
    FeedMetadata, FeedRegistration, FeedSource, OracleAdapterFeedSkip, OracleAdapterId,
    OracleAdapterPlugin, OracleAdapterSkipReason, OracleDiscoveredFeed, OracleDiscoveryContext,
    OracleDiscoveryReport, OracleError, OracleReactiveHandler, OracleStorageSync, RoundData,
    StalenessPolicy,
};

sol! {
    interface EulerRouterInterface {
        function getConfiguredOracle(address base, address quote) external view returns (address);
        function fallbackOracle() external view returns (address);
    }

    interface EulerPriceOracleInterface {
        function name() external view returns (string);
    }

    interface EulerChainlinkOracleInterface {
        function base() external view returns (address);
        function quote() external view returns (address);
        function feed() external view returns (address);
        function maxStaleness() external view returns (uint256);
    }

    interface EulerFixedRateOracleInterface {
        function base() external view returns (address);
        function quote() external view returns (address);
        function rate() external view returns (uint256);
    }

    interface EulerRateProviderOracleInterface {
        function base() external view returns (address);
        function quote() external view returns (address);
        function rateProvider() external view returns (address);
    }

    interface EulerCrossAdapterInterface {
        function base() external view returns (address);
        function cross() external view returns (address);
        function quote() external view returns (address);
        function oracleBaseCross() external view returns (address);
        function oracleCrossQuote() external view returns (address);
    }

    interface RateProviderInterface {
        function getRate() external view returns (uint256);
    }

    interface Erc20Metadata {
        function decimals() external view returns (uint8);
    }
}

use Erc20Metadata::decimalsCall;
use EulerChainlinkOracleInterface::{
    baseCall as chainlinkBaseCall, feedCall, maxStalenessCall, quoteCall as chainlinkQuoteCall,
};
use EulerCrossAdapterInterface::{
    baseCall as crossBaseCall, crossCall, oracleBaseCrossCall, oracleCrossQuoteCall,
    quoteCall as crossQuoteCall,
};
use EulerFixedRateOracleInterface::{
    baseCall as fixedBaseCall, quoteCall as fixedQuoteCall, rateCall,
};
use EulerPriceOracleInterface::nameCall;
use EulerRateProviderOracleInterface::{
    baseCall as rateBaseCall, quoteCall as rateQuoteCall, rateProviderCall,
};
use EulerRouterInterface::{fallbackOracleCall, getConfiguredOracleCall};
use RateProviderInterface::getRateCall;

const ADAPTER_ID: &str = "evm-oracle-state.euler";
const RESERVED_DECIMALS_RANGE: u128 = 0xffff_ffff;

/// Declarative Euler quote-pair registration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EulerQuotePair {
    base: Address,
    quote: Address,
    oracle: Option<Address>,
    id: Option<FeedId>,
    label: Option<String>,
    base_label: Option<AssetId>,
    quote_label: Option<Denomination>,
    staleness: StalenessPolicy,
}

impl EulerQuotePair {
    /// Register a base/quote pair to be resolved through an adapter-level router.
    pub fn new(base: Address, quote: Address) -> Self {
        Self {
            base,
            quote,
            oracle: None,
            id: None,
            label: None,
            base_label: None,
            quote_label: None,
            staleness: StalenessPolicy::default(),
        }
    }

    /// Register a base/quote pair with a direct Euler oracle adapter.
    pub fn with_oracle(base: Address, quote: Address, oracle: Address) -> Self {
        Self {
            oracle: Some(oracle),
            ..Self::new(base, quote)
        }
    }

    /// Set a stable feed id.
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(FeedId::new(id));
        self
    }

    /// Set a stable feed id.
    pub fn feed_id(mut self, id: FeedId) -> Self {
        self.id = Some(id);
        self
    }

    /// Set a human-readable label.
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Set the base asset label.
    pub fn base_label(mut self, base: AssetId) -> Self {
        self.base_label = Some(base);
        self
    }

    /// Set the quote denomination label.
    pub fn quote_label(mut self, quote: Denomination) -> Self {
        self.quote_label = Some(quote);
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
}

/// Cache-backed Euler price oracle discovery adapter.
#[derive(Clone, Debug, Default)]
pub struct EulerOracleAdapter {
    router: Option<Address>,
    pairs: Vec<EulerQuotePair>,
    now_timestamp: Option<u64>,
}

impl EulerOracleAdapter {
    /// Create an adapter with no default router.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create an adapter that resolves quote pairs through `EulerRouter`.
    pub fn router(router: Address) -> Self {
        Self {
            router: Some(router),
            pairs: Vec::new(),
            now_timestamp: None,
        }
    }

    /// Create an adapter for one direct oracle quote pair.
    pub fn oracle(oracle: Address, base: Address, quote: Address) -> Self {
        Self {
            router: None,
            pairs: vec![EulerQuotePair::with_oracle(base, quote, oracle)],
            now_timestamp: None,
        }
    }

    /// Add one quote pair.
    pub fn quote_pair(mut self, pair: EulerQuotePair) -> Self {
        self.pairs.push(pair);
        self
    }

    /// Add multiple quote pairs.
    pub fn quote_pairs(mut self, pairs: impl IntoIterator<Item = EulerQuotePair>) -> Self {
        self.pairs.extend(pairs);
        self
    }

    /// Set a fixed timestamp for deterministic registration status classification.
    pub fn now_timestamp(mut self, now_timestamp: u64) -> Self {
        self.now_timestamp = Some(now_timestamp);
        self
    }

    fn timestamp(&self, fallback: Option<u64>) -> Result<u64, OracleError> {
        if let Some(now_timestamp) = self.now_timestamp.or(fallback) {
            return Ok(now_timestamp);
        }
        Ok(SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(provider_error)?
            .as_secs())
    }

    fn discover_pairs(
        &self,
        cache: &mut EvmCache,
        now_timestamp: u64,
    ) -> Result<OracleDiscoveryReport, OracleError> {
        let mut report = OracleDiscoveryReport::new();
        for pair in &self.pairs {
            match self.discover_pair(cache, pair, now_timestamp) {
                Ok(feed) => report = report.with_feed(feed),
                Err(error) => {
                    report = report.with_skip(OracleAdapterFeedSkip {
                        feed: crate::Feed::proxy(pair.oracle.unwrap_or_default()).build(),
                        proxy: pair.oracle.unwrap_or_default(),
                        reason: OracleAdapterSkipReason::UnsupportedEulerSource {
                            error: error.to_string(),
                        },
                    });
                }
            }
        }
        Ok(report)
    }

    fn discover_pair(
        &self,
        cache: &mut EvmCache,
        pair: &EulerQuotePair,
        now_timestamp: u64,
    ) -> Result<OracleDiscoveredFeed, OracleError> {
        let oracle = match pair.oracle {
            Some(oracle) => oracle,
            None => self.resolve_router_oracle(cache, pair.base, pair.quote)?,
        };
        let proxy = pair.oracle.or(self.router).unwrap_or(oracle);
        let base_decimals = read_asset_decimals(cache, pair.base)?;
        let quote_decimals = read_asset_decimals(cache, pair.quote)?;
        let source = self.read_source(
            cache,
            oracle,
            pair.base,
            pair.quote,
            base_decimals,
            quote_decimals,
            0,
        )?;
        let answer = source.normalize_answer(I256::ZERO);
        let id = pair.id.unwrap_or_else(|| {
            FeedId::new(format!("euler-{proxy:?}-{:?}-{:?}", pair.base, pair.quote))
        });
        let label = pair
            .label
            .clone()
            .or_else(|| Some(format!("Euler {:?}/{:?}", pair.base, pair.quote)));
        let metadata = FeedMetadata {
            decimals: quote_decimals,
            description: label.clone().unwrap_or_else(|| "Euler quote".to_string()),
            version: U256::ZERO,
        };
        let registration = FeedRegistration {
            id,
            proxy,
            label,
            base: pair
                .base_label
                .clone()
                .map(String::from)
                .or_else(|| Some(format!("{:?}", pair.base))),
            quote: pair
                .quote_label
                .clone()
                .map(String::from)
                .or_else(|| Some(format!("{:?}", pair.quote))),
            staleness: pair.staleness,
            current_aggregator: source.event_aggregators(None).first().copied(),
            aggregator_layout: None,
            metadata,
            source,
        };
        let round = RoundData {
            round_id: U256::ZERO,
            answer,
            started_at: now_timestamp,
            updated_at: now_timestamp,
            answered_in_round: U256::ZERO,
        };
        Ok(OracleDiscoveredFeed::new(registration, round))
    }

    fn resolve_router_oracle(
        &self,
        cache: &mut EvmCache,
        base: Address,
        quote: Address,
    ) -> Result<Address, OracleError> {
        let router = self.router.ok_or_else(|| {
            OracleError::Provider("Euler router or direct oracle is required".to_string())
        })?;
        let configured = cache
            .call_sol(router, getConfiguredOracleCall { base, quote })
            .map_err(provider_error)?;
        if configured != Address::ZERO {
            return Ok(configured);
        }
        let fallback = cache
            .call_sol(router, fallbackOracleCall {})
            .map_err(provider_error)?;
        if fallback != Address::ZERO {
            return Ok(fallback);
        }
        Err(OracleError::Provider(
            "Euler router has no configured or fallback oracle for pair".to_string(),
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn read_source(
        &self,
        cache: &mut EvmCache,
        oracle: Address,
        requested_base: Address,
        requested_quote: Address,
        requested_base_decimals: u8,
        requested_quote_decimals: u8,
        depth: usize,
    ) -> Result<FeedSource, OracleError> {
        if depth > 4 {
            return Err(OracleError::Provider(
                "Euler oracle nesting depth exceeded".to_string(),
            ));
        }
        let name = cache
            .call_sol(oracle, nameCall {})
            .map_err(provider_error)?;
        match name.as_str() {
            "ChainlinkOracle" => {
                let leg = self.read_chainlink_leg(
                    cache,
                    oracle,
                    requested_base,
                    requested_quote,
                    requested_quote_decimals,
                )?;
                Ok(FeedSource::euler_quote(
                    oracle,
                    requested_base,
                    requested_quote,
                    requested_base_decimals,
                    requested_quote_decimals,
                    leg,
                ))
            }
            "FixedRateOracle" => {
                let leg = self.read_fixed_rate_leg(
                    cache,
                    oracle,
                    requested_base,
                    requested_quote,
                    requested_base_decimals,
                    requested_quote_decimals,
                )?;
                Ok(FeedSource::euler_quote(
                    oracle,
                    requested_base,
                    requested_quote,
                    requested_base_decimals,
                    requested_quote_decimals,
                    leg,
                ))
            }
            "RateProviderOracle" => {
                let leg = self.read_rate_provider_leg(
                    cache,
                    oracle,
                    requested_base,
                    requested_quote,
                    requested_quote_decimals,
                )?;
                Ok(FeedSource::euler_quote(
                    oracle,
                    requested_base,
                    requested_quote,
                    requested_base_decimals,
                    requested_quote_decimals,
                    leg,
                ))
            }
            "CrossAdapter" => self.read_cross_source(
                cache,
                oracle,
                requested_base,
                requested_quote,
                requested_base_decimals,
                requested_quote_decimals,
                depth,
            ),
            other => Err(OracleError::Provider(format!(
                "unsupported Euler oracle adapter {other}"
            ))),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn read_cross_source(
        &self,
        cache: &mut EvmCache,
        oracle: Address,
        requested_base: Address,
        requested_quote: Address,
        requested_base_decimals: u8,
        requested_quote_decimals: u8,
        depth: usize,
    ) -> Result<FeedSource, OracleError> {
        let configured_base = cache
            .call_sol(oracle, crossBaseCall {})
            .map_err(provider_error)?;
        let cross = cache
            .call_sol(oracle, crossCall {})
            .map_err(provider_error)?;
        let configured_quote = cache
            .call_sol(oracle, crossQuoteCall {})
            .map_err(provider_error)?;
        let inverse = direction_or_revert(
            requested_base,
            configured_base,
            requested_quote,
            configured_quote,
        )?;
        let oracle_base_cross = cache
            .call_sol(oracle, oracleBaseCrossCall {})
            .map_err(provider_error)?;
        let oracle_cross_quote = cache
            .call_sol(oracle, oracleCrossQuoteCall {})
            .map_err(provider_error)?;
        let cross_decimals = read_asset_decimals(cache, cross)?;
        let (first_oracle, second_oracle) = if inverse {
            (oracle_cross_quote, oracle_base_cross)
        } else {
            (oracle_base_cross, oracle_cross_quote)
        };
        let base_cross = self.read_leg(
            cache,
            first_oracle,
            requested_base,
            cross,
            requested_base_decimals,
            cross_decimals,
            depth + 1,
        )?;
        let cross_quote = self.read_leg(
            cache,
            second_oracle,
            cross,
            requested_quote,
            cross_decimals,
            requested_quote_decimals,
            depth + 1,
        )?;

        Ok(FeedSource::euler_cross(
            oracle,
            requested_base,
            cross,
            requested_quote,
            requested_base_decimals,
            cross_decimals,
            requested_quote_decimals,
            base_cross,
            cross_quote,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn read_leg(
        &self,
        cache: &mut EvmCache,
        oracle: Address,
        requested_base: Address,
        requested_quote: Address,
        requested_base_decimals: u8,
        requested_quote_decimals: u8,
        depth: usize,
    ) -> Result<EulerQuoteLeg, OracleError> {
        let source = self.read_source(
            cache,
            oracle,
            requested_base,
            requested_quote,
            requested_base_decimals,
            requested_quote_decimals,
            depth,
        )?;
        match source {
            FeedSource::EulerQuote { leg, .. } => Ok(leg),
            FeedSource::EulerCross { .. } => Err(OracleError::Provider(
                "nested Euler CrossAdapter legs are not supported in this MVP".to_string(),
            )),
            _ => Err(OracleError::Provider(
                "Euler child source did not produce a quote leg".to_string(),
            )),
        }
    }

    fn read_chainlink_leg(
        &self,
        cache: &mut EvmCache,
        oracle: Address,
        requested_base: Address,
        requested_quote: Address,
        requested_quote_decimals: u8,
    ) -> Result<EulerQuoteLeg, OracleError> {
        let configured_base = cache
            .call_sol(oracle, chainlinkBaseCall {})
            .map_err(provider_error)?;
        let configured_quote = cache
            .call_sol(oracle, chainlinkQuoteCall {})
            .map_err(provider_error)?;
        let inverse = direction_or_revert(
            requested_base,
            configured_base,
            requested_quote,
            configured_quote,
        )?;
        let feed = cache
            .call_sol(oracle, feedCall {})
            .map_err(provider_error)?;
        let _max_staleness = cache
            .call_sol(oracle, maxStalenessCall {})
            .map_err(provider_error)?;
        let reader = EvmCacheChainlinkReader::new(cache);
        let feed_decimals = reader.read_decimals(feed)?;
        let round = reader.read_latest_round_data(feed)?;
        let aggregator = reader.read_aggregator(feed)?.ok_or_else(|| {
            OracleError::Provider(
                "Euler ChainlinkOracle feed aggregator() returned none".to_string(),
            )
        })?;
        Ok(EulerQuoteLeg::Chainlink {
            oracle,
            feed,
            aggregator,
            answer: round.answer,
            feed_decimals,
            inverse,
            quote_decimals: requested_quote_decimals,
        })
    }

    fn read_fixed_rate_leg(
        &self,
        cache: &mut EvmCache,
        oracle: Address,
        requested_base: Address,
        requested_quote: Address,
        requested_base_decimals: u8,
        requested_quote_decimals: u8,
    ) -> Result<EulerQuoteLeg, OracleError> {
        let configured_base = cache
            .call_sol(oracle, fixedBaseCall {})
            .map_err(provider_error)?;
        let configured_quote = cache
            .call_sol(oracle, fixedQuoteCall {})
            .map_err(provider_error)?;
        let inverse = direction_or_revert(
            requested_base,
            configured_base,
            requested_quote,
            configured_quote,
        )?;
        let rate = cache
            .call_sol(oracle, rateCall {})
            .map_err(provider_error)?;
        Ok(EulerQuoteLeg::FixedRate {
            oracle,
            rate: i256_from_u256(rate, "rate")?,
            inverse,
            base_decimals: requested_base_decimals,
            quote_decimals: requested_quote_decimals,
        })
    }

    fn read_rate_provider_leg(
        &self,
        cache: &mut EvmCache,
        oracle: Address,
        requested_base: Address,
        requested_quote: Address,
        requested_quote_decimals: u8,
    ) -> Result<EulerQuoteLeg, OracleError> {
        let configured_base = cache
            .call_sol(oracle, rateBaseCall {})
            .map_err(provider_error)?;
        let configured_quote = cache
            .call_sol(oracle, rateQuoteCall {})
            .map_err(provider_error)?;
        let inverse = direction_or_revert(
            requested_base,
            configured_base,
            requested_quote,
            configured_quote,
        )?;
        let rate_provider = cache
            .call_sol(oracle, rateProviderCall {})
            .map_err(provider_error)?;
        let rate = cache
            .call_sol(rate_provider, getRateCall {})
            .map_err(provider_error)?;
        Ok(EulerQuoteLeg::RateProvider {
            oracle,
            rate_provider,
            rate: i256_from_u256(rate, "rate")?,
            inverse,
            quote_decimals: requested_quote_decimals,
        })
    }
}

impl OracleAdapterPlugin for EulerOracleAdapter {
    fn adapter_id(&self) -> OracleAdapterId {
        OracleAdapterId::new(ADAPTER_ID)
    }

    fn discover<'a>(
        &'a self,
        ctx: OracleDiscoveryContext<'a>,
    ) -> AdapterFuture<'a, OracleDiscoveryReport> {
        Box::pin(async move {
            let now_timestamp = self.timestamp(Some(ctx.now_timestamp))?;
            self.discover_pairs(ctx.cache, now_timestamp)
        })
    }

    fn reactive_handler(
        &self,
        registrations: Vec<FeedRegistration>,
        storage_sync: OracleStorageSync,
    ) -> Arc<dyn ReactiveHandler<Ethereum>> {
        Arc::new(OracleReactiveHandler::with_storage_sync(
            registrations,
            storage_sync,
        ))
    }
}

fn direction_or_revert(
    requested_base: Address,
    configured_base: Address,
    requested_quote: Address,
    configured_quote: Address,
) -> Result<bool, OracleError> {
    if requested_base == configured_base && requested_quote == configured_quote {
        Ok(false)
    } else if requested_base == configured_quote && requested_quote == configured_base {
        Ok(true)
    } else {
        Err(OracleError::Provider(
            "Euler oracle does not support requested base/quote pair".to_string(),
        ))
    }
}

fn read_asset_decimals(cache: &mut EvmCache, asset: Address) -> Result<u8, OracleError> {
    if is_reserved_asset_address(asset) {
        return Ok(18);
    }
    match cache.call_sol(asset, decimalsCall {}) {
        Ok(decimals) => Ok(decimals),
        Err(_) => Ok(18),
    }
}

fn is_reserved_asset_address(asset: Address) -> bool {
    let bytes = asset.as_slice();
    bytes[..16].iter().all(|byte| *byte == 0)
        && u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]])
            <= RESERVED_DECIMALS_RANGE as u32
}

fn i256_from_u256(value: U256, field: &'static str) -> Result<I256, OracleError> {
    I256::try_from(value).map_err(|_| OracleError::Provider(format!("{field} does not fit int256")))
}

fn provider_error(error: impl ToString) -> OracleError {
    OracleError::Provider(error.to_string())
}
