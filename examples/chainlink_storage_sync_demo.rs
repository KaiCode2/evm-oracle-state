use std::{collections::BTreeMap, error::Error};

use alloy_primitives::{Address, I256, U256};
use evm_fork_cache::{StateUpdate, StateView};
use evm_oracle_state::{
    AggregatorLayoutEvidence, ChainlinkOcr1StorageAdapter, ChainlinkOcr2StorageAdapter, FeedId,
    FeedMetadata, FeedRegistration, FeedSource, Ocr1TransmissionStorageUpdate,
    Ocr2TransmissionStorageUpdate, OracleStorageEffect, OracleStorageSync, StalenessPolicy,
};

const PROXY: Address = Address::repeat_byte(0x10);
const OCR2_AGGREGATOR: Address = Address::repeat_byte(0x22);
const OCR1_AGGREGATOR: Address = Address::repeat_byte(0x11);

fn main() -> Result<(), Box<dyn Error>> {
    let sync = OracleStorageSync::chainlink_defaults();

    println!("1. OCR2 direct storage sync");
    let ocr2 = registration(
        OCR2_AGGREGATOR,
        "AccessControlledOCR2Aggregator 1.0.0",
        "eth-usd-ocr2",
    );
    let hot_ocr2_state = MapStateView::default().with_slot(
        OCR2_AGGREGATOR,
        ChainlinkOcr2StorageAdapter::hot_vars_slot(),
        U256::from(0xaaaa_u64),
    );
    let ocr2_update = Ocr2TransmissionStorageUpdate {
        aggregator: OCR2_AGGREGATOR,
        aggregator_round_id: U256::from(101_u64),
        answer: I256::unchecked_from(2_150_000_000_i64),
        observations_timestamp: 1_700_000_040,
        transmission_timestamp: 1_700_000_120,
        epoch_and_round: 0x1a_89_04,
    };
    print_effect(sync.state_effect_for_ocr2_transmission(&ocr2, &ocr2_update, &hot_ocr2_state)?);
    println!("   latestRoundData startedAt comes from observationsTimestamp");
    println!("   latestRoundData updatedAt comes from the transmission block timestamp");

    println!("\n2. OCR1 direct storage sync");
    let ocr1 = registration(
        OCR1_AGGREGATOR,
        "AccessControlledOffchainAggregator 3.0.0",
        "eth-usd-ocr1",
    );
    let hot_ocr1_state = MapStateView::default().with_slot(
        OCR1_AGGREGATOR,
        ChainlinkOcr1StorageAdapter::hot_vars_slot(),
        U256::from(0xbbbb_u64),
    );
    let ocr1_update = Ocr1TransmissionStorageUpdate {
        aggregator: OCR1_AGGREGATOR,
        aggregator_round_id: U256::from(202_u64),
        answer: I256::unchecked_from(2_151_000_000_i64),
        transmission_timestamp: 1_700_000_240,
        epoch_and_round: 0x1b_00_01,
    };
    print_effect(sync.state_effect_for_ocr1_transmission(&ocr1, &ocr1_update, &hot_ocr1_state)?);
    println!("   OCR1 NewTransmission has one timestamp, so startedAt and updatedAt match");

    println!("\n3. conservative fallback when the hot-vars slot is cold");
    print_effect(sync.state_effect_for_ocr2_transmission(
        &ocr2,
        &ocr2_update,
        &MapStateView::default(),
    )?);

    Ok(())
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

fn registration(aggregator: Address, type_and_version: &str, id: &str) -> FeedRegistration {
    FeedRegistration {
        id: FeedId::new(id),
        proxy: PROXY,
        label: Some(id.to_string()),
        base: Some("ETH".to_string()),
        quote: Some("USD".to_string()),
        staleness: StalenessPolicy::max_age(3_600),
        current_aggregator: Some(aggregator),
        aggregator_layout: Some(AggregatorLayoutEvidence::from_type_and_version(
            aggregator,
            type_and_version,
            None,
        )),
        metadata: FeedMetadata {
            decimals: 8,
            description: "ETH / USD".to_string(),
            version: U256::from(4_u64),
        },
        source: FeedSource::Chainlink,
    }
}

fn print_effect(effect: OracleStorageEffect) {
    match effect {
        OracleStorageEffect::StateUpdates { adapter, updates } => {
            println!(
                "   adapter={adapter} emitted {} direct cache write(s)",
                updates.len()
            );
            for update in updates {
                print_state_update(&update);
            }
        }
        OracleStorageEffect::FallbackPurge => {
            println!("   no verified direct write; fallback is purge/refetch");
        }
    }
}

fn print_state_update(update: &StateUpdate) {
    match update {
        StateUpdate::Slot {
            address,
            slot,
            value,
        } => println!("   slot write address={address:?} slot={slot} value={value}"),
        StateUpdate::SlotMasked {
            address,
            slot,
            mask,
            value,
        } => {
            println!("   masked write address={address:?} slot={slot} mask={mask} value={value}")
        }
        other => println!("   state update {other:?}"),
    }
}
