//! Offline, compile-checked pending-oracle integration.
//!
//! Feeds are registered once on `OracleRuntime`. Optional source tasks reuse that
//! registration scope and publish through a separate speculative channel.

use std::error::Error;

use alloy_primitives::{Address, B256, I256, U256};
use evm_oracle_state::{
    ChainlinkFeed, ChainlinkFeedProvider, FeedId, OracleAdapterId, OracleRuntime,
    PendingOracleConfig, PendingOracleEvent, PendingOracleEvidence, PendingOracleOrderingHandle,
    PendingOracleRoute, PendingOracleSimulationMaterial, PendingOracleSourceId,
    PendingOracleTransmission, PendingOracleTransmissionId, PendingOracleUpdate,
    PendingOracleUpdateId, PendingOracleValue, ProviderFuture, RoundData,
};

const PROXY: Address = Address::repeat_byte(0x10);
const AGGREGATOR: Address = Address::repeat_byte(0x20);

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn Error>> {
    #[allow(unused_mut)]
    let mut oracle = OracleRuntime::builder(DemoProvider)
        .now_timestamp(1_700_000_100)
        .feed(
            ChainlinkFeed::new(PROXY)
                .id("eth-usd")
                .label("ETH/USD")
                .base("ETH")
                .quote("USD")
                .max_age_secs(3_600),
        )
        // This is a projection of the registration above, not another feed list.
        .pending_updates(
            PendingOracleConfig::ethereum_mainnet()
                .public_mempool()
                .mev_share()
                .channel_capacity(1_024),
        )
        .build()
        .await?;
    let mut updates = oracle
        .pending_updates()
        .expect("pending channel enabled")
        .subscribe();
    #[cfg(any(
        feature = "pending-oracle-ethereum",
        feature = "pending-oracle-mev-share"
    ))]
    let mut live_sources = 0_usize;

    // Built-in transports are separate Cargo features because they add network
    // dependencies. Supplying the URL opts in to I/O; the feed is not repeated.
    #[cfg(feature = "pending-oracle-ethereum")]
    if let Ok(ws_url) = std::env::var("EVM_ORACLE_ALCHEMY_WS_URL") {
        oracle.start_pending_source(evm_oracle_state::AlchemyPendingTransactionSource::new(
            ws_url,
        ))?;
        live_sources += 1;
    }
    #[cfg(feature = "pending-oracle-mev-share")]
    if std::env::var_os("EVM_ORACLE_ENABLE_MEV_SHARE").is_some() {
        oracle.start_pending_source(
            evm_oracle_state::MevSharePendingTransactionSource::ethereum_mainnet(),
        )?;
        live_sources += 1;
    }

    #[cfg(any(
        feature = "pending-oracle-ethereum",
        feature = "pending-oracle-mev-share"
    ))]
    if live_sources > 0 {
        let mut ready = 0_usize;
        while ready < live_sources {
            let streamed =
                tokio::time::timeout(std::time::Duration::from_secs(20), updates.recv()).await??;
            if matches!(
                streamed.event,
                PendingOracleEvent::SourceHealthChanged(ref health)
                    if health.state == evm_oracle_state::PendingOracleSourceState::Ready
            ) {
                ready += 1;
            }
        }
        println!("pending sources ready={ready}");
    }

    let pending = oracle.pending_updates().expect("pending channel enabled");

    // This offline fixture keeps the example deterministic. A live built-in
    // source calls the generic adapter router instead.
    let tx_hash = B256::repeat_byte(0x51);
    let update = PendingOracleUpdate::new(
        PendingOracleUpdateId::from_hash(B256::repeat_byte(0x41)),
        [FeedId::new("eth-usd")],
        PendingOracleEvidence::adapter(
            OracleAdapterId::new("example-adapter"),
            "signed-report",
            B256::repeat_byte(0x42),
        ),
    )
    .with_proposed_value(PendingOracleValue {
        raw_answer: I256::unchecked_from(2_150_000_000_i64),
        observed_at: 1_700_000_110,
    })
    .with_transmission(PendingOracleTransmission::new(
        PendingOracleTransmissionId::from_hash(tx_hash),
        PendingOracleSourceId::new("ethereum-public-mempool"),
        PendingOracleRoute::ChainlinkDirect {
            aggregator: AGGREGATOR,
            secondary: false,
        },
        PendingOracleOrderingHandle::TransactionHashOnly { tx_hash },
        PendingOracleSimulationMaterial::AnswerOnlyProjection,
        B256::repeat_byte(0x52),
        Some(20_000_000),
    ));
    pending.observe(update);

    loop {
        let streamed = updates.recv().await?;
        let PendingOracleEvent::Observed(update) = streamed.event else {
            continue;
        };
        let transmission = update
            .transmissions
            .iter()
            .filter(|candidate| candidate.is_referenceable())
            .max_by_key(|candidate| candidate.actionability_rank())
            .expect("referenceable transaction");
        println!(
            "simulate feeds={:?} proposed={:?} after_tx={:?} fidelity={:?}",
            update.feed_ids,
            update.proposed_value,
            transmission.ordering.transaction_hash(),
            transmission.simulation.fidelity(),
        );
        break;
    }

    // Pending observations never changed the authoritative snapshot.
    assert_eq!(
        oracle.latest_round("eth-usd")?.answer,
        I256::unchecked_from(2_000_000_000_i64)
    );
    Ok(())
}

#[derive(Clone, Debug)]
struct DemoProvider;

impl ChainlinkFeedProvider for DemoProvider {
    fn decimals(&self, proxy: Address) -> ProviderFuture<'_, u8> {
        assert_eq!(proxy, PROXY);
        Box::pin(async { Ok(8) })
    }

    fn description(&self, proxy: Address) -> ProviderFuture<'_, String> {
        assert_eq!(proxy, PROXY);
        Box::pin(async { Ok("ETH / USD".to_string()) })
    }

    fn version(&self, proxy: Address) -> ProviderFuture<'_, U256> {
        assert_eq!(proxy, PROXY);
        Box::pin(async { Ok(U256::from(4_u64)) })
    }

    fn latest_round_data(&self, proxy: Address) -> ProviderFuture<'_, RoundData> {
        assert_eq!(proxy, PROXY);
        Box::pin(async {
            Ok(RoundData {
                round_id: U256::from(100_u64),
                answer: I256::unchecked_from(2_000_000_000_i64),
                started_at: 1_700_000_000,
                updated_at: 1_700_000_000,
                answered_in_round: U256::from(100_u64),
            })
        })
    }

    fn aggregator(&self, proxy: Address) -> ProviderFuture<'_, Option<Address>> {
        assert_eq!(proxy, PROXY);
        Box::pin(async { Ok(Some(AGGREGATOR)) })
    }
}
