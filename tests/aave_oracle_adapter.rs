#![cfg(feature = "aave")]

use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};

use alloy_eips::BlockId;
use alloy_network::Ethereum;
use alloy_primitives::{Address, B256, Bytes, I256, Log as PrimitiveLog, U256};
use alloy_provider::{RootProvider, network::AnyNetwork};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::Log as RpcLog;
use alloy_transport::mock::Asserter;
use evm_fork_cache::{
    cache::EvmCache,
    reactive::{
        BlockRef, ChainStatus, HandlerId, HookSignal, InputSource, ReactiveConfig, ReactiveInput,
        ReactiveInputBatch, ReactiveInputRecord, ReactiveInterest, ReactiveRuntime, RouteKey,
        RouteKeySpec, StateEffectQuality,
    },
};
use evm_oracle_state::{
    ANSWER_UPDATED_TOPIC, AaveAsset, AaveV3OracleAdapter, AssetId, ChainlinkFeedProvider,
    Denomination, FeedId, FeedMetadata, FeedRegistration, FeedSource, OracleHookEvent,
    OraclePriceUpdate, OracleReactiveHandler, OracleReconciler, OracleTracker, ProviderFuture,
    RoundData, SnapshotSource, StalenessPolicy,
};

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
        transaction_hash: Some(B256::repeat_byte(0x44)),
        transaction_index: Some(0),
        log_index: Some(log_index),
        removed: false,
    }
}

fn block_hash(block_number: u64) -> B256 {
    B256::repeat_byte(block_number as u8)
}

fn block_ref(block_number: u64) -> BlockRef {
    BlockRef {
        number: block_number,
        hash: block_hash(block_number),
        parent_hash: Some(block_hash(block_number.saturating_sub(1))),
        timestamp: Some(1_700_000_000 + block_number),
    }
}

