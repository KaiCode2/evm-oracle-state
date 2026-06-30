use std::collections::BTreeMap;

use alloy_primitives::{Address, B256, I256, U256};

use crate::{
    AggregatorLayout, AggregatorLayoutConfidence, AggregatorLayoutEvidence, ChainlinkFeedProvider,
    FeedConfig, FeedMetadata, OracleBlockRef, OracleError, OracleSnapshot, RoundData,
    classify_type_and_version,
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

    /// Return true when this source is a plain identity Chainlink source.
    pub fn is_identity(&self) -> bool {
        matches!(self, Self::Chainlink)
    }

    /// Proxy that should be read for authoritative Chainlink round data.
    pub fn read_proxy(&self, registration_proxy: Address) -> Address {
        match self {
            Self::Chainlink => registration_proxy,
            Self::AavePriceCapStable {
                underlying_proxy, ..
            } => *underlying_proxy,
        }
    }

    /// Apply the source transform to an event or read answer.
    pub fn normalize_answer(&self, answer: I256) -> I256 {
        match self {
            Self::Chainlink => answer,
            Self::AavePriceCapStable { price_cap, .. } if answer > *price_cap => *price_cap,
            Self::AavePriceCapStable { .. } => answer,
        }
    }

    /// Apply the source transform to round data.
    pub fn normalize_round(&self, mut round: RoundData) -> RoundData {
        round.answer = self.normalize_answer(round.answer);
        round
    }
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
            id,
            config,
            source,
            metadata,
            round,
            current_aggregator,
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
            id,
            config,
            source,
            metadata,
            round,
            current_aggregator,
        )
        .await
    }

    async fn insert_prepared_feed<P: ChainlinkFeedProvider>(
        &mut self,
        provider: &P,
        id: FeedId,
        config: FeedConfig,
        source: FeedSource,
        metadata: FeedMetadata,
        round: RoundData,
        current_aggregator: Option<Address>,
    ) -> Result<FeedId, OracleError> {
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
