use std::{borrow::Cow, collections::BTreeMap, sync::Arc};

use alloy_network::Ethereum;
use alloy_primitives::{Address, U256};
use alloy_rpc_types_eth::Filter;
use evm_fork_cache::{
    StateView,
    reactive::{
        ChainStatus, HandlerError, HandlerId, HandlerOutcome, HookSignal, InvalidationReason,
        InvalidationRequest, LogInterest, ReactiveContext, ReactiveEffect, ReactiveHandler,
        ReactiveInput, ReactiveInterest, ReportTag, RouteKeySpec, StateEffectQuality,
    },
    state_update::PurgeScope,
};

use crate::{
    ANSWER_UPDATED_TOPIC, FeedRegistration, ORACLE_LEGACY_ANSWER_UPDATED_KIND,
    ORACLE_SIGNAL_NAMESPACE, OraclePriceUpdate, OracleSignalKind, OracleSnapshotStatus,
    OracleStorageEffect, OracleStorageSync, OracleUpdate, RoundData, decode_answer_updated,
};

const HANDLER_ID: &str = "evm-oracle-state.chainlink";

/// Reactive-runtime bridge for Chainlink-compatible oracle feeds.
#[derive(Clone, Debug)]
pub struct OracleReactiveHandler {
    registrations_by_aggregator: BTreeMap<Address, Vec<FeedRegistration>>,
    storage_sync: OracleStorageSync,
}

impl OracleReactiveHandler {
    /// Create a handler from current feed registrations.
    pub fn new(registrations: Vec<FeedRegistration>) -> Self {
        Self::with_storage_sync(registrations, OracleStorageSync::default())
    }

    /// Create a handler from current feed registrations and storage adapters.
    pub fn with_storage_sync(
        registrations: Vec<FeedRegistration>,
        storage_sync: OracleStorageSync,
    ) -> Self {
        let mut registrations_by_aggregator: BTreeMap<Address, Vec<FeedRegistration>> =
            BTreeMap::new();
        for registration in registrations {
            if let Some(aggregator) = registration.current_aggregator {
                registrations_by_aggregator
                    .entry(aggregator)
                    .or_default()
                    .push(registration);
            }
        }
        Self {
            registrations_by_aggregator,
            storage_sync,
        }
    }

    /// Stable handler id.
    pub fn id(&self) -> HandlerId {
        HandlerId::new(HANDLER_ID)
    }

    /// Interests for current aggregators.
    pub fn interests(&self) -> Vec<ReactiveInterest<Ethereum>> {
        self.registrations_by_aggregator
            .keys()
            .copied()
            .map(|aggregator| {
                ReactiveInterest::Logs(LogInterest {
                    provider_filter: Filter::new()
                        .address(aggregator)
                        .event_signature(ANSWER_UPDATED_TOPIC),
                    local_matcher: None,
                    route_key: Some(RouteKeySpec::EmitterAddress),
                })
            })
            .collect()
    }

    /// Test helper that avoids requiring callers to construct a cache view.
    pub fn handle_for_test(
        &self,
        ctx: &ReactiveContext,
        input: &ReactiveInput<Ethereum>,
    ) -> Result<HandlerOutcome, HandlerError> {
        self.handle(ctx, input, &EmptyStateView)
    }
}

impl ReactiveHandler<Ethereum> for OracleReactiveHandler {
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

        let aggregator = log.address();
        let Some(registrations) = self.registrations_by_aggregator.get(&aggregator) else {
            return Ok(HandlerOutcome::empty(StateEffectQuality::NoStateEffect));
        };

        let event = decode_answer_updated(log)
            .map_err(|error| HandlerError::new(format!("decode AnswerUpdated failed: {error}")))?;
        let block_number = event
            .block_number
            .or_else(|| ctx.block.as_ref().map(|block| block.number));
        let block_hash = log
            .block_hash
            .or_else(|| ctx.block.as_ref().map(|block| block.hash));
        let log_index = event.log_index.or(ctx.log_index);

        let mut effects = Vec::new();
        let mut tags = Vec::new();
        let mut needs_aggregator_purge = false;

        for registration in registrations {
            let update = OracleUpdate {
                id: registration.id,
                proxy: registration.proxy,
                aggregator,
                round: RoundData {
                    round_id: event.round_id,
                    answer: event.current,
                    started_at: event.updated_at,
                    updated_at: event.updated_at,
                    answered_in_round: event.round_id,
                },
                block_number,
                log_index,
            };
            let price_update = OraclePriceUpdate {
                id: registration.id,
                proxy: registration.proxy,
                aggregator,
                label: registration.label.clone(),
                base: registration.base.clone(),
                quote: registration.quote.clone(),
                raw_answer: event.current,
                decimals: registration.metadata.decimals,
                event_round_id: event.round_id,
                updated_at: event.updated_at,
                block_number,
                block_hash,
                log_index,
                confidence: if log.removed {
                    OracleSnapshotStatus::Unknown
                } else {
                    OracleSnapshotStatus::EventPending
                },
            };
            match self
                .storage_sync
                .state_effect_for_answer(registration, &price_update, _state)
                .map_err(|error| HandlerError::new(format!("{error}")))?
            {
                OracleStorageEffect::StateUpdates { updates, .. } => {
                    effects.extend(updates.into_iter().map(ReactiveEffect::StateUpdate));
                }
                OracleStorageEffect::FallbackPurge => {
                    needs_aggregator_purge = true;
                    effects.push(ReactiveEffect::Invalidate(InvalidationRequest {
                        scope: PurgeScope::AllStorage,
                        address: registration.proxy,
                        reason: InvalidationReason::HandlerRequested,
                    }));
                }
            }
            let hook_tags = vec![
                ReportTag::new("feed_id", registration.id.to_string()),
                ReportTag::new("proxy", format!("{:?}", registration.proxy)),
                ReportTag::new("aggregator", format!("{aggregator:?}")),
            ];
            tags.extend(hook_tags.clone());
            if !log.removed {
                effects.push(ReactiveEffect::Hook(HookSignal {
                    namespace: Cow::Borrowed(ORACLE_SIGNAL_NAMESPACE),
                    kind: Cow::Borrowed(ORACLE_LEGACY_ANSWER_UPDATED_KIND),
                    labels: hook_tags.clone(),
                    payload: Some(Arc::new(update)),
                }));
            }
            effects.push(ReactiveEffect::Hook(HookSignal {
                namespace: Cow::Borrowed(ORACLE_SIGNAL_NAMESPACE),
                kind: Cow::Borrowed(OracleSignalKind::PriceUpdate.as_str()),
                labels: hook_tags,
                payload: Some(Arc::new(price_update)),
            }));
        }

        if needs_aggregator_purge {
            effects.insert(
                0,
                ReactiveEffect::Invalidate(InvalidationRequest {
                    scope: PurgeScope::AllStorage,
                    address: aggregator,
                    reason: InvalidationReason::HandlerRequested,
                }),
            );
        }

        Ok(HandlerOutcome {
            effects,
            quality: StateEffectQuality::RequiresRepair,
            tags,
        })
    }
}

struct EmptyStateView;

impl StateView for EmptyStateView {
    fn storage(&self, _address: Address, _slot: U256) -> Option<U256> {
        None
    }
}
