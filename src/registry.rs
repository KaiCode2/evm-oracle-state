use std::collections::BTreeMap;

use alloy_primitives::{Address, B256, I256, U256};

use crate::{
    AggregatorLayout, AggregatorLayoutConfidence, AggregatorLayoutEvidence, ChainlinkFeedProvider,
    FeedConfig, FeedMetadata, OracleBlockRef, OracleError, OracleSnapshot, OracleSourceDescriptor,
    OracleTransformDescriptor, RoundData, classify_type_and_version,
};

/// Stable id for a registered feed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FeedId(&'static str);

impl FeedId {
    /// Create a new feed id.
    pub fn new(id: impl Into<String>) -> Self {
        Self(Box::leak(id.into().into_boxed_str()))
    }

    /// Borrow the string id.
    pub fn as_str(&self) -> &str {
        self.0
    }
}

impl std::fmt::Display for FeedId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Network source and transform for a registered feed.
#[non_exhaustive]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum FeedSource {
    /// Plain Chainlink-compatible proxy with identity answer transform.
    #[default]
    Chainlink,
    /// Aave `PriceCapAdapterStable` source backed by an underlying Chainlink proxy.
    AavePriceCapStable {
        /// User-facing Aave source adapter.
        source: Address,
        /// Underlying Chainlink proxy read for rounds and aggregator discovery.
        underlying_proxy: Address,
        /// Maximum answer exposed by the Aave adapter.
        price_cap: I256,
    },
    /// Aave ratio-cap/CAPO source backed by a base-to-USD Chainlink proxy.
    AaveRatioCap {
        /// User-facing Aave source adapter.
        source: Address,
        /// Underlying base-to-USD Chainlink proxy.
        base_to_usd_proxy: Address,
        /// Ratio provider used by the Aave source.
        ratio_provider: Address,
        /// Current source ratio.
        current_ratio: I256,
        /// Maximum ratio exposed by the Aave source.
        max_ratio: I256,
        /// Decimals used by the ratio values.
        ratio_decimals: u8,
    },
    /// Aave Chainlink synchronicity adapter composing asset-to-peg and peg-to-base feeds.
    AaveSynchronicityPegToBase {
        /// User-facing Aave source adapter.
        source: Address,
        /// Asset-to-peg Chainlink proxy.
        asset_to_peg_proxy: Address,
        /// Asset-to-peg current aggregator.
        asset_to_peg_aggregator: Address,
        /// Seeded asset-to-peg answer used when the other dependency updates.
        asset_to_peg_answer: I256,
        /// Asset-to-peg decimals.
        asset_to_peg_decimals: u8,
        /// Peg-to-base Chainlink proxy.
        peg_to_base_proxy: Address,
        /// Peg-to-base current aggregator.
        peg_to_base_aggregator: Address,
        /// Seeded peg-to-base answer used when the other dependency updates.
        peg_to_base_answer: I256,
        /// Peg-to-base decimals.
        peg_to_base_decimals: u8,
        /// Decimals for the user-facing composed answer.
        output_decimals: u8,
    },
    /// Aave fixed-price source with no Chainlink dependency.
    AaveFixedPrice {
        /// User-facing Aave source adapter.
        source: Address,
        /// Fixed user-facing answer.
        price: I256,
    },
    /// Adapter-owned oracle source.
    Custom(OracleSourceDescriptor),
}

impl FeedSource {
    /// Construct an Aave capped stable source transform.
    pub fn aave_price_cap_stable(
        source: Address,
        underlying_proxy: Address,
        price_cap: I256,
    ) -> Self {
        Self::AavePriceCapStable {
            source,
            underlying_proxy,
            price_cap,
        }
    }

    /// Construct an Aave ratio-cap/CAPO source transform.
    pub fn aave_ratio_cap(
        source: Address,
        base_to_usd_proxy: Address,
        ratio_provider: Address,
        current_ratio: I256,
        max_ratio: I256,
        ratio_decimals: u8,
    ) -> Self {
        Self::AaveRatioCap {
            source,
            base_to_usd_proxy,
            ratio_provider,
            current_ratio,
            max_ratio,
            ratio_decimals,
        }
    }

