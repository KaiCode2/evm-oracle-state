use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use alloy_network::Ethereum;
use alloy_primitives::{Address, B256, I256, U256};
use alloy_rpc_types_eth::Filter;
use alloy_sol_types::sol;
use evm_fork_cache::{
    StateView,
    cache::EvmCache,
    reactive::{
        ChainStatus, HandlerError, HandlerId, HandlerOutcome, HookSignal, InvalidationReason,
        InvalidationRequest, LogInterest, ReactiveContext, ReactiveEffect, ReactiveHandler,
        ReactiveInput, ReactiveInterest, ReportTag, RouteKeySpec, StateEffectQuality,
    },
    state_update::PurgeScope,
};

use crate::{
    ANSWER_UPDATED_TOPIC, AdapterFuture, AssetId, Denomination, EvmCacheChainlinkReader, Feed,
    FeedConfig, FeedId, FeedMetadata, FeedRegistration, FeedSource, ORACLE_SIGNAL_NAMESPACE,
    OracleAdapterFeedSkip, OracleAdapterId, OracleAdapterPlugin, OracleAdapterSkipReason,
    OracleDiscoveredFeed, OracleDiscoveryContext, OracleDiscoveryReport, OracleError,
    OraclePriceUpdate, OracleSignalKind, OracleSnapshotStatus, OracleStorageSync,
    REDSTONE_VALUE_UPDATE_TOPIC, RedstoneValueUpdate, StalenessPolicy, decode_answer_updated,
    decode_redstone_value_update,
};

sol! {
    interface RedstonePriceFeedInterface {
        function getDataFeedId() external view returns (bytes32);
        function getPriceFeedAdapter() external view returns (address);
    }
}

use RedstonePriceFeedInterface::{getDataFeedIdCall, getPriceFeedAdapterCall};

const ADAPTER_ID: &str = "evm-oracle-state.redstone";
const HANDLER_ID: &str = "evm-oracle-state.redstone";
const REDSTONE_NO_ROUNDS_ROUND_ID: u64 = 1;

/// Declarative RedStone push-feed registration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RedstoneFeed {
    price_feed: Address,
    adapter: Option<Address>,
    data_feed_id: Option<B256>,
    feed_id: Option<FeedId>,
    label: Option<String>,
    base: Option<AssetId>,
    quote: Option<Denomination>,
    staleness: StalenessPolicy,
}

impl RedstoneFeed {
    /// Register a RedStone price-feed wrapper and discover adapter/feed id from it.
    pub fn new(price_feed: Address) -> Self {
        Self {
            price_feed,
            adapter: None,
            data_feed_id: None,
            feed_id: None,
            label: None,
            base: None,
            quote: None,
            staleness: StalenessPolicy::default(),
        }
    }

    /// Register a RedStone price-feed wrapper and discover adapter/feed id from it.
    pub fn price_feed(price_feed: Address) -> Self {
        Self::new(price_feed)
    }

    /// Register a RedStone price-feed wrapper with an explicit adapter and data feed id.
    pub fn push(price_feed: Address, adapter: Address, data_feed_id: B256) -> Self {
        Self::new(price_feed)
            .adapter(adapter)
            .data_feed_id(data_feed_id)
    }

    /// Return the user-facing price-feed wrapper.
    pub fn price_feed_address(&self) -> Address {
        self.price_feed
    }

    /// Set the RedStone adapter/event-source address.
    pub fn adapter(mut self, adapter: Address) -> Self {
        self.adapter = Some(adapter);
        self
    }

    /// Set the RedStone data feed id.
    pub fn data_feed_id(mut self, data_feed_id: B256) -> Self {
        self.data_feed_id = Some(data_feed_id);
        self
    }

