use alloy_primitives::{Address, I256, U256};
use thiserror::Error;

use crate::{
    AssetId, Denomination, FeedRegistration, OracleError, OracleRoundStatus, OracleSnapshot,
    OracleValueSource, OracleValueStatus, RoundData, TokenAmount, ValuedAmount,
};

/// Structured reason a [`PricePolicy`] check or valuation was rejected.
///
/// Carried by [`OracleError::Policy`]. Each variant names one distinct
/// policy or valuation failure; the enum is `#[non_exhaustive]` because new
/// checks may be added.
#[non_exhaustive]
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum PricePolicyViolation {
    /// The price does not carry a base asset symbol.
    #[error("price base asset is unknown")]
    MissingBase,
    /// The price does not carry a quote denomination.
    #[error("price quote denomination is unknown")]
    MissingQuote,
    /// The price's quote denomination does not match the quote required by
    /// the policy.
    #[error("price quote {actual} does not match required quote {required}")]
    QuoteMismatch {
        /// Quote denomination required by the policy.
        required: Denomination,
        /// Quote denomination carried by the price.
        actual: Denomination,
    },
    /// The round is older than its configured max age.
    #[error("price is stale: age {age_secs}s exceeds max age {max_age_secs}s")]
    Stale {
        /// Observed age in seconds.
        age_secs: u64,
        /// Configured max age in seconds.
        max_age_secs: u64,
    },
    /// The round data is incomplete.
    #[error("price round is incomplete")]
    IncompleteRound,
    /// The answer is disallowed by the feed's validity classification.
    #[error("price answer is invalid")]
    InvalidAnswer,
    /// The round validity is not currently known.
    #[error("price round status is unknown")]
    UnknownRoundStatus,
    /// The value's reconciliation lifecycle is not allowed by the policy
    /// (for example event-pending without
    /// [`PricePolicy::allow_event_pending`]).
    #[error("{}", value_status_violation_message(.0))]
    ValueStatusNotAllowed(OracleValueStatus),
    /// The source that produced the value is not allowed by the policy (for
    /// example a mock source without [`PricePolicy::allow_mock_source`]).
    #[error("{}", source_violation_message(.0))]
    SourceNotAllowed(OracleValueSource),
    /// The policy requires a strictly positive market price.
    #[error("liquidation price requires a positive market price")]
    NonPositivePrice,
    /// An amount cannot be valued with a negative market price.
    #[error("cannot value an amount with a negative market price")]
    NegativePrice,
    /// The token amount's asset does not match the price's base asset.
    #[error("token amount base asset {actual} does not match price base {expected}")]
    BaseAssetMismatch {
        /// Base asset priced by the checked price.
        expected: AssetId,
        /// Asset carried by the token amount.
        actual: AssetId,
    },
    /// A [`ValuedAmount`] cannot be negative.
    #[error("valued amount cannot be negative")]
    NegativeValuedAmount,
    /// A valuation or scaling computation left the representable range.
    ///
    /// The payload is a static description of the failed step (for example
    /// `"valuation multiplication overflowed"`).
    #[error("{0}")]
    ValueOutOfRange(&'static str),
}

fn value_status_violation_message(status: &OracleValueStatus) -> &'static str {
    match status {
        OracleValueStatus::EventPending => "price is pending authoritative proxy reconciliation",
        OracleValueStatus::RequiresRepair => "price requires authoritative repair",
        OracleValueStatus::Unknown => "price value status is unknown",
        _ => "price value status is not allowed",
    }
}

fn source_violation_message(source: &OracleValueSource) -> &'static str {
    match source {
        OracleValueSource::Event => "event-derived price source is not allowed",
        OracleValueSource::Mock => "mock price source is not allowed",
        OracleValueSource::Derived => "derived price source is not allowed",
        OracleValueSource::Unknown => "price source is unknown",
        _ => "price source is not allowed",
    }
}

