use std::{borrow::Cow, sync::Arc};

use alloy_network::Ethereum;
use alloy_primitives::{Address, B256, Bytes, I256, Log as PrimitiveLog, U256};
use alloy_provider::{RootProvider, network::AnyNetwork};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::{Filter, Log as RpcLog};
use alloy_transport::mock::Asserter;
use evm_fork_cache::{
    StateView,
    cache::EvmCache,
    reactive::{
        BlockRef, ChainStatus, HandlerError, HandlerId, HandlerOutcome, HookSignal, InputSource,
        LogInterest, ReactiveEffect, ReactiveHandler, ReactiveInput, ReactiveInputBatch,
        ReactiveInputRecord, ReactiveInterest, ReportTag, RouteKeySpec, StateEffectQuality,
    },
};
use evm_oracle_state::{
    AdapterFuture, Feed, FeedId, FeedMetadata, FeedRegistration, FeedSource,
    ORACLE_SIGNAL_NAMESPACE, OracleAdapterFeedSkip, OracleAdapterId, OracleAdapterPlugin,
    OracleAdapterSkipReason, OracleDiscoveredFeed, OracleDiscoveryContext, OracleDiscoveryReport,
    OraclePriceUpdate, OracleRuntime, OracleSignalKind, OracleSnapshotStatus,
    OracleSourceDescriptor, OracleStorageSync, OracleTransformDescriptor, RoundData,
    StalenessPolicy,
};

const CUSTOM_TOPIC: B256 = B256::with_last_byte(0x91);

#[derive(Clone, Debug)]
struct CustomOracleAdapter {
    id: OracleAdapterId,
    proxy: Address,
    event_source: Address,
    initial_answer: I256,
}

impl CustomOracleAdapter {
    fn new(proxy: Address, event_source: Address, initial_answer: I256) -> Self {
        Self {
            id: OracleAdapterId::new("test.custom-oracle"),
            proxy,
            event_source,
            initial_answer,
        }
    }

    fn registration(&self) -> FeedRegistration {
        let source = OracleSourceDescriptor {
            adapter_id: self.id.clone(),
            source_kind: Cow::Borrowed("test-source"),
            user_facing_proxy: self.proxy,
            read_proxy: self.proxy,
            event_source: self.event_source,
            transform: OracleTransformDescriptor::Identity,
            metadata: Default::default(),
        };

        FeedRegistration {
            id: FeedId::new("custom-usd"),
            proxy: self.proxy,
            label: Some("Custom/USD".to_string()),
            base: Some("CUSTOM".to_string()),
            quote: Some("USD".to_string()),
            staleness: StalenessPolicy::max_age(3_600),
            current_aggregator: Some(self.event_source),
            aggregator_layout: None,
            metadata: metadata(),
            source: FeedSource::custom(source),
        }
    }
}

impl OracleAdapterPlugin for CustomOracleAdapter {
    fn adapter_id(&self) -> OracleAdapterId {
        self.id.clone()
    }

    fn discover<'a>(
        &'a self,
        _ctx: OracleDiscoveryContext<'a>,
    ) -> AdapterFuture<'a, OracleDiscoveryReport> {
        Box::pin(async move {
            Ok(OracleDiscoveryReport {
                feeds: vec![OracleDiscoveredFeed {
                    registration: self.registration(),
                    round: round(1, self.initial_answer, 1_700_000_000),
                }],
                skipped: Vec::new(),
            })
        })
    }

    fn reactive_handler(
        &self,
        registrations: Vec<FeedRegistration>,
        _storage_sync: OracleStorageSync,
    ) -> Arc<dyn ReactiveHandler<Ethereum>> {
        Arc::new(CustomOracleReactiveHandler {
            event_source: self.event_source,
            registrations,
        })
    }
}

#[derive(Clone, Debug)]
struct SkippingOracleAdapter {
    id: OracleAdapterId,
    proxy: Address,
}

impl SkippingOracleAdapter {
    fn new(proxy: Address) -> Self {
        Self {
            id: OracleAdapterId::new("test.skipping-oracle"),
            proxy,
        }
    }
}

impl OracleAdapterPlugin for SkippingOracleAdapter {
    fn adapter_id(&self) -> OracleAdapterId {
        self.id.clone()
    }

    fn discover<'a>(
        &'a self,
        _ctx: OracleDiscoveryContext<'a>,
    ) -> AdapterFuture<'a, OracleDiscoveryReport> {
        Box::pin(async move {
            Ok(OracleDiscoveryReport {
                feeds: Vec::new(),
                skipped: vec![OracleAdapterFeedSkip {
                    feed: Feed::proxy(self.proxy).id("skipped-custom").build(),
                    proxy: self.proxy,
                    reason: OracleAdapterSkipReason::NotChainlinkCompatible {
                        error: "synthetic unsupported oracle".to_string(),
                    },
                }],
            })
        })
    }

    fn reactive_handler(
        &self,
        registrations: Vec<FeedRegistration>,
        _storage_sync: OracleStorageSync,
    ) -> Arc<dyn ReactiveHandler<Ethereum>> {
        Arc::new(CustomOracleReactiveHandler {
            event_source: self.proxy,
            registrations,
        })
    }
}

