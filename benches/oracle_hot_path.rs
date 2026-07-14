//! Fully-offline oracle hot-path micro-benchmarks: the local-CPU work this
//! crate does between provider round-trips — warm-slot dedupe ahead of cache
//! prewarm, typed [`OracleReadOverlay`] reads, and direct-sync storage write
//! planning.
//!
//! The measured closures mirror the crate's example benches exactly so numbers
//! stay comparable across harnesses:
//!
//! - `warm_slot_dedupe_1024` and `pyth_overlay_lookup_1024` mirror
//!   `examples/oracle_cpu_overhead_bench.rs`;
//! - `direct_sync_cold_fallback` and `direct_sync_warm_exact_write` mirror the
//!   `run_direct_sync_microbench` loops in
//!   `examples/oracle_storage_warmup_bench.rs`.
//!
//! Unlike the live warmup example, this harness needs **no network and no env
//! vars**: every fixture is synthetic and in-memory, and nothing measured is
//! feature-gated. Run it anywhere, including CI, for regression tracking:
//!
//! ```text
//! cargo bench --bench oracle_hot_path
//! ```

use std::collections::BTreeMap;

use alloy_primitives::{Address, B256, I256, U256};
use alloy_sol_types::{SolCall, sol};
use criterion::{Criterion, black_box, criterion_group, criterion_main};
use evm_fork_cache::StateView;
use evm_oracle_state::{
    AggregatorLayoutEvidence, ChainlinkOcr2StorageAdapter, FeedId, FeedMetadata, FeedRegistration,
    FeedSource, OracleFeedStatus, OraclePriceUpdate, OracleReadOverlay, OracleRoundStatus,
    OracleStorageEffect, OracleStorageSync, OracleTracker, OracleValueSource, OracleValueStatus,
    RoundData, StalenessPolicy,
};

sol! {
    struct PythPrice {
        int64 price;
        uint64 conf;
        int32 expo;
        uint256 publishTime;
    }

    interface PythInterface {
        function getPriceUnsafe(bytes32 id) external view returns (PythPrice memory price);
    }
}

use PythInterface::getPriceUnsafeCall;

/// Feed count for the dedupe and overlay fixtures — matches the
/// `ORACLE_CPU_BENCH_FEEDS` default in `examples/oracle_cpu_overhead_bench.rs`.
const FEEDS: usize = 1024;

// --- fixtures (mirroring examples/oracle_cpu_overhead_bench.rs) ---------------

fn indexed_ocr2_registration(index: usize) -> FeedRegistration {
    let proxy = indexed_address(0x10, index);
    let aggregator = indexed_address(0x20, index);
    FeedRegistration {
        id: FeedId::new(format!("ocr2-feed-{index}")),
        proxy,
        label: Some(format!("OCR2 feed {index}")),
        base: Some(format!("ASSET{index}")),
        quote: Some("USD".to_string()),
        staleness: StalenessPolicy::max_age(3_600),
        current_aggregator: Some(aggregator),
        aggregator_layout: Some(AggregatorLayoutEvidence::from_type_and_version(
            aggregator,
            "AccessControlledOCR2Aggregator 1.0.0",
            None,
        )),
        metadata: metadata("OCR2"),
        source: FeedSource::Chainlink,
        status: OracleFeedStatus::Ready,
    }
}

fn pyth_registration(pyth: Address, index: usize) -> FeedRegistration {
    let proxy = indexed_address(0x40, index);
    let id = price_id(index);
    FeedRegistration {
        id: FeedId::new(format!("pyth-feed-{index}")),
        proxy,
        label: Some(format!("Pyth feed {index}")),
        base: Some(format!("ASSET{index}")),
        quote: Some("USD".to_string()),
        staleness: StalenessPolicy::max_age(3_600),
        current_aggregator: None,
        aggregator_layout: None,
        metadata: metadata("Pyth"),
        source: FeedSource::pyth(pyth, id, -8, 100),
        status: OracleFeedStatus::Ready,
    }
}

