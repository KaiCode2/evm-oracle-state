use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    time::{SystemTime, UNIX_EPOCH},
};

#[cfg(feature = "aave")]
use alloy_primitives::I256;
use alloy_primitives::{Address, Bytes, U256};
use alloy_sol_types::{SolCall, sol};
use evm_fork_cache::{
    cache::EvmCache,
    multicall::{IMulticall3, MULTICALL3_ADDRESS, execute_batched, try_decode_result},
};

use crate::{
    AggregatorLayoutEvidence, ChainlinkFeedProvider, Feed, FeedConfig, FeedId, FeedMetadata,
    FeedRegistration, FeedSource, OracleError, OracleRegistry, OracleTracker, ProviderFuture,
    RoundData, registry::derive_feed_id,
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

#[derive(Debug)]
struct ChainlinkCoreRead {
    decimals: u8,
    description: String,
    version: U256,
    round: RoundData,
    aggregator: Option<Address>,
}

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

    interface AaveRatioCapAdapterInterface {
        function BASE_TO_USD_AGGREGATOR() external view returns (address);
        function RATIO_PROVIDER() external view returns (address);
        function RATIO_DECIMALS() external view returns (uint8);
        function getRatio() external view returns (int256);
        function isCapped() external view returns (bool);
        function decimals() external view returns (uint8);
        function description() external view returns (string);
        function latestAnswer() external view returns (int256);
    }

    interface AaveSynchronicityPegToBaseInterface {
        function ASSET_TO_PEG() external view returns (address);
        function PEG_TO_BASE() external view returns (address);
        function decimals() external view returns (uint8);
        function description() external view returns (string);
        function latestAnswer() external view returns (int256);
    }

    interface AaveFixedPriceSourceInterface {
        function price() external view returns (int256);
        function decimals() external view returns (uint8);
        function description() external view returns (string);
        function latestAnswer() external view returns (int256);
    }

    interface AaveConstantPriceSourceInterface {
        function PRICE() external view returns (int256);
    }

    interface AaveBaseToPegProbeInterface {
        function BASE_TO_PEG() external view returns (address);
    }

    interface AaveDynamicSourceProbeInterface {
        function REFERENCE_FEED() external view returns (address);
        function DISCOUNT_RATE() external view returns (uint256);
        function discount() external view returns (uint256);
        function EXCHANGE_RATE() external view returns (uint256);
        function PENDLE_PRINCIPAL_TOKEN() external view returns (address);
        function PENDLE_ORACLE() external view returns (address);
    }
}

#[cfg(feature = "aave")]
use AaveBaseToPegProbeInterface::BASE_TO_PEGCall;
#[cfg(feature = "aave")]
use AaveConstantPriceSourceInterface::PRICECall;
#[cfg(feature = "aave")]
use AaveDynamicSourceProbeInterface::{
    DISCOUNT_RATECall, EXCHANGE_RATECall, PENDLE_ORACLECall, PENDLE_PRINCIPAL_TOKENCall,
    REFERENCE_FEEDCall, discountCall,
};
#[cfg(feature = "aave")]
use AaveFixedPriceSourceInterface::{
    decimalsCall as aaveFixedDecimalsCall, descriptionCall as aaveFixedDescriptionCall,
    latestAnswerCall as aaveFixedLatestAnswerCall, priceCall as aaveFixedPriceCall,
};
#[cfg(feature = "aave")]
use AaveOracleInterface::getSourceOfAssetCall;
#[cfg(feature = "aave")]
use AavePriceCapAdapterStableInterface::{
    ASSET_TO_USD_AGGREGATORCall, decimalsCall as aaveDecimalsCall,
    descriptionCall as aaveDescriptionCall, getPriceCapCall, latestAnswerCall,
};
#[cfg(feature = "aave")]
use AaveRatioCapAdapterInterface::{
    BASE_TO_USD_AGGREGATORCall, RATIO_DECIMALSCall, RATIO_PROVIDERCall,
    decimalsCall as aaveRatioDecimalsCall, descriptionCall as aaveRatioDescriptionCall,
    getRatioCall, isCappedCall, latestAnswerCall as aaveRatioLatestAnswerCall,
};
#[cfg(feature = "aave")]
use AaveSynchronicityPegToBaseInterface::{
    ASSET_TO_PEGCall, PEG_TO_BASECall, decimalsCall as aaveSynchronicityDecimalsCall,
    descriptionCall as aaveSynchronicityDescriptionCall,
    latestAnswerCall as aaveSynchronicityLatestAnswerCall,
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

#[cfg(feature = "aave")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AaveRatioCapSource {
    pub(crate) base_to_usd_proxy: Address,
    pub(crate) ratio_provider: Address,
    pub(crate) current_ratio: I256,
    pub(crate) is_capped: bool,
    pub(crate) ratio_decimals: u8,
    pub(crate) decimals: u8,
    pub(crate) description: String,
    pub(crate) latest_answer: I256,
}

