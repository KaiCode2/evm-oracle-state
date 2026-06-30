use alloy_network::{Ethereum, Network};
use alloy_primitives::{Address, B256, I256, U256};
use evm_fork_cache::reactive::{ReactiveBatchReport, ReactiveReport};

use crate::{
    AggregatorChange, AggregatorLayoutEvidence, ChainlinkFeedProvider, FeedId, FeedRegistration,
    ORACLE_LEGACY_ANSWER_UPDATED_KIND, ORACLE_SIGNAL_NAMESPACE, OracleBlockRef, OracleError,
    OraclePrice, OracleRegistry, OracleSignalKind, OracleSnapshot, OracleSnapshotStatus, RoundData,
    SnapshotSource,
    registry::snapshot_from_proxy_read,
    state::{EventSnapshotInput, FeedStatus},
};

/// Event-derived oracle update carried in reactive hook payloads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OracleUpdate {
    /// Feed id.
    pub id: FeedId,
    /// Proxy address.
    pub proxy: Address,
    /// Emitting aggregator.
    pub aggregator: Address,
    /// Event-derived round.
    pub round: RoundData,
    /// Event block number, when known.
    pub block_number: Option<u64>,
    /// Event log index, when known.
    pub log_index: Option<u64>,
}

/// Rich event-derived oracle price update emitted by the reactive handler.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OraclePriceUpdate {
    /// Feed id.
    pub id: FeedId,
    /// Proxy address.
    pub proxy: Address,
    /// Emitting aggregator.
    pub aggregator: Address,
    /// Optional human-readable feed label.
    pub label: Option<String>,
    /// Optional base symbol.
    pub base: Option<String>,
    /// Optional quote symbol.
    pub quote: Option<String>,
    /// Raw signed event answer.
    pub raw_answer: I256,
    /// Feed decimals.
    pub decimals: u8,
    /// Event round id.
    pub event_round_id: U256,
    /// Event-derived `startedAt` timestamp.
    pub started_at: u64,
    /// Event `updatedAt` timestamp.
    pub updated_at: u64,
    /// Event block number, when known.
    pub block_number: Option<u64>,
    /// Event block hash, when known.
    pub block_hash: Option<B256>,
    /// Event log index, when known.
    pub log_index: Option<u64>,
    /// Snapshot confidence produced by the event.
    pub confidence: OracleSnapshotStatus,
}

impl OraclePriceUpdate {
    fn round(&self) -> RoundData {
        RoundData {
            round_id: self.event_round_id,
            answer: self.raw_answer,
            started_at: self.started_at,
            updated_at: self.updated_at,
            answered_in_round: self.event_round_id,
        }
    }
}

/// Event-specific proxy reconciliation request queued after an event update.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OracleReconciliationRequest {
    /// Feed id.
    pub id: FeedId,
    /// Proxy address to read authoritatively.
    pub proxy: Address,
    /// Aggregator that emitted the event.
    pub aggregator: Option<Address>,
    /// Event-derived round that needs confirmation.
    pub event_round: RoundData,
    /// Event block number, when known.
    pub block_number: Option<u64>,
    /// Event block hash, when known.
    pub block_hash: Option<B256>,
    /// Event log index, when known.
    pub log_index: Option<u64>,
}

impl OracleReconciliationRequest {
    /// Block identity for providers that support hash-pinned reads.
    pub fn block_ref(&self) -> Option<OracleBlockRef> {
        Some(OracleBlockRef {
            number: self.block_number?,
            hash: self.block_hash?,
        })
    }
}

