#![cfg(any(feature = "morpho", feature = "euler"))]

use std::sync::Arc;

use alloy_network::Ethereum;
use alloy_primitives::{Address, B256, Bytes, I256, Log as PrimitiveLog, U256};
use alloy_provider::{RootProvider, network::AnyNetwork};
use alloy_rpc_client::RpcClient;
use alloy_rpc_types_eth::Log as RpcLog;
use alloy_sol_types::{SolCall, sol};
use alloy_transport::mock::Asserter;
use evm_fork_cache::{
    cache::EvmCache,
    reactive::{
        BlockRef, ChainStatus, InputSource, ReactiveConfig, ReactiveInput, ReactiveInputBatch,
        ReactiveInputRecord, ReactiveRuntime,
    },
};
use evm_oracle_state::{
    ANSWER_UPDATED_TOPIC, EulerQuoteLeg, FeedId, FeedMetadata, FeedRegistration, FeedSource,
    FeedStatus, MorphoChainlinkFeed, MorphoChainlinkFeedRole, OracleDependency,
    OracleReactiveHandler, OracleReadOverlay, OracleTracker, RoundData, SnapshotSource,
    StalenessPolicy,
};

sol! {
    interface MorphoOracleInterface {
        function price() external view returns (uint256);
    }

    interface EulerPriceOracleInterface {
        function getQuote(uint256 inAmount, address base, address quote) external view returns (uint256 outAmount);
    }
}

use EulerPriceOracleInterface::getQuoteCall;
use MorphoOracleInterface::priceCall;

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

fn round(answer: i64) -> RoundData {
    RoundData {
        round_id: U256::ZERO,
        answer: I256::unchecked_from(answer),
        started_at: 1_700_000_000,
        updated_at: 1_700_000_000,
        answered_in_round: U256::ZERO,
    }
}

fn metadata(decimals: u8, description: &str) -> FeedMetadata {
    FeedMetadata {
        decimals,
        description: description.to_string(),
        version: U256::ZERO,
    }
}

#[cfg(feature = "morpho")]
#[tokio::test]
async fn morpho_chainlink_dependency_event_updates_price_without_reconciliation_queue() {
    let oracle = Address::repeat_byte(0x10);
    let base_feed = Address::repeat_byte(0x11);
    let base_aggregator = Address::repeat_byte(0x12);
    let quote_feed = Address::repeat_byte(0x21);
    let quote_aggregator = Address::repeat_byte(0x22);
    let registration = FeedRegistration {
        id: FeedId::new("morpho-weth-usdc"),
        proxy: oracle,
        label: Some("Morpho WETH/USDC".to_string()),
        base: Some("WETH".to_string()),
        quote: Some("USDC".to_string()),
        staleness: StalenessPolicy::max_age(3600),
        current_aggregator: Some(base_aggregator),
        aggregator_layout: None,
        metadata: metadata(36, "Morpho WETH/USDC"),
        source: FeedSource::morpho_chainlink_v2(
            oracle,
            I256::unchecked_from(1_i64),
            I256::unchecked_from(1_i64),
            I256::unchecked_from(1_i64),
            vec![
                MorphoChainlinkFeed::new(
                    MorphoChainlinkFeedRole::BaseFeed1,
                    OracleDependency::new(
                        base_feed,
                        base_aggregator,
                        I256::unchecked_from(200_i64),
                    ),
                ),
                MorphoChainlinkFeed::new(
                    MorphoChainlinkFeedRole::QuoteFeed1,
                    OracleDependency::new(
                        quote_feed,
                        quote_aggregator,
                        I256::unchecked_from(2_i64),
                    ),
                ),
            ],
        ),
    };
    let mut tracker = OracleTracker::from_registrations_at_timestamp(
        vec![(registration.clone(), round(100))],
        1_700_000_010,
    )
    .unwrap();
    let handler = OracleReactiveHandler::new(vec![registration]);
    let mut runtime = ReactiveRuntime::<Ethereum>::new(ReactiveConfig::default());
    runtime.register_handler(Arc::new(handler)).unwrap();
    let mut cache = setup_cache().await;

    let log = rpc_log(
        base_aggregator,
        vec![
            ANSWER_UPDATED_TOPIC,
            int_topic(I256::unchecked_from(300_i64)),
            uint_topic(U256::from(9_u64)),
        ],
        word(U256::from(1_700_000_020_u64)),
        120,
        7,
    );
    let report = runtime
        .ingest_batch(&mut cache, batch(ReactiveInput::Log(log), context(120, 7)))
        .unwrap();
    tracker.apply_batch_report(&report).unwrap();
    let price = tracker.price("morpho-weth-usdc").unwrap();
    assert_eq!(price.raw_answer, I256::unchecked_from(150_i64));
    assert_eq!(price.status, FeedStatus::Fresh);
    assert_eq!(price.source, SnapshotSource::Event);
    assert!(tracker.pending_reconciliations().is_empty());

    let overlay = OracleReadOverlay::new(&tracker);
    let returned = overlay
        .try_call(oracle, &priceCall {}.abi_encode())
        .unwrap()
        .unwrap();
    let overlay_price = priceCall::abi_decode_returns(&returned).unwrap();
    assert_eq!(overlay_price, U256::from(150_u64));
}

