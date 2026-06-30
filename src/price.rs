use alloy_primitives::{Address, I256, U256};

use crate::{
    AssetId, Denomination, FeedRegistration, FeedStatus, OracleError, OracleSnapshot,
    OracleSnapshotStatus, RoundData, SnapshotSource, TokenAmount, ValuedAmount,
};

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
    pub status: FeedStatus,
    /// Snapshot lifecycle confidence.
    pub confidence: OracleSnapshotStatus,
    /// Source that produced the current snapshot.
    pub source: SnapshotSource,
}

impl OraclePrice {
    pub(crate) fn from_snapshot(
        snapshot: &OracleSnapshot,
        registration: &FeedRegistration,
    ) -> Self {
        Self {
            id: snapshot.id,
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
            status: snapshot.status.clone(),
            confidence: snapshot.confidence,
            source: snapshot.source,
        }
    }

    /// Return true when the price is fresh and allowed for immediate decisions.
    pub fn is_actionable(&self) -> bool {
        self.status == FeedStatus::Fresh
            && matches!(
                self.confidence,
                OracleSnapshotStatus::EventPending
                    | OracleSnapshotStatus::Confirmed
                    | OracleSnapshotStatus::Corrected
            )
    }

    /// Return true when the price came from an event awaiting reconciliation.
    pub fn is_event_pending(&self) -> bool {
        self.confidence == OracleSnapshotStatus::EventPending
    }

    /// Return true when the price was confirmed by a proxy read.
    pub fn is_confirmed(&self) -> bool {
        self.confidence == OracleSnapshotStatus::Confirmed
    }

    /// Return true when the price was corrected by a proxy read.
    pub fn is_corrected(&self) -> bool {
        self.confidence == OracleSnapshotStatus::Corrected
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

    fn check(&self, price: &OraclePrice) -> Result<CheckedPrice, OracleError> {
        let base = price
            .base
            .as_deref()
            .map(AssetId::symbol)
            .ok_or_else(|| policy_error("price base asset is unknown"))?;
        let quote = price
            .quote
            .as_deref()
            .map(Denomination::from)
            .ok_or_else(|| policy_error("price quote denomination is unknown"))?;

        if let Some(required_quote) = &self.quote
            && &quote != required_quote
        {
            return Err(policy_error(format!(
                "price quote {quote} does not match required quote {required_quote}"
            )));
        }

        match &price.status {
            FeedStatus::Fresh => {}
            FeedStatus::Stale {
                age_secs,
                max_age_secs,
            } => {
                return Err(policy_error(format!(
                    "price is stale: age {age_secs}s exceeds max age {max_age_secs}s"
                )));
            }
            FeedStatus::IncompleteRound => {
                return Err(policy_error("price round is incomplete"));
            }
            FeedStatus::InvalidAnswer => {
                return Err(policy_error("price answer is invalid"));
            }
            FeedStatus::Unknown => {
                return Err(policy_error("price status is unknown"));
            }
        }

        match price.confidence {
            OracleSnapshotStatus::Confirmed | OracleSnapshotStatus::Corrected => {}
            OracleSnapshotStatus::EventPending if self.allow_event_pending => {}
            OracleSnapshotStatus::EventPending => {
                return Err(policy_error(
                    "price is pending authoritative proxy reconciliation",
                ));
            }
            OracleSnapshotStatus::Stale => {
                return Err(policy_error("price snapshot is stale"));
            }
            OracleSnapshotStatus::Invalid => {
                return Err(policy_error("price snapshot is invalid"));
            }
            OracleSnapshotStatus::Unknown => {
                return Err(policy_error("price snapshot confidence is unknown"));
            }
        }

        match self.kind {
            PricePolicyKind::Liquidation if price.raw_answer <= I256::ZERO => {
                return Err(policy_error(
                    "liquidation price requires a positive market price",
                ));
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
            return Err(policy_error(format!(
                "token amount base asset {} does not match price base {}",
                amount.asset(),
                self.base
            )));
        }
        if self.raw_answer.is_negative() {
            return Err(policy_error(
                "cannot value an amount with a negative market price",
            ));
        }

        let amount_raw = I256::try_from(amount.raw())
            .map_err(|_| policy_error("token amount does not fit in int256"))?;
        let product = amount_raw
            .checked_mul(self.raw_answer)
            .ok_or_else(|| policy_error("valuation multiplication overflowed"))?;

        let input_decimals = u16::from(amount.decimals()) + u16::from(self.decimals);
        let raw = if input_decimals >= u16::from(output_decimals) {
            let scale = pow10_i256(input_decimals - u16::from(output_decimals))?;
            product
                .checked_div(scale)
                .ok_or_else(|| policy_error("valuation division failed"))?
        } else {
            let scale = pow10_i256(u16::from(output_decimals) - input_decimals)?;
            product
                .checked_mul(scale)
                .ok_or_else(|| policy_error("valuation scale-up overflowed"))?
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
        .ok_or_else(|| OracleError::Provider("decimal scale factor overflowed".to_string()))?;

    if to_decimals > from_decimals {
        value
            .checked_mul(factor)
            .ok_or_else(|| OracleError::Provider("scaled oracle price overflowed".to_string()))
    } else {
        value
            .checked_div(factor)
            .ok_or_else(|| OracleError::Provider("scaled oracle price division failed".to_string()))
    }
}

fn pow10_i256(exp: u16) -> Result<I256, OracleError> {
    I256::unchecked_from(10_i64)
        .checked_pow(U256::from(exp))
        .ok_or_else(|| policy_error("decimal scale factor overflowed"))
}

fn policy_error(message: impl Into<String>) -> OracleError {
    OracleError::Provider(message.into())
}