    /// Set a stable feed id.
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.feed_id = Some(FeedId::new(id));
        self
    }

    /// Set a stable feed id.
    pub fn feed_id(mut self, id: FeedId) -> Self {
        self.feed_id = Some(id);
        self
    }

    /// Set a human-readable label.
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Set the base asset label.
    pub fn base(mut self, base: AssetId) -> Self {
        self.base = Some(base);
        self
    }

    /// Set the quote denomination label.
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

    fn feed_for_skip(&self) -> Feed {
        let mut feed = Feed::proxy(self.price_feed);
        if let Some(id) = self.feed_id {
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

    fn config(&self) -> FeedConfig {
        FeedConfig {
            proxy: self.price_feed,
            id: self.feed_id,
            label: self.label.clone(),
            base: self.base.clone().map(String::from),
            quote: self.quote.clone().map(String::from),
            staleness: self.staleness,
        }
    }
}

/// Cache-backed RedStone push oracle discovery adapter.
#[derive(Clone, Debug, Default)]
pub struct RedstoneOracleAdapter {
    feeds: Vec<RedstoneFeed>,
    now_timestamp: Option<u64>,
}

impl RedstoneOracleAdapter {
    /// Create an empty RedStone adapter.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create an adapter for one RedStone price-feed wrapper.
    pub fn price_feed(price_feed: Address) -> Self {
        Self::new().feed(RedstoneFeed::new(price_feed))
    }

    /// Add one RedStone feed.
    pub fn feed(mut self, feed: RedstoneFeed) -> Self {
        self.feeds.push(feed);
        self
    }

    /// Add multiple RedStone feeds.
    pub fn feeds(mut self, feeds: impl IntoIterator<Item = RedstoneFeed>) -> Self {
        self.feeds.extend(feeds);
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

    fn discover_feeds(
        &self,
        cache: &mut EvmCache,
        _now_timestamp: u64,
    ) -> Result<OracleDiscoveryReport, OracleError> {
        let mut report = OracleDiscoveryReport::new();
        for feed in &self.feeds {
            match self.discover_feed(cache, feed) {
                Ok(discovered) => report = report.with_feed(discovered),
                Err(error) => {
                    report = report.with_skip(OracleAdapterFeedSkip {
                        feed: feed.feed_for_skip(),
                        proxy: feed.price_feed,
                        reason: OracleAdapterSkipReason::UnsupportedRedstoneSource {
                            error: error.to_string(),
                        },
                    });
                }
            }
        }
        Ok(report)
    }

    fn discover_feed(
        &self,
        cache: &mut EvmCache,
        feed: &RedstoneFeed,
    ) -> Result<OracleDiscoveredFeed, OracleError> {
        let data_feed_id = match feed.data_feed_id {
            Some(data_feed_id) => data_feed_id,
            None => cache
                .call_sol(feed.price_feed, getDataFeedIdCall {})
                .map_err(provider_error)?,
        };
        let adapter = match feed.adapter {
            Some(adapter) => adapter,
            None => cache
                .call_sol(feed.price_feed, getPriceFeedAdapterCall {})
                .map_err(provider_error)?,
        };

        let reader = EvmCacheChainlinkReader::new(cache);
        let metadata = FeedMetadata {
            decimals: reader.read_decimals(feed.price_feed)?,
            description: reader.read_description(feed.price_feed)?,
            version: reader.read_version(feed.price_feed)?,
        };
        let round = reader.read_latest_round_data(feed.price_feed)?;
        let config = feed.config();
        let id = config
            .id
            .unwrap_or_else(|| derive_redstone_feed_id(config.label.as_deref(), feed.price_feed));
        let registration = FeedRegistration {
            id,
            proxy: config.proxy,
            label: config.label,
            base: config.base,
            quote: config.quote,
            staleness: config.staleness,
            current_aggregator: Some(adapter),
            aggregator_layout: None,
            metadata,
            source: FeedSource::redstone_push(feed.price_feed, adapter, data_feed_id),
        };

        Ok(OracleDiscoveredFeed::new(registration, round))
    }
}

impl OracleAdapterPlugin for RedstoneOracleAdapter {
    fn adapter_id(&self) -> OracleAdapterId {
        OracleAdapterId::new(ADAPTER_ID)
    }

    fn discover<'a>(
        &'a self,
        ctx: OracleDiscoveryContext<'a>,
    ) -> AdapterFuture<'a, OracleDiscoveryReport> {
        Box::pin(async move {
            let now_timestamp = self.timestamp(Some(ctx.now_timestamp))?;
            self.discover_feeds(ctx.cache, now_timestamp)
        })
    }

    fn reactive_handler(
        &self,
        registrations: Vec<FeedRegistration>,
        _storage_sync: OracleStorageSync,
    ) -> Arc<dyn ReactiveHandler<Ethereum>> {
        Arc::new(RedstoneReactiveHandler::new(registrations))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct RedstoneValueUpdateKey {
    adapter: Address,
    data_feed_id: B256,
}

/// Reactive-runtime bridge for RedStone push oracle feeds.
#[derive(Clone, Debug)]
pub struct RedstoneReactiveHandler {
    registrations_by_value_update: BTreeMap<RedstoneValueUpdateKey, Vec<FeedRegistration>>,
    registrations_by_answer_updated: BTreeMap<Address, Vec<FeedRegistration>>,
}

impl RedstoneReactiveHandler {
    /// Create a handler from current RedStone feed registrations.
    pub fn new(registrations: Vec<FeedRegistration>) -> Self {
        let mut registrations_by_value_update: BTreeMap<
            RedstoneValueUpdateKey,
            Vec<FeedRegistration>,
        > = BTreeMap::new();
        let mut registrations_by_answer_updated: BTreeMap<Address, Vec<FeedRegistration>> =
            BTreeMap::new();
        let mut seen_value_updates = BTreeSet::new();
        let mut seen_answer_updates = BTreeSet::new();

        for registration in registrations {
            let FeedSource::RedstonePush {
                price_feed,
                adapter,
                data_feed_id,
            } = registration.source
            else {
                continue;
            };

            let value_key = RedstoneValueUpdateKey {
                adapter,
                data_feed_id,
            };
            if seen_value_updates.insert((registration.id, value_key)) {
                registrations_by_value_update
                    .entry(value_key)
                    .or_default()
                    .push(registration.clone());
            }

            if seen_answer_updates.insert((registration.id, price_feed)) {
                registrations_by_answer_updated
                    .entry(price_feed)
                    .or_default()
                    .push(registration);
            }
        }

        Self {
            registrations_by_value_update,
            registrations_by_answer_updated,
        }
    }

    /// Stable handler id.
    pub fn id(&self) -> HandlerId {
        HandlerId::new(HANDLER_ID)
    }

    /// Interests for RedStone adapter and merged-feed events.
    pub fn interests(&self) -> Vec<ReactiveInterest<Ethereum>> {
        let mut interests = Vec::new();
        let mut value_adapters = BTreeSet::new();
        for key in self.registrations_by_value_update.keys() {
            if value_adapters.insert(key.adapter) {
                interests.push(log_interest(key.adapter, REDSTONE_VALUE_UPDATE_TOPIC));
            }
        }
        interests.extend(
            self.registrations_by_answer_updated
                .keys()
                .copied()
                .map(|price_feed| log_interest(price_feed, ANSWER_UPDATED_TOPIC)),
        );
        interests
    }

    fn handle_value_update(
        &self,
        ctx: &ReactiveContext,
        log: &alloy_rpc_types_eth::Log,
    ) -> Result<HandlerOutcome, HandlerError> {
        let event = decode_redstone_value_update(log).map_err(|error| {
            HandlerError::new(format!("decode RedStone ValueUpdate failed: {error}"))
        })?;
        let key = RedstoneValueUpdateKey {
            adapter: event.adapter,
            data_feed_id: event.data_feed_id,
        };
        let Some(registrations) = self.registrations_by_value_update.get(&key) else {
            return Ok(HandlerOutcome::empty(StateEffectQuality::NoStateEffect));
        };

        let answer = redstone_value_to_i256(event.value)?;
        Ok(self.outcome_for_registrations(
            ctx,
            log,
            registrations,
            event.adapter,
            answer,
            U256::from(REDSTONE_NO_ROUNDS_ROUND_ID),
            event.updated_at,
            event.block_number,
            event.log_index,
            event.removed,
            Some(event),
        ))
    }

    fn handle_answer_updated(
        &self,
        ctx: &ReactiveContext,
        log: &alloy_rpc_types_eth::Log,
    ) -> Result<HandlerOutcome, HandlerError> {
        let price_feed = log.address();
        let Some(registrations) = self.registrations_by_answer_updated.get(&price_feed) else {
            return Ok(HandlerOutcome::empty(StateEffectQuality::NoStateEffect));
        };
        let event = decode_answer_updated(log).map_err(|error| {
            HandlerError::new(format!("decode RedStone AnswerUpdated failed: {error}"))
        })?;

        Ok(self.outcome_for_registrations(
            ctx,
            log,
            registrations,
            price_feed,
            event.current,
            event.round_id,
            event.updated_at,
            event.block_number,
            event.log_index,
            event.removed,
            None,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn outcome_for_registrations(
        &self,
        ctx: &ReactiveContext,
        log: &alloy_rpc_types_eth::Log,
        registrations: &[FeedRegistration],
        event_source: Address,
        raw_answer: I256,
        event_round_id: U256,
        updated_at: u64,
        block_number: Option<u64>,
        log_index: Option<u64>,
        removed: bool,
        value_update: Option<RedstoneValueUpdate>,
    ) -> HandlerOutcome {
        let block_number = block_number.or_else(|| ctx.block.as_ref().map(|block| block.number));
        let block_hash = log
            .block_hash
            .or_else(|| ctx.block.as_ref().map(|block| block.hash));
        let log_index = log_index.or(ctx.log_index);
        let confidence = if removed {
            OracleSnapshotStatus::Unknown
        } else {
            OracleSnapshotStatus::EventPending
        };
        let mut effects = Vec::new();
        let mut tags = Vec::new();

        for registration in registrations {
            append_redstone_invalidations(&mut effects, registration, event_source);
            let hook_tags = redstone_hook_tags(registration, event_source, value_update.as_ref());
            tags.extend(hook_tags.clone());
            effects.push(ReactiveEffect::Hook(HookSignal {
                namespace: Cow::Borrowed(ORACLE_SIGNAL_NAMESPACE),
                kind: Cow::Borrowed(OracleSignalKind::PriceUpdate.as_str()),
                labels: hook_tags,
                payload: Some(Arc::new(OraclePriceUpdate {
                    id: registration.id,
                    proxy: registration.proxy,
                    aggregator: event_source,
                    label: registration.label.clone(),
                    base: registration.base.clone(),
                    quote: registration.quote.clone(),
                    raw_answer: registration
                        .source
                        .normalize_answer_from_event(Some(event_source), raw_answer),
                    decimals: registration.metadata.decimals,
                    event_round_id,
                    started_at: updated_at,
                    updated_at,
                    block_number,
                    block_hash,
                    log_index,
                    confidence,
                })),
            }));
        }

        HandlerOutcome {
            effects,
            quality: StateEffectQuality::RequiresRepair,
            tags,
        }
    }
}

impl ReactiveHandler<Ethereum> for RedstoneReactiveHandler {
    fn id(&self) -> HandlerId {
        self.id()
    }

    fn interests(&self) -> Vec<ReactiveInterest<Ethereum>> {
        self.interests()
    }

    fn handle(
        &self,
        ctx: &ReactiveContext,
        input: &ReactiveInput<Ethereum>,
        _state: &dyn StateView,
    ) -> Result<HandlerOutcome, HandlerError> {
        let ReactiveInput::Log(log) = input else {
            return Ok(HandlerOutcome::empty(StateEffectQuality::NoStateEffect));
        };
        if log.removed && !matches!(ctx.chain_status, ChainStatus::Reorged { .. }) {
            return Ok(HandlerOutcome::empty(StateEffectQuality::NoStateEffect));
        }

        match log.topics().first().copied() {
            Some(REDSTONE_VALUE_UPDATE_TOPIC) => self.handle_value_update(ctx, log),
            Some(ANSWER_UPDATED_TOPIC) => self.handle_answer_updated(ctx, log),
            _ => Ok(HandlerOutcome::empty(StateEffectQuality::NoStateEffect)),
        }
    }
}

fn log_interest(address: Address, topic: B256) -> ReactiveInterest<Ethereum> {
    ReactiveInterest::Logs(LogInterest {
        provider_filter: Filter::new().address(address).event_signature(topic),
        local_matcher: None,
        route_key: Some(RouteKeySpec::EmitterAddress),
    })
}

fn append_redstone_invalidations(
    effects: &mut Vec<ReactiveEffect>,
    registration: &FeedRegistration,
    event_source: Address,
) {
    let mut addresses = BTreeSet::from([registration.proxy, event_source]);
    if let FeedSource::RedstonePush { adapter, .. } = registration.source {
        addresses.insert(adapter);
    }
    effects.extend(addresses.into_iter().map(|address| {
        ReactiveEffect::Invalidate(InvalidationRequest {
            scope: PurgeScope::AllStorage,
            address,
            reason: InvalidationReason::HandlerRequested,
        })
    }));
}

fn redstone_hook_tags(
    registration: &FeedRegistration,
    event_source: Address,
    value_update: Option<&RedstoneValueUpdate>,
) -> Vec<ReportTag> {
    let mut tags = vec![
        ReportTag::new("feed_id", registration.id.to_string()),
        ReportTag::new("proxy", format!("{:?}", registration.proxy)),
        ReportTag::new("aggregator", format!("{event_source:?}")),
    ];
    if let Some(value_update) = value_update {
        tags.push(ReportTag::new(
            "data_feed_id",
            format!("{:?}", value_update.data_feed_id),
        ));
    }
    tags
}

fn redstone_value_to_i256(value: U256) -> Result<I256, HandlerError> {
    I256::try_from(value)
        .map_err(|_| HandlerError::new(format!("RedStone value {value} does not fit int256")))
}

fn derive_redstone_feed_id(label: Option<&str>, price_feed: Address) -> FeedId {
    if let Some(label) = label {
        let normalized = label
            .chars()
            .filter_map(|ch| {
                if ch.is_ascii_alphanumeric() {
                    Some(ch.to_ascii_lowercase())
                } else if ch.is_ascii_whitespace() || matches!(ch, '/' | '_' | '-') {
                    Some('-')
                } else {
                    None
                }
            })
            .collect::<String>()
            .trim_matches('-')
            .to_string();
        if !normalized.is_empty() {
            return FeedId::new(normalized);
        }
    }
    FeedId::new(format!("redstone-{price_feed:?}"))
}

fn provider_error(error: impl std::fmt::Debug) -> OracleError {
    OracleError::Provider(format!("{error:?}"))
}