fn context(block_number: u64, log_index: u64) -> evm_fork_cache::reactive::ReactiveContext {
    let block = block_ref(block_number);
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

async fn setup_cache() -> EvmCache {
    let asserter = Asserter::new();
    let client = RpcClient::mocked(asserter);
    let provider = RootProvider::<AnyNetwork>::new(client);
    EvmCache::new(Arc::new(provider)).await
}

fn word(value: U256) -> Vec<u8> {
    value.to_be_bytes::<32>().to_vec()
}

fn uint_topic(value: U256) -> B256 {
    B256::from(value.to_be_bytes::<32>())
}

fn int_topic(value: I256) -> B256 {
    B256::from(value.into_raw().to_be_bytes::<32>())
}

fn metadata(description: &str) -> FeedMetadata {
    FeedMetadata {
        decimals: 8,
        description: description.to_string(),
        version: U256::ZERO,
    }
}

fn round(id: u64, answer: i64, updated_at: u64) -> RoundData {
    RoundData {
        round_id: U256::from(id),
        answer: I256::unchecked_from(answer),
        started_at: updated_at,
        updated_at,
        answered_in_round: U256::from(id),
    }
}

fn aave_cap_registration(
    source: Address,
    underlying_proxy: Address,
    underlying_aggregator: Address,
    cap: i64,
) -> FeedRegistration {
    FeedRegistration {
        id: FeedId::new("aave-v3-usdt"),
        proxy: source,
        label: Some("Aave V3 USDT source".to_string()),
        base: Some("USDT".to_string()),
        quote: Some("USD".to_string()),
        staleness: StalenessPolicy::max_age(24 * 60 * 60),
        current_aggregator: Some(underlying_aggregator),
        aggregator_layout: None,
        metadata: metadata("Capped USDT/USD"),
        source: FeedSource::aave_price_cap_stable(
            source,
            underlying_proxy,
            I256::unchecked_from(cap),
        ),
    }
}

fn price_update_hooks(signals: &[HookSignal]) -> Vec<OraclePriceUpdate> {
    signals
        .iter()
        .filter_map(|signal| {
            if signal.kind.as_ref() != "oracle.price_update" {
                return None;
            }
            signal
                .payload
                .as_ref()
                .and_then(|payload| payload.downcast_ref::<OraclePriceUpdate>())
                .cloned()
        })
        .collect()
}

#[test]
fn aave_builder_api_tracks_assets_declaratively() {
    let oracle = Address::repeat_byte(0x01);
    let usdt = Address::repeat_byte(0x02);
    let adapter = AaveV3OracleAdapter::new(oracle)
        .asset(
            AaveAsset::new(usdt)
                .id("aave-v3-usdt")
                .label("Aave V3 USDT")
                .base(AssetId::symbol("USDT"))
                .quote(Denomination::Usd)
                .max_age_secs(24 * 60 * 60),
        )
        .now_timestamp(1_700_000_000);

    assert_eq!(adapter.oracle(), oracle);
    assert_eq!(adapter.assets().len(), 1);
    assert_eq!(adapter.assets()[0].asset(), usdt);
}

#[tokio::test]
async fn price_cap_stable_underlying_event_emits_aave_normalized_price() {
    let source = Address::repeat_byte(0x10);
    let underlying_proxy = Address::repeat_byte(0x11);
    let underlying_aggregator = Address::repeat_byte(0x12);
    let cap = 104_000_000_i64;
    let registration = aave_cap_registration(source, underlying_proxy, underlying_aggregator, cap);
    let mut tracker = OracleTracker::from_registrations_at_timestamp(
        vec![(registration.clone(), round(100, 99_000_000, 1_700_000_000))],
        1_700_000_200,
    )
    .expect("seed tracker");
    let handler = OracleReactiveHandler::new(tracker.registrations().collect::<Vec<_>>());
    let matching_interest = handler
        .interests()
        .into_iter()
        .find_map(|interest| match interest {
            ReactiveInterest::Logs(interest) => {
                let log = rpc_log(
                    underlying_aggregator,
                    vec![
                        ANSWER_UPDATED_TOPIC,
                        int_topic(I256::unchecked_from(110_000_000_i64)),
                        uint_topic(U256::from(101_u64)),
                    ],
                    word(U256::from(1_700_000_180_u64)),
                    102,
                    0,
                );
                interest.matches(&log).then_some(interest)
            }
            _ => None,
        })
        .expect("underlying Chainlink aggregator should be subscribed");
    assert_eq!(
        matching_interest.route_key(&rpc_log(
            underlying_aggregator,
            vec![
                ANSWER_UPDATED_TOPIC,
                int_topic(I256::unchecked_from(110_000_000_i64)),
                uint_topic(U256::from(101_u64)),
            ],
            word(U256::from(1_700_000_180_u64)),
            102,
            0,
        )),
        Some(RouteKey::Address(underlying_aggregator))
    );
    assert!(matches!(
        matching_interest.route_key,
        Some(RouteKeySpec::EmitterAddress)
    ));

    let mut runtime = ReactiveRuntime::<Ethereum>::new(ReactiveConfig::default());
    runtime
        .register_handler(Arc::new(handler))
        .expect("register handler");
    let mut cache = setup_cache().await;
    let log = rpc_log(
        underlying_aggregator,
        vec![
            ANSWER_UPDATED_TOPIC,
            int_topic(I256::unchecked_from(110_000_000_i64)),
            uint_topic(U256::from(101_u64)),
        ],
        word(U256::from(1_700_000_180_u64)),
        102,
        0,
    );
    let batch_report = runtime
        .ingest_batch(&mut cache, batch(ReactiveInput::Log(log), context(102, 0)))
        .expect("runtime should commit handler effects");

    assert_eq!(batch_report.applied.len(), 1);
    let applied = &batch_report.applied[0];
    assert_eq!(
        applied.handler_id,
        HandlerId::new("evm-oracle-state.chainlink")
    );
    assert_eq!(applied.quality, StateEffectQuality::RequiresRepair);
    let hooks = price_update_hooks(&applied.hook_signals);
    assert_eq!(hooks.len(), 1);
    let hook = &hooks[0];
    assert_eq!(hook.proxy, source);
    assert_eq!(hook.aggregator, underlying_aggregator);
    assert_eq!(hook.raw_answer, I256::unchecked_from(cap));
    assert_eq!(hook.base.as_deref(), Some("USDT"));
    assert_eq!(hook.quote.as_deref(), Some("USD"));

    tracker
        .apply_batch_report(&batch_report)
        .expect("committed report should update typed state");
    let snapshot = tracker
        .latest(source)
        .expect("updated Aave source snapshot");
    assert_eq!(snapshot.round.answer, I256::unchecked_from(cap));
    assert_eq!(snapshot.round.updated_at, 1_700_000_180);
    assert_eq!(snapshot.source, SnapshotSource::Event);
    assert_eq!(tracker.pending_reconciliations().len(), 1);
    assert_eq!(tracker.pending_reconciliations()[0].proxy, source);
}

#[tokio::test]
async fn price_cap_stable_reconciliation_reads_underlying_proxy_and_confirms_capped_round() {
    let source = Address::repeat_byte(0x20);
    let underlying_proxy = Address::repeat_byte(0x21);
    let underlying_aggregator = Address::repeat_byte(0x22);
    let cap = 104_000_000_i64;
    let registration = aave_cap_registration(source, underlying_proxy, underlying_aggregator, cap);
    let mut tracker = OracleTracker::from_registrations_at_timestamp(
        vec![(registration.clone(), round(100, 99_000_000, 1_700_000_000))],
        1_700_000_200,
    )
    .expect("seed tracker");
    let handler = OracleReactiveHandler::new(tracker.registrations().collect::<Vec<_>>());
    let mut runtime = ReactiveRuntime::<Ethereum>::new(ReactiveConfig::default());
    runtime
        .register_handler(Arc::new(handler))
        .expect("register handler");
    let mut cache = setup_cache().await;
    let log = rpc_log(
        underlying_aggregator,
        vec![
            ANSWER_UPDATED_TOPIC,
            int_topic(I256::unchecked_from(110_000_000_i64)),
            uint_topic(U256::from(101_u64)),
        ],
        word(U256::from(1_700_000_180_u64)),
        102,
        0,
    );
    let batch_report = runtime
        .ingest_batch(&mut cache, batch(ReactiveInput::Log(log), context(102, 0)))
        .expect("runtime should commit handler effects");
    tracker
        .apply_batch_report(&batch_report)
        .expect("committed report should update typed state");

    let request = tracker.pending_reconciliations()[0].clone();
    let provider = RecordingProvider::new(
        underlying_proxy,
        underlying_aggregator,
        round(101, 110_000_000, 1_700_000_180),
    );
    let mut reconciler = OracleReconciler::default();
    reconciler.enqueue(request);
    let result = reconciler
        .reconcile_next(&mut tracker, &provider)
        .await
        .expect("reconcile should succeed")
        .expect("request should reconcile");

    assert_eq!(provider.latest_round_calls(), vec![underlying_proxy]);
    assert!(matches!(
        result.hooks.as_slice(),
        [OracleHookEvent::PriceConfirmed(confirmed)]
            if confirmed.proxy == source
                && confirmed.raw_answer == I256::unchecked_from(cap)
                && confirmed.proxy_round.answer == I256::unchecked_from(cap)
    ));
    let snapshot = tracker
        .latest(source)
        .expect("confirmed Aave source snapshot");
    assert_eq!(snapshot.round.answer, I256::unchecked_from(cap));
    assert!(!snapshot.requires_reconciliation());
}

#[derive(Clone, Debug)]
struct RecordingProvider {
    expected_proxy: Address,
    aggregator: Address,
    latest_round: RoundData,
    latest_round_calls: Arc<Mutex<Vec<Address>>>,
}

impl RecordingProvider {
    fn new(expected_proxy: Address, aggregator: Address, latest_round: RoundData) -> Self {
        Self {
            expected_proxy,
            aggregator,
            latest_round,
            latest_round_calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn latest_round_calls(&self) -> Vec<Address> {
        self.latest_round_calls.lock().unwrap().clone()
    }
}

impl ChainlinkFeedProvider for RecordingProvider {
    fn decimals(&self, _proxy: Address) -> ProviderFuture<'_, u8> {
        Box::pin(async { Ok(8) })
    }

    fn description(&self, _proxy: Address) -> ProviderFuture<'_, String> {
        Box::pin(async { Ok("USDT / USD".to_string()) })
    }

    fn version(&self, _proxy: Address) -> ProviderFuture<'_, U256> {
        Box::pin(async { Ok(U256::from(6_u64)) })
    }

    fn latest_round_data(&self, proxy: Address) -> ProviderFuture<'_, RoundData> {
        self.latest_round_data_at(proxy, None)
    }

    fn latest_round_data_at(
        &self,
        proxy: Address,
        _block: Option<evm_oracle_state::OracleBlockRef>,
    ) -> ProviderFuture<'_, RoundData> {
        assert_eq!(proxy, self.expected_proxy);
        self.latest_round_calls.lock().unwrap().push(proxy);
        Box::pin(async move { Ok(self.latest_round.clone()) })
    }

    fn aggregator(&self, proxy: Address) -> ProviderFuture<'_, Option<Address>> {
        self.aggregator_at(proxy, None)
    }

    fn aggregator_at(
        &self,
        proxy: Address,
        _block: Option<evm_oracle_state::OracleBlockRef>,
    ) -> ProviderFuture<'_, Option<Address>> {
        assert_eq!(proxy, self.expected_proxy);
        Box::pin(async move { Ok(Some(self.aggregator)) })
    }

    fn aggregator_type_and_version(
        &self,
        _aggregator: Address,
    ) -> Pin<
        Box<dyn Future<Output = Result<Option<String>, evm_oracle_state::OracleError>> + Send + '_>,
    > {
        Box::pin(async { Ok(None) })
    }

    fn aggregator_code_hash(
        &self,
        _aggregator: Address,
    ) -> Pin<
        Box<dyn Future<Output = Result<Option<B256>, evm_oracle_state::OracleError>> + Send + '_>,
    > {
        Box::pin(async { Ok(None) })
    }
}

#[allow(dead_code)]
fn _block_id_is_available_for_reconcile_signatures(block: BlockId) -> BlockId {
    block
}

#[allow(dead_code)]
fn _btree_map_is_available_for_future_source_graphs<K: Ord, V>(
    map: BTreeMap<K, V>,
) -> BTreeMap<K, V> {
    map
}
