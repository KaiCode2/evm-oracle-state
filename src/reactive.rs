use std::{borrow::Cow, collections::BTreeMap, sync::Arc};

use alloy_network::Ethereum;
use alloy_primitives::{Address, B256, U256};
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
    ANSWER_UPDATED_TOPIC, FeedRegistration, OCR1_NEW_TRANSMISSION_TOPIC,
    OCR2_NEW_TRANSMISSION_TOPIC, ORACLE_LEGACY_ANSWER_UPDATED_KIND, ORACLE_SIGNAL_NAMESPACE,
    Ocr1TransmissionStorageUpdate, Ocr2TransmissionStorageUpdate, OraclePriceUpdate,
    OracleSignalKind, OracleSnapshotStatus, OracleStorageEffect, OracleStorageSync, OracleUpdate,
    RoundData, decode_answer_updated, decode_ocr1_new_transmission, decode_ocr2_new_transmission,
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
        Self::with_storage_sync(registrations, OracleStorageSync::chainlink_defaults())
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
        let mut interests = Vec::new();
        for (aggregator, registrations) in &self.registrations_by_aggregator {
            let wants_ocr2 = registrations.iter().any(|registration| {
                self.storage_sync
                    .prefers_ocr2_new_transmission(registration)
            });
            let wants_ocr1 = registrations.iter().any(|registration| {
                self.storage_sync
                    .prefers_ocr1_new_transmission(registration)
            });
            let wants_answer = registrations.iter().any(|registration| {
                !self
                    .storage_sync
                    .prefers_ocr2_new_transmission(registration)
                    && !self
                        .storage_sync
                        .prefers_ocr1_new_transmission(registration)
            });

            if wants_answer {
                interests.push(log_interest(*aggregator, ANSWER_UPDATED_TOPIC));
            }
            if wants_ocr1 {
                interests.push(log_interest(*aggregator, OCR1_NEW_TRANSMISSION_TOPIC));
            }
            if wants_ocr2 {
                interests.push(log_interest(*aggregator, OCR2_NEW_TRANSMISSION_TOPIC));
            }
        }
        interests
    }

    /// Test helper that avoids requiring callers to construct a cache view.
    pub fn handle_for_test(
        &self,
        ctx: &ReactiveContext,
        input: &ReactiveInput<Ethereum>,
    ) -> Result<HandlerOutcome, HandlerError> {
        self.handle(ctx, input, &EmptyStateView)
    }

    fn handle_answer_updated(
        &self,
        ctx: &ReactiveContext,
        log: &alloy_rpc_types_eth::Log,
        state: &dyn StateView,
    ) -> Result<HandlerOutcome, HandlerError> {
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
        let mut handled_any = false;

        for registration in registrations {
            if self
                .storage_sync
                .prefers_ocr2_new_transmission(registration)
                || self
                    .storage_sync
                    .prefers_ocr1_new_transmission(registration)
            {
                continue;
            }
            handled_any = true;

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
                started_at: event.updated_at,
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
            apply_storage_effect(
                &mut effects,
                &mut needs_aggregator_purge,
                registration.proxy,
                self.storage_sync
                    .state_effect_for_answer(registration, &price_update, state)
                    .map_err(|error| HandlerError::new(format!("{error}")))?,
            );
            let hook_tags = hook_tags(registration, aggregator);
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

        if !handled_any {
            return Ok(HandlerOutcome::empty(StateEffectQuality::NoStateEffect));
        }
        prepend_aggregator_purge(&mut effects, needs_aggregator_purge, aggregator);

        Ok(HandlerOutcome {
            effects,
            quality: StateEffectQuality::RequiresRepair,
            tags,
        })
    }

    fn handle_ocr2_new_transmission(
        &self,
        ctx: &ReactiveContext,
        log: &alloy_rpc_types_eth::Log,
        state: &dyn StateView,
    ) -> Result<HandlerOutcome, HandlerError> {
        let aggregator = log.address();
        let Some(registrations) = self.registrations_by_aggregator.get(&aggregator) else {
            return Ok(HandlerOutcome::empty(StateEffectQuality::NoStateEffect));
        };

        let event = decode_ocr2_new_transmission(log).map_err(|error| {
            HandlerError::new(format!("decode OCR2 NewTransmission failed: {error}"))
        })?;
        let transmission_timestamp = ctx
            .block
            .as_ref()
            .and_then(|block| block.timestamp)
            .or(log.block_timestamp)
            .ok_or_else(|| {
                HandlerError::new("OCR2 NewTransmission handling requires block timestamp")
            })?;
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
        let mut handled_any = false;

        for registration in registrations {
            if !self
                .storage_sync
                .prefers_ocr2_new_transmission(registration)
            {
                continue;
            }
            handled_any = true;

            let price_update = OraclePriceUpdate {
                id: registration.id,
                proxy: registration.proxy,
                aggregator,
                label: registration.label.clone(),
                base: registration.base.clone(),
                quote: registration.quote.clone(),
                raw_answer: event.answer,
                decimals: registration.metadata.decimals,
                event_round_id: event.aggregator_round_id,
                started_at: event.observations_timestamp,
                updated_at: transmission_timestamp,
                block_number,
                block_hash,
                log_index,
                confidence: if log.removed {
                    OracleSnapshotStatus::Unknown
                } else {
                    OracleSnapshotStatus::EventPending
                },
            };
            let transmission_update = Ocr2TransmissionStorageUpdate {
                aggregator,
                aggregator_round_id: event.aggregator_round_id,
                answer: event.answer,
                observations_timestamp: event.observations_timestamp,
                transmission_timestamp,
                epoch_and_round: event.epoch_and_round,
            };
            apply_storage_effect(
                &mut effects,
                &mut needs_aggregator_purge,
                registration.proxy,
                self.storage_sync
                    .state_effect_for_ocr2_transmission(registration, &transmission_update, state)
                    .map_err(|error| HandlerError::new(format!("{error}")))?,
            );
            let hook_tags = hook_tags(registration, aggregator);
            tags.extend(hook_tags.clone());
            effects.push(ReactiveEffect::Hook(HookSignal {
                namespace: Cow::Borrowed(ORACLE_SIGNAL_NAMESPACE),
                kind: Cow::Borrowed(OracleSignalKind::PriceUpdate.as_str()),
                labels: hook_tags,
                payload: Some(Arc::new(price_update)),
            }));
        }

        if !handled_any {
            return Ok(HandlerOutcome::empty(StateEffectQuality::NoStateEffect));
        }
        prepend_aggregator_purge(&mut effects, needs_aggregator_purge, aggregator);

        Ok(HandlerOutcome {
            effects,
            quality: StateEffectQuality::RequiresRepair,
            tags,
        })
    }

    fn handle_ocr1_new_transmission(
        &self,
        ctx: &ReactiveContext,
        log: &alloy_rpc_types_eth::Log,
        state: &dyn StateView,
    ) -> Result<HandlerOutcome, HandlerError> {
        let aggregator = log.address();
        let Some(registrations) = self.registrations_by_aggregator.get(&aggregator) else {
            return Ok(HandlerOutcome::empty(StateEffectQuality::NoStateEffect));
        };

        let event = decode_ocr1_new_transmission(log).map_err(|error| {
            HandlerError::new(format!("decode OCR1 NewTransmission failed: {error}"))
        })?;
        let transmission_timestamp = ctx
            .block
            .as_ref()
            .and_then(|block| block.timestamp)
            .or(log.block_timestamp)
            .ok_or_else(|| {
                HandlerError::new("OCR1 NewTransmission handling requires block timestamp")
            })?;
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
        let mut handled_any = false;

        for registration in registrations {
            if !self
                .storage_sync
                .prefers_ocr1_new_transmission(registration)
            {
                continue;
            }
            handled_any = true;

            let price_update = OraclePriceUpdate {
                id: registration.id,
                proxy: registration.proxy,
                aggregator,
                label: registration.label.clone(),
                base: registration.base.clone(),
                quote: registration.quote.clone(),
                raw_answer: event.answer,
                decimals: registration.metadata.decimals,
                event_round_id: event.aggregator_round_id,
                started_at: transmission_timestamp,
                updated_at: transmission_timestamp,
                block_number,
                block_hash,
                log_index,
                confidence: if log.removed {
                    OracleSnapshotStatus::Unknown
                } else {
                    OracleSnapshotStatus::EventPending
                },
            };
            let transmission_update = Ocr1TransmissionStorageUpdate {
                aggregator,
                aggregator_round_id: event.aggregator_round_id,
                answer: event.answer,
                transmission_timestamp,
                epoch_and_round: event.epoch_and_round,
            };
            apply_storage_effect(
                &mut effects,
                &mut needs_aggregator_purge,
                registration.proxy,
                self.storage_sync
                    .state_effect_for_ocr1_transmission(registration, &transmission_update, state)
                    .map_err(|error| HandlerError::new(format!("{error}")))?,
            );
            let hook_tags = hook_tags(registration, aggregator);
            tags.extend(hook_tags.clone());
            effects.push(ReactiveEffect::Hook(HookSignal {
                namespace: Cow::Borrowed(ORACLE_SIGNAL_NAMESPACE),
                kind: Cow::Borrowed(OracleSignalKind::PriceUpdate.as_str()),
                labels: hook_tags,
                payload: Some(Arc::new(price_update)),
            }));
        }

        if !handled_any {
            return Ok(HandlerOutcome::empty(StateEffectQuality::NoStateEffect));
        }
        prepend_aggregator_purge(&mut effects, needs_aggregator_purge, aggregator);

        Ok(HandlerOutcome {
            effects,
            quality: StateEffectQuality::RequiresRepair,
            tags,
        })
    }
}