    /// Construct an Aave peg-to-base synchronicity source transform.
    #[allow(clippy::too_many_arguments)]
    pub fn aave_synchronicity_peg_to_base(
        source: Address,
        asset_to_peg_proxy: Address,
        asset_to_peg_aggregator: Address,
        asset_to_peg_answer: I256,
        asset_to_peg_decimals: u8,
        peg_to_base_proxy: Address,
        peg_to_base_aggregator: Address,
        peg_to_base_answer: I256,
        peg_to_base_decimals: u8,
        output_decimals: u8,
    ) -> Self {
        Self::AaveSynchronicityPegToBase {
            source,
            asset_to_peg_proxy,
            asset_to_peg_aggregator,
            asset_to_peg_answer,
            asset_to_peg_decimals,
            peg_to_base_proxy,
            peg_to_base_aggregator,
            peg_to_base_answer,
            peg_to_base_decimals,
            output_decimals,
        }
    }

    /// Construct an Aave fixed-price source.
    pub fn aave_fixed_price(source: Address, price: I256) -> Self {
        Self::AaveFixedPrice { source, price }
    }

    /// Construct an adapter-owned custom source.
    pub fn custom(descriptor: OracleSourceDescriptor) -> Self {
        Self::Custom(descriptor)
    }

    /// Return true when this source is a plain identity Chainlink source.
    pub fn is_identity(&self) -> bool {
        matches!(self, Self::Chainlink)
    }

    /// Return true when the built-in Chainlink reactive handler should route this source.
    pub fn uses_builtin_chainlink_handler(&self) -> bool {
        matches!(
            self,
            Self::Chainlink
                | Self::AavePriceCapStable { .. }
                | Self::AaveRatioCap { .. }
                | Self::AaveSynchronicityPegToBase { .. }
        )
    }

    /// Return true when this source can be reconciled by reading a Chainlink proxy.
    pub fn supports_proxy_reconciliation(&self) -> bool {
        !matches!(self, Self::AaveFixedPrice { .. })
    }

    /// Proxy that should be read for authoritative Chainlink round data.
    pub fn read_proxy(&self, registration_proxy: Address) -> Address {
        match self {
            Self::Chainlink => registration_proxy,
            Self::AavePriceCapStable {
                underlying_proxy, ..
            } => *underlying_proxy,
            Self::AaveRatioCap {
                base_to_usd_proxy, ..
            } => *base_to_usd_proxy,
            Self::AaveSynchronicityPegToBase {
                asset_to_peg_proxy, ..
            } => *asset_to_peg_proxy,
            Self::AaveFixedPrice { source, .. } => *source,
            Self::Custom(descriptor) => descriptor.read_proxy,
        }
    }

    /// Aggregators whose Chainlink events should drive this source.
    pub fn event_aggregators(&self, current_aggregator: Option<Address>) -> Vec<Address> {
        match self {
            Self::AaveSynchronicityPegToBase {
                asset_to_peg_aggregator,
                peg_to_base_aggregator,
                ..
            } => vec![*asset_to_peg_aggregator, *peg_to_base_aggregator],
            Self::AaveFixedPrice { .. } => Vec::new(),
            _ => current_aggregator.into_iter().collect(),
        }
    }

    /// Return true when an event from `aggregator` is acceptable for this source.
    pub fn accepts_event_from(
        &self,
        current_aggregator: Option<Address>,
        aggregator: Address,
    ) -> bool {
        self.event_aggregators(current_aggregator)
            .into_iter()
            .any(|event_aggregator| event_aggregator == aggregator)
    }

    /// Return true when this source should use AnswerUpdated events for `aggregator`.
    pub fn wants_answer_updated_from(
        &self,
        current_aggregator: Option<Address>,
        aggregator: Address,
    ) -> bool {
        match self {
            Self::AaveRatioCap { .. } | Self::AaveSynchronicityPegToBase { .. } => {
                self.accepts_event_from(current_aggregator, aggregator)
            }
            _ => false,
        }
    }