/// Hook emitted when an event price is confirmed by the proxy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OraclePriceConfirmed {
    /// Feed id.
    pub id: FeedId,
    /// Proxy address.
    pub proxy: Address,
    /// Best-known current aggregator.
    pub aggregator: Option<Address>,
    /// Optional human-readable feed label.
    pub label: Option<String>,
    /// Optional base symbol.
    pub base: Option<String>,
    /// Optional quote symbol.
    pub quote: Option<String>,
    /// Confirmed raw answer.
    pub raw_answer: I256,
    /// Feed decimals.
    pub decimals: u8,
    /// Event round id.
    pub event_round_id: U256,
    /// Event `updatedAt` timestamp.
    pub updated_at: u64,
    /// Event block number, when known.
    pub block_number: Option<u64>,
    /// Event block hash, when known.
    pub block_hash: Option<B256>,
    /// Event log index, when known.
    pub log_index: Option<u64>,
    /// Resulting confidence.
    pub confidence: OracleSnapshotStatus,
    /// Authoritative proxy round.
    pub proxy_round: RoundData,
}

/// Hook emitted when a proxy read corrects an event price.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OraclePriceCorrected {
    /// Feed id.
    pub id: FeedId,
    /// Proxy address.
    pub proxy: Address,
    /// Best-known current aggregator.
    pub aggregator: Option<Address>,
    /// Optional human-readable feed label.
    pub label: Option<String>,
    /// Optional base symbol.
    pub base: Option<String>,
    /// Optional quote symbol.
    pub quote: Option<String>,
    /// Raw event answer that was corrected.
    pub event_answer: I256,
    /// Corrected raw answer from the proxy.
    pub raw_answer: I256,
    /// Feed decimals.
    pub decimals: u8,
    /// Event round id.
    pub event_round_id: U256,
    /// Event `updatedAt` timestamp.
    pub updated_at: u64,
    /// Event block number, when known.
    pub block_number: Option<u64>,
    /// Event block hash, when known.
    pub block_hash: Option<B256>,
    /// Event log index, when known.
    pub log_index: Option<u64>,
    /// Resulting confidence.
    pub confidence: OracleSnapshotStatus,
    /// Corrected authoritative proxy round.
    pub corrected_round: RoundData,
}

/// Hook emitted when the proxy read is stale under policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OraclePriceStale {
    /// Feed id.
    pub id: FeedId,
    /// Proxy address.
    pub proxy: Address,
    /// Best-known current aggregator.
    pub aggregator: Option<Address>,
    /// Optional human-readable feed label.
    pub label: Option<String>,
    /// Optional base symbol.
    pub base: Option<String>,
    /// Optional quote symbol.
    pub quote: Option<String>,
    /// Raw stale answer.
    pub raw_answer: I256,
    /// Feed decimals.
    pub decimals: u8,
    /// Event round id.
    pub event_round_id: U256,
    /// Event `updatedAt` timestamp.
    pub updated_at: u64,
    /// Event block number, when known.
    pub block_number: Option<u64>,
    /// Event block hash, when known.
    pub block_hash: Option<B256>,
    /// Event log index, when known.
    pub log_index: Option<u64>,
    /// Resulting confidence.
    pub confidence: OracleSnapshotStatus,
    /// Authoritative proxy round.
    pub proxy_round: RoundData,
}

/// Rich oracle hook events emitted by reconciliation.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OracleHookEvent {
    /// Immediate event-derived price update.
    PriceUpdate(OraclePriceUpdate),
    /// Event price matched the authoritative proxy read.
    PriceConfirmed(OraclePriceConfirmed),
    /// Event price was corrected by the authoritative proxy read.
    PriceCorrected(OraclePriceCorrected),
    /// Authoritative proxy read was stale under policy.
    PriceStale(OraclePriceStale),
    /// Proxy now points at a different aggregator.
    AggregatorChanged(AggregatorChange),
}

/// Hook emitted when proxy reconciliation detects an aggregator change.
pub type OracleAggregatorChanged = AggregatorChange;

/// One completed event reconciliation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OracleReconciliationResult {
    /// Request that was reconciled.
    pub request: OracleReconciliationRequest,
    /// Lifecycle hooks emitted by this reconciliation.
    pub hooks: Vec<OracleHookEvent>,
}

