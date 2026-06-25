use alloy_primitives::{Address, Bytes, U256, Uint};
use alloy_sol_types::{SolCall, sol};

use crate::{FeedStatus, OracleError, OracleSnapshot, OracleSnapshotStatus, OracleTracker};

sol! {
    interface AggregatorV3Interface {
        function latestRoundData() external view returns (
            uint80 roundId,
            int256 answer,
            uint256 startedAt,
            uint256 updatedAt,
            uint80 answeredInRound
        );
        function decimals() external view returns (uint8);
        function description() external view returns (string);
        function version() external view returns (uint256);
    }
}

use AggregatorV3Interface::{decimalsCall, descriptionCall, latestRoundDataCall, versionCall};

/// Simulation-facing ABI call overlay backed by typed oracle state.
pub struct OracleReadOverlay<'a> {
    tracker: &'a OracleTracker,
}

impl<'a> OracleReadOverlay<'a> {
    /// Create a new read overlay over the current tracker state.
    pub fn new(tracker: &'a OracleTracker) -> Self {
        Self { tracker }
    }

    /// Try to serve a Chainlink proxy view call from typed oracle state.
    pub fn try_call(&self, target: Address, calldata: &[u8]) -> Result<Option<Bytes>, OracleError> {
        let Some(snapshot) = self.tracker.latest(target) else {
            return Ok(None);
        };
        if !is_actionable(snapshot) || calldata.len() != 4 {
            return Ok(None);
        }

        if calldata == latestRoundDataCall::SELECTOR {
            let round_id = u256_to_uint80(snapshot.round.round_id, "round_id")?;
            let answered_in_round =
                u256_to_uint80(snapshot.round.answered_in_round, "answered_in_round")?;
            return Ok(Some(Bytes::from(
                latestRoundDataCall::abi_encode_returns_tuple(&(
                    round_id,
                    snapshot.round.answer,
                    U256::from(snapshot.round.started_at),
                    U256::from(snapshot.round.updated_at),
                    answered_in_round,
                )),
            )));
        }

        if calldata == decimalsCall::SELECTOR {
            return Ok(Some(Bytes::from(decimalsCall::abi_encode_returns(
                &snapshot.metadata.decimals,
            ))));
        }

        if calldata == descriptionCall::SELECTOR {
            return Ok(Some(Bytes::from(descriptionCall::abi_encode_returns(
                &snapshot.metadata.description,
            ))));
        }

        if calldata == versionCall::SELECTOR {
            return Ok(Some(Bytes::from(versionCall::abi_encode_returns(
                &snapshot.metadata.version,
            ))));
        }

        Ok(None)
    }
}

fn is_actionable(snapshot: &OracleSnapshot) -> bool {
    snapshot.status == FeedStatus::Fresh
        && matches!(
            snapshot.confidence,
            OracleSnapshotStatus::EventPending
                | OracleSnapshotStatus::Confirmed
                | OracleSnapshotStatus::Corrected
        )
}

fn u256_to_uint80(value: U256, field: &'static str) -> Result<Uint<80, 2>, OracleError> {
    let value = u128::try_from(value)
        .map_err(|_| OracleError::Provider(format!("{field} does not fit uint80")))?;
    if value >= (1_u128 << 80) {
        return Err(OracleError::Provider(format!(
            "{field} does not fit uint80"
        )));
    }
    Uint::<80, 2>::try_from(value)
        .map_err(|_| OracleError::Provider(format!("{field} does not fit uint80")))
}
