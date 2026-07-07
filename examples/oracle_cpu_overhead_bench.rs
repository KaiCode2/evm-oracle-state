use std::{
    env,
    error::Error,
    hint::black_box,
    time::{Duration, Instant},
};

use alloy_primitives::{Address, B256, I256, U256};
use alloy_sol_types::{SolCall, sol};
use evm_oracle_state::{
    AggregatorLayoutEvidence, FeedId, FeedMetadata, FeedRegistration, FeedSource,
    OracleReadOverlay, OracleStorageSync, OracleTracker, RoundData, StalenessPolicy,
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

fn main() -> Result<(), Box<dyn Error>> {
    let feeds = env_usize("ORACLE_CPU_BENCH_FEEDS").unwrap_or(1024);
    let iterations = env_usize("ORACLE_CPU_BENCH_ITERS").unwrap_or(20_000);

    bench_warm_slot_dedupe(feeds, iterations);
    bench_pyth_overlay_lookup(feeds, iterations)?;
    Ok(())
}

fn bench_warm_slot_dedupe(feeds: usize, iterations: usize) {
    let registrations = (0..feeds).map(ocr2_registration).collect::<Vec<_>>();
    let refs = registrations.iter().collect::<Vec<_>>();
    let sync = OracleStorageSync::chainlink_defaults();
    let elapsed = time_loop(iterations, || {
        let slots = sync.warm_slots_for_registrations(refs.iter().copied());
        black_box(slots.len())
    });

    println!(
        "cpu_warm_slot_dedupe feeds={} iterations={} ns_per_iter={} checksum={}",
        feeds,
        iterations,
        per_iter_ns(elapsed, iterations),
        sync.warm_slots_for_registrations(refs.iter().copied())
            .len()
    );
}

fn bench_pyth_overlay_lookup(feeds: usize, iterations: usize) -> Result<(), Box<dyn Error>> {
    let pyth = Address::repeat_byte(0x70);
    let registrations = (0..feeds)
        .map(|index| pyth_registration(pyth, index))
        .collect::<Vec<_>>();
    let rounds = registrations
        .iter()
        .enumerate()
        .map(|(index, registration)| (registration.clone(), pyth_round(index)))
        .collect::<Vec<_>>();
    let tracker = OracleTracker::from_registrations_at_timestamp(rounds, 1_700_000_100)?;
    let overlay = OracleReadOverlay::new(&tracker);
    let target_price_id = price_id(feeds.saturating_sub(1));
    let calldata = getPriceUnsafeCall {
        id: target_price_id,
    }
    .abi_encode();

    let elapsed = time_loop(iterations, || {
        let bytes = overlay
            .try_call(pyth, &calldata)
            .expect("overlay call")
            .expect("pyth price");
        black_box(bytes.len())
    });

    println!(
        "cpu_pyth_overlay_lookup feeds={} iterations={} ns_per_call={} target_last=true",
        feeds,
        iterations,
        per_iter_ns(elapsed, iterations)
    );
    Ok(())
}

fn ocr2_registration(index: usize) -> FeedRegistration {
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

fn time_loop(iterations: usize, mut f: impl FnMut() -> usize) -> Duration {
    let started = Instant::now();
    let mut checksum = 0usize;
    for _ in 0..iterations {
        checksum ^= f();
    }
    black_box(checksum);
    started.elapsed()
}

fn per_iter_ns(elapsed: Duration, iterations: usize) -> u128 {
    elapsed.as_nanos() / iterations.max(1) as u128
}

fn env_usize(key: &str) -> Option<usize> {
    env::var(key).ok()?.parse().ok()
}
