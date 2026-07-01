use std::{
    collections::BTreeSet,
    fmt,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use alloy_network::{Ethereum, Network};
use alloy_primitives::Address;
use evm_fork_cache::{
    cache::EvmCache,
    reactive::{
        EventSubscriber, ReactiveBatchReport, ReactiveConfig, ReactiveHandler, ReactiveInterest,
        ReactiveRuntime,
    },
};

use crate::{
    ChainlinkFeedProvider, FeedConfig, FeedId, OracleAdapterFeedSkip, OracleAdapterPlugin,
    OracleDiscoveryContext, OracleError, OracleHookEvent, OraclePrice, OraclePriceCorrected,
    OraclePriceUpdate, OracleReactiveHandler, OracleReadOverlay, OracleReconciler, OracleRegistry,
    OracleSignal, OracleStorageAdapter, OracleStorageSync, OracleTracker, RoundData,
    StalenessPolicy,
};

type OracleEventCallback = Arc<dyn Fn(&OracleHookEvent) + Send + Sync>;

/// Builder-friendly Chainlink feed registration.
#[derive(Clone, Debug)]
pub struct ChainlinkFeed {
    config: FeedConfig,
}

impl ChainlinkFeed {
    /// Create a feed registration for a Chainlink-compatible proxy.
    pub fn new(proxy: Address) -> Self {
        Self {
            config: FeedConfig {
                proxy,
                ..Default::default()
            },
        }
    }

    /// Set a stable feed id.
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.config.id = Some(FeedId::new(id));
        self
    }

    /// Set a stable feed id.
    pub fn feed_id(mut self, id: FeedId) -> Self {
        self.config.id = Some(id);
        self
    }

    /// Set a human-readable label.
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.config.label = Some(label.into());
        self
    }

    /// Set the base symbol.
    pub fn base(mut self, base: impl Into<String>) -> Self {
        self.config.base = Some(base.into());
        self
    }

    /// Set the quote symbol.
    pub fn quote(mut self, quote: impl Into<String>) -> Self {
        self.config.quote = Some(quote.into());
        self
    }

    /// Set a max-age staleness policy.
    pub fn max_age_secs(mut self, max_age_secs: u64) -> Self {
        self.config.staleness = StalenessPolicy::max_age(max_age_secs);
        self
    }

    /// Set the full staleness policy.
    pub fn staleness(mut self, staleness: StalenessPolicy) -> Self {
        self.config.staleness = staleness;
        self
    }

    /// Allow zero or negative answers for signed generic feeds.
    pub fn allow_zero_or_negative_answer(mut self, allow: bool) -> Self {
        self.config.staleness = self.config.staleness.allow_zero_or_negative_answer(allow);
        self
    }

    /// Convert into the lower-level registration config.
    pub fn into_config(self) -> FeedConfig {
        self.config
    }
}

impl From<ChainlinkFeed> for FeedConfig {
    fn from(feed: ChainlinkFeed) -> Self {
        feed.into_config()
    }
}

/// Builder for a high-level oracle runtime facade.
pub struct OracleRuntimeBuilder<P> {
    provider: P,
    feeds: Vec<ChainlinkFeed>,
    now_timestamp: Option<u64>,
    storage_sync: OracleStorageSync,
    callbacks: Vec<OracleEventCallback>,
}

/// Cache-native runtime builder with first-class oracle adapter plugins.
pub struct OracleCacheRuntimeBuilder {
    adapters: Vec<Arc<dyn OracleAdapterPlugin>>,
    now_timestamp: Option<u64>,
    storage_sync: OracleStorageSync,
    callbacks: Vec<OracleEventCallback>,
}

/// Best-effort cache-native oracle runtime build report.
#[non_exhaustive]
#[derive(Debug)]
pub struct OracleCacheRuntimeBuildReport {
    /// Runtime seeded with every successfully discovered feed.
    pub runtime: OracleRuntime<()>,
    /// Feeds that were skipped because an adapter could not register them.
    pub skipped: Vec<OracleAdapterFeedSkip>,
}

impl<P> OracleRuntimeBuilder<P> {
    /// Create a runtime builder around a provider.
    pub fn new(provider: P) -> Self {
        Self {
            provider,
            feeds: Vec::new(),
            now_timestamp: None,
            storage_sync: OracleStorageSync::chainlink_defaults(),
            callbacks: Vec::new(),
        }
    }

    /// Add one feed registration.
    pub fn feed(mut self, feed: ChainlinkFeed) -> Self {
        self.feeds.push(feed);
        self
    }

    /// Set a fixed timestamp for deterministic registration status classification.
    pub fn now_timestamp(mut self, now_timestamp: u64) -> Self {
        self.now_timestamp = Some(now_timestamp);
        self
    }

