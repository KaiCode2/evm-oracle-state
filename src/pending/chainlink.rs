//! Pure Chainlink OCR2 pending-calldata recognition.

use alloy_primitives::{Address, B256, I256, U256, keccak256};

use super::{PendingOracleRoute, PendingOracleUpdateId};

/// Chainlink OCR2 `transmit` selector observed on current Ethereum aggregators.
pub const CHAINLINK_TRANSMIT_SELECTOR: [u8; 4] = [0xb1, 0xdc, 0x65, 0xa4];
/// Chainlink OCR2 `transmitSecondary` selector used by Smart Value Recapture feeds.
pub const CHAINLINK_TRANSMIT_SECONDARY_SELECTOR: [u8; 4] = [0xba, 0x0c, 0xb2, 0x9e];
/// Chainlink forwarder `forward(address,bytes)` selector.
pub const CHAINLINK_FORWARD_SELECTOR: [u8; 4] = [0x6f, 0xad, 0xcf, 0x72];

const MINIMUM_PLAUSIBLE_TIMESTAMP: u64 = 1_577_836_800;
const MAXIMUM_OBSERVATIONS: usize = 256;

/// Decoded report-level evidence shared by every transport variant carrying it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainlinkPendingReport {
    /// Aggregator that would receive the OCR2 report.
    pub aggregator: Address,
    /// Outer forwarder when the report is wrapped in `forward(address,bytes)`.
    pub forwarder: Option<Address>,
    /// Whether the `transmitSecondary` entry point is used.
    pub secondary: bool,
    /// Observation timestamp encoded by the report.
    pub observations_timestamp: u64,
    /// Signed oracle-node observations in report order.
    pub observations: Vec<I256>,
    /// Median observation selected by the OCR report.
    pub median_answer: I256,
    /// Lowest decoded observation.
    pub minimum_answer: I256,
    /// Highest decoded observation.
    pub maximum_answer: I256,
    /// Content hash of the raw OCR report bytes.
    pub report_hash: B256,
    /// Content hash of the complete outer calldata.
    pub calldata_hash: B256,
    /// Content hash of the inner `transmit` calldata.
    pub inner_calldata_hash: B256,
}

/// A recognized Chainlink OCR2 call plus its generic pending route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedChainlinkOcr2 {
    /// Generic outer and inner call path.
    pub route: PendingOracleRoute,
    /// Decoded report evidence.
    pub report: ChainlinkPendingReport,
}

impl DecodedChainlinkOcr2 {
    /// Derive report identity from the destination aggregator and report content.
    pub fn update_id(&self) -> PendingOracleUpdateId {
        let mut identity = Vec::with_capacity(52);
        identity.extend_from_slice(self.report.aggregator.as_slice());
        identity.extend_from_slice(self.report.report_hash.as_slice());
        PendingOracleUpdateId::from_hash(keccak256(identity))
    }
}

/// Failure to decode calldata whose selector identifies a supported Chainlink path.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PendingOracleDecodeError {
    /// A required ABI word or dynamic byte range was outside the payload.
    #[error("Chainlink pending calldata is ABI-truncated at {field}")]
    Truncated {
        /// Logical field whose bytes were unavailable.
        field: &'static str,
    },
    /// A dynamic ABI offset or length cannot fit this platform or payload.
    #[error("Chainlink pending calldata has an invalid ABI offset for {field}")]
    InvalidOffset {
        /// Logical field containing the invalid offset.
        field: &'static str,
    },
    /// A report timestamp was too old to plausibly represent supported OCR2 traffic.
    #[error("Chainlink report has implausible observations timestamp {timestamp}")]
    ImplausibleTimestamp {
        /// Decoded timestamp.
        timestamp: u64,
    },
    /// The report contained no observations or an implausibly large collection.
    #[error("Chainlink report has implausible observation count {count}")]
    ImplausibleObservationCount {
        /// Decoded observation count.
        count: usize,
    },
}