#[cfg(feature = "aave")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AaveSynchronicityPegToBaseSource {
    pub(crate) asset_to_peg_proxy: Address,
    pub(crate) peg_to_base_proxy: Address,
    pub(crate) decimals: u8,
    pub(crate) description: String,
    pub(crate) latest_answer: I256,
}

#[cfg(feature = "aave")]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AaveFixedPriceSource {
    pub(crate) decimals: u8,
    pub(crate) description: String,
    pub(crate) latest_answer: I256,
}

/// Chainlink view reader backed by an `evm-fork-cache` fork cache.
pub struct EvmCacheChainlinkReader<'a> {
    cache: RefCell<&'a mut EvmCache>,
    multicall_reads: bool,
}

impl<'a> EvmCacheChainlinkReader<'a> {
    /// Create a reader around an existing mutable EVM cache.
    pub fn new(cache: &'a mut EvmCache) -> Self {
        Self {
            cache: RefCell::new(cache),
            multicall_reads: true,
        }
    }

    /// Create a reader with direct/fork Multicall3 view batching disabled.
    pub fn without_multicall(cache: &'a mut EvmCache) -> Self {
        Self {
            cache: RefCell::new(cache),
            multicall_reads: false,
        }
    }

    /// Enable or disable direct/fork Multicall3 view batching.
    pub fn with_multicall_reads(mut self, enabled: bool) -> Self {
        self.multicall_reads = enabled;
        self
    }

    /// Register a typed feed into an existing oracle registry.
    pub async fn register_feed(
        &self,
        registry: &mut OracleRegistry,
        feed: Feed,
    ) -> Result<FeedId, OracleError> {
        let config = FeedConfig::try_from(feed)?;
        self.register_config(registry, config).await
    }

    /// Register multiple Chainlink-compatible feeds, batching proxy reads when possible.
    pub async fn register_feeds(
        &self,
        registry: &mut OracleRegistry,
        feeds: Vec<Feed>,
    ) -> Result<Vec<OracleAdapterFeedSkip>, OracleError> {
        let mut prepared = Vec::with_capacity(feeds.len());
        for feed in feeds {
            let proxy = feed.proxy().unwrap_or_default();
            let config = FeedConfig::try_from(feed.clone())?;
            let read_proxy = FeedSource::Chainlink.read_proxy(config.proxy);
            prepared.push((feed, config, proxy, read_proxy));
        }

        let read_proxies = prepared
            .iter()
            .map(|(_, _, _, read_proxy)| *read_proxy)
            .collect::<Vec<_>>();
        let mut cores = self
            .read_chainlink_cores_multicall(&read_proxies)
            .unwrap_or_default();
        let mut layouts = self.layout_evidence_for_cores(&cores);

        let mut skipped = Vec::new();
        for (index, (feed, config, proxy, read_proxy)) in prepared.into_iter().enumerate() {
            let mut from_batched_core = false;
            let core = match cores.get_mut(index).and_then(Option::take) {
                Some(Ok(core)) => {
                    from_batched_core = true;
                    Ok(core)
                }
                Some(Err(error)) => Err(error),
                None => self.read_chainlink_core(read_proxy),
            };

            match core {
                Ok(core) => {
                    if from_batched_core {
                        if let Some(layouts) = layouts.as_mut() {
                            let layout = layouts.get_mut(index).and_then(Option::take);
                            self.insert_config_core_with_layout(registry, config, core, layout)?;
                        } else {
                            self.insert_config_core(registry, config, core).await?;
                        }
                    } else {
                        self.insert_config_core(registry, config, core).await?;
                    }
                }
                Err(OracleError::Provider(error)) => {
                    skipped.push(OracleAdapterFeedSkip {
                        feed,
                        proxy,
                        reason: OracleAdapterSkipReason::NotChainlinkCompatible { error },
                    });
                }
                Err(error) => return Err(error),
            }
        }

        Ok(skipped)
    }