fn pyth_round(index: usize) -> RoundData {
    RoundData {
        round_id: U256::from(index + 1),
        answer: I256::unchecked_from(100_000_000_i64 + index as i64),
        started_at: 1_700_000_000,
        updated_at: 1_700_000_000,
        answered_in_round: U256::from(index + 1),
    }
}

fn metadata(prefix: &str) -> FeedMetadata {
    FeedMetadata {
        decimals: 8,
        description: format!("{prefix} / USD"),
        version: U256::from(1_u64),
    }
}

fn indexed_address(prefix: u8, index: usize) -> Address {
    let mut bytes = [prefix; 20];
    bytes[12..].copy_from_slice(&(index as u64).to_be_bytes());
    Address::from(bytes)
}

fn price_id(index: usize) -> B256 {
    let mut bytes = [0_u8; 32];
    bytes[24..].copy_from_slice(&(index as u64).to_be_bytes());
    B256::from(bytes)
}

// --- fixtures (mirroring examples/oracle_storage_warmup_bench.rs) -------------

fn ocr2_registration(proxy: Address, aggregator: Address) -> FeedRegistration {
    FeedRegistration {
        id: FeedId::new("eth-usd"),
        proxy,
        label: Some("ETH/USD".to_string()),
        base: Some("ETH".to_string()),
        quote: Some("USD".to_string()),
        staleness: StalenessPolicy::max_age(3_600),
        current_aggregator: Some(aggregator),
        aggregator_layout: Some(AggregatorLayoutEvidence::from_type_and_version(
            aggregator,
            "AccessControlledOCR2Aggregator 1.0.0",
            None,
        )),
        metadata: FeedMetadata {
            decimals: 8,
            description: "ETH / USD".to_string(),
            version: U256::from(4_u64),
        },
        source: FeedSource::Chainlink,
        status: OracleFeedStatus::Ready,
    }
}

fn price_update(proxy: Address, aggregator: Address) -> OraclePriceUpdate {
    OraclePriceUpdate {
        id: FeedId::new("eth-usd"),
        proxy,
        aggregator,
        label: Some("ETH/USD".to_string()),
        base: Some("ETH".to_string()),
        quote: Some("USD".to_string()),
        raw_answer: I256::unchecked_from(2_100_000_000_i64),
        decimals: 8,
        event_round_id: U256::from(101_u64),
        started_at: 1_700_000_100,
        updated_at: 1_700_000_100,
        block_number: Some(120),
        block_hash: Some(B256::repeat_byte(0x12)),
        log_index: Some(0),
        round_status: OracleRoundStatus::Fresh,
        value_status: OracleValueStatus::EventPending,
        source: OracleValueSource::Event,
    }
}

#[derive(Default)]
struct MapStateView {
    slots: BTreeMap<(Address, U256), U256>,
}

impl MapStateView {
    fn with_slot(mut self, address: Address, slot: U256, value: U256) -> Self {
        self.slots.insert((address, slot), value);
        self
    }
}

impl StateView for MapStateView {
    fn storage(&self, address: Address, slot: U256) -> Option<U256> {
        self.slots.get(&(address, slot)).copied()
    }
}

// --- benches -------------------------------------------------------------------

/// Warm-slot dedupe over 1024 synthetic OCR2 registrations: each iteration
/// walks every registration × storage adapter and BTreeSet-dedupes the hot
/// slots the direct-write path needs prewarmed.
fn bench_warm_slot_dedupe(c: &mut Criterion) {
    let registrations = (0..FEEDS)
        .map(indexed_ocr2_registration)
        .collect::<Vec<_>>();
    let refs = registrations.iter().collect::<Vec<_>>();
    let sync = OracleStorageSync::chainlink_defaults();

    // Pre-flight: every feed has a distinct aggregator, so dedupe must keep
    // exactly one hot-vars slot per feed.
    assert_eq!(
        sync.warm_slots_for_registrations(refs.iter().copied())
            .len(),
        FEEDS
    );

    c.bench_function("oracle_hot_path/warm_slot_dedupe_1024", |b| {
        b.iter(|| {
            let slots = sync.warm_slots_for_registrations(refs.iter().copied());
            black_box(slots.len())
        })
    });
}