/// Consumer-facing typed oracle price.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OraclePrice {
    /// Stable feed id.
    pub id: crate::FeedId,
    /// User-facing proxy address.
    pub proxy: Address,
    /// Best-known aggregator.
    pub aggregator: Option<Address>,
    /// Optional human-readable label.
    pub label: Option<String>,
    /// Optional base symbol.
    pub base: Option<String>,
    /// Optional quote symbol.
    pub quote: Option<String>,
    /// Raw signed answer.
    pub raw_answer: I256,
    /// Feed decimals.
    pub decimals: u8,
    /// Current round id.
    pub round_id: U256,
    /// Current `updatedAt` timestamp.
    pub updated_at: u64,
    /// Full current round data.
    pub round: RoundData,
    /// Round validity classification.
    pub round_status: OracleRoundStatus,
    /// Value reconciliation lifecycle.
    pub value_status: OracleValueStatus,
    /// Source that produced the current snapshot.
    pub source: OracleValueSource,
}

impl OraclePrice {
    pub(crate) fn from_snapshot(
        snapshot: &OracleSnapshot,
        registration: &FeedRegistration,
    ) -> Self {
        Self {
            id: snapshot.id.clone(),
            proxy: snapshot.proxy,
            aggregator: snapshot.aggregator,
            label: registration.label.clone(),
            base: registration.base.clone(),
            quote: registration.quote.clone(),
            raw_answer: snapshot.round.answer,
            decimals: snapshot.metadata.decimals,
            round_id: snapshot.round.round_id,
            updated_at: snapshot.round.updated_at,
            round: snapshot.round.clone(),
            round_status: snapshot.round_status.clone(),
            value_status: snapshot.value_status,
            source: snapshot.source,
        }
    }

    /// Return true when the price is fresh and allowed for immediate decisions.
    pub fn is_actionable(&self) -> bool {
        self.round_status == OracleRoundStatus::Fresh
            && matches!(
                self.value_status,
                OracleValueStatus::EventPending
                    | OracleValueStatus::Confirmed
                    | OracleValueStatus::Corrected
            )
    }

    /// Return true when the price came from an event awaiting reconciliation.
    pub fn is_event_pending(&self) -> bool {
        self.value_status == OracleValueStatus::EventPending
    }

    /// Return true when the price was confirmed by a proxy read.
    pub fn is_confirmed(&self) -> bool {
        self.value_status == OracleValueStatus::Confirmed
    }

    /// Return true when the price was corrected by a proxy read.
    pub fn is_corrected(&self) -> bool {
        self.value_status == OracleValueStatus::Corrected
    }

    /// Return the corresponding latest round tuple.
    pub fn round_data(&self) -> RoundData {
        self.round.clone()
    }

    /// Scale `raw_answer` to `target_decimals`.
    pub fn scaled_to(&self, target_decimals: u8) -> Result<I256, OracleError> {
        scale_i256(self.raw_answer, self.decimals, target_decimals)
    }

    /// Promote this price to a checked price under the supplied policy.
    pub fn require(&self, policy: PricePolicy) -> Result<CheckedPrice, OracleError> {
        policy.check(self)
    }
}

/// Policy for promoting raw oracle prices into checked prices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PricePolicy {
    kind: PricePolicyKind,
    quote: Option<Denomination>,
    allow_event_pending: bool,
    allow_mock_source: bool,
    allow_derived_source: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PricePolicyKind {
    Liquidation,
}

impl PricePolicy {
    /// Policy suitable for liquidation and other market-valuation decisions.
    pub fn liquidation() -> Self {
        Self {
            kind: PricePolicyKind::Liquidation,
            quote: None,
            allow_event_pending: false,
            allow_mock_source: false,
            allow_derived_source: false,
        }
    }

    /// Require a specific quote denomination.
    pub fn quote(mut self, quote: Denomination) -> Self {
        self.quote = Some(quote);
        self
    }