    /// Register a Chainlink-compatible feed config into an existing registry.
    pub async fn register_config(
        &self,
        registry: &mut OracleRegistry,
        config: FeedConfig,
    ) -> Result<FeedId, OracleError> {
        let source = FeedSource::Chainlink;
        let read_proxy = source.read_proxy(config.proxy);
        let core = self.read_chainlink_core(read_proxy)?;
        self.insert_config_core(registry, config, core).await
    }

    fn layout_evidence_for_cores(
        &self,
        cores: &[Option<Result<ChainlinkCoreRead, OracleError>>],
    ) -> Option<Vec<Option<AggregatorLayoutEvidence>>> {
        let mut aggregators = Vec::new();
        let mut seen_aggregators = BTreeSet::new();
        for core in cores.iter().filter_map(|core| match core {
            Some(Ok(core)) => core.aggregator,
            _ => None,
        }) {
            if seen_aggregators.insert(core) {
                aggregators.push(core);
            }
        }
        if aggregators.is_empty() {
            return Some(vec![None; cores.len()]);
        }

        let mut type_and_versions = self.read_type_and_versions_multicall(&aggregators)?;
        let mut evidence_by_aggregator = BTreeMap::new();
        for (index, aggregator) in aggregators.iter().copied().enumerate() {
            let evidence = type_and_versions
                .get_mut(index)
                .and_then(Option::take)
                .map(|type_and_version| {
                    AggregatorLayoutEvidence::from_type_and_version(
                        aggregator,
                        type_and_version,
                        None,
                    )
                })
                .unwrap_or_else(|| AggregatorLayoutEvidence::unknown(aggregator, None));
            evidence_by_aggregator.insert(aggregator, evidence);
        }

        Some(
            cores
                .iter()
                .map(|core| {
                    let aggregator = match core {
                        Some(Ok(core)) => core.aggregator?,
                        _ => return None,
                    };
                    evidence_by_aggregator.get(&aggregator).cloned()
                })
                .collect(),
        )
    }

    async fn insert_config_core(
        &self,
        registry: &mut OracleRegistry,
        config: FeedConfig,
        core: ChainlinkCoreRead,
    ) -> Result<FeedId, OracleError> {
        let aggregator_layout = registry
            .detect_aggregator_layout(self, core.aggregator, None)
            .await;
        self.insert_config_core_with_layout(registry, config, core, aggregator_layout)
    }