    /// Return true when Chainlink storage adapters can safely mirror this event.
    pub fn supports_direct_chainlink_storage_effects(&self) -> bool {
        !matches!(
            self,
            Self::AaveRatioCap { .. } | Self::AaveSynchronicityPegToBase { .. }
        )
    }

    /// Apply the source transform to an event or read answer.
    pub fn normalize_answer(&self, answer: I256) -> I256 {
        self.normalize_answer_from_event(None, answer)
    }

    /// Apply the source transform to an answer emitted by a specific dependency aggregator.
    pub fn normalize_answer_from_event(&self, aggregator: Option<Address>, answer: I256) -> I256 {
        match self {
            Self::Chainlink => answer,
            Self::AavePriceCapStable { price_cap, .. } if answer > *price_cap => *price_cap,
            Self::AavePriceCapStable { .. } => answer,
            Self::AaveRatioCap {
                current_ratio,
                max_ratio,
                ratio_decimals,
                ..
            } => {
                if answer <= I256::ZERO || *current_ratio <= I256::ZERO {
                    return I256::ZERO;
                }
                let capped_ratio = if current_ratio < max_ratio {
                    *current_ratio
                } else {
                    *max_ratio
                };
                if capped_ratio <= I256::ZERO {
                    return I256::ZERO;
                }
                div_or_zero(
                    answer.saturating_mul(capped_ratio),
                    decimal_scale(*ratio_decimals),
                )
            }
            Self::AaveSynchronicityPegToBase {
                asset_to_peg_aggregator,
                asset_to_peg_answer,
                asset_to_peg_decimals,
                peg_to_base_aggregator,
                peg_to_base_answer,
                peg_to_base_decimals,
                output_decimals,
                ..
            } => {
                let asset_to_peg = if aggregator == Some(*asset_to_peg_aggregator) {
                    answer
                } else {
                    *asset_to_peg_answer
                };
                let peg_to_base = if aggregator == Some(*peg_to_base_aggregator) {
                    answer
                } else {
                    *peg_to_base_answer
                };
                normalize_synchronicity_answer(
                    asset_to_peg,
                    *asset_to_peg_decimals,
                    peg_to_base,
                    *peg_to_base_decimals,
                    *output_decimals,
                )
            }
            Self::AaveFixedPrice { price, .. } => *price,
            Self::Custom(descriptor) => match &descriptor.transform {
                OracleTransformDescriptor::Identity | OracleTransformDescriptor::Custom { .. } => {
                    answer
                }
                OracleTransformDescriptor::PriceCap { cap } if answer > *cap => *cap,
                OracleTransformDescriptor::PriceCap { .. } => answer,
            },
        }
    }

    /// Apply the source transform to round data.
    pub fn normalize_round(&self, mut round: RoundData) -> RoundData {
        round.answer = match self {
            Self::AaveSynchronicityPegToBase {
                asset_to_peg_aggregator,
                ..
            } => self.normalize_answer_from_event(Some(*asset_to_peg_aggregator), round.answer),
            _ => self.normalize_answer(round.answer),
        };
        round
    }

    pub(crate) fn normalize_dependency_answers(
        &self,
        asset_to_peg_answer: I256,
        peg_to_base_answer: I256,
    ) -> Option<I256> {
        match self {
            Self::AaveSynchronicityPegToBase {
                asset_to_peg_decimals,
                peg_to_base_decimals,
                output_decimals,
                ..
            } => Some(normalize_synchronicity_answer(
                asset_to_peg_answer,
                *asset_to_peg_decimals,
                peg_to_base_answer,
                *peg_to_base_decimals,
                *output_decimals,
            )),
            _ => None,
        }
    }