#[cfg(feature = "euler")]
#[tokio::test]
async fn euler_chainlink_event_updates_quote_and_overlay_get_quote() {
    let oracle = Address::repeat_byte(0x30);
    let base = Address::repeat_byte(0x31);
    let quote = Address::repeat_byte(0x32);
    let feed = Address::repeat_byte(0x33);
    let aggregator = Address::repeat_byte(0x34);
    let registration = FeedRegistration {
        id: FeedId::new("euler-weth-usdc"),
        proxy: oracle,
        label: Some("Euler WETH/USDC".to_string()),
        base: Some("WETH".to_string()),
        quote: Some("USDC".to_string()),
        staleness: StalenessPolicy::max_age(3600),
        current_aggregator: Some(aggregator),
        aggregator_layout: None,
        metadata: metadata(6, "Euler WETH/USDC"),
        source: FeedSource::euler_quote(
            oracle,
            base,
            quote,
            18,
            6,
            EulerQuoteLeg::Chainlink {
                oracle,
                feed,
                aggregator,
                answer: I256::unchecked_from(200_000_000_i64),
                feed_decimals: 8,
                inverse: false,
                quote_decimals: 6,
            },
        ),
    };
    let mut tracker = OracleTracker::from_registrations_at_timestamp(
        vec![(registration.clone(), round(2_000_000))],
        1_700_000_010,
    )
    .unwrap();
    let handler = OracleReactiveHandler::new(vec![registration]);
    let mut runtime = ReactiveRuntime::<Ethereum>::new(ReactiveConfig::default());
    runtime.register_handler(Arc::new(handler)).unwrap();
    let mut cache = setup_cache().await;

    let log = rpc_log(
        aggregator,
        vec![
            ANSWER_UPDATED_TOPIC,
            int_topic(I256::unchecked_from(300_000_000_i64)),
            uint_topic(U256::from(10_u64)),
        ],
        word(U256::from(1_700_000_020_u64)),
        121,
        3,
    );
    let report = runtime
        .ingest_batch(&mut cache, batch(ReactiveInput::Log(log), context(121, 3)))
        .unwrap();
    tracker.apply_batch_report(&report).unwrap();

    let price = tracker.price("euler-weth-usdc").unwrap();
    assert_eq!(price.raw_answer, I256::unchecked_from(3_000_000_i64));
    assert!(tracker.pending_reconciliations().is_empty());

    let overlay = OracleReadOverlay::new(&tracker);
    let calldata = getQuoteCall {
        inAmount: U256::from(1_000_000_000_000_000_000_u128),
        base,
        quote,
    }
    .abi_encode();
    let returned = overlay.try_call(oracle, &calldata).unwrap().unwrap();
    let amount = getQuoteCall::abi_decode_returns(&returned).unwrap();
    assert_eq!(amount, U256::from(3_000_000_u64));

    let inverse_calldata = getQuoteCall {
        inAmount: U256::from(3_000_000_u64),
        base: quote,
        quote: base,
    }
    .abi_encode();
    let inverse_returned = overlay
        .try_call(oracle, &inverse_calldata)
        .unwrap()
        .unwrap();
    let inverse_amount = getQuoteCall::abi_decode_returns(&inverse_returned).unwrap();
    assert_eq!(inverse_amount, U256::from(1_000_000_000_000_000_000_u128));
}