    fn insert_config_core_with_layout(
        &self,
        registry: &mut OracleRegistry,
        config: FeedConfig,
        core: ChainlinkCoreRead,
        aggregator_layout: Option<AggregatorLayoutEvidence>,
    ) -> Result<FeedId, OracleError> {
        let id = config
            .id
            .unwrap_or_else(|| derive_feed_id(config.label.as_deref(), config.proxy));
        let source = FeedSource::Chainlink;
        let registration = FeedRegistration {
            id,
            proxy: config.proxy,
            label: config.label,
            base: config.base,
            quote: config.quote,
            staleness: config.staleness,
            current_aggregator: core.aggregator,
            aggregator_layout,
            metadata: FeedMetadata {
                decimals: core.decimals,
                description: core.description,
                version: core.version,
            },
            source,
        };
        registry.insert_seeded_registration(registration, core.round)?;
        Ok(id)
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
        round_from_raw(round)
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

    fn read_chainlink_core(&self, proxy: Address) -> Result<ChainlinkCoreRead, OracleError> {
        if let Some(read) = self.read_chainlink_core_multicall(proxy) {
            return read;
        }

        Ok(ChainlinkCoreRead {
            decimals: self.read_decimals(proxy)?,
            description: self.read_description(proxy)?,
            version: self.read_version(proxy)?,
            round: self.read_latest_round_data(proxy)?,
            aggregator: self.read_aggregator(proxy)?,
        })
    }

    fn read_chainlink_core_multicall(
        &self,
        proxy: Address,
    ) -> Option<Result<ChainlinkCoreRead, OracleError>> {
        let results = self.execute_multicall([
            multicall_call(proxy, decimalsCall {}, true),
            multicall_call(proxy, descriptionCall {}, true),
            multicall_call(proxy, versionCall {}, true),
            multicall_call(proxy, latestRoundDataCall {}, true),
            multicall_call(proxy, aggregatorCall {}, true),
        ])?;
        if results.len() != 5 {
            return None;
        }

        let decimals = try_decode_result::<decimalsCall>(&results[0])?;
        let description = try_decode_result::<descriptionCall>(&results[1])?;
        let version = try_decode_result::<versionCall>(&results[2])?;
        let raw_round = try_decode_result::<latestRoundDataCall>(&results[3])?;
        let aggregator = try_decode_result::<aggregatorCall>(&results[4]);
        let round = match round_from_raw(raw_round) {
            Ok(round) => round,
            Err(error) => return Some(Err(error)),
        };
        Some(Ok(ChainlinkCoreRead {
            decimals,
            description,
            version,
            round,
            aggregator,
        }))
    }

    fn read_chainlink_cores_multicall(
        &self,
        proxies: &[Address],
    ) -> Option<Vec<Option<Result<ChainlinkCoreRead, OracleError>>>> {
        let calls = proxies
            .iter()
            .flat_map(|proxy| {
                [
                    multicall_call(*proxy, decimalsCall {}, true),
                    multicall_call(*proxy, descriptionCall {}, true),
                    multicall_call(*proxy, versionCall {}, true),
                    multicall_call(*proxy, latestRoundDataCall {}, true),
                    multicall_call(*proxy, aggregatorCall {}, true),
                ]
            })
            .collect::<Vec<_>>();
        let results = self.execute_multicall(calls)?;
        if results.len() != proxies.len() * 5 {
            return None;
        }

        let mut cores = Vec::with_capacity(proxies.len());
        for chunk in results.chunks_exact(5) {
            let decimals = try_decode_result::<decimalsCall>(&chunk[0]);
            let description = try_decode_result::<descriptionCall>(&chunk[1]);
            let version = try_decode_result::<versionCall>(&chunk[2]);
            let raw_round = try_decode_result::<latestRoundDataCall>(&chunk[3]);
            let aggregator = try_decode_result::<aggregatorCall>(&chunk[4]);
            let Some((decimals, description, version, raw_round)) =
                decimals.zip(description).zip(version).zip(raw_round).map(
                    |(((decimals, description), version), raw_round)| {
                        (decimals, description, version, raw_round)
                    },
                )
            else {
                cores.push(None);
                continue;
            };

            let round = match round_from_raw(raw_round) {
                Ok(round) => round,
                Err(error) => {
                    cores.push(Some(Err(error)));
                    continue;
                }
            };
            cores.push(Some(Ok(ChainlinkCoreRead {
                decimals,
                description,
                version,
                round,
                aggregator,
            })));
        }
        Some(cores)
    }

    fn read_type_and_versions_multicall(
        &self,
        aggregators: &[Address],
    ) -> Option<Vec<Option<String>>> {
        let calls = aggregators
            .iter()
            .map(|aggregator| multicall_call(*aggregator, typeAndVersionCall {}, true))
            .collect::<Vec<_>>();
        let results = self.execute_multicall(calls)?;
        if results.len() != aggregators.len() {
            return None;
        }
        Some(
            results
                .iter()
                .map(try_decode_result::<typeAndVersionCall>)
                .collect(),
        )
    }

    fn execute_multicall<I>(&self, calls: I) -> Option<Vec<IMulticall3::Result>>
    where
        I: IntoIterator<Item = (Address, Bytes, bool)>,
    {
        if !self.multicall_reads {
            return None;
        }
        let calls = calls.into_iter().collect::<Vec<_>>();
        if calls.is_empty() {
            return Some(Vec::new());
        }
        if let Some(results) = self.execute_direct_rpc_multicall(&calls) {
            return Some(results);
        }
        let mut cache = self.cache.borrow_mut();
        execute_batched(&mut cache, calls).ok()
    }

    fn execute_direct_rpc_multicall(
        &self,
        calls: &[(Address, Bytes, bool)],
    ) -> Option<Vec<IMulticall3::Result>> {
        let calls = calls
            .iter()
            .map(|(target, call_data, allow_failure)| IMulticall3::Call3 {
                target: *target,
                allowFailure: *allow_failure,
                callData: call_data.clone(),
            })
            .collect::<Vec<_>>();
        let call = IMulticall3::aggregate3Call { calls };
        let cache = self.cache.borrow();
        let bytes = cache
            .rpc_call(MULTICALL3_ADDRESS, Bytes::from(call.abi_encode()))
            .and_then(Result::ok)?;
        IMulticall3::aggregate3Call::abi_decode_returns(&bytes).ok()
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

    #[cfg(feature = "aave")]
    pub(crate) fn read_aave_ratio_cap(
        &self,
        source: Address,
    ) -> Result<AaveRatioCapSource, OracleError> {
        let base_to_usd_proxy = self
            .cache
            .borrow_mut()
            .call_sol(source, BASE_TO_USD_AGGREGATORCall {})
            .map_err(provider_error)?;
        let ratio_provider = self
            .cache
            .borrow_mut()
            .call_sol(source, RATIO_PROVIDERCall {})
            .map_err(provider_error)?;
        let current_ratio = self
            .cache
            .borrow_mut()
            .call_sol(source, getRatioCall {})
            .map_err(provider_error)?;
        let is_capped = self
            .cache
            .borrow_mut()
            .call_sol(source, isCappedCall {})
            .map_err(provider_error)?;
        let ratio_decimals = self
            .cache
            .borrow_mut()
            .call_sol(source, RATIO_DECIMALSCall {})
            .map_err(provider_error)?;
        let decimals = self
            .cache
            .borrow_mut()
            .call_sol(source, aaveRatioDecimalsCall {})
            .map_err(provider_error)?;
        let description = self
            .cache
            .borrow_mut()
            .call_sol(source, aaveRatioDescriptionCall {})
            .map_err(provider_error)?;
        let latest_answer = self
            .cache
            .borrow_mut()
            .call_sol(source, aaveRatioLatestAnswerCall {})
            .map_err(provider_error)?;

        Ok(AaveRatioCapSource {
            base_to_usd_proxy,
            ratio_provider,
            current_ratio,
            is_capped,
            ratio_decimals,
            decimals,
            description,
            latest_answer,
        })
    }

    #[cfg(feature = "aave")]
    pub(crate) fn read_aave_synchronicity_peg_to_base(
        &self,
        source: Address,
    ) -> Result<AaveSynchronicityPegToBaseSource, OracleError> {
        let asset_to_peg_proxy = self
            .cache
            .borrow_mut()
            .call_sol(source, ASSET_TO_PEGCall {})
            .map_err(provider_error)?;
        let peg_to_base_proxy = self
            .cache
            .borrow_mut()
            .call_sol(source, PEG_TO_BASECall {})
            .map_err(provider_error)?;
        let decimals = self
            .cache
            .borrow_mut()
            .call_sol(source, aaveSynchronicityDecimalsCall {})
            .map_err(provider_error)?;
        let description = self
            .cache
            .borrow_mut()
            .call_sol(source, aaveSynchronicityDescriptionCall {})
            .map_err(provider_error)?;
        let latest_answer = self
            .cache
            .borrow_mut()
            .call_sol(source, aaveSynchronicityLatestAnswerCall {})
            .map_err(provider_error)?;

        Ok(AaveSynchronicityPegToBaseSource {
            asset_to_peg_proxy,
            peg_to_base_proxy,
            decimals,
            description,
            latest_answer,
        })
    }

    #[cfg(feature = "aave")]
    pub(crate) fn read_aave_fixed_price(
        &self,
        source: Address,
    ) -> Result<AaveFixedPriceSource, OracleError> {
        if self.has_known_aave_dependency_or_config(source) {
            return Err(OracleError::Provider(
                "source exposes dependency/config view(s); not fixed-price".to_string(),
            ));
        }
        let fixed_price = self.read_fixed_price_value(source)?;
        let decimals = self
            .cache
            .borrow_mut()
            .call_sol(source, aaveFixedDecimalsCall {})
            .map_err(provider_error)?;
        let description = self
            .cache
            .borrow_mut()
            .call_sol(source, aaveFixedDescriptionCall {})
            .map_err(provider_error)?;
        let latest_answer = self
            .cache
            .borrow_mut()
            .call_sol(source, aaveFixedLatestAnswerCall {})
            .map_err(provider_error)?;
        if latest_answer != fixed_price {
            return Err(OracleError::Provider(
                "latestAnswer() does not match fixed price getter".to_string(),
            ));
        }

        Ok(AaveFixedPriceSource {
            decimals,
            description,
            latest_answer,
        })
    }

    #[cfg(feature = "aave")]
    fn read_fixed_price_value(&self, source: Address) -> Result<I256, OracleError> {
        match self
            .cache
            .borrow_mut()
            .call_sol(source, aaveFixedPriceCall {})
        {
            Ok(price) => Ok(price),
            Err(price_error) => match self.cache.borrow_mut().call_sol(source, PRICECall {}) {
                Ok(price) => Ok(price),
                Err(constant_error) => Err(OracleError::Provider(format!(
                    "missing fixed-price getter price() ({price_error:?}) or PRICE() ({constant_error:?})"
                ))),
            },
        }
    }

    #[cfg(feature = "aave")]
    fn has_known_aave_dependency_or_config(&self, source: Address) -> bool {
        self.cache
            .borrow_mut()
            .call_sol(source, ASSET_TO_USD_AGGREGATORCall {})
            .is_ok()
            || self
                .cache
                .borrow_mut()
                .call_sol(source, BASE_TO_USD_AGGREGATORCall {})
                .is_ok()
            || self
                .cache
                .borrow_mut()
                .call_sol(source, ASSET_TO_PEGCall {})
                .is_ok()
            || self
                .cache
                .borrow_mut()
                .call_sol(source, PEG_TO_BASECall {})
                .is_ok()
            || self
                .cache
                .borrow_mut()
                .call_sol(source, BASE_TO_PEGCall {})
                .is_ok()
            || self
                .cache
                .borrow_mut()
                .call_sol(source, REFERENCE_FEEDCall {})
                .is_ok()
            || self
                .cache
                .borrow_mut()
                .call_sol(source, DISCOUNT_RATECall {})
                .is_ok()
            || self
                .cache
                .borrow_mut()
                .call_sol(source, discountCall {})
                .is_ok()
            || self
                .cache
                .borrow_mut()
                .call_sol(source, EXCHANGE_RATECall {})
                .is_ok()
            || self
                .cache
                .borrow_mut()
                .call_sol(source, PENDLE_PRINCIPAL_TOKENCall {})
                .is_ok()
            || self
                .cache
                .borrow_mut()
                .call_sol(source, PENDLE_ORACLECall {})
                .is_ok()
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
#[derive(Clone, Debug)]
pub struct OracleAdapterBuilder {
    feeds: Vec<Feed>,
    now_timestamp: Option<u64>,
    multicall_reads: bool,
}

impl Default for OracleAdapterBuilder {
    fn default() -> Self {
        Self {
            feeds: Vec::new(),
            now_timestamp: None,
            multicall_reads: true,
        }
    }
}

/// Best-effort cache-native oracle registration report.
#[non_exhaustive]
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
#[non_exhaustive]
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
    /// The Morpho oracle source variant is not supported by this adapter.
    UnsupportedMorphoSource {
        /// Classified discovery or compatibility error.
        error: String,
    },
    /// The Euler oracle source variant is not supported by this adapter.
    UnsupportedEulerSource {
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

    /// Enable or disable direct/fork Multicall3 batching for cache-native Chainlink view reads.
    ///
    /// Enabled by default. When enabled, feed registration first batches proxy
    /// `decimals`, `description`, `version`, `latestRoundData`, `aggregator`,
    /// and aggregator `typeAndVersion` reads through direct RPC Multicall3,
    /// falling back to fork-cache Multicall3 and then sequential cache calls
    /// when direct RPC batching cannot be used.
    pub fn multicall_reads(mut self, enabled: bool) -> Self {
        self.multicall_reads = enabled;
        self
    }

    /// Disable direct/fork Multicall3 view batching for cache-native Chainlink registration.
    pub fn disable_multicall_reads(self) -> Self {
        self.multicall_reads(false)
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
        let reader = EvmCacheChainlinkReader::new(cache).with_multicall_reads(self.multicall_reads);
        let mut registry = OracleRegistry::new_at_timestamp(now_timestamp);
        let skipped = reader.register_feeds(&mut registry, self.feeds).await?;
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
            Self::UnsupportedMorphoSource { error } => {
                write!(f, "unsupported Morpho oracle source ({error})")
            }
            Self::UnsupportedEulerSource { error } => {
                write!(f, "unsupported Euler oracle source ({error})")
            }
        }
    }
}

fn u64_from_u256(value: U256, field: &'static str) -> Result<u64, OracleError> {
    u64::try_from(value).map_err(|_| OracleError::Provider(format!("{field} does not fit in u64")))
}

fn round_from_raw(
    round: <latestRoundDataCall as SolCall>::Return,
) -> Result<RoundData, OracleError> {
    Ok(RoundData {
        round_id: U256::from(round.roundId),
        answer: round.answer,
        started_at: u64_from_u256(round.startedAt, "startedAt")?,
        updated_at: u64_from_u256(round.updatedAt, "updatedAt")?,
        answered_in_round: U256::from(round.answeredInRound),
    })
}

fn multicall_call<C: SolCall>(
    target: Address,
    call: C,
    allow_failure: bool,
) -> (Address, Bytes, bool) {
    (target, Bytes::from(call.abi_encode()), allow_failure)
}

fn provider_error(error: impl std::fmt::Debug) -> OracleError {
    OracleError::Provider(format!("{error:?}"))
}