    pub(crate) fn with_synchronicity_dependency_state(
        &self,
        asset_to_peg_aggregator: Address,
        asset_to_peg_answer: I256,
        peg_to_base_aggregator: Address,
        peg_to_base_answer: I256,
    ) -> Option<Self> {
        match self {
            Self::AaveSynchronicityPegToBase {
                source,
                asset_to_peg_proxy,
                asset_to_peg_decimals,
                peg_to_base_proxy,
                peg_to_base_decimals,
                output_decimals,
                ..
            } => Some(Self::AaveSynchronicityPegToBase {
                source: *source,
                asset_to_peg_proxy: *asset_to_peg_proxy,
                asset_to_peg_aggregator,
                asset_to_peg_answer,
                asset_to_peg_decimals: *asset_to_peg_decimals,
                peg_to_base_proxy: *peg_to_base_proxy,
                peg_to_base_aggregator,
                peg_to_base_answer,
                peg_to_base_decimals: *peg_to_base_decimals,
                output_decimals: *output_decimals,
            }),
            _ => None,
        }
    }
}

fn decimal_scale(decimals: u8) -> I256 {
    let mut scale = I256::unchecked_from(1_i8);
    for _ in 0..decimals {
        scale = scale.saturating_mul(I256::unchecked_from(10_i8));
    }
    scale
}

fn div_or_zero(numerator: I256, denominator: I256) -> I256 {
    if denominator == I256::ZERO {
        I256::ZERO
    } else {
        numerator / denominator
    }
}

fn normalize_synchronicity_answer(
    asset_to_peg: I256,
    asset_to_peg_decimals: u8,
    peg_to_base: I256,
    peg_to_base_decimals: u8,
    output_decimals: u8,
) -> I256 {
    if asset_to_peg <= I256::ZERO || peg_to_base <= I256::ZERO {
        return I256::ZERO;
    }
    let numerator = asset_to_peg
        .saturating_mul(peg_to_base)
        .saturating_mul(decimal_scale(output_decimals));
    let denominator = decimal_scale(asset_to_peg_decimals.saturating_add(peg_to_base_decimals));
    div_or_zero(numerator, denominator)
}

/// Registered Chainlink-compatible proxy feed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedRegistration {
    /// Stable feed id.
    pub id: FeedId,
    /// User-facing proxy address.
    pub proxy: Address,
    /// Optional human-readable label.
    pub label: Option<String>,
    /// Optional base symbol.
    pub base: Option<String>,
    /// Optional quote symbol.
    pub quote: Option<String>,
    /// Freshness and answer validity policy.
    pub staleness: crate::StalenessPolicy,
    /// Best-known current aggregator.
    pub current_aggregator: Option<Address>,
    /// Best-known current aggregator layout evidence.
    pub aggregator_layout: Option<AggregatorLayoutEvidence>,
    /// Metadata loaded from the proxy.
    pub metadata: FeedMetadata,
    /// Network source and transform used by this registration.
    pub source: FeedSource,
}

/// Aggregator routing change found during reconciliation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AggregatorChange {
    /// Feed id whose aggregator changed.
    pub id: FeedId,
    /// Feed proxy.
    pub proxy: Address,
    /// Previous aggregator.
    pub old: Option<Address>,
    /// New aggregator.
    pub new: Option<Address>,
}

struct PreparedFeed {
    id: FeedId,
    config: FeedConfig,
    source: FeedSource,
    metadata: FeedMetadata,
    round: RoundData,
    current_aggregator: Option<Address>,
}

/// Registry plus latest proxy-read snapshots.
#[derive(Clone, Debug)]
pub struct OracleRegistry {
    registrations: BTreeMap<FeedId, FeedRegistration>,
    ids_by_proxy: BTreeMap<Address, FeedId>,
    snapshots_by_proxy: BTreeMap<Address, OracleSnapshot>,
    layouts_by_code_hash: BTreeMap<B256, AggregatorLayout>,
    now_timestamp: u64,
}