/// Reconciliation summary.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Number of feeds checked.
    pub checked_feeds: usize,
    /// Feeds whose snapshot changed.
    pub changed_feeds: Vec<FeedId>,
    /// Aggregator changes detected while reading proxies.
    pub aggregator_changes: Vec<AggregatorChange>,
}

/// Mutable typed oracle state.
#[derive(Clone, Debug)]
pub struct OracleTracker {
    registry: OracleRegistry,
    pending_reconciliations: Vec<OracleReconciliationRequest>,
}

impl OracleTracker {
    /// Create a tracker from an existing registry.
    pub fn new(registry: OracleRegistry) -> Self {
        Self {
            registry,
            pending_reconciliations: Vec::new(),
        }
    }

    /// Seed a tracker from registrations and round data.
    pub fn from_registrations_at_timestamp(
        feeds: Vec<(FeedRegistration, RoundData)>,
        now_timestamp: u64,
    ) -> Result<Self, OracleError> {
        let mut registry = OracleRegistry::new_at_timestamp(now_timestamp);
        for (registration, round) in feeds {
            let id = registration.id;
            if registry.registrations_map().contains_key(&id) {
                return Err(OracleError::DuplicateFeedId(id.to_string()));
            }
            if registry.id_by_proxy(registration.proxy).is_some() {
                return Err(OracleError::DuplicateProxy(registration.proxy));
            }

            let snapshot = OracleSnapshot::proxy_read(
                id,
                registration.proxy,
                registration.current_aggregator,
                registration.metadata.clone(),
                round,
                now_timestamp,
                &registration.staleness,
            );
            registry
                .registrations_map_mut()
                .insert(id, registration.clone());
            registry
                .snapshots_by_proxy_mut()
                .insert(registration.proxy, snapshot);
            registry.record_proxy_id(registration.proxy, id);
        }
        Ok(Self {
            registry,
            pending_reconciliations: Vec::new(),
        })
    }

