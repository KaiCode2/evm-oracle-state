use std::collections::BTreeMap;

use alloy_primitives::{Address, B256, Bytes, I256, U256, Uint};
use alloy_sol_types::{SolCall, sol};

use crate::{
    FeedRegistration, FeedSource, OracleError, OracleRoundStatus, OracleSnapshot, OracleTracker,
    OracleValueStatus,
};

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

    interface MorphoOracleInterface {
        function price() external view returns (uint256);
    }

    interface EulerPriceOracleInterface {
        function getQuote(uint256 inAmount, address base, address quote) external view returns (uint256 outAmount);
        function getQuotes(uint256 inAmount, address base, address quote) external view returns (uint256 bidOutAmount, uint256 askOutAmount);
    }

    struct PythPrice {
        int64 price;
        uint64 conf;
        int32 expo;
        uint256 publishTime;
    }

    interface PythInterface {
        function getPriceUnsafe(bytes32 id) external view returns (PythPrice memory price);
        function getPriceNoOlderThan(bytes32 id, uint256 age) external view returns (PythPrice memory price);
    }
}

use AggregatorV3Interface::{decimalsCall, descriptionCall, latestRoundDataCall, versionCall};
use EulerPriceOracleInterface::{getQuoteCall, getQuotesCall, getQuotesReturn};
use MorphoOracleInterface::priceCall;
use PythInterface::{getPriceNoOlderThanCall, getPriceUnsafeCall};

/// Simulation-facing ABI call overlay backed by typed oracle state.
///
/// # Fast-path semantics
///
/// The overlay intentionally serves values whose lifecycle is still
/// [`OracleValueStatus::EventPending`]: its purpose is to let a simulation
/// observe a decoded oracle event *before* the follow-up authoritative proxy
/// read completes. This is a deliberate exception to the crate's
/// "proxy reads are authoritative" contract, scoped to registered view calls:
///
/// - Values classified `RequiresRepair` or `Unknown`, and rounds that are not
///   [`OracleRoundStatus::Fresh`], are never served — the overlay returns
///   `Ok(None)` and the caller falls back to the raw cache/RPC read, which the
///   reactive handler keeps conservatively purged.
/// - Decision-making callers should keep promoting typed prices through
///   [`crate::PricePolicy`], which rejects `EventPending` values unless the
///   caller opted in via `allow_event_pending()`.
///
/// Consequently a raw `try_call` user can observe an event-derived value at
/// the same moment the underlying cache storage was purged for refetch; the
/// two read paths reconverge after reconciliation.
pub struct OracleReadOverlay<'a> {
    tracker: &'a OracleTracker,
    pyth_feeds: BTreeMap<(Address, B256), PythOverlayFeed>,
    // Euler feeds are registered under synthetic per-feed state keys, so
    // getQuote/getQuotes calls addressed to the real user-facing contract
    // (router or direct adapter) route through this index. Multiple pairs can
    // share one serve address; dispatch matches the calldata base/quote.
    euler_feeds: BTreeMap<Address, Vec<EulerOverlayFeed>>,
}

#[derive(Clone, Copy, Debug)]
struct PythOverlayFeed {
    proxy: Address,
    expo: i32,
    conf: u64,
}

#[derive(Clone, Copy, Debug)]
struct EulerOverlayFeed {
    proxy: Address,
    base: Address,
    quote: Address,
}

impl<'a> OracleReadOverlay<'a> {
    /// Create a new read overlay over the current tracker state.
    pub fn new(tracker: &'a OracleTracker) -> Self {
        let mut pyth_feeds = BTreeMap::new();
        let mut euler_feeds: BTreeMap<Address, Vec<EulerOverlayFeed>> = BTreeMap::new();
        for registration in tracker.registrations_iter() {
            match &registration.source {
                FeedSource::EulerQuote {
                    source,
                    base,
                    quote,
                    ..
                }
                | FeedSource::EulerCross {
                    source,
                    base,
                    quote,
                    ..
                } => {
                    euler_feeds
                        .entry(*source)
                        .or_default()
                        .push(EulerOverlayFeed {
                            proxy: registration.proxy,
                            base: *base,
                            quote: *quote,
                        });
                    continue;
                }
                _ => {}
            }
            let Some((pyth, price_id, expo, conf)) = registration.source.pyth_source() else {
                continue;
            };
            pyth_feeds
                .entry((pyth, price_id))
                .or_insert(PythOverlayFeed {
                    proxy: registration.proxy,
                    expo,
                    conf,
                });
        }
        Self {
            tracker,
            pyth_feeds,
            euler_feeds,
        }
    }