    /// Add one storage adapter.
    pub fn storage_adapter<A>(mut self, adapter: A) -> Self
    where
        A: OracleStorageAdapter + 'static,
    {
        self.storage_sync.push_adapter(Arc::new(adapter));
        self
    }

    /// Replace the storage-sync registry.
    pub fn storage_sync(mut self, storage_sync: OracleStorageSync) -> Self {
        self.storage_sync = storage_sync;
        self
    }

    /// Subscribe to every typed oracle event emitted by this runtime.
    pub fn on_event<F>(mut self, callback: F) -> Self
    where
        F: Fn(&OracleHookEvent) + Send + Sync + 'static,
    {
        self.callbacks.push(Arc::new(callback));
        self
    }

    /// Subscribe to immediate event-derived price updates.
    pub fn on_price_update<F>(self, callback: F) -> Self
    where
        F: Fn(&OraclePriceUpdate) + Send + Sync + 'static,
    {
        self.on_event(move |event| {
            if let OracleHookEvent::PriceUpdate(update) = event {
                callback(update);
            }
        })
    }

    /// Subscribe to proxy corrections.
    pub fn on_price_corrected<F>(self, callback: F) -> Self
    where
        F: Fn(&OraclePriceCorrected) + Send + Sync + 'static,
    {
        self.on_event(move |event| {
            if let OracleHookEvent::PriceCorrected(corrected) = event {
                callback(corrected);
            }
        })
    }
}

impl<P: ChainlinkFeedProvider> OracleRuntimeBuilder<P> {
    /// Register feeds and construct the runtime.
    pub async fn build(self) -> Result<OracleRuntime<P>, OracleError> {
        let now_timestamp = match self.now_timestamp {
            Some(now_timestamp) => now_timestamp,
            None => SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|error| OracleError::Provider(error.to_string()))?
                .as_secs(),
        };
        let mut registry = OracleRegistry::new_at_timestamp(now_timestamp);
        for feed in self.feeds {
            registry
                .register_chainlink_feed(&self.provider, feed.into_config())
                .await?;
        }

        Ok(OracleRuntime {
            provider: self.provider,
            tracker: OracleTracker::new(registry),
            storage_sync: self.storage_sync,
            callbacks: self.callbacks,
            adapter_handlers: Vec::new(),
        })
    }
}

impl Default for OracleCacheRuntimeBuilder {
    fn default() -> Self {
        Self {
            adapters: Vec::new(),
            now_timestamp: None,
            storage_sync: OracleStorageSync::chainlink_defaults(),
            callbacks: Vec::new(),
        }
    }
}

impl OracleCacheRuntimeBuilder {
    /// Install one oracle adapter plugin.
    pub fn install_adapter<A>(mut self, adapter: A) -> Self
    where
        A: OracleAdapterPlugin + 'static,
    {
        self.adapters.push(Arc::new(adapter));
        self
    }

    /// Set a fixed timestamp for deterministic registration status classification.
    pub fn now_timestamp(mut self, now_timestamp: u64) -> Self {
        self.now_timestamp = Some(now_timestamp);
        self
    }

    /// Add one storage adapter.
    pub fn storage_adapter<A>(mut self, adapter: A) -> Self
    where
        A: OracleStorageAdapter + 'static,
    {
        self.storage_sync.push_adapter(Arc::new(adapter));
        self
    }

    /// Replace the storage-sync registry.
    pub fn storage_sync(mut self, storage_sync: OracleStorageSync) -> Self {
        self.storage_sync = storage_sync;
        self
    }

    /// Subscribe to every typed oracle event emitted by this runtime.
    pub fn on_event<F>(mut self, callback: F) -> Self
    where
        F: Fn(&OracleHookEvent) + Send + Sync + 'static,
    {
        self.callbacks.push(Arc::new(callback));
        self
    }

    /// Subscribe to immediate event-derived price updates.
    pub fn on_price_update<F>(self, callback: F) -> Self
    where
        F: Fn(&OraclePriceUpdate) + Send + Sync + 'static,
    {
        self.on_event(move |event| {
            if let OracleHookEvent::PriceUpdate(update) = event {
                callback(update);
            }
        })
    }

    /// Register adapter-discovered feeds and construct the runtime.
    pub async fn build(self, cache: &mut EvmCache) -> Result<OracleRuntime<()>, OracleError> {
        let report = self.build_report(cache).await?;
        if let Some(skipped) = report.skipped.first() {
            return Err(cache_runtime_skip_error(skipped));
        }
        Ok(report.runtime)
    }