    /// Return cloned registrations for rebuilding reactive routing.
    pub fn registrations(&self) -> impl Iterator<Item = FeedRegistration> + '_ {
        self.registry.registrations()
    }

    /// Return the latest snapshot by proxy.
    pub fn latest(&self, proxy: Address) -> Option<&OracleSnapshot> {
        self.registry.latest(proxy)
    }

    /// Return the latest actionable-facing price by feed id string.
    pub fn price(&self, id: impl AsRef<str>) -> Result<OraclePrice, OracleError> {
        let registration = self
            .registration_by_id_str(id.as_ref())
            .ok_or(OracleError::FeedNotFound)?;
        self.price_for_registration(registration)
    }

    /// Return the latest actionable-facing price by feed id.
    pub fn price_by_id(&self, id: FeedId) -> Result<OraclePrice, OracleError> {
        let registration = self
            .registry
            .registrations_map()
            .get(&id)
            .ok_or(OracleError::FeedNotFound)?;
        self.price_for_registration(registration)
    }

    /// Return the latest actionable-facing price by proxy address.
    pub fn price_by_proxy(&self, proxy: Address) -> Result<OraclePrice, OracleError> {
        let id = self
            .registry
            .id_by_proxy(proxy)
            .copied()
            .ok_or(OracleError::FeedNotFound)?;
        self.price_by_id(id)
    }

    /// Return the latest round by feed id string.
    pub fn latest_round(&self, id: impl AsRef<str>) -> Result<RoundData, OracleError> {
        Ok(self.price(id)?.round_data())
    }

    /// Return the latest round by proxy address.
    pub fn latest_round_by_proxy(&self, proxy: Address) -> Result<RoundData, OracleError> {
        Ok(self.price_by_proxy(proxy)?.round_data())
    }

    /// Return event-specific proxy reconciliations waiting to be processed.
    pub fn pending_reconciliations(&self) -> &[OracleReconciliationRequest] {
        &self.pending_reconciliations
    }

    /// Apply committed reactive reports to typed oracle state.
    pub fn apply_batch_report<N: Network>(
        &mut self,
        report: &ReactiveBatchReport<N>,
    ) -> Result<(), OracleError> {
        let has_rich_price_updates = report.applied.iter().any(|applied| {
            applied.hook_signals.iter().any(|signal| {
                signal.namespace.as_ref() == ORACLE_SIGNAL_NAMESPACE
                    && signal.kind.as_ref() == OracleSignalKind::PriceUpdate.as_str()
                    && signal.payload.is_some()
            })
        });

        for applied in &report.applied {
            for signal in &applied.hook_signals {
                if signal.namespace.as_ref() != ORACLE_SIGNAL_NAMESPACE {
                    continue;
                }

                let Some(payload) = signal.payload.as_ref() else {
                    continue;
                };

                match signal.kind.as_ref() {
                    kind if kind == OracleSignalKind::PriceUpdate.as_str() => {
                        if let Some(update) = payload.downcast_ref::<OraclePriceUpdate>() {
                            self.apply_price_update(update.clone())?;
                        }
                    }
                    kind if kind == ORACLE_LEGACY_ANSWER_UPDATED_KIND
                        && !has_rich_price_updates =>
                    {
                        if let Some(update) = payload.downcast_ref::<OracleUpdate>() {
                            self.apply_update(update.clone())?;
                        }
                    }
                    _ => {}
                }
            }
        }

        if report.reports.iter().any(|report| {
            matches!(
                report.as_ref(),
                ReactiveReport::Reorg(reorg) if !reorg.dropped_blocks.is_empty()
            )
        }) {
            self.mark_event_snapshots_unknown();
        }

        Ok(())
    }

    /// Reconcile all registered feeds with authoritative proxy reads.
    pub async fn reconcile<P: ChainlinkFeedProvider>(
        &mut self,
        provider: &P,
    ) -> Result<ReconcileReport, OracleError> {
        let registrations: Vec<_> = self.registry.registrations().collect();
        let mut report = ReconcileReport::default();

        for mut registration in registrations {
            report.checked_feeds += 1;
            let read_proxy = registration.source.read_proxy(registration.proxy);
            let round = registration
                .source
                .normalize_round(provider.latest_round_data(read_proxy).await?);
            let new_aggregator = provider
                .aggregator(read_proxy)
                .await
                .unwrap_or(registration.current_aggregator);
            let new_layout = self
                .registry
                .detect_aggregator_layout(provider, new_aggregator, None)
                .await;
            let old_aggregator = registration.current_aggregator;
            let snapshot = snapshot_from_proxy_read(
                &registration,
                round,
                new_aggregator,
                self.registry.now_timestamp(),
            );

            let changed = self
                .registry
                .latest(registration.proxy)
                .is_none_or(|old| old != &snapshot);
            if changed {
                report.changed_feeds.push(registration.id);
            }

            if old_aggregator != new_aggregator {
                report.aggregator_changes.push(AggregatorChange {
                    id: registration.id,
                    proxy: registration.proxy,
                    old: old_aggregator,
                    new: new_aggregator,
                });
            }

            registration.current_aggregator = new_aggregator;
            registration.aggregator_layout = new_layout.clone();
            if let Some(stored) = self
                .registry
                .registrations_map_mut()
                .get_mut(&registration.id)
            {
                stored.current_aggregator = new_aggregator;
                stored.aggregator_layout = new_layout;
            }
            self.registry.replace_snapshot(snapshot);
            self.clear_pending_for_proxy(registration.proxy);
        }

        Ok(report)
    }

    fn registration_by_id_str(&self, id: &str) -> Option<&FeedRegistration> {
        self.registry
            .registrations_map()
            .values()
            .find(|registration| registration.id.as_str() == id)
    }

    fn price_for_registration(
        &self,
        registration: &FeedRegistration,
    ) -> Result<OraclePrice, OracleError> {
        let snapshot = self
            .registry
            .latest(registration.proxy)
            .ok_or(OracleError::FeedNotFound)?;
        Ok(OraclePrice::from_snapshot(snapshot, registration))
    }

    fn apply_update(&mut self, update: OracleUpdate) -> Result<(), OracleError> {
        let Some(registration) = self.registry.registrations_map().get(&update.id).cloned() else {
            return Ok(());
        };

        if registration.current_aggregator != Some(update.aggregator) {
            return Ok(());
        }

        let snapshot = OracleSnapshot::event(EventSnapshotInput {
            id: update.id,
            proxy: update.proxy,
            aggregator: Some(update.aggregator),
            metadata: registration.metadata,
            round: update.round,
            now_timestamp: self.registry.now_timestamp(),
            staleness: &registration.staleness,
            block_number: update.block_number,
            block_hash: None,
        });
        self.queue_reconciliation(OracleReconciliationRequest {
            id: update.id,
            proxy: update.proxy,
            aggregator: Some(update.aggregator),
            event_round: snapshot.round.clone(),
            block_number: update.block_number,
            block_hash: None,
            log_index: update.log_index,
        });
        self.registry.replace_snapshot(snapshot);
        Ok(())
    }

    fn apply_price_update(&mut self, update: OraclePriceUpdate) -> Result<(), OracleError> {
        let Some(registration) = self.registry.registrations_map().get(&update.id).cloned() else {
            return Ok(());
        };

        if registration.current_aggregator != Some(update.aggregator) {
            return Ok(());
        }

        let event_round = update.round();
        if update.confidence == OracleSnapshotStatus::Unknown {
            self.mark_matching_event_snapshot_unknown(
                update.proxy,
                &event_round,
                update.block_number,
                update.block_hash,
            );
            return Ok(());
        }

        let snapshot = OracleSnapshot::event(EventSnapshotInput {
            id: update.id,
            proxy: update.proxy,
            aggregator: Some(update.aggregator),
            metadata: registration.metadata,
            round: event_round.clone(),
            now_timestamp: self.registry.now_timestamp(),
            staleness: &registration.staleness,
            block_number: update.block_number,
            block_hash: update.block_hash,
        });
        self.queue_reconciliation(OracleReconciliationRequest {
            id: update.id,
            proxy: update.proxy,
            aggregator: Some(update.aggregator),
            event_round,
            block_number: update.block_number,
            block_hash: update.block_hash,
            log_index: update.log_index,
        });
        self.registry.replace_snapshot(snapshot);
        Ok(())
    }

    fn mark_matching_event_snapshot_unknown(
        &mut self,
        proxy: Address,
        event_round: &RoundData,
        block_number: Option<u64>,
        block_hash: Option<B256>,
    ) {
        let Some(snapshot) = self.registry.snapshots_by_proxy_mut().get_mut(&proxy) else {
            return;
        };
        if snapshot.source != SnapshotSource::Event {
            return;
        }
        if !proxy_round_matches_event(&snapshot.round, event_round) {
            return;
        }
        if block_number.is_some() && snapshot.block_number != block_number {
            return;
        }
        if block_hash.is_some() && snapshot.block_hash != block_hash {
            return;
        }

        snapshot.mark_unknown();
        self.clear_pending_for_proxy(proxy);
    }

    fn mark_event_snapshots_unknown(&mut self) {
        let mut unknown_proxies = Vec::new();
        for snapshot in self.registry.snapshots_by_proxy_mut().values_mut() {
            if snapshot.source == SnapshotSource::Event {
                unknown_proxies.push(snapshot.proxy);
                snapshot.mark_unknown();
            }
        }
        for proxy in unknown_proxies {
            self.clear_pending_for_proxy(proxy);
        }
    }

    fn queue_reconciliation(&mut self, request: OracleReconciliationRequest) {
        self.clear_pending_for_proxy(request.proxy);
        self.pending_reconciliations.push(request);
    }

    fn clear_pending_for_proxy(&mut self, proxy: Address) {
        self.pending_reconciliations
            .retain(|request| request.proxy != proxy);
    }

    fn complete_pending_reconciliation(&mut self, request: &OracleReconciliationRequest) {
        self.pending_reconciliations
            .retain(|pending| pending != request);
    }

    fn apply_reconciled_request(
        &mut self,
        request: &OracleReconciliationRequest,
        proxy_round: RoundData,
        new_aggregator: Option<Address>,
        new_layout: Option<AggregatorLayoutEvidence>,
    ) -> Result<Vec<OracleHookEvent>, OracleError> {
        let Some(registration) = self.registry.registrations_map().get(&request.id).cloned() else {
            return Err(OracleError::FeedNotFound);
        };

        let proxy_round = registration.source.normalize_round(proxy_round);
        let old_aggregator = registration.current_aggregator;
        let mut snapshot = snapshot_from_proxy_read(
            &registration,
            proxy_round.clone(),
            new_aggregator,
            self.registry.now_timestamp(),
        );
        let confidence = reconciliation_confidence(&snapshot.status, request, &proxy_round);
        snapshot.set_confidence(confidence);

        let mut hooks = Vec::new();
        if old_aggregator != new_aggregator {
            hooks.push(OracleHookEvent::AggregatorChanged(AggregatorChange {
                id: registration.id,
                proxy: registration.proxy,
                old: old_aggregator,
                new: new_aggregator,
            }));
        }

        match confidence {
            OracleSnapshotStatus::Confirmed => {
                hooks.push(OracleHookEvent::PriceConfirmed(OraclePriceConfirmed {
                    id: registration.id,
                    proxy: registration.proxy,
                    aggregator: new_aggregator,
                    label: registration.label.clone(),
                    base: registration.base.clone(),
                    quote: registration.quote.clone(),
                    raw_answer: proxy_round.answer,
                    decimals: registration.metadata.decimals,
                    event_round_id: request.event_round.round_id,
                    updated_at: request.event_round.updated_at,
                    block_number: request.block_number,
                    block_hash: request.block_hash,
                    log_index: request.log_index,
                    confidence,
                    proxy_round: proxy_round.clone(),
                }));
            }
            OracleSnapshotStatus::Corrected => {
                hooks.push(OracleHookEvent::PriceCorrected(OraclePriceCorrected {
                    id: registration.id,
                    proxy: registration.proxy,
                    aggregator: new_aggregator,
                    label: registration.label.clone(),
                    base: registration.base.clone(),
                    quote: registration.quote.clone(),
                    event_answer: request.event_round.answer,
                    raw_answer: proxy_round.answer,
                    decimals: registration.metadata.decimals,
                    event_round_id: request.event_round.round_id,
                    updated_at: request.event_round.updated_at,
                    block_number: request.block_number,
                    block_hash: request.block_hash,
                    log_index: request.log_index,
                    confidence,
                    corrected_round: proxy_round.clone(),
                }));
            }
            OracleSnapshotStatus::Stale => {
                hooks.push(OracleHookEvent::PriceStale(OraclePriceStale {
                    id: registration.id,
                    proxy: registration.proxy,
                    aggregator: new_aggregator,
                    label: registration.label.clone(),
                    base: registration.base.clone(),
                    quote: registration.quote.clone(),
                    raw_answer: proxy_round.answer,
                    decimals: registration.metadata.decimals,
                    event_round_id: request.event_round.round_id,
                    updated_at: request.event_round.updated_at,
                    block_number: request.block_number,
                    block_hash: request.block_hash,
                    log_index: request.log_index,
                    confidence,
                    proxy_round: proxy_round.clone(),
                }));
            }
            OracleSnapshotStatus::Invalid
            | OracleSnapshotStatus::EventPending
            | OracleSnapshotStatus::Unknown => {}
        }

        if let Some(stored) = self
            .registry
            .registrations_map_mut()
            .get_mut(&registration.id)
        {
            stored.current_aggregator = new_aggregator;
            stored.aggregator_layout = new_layout;
        }
        self.registry.replace_snapshot(snapshot);
        self.complete_pending_reconciliation(request);
        Ok(hooks)
    }
}

