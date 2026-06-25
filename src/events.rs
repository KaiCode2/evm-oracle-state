use alloy_primitives::{Address, B256, I256, U256, b256};
use alloy_rpc_types_eth::Log as RpcLog;

use crate::ChainlinkEventDecodeError;

/// `AnswerUpdated(int256,uint256,uint256)` topic0.
pub const ANSWER_UPDATED_TOPIC: B256 =
    b256!("0559884fd3a460db3073b7fc896cc77986f16e378210ded43186175bf646fc5f");

/// `NewRound(uint256,address,uint256)` topic0.
pub const NEW_ROUND_TOPIC: B256 =
    b256!("0109fc6f55cf40689f02fbaad7af7fe7bbac8a3d2186600afc7d3e10cac60271");

/// Decoded Chainlink `AnswerUpdated` event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnswerUpdated {
    /// Emitting aggregator address.
    pub aggregator: Address,
    /// Signed answer from the event.
    pub current: I256,
    /// Aggregator-local round id.
    pub round_id: U256,
    /// Event `updatedAt` timestamp.
    pub updated_at: u64,
    /// Log block number, when present.
    pub block_number: Option<u64>,
    /// Log index, when present.
    pub log_index: Option<u64>,
    /// Whether the provider marked the log as removed.
    pub removed: bool,
}

/// Decoded Chainlink `NewRound` event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewRound {
    /// Emitting aggregator address.
    pub aggregator: Address,
    /// Aggregator-local round id.
    pub round_id: U256,
    /// Address that started the round.
    pub started_by: Address,
    /// Event `startedAt` timestamp.
    pub started_at: u64,
    /// Log block number, when present.
    pub block_number: Option<u64>,
    /// Log index, when present.
    pub log_index: Option<u64>,
    /// Whether the provider marked the log as removed.
    pub removed: bool,
}

/// Decode a Chainlink `AnswerUpdated` log.
pub fn decode_answer_updated(log: &RpcLog) -> Result<AnswerUpdated, ChainlinkEventDecodeError> {
    require_topic(log, ANSWER_UPDATED_TOPIC)?;
    require_topic_count(log, 3)?;
    let data = data_word(log, "updated_at")?;
    Ok(AnswerUpdated {
        aggregator: log.address(),
        current: topic_i256(log.topics()[1]),
        round_id: topic_u256(log.topics()[2]),
        updated_at: u256_to_u64(data, "updated_at")?,
        block_number: log.block_number,
        log_index: log.log_index,
        removed: log.removed,
    })
}

/// Decode a Chainlink `NewRound` log.
pub fn decode_new_round(log: &RpcLog) -> Result<NewRound, ChainlinkEventDecodeError> {
    require_topic(log, NEW_ROUND_TOPIC)?;
    require_topic_count(log, 3)?;
    let data = data_word(log, "started_at")?;
    Ok(NewRound {
        aggregator: log.address(),
        round_id: topic_u256(log.topics()[1]),
        started_by: topic_address(log.topics()[2]),
        started_at: u256_to_u64(data, "started_at")?,
        block_number: log.block_number,
        log_index: log.log_index,
        removed: log.removed,
    })
}

fn require_topic(log: &RpcLog, expected: B256) -> Result<(), ChainlinkEventDecodeError> {
    let actual = log.topics().first().copied();
    if actual == Some(expected) {
        Ok(())
    } else {
        Err(ChainlinkEventDecodeError::WrongTopic { expected, actual })
    }
}

fn require_topic_count(log: &RpcLog, expected: usize) -> Result<(), ChainlinkEventDecodeError> {
    let actual = log.topics().len();
    if actual == expected {
        Ok(())
    } else {
        Err(ChainlinkEventDecodeError::WrongTopicCount { expected, actual })
    }
}

fn data_word(log: &RpcLog, field: &'static str) -> Result<U256, ChainlinkEventDecodeError> {
    let data = log.inner.data.data.as_ref();
    if data.len() != 32 {
        return Err(ChainlinkEventDecodeError::WrongDataLength {
            expected: 32,
            actual: data.len(),
        });
    }

    let _ = field;
    Ok(U256::from_be_slice(data))
}

fn topic_u256(topic: B256) -> U256 {
    U256::from_be_slice(topic.as_slice())
}

fn topic_i256(topic: B256) -> I256 {
    I256::from_raw(topic_u256(topic))
}

fn topic_address(topic: B256) -> Address {
    Address::from_slice(&topic.as_slice()[12..])
}

fn u256_to_u64(value: U256, field: &'static str) -> Result<u64, ChainlinkEventDecodeError> {
    u64::try_from(value).map_err(|_| ChainlinkEventDecodeError::Uint64Overflow { field, value })
}