    /// Register every compatible adapter feed and return skipped feeds instead of failing fast.
    pub async fn build_report(
        self,
        cache: &mut EvmCache,
    ) -> Result<OracleCacheRuntimeBuildReport, OracleError> {
        let now_timestamp = match self.now_timestamp {
            Some(now_timestamp) => now_timestamp,
            None => SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|error| OracleError::Provider(error.to_string()))?
                .as_secs(),
        };

        let mut discovered_by_adapter = Vec::new();
        let mut seeded = Vec::new();
        let mut skipped = Vec::new();
        for adapter in &self.adapters {
            let report = adapter
                .discover(OracleDiscoveryContext {
                    cache,
                    now_timestamp,
                })
                .await?;
            skipped.extend(report.skipped);
            let registrations = report
                .feeds
                .iter()
                .map(|feed| feed.registration.clone())
                .collect::<Vec<_>>();
            seeded.extend(
                report
                    .feeds
                    .into_iter()
                    .map(|feed| (feed.registration, feed.round)),
            );
            discovered_by_adapter.push((Arc::clone(adapter), registrations));
        }

        let tracker = OracleTracker::from_registrations_at_timestamp(seeded, now_timestamp)?;
        let adapter_handlers = discovered_by_adapter
            .into_iter()
            .filter(|(_, registrations)| !registrations.is_empty())
            .map(|(adapter, registrations)| {
                adapter.reactive_handler(registrations, self.storage_sync.clone())
            })
            .collect();

        Ok(OracleCacheRuntimeBuildReport {
            runtime: OracleRuntime {
                provider: (),
                tracker,
                storage_sync: self.storage_sync,
                callbacks: self.callbacks,
                adapter_handlers,
            },
            skipped,
        })
    }
}

/// High-level facade for registration, typed events, reads, and reconciliation.
pub struct OracleRuntime<P> {
    provider: P,
    tracker: OracleTracker,
    storage_sync: OracleStorageSync,
    callbacks: Vec<OracleEventCallback>,
    adapter_handlers: Vec<Arc<dyn ReactiveHandler<Ethereum>>>,
}

impl<P: fmt::Debug> fmt::Debug for OracleRuntime<P> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OracleRuntime")
            .field("provider", &self.provider)
            .field("tracker", &self.tracker)
            .field("storage_sync", &self.storage_sync)
            .field("callbacks_len", &self.callbacks.len())
            .field("adapter_handlers_len", &self.adapter_handlers.len())
            .finish()
    }
}

impl<P> OracleRuntime<P> {
    /// Start building a runtime around a provider.
    pub fn builder(provider: P) -> OracleRuntimeBuilder<P> {
        OracleRuntimeBuilder::new(provider)
    }

    /// Borrow the underlying provider.
    pub fn provider(&self) -> &P {
        &self.provider
    }

    /// Borrow the tracker.
    pub fn tracker(&self) -> &OracleTracker {
        &self.tracker
    }

    /// Mutably borrow the tracker.
    pub fn tracker_mut(&mut self) -> &mut OracleTracker {
        &mut self.tracker
    }

    /// Build a fresh built-in Chainlink-compatible reactive handler from current registrations.
    ///
    /// Adapter plugin handlers are not included. Use [`Self::reactive_handlers`],
    /// [`Self::reactive_runtime`], or [`Self::register_subscriber`] when the runtime
    /// was built with installed adapter plugins.
    pub fn reactive_handler(&self) -> OracleReactiveHandler {
        OracleReactiveHandler::with_storage_sync(
            self.tracker.registrations().collect(),
            self.storage_sync.clone(),
        )
    }

    /// Build fresh reactive handlers for every source installed in this runtime.
    ///
    /// The first handler is the built-in Chainlink-compatible handler. Any adapter
    /// plugin handlers discovered during cache-native startup follow it.
    pub fn reactive_handlers(&self) -> Vec<Arc<dyn ReactiveHandler<Ethereum>>> {
        let built_in: Arc<dyn ReactiveHandler<Ethereum>> = Arc::new(self.reactive_handler());
        let mut seen = BTreeSet::from([built_in.id()]);
        let mut handlers = vec![built_in];
        handlers.extend(
            self.adapter_handlers
                .iter()
                .filter(|handler| seen.insert(handler.id()))
                .cloned(),
        );
        handlers
    }

    /// Build a reactive runtime with this oracle runtime's handlers already installed.
    pub fn reactive_runtime(&self) -> Result<ReactiveRuntime<Ethereum>, OracleError> {
        let mut runtime = ReactiveRuntime::<Ethereum>::new(ReactiveConfig::default());
        for handler in self.reactive_handlers() {
            runtime.register_handler(handler).map_err(runtime_error)?;
        }
        Ok(runtime)
    }