#[derive(Clone, Debug)]
struct CustomOracleReactiveHandler {
    event_source: Address,
    registrations: Vec<FeedRegistration>,
}

impl ReactiveHandler<Ethereum> for CustomOracleReactiveHandler {
    fn id(&self) -> HandlerId {
        HandlerId::new("test.custom-oracle")
    }

    fn interests(&self) -> Vec<ReactiveInterest<Ethereum>> {
        vec![ReactiveInterest::Logs(LogInterest {
            provider_filter: Filter::new()
                .address(self.event_source)
                .event_signature(CUSTOM_TOPIC),
            local_matcher: None,
            route_key: Some(RouteKeySpec::EmitterAddress),
        })]
    }

    fn handle(
        &self,
        ctx: &evm_fork_cache::reactive::ReactiveContext,
        input: &ReactiveInput<Ethereum>,
        _state: &dyn StateView,
    ) -> Result<HandlerOutcome, HandlerError> {
        let ReactiveInput::Log(log) = input else {
            return Ok(HandlerOutcome::empty(StateEffectQuality::NoStateEffect));
        };
        if log.address() != self.event_source || log.topics().first().copied() != Some(CUSTOM_TOPIC)
        {
            return Ok(HandlerOutcome::empty(StateEffectQuality::NoStateEffect));
        }

        let answer = I256::unchecked_from(
            U256::try_from_be_slice(log.data().data.as_ref())
                .unwrap_or_default()
                .to::<u64>() as i128,
        );
        let block_number = log
            .block_number
            .or_else(|| ctx.block.as_ref().map(|b| b.number));
        let block_hash = log
            .block_hash
            .or_else(|| ctx.block.as_ref().map(|b| b.hash));
        let log_index = log.log_index.or(ctx.log_index);

        let effects = self
            .registrations
            .iter()
            .map(|registration| {
                ReactiveEffect::Hook(HookSignal {
                    namespace: Cow::Borrowed(ORACLE_SIGNAL_NAMESPACE),
                    kind: Cow::Borrowed(OracleSignalKind::PriceUpdate.as_str()),
                    labels: vec![ReportTag::new("feed_id", registration.id.to_string())],
                    payload: Some(Arc::new(OraclePriceUpdate {
                        id: registration.id,
                        proxy: registration.proxy,
                        aggregator: self.event_source,
                        label: registration.label.clone(),
                        base: registration.base.clone(),
                        quote: registration.quote.clone(),
                        raw_answer: answer,
                        decimals: registration.metadata.decimals,
                        event_round_id: U256::from(2_u64),
                        started_at: 1_700_000_120,
                        updated_at: 1_700_000_120,
                        block_number,
                        block_hash,
                        log_index,
                        confidence: OracleSnapshotStatus::EventPending,
                    })),
                })
            })
            .collect();

        Ok(HandlerOutcome {
            effects,
            quality: StateEffectQuality::RequiresRepair,
            tags: Vec::new(),
        })
    }
}

#[tokio::test]
async fn external_adapter_discovers_feed_and_installs_custom_reactive_handler() {
    let proxy = Address::repeat_byte(0x10);
    let event_source = Address::repeat_byte(0x20);
    let adapter = CustomOracleAdapter::new(proxy, event_source, I256::unchecked_from(123_i64));
    let mut cache = setup_cache().await;

    let runtime = OracleRuntime::cache_builder()
        .install_adapter(adapter)
        .now_timestamp(1_700_000_030)
        .build(&mut cache)
        .await
        .expect("external adapter should discover and register its feed");

    let price = runtime.price("custom-usd").expect("registered price");
    assert_eq!(price.raw_answer, I256::unchecked_from(123_i64));
    assert_eq!(price.base.as_deref(), Some("CUSTOM"));
    assert_eq!(price.quote.as_deref(), Some("USD"));

    let reactive = runtime
        .reactive_runtime()
        .expect("external adapter handler should install");
    let matching_log = rpc_log(event_source, vec![CUSTOM_TOPIC], Vec::new(), 101, 0);
    assert!(reactive.interests().iter().any(|interest| match interest {
        ReactiveInterest::Logs(interest) => interest.matches(&matching_log),
        _ => false,
    }));
}

#[tokio::test]
async fn external_adapter_event_updates_tracker_state_through_standard_oracle_hook() {
    let proxy = Address::repeat_byte(0x11);
    let event_source = Address::repeat_byte(0x21);
    let adapter = CustomOracleAdapter::new(proxy, event_source, I256::unchecked_from(100_i64));
    let mut cache = setup_cache().await;
    let mut oracle = OracleRuntime::cache_builder()
        .install_adapter(adapter)
        .now_timestamp(1_700_000_030)
        .build(&mut cache)
        .await
        .expect("external adapter should build runtime");
    let mut reactive = oracle
        .reactive_runtime()
        .expect("external adapter handler should install");

    let report = reactive
        .ingest_batch(
            &mut cache,
            batch(
                ReactiveInput::Log(rpc_log(
                    event_source,
                    vec![CUSTOM_TOPIC],
                    U256::from(456_u64).to_be_bytes::<32>().to_vec(),
                    105,
                    7,
                )),
                context(105, 7),
            ),
        )
        .expect("custom oracle event should commit");

    let events = oracle
        .apply_batch_report(&report)
        .expect("standard oracle hook should update tracker state");
    assert_eq!(events.len(), 1);

    let price = oracle.price("custom-usd").expect("updated price");
    assert_eq!(price.raw_answer, I256::unchecked_from(456_i64));
    assert!(price.is_event_pending());
    assert_eq!(price.updated_at, 1_700_000_120);
}