    /// Allow fresh event-derived prices before authoritative proxy reconciliation.
    pub fn allow_event_pending(mut self) -> Self {
        self.allow_event_pending = true;
        self
    }

    /// Allow caller-controlled mock prices. Intended for tests and simulations.
    pub fn allow_mock_source(mut self) -> Self {
        self.allow_mock_source = true;
        self
    }

    /// Allow prices derived from dependency feeds.
    pub fn allow_derived_source(mut self) -> Self {
        self.allow_derived_source = true;
        self
    }

    fn check(&self, price: &OraclePrice) -> Result<CheckedPrice, OracleError> {
        let base = price
            .base
            .as_deref()
            .map(AssetId::symbol)
            .ok_or(OracleError::Policy(PricePolicyViolation::MissingBase))?;
        let quote = price
            .quote
            .as_deref()
            .map(Denomination::from)
            .ok_or(OracleError::Policy(PricePolicyViolation::MissingQuote))?;

        if let Some(required_quote) = &self.quote
            && &quote != required_quote
        {
            return Err(OracleError::Policy(PricePolicyViolation::QuoteMismatch {
                required: required_quote.clone(),
                actual: quote.clone(),
            }));
        }

        match &price.round_status {
            OracleRoundStatus::Fresh => {}
            OracleRoundStatus::Stale {
                age_secs,
                max_age_secs,
            } => {
                return Err(OracleError::Policy(PricePolicyViolation::Stale {
                    age_secs: *age_secs,
                    max_age_secs: *max_age_secs,
                }));
            }
            OracleRoundStatus::IncompleteRound => {
                return Err(OracleError::Policy(PricePolicyViolation::IncompleteRound));
            }
            OracleRoundStatus::InvalidAnswer => {
                return Err(OracleError::Policy(PricePolicyViolation::InvalidAnswer));
            }
            OracleRoundStatus::Unknown => {
                return Err(OracleError::Policy(
                    PricePolicyViolation::UnknownRoundStatus,
                ));
            }
        }

        match price.value_status {
            OracleValueStatus::Confirmed | OracleValueStatus::Corrected => {}
            OracleValueStatus::EventPending if self.allow_event_pending => {}
            OracleValueStatus::EventPending => {
                return Err(OracleError::Policy(
                    PricePolicyViolation::ValueStatusNotAllowed(OracleValueStatus::EventPending),
                ));
            }
            OracleValueStatus::RequiresRepair => {
                return Err(OracleError::Policy(
                    PricePolicyViolation::ValueStatusNotAllowed(OracleValueStatus::RequiresRepair),
                ));
            }
            OracleValueStatus::Unknown => {
                return Err(OracleError::Policy(
                    PricePolicyViolation::ValueStatusNotAllowed(OracleValueStatus::Unknown),
                ));
            }
        }

        match price.source {
            OracleValueSource::Proxy => {}
            OracleValueSource::Event if self.allow_event_pending => {}
            OracleValueSource::Event => {
                return Err(OracleError::Policy(PricePolicyViolation::SourceNotAllowed(
                    OracleValueSource::Event,
                )));
            }
            OracleValueSource::Mock if self.allow_mock_source => {}
            OracleValueSource::Mock => {
                return Err(OracleError::Policy(PricePolicyViolation::SourceNotAllowed(
                    OracleValueSource::Mock,
                )));
            }
            OracleValueSource::Derived if self.allow_derived_source => {}
            OracleValueSource::Derived => {
                return Err(OracleError::Policy(PricePolicyViolation::SourceNotAllowed(
                    OracleValueSource::Derived,
                )));
            }
            OracleValueSource::Unknown => {
                return Err(OracleError::Policy(PricePolicyViolation::SourceNotAllowed(
                    OracleValueSource::Unknown,
                )));
            }
        }

        match self.kind {
            PricePolicyKind::Liquidation if price.raw_answer <= I256::ZERO => {
                return Err(OracleError::Policy(PricePolicyViolation::NonPositivePrice));
            }
            PricePolicyKind::Liquidation => {}
        }

        Ok(CheckedPrice {
            base,
            quote,
            raw_answer: price.raw_answer,
            decimals: price.decimals,
        })
    }
}

