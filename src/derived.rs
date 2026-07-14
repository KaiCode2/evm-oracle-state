//! Authoritative protocol reads for derived oracle sources.
//!
//! Derived sources (Morpho `MorphoChainlinkOracleV2`, Euler quote adapters)
//! cannot be confirmed through Chainlink proxy reads; their authoritative
//! value is the protocol's own view call. [`OracleDerivedReader`] is the seam
//! [`crate::OracleTracker::reconcile_derived_pending_with`] reads through, and
//! the [`EvmCache`] implementation performs the real reads: Morpho `price()`
//! on the oracle contract, and Euler `getQuote(one base unit, base, quote)`
//! through the user-facing quote target.

use alloy_primitives::I256;
use evm_fork_cache::cache::EvmCache;

use crate::{FeedRegistration, OracleError};

#[cfg(feature = "morpho")]
alloy_sol_types::sol! {
    interface MorphoDerivedOracle {
        function price() external view returns (uint256);
    }
}

#[cfg(feature = "euler")]
alloy_sol_types::sol! {
    interface EulerDerivedOracle {
        function getQuote(uint256 inAmount, address base, address quote) external view returns (uint256 outAmount);
    }
}

/// Reads the authoritative protocol value for a derived oracle source.
///
/// Implementations return the raw signed answer in the feed's registered
/// decimals (`registration.metadata.decimals`) — the same scale the feed's
/// event path produces. The [`EvmCache`] implementation is the production
/// reader; tests can substitute a mock to drive
/// [`crate::OracleTracker::reconcile_derived_pending_with`] offline.
pub trait OracleDerivedReader {
    /// Read the authoritative user-facing value for `registration`.
    ///
    /// # Errors
    ///
    /// Returns [`OracleError::Provider`] when the protocol read fails and
    /// [`OracleError::Unsupported`] when the registration's source has no
    /// derived protocol read.
    fn read_derived_value(&mut self, registration: &FeedRegistration) -> Result<I256, OracleError>;
}

impl OracleDerivedReader for EvmCache {
    fn read_derived_value(&mut self, registration: &FeedRegistration) -> Result<I256, OracleError> {
        #[cfg(feature = "morpho")]
        if let crate::FeedSource::MorphoChainlinkV2 { source, .. } = &registration.source {
            let price = self
                .call_sol(*source, MorphoDerivedOracle::priceCall {})
                .map_err(|error| {
                    OracleError::Provider(format!(
                        "Morpho price() read via {source:?} failed: {error}"
                    ))
                })?;
            return i256_answer(price, "Morpho price()");
        }
        #[cfg(feature = "euler")]
        if let crate::FeedSource::EulerQuote {
            source,
            base,
            quote,
            base_decimals,
            ..
        }
        | crate::FeedSource::EulerCross {
            source,
            base,
            quote,
            base_decimals,
            ..
        } = &registration.source
        {
            // Euler adapters enforce their own `maxStaleness` inside
            // `getQuote`, so a successful read implies the oracle currently
            // considers its dependencies fresh.
            let in_amount = crate::euler::one_base_unit(*base_decimals)?;
            let out = self
                .call_sol(
                    *source,
                    EulerDerivedOracle::getQuoteCall {
                        inAmount: in_amount,
                        base: *base,
                        quote: *quote,
                    },
                )
                .map_err(|error| {
                    OracleError::Provider(format!(
                        "Euler getQuote read via {source:?} failed: {error}"
                    ))
                })?;
            return i256_answer(out, "Euler getQuote outAmount");
        }
        Err(OracleError::Unsupported(format!(
            "source for feed `{}` has no derived protocol read",
            registration.id
        )))
    }
}

#[cfg(any(feature = "morpho", feature = "euler"))]
fn i256_answer(value: alloy_primitives::U256, field: &'static str) -> Result<I256, OracleError> {
    I256::try_from(value)
        .map_err(|_| OracleError::Unsupported(format!("{field} does not fit int256")))
}