impl OracleRegistry {
    /// Create an empty registry with a fixed wall-clock timestamp for tests.
    pub fn new_at_timestamp(now_timestamp: u64) -> Self {
        Self {
            registrations: BTreeMap::new(),
            ids_by_proxy: BTreeMap::new(),
            snapshots_by_proxy: BTreeMap::new(),
            layouts_by_code_hash: BTreeMap::new(),
            now_timestamp,
        }
    }

    /// Register a Chainlink-compatible proxy and seed state from proxy reads.
    pub async fn register_chainlink_feed<P: ChainlinkFeedProvider>(
        &mut self,
        provider: &P,
        config: FeedConfig,
    ) -> Result<FeedId, OracleError> {
        let id = self.prepare_feed_id(&config)?;
        let source = FeedSource::Chainlink;
        let read_proxy = source.read_proxy(config.proxy);
        let metadata = FeedMetadata {
            decimals: provider.decimals(read_proxy).await?,
            description: provider.description(read_proxy).await?,
            version: provider.version(read_proxy).await?,
        };
        let round = source.normalize_round(provider.latest_round_data(read_proxy).await?);
        let current_aggregator = provider.aggregator(read_proxy).await.unwrap_or(None);
        self.insert_prepared_feed(
            provider,
            PreparedFeed {
                id,
                config,
                source,
                metadata,
                round,
                current_aggregator,
            },
        )
        .await
    }

    #[cfg(feature = "aave")]
    pub(crate) async fn register_discovered_feed<P: ChainlinkFeedProvider>(
        &mut self,
        provider: &P,
        config: FeedConfig,
        source: FeedSource,
        metadata: FeedMetadata,
        round: RoundData,
        current_aggregator: Option<Address>,
    ) -> Result<FeedId, OracleError> {
        let id = self.prepare_feed_id(&config)?;
        self.insert_prepared_feed(
            provider,
            PreparedFeed {
                id,
                config,
                source,
                metadata,
                round,
                current_aggregator,
            },
        )
        .await
    }

    async fn insert_prepared_feed<P: ChainlinkFeedProvider>(
        &mut self,
        provider: &P,
        prepared: PreparedFeed,
    ) -> Result<FeedId, OracleError> {
        let PreparedFeed {
            id,
            config,
            source,
            metadata,
            round,
            current_aggregator,
        } = prepared;
        let aggregator_layout = self
            .detect_aggregator_layout(provider, current_aggregator, None)
            .await;

        let registration = FeedRegistration {
            id,
            proxy: config.proxy,
            label: config.label,
            base: config.base,
            quote: config.quote,
            staleness: config.staleness,
            current_aggregator,
            aggregator_layout,
            metadata: metadata.clone(),
            source,
        };
        let snapshot = OracleSnapshot::proxy_read(
            id,
            registration.proxy,
            current_aggregator,
            metadata,
            round,
            self.now_timestamp,
            &registration.staleness,
        );

        self.ids_by_proxy.insert(registration.proxy, id);
        self.snapshots_by_proxy.insert(registration.proxy, snapshot);
        self.registrations.insert(id, registration);
        Ok(id)
    }

    fn prepare_feed_id(&self, config: &FeedConfig) -> Result<FeedId, OracleError> {
        if self.ids_by_proxy.contains_key(&config.proxy) {
            return Err(OracleError::DuplicateProxy(config.proxy));
        }

        let id = config
            .id
            .unwrap_or_else(|| derive_feed_id(config.label.as_deref(), config.proxy));
        if self.registrations.contains_key(&id) {
            return Err(OracleError::DuplicateFeedId(id.to_string()));
        }
        Ok(id)
    }

    /// Return a registration by id.
    pub fn registration(&self, id: FeedId) -> Option<&FeedRegistration> {
        self.registrations.get(&id)
    }

    /// Return the latest snapshot by proxy.
    pub fn latest(&self, proxy: Address) -> Option<&OracleSnapshot> {
        self.snapshots_by_proxy.get(&proxy)
    }

    pub(crate) fn now_timestamp(&self) -> u64 {
        self.now_timestamp
    }

