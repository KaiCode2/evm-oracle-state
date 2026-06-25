use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use alloy_network::Network;
use alloy_primitives::Address;
use evm_fork_cache::reactive::ReactiveBatchReport;

use crate::{
    ChainlinkFeedProvider, FeedConfig, FeedId, OracleError, OracleHookEvent, OraclePrice,
    OraclePriceCorrected, OraclePriceUpdate, OracleReactiveHandler, OracleReadOverlay,
    OracleReconciler, OracleRegistry, OracleSignal, OracleStorageAdapter, OracleStorageSync,
    OracleTracker, RoundData, StalenessPolicy,
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

impl<P> OracleRuntimeBuilder<P> {
    /// Create a runtime builder around a provider.
    pub fn new(provider: P) -> Self {
        Self {
            provider,
            feeds: Vec::new(),
            now_timestamp: None,
            storage_sync: OracleStorageSync::default(),
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

    /// Add one verified storage adapter.
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
        })
    }
}

/// High-level facade for registration, typed events, reads, and reconciliation.
pub struct OracleRuntime<P> {
    provider: P,
    tracker: OracleTracker,
    storage_sync: OracleStorageSync,
    callbacks: Vec<OracleEventCallback>,
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

    /// Build a fresh reactive handler from current registrations.
    pub fn reactive_handler(&self) -> OracleReactiveHandler {
        OracleReactiveHandler::with_storage_sync(
            self.tracker.registrations().collect(),
            self.storage_sync.clone(),
        )
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
}