#[cfg(feature = "euler")]
#[tokio::test]
async fn euler_cross_dependency_event_recomputes_composed_quote() {
    let oracle = Address::repeat_byte(0x50);
    let base = Address::repeat_byte(0x51);
    let cross = Address::repeat_byte(0x52);
    let quote = Address::repeat_byte(0x53);
    let cross_quote_oracle = Address::repeat_byte(0x54);
    let cross_quote_feed = Address::repeat_byte(0x55);
    let cross_quote_aggregator = Address::repeat_byte(0x56);
    let registration = FeedRegistration {
        id: FeedId::new("euler-cross"),
        proxy: oracle,
        label: Some("Euler Cross".to_string()),
        base: Some("BASE".to_string()),
        quote: Some("QUOTE".to_string()),
        staleness: StalenessPolicy::max_age(3600),
        current_aggregator: Some(cross_quote_aggregator),
        aggregator_layout: None,
        metadata: metadata(6, "Euler Cross"),
        source: FeedSource::euler_cross(
            oracle,
            base,
            cross,
            quote,
            18,
            18,
            6,
            EulerQuoteLeg::FixedRate {
                oracle,
                rate: I256::unchecked_from(2_000_000_000_000_000_000_i128),
                inverse: false,
                base_decimals: 18,
                quote_decimals: 18,
            },
            EulerQuoteLeg::Chainlink {
                oracle: cross_quote_oracle,
                feed: cross_quote_feed,
                aggregator: cross_quote_aggregator,
                answer: I256::unchecked_from(100_000_000_000_i64),
                feed_decimals: 8,
                inverse: false,
                quote_decimals: 6,
            },
        ),
    };
    let mut tracker = OracleTracker::from_registrations_at_timestamp(
        vec![(registration.clone(), round(2_000_000_000))],
        1_700_000_010,
    )
    .unwrap();
    let handler = OracleReactiveHandler::new(vec![registration]);
    let mut runtime = ReactiveRuntime::<Ethereum>::new(ReactiveConfig::default());
    runtime.register_handler(Arc::new(handler)).unwrap();
    let mut cache = setup_cache().await;

    let log = rpc_log(
        cross_quote_aggregator,
        vec![
            ANSWER_UPDATED_TOPIC,
            int_topic(I256::unchecked_from(150_000_000_000_i64)),
            uint_topic(U256::from(11_u64)),
        ],
        word(U256::from(1_700_000_020_u64)),
        122,
        4,
    );
    let report = runtime
        .ingest_batch(&mut cache, batch(ReactiveInput::Log(log), context(122, 4)))
        .unwrap();
    tracker.apply_batch_report(&report).unwrap();

    let price = tracker.price("euler-cross").unwrap();
    assert_eq!(price.raw_answer, I256::unchecked_from(3_000_000_000_i64));
    assert!(tracker.pending_reconciliations().is_empty());
}

#[cfg(feature = "euler")]
#[test]
fn euler_fixed_rate_source_has_no_chainlink_event_interest() {
    let oracle = Address::repeat_byte(0x40);
    let base = Address::repeat_byte(0x41);
    let quote = Address::repeat_byte(0x42);
    let registration = FeedRegistration {
        id: FeedId::new("euler-fixed"),
        proxy: oracle,
        label: Some("Euler Fixed".to_string()),
        base: Some("BASE".to_string()),
        quote: Some("QUOTE".to_string()),
        staleness: StalenessPolicy::max_age(3600),
        current_aggregator: None,
        aggregator_layout: None,
        metadata: metadata(6, "Euler Fixed"),
        source: FeedSource::euler_quote(
            oracle,
            base,
            quote,
            18,
            6,
            EulerQuoteLeg::FixedRate {
                oracle,
                rate: I256::unchecked_from(1_000_000_i64),
                inverse: false,
                base_decimals: 18,
                quote_decimals: 6,
            },
        ),
    };

    let handler = OracleReactiveHandler::new(vec![registration]);
    assert!(handler.interests().is_empty());
}