/// Typed [`OracleReadOverlay`] answering a Pyth `getPriceUnsafe` view call
/// from in-memory state, with 1024 feeds registered and the worst-case (last
/// inserted) price id targeted.
fn bench_pyth_overlay_lookup(c: &mut Criterion) {
    let pyth = Address::repeat_byte(0x70);
    let registrations = (0..FEEDS)
        .map(|index| pyth_registration(pyth, index))
        .collect::<Vec<_>>();
    let rounds = registrations
        .iter()
        .enumerate()
        .map(|(index, registration)| (registration.clone(), pyth_round(index)))
        .collect::<Vec<_>>();
    let tracker =
        OracleTracker::from_registrations_at_timestamp(rounds, 1_700_000_100).expect("tracker");
    let overlay = OracleReadOverlay::new(&tracker);
    let target_price_id = price_id(FEEDS.saturating_sub(1));
    let calldata = getPriceUnsafeCall {
        id: target_price_id,
    }
    .abi_encode();

    // Pre-flight: the overlay must answer the target feed from typed state.
    let bytes = overlay
        .try_call(pyth, &calldata)
        .expect("overlay call")
        .expect("pyth price");
    assert!(!bytes.is_empty());

    c.bench_function("oracle_hot_path/pyth_overlay_lookup_1024", |b| {
        b.iter(|| {
            let bytes = overlay
                .try_call(pyth, &calldata)
                .expect("overlay call")
                .expect("pyth price");
            black_box(bytes.len())
        })
    });
}

/// Direct-sync storage planning for one OCR2 `AnswerUpdated`: the cold state
/// declines into purge/refetch, the warm state (hot packed word present) plans
/// exact masked writes.
fn bench_direct_sync(c: &mut Criterion) {
    let proxy = Address::repeat_byte(0x10);
    let aggregator = Address::repeat_byte(0x20);
    let registration = ocr2_registration(proxy, aggregator);
    let update = price_update(proxy, aggregator);
    let cold_state = MapStateView::default();
    let warm_state = MapStateView::default().with_slot(
        aggregator,
        ChainlinkOcr2StorageAdapter::hot_vars_slot(),
        U256::from(1_u64),
    );
    let sync = OracleStorageSync::chainlink_defaults();

    // Pre-flight: cold must fall back to purge/refetch, warm must plan exact
    // state updates.
    assert!(matches!(
        sync.state_effect_for_answer(&registration, &update, &cold_state)
            .expect("cold storage effect"),
        OracleStorageEffect::FallbackPurge
    ));
    assert!(matches!(
        sync.state_effect_for_answer(&registration, &update, &warm_state)
            .expect("warm storage effect"),
        OracleStorageEffect::StateUpdates { .. }
    ));

    c.bench_function("oracle_hot_path/direct_sync_cold_fallback", |b| {
        b.iter(|| {
            matches!(
                black_box(
                    sync.state_effect_for_answer(&registration, &update, &cold_state)
                        .expect("cold storage effect")
                ),
                OracleStorageEffect::FallbackPurge
            )
        })
    });
    c.bench_function("oracle_hot_path/direct_sync_warm_exact_write", |b| {
        b.iter(|| {
            matches!(
                black_box(
                    sync.state_effect_for_answer(&registration, &update, &warm_state)
                        .expect("warm storage effect")
                ),
                OracleStorageEffect::StateUpdates { .. }
            )
        })
    });
}

fn benches(c: &mut Criterion) {
    bench_warm_slot_dedupe(c);
    bench_pyth_overlay_lookup(c);
    bench_direct_sync(c);
}

criterion_group!(oracle_hot_path, benches);
criterion_main!(oracle_hot_path);