    pub(crate) fn registrations_map(&self) -> &BTreeMap<FeedId, FeedRegistration> {
        &self.registrations
    }

    pub(crate) fn registrations_map_mut(&mut self) -> &mut BTreeMap<FeedId, FeedRegistration> {
        &mut self.registrations
    }

    pub(crate) fn snapshots_by_proxy_mut(&mut self) -> &mut BTreeMap<Address, OracleSnapshot> {
        &mut self.snapshots_by_proxy
    }

    pub(crate) fn id_by_proxy(&self, proxy: Address) -> Option<&FeedId> {
        self.ids_by_proxy.get(&proxy)
    }

    pub(crate) fn record_proxy_id(&mut self, proxy: Address, id: FeedId) {
        self.ids_by_proxy.insert(proxy, id);
    }

    pub(crate) fn replace_snapshot(&mut self, snapshot: OracleSnapshot) {
        self.snapshots_by_proxy.insert(snapshot.proxy, snapshot);
    }

    pub(crate) async fn detect_aggregator_layout<P: ChainlinkFeedProvider>(
        &mut self,
        provider: &P,
        aggregator: Option<Address>,
        block: Option<OracleBlockRef>,
    ) -> Option<AggregatorLayoutEvidence> {
        let aggregator = aggregator?;
        let code_hash = provider
            .aggregator_code_hash_at(aggregator, block)
            .await
            .unwrap_or(None);
        let type_and_version = provider
            .aggregator_type_and_version_at(aggregator, block)
            .await
            .unwrap_or(None);

        if let Some(type_and_version) = type_and_version {
            let layout = classify_type_and_version(&type_and_version);
            if let Some(code_hash) = code_hash
                && is_cacheable_layout(layout)
            {
                self.layouts_by_code_hash.insert(code_hash, layout);
            }
            return Some(AggregatorLayoutEvidence {
                aggregator,
                type_and_version: Some(type_and_version),
                code_hash,
                layout,
                confidence: AggregatorLayoutConfidence::TypeAndVersion,
            });
        }

        if let Some(code_hash) = code_hash {
            if let Some(layout) = self.layouts_by_code_hash.get(&code_hash).copied() {
                return Some(AggregatorLayoutEvidence::from_code_hash_cache(
                    aggregator, code_hash, layout,
                ));
            }
            return Some(AggregatorLayoutEvidence::unknown(
                aggregator,
                Some(code_hash),
            ));
        }

        Some(AggregatorLayoutEvidence::unknown(aggregator, None))
    }

    /// Return cloned registrations for routing rebuilds.
    pub fn registrations(&self) -> impl Iterator<Item = FeedRegistration> + '_ {
        self.registrations.values().cloned()
    }
}

fn is_cacheable_layout(layout: AggregatorLayout) -> bool {
    matches!(
        layout,
        AggregatorLayout::ChainlinkOcr2V1
            | AggregatorLayout::ChainlinkOcr1V2
            | AggregatorLayout::ChainlinkOcr1V3
            | AggregatorLayout::ChainlinkOcr1V4
    )
}

pub(crate) fn snapshot_from_proxy_read(
    registration: &FeedRegistration,
    round: RoundData,
    aggregator: Option<Address>,
    now_timestamp: u64,
) -> OracleSnapshot {
    OracleSnapshot::proxy_read(
        registration.id,
        registration.proxy,
        aggregator,
        registration.metadata.clone(),
        round,
        now_timestamp,
        &registration.staleness,
    )
}

fn derive_feed_id(label: Option<&str>, proxy: Address) -> FeedId {
    if let Some(label) = label {
        let slug = label
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() {
                    ch.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect::<String>()
            .trim_matches('-')
            .to_string();
        if !slug.is_empty() {
            return FeedId::new(slug);
        }
    }

    FeedId::new(format!("{proxy:?}"))
}

#[allow(dead_code)]
fn _u256_is_part_of_public_metadata(_: U256) {}