/// Oracle price that has satisfied a caller-selected policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckedPrice {
    base: AssetId,
    quote: Denomination,
    raw_answer: I256,
    decimals: u8,
}

impl CheckedPrice {
    /// Base asset priced by this oracle.
    pub fn base(&self) -> &AssetId {
        &self.base
    }

    /// Quote denomination of this oracle price.
    pub fn quote(&self) -> &Denomination {
        &self.quote
    }

    /// Raw signed oracle answer.
    pub fn raw_answer(&self) -> I256 {
        self.raw_answer
    }

    /// Value a token amount in this price's quote denomination.
    pub fn value_of(
        &self,
        amount: TokenAmount,
        output_decimals: u8,
    ) -> Result<ValuedAmount, OracleError> {
        if amount.asset() != &self.base {
            return Err(OracleError::Policy(
                PricePolicyViolation::BaseAssetMismatch {
                    expected: self.base.clone(),
                    actual: amount.asset().clone(),
                },
            ));
        }
        if self.raw_answer.is_negative() {
            return Err(OracleError::Policy(PricePolicyViolation::NegativePrice));
        }

        let amount_raw = I256::try_from(amount.raw()).map_err(|_| {
            OracleError::Policy(PricePolicyViolation::ValueOutOfRange(
                "token amount does not fit in int256",
            ))
        })?;
        let product = amount_raw
            .checked_mul(self.raw_answer)
            .ok_or(OracleError::Policy(PricePolicyViolation::ValueOutOfRange(
                "valuation multiplication overflowed",
            )))?;

        let input_decimals = u16::from(amount.decimals()) + u16::from(self.decimals);
        let raw = if input_decimals >= u16::from(output_decimals) {
            let scale = pow10_i256(input_decimals - u16::from(output_decimals))?;
            product.checked_div(scale).ok_or(OracleError::Policy(
                PricePolicyViolation::ValueOutOfRange("valuation division failed"),
            ))?
        } else {
            let scale = pow10_i256(u16::from(output_decimals) - input_decimals)?;
            product.checked_mul(scale).ok_or(OracleError::Policy(
                PricePolicyViolation::ValueOutOfRange("valuation scale-up overflowed"),
            ))?
        };

        ValuedAmount::checked_new(self.quote.clone(), raw, output_decimals)
    }
}

fn scale_i256(value: I256, from_decimals: u8, to_decimals: u8) -> Result<I256, OracleError> {
    if from_decimals == to_decimals {
        return Ok(value);
    }

    let diff = from_decimals.abs_diff(to_decimals);
    let factor = I256::unchecked_from(10_i64)
        .checked_pow(U256::from(diff))
        .ok_or(OracleError::Policy(PricePolicyViolation::ValueOutOfRange(
            "decimal scale factor overflowed",
        )))?;

    if to_decimals > from_decimals {
        value
            .checked_mul(factor)
            .ok_or(OracleError::Policy(PricePolicyViolation::ValueOutOfRange(
                "scaled oracle price overflowed",
            )))
    } else {
        value
            .checked_div(factor)
            .ok_or(OracleError::Policy(PricePolicyViolation::ValueOutOfRange(
                "scaled oracle price division failed",
            )))
    }
}

fn pow10_i256(exp: u16) -> Result<I256, OracleError> {
    I256::unchecked_from(10_i64)
        .checked_pow(U256::from(exp))
        .ok_or(OracleError::Policy(PricePolicyViolation::ValueOutOfRange(
            "decimal scale factor overflowed",
        )))
}