fn log_interest(aggregator: Address, topic: B256) -> ReactiveInterest<Ethereum> {
    ReactiveInterest::Logs(LogInterest {
        provider_filter: Filter::new().address(aggregator).event_signature(topic),
        local_matcher: None,
        route_key: Some(RouteKeySpec::EmitterAddress),
    })
}

fn apply_storage_effect(
    effects: &mut Vec<ReactiveEffect>,
    needs_aggregator_purge: &mut bool,
    proxy: Address,
    effect: OracleStorageEffect,
) {
    match effect {
        OracleStorageEffect::StateUpdates { updates, .. } => {
            effects.extend(updates.into_iter().map(ReactiveEffect::StateUpdate));
        }
        OracleStorageEffect::FallbackPurge => {
            *needs_aggregator_purge = true;
            effects.push(ReactiveEffect::Invalidate(InvalidationRequest {
                scope: PurgeScope::AllStorage,
                address: proxy,
                reason: InvalidationReason::HandlerRequested,
            }));
        }
    }
}

fn prepend_aggregator_purge(
    effects: &mut Vec<ReactiveEffect>,
    needs_aggregator_purge: bool,
    aggregator: Address,
) {
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
}

fn hook_tags(registration: &FeedRegistration, aggregator: Address) -> Vec<ReportTag> {
    vec![
        ReportTag::new("feed_id", registration.id.to_string()),
        ReportTag::new("proxy", format!("{:?}", registration.proxy)),
        ReportTag::new("aggregator", format!("{aggregator:?}")),
    ]
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
        state: &dyn StateView,
    ) -> Result<HandlerOutcome, HandlerError> {
        let ReactiveInput::Log(log) = input else {
            return Ok(HandlerOutcome::empty(StateEffectQuality::NoStateEffect));
        };
        if log.removed && !matches!(ctx.chain_status, ChainStatus::Reorged { .. }) {
            return Ok(HandlerOutcome::empty(StateEffectQuality::NoStateEffect));
        }

        match log.topics().first().copied() {
            Some(ANSWER_UPDATED_TOPIC) => self.handle_answer_updated(ctx, log, state),
            Some(OCR1_NEW_TRANSMISSION_TOPIC) => self.handle_ocr1_new_transmission(ctx, log, state),
            Some(OCR2_NEW_TRANSMISSION_TOPIC) => self.handle_ocr2_new_transmission(ctx, log, state),
            _ => Ok(HandlerOutcome::empty(StateEffectQuality::NoStateEffect)),
        }
    }
}

struct EmptyStateView;

impl StateView for EmptyStateView {
    fn storage(&self, _address: Address, _slot: U256) -> Option<U256> {
        None
    }
}