    /// Register this runtime's oracle interests with an event subscriber.
    pub fn register_subscriber<S>(&self, subscriber: &mut S) -> Result<(), OracleError>
    where
        S: EventSubscriber<Ethereum>,
    {
        subscriber
            .register_interests(&self.reactive_interests())
            .map_err(runtime_error)
    }

    /// Read one subscriber batch, apply it to the cache, and return typed oracle events.
    pub async fn next_subscriber_events<S>(
        &mut self,
        cache: &mut EvmCache,
        runtime: &mut ReactiveRuntime<Ethereum>,
        subscriber: &mut S,
    ) -> Result<Option<Vec<OracleHookEvent>>, OracleError>
    where
        S: EventSubscriber<Ethereum>,
    {
        let Some(batch) = subscriber.next_batch().await.map_err(runtime_error)? else {
            return Ok(None);
        };
        let report = runtime.ingest_batch(cache, batch).map_err(runtime_error)?;
        self.apply_batch_report(&report).map(Some)
    }

    /// Build a read overlay over the current tracker state.
    pub fn read_overlay(&self) -> OracleReadOverlay<'_> {
        OracleReadOverlay::new(&self.tracker)
    }

    /// Return the latest typed price by feed id string.
    pub fn price(&self, id: impl AsRef<str>) -> Result<OraclePrice, OracleError> {
        self.tracker.price(id)
    }

    /// Return the latest typed price by proxy address.
    pub fn price_by_proxy(&self, proxy: Address) -> Result<OraclePrice, OracleError> {
        self.tracker.price_by_proxy(proxy)
    }

    /// Return the latest round by feed id string.
    pub fn latest_round(&self, id: impl AsRef<str>) -> Result<RoundData, OracleError> {
        self.tracker.latest_round(id)
    }

    /// Apply a committed reactive batch and return typed oracle events.
    pub fn apply_batch_report<N: Network>(
        &mut self,
        report: &ReactiveBatchReport<N>,
    ) -> Result<Vec<OracleHookEvent>, OracleError> {
        let mut events = Vec::new();
        for signal in report
            .applied
            .iter()
            .flat_map(|applied| applied.hook_signals.iter())
        {
            if let Some(signal) = OracleSignal::from_hook(signal)? {
                events.push(signal.to_event());
            }
        }

        self.tracker.apply_batch_report(report)?;
        self.emit(&events);
        Ok(events)
    }

    /// Reconcile all currently pending event-derived updates.
    pub async fn reconcile_pending(&mut self) -> Result<Vec<OracleHookEvent>, OracleError>
    where
        P: ChainlinkFeedProvider,
    {
        let mut reconciler = OracleReconciler::default();
        for request in self.tracker.pending_reconciliations().iter().cloned() {
            reconciler.enqueue(request);
        }

        let mut events = Vec::new();
        while let Some(result) = reconciler
            .reconcile_next(&mut self.tracker, &self.provider)
            .await?
        {
            events.extend(result.hooks);
        }
        self.emit(&events);
        Ok(events)
    }

    fn emit(&self, events: &[OracleHookEvent]) {
        for event in events {
            for callback in &self.callbacks {
                callback(event);
            }
        }
    }

    /// Return the complete log/event interest set for built-in and adapter handlers.
    pub fn reactive_interests(&self) -> Vec<ReactiveInterest<Ethereum>> {
        let mut interests = Vec::new();
        for handler in self.reactive_handlers() {
            interests.extend(handler.interests());
        }
        interests
    }
}

impl OracleRuntime<()> {
    /// Start building a cache-native runtime from oracle adapter plugins.
    pub fn cache_builder() -> OracleCacheRuntimeBuilder {
        OracleCacheRuntimeBuilder::default()
    }

    /// Build a runtime facade from a cache-native tracker.
    pub fn from_tracker(tracker: OracleTracker) -> Self {
        Self {
            provider: (),
            tracker,
            storage_sync: OracleStorageSync::chainlink_defaults(),
            callbacks: Vec::new(),
            adapter_handlers: Vec::new(),
        }
    }
}

fn runtime_error(error: impl ToString) -> OracleError {
    OracleError::Provider(error.to_string())
}

fn cache_runtime_skip_error(skipped: &OracleAdapterFeedSkip) -> OracleError {
    let label = skipped
        .feed
        .label()
        .map(str::to_string)
        .or_else(|| skipped.feed.id().map(|id| id.to_string()))
        .unwrap_or_else(|| "unknown".to_string());
    OracleError::Provider(format!(
        "adapter feed {} at {:?} was skipped: {}",
        label, skipped.proxy, skipped.reason
    ))
}