/// Reconciles event-derived oracle state against authoritative proxy reads.
#[derive(Clone, Debug, Default)]
pub struct OracleReconciler {
    queue: std::collections::VecDeque<OracleReconciliationRequest>,
}

impl OracleReconciler {
    /// Enqueue an event reconciliation request.
    pub fn enqueue(&mut self, request: OracleReconciliationRequest) {
        self.queue.push_back(request);
    }

    /// Reconcile the next queued event against authoritative proxy reads.
    pub async fn reconcile_next<P: ChainlinkFeedProvider>(
        &mut self,
        tracker: &mut OracleTracker,
        provider: &P,
    ) -> Result<Option<OracleReconciliationResult>, OracleError> {
        let Some(request) = self.queue.front().cloned() else {
            return Ok(None);
        };

        let Some(registration) = tracker
            .registry
            .registrations_map()
            .get(&request.id)
            .cloned()
        else {
            return Err(OracleError::FeedNotFound);
        };
        let block_ref = request.block_ref();
        let read_proxy = registration.source.read_proxy(registration.proxy);
        let proxy_round = provider.latest_round_data_at(read_proxy, block_ref).await?;
        let new_aggregator = provider
            .aggregator_at(read_proxy, block_ref)
            .await
            .unwrap_or_else(|_| {
                tracker
                    .current_aggregator(request.id)
                    .or(request.aggregator)
            });
        let new_layout = tracker
            .detect_aggregator_layout(provider, new_aggregator, block_ref)
            .await;
        let hooks =
            tracker.apply_reconciled_request(&request, proxy_round, new_aggregator, new_layout)?;
        self.queue.pop_front();

        Ok(Some(OracleReconciliationResult { request, hooks }))
    }
}

