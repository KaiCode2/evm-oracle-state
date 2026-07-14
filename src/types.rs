use std::{convert::Infallible, fmt, str::FromStr};

use alloy_primitives::{I256, U256};

use crate::OracleError;

/// Stable typed asset identifier used for oracle bases and token amounts.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AssetId(String);

impl AssetId {
    /// Create an asset id from a human-readable symbol.
    pub fn symbol(symbol: impl Into<String>) -> Self {
        Self(normalize_symbol(symbol.into()))
    }

    /// Borrow the config-compatible string representation.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AssetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl From<&str> for AssetId {
    fn from(value: &str) -> Self {
        Self::symbol(value)
    }
}

impl From<String> for AssetId {
    fn from(value: String) -> Self {
        Self::symbol(value)
    }
}

impl From<AssetId> for String {
    fn from(value: AssetId) -> Self {
        value.0
    }
}

impl FromStr for AssetId {
    type Err = Infallible;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self::symbol(value))
    }
}

/// Typed quote denomination for oracle prices and valued amounts.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Denomination {
    /// United States dollars.
    Usd,
    /// Ether.
    Eth,
    /// Bitcoin.
    Btc,
    /// Another config-compatible quote symbol.
    Other(String),
}

impl Denomination {
    /// Borrow the config-compatible string representation.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Usd => "USD",
            Self::Eth => "ETH",
            Self::Btc => "BTC",
            Self::Other(value) => value.as_str(),
        }
    }
}

impl fmt::Display for Denomination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.as_str().fmt(f)
    }
}

impl From<&str> for Denomination {
    fn from(value: &str) -> Self {
        match normalize_symbol(value.to_string()).as_str() {
            "USD" => Self::Usd,
            "ETH" => Self::Eth,
            "BTC" => Self::Btc,
            other => Self::Other(other.to_string()),
        }
    }
}

impl From<String> for Denomination {
    fn from(value: String) -> Self {
        Self::from(value.as_str())
    }
}

impl From<Denomination> for String {
    fn from(value: Denomination) -> Self {
        value.to_string()
    }
}

impl FromStr for Denomination {
    type Err = Infallible;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(Self::from(value))
    }
}

/// Raw token amount in the base asset's smallest units.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenAmount {
    asset: AssetId,
    raw: U256,
    decimals: u8,
}

impl TokenAmount {
    /// Create a raw token amount.
    pub fn new(asset: AssetId, raw: U256, decimals: u8) -> Self {
        Self {
            asset,
            raw,
            decimals,
        }
    }

    /// Asset this amount is denominated in.
    pub fn asset(&self) -> &AssetId {
        &self.asset
    }

    /// Raw amount in smallest units.
    pub fn raw(&self) -> U256 {
        self.raw
    }

    /// Number of decimals used by the raw token amount.
    pub fn decimals(&self) -> u8 {
        self.decimals
    }
}

/// Checked quote-denominated value produced from a typed oracle price.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValuedAmount {
    denomination: Denomination,
    raw: I256,
    decimals: u8,
}

impl ValuedAmount {
    /// Construct a confirmed valued amount after validating the raw value is non-negative.
    pub fn checked_new(
        denomination: Denomination,
        raw: I256,
        decimals: u8,
    ) -> Result<Self, OracleError> {
        if raw.is_negative() {
            return Err(OracleError::Policy(
                crate::PricePolicyViolation::NegativeValuedAmount,
            ));
        }
        Ok(Self {
            denomination,
            raw,
            decimals,
        })
    }

    /// Denomination of this value.
    pub fn denomination(&self) -> &Denomination {
        &self.denomination
    }

    /// Raw value in the requested output decimals.
    pub fn raw(&self) -> I256 {
        self.raw
    }

    /// Number of decimals used by the raw value.
    pub fn decimals(&self) -> u8 {
        self.decimals
    }
}

fn normalize_symbol(symbol: String) -> String {
    symbol.trim().to_ascii_uppercase()
}