#[tokio::test]
async fn cache_runtime_builder_rejects_duplicate_feed_ids_across_external_adapters() {
    let mut cache = setup_cache().await;
    let first = CustomOracleAdapter::new(
        Address::repeat_byte(0x31),
        Address::repeat_byte(0x41),
        I256::unchecked_from(1_i64),
    );
    let second = CustomOracleAdapter::new(
        Address::repeat_byte(0x32),
        Address::repeat_byte(0x42),
        I256::unchecked_from(2_i64),
    );

    let error = OracleRuntime::cache_builder()
        .install_adapter(first)
        .install_adapter(second)
        .now_timestamp(1_700_000_030)
        .build(&mut cache)
        .await
        .expect_err("duplicate feed ids from external adapters should be rejected");

    assert!(
        error.to_string().contains("custom-usd"),
        "duplicate-feed error should identify the colliding feed id: {error}"
    );
}

#[tokio::test]
async fn cache_runtime_builder_exposes_and_fails_fast_on_adapter_skips() {
    let proxy = Address::repeat_byte(0x51);
    let mut cache = setup_cache().await;
    let report = OracleRuntime::cache_builder()
        .install_adapter(SkippingOracleAdapter::new(proxy))
        .now_timestamp(1_700_000_030)
        .build_report(&mut cache)
        .await
        .expect("build_report should surface adapter skips");

    assert_eq!(report.skipped.len(), 1);
    assert_eq!(report.skipped[0].proxy, proxy);
    assert!(
        report.runtime.price("skipped-custom").is_err(),
        "skipped feeds should not seed typed state"
    );

    let error = OracleRuntime::cache_builder()
        .install_adapter(SkippingOracleAdapter::new(proxy))
        .now_timestamp(1_700_000_030)
        .build(&mut cache)
        .await
        .expect_err("build should fail fast when adapters report skipped feeds");
    assert!(
        error.to_string().contains("skipped-custom")
            && error.to_string().contains("synthetic unsupported oracle"),
        "skip error should identify the skipped feed and reason: {error}"
    );
}

async fn setup_cache() -> EvmCache {
    let asserter = Asserter::new();
    let client = RpcClient::mocked(asserter);
    let provider = RootProvider::<AnyNetwork>::new(client);
    EvmCache::new(Arc::new(provider)).await
}

fn metadata() -> FeedMetadata {
    FeedMetadata {
        decimals: 8,
        description: "CUSTOM / USD".to_string(),
        version: U256::ZERO,
    }
}

fn round(id: u64, answer: I256, updated_at: u64) -> RoundData {
    RoundData {
        round_id: U256::from(id),
        answer,
        started_at: updated_at,
        updated_at,
        answered_in_round: U256::from(id),
    }
}

fn rpc_log(
    address: Address,
    topics: Vec<B256>,
    data: Vec<u8>,
    block_number: u64,
    log_index: u64,
) -> RpcLog {
    RpcLog {
        inner: PrimitiveLog::new_unchecked(address, topics, Bytes::from(data)),
        block_hash: Some(block_hash(block_number)),
        block_number: Some(block_number),
        block_timestamp: Some(1_700_000_000 + block_number),
        transaction_hash: Some(B256::repeat_byte(0x77)),
        transaction_index: Some(0),
        log_index: Some(log_index),
        removed: false,
    }
}

fn block_hash(block_number: u64) -> B256 {
    B256::repeat_byte(block_number as u8)
}

fn context(block_number: u64, log_index: u64) -> evm_fork_cache::reactive::ReactiveContext {
    let block = BlockRef {
        number: block_number,
        hash: block_hash(block_number),
        parent_hash: Some(block_hash(block_number.saturating_sub(1))),
        timestamp: Some(1_700_000_000 + block_number),
    };
    evm_fork_cache::reactive::ReactiveContext {
        chain_id: Some(1),
        source: InputSource::Synthetic,
        chain_status: ChainStatus::Included {
            block: block.clone(),
            confirmations: 0,
        },
        block: Some(block),
        transaction_index: Some(0),
        log_index: Some(log_index),
    }
}

fn batch(
    input: ReactiveInput<Ethereum>,
    context: evm_fork_cache::reactive::ReactiveContext,
) -> ReactiveInputBatch<Ethereum> {
    ReactiveInputBatch::new(vec![ReactiveInputRecord::new(input, context)])
}