impl OracleTracker {
    async fn detect_aggregator_layout<P: ChainlinkFeedProvider>(
        &mut self,
        provider: &P,
        aggregator: Option<Address>,
        block: Option<OracleBlockRef>,
    ) -> Option<AggregatorLayoutEvidence> {
        self.registry
            .detect_aggregator_layout(provider, aggregator, block)
            .await
    }

    fn current_aggregator(&self, id: FeedId) -> Option<Address> {
        self.registry
            .registrations_map()
            .get(&id)
            .and_then(|registration| registration.current_aggregator)
    }
}

fn reconciliation_confidence(
    status: &FeedStatus,
    request: &OracleReconciliationRequest,
    proxy_round: &RoundData,
) -> OracleSnapshotStatus {
    match status {
        FeedStatus::Fresh if proxy_round_matches_event(proxy_round, &request.event_round) => {
            OracleSnapshotStatus::Confirmed
        }
        FeedStatus::Fresh => OracleSnapshotStatus::Corrected,
        FeedStatus::Stale { .. } => OracleSnapshotStatus::Stale,
        FeedStatus::IncompleteRound | FeedStatus::InvalidAnswer => OracleSnapshotStatus::Invalid,
        FeedStatus::Unknown => OracleSnapshotStatus::Unknown,
    }
}

fn proxy_round_matches_event(proxy_round: &RoundData, event_round: &RoundData) -> bool {
    proxy_round.round_id == event_round.round_id
        && proxy_round.answer == event_round.answer
        && proxy_round.updated_at == event_round.updated_at
}

#[allow(dead_code)]
fn _ethereum_batch_report_type_is_supported(_: &ReactiveBatchReport<Ethereum>) {}