/// Decode supported direct or forwarded Chainlink OCR2 calldata.
///
/// Unknown selectors return `Ok(None)`. Once a supported outer or inner selector
/// is recognized, malformed ABI or report content returns a typed error rather
/// than being silently treated as unrelated traffic.
pub fn decode_chainlink_ocr2_calldata(
    to: Address,
    outer_calldata: &[u8],
) -> Result<Option<DecodedChainlinkOcr2>, PendingOracleDecodeError> {
    let Some(outer_selector) = outer_calldata.get(..4) else {
        return Ok(None);
    };

    let (aggregator, forwarder, inner_calldata) = if outer_selector == CHAINLINK_FORWARD_SELECTOR {
        let args = &outer_calldata[4..];
        let target = abi_word(args, 0, "forward target")?;
        let aggregator = Address::from_slice(&target[12..]);
        let inner = dynamic_bytes(args, 32, "forward calldata")?;
        (aggregator, Some(to), inner)
    } else {
        (to, None, outer_calldata)
    };

    let selector = inner_calldata
        .get(..4)
        .ok_or(PendingOracleDecodeError::Truncated {
            field: "transmit selector",
        })?;
    let secondary = if selector == CHAINLINK_TRANSMIT_SELECTOR {
        false
    } else if selector == CHAINLINK_TRANSMIT_SECONDARY_SELECTOR {
        true
    } else {
        return Ok(None);
    };

    let transmit_args = &inner_calldata[4..];
    let report_bytes = dynamic_bytes(transmit_args, 3 * 32, "OCR report")?;
    let (observations_timestamp, observations) = decode_report(report_bytes)?;
    let mut ordered = observations.clone();
    ordered.sort_unstable();
    let median_answer = ordered[ordered.len() / 2];
    let minimum_answer = ordered[0];
    let maximum_answer = ordered[ordered.len() - 1];
    let route = match forwarder {
        Some(forwarder) => PendingOracleRoute::ChainlinkForwarded {
            forwarder,
            aggregator,
            secondary,
        },
        None => PendingOracleRoute::ChainlinkDirect {
            aggregator,
            secondary,
        },
    };

    Ok(Some(DecodedChainlinkOcr2 {
        route,
        report: ChainlinkPendingReport {
            aggregator,
            forwarder,
            secondary,
            observations_timestamp,
            observations,
            median_answer,
            minimum_answer,
            maximum_answer,
            report_hash: keccak256(report_bytes),
            calldata_hash: keccak256(outer_calldata),
            inner_calldata_hash: keccak256(inner_calldata),
        },
    }))
}

fn decode_report(report: &[u8]) -> Result<(u64, Vec<I256>), PendingOracleDecodeError> {
    let timestamp = abi_u64(report, 0, "observations timestamp")?;
    if timestamp < MINIMUM_PLAUSIBLE_TIMESTAMP {
        return Err(PendingOracleDecodeError::ImplausibleTimestamp { timestamp });
    }
    let observations_offset = abi_usize(report, 64, "observations offset")?;
    let count = abi_usize(report, observations_offset, "observation count")?;
    if count == 0 || count > MAXIMUM_OBSERVATIONS {
        return Err(PendingOracleDecodeError::ImplausibleObservationCount { count });
    }

    let mut observations = Vec::with_capacity(count);
    for index in 0..count {
        let offset = observations_offset
            .checked_add(32)
            .and_then(|offset| offset.checked_add(index * 32))
            .ok_or(PendingOracleDecodeError::InvalidOffset {
                field: "observation",
            })?;
        let value = U256::from_be_slice(abi_word(report, offset, "observation")?);
        observations.push(I256::from_raw(value));
    }
    Ok((timestamp, observations))
}

fn dynamic_bytes<'a>(
    args: &'a [u8],
    offset_position: usize,
    field: &'static str,
) -> Result<&'a [u8], PendingOracleDecodeError> {
    let offset = abi_usize(args, offset_position, field)?;
    let length = abi_usize(args, offset, field)?;
    let start = offset
        .checked_add(32)
        .ok_or(PendingOracleDecodeError::InvalidOffset { field })?;
    let end = start
        .checked_add(length)
        .ok_or(PendingOracleDecodeError::InvalidOffset { field })?;
    args.get(start..end)
        .ok_or(PendingOracleDecodeError::Truncated { field })
}

fn abi_u64(
    data: &[u8],
    offset: usize,
    field: &'static str,
) -> Result<u64, PendingOracleDecodeError> {
    let word = abi_word(data, offset, field)?;
    if word[..24].iter().any(|byte| *byte != 0) {
        return Err(PendingOracleDecodeError::InvalidOffset { field });
    }
    Ok(u64::from_be_bytes(
        word[24..].try_into().expect("eight bytes"),
    ))
}

fn abi_usize(
    data: &[u8],
    offset: usize,
    field: &'static str,
) -> Result<usize, PendingOracleDecodeError> {
    let value = abi_u64(data, offset, field)?;
    usize::try_from(value).map_err(|_| PendingOracleDecodeError::InvalidOffset { field })
}

fn abi_word<'a>(
    data: &'a [u8],
    offset: usize,
    field: &'static str,
) -> Result<&'a [u8], PendingOracleDecodeError> {
    let end = offset
        .checked_add(32)
        .ok_or(PendingOracleDecodeError::InvalidOffset { field })?;
    data.get(offset..end)
        .ok_or(PendingOracleDecodeError::Truncated { field })
}
