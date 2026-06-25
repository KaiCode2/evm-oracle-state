use alloy_primitives::{Address, B256, Bytes, I256, Log as PrimitiveLog, U256, keccak256};
use alloy_rpc_types_eth::Log as RpcLog;
use evm_oracle_state::{
    ANSWER_UPDATED_TOPIC, AnswerUpdated, ChainlinkEventDecodeError, NEW_ROUND_TOPIC, NewRound,
    decode_answer_updated, decode_new_round,
};

fn rpc_log(address: Address, topics: Vec<B256>, data: Vec<u8>) -> RpcLog {
    RpcLog {
        inner: PrimitiveLog::new_unchecked(address, topics, Bytes::from(data)),
        block_hash: Some(B256::repeat_byte(0x01)),
        block_number: Some(100),
        block_timestamp: Some(1_700_000_000),
        transaction_hash: Some(B256::repeat_byte(0x02)),
        transaction_index: Some(0),
        log_index: Some(3),
        removed: false,
    }
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

fn address_topic(address: Address) -> B256 {
    let mut bytes = [0u8; 32];
    bytes[12..].copy_from_slice(address.as_slice());
    B256::from(bytes)
}

#[test]
fn exposes_chainlink_event_topics() {
    assert_eq!(
        ANSWER_UPDATED_TOPIC,
        keccak256("AnswerUpdated(int256,uint256,uint256)")
    );
    assert_eq!(
        NEW_ROUND_TOPIC,
        keccak256("NewRound(uint256,address,uint256)")
    );
}

#[test]
fn decodes_answer_updated_with_signed_current_answer() {
    let aggregator = Address::repeat_byte(0x11);
    let current = I256::unchecked_from(-42);
    let round_id = U256::from(9_001_u64);
    let updated_at = U256::from(1_700_000_123_u64);
    let log = rpc_log(
        aggregator,
        vec![
            ANSWER_UPDATED_TOPIC,
            int_topic(current),
            uint_topic(round_id),
        ],
        word(updated_at),
    );

    let decoded = decode_answer_updated(&log).expect("valid AnswerUpdated log should decode");

    assert_eq!(
        decoded,
        AnswerUpdated {
            aggregator,
            current,
            round_id,
            updated_at: 1_700_000_123,
            block_number: Some(100),
            log_index: Some(3),
            removed: false,
        }
    );
}

#[test]
fn decodes_new_round_for_observability() {
    let aggregator = Address::repeat_byte(0x22);
    let started_by = Address::repeat_byte(0x33);
    let round_id = U256::from(77_u64);
    let started_at = U256::from(1_700_000_456_u64);
    let log = rpc_log(
        aggregator,
        vec![
            NEW_ROUND_TOPIC,
            uint_topic(round_id),
            address_topic(started_by),
        ],
        word(started_at),
    );

    let decoded = decode_new_round(&log).expect("valid NewRound log should decode");

    assert_eq!(
        decoded,
        NewRound {
            aggregator,
            round_id,
            started_by,
            started_at: 1_700_000_456,
            block_number: Some(100),
            log_index: Some(3),
            removed: false,
        }
    );
}

#[test]
fn malformed_answer_updated_returns_typed_decode_error() {
    let log = rpc_log(
        Address::repeat_byte(0x44),
        vec![ANSWER_UPDATED_TOPIC, int_topic(I256::unchecked_from(1))],
        Vec::new(),
    );

    let err = decode_answer_updated(&log).expect_err("missing topic/data should fail");

    assert!(matches!(
        err,
        ChainlinkEventDecodeError::WrongTopicCount { .. }
            | ChainlinkEventDecodeError::WrongDataLength { .. }
    ));
}