    /// Try to serve a Chainlink proxy view call from typed oracle state.
    pub fn try_call(&self, target: Address, calldata: &[u8]) -> Result<Option<Bytes>, OracleError> {
        if let Some(bytes) = self.try_pyth_call(target, calldata)? {
            return Ok(Some(bytes));
        }
        if let Some(bytes) = self.try_euler_target_call(target, calldata)? {
            return Ok(Some(bytes));
        }

        let Some(snapshot) = self.tracker.latest(target) else {
            return Ok(None);
        };
        let Some(registration) = self.tracker.registration_for_proxy(target) else {
            return Ok(None);
        };
        if !is_actionable(snapshot) || calldata.len() != 4 {
            if is_actionable(snapshot)
                && let Some(bytes) = try_euler_call(registration, snapshot, calldata)?
            {
                return Ok(Some(bytes));
            }
            return Ok(None);
        }

        if calldata == priceCall::SELECTOR
            && matches!(registration.source, FeedSource::MorphoChainlinkV2 { .. })
        {
            return Ok(Some(Bytes::from(priceCall::abi_encode_returns(
                &i256_to_u256(snapshot.round.answer, "price")?,
            ))));
        }

        if !supports_chainlink_overlay(&registration.source) {
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

    fn try_pyth_call(
        &self,
        target: Address,
        calldata: &[u8],
    ) -> Result<Option<Bytes>, OracleError> {
        if calldata.len() < 4 {
            return Ok(None);
        }

        let price_id = if calldata[..4] == getPriceUnsafeCall::SELECTOR {
            getPriceUnsafeCall::abi_decode(calldata)
                .map_err(|error| {
                    OracleError::Config(crate::error::OracleConfigError::InvalidRequest(format!(
                        "decode Pyth getPriceUnsafe failed: {error}"
                    )))
                })?
                .id
        } else if calldata[..4] == getPriceNoOlderThanCall::SELECTOR {
            getPriceNoOlderThanCall::abi_decode(calldata)
                .map_err(|error| {
                    OracleError::Config(crate::error::OracleConfigError::InvalidRequest(format!(
                        "decode Pyth getPriceNoOlderThan failed: {error}"
                    )))
                })?
                .id
        } else {
            return Ok(None);
        };

        let Some((snapshot, expo, conf)) = self.pyth_registration_for_call(target, price_id) else {
            return Ok(None);
        };
        if !is_actionable(snapshot) {
            return Ok(None);
        }

        let price = pyth_abi_price(snapshot.round.answer, expo)?;
        let publish_time = U256::from(snapshot.round.updated_at);
        let pyth_price = PythPrice {
            price,
            conf,
            expo,
            publishTime: publish_time,
        };

        let bytes = if calldata[..4] == getPriceUnsafeCall::SELECTOR {
            getPriceUnsafeCall::abi_encode_returns(&pyth_price)
        } else {
            getPriceNoOlderThanCall::abi_encode_returns(&pyth_price)
        };
        Ok(Some(Bytes::from(bytes)))
    }

    fn pyth_registration_for_call(
        &self,
        target: Address,
        price_id: B256,
    ) -> Option<(&OracleSnapshot, i32, u64)> {
        let feed = self.pyth_feeds.get(&(target, price_id))?;
        let snapshot = self.tracker.latest(feed.proxy)?;
        Some((snapshot, feed.expo, feed.conf))
    }

    /// Serve Euler `getQuote`/`getQuotes` calls addressed to a real
    /// user-facing quote contract (router or direct adapter), dispatching to
    /// the registered pair whose base/quote matches the calldata. Exact
    /// direction matches win over inverse matches so a router carrying both
    /// directions of one pair stays unambiguous.
    fn try_euler_target_call(
        &self,
        target: Address,
        calldata: &[u8],
    ) -> Result<Option<Bytes>, OracleError> {
        let Some(candidates) = self.euler_feeds.get(&target) else {
            return Ok(None);
        };
        let (given_base, given_quote) =
            if calldata.len() >= 4 && calldata[..4] == getQuoteCall::SELECTOR {
                let call = getQuoteCall::abi_decode(calldata).map_err(|error| {
                    OracleError::Config(crate::error::OracleConfigError::InvalidRequest(format!(
                        "decode Euler getQuote failed: {error}"
                    )))
                })?;
                (call.base, call.quote)
            } else if calldata.len() >= 4 && calldata[..4] == getQuotesCall::SELECTOR {
                let call = getQuotesCall::abi_decode(calldata).map_err(|error| {
                    OracleError::Config(crate::error::OracleConfigError::InvalidRequest(format!(
                        "decode Euler getQuotes failed: {error}"
                    )))
                })?;
                (call.base, call.quote)
            } else {
                return Ok(None);
            };
        let feed = candidates
            .iter()
            .find(|feed| feed.base == given_base && feed.quote == given_quote)
            .or_else(|| {
                candidates
                    .iter()
                    .find(|feed| feed.base == given_quote && feed.quote == given_base)
            });
        let Some(feed) = feed else {
            return Err(OracleError::Config(
                crate::error::OracleConfigError::InvalidRequest(
                    "Euler overlay request does not match registered base/quote".to_string(),
                ),
            ));
        };
        let Some(registration) = self.tracker.registration_for_proxy(feed.proxy) else {
            return Ok(None);
        };
        let Some(snapshot) = self.tracker.latest(feed.proxy) else {
            return Ok(None);
        };
        if !is_actionable(snapshot) {
            return Ok(None);
        }
        try_euler_call(registration, snapshot, calldata)
    }
}

fn supports_chainlink_overlay(source: &FeedSource) -> bool {
    !matches!(
        source,
        FeedSource::MorphoChainlinkV2 { .. }
            | FeedSource::EulerQuote { .. }
            | FeedSource::EulerCross { .. }
            | FeedSource::Pyth { .. }
    )
}

fn try_euler_call(
    registration: &FeedRegistration,
    snapshot: &OracleSnapshot,
    calldata: &[u8],
) -> Result<Option<Bytes>, OracleError> {
    let (base, quote, base_decimals, _quote_decimals) = match &registration.source {
        FeedSource::EulerQuote {
            base,
            quote,
            base_decimals,
            quote_decimals,
            ..
        }
        | FeedSource::EulerCross {
            base,
            quote,
            base_decimals,
            quote_decimals,
            ..
        } => (*base, *quote, *base_decimals, *quote_decimals),
        _ => return Ok(None),
    };

    if calldata.len() >= 4 && calldata[..4] == getQuoteCall::SELECTOR {
        let call = getQuoteCall::abi_decode(calldata).map_err(|error| {
            OracleError::Config(crate::error::OracleConfigError::InvalidRequest(format!(
                "decode Euler getQuote failed: {error}"
            )))
        })?;
        let out = euler_quote_amount(
            call.inAmount,
            call.base,
            call.quote,
            base,
            quote,
            base_decimals,
            snapshot.round.answer,
        )?;
        return Ok(Some(Bytes::from(getQuoteCall::abi_encode_returns(&out))));
    }

    if calldata.len() >= 4 && calldata[..4] == getQuotesCall::SELECTOR {
        let call = getQuotesCall::abi_decode(calldata).map_err(|error| {
            OracleError::Config(crate::error::OracleConfigError::InvalidRequest(format!(
                "decode Euler getQuotes failed: {error}"
            )))
        })?;
        let out = euler_quote_amount(
            call.inAmount,
            call.base,
            call.quote,
            base,
            quote,
            base_decimals,
            snapshot.round.answer,
        )?;
        return Ok(Some(Bytes::from(getQuotesCall::abi_encode_returns(
            &getQuotesReturn {
                bidOutAmount: out,
                askOutAmount: out,
            },
        ))));
    }

    Ok(None)
}

fn euler_quote_amount(
    in_amount: U256,
    given_base: Address,
    given_quote: Address,
    base: Address,
    quote: Address,
    base_decimals: u8,
    raw_price: I256,
) -> Result<U256, OracleError> {
    let price = i256_to_u256(raw_price, "Euler price")?;
    if given_base == base && given_quote == quote {
        // out = in * price / 10^base_decimals. Caller-supplied `inAmount` is
        // unbounded, so overflow must error instead of saturating into a
        // silently capped quote.
        let scaled = in_amount.checked_mul(price).ok_or_else(|| {
            OracleError::Unsupported("Euler quote multiplication overflowed".to_string())
        })?;
        return scaled
            .checked_div(decimal_scale_u256(base_decimals))
            .ok_or_else(|| {
                OracleError::Unsupported("Euler quote scale division failed".to_string())
            });
    }
    if given_base == quote && given_quote == base {
        if price.is_zero() {
            return Err(OracleError::Unsupported(
                "cannot invert a zero Euler price".to_string(),
            ));
        }
        let scaled = in_amount
            .checked_mul(decimal_scale_u256(base_decimals))
            .ok_or_else(|| {
                OracleError::Unsupported(
                    "Euler inverse quote multiplication overflowed".to_string(),
                )
            })?;
        return scaled.checked_div(price).ok_or_else(|| {
            OracleError::Unsupported("Euler inverse quote division failed".to_string())
        });
    }
    Err(OracleError::Config(
        crate::error::OracleConfigError::InvalidRequest(
            "Euler overlay request does not match registered base/quote".to_string(),
        ),
    ))
}

fn is_actionable(snapshot: &OracleSnapshot) -> bool {
    snapshot.round_status == OracleRoundStatus::Fresh
        && matches!(
            snapshot.value_status,
            OracleValueStatus::EventPending
                | OracleValueStatus::Confirmed
                | OracleValueStatus::Corrected
        )
}

fn u256_to_uint80(value: U256, field: &'static str) -> Result<Uint<80, 2>, OracleError> {
    let value = u128::try_from(value)
        .map_err(|_| OracleError::Unsupported(format!("{field} does not fit uint80")))?;
    if value >= (1_u128 << 80) {
        return Err(OracleError::Unsupported(format!(
            "{field} does not fit uint80"
        )));
    }
    Uint::<80, 2>::try_from(value)
        .map_err(|_| OracleError::Unsupported(format!("{field} does not fit uint80")))
}

fn i256_to_u256(value: I256, field: &'static str) -> Result<U256, OracleError> {
    U256::try_from(value)
        .map_err(|_| OracleError::Unsupported(format!("{field} is negative or too large")))
}

fn i256_to_i64(value: I256, field: &'static str) -> Result<i64, OracleError> {
    i128::try_from(value)
        .ok()
        .and_then(|value| i64::try_from(value).ok())
        .ok_or_else(|| OracleError::Unsupported(format!("{field} does not fit int64")))
}

fn pyth_abi_price(raw_answer: I256, expo: i32) -> Result<i64, OracleError> {
    let price = if expo > 0 {
        let scale = I256::unchecked_from(10_i8)
            .checked_pow(U256::from(expo as u32))
            .ok_or_else(|| {
                OracleError::Unsupported(format!("Pyth expo {expo} scale overflowed"))
            })?;
        raw_answer
            .checked_div(scale)
            .ok_or_else(|| OracleError::Unsupported("Pyth price unscale failed".to_string()))?
    } else {
        raw_answer
    };
    i256_to_i64(price, "Pyth price")
}

fn decimal_scale_u256(decimals: u8) -> U256 {
    let mut scale = U256::from(1_u8);
    for _ in 0..decimals {
        scale = scale.saturating_mul(U256::from(10_u8));
    }
    scale
}
