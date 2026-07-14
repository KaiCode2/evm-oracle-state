use std::{collections::BTreeMap, sync::Arc};

use alloy_primitives::{Address, B256, I256, U256, U512};

use crate::{
    AggregatorLayout, AggregatorLayoutConfidence, AggregatorLayoutEvidence, ChainlinkFeedProvider,
    FeedConfig, FeedMetadata, OracleBlockRef, OracleError, OracleFeedStatus, OracleSnapshot,
    OracleSourceDescriptor, OracleTransformDescriptor, OracleValueSource, RoundData,
    classify_type_and_version,
};

/// Stable id for a registered feed.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FeedId(Arc<str>);

impl FeedId {
    /// Create a feed id from its string form. Ids compare and order exactly
    /// like their underlying string; cloning is a cheap `Arc` bump.
    pub fn new(id: impl Into<String>) -> Self {
        Self(Arc::from(id.into()))
    }

    /// Borrow the underlying string, e.g. for logging or map keys.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

// Enables allocation-free `BTreeMap<FeedId, _>` lookups by `&str`. `FeedId`'s
// derived `Ord`/`Eq`/`Hash` all delegate to the inner `str`, so the `Borrow`
// consistency contract holds.
impl std::borrow::Borrow<str> for FeedId {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl std::fmt::Display for FeedId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl AsRef<str> for FeedId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

/// One Chainlink-compatible dependency used by a derived oracle source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OracleDependency {
    /// Dependency proxy address.
    pub proxy: Address,
    /// Dependency current aggregator address.
    pub aggregator: Address,
    /// Last known dependency answer.
    pub answer: I256,
}

impl OracleDependency {
    /// Construct a dependency leg from its proxy, current aggregator, and the
    /// last answer to seed derived-source math with until the next event.
    pub fn new(proxy: Address, aggregator: Address, answer: I256) -> Self {
        Self {
            proxy,
            aggregator,
            answer,
        }
    }
}

/// Role of a Chainlink dependency inside `MorphoChainlinkOracleV2`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MorphoChainlinkFeedRole {
    /// First numerator feed.
    BaseFeed1,
    /// Second numerator feed.
    BaseFeed2,
    /// First denominator feed.
    QuoteFeed1,
    /// Second denominator feed.
    QuoteFeed2,
}

/// One optional Chainlink feed used by `MorphoChainlinkOracleV2`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MorphoChainlinkFeed {
    /// Feed role in the Morpho formula.
    pub role: MorphoChainlinkFeedRole,
    /// Dependency proxy, aggregator, and answer.
    pub dependency: OracleDependency,
}

impl MorphoChainlinkFeed {
    /// Construct a Morpho Chainlink dependency.
    pub fn new(role: MorphoChainlinkFeedRole, dependency: OracleDependency) -> Self {
        Self { role, dependency }
    }
}

/// One dependency leg inside an Euler quote adapter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EulerQuoteLeg {
    /// Chainlink-backed Euler `ChainlinkOracle` leg.
    Chainlink {
        /// Euler leg adapter address.
        oracle: Address,
        /// Chainlink feed proxy.
        feed: Address,
        /// Chainlink feed current aggregator.
        aggregator: Address,
        /// Last known Chainlink answer.
        answer: I256,
        /// Chainlink feed decimals.
        feed_decimals: u8,
        /// True when the requested leg is the inverse of the adapter's configured pair.
        inverse: bool,
        /// Output decimals for this leg.
        quote_decimals: u8,
    },
    /// Euler `FixedRateOracle` leg.
    FixedRate {
        /// Euler leg adapter address.
        oracle: Address,
        /// Fixed rate scaled to this leg's quote decimals.
        rate: I256,
        /// True when the requested leg is the inverse of the adapter's configured pair.
        inverse: bool,
        /// Requested base decimals.
        base_decimals: u8,
        /// Requested quote decimals.
        quote_decimals: u8,
    },
    /// Euler `RateProviderOracle` leg.
    RateProvider {
        /// Euler leg adapter address.
        oracle: Address,
        /// Rate provider contract.
        rate_provider: Address,
        /// Last known 18-decimal rate.
        rate: I256,
        /// True when the requested leg is the inverse of the adapter's configured pair.
        inverse: bool,
        /// Output decimals for this leg.
        quote_decimals: u8,
    },
}

impl EulerQuoteLeg {
    /// Return the event aggregator that drives this leg, if any.
    pub fn event_aggregator(&self) -> Option<Address> {
        match self {
            Self::Chainlink { aggregator, .. } => Some(*aggregator),
            Self::FixedRate { .. } | Self::RateProvider { .. } => None,
        }
    }

    /// Return the leg price scaled to the leg quote decimals.
    pub fn price(&self) -> I256 {
        self.price_from_event(None, I256::ZERO)
    }

    /// Return the leg price with a dependency event answer applied when relevant.
    pub fn price_from_event(&self, aggregator: Option<Address>, answer: I256) -> I256 {
        match self {
            Self::Chainlink {
                aggregator: dependency_aggregator,
                answer: stored_answer,
                feed_decimals,
                inverse,
                quote_decimals,
                ..
            } => {
                let answer = if aggregator == Some(*dependency_aggregator) {
                    answer
                } else {
                    *stored_answer
                };
                if *inverse {
                    inverse_positive_price(answer, *feed_decimals, *quote_decimals)
                } else {
                    scale_positive_price(answer, *feed_decimals, *quote_decimals)
                }
            }
            Self::FixedRate {
                rate,
                inverse,
                base_decimals,
                quote_decimals,
                ..
            } => {
                if *inverse {
                    inverse_fixed_rate(*rate, *base_decimals, *quote_decimals)
                } else {
                    *rate
                }
            }
            Self::RateProvider {
                rate,
                inverse,
                quote_decimals,
                ..
            } => {
                if *inverse {
                    inverse_positive_price(*rate, 18, *quote_decimals)
                } else {
                    scale_positive_price(*rate, 18, *quote_decimals)
                }
            }
        }
    }
}

/// Network source and transform for a registered feed.
#[non_exhaustive]
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum FeedSource {
    /// Plain Chainlink-compatible proxy with identity answer transform.
    #[default]
    Chainlink,
    /// Aave `PriceCapAdapterStable` source backed by an underlying Chainlink proxy.
    AavePriceCapStable {
        /// User-facing Aave source adapter.
        source: Address,
        /// Underlying Chainlink proxy read for rounds and aggregator discovery.
        underlying_proxy: Address,
        /// Maximum answer exposed by the Aave adapter.
        price_cap: I256,
    },
    /// Aave ratio-cap/CAPO source backed by a base-to-USD Chainlink proxy.
    AaveRatioCap {
        /// User-facing Aave source adapter.
        source: Address,
        /// Underlying base-to-USD Chainlink proxy.
        base_to_usd_proxy: Address,
        /// Ratio provider used by the Aave source.
        ratio_provider: Address,
        /// Current source ratio.
        current_ratio: I256,
        /// Maximum ratio exposed by the Aave source.
        max_ratio: I256,
        /// Decimals used by the ratio values.
        ratio_decimals: u8,
    },
    /// Aave Chainlink synchronicity adapter composing asset-to-peg and peg-to-base feeds.
    AaveSynchronicityPegToBase {
        /// User-facing Aave source adapter.
        source: Address,
        /// Asset-to-peg Chainlink proxy.
        asset_to_peg_proxy: Address,
        /// Asset-to-peg current aggregator.
        asset_to_peg_aggregator: Address,
        /// Seeded asset-to-peg answer used when the other dependency updates.
        asset_to_peg_answer: I256,
        /// Asset-to-peg decimals.
        asset_to_peg_decimals: u8,
        /// Peg-to-base Chainlink proxy.
        peg_to_base_proxy: Address,
        /// Peg-to-base current aggregator.
        peg_to_base_aggregator: Address,
        /// Seeded peg-to-base answer used when the other dependency updates.
        peg_to_base_answer: I256,
        /// Peg-to-base decimals.
        peg_to_base_decimals: u8,
        /// Decimals for the user-facing composed answer.
        output_decimals: u8,
    },
    /// Aave fixed-price source with no Chainlink dependency.
    AaveFixedPrice {
        /// User-facing Aave source adapter.
        source: Address,
        /// Fixed user-facing answer.
        price: I256,
    },
    /// Morpho Blue `MorphoChainlinkOracleV2` source composed from Chainlink feeds and optional vault conversions.
    MorphoChainlinkV2 {
        /// User-facing Morpho oracle.
        source: Address,
        /// Latest base-vault conversion factor, or 1 when no base vault is configured.
        base_vault_assets: I256,
        /// Latest quote-vault conversion factor, or 1 when no quote vault is configured.
        quote_vault_assets: I256,
        /// Immutable Morpho scale factor.
        scale_factor: I256,
        /// Non-zero Chainlink feed dependencies.
        feeds: Vec<MorphoChainlinkFeed>,
    },
    /// Euler quote source backed by a single leg.
    EulerQuote {
        /// User-facing Euler oracle adapter or router.
        source: Address,
        /// Base asset for the quote.
        base: Address,
        /// Quote asset for the quote.
        quote: Address,
        /// Base token decimals.
        base_decimals: u8,
        /// Quote token decimals.
        quote_decimals: u8,
        /// Quote leg.
        leg: EulerQuoteLeg,
    },
    /// Euler `CrossAdapter` quote source backed by two quote legs.
    EulerCross {
        /// User-facing Euler cross adapter.
        source: Address,
        /// Base asset for the final quote.
        base: Address,
        /// Intermediate asset.
        cross: Address,
        /// Quote asset for the final quote.
        quote: Address,
        /// Base token decimals.
        base_decimals: u8,
        /// Intermediate token decimals.
        cross_decimals: u8,
        /// Quote token decimals.
        quote_decimals: u8,
        /// Base-to-cross leg.
        base_cross: EulerQuoteLeg,
        /// Cross-to-quote leg.
        cross_quote: EulerQuoteLeg,
    },
    /// Pyth EVM price feed keyed by a shared Pyth contract and per-feed price id.
    Pyth {
        /// Shared Pyth contract that emits updates and serves reads.
        pyth: Address,
        /// Pyth price feed id.
        price_id: B256,
        /// Pyth fixed-point exponent.
        expo: i32,
        /// Most recent Pyth confidence interval.
        conf: u64,
    },
    /// RedStone push price feed backed by an on-chain adapter/event source.
    #[cfg(feature = "redstone")]
    RedstonePush {
        /// User-facing RedStone price feed wrapper.
        price_feed: Address,
        /// RedStone adapter or merged price-feed event source.
        adapter: Address,
        /// RedStone data feed id.
        data_feed_id: B256,
    },
    /// Adapter-owned oracle source.
    Custom(OracleSourceDescriptor),
}

impl FeedSource {
    /// Construct an Aave capped stable source transform.
    pub fn aave_price_cap_stable(
        source: Address,
        underlying_proxy: Address,
        price_cap: I256,
    ) -> Self {
        Self::AavePriceCapStable {
            source,
            underlying_proxy,
            price_cap,
        }
    }

    /// Construct an Aave ratio-cap/CAPO source transform.
    pub fn aave_ratio_cap(
        source: Address,
        base_to_usd_proxy: Address,
        ratio_provider: Address,
        current_ratio: I256,
        max_ratio: I256,
        ratio_decimals: u8,
    ) -> Self {
        Self::AaveRatioCap {
            source,
            base_to_usd_proxy,
            ratio_provider,
            current_ratio,
            max_ratio,
            ratio_decimals,
        }
    }

    /// Construct an Aave peg-to-base synchronicity source transform.
    #[allow(clippy::too_many_arguments)]
    pub fn aave_synchronicity_peg_to_base(
        source: Address,
        asset_to_peg_proxy: Address,
        asset_to_peg_aggregator: Address,
        asset_to_peg_answer: I256,
        asset_to_peg_decimals: u8,
        peg_to_base_proxy: Address,
        peg_to_base_aggregator: Address,
        peg_to_base_answer: I256,
        peg_to_base_decimals: u8,
        output_decimals: u8,
    ) -> Self {
        Self::AaveSynchronicityPegToBase {
            source,
            asset_to_peg_proxy,
            asset_to_peg_aggregator,
            asset_to_peg_answer,
            asset_to_peg_decimals,
            peg_to_base_proxy,
            peg_to_base_aggregator,
            peg_to_base_answer,
            peg_to_base_decimals,
            output_decimals,
        }
    }

    /// Construct an Aave fixed-price source.
    pub fn aave_fixed_price(source: Address, price: I256) -> Self {
        Self::AaveFixedPrice { source, price }
    }

    /// Construct a Morpho Blue Chainlink V2 source transform.
    pub fn morpho_chainlink_v2(
        source: Address,
        base_vault_assets: I256,
        quote_vault_assets: I256,
        scale_factor: I256,
        feeds: Vec<MorphoChainlinkFeed>,
    ) -> Self {
        Self::MorphoChainlinkV2 {
            source,
            base_vault_assets,
            quote_vault_assets,
            scale_factor,
            feeds,
        }
    }

    /// Construct an Euler quote source backed by one quote leg.
    pub fn euler_quote(
        source: Address,
        base: Address,
        quote: Address,
        base_decimals: u8,
        quote_decimals: u8,
        leg: EulerQuoteLeg,
    ) -> Self {
        Self::EulerQuote {
            source,
            base,
            quote,
            base_decimals,
            quote_decimals,
            leg,
        }
    }

    /// Construct an Euler cross-adapter source backed by two quote legs.
    #[allow(clippy::too_many_arguments)]
    pub fn euler_cross(
        source: Address,
        base: Address,
        cross: Address,
        quote: Address,
        base_decimals: u8,
        cross_decimals: u8,
        quote_decimals: u8,
        base_cross: EulerQuoteLeg,
        cross_quote: EulerQuoteLeg,
    ) -> Self {
        Self::EulerCross {
            source,
            base,
            cross,
            quote,
            base_decimals,
            cross_decimals,
            quote_decimals,
            base_cross,
            cross_quote,
        }
    }

    /// Rewrite an Euler source's user-facing quote address.
    ///
    /// Router-resolved discovery reads the source config through the resolved
    /// adapter, but the user-facing contract callers quote through (and the
    /// read overlay serves) is the router itself. Non-Euler sources are
    /// returned unchanged.
    #[cfg(feature = "euler")]
    pub(crate) fn with_euler_user_source(self, user_source: Address) -> Self {
        match self {
            Self::EulerQuote {
                base,
                quote,
                base_decimals,
                quote_decimals,
                leg,
                ..
            } => Self::EulerQuote {
                source: user_source,
                base,
                quote,
                base_decimals,
                quote_decimals,
                leg,
            },
            Self::EulerCross {
                base,
                cross,
                quote,
                base_decimals,
                cross_decimals,
                quote_decimals,
                base_cross,
                cross_quote,
                ..
            } => Self::EulerCross {
                source: user_source,
                base,
                cross,
                quote,
                base_decimals,
                cross_decimals,
                quote_decimals,
                base_cross,
                cross_quote,
            },
            other => other,
        }
    }

    /// Construct a Pyth EVM source.
    pub fn pyth(pyth: Address, price_id: B256, expo: i32, conf: u64) -> Self {
        Self::Pyth {
            pyth,
            price_id,
            expo,
            conf,
        }
    }

    /// Construct a RedStone push source.
    #[cfg(feature = "redstone")]
    pub fn redstone_push(price_feed: Address, adapter: Address, data_feed_id: B256) -> Self {
        Self::RedstonePush {
            price_feed,
            adapter,
            data_feed_id,
        }
    }

    /// Construct an adapter-owned custom source.
    ///
    /// Custom sources participate in **Chainlink-style proxy reconciliation
    /// by default**: [`Self::supports_proxy_reconciliation`] returns `true`
    /// for `Custom`, so reconciliation reads
    /// [`OracleSourceDescriptor::read_proxy`] with `latestRoundData()` after
    /// every accepted event (and during [`crate::OracleTracker::reconcile`]).
    /// The `read_proxy` address therefore **must answer `latestRoundData()`**
    /// or reconciliation will fail with [`crate::OracleError::Provider`].
    /// There is currently no per-descriptor flag to opt a custom source out
    /// of this path — if the source is not Chainlink-shaped, point
    /// `read_proxy` at a contract that exposes a Chainlink-compatible
    /// `latestRoundData()` view for it.
    pub fn custom(descriptor: OracleSourceDescriptor) -> Self {
        Self::Custom(descriptor)
    }

    /// Return true when this source is a plain identity Chainlink source.
    pub fn is_identity(&self) -> bool {
        matches!(self, Self::Chainlink)
    }

    /// Return true when the built-in Chainlink reactive handler should route this source.
    pub fn uses_builtin_chainlink_handler(&self) -> bool {
        matches!(
            self,
            Self::Chainlink
                | Self::AavePriceCapStable { .. }
                | Self::AaveRatioCap { .. }
                | Self::AaveSynchronicityPegToBase { .. }
                | Self::MorphoChainlinkV2 { .. }
                | Self::EulerQuote { .. }
                | Self::EulerCross { .. }
        )
    }

    /// Return true when this source can be reconciled by reading a Chainlink proxy.
    ///
    /// Sources that fail this predicate are either reconciled through their
    /// own protocol view (see [`Self::supports_derived_reconciliation`]) or
    /// not reconciled at all.
    pub fn supports_proxy_reconciliation(&self) -> bool {
        !matches!(
            self,
            Self::AaveFixedPrice { .. }
                | Self::MorphoChainlinkV2 { .. }
                | Self::EulerQuote { .. }
                | Self::EulerCross { .. }
                | Self::Pyth { .. }
        )
    }

    /// Return true when this source is reconciled by reading its own protocol
    /// view instead of a Chainlink proxy.
    ///
    /// Morpho `MorphoChainlinkOracleV2` sources re-read `price()` and Euler
    /// quote/cross sources re-read `getQuote(one base unit, base, quote)`;
    /// both promote event-derived snapshots to `Confirmed`/`Corrected` through
    /// [`crate::OracleTracker::reconcile_derived_pending_with`]. Each family
    /// only participates when its adapter feature (`morpho` / `euler`) is
    /// enabled, so builds without the feature never queue derived requests
    /// they cannot serve.
    pub fn supports_derived_reconciliation(&self) -> bool {
        #[cfg(feature = "morpho")]
        if matches!(self, Self::MorphoChainlinkV2 { .. }) {
            return true;
        }
        #[cfg(feature = "euler")]
        if matches!(self, Self::EulerQuote { .. } | Self::EulerCross { .. }) {
            return true;
        }
        false
    }

    /// Proxy that should be read for authoritative Chainlink round data.
    pub fn read_proxy(&self, registration_proxy: Address) -> Address {
        match self {
            Self::Chainlink => registration_proxy,
            Self::AavePriceCapStable {
                underlying_proxy, ..
            } => *underlying_proxy,
            Self::AaveRatioCap {
                base_to_usd_proxy, ..
            } => *base_to_usd_proxy,
            Self::AaveSynchronicityPegToBase {
                asset_to_peg_proxy, ..
            } => *asset_to_peg_proxy,
            Self::AaveFixedPrice { source, .. } => *source,
            Self::MorphoChainlinkV2 { source, .. }
            | Self::EulerQuote { source, .. }
            | Self::EulerCross { source, .. } => *source,
            Self::Pyth { pyth, .. } => *pyth,
            #[cfg(feature = "redstone")]
            Self::RedstonePush { price_feed, .. } => *price_feed,
            Self::Custom(descriptor) => descriptor.read_proxy,
        }
    }

    /// Aggregators whose Chainlink events should drive this source.
    pub fn event_aggregators(&self, current_aggregator: Option<Address>) -> Vec<Address> {
        match self {
            Self::AaveSynchronicityPegToBase {
                asset_to_peg_aggregator,
                peg_to_base_aggregator,
                ..
            } => vec![*asset_to_peg_aggregator, *peg_to_base_aggregator],
            Self::MorphoChainlinkV2 { feeds, .. } => feeds
                .iter()
                .map(|feed| feed.dependency.aggregator)
                .collect(),
            Self::EulerQuote { leg, .. } => leg.event_aggregator().into_iter().collect(),
            Self::EulerCross {
                base_cross,
                cross_quote,
                ..
            } => base_cross
                .event_aggregator()
                .into_iter()
                .chain(cross_quote.event_aggregator())
                .collect(),
            Self::Pyth { pyth, .. } => vec![*pyth],
            #[cfg(feature = "redstone")]
            Self::RedstonePush {
                price_feed,
                adapter,
                ..
            } => {
                let mut aggregators = vec![*adapter];
                if price_feed != adapter {
                    aggregators.push(*price_feed);
                }
                aggregators
            }
            Self::AaveFixedPrice { .. } => Vec::new(),
            _ => current_aggregator.into_iter().collect(),
        }
    }

    /// Source classification for a value produced from this source's event path.
    pub fn event_value_source(&self) -> OracleValueSource {
        match self {
            Self::AavePriceCapStable { .. }
            | Self::AaveRatioCap { .. }
            | Self::AaveSynchronicityPegToBase { .. }
            | Self::AaveFixedPrice { .. }
            | Self::MorphoChainlinkV2 { .. }
            | Self::EulerQuote { .. }
            | Self::EulerCross { .. } => OracleValueSource::Derived,
            _ => OracleValueSource::Event,
        }
    }

    /// Return true when an event from `aggregator` is acceptable for this source.
    pub fn accepts_event_from(
        &self,
        current_aggregator: Option<Address>,
        aggregator: Address,
    ) -> bool {
        self.event_aggregators(current_aggregator)
            .into_iter()
            .any(|event_aggregator| event_aggregator == aggregator)
    }

    /// Return true when this source should use AnswerUpdated events for `aggregator`.
    pub fn wants_answer_updated_from(
        &self,
        current_aggregator: Option<Address>,
        aggregator: Address,
    ) -> bool {
        match self {
            Self::AaveRatioCap { .. }
            | Self::AaveSynchronicityPegToBase { .. }
            | Self::MorphoChainlinkV2 { .. }
            | Self::EulerQuote { .. }
            | Self::EulerCross { .. } => self.accepts_event_from(current_aggregator, aggregator),
            _ => false,
        }
    }

    /// Return true when Chainlink storage adapters can safely mirror this event.
    pub fn supports_direct_chainlink_storage_effects(&self) -> bool {
        match self {
            Self::AaveRatioCap { .. }
            | Self::AaveSynchronicityPegToBase { .. }
            | Self::MorphoChainlinkV2 { .. }
            | Self::EulerQuote { .. }
            | Self::EulerCross { .. }
            | Self::Pyth { .. } => false,
            #[cfg(feature = "redstone")]
            Self::RedstonePush { .. } => false,
            _ => true,
        }
    }

    /// Apply the source transform to an event or read answer.
    pub fn normalize_answer(&self, answer: I256) -> I256 {
        self.normalize_answer_from_event(None, answer)
    }

    /// Apply the source transform to an answer emitted by a specific dependency aggregator.
    pub fn normalize_answer_from_event(&self, aggregator: Option<Address>, answer: I256) -> I256 {
        match self {
            Self::Chainlink => answer,
            Self::AavePriceCapStable { price_cap, .. } if answer > *price_cap => *price_cap,
            Self::AavePriceCapStable { .. } => answer,
            Self::AaveRatioCap {
                current_ratio,
                max_ratio,
                ratio_decimals,
                ..
            } => {
                if answer <= I256::ZERO || *current_ratio <= I256::ZERO {
                    return I256::ZERO;
                }
                let capped_ratio = if current_ratio < max_ratio {
                    *current_ratio
                } else {
                    *max_ratio
                };
                if capped_ratio <= I256::ZERO {
                    return I256::ZERO;
                }
                div_or_zero(
                    answer.saturating_mul(capped_ratio),
                    decimal_scale(*ratio_decimals),
                )
            }
            Self::AaveSynchronicityPegToBase {
                asset_to_peg_aggregator,
                asset_to_peg_answer,
                asset_to_peg_decimals,
                peg_to_base_aggregator,
                peg_to_base_answer,
                peg_to_base_decimals,
                output_decimals,
                ..
            } => {
                let asset_to_peg = if aggregator == Some(*asset_to_peg_aggregator) {
                    answer
                } else {
                    *asset_to_peg_answer
                };
                let peg_to_base = if aggregator == Some(*peg_to_base_aggregator) {
                    answer
                } else {
                    *peg_to_base_answer
                };
                normalize_synchronicity_answer(
                    asset_to_peg,
                    *asset_to_peg_decimals,
                    peg_to_base,
                    *peg_to_base_decimals,
                    *output_decimals,
                )
            }
            Self::AaveFixedPrice { price, .. } => *price,
            Self::MorphoChainlinkV2 {
                base_vault_assets,
                quote_vault_assets,
                scale_factor,
                feeds,
                ..
            } => normalize_morpho_chainlink_v2_answer(
                *base_vault_assets,
                *quote_vault_assets,
                *scale_factor,
                feeds,
                aggregator,
                answer,
            ),
            Self::EulerQuote { leg, .. } => leg.price_from_event(aggregator, answer),
            Self::EulerCross {
                cross_decimals,
                base_cross,
                cross_quote,
                ..
            } => normalize_euler_cross_answer(
                base_cross.price_from_event(aggregator, answer),
                cross_quote.price_from_event(aggregator, answer),
                *cross_decimals,
            ),
            Self::Pyth { .. } => answer,
            #[cfg(feature = "redstone")]
            Self::RedstonePush { .. } => answer,
            Self::Custom(descriptor) => match &descriptor.transform {
                OracleTransformDescriptor::Identity | OracleTransformDescriptor::Custom { .. } => {
                    answer
                }
                OracleTransformDescriptor::PriceCap { cap } if answer > *cap => *cap,
                OracleTransformDescriptor::PriceCap { .. } => answer,
            },
        }
    }

    /// Apply the source transform to round data.
    pub fn normalize_round(&self, mut round: RoundData) -> RoundData {
        round.answer = match self {
            Self::AaveSynchronicityPegToBase {
                asset_to_peg_aggregator,
                ..
            } => self.normalize_answer_from_event(Some(*asset_to_peg_aggregator), round.answer),
            Self::MorphoChainlinkV2 { .. }
            | Self::EulerQuote { .. }
            | Self::EulerCross { .. }
            | Self::Pyth { .. } => round.answer,
            _ => self.normalize_answer(round.answer),
        };
        round
    }

    pub(crate) fn normalize_dependency_answers(
        &self,
        asset_to_peg_answer: I256,
        peg_to_base_answer: I256,
    ) -> Option<I256> {
        match self {
            Self::AaveSynchronicityPegToBase {
                asset_to_peg_decimals,
                peg_to_base_decimals,
                output_decimals,
                ..
            } => Some(normalize_synchronicity_answer(
                asset_to_peg_answer,
                *asset_to_peg_decimals,
                peg_to_base_answer,
                *peg_to_base_decimals,
                *output_decimals,
            )),
            _ => None,
        }
    }

    pub(crate) fn with_synchronicity_dependency_state(
        &self,
        asset_to_peg_aggregator: Address,
        asset_to_peg_answer: I256,
        peg_to_base_aggregator: Address,
        peg_to_base_answer: I256,
    ) -> Option<Self> {
        match self {
            Self::AaveSynchronicityPegToBase {
                source,
                asset_to_peg_proxy,
                asset_to_peg_decimals,
                peg_to_base_proxy,
                peg_to_base_decimals,
                output_decimals,
                ..
            } => Some(Self::AaveSynchronicityPegToBase {
                source: *source,
                asset_to_peg_proxy: *asset_to_peg_proxy,
                asset_to_peg_aggregator,
                asset_to_peg_answer,
                asset_to_peg_decimals: *asset_to_peg_decimals,
                peg_to_base_proxy: *peg_to_base_proxy,
                peg_to_base_aggregator,
                peg_to_base_answer,
                peg_to_base_decimals: *peg_to_base_decimals,
                output_decimals: *output_decimals,
            }),
            _ => None,
        }
    }

    /// Return Pyth source fields for Pyth-backed registrations.
    pub fn pyth_source(&self) -> Option<(Address, B256, i32, u64)> {
        match self {
            Self::Pyth {
                pyth,
                price_id,
                expo,
                conf,
            } => Some((*pyth, *price_id, *expo, *conf)),
            _ => None,
        }
    }

    /// Return this Pyth source with event-updated metadata.
    pub fn with_pyth_event_metadata(&self, price_id: B256, expo: i32, conf: u64) -> Option<Self> {
        match self {
            Self::Pyth { pyth, .. } => Some(Self::Pyth {
                pyth: *pyth,
                price_id,
                expo,
                conf,
            }),
            _ => None,
        }
    }
}

fn decimal_scale(decimals: u8) -> I256 {
    let mut scale = I256::unchecked_from(1_i8);
    for _ in 0..decimals {
        scale = scale.saturating_mul(I256::unchecked_from(10_i8));
    }
    scale
}

fn div_or_zero(numerator: I256, denominator: I256) -> I256 {
    if denominator == I256::ZERO {
        I256::ZERO
    } else {
        numerator / denominator
    }
}

fn normalize_synchronicity_answer(
    asset_to_peg: I256,
    asset_to_peg_decimals: u8,
    peg_to_base: I256,
    peg_to_base_decimals: u8,
    output_decimals: u8,
) -> I256 {
    if asset_to_peg <= I256::ZERO || peg_to_base <= I256::ZERO {
        return I256::ZERO;
    }
    let numerator = asset_to_peg
        .saturating_mul(peg_to_base)
        .saturating_mul(decimal_scale(output_decimals));
    let denominator = decimal_scale(asset_to_peg_decimals.saturating_add(peg_to_base_decimals));
    div_or_zero(numerator, denominator)
}

fn normalize_morpho_chainlink_v2_answer(
    base_vault_assets: I256,
    quote_vault_assets: I256,
    scale_factor: I256,
    feeds: &[MorphoChainlinkFeed],
    aggregator: Option<Address>,
    answer: I256,
) -> I256 {
    if base_vault_assets < I256::ZERO
        || quote_vault_assets <= I256::ZERO
        || scale_factor <= I256::ZERO
    {
        return I256::ZERO;
    }

    // Mirror `MorphoChainlinkOracleV2.price()`:
    // `SCALE_FACTOR.mulDiv(baseFeed1 * baseFeed2 * baseVaultAssets,
    //                      quoteFeed1 * quoteFeed2 * quoteVaultAssets)`.
    // The inner products are plain checked uint256 math on-chain (they revert
    // on overflow; the event path cannot revert, so overflow degrades to zero,
    // which round classification rejects as an invalid answer). The outer
    // multiply/divide uses a 512-bit intermediate exactly like OpenZeppelin's
    // `Math.mulDiv`, so large scale factors composed with multi-feed products
    // never saturate into a silently wrong price.
    let mut base_product = i256_magnitude(base_vault_assets);
    let mut quote_product = i256_magnitude(quote_vault_assets);
    for feed in feeds {
        let feed_answer = if aggregator == Some(feed.dependency.aggregator) {
            answer
        } else {
            feed.dependency.answer
        };
        if feed_answer < I256::ZERO {
            return I256::ZERO;
        }
        let magnitude = i256_magnitude(feed_answer);
        let product = match feed.role {
            MorphoChainlinkFeedRole::BaseFeed1 | MorphoChainlinkFeedRole::BaseFeed2 => {
                &mut base_product
            }
            MorphoChainlinkFeedRole::QuoteFeed1 | MorphoChainlinkFeedRole::QuoteFeed2 => {
                &mut quote_product
            }
        };
        *product = match product.checked_mul(magnitude) {
            Some(value) => value,
            None => return I256::ZERO,
        };
    }
    if quote_product.is_zero() {
        return I256::ZERO;
    }

    // 2^255 * 2^256 = 2^511 fits a U512, so this cannot overflow.
    let numerator = u512_from_u256(i256_magnitude(scale_factor)) * u512_from_u256(base_product);
    let quotient = numerator / u512_from_u256(quote_product);
    let limbs = quotient.into_limbs();
    if limbs[4..].iter().any(|limb| *limb != 0) {
        return I256::ZERO;
    }
    let low = U256::from_limbs([limbs[0], limbs[1], limbs[2], limbs[3]]);
    I256::try_from(low).unwrap_or(I256::ZERO)
}

/// Magnitude of a non-negative `I256` (callers guard against negatives).
fn i256_magnitude(value: I256) -> U256 {
    value.into_raw()
}

fn u512_from_u256(value: U256) -> U512 {
    let limbs = value.into_limbs();
    U512::from_limbs([limbs[0], limbs[1], limbs[2], limbs[3], 0, 0, 0, 0])
}

fn normalize_euler_cross_answer(
    base_cross_price: I256,
    cross_quote_price: I256,
    cross_decimals: u8,
) -> I256 {
    if base_cross_price <= I256::ZERO || cross_quote_price <= I256::ZERO {
        return I256::ZERO;
    }
    div_or_zero(
        base_cross_price.saturating_mul(cross_quote_price),
        decimal_scale(cross_decimals),
    )
}

fn scale_positive_price(answer: I256, from_decimals: u8, to_decimals: u8) -> I256 {
    if answer <= I256::ZERO {
        return I256::ZERO;
    }
    if from_decimals == to_decimals {
        answer
    } else if from_decimals < to_decimals {
        answer.saturating_mul(decimal_scale(to_decimals - from_decimals))
    } else {
        div_or_zero(answer, decimal_scale(from_decimals - to_decimals))
    }
}

fn inverse_positive_price(answer: I256, feed_decimals: u8, quote_decimals: u8) -> I256 {
    if answer <= I256::ZERO {
        return I256::ZERO;
    }
    div_or_zero(
        decimal_scale(feed_decimals.saturating_add(quote_decimals)),
        answer,
    )
}

fn inverse_fixed_rate(rate: I256, base_decimals: u8, quote_decimals: u8) -> I256 {
    if rate <= I256::ZERO {
        return I256::ZERO;
    }
    div_or_zero(
        decimal_scale(base_decimals.saturating_add(quote_decimals)),
        rate,
    )
}

/// Registered Chainlink-compatible proxy feed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedRegistration {
    /// Stable feed id.
    pub id: FeedId,
    /// User-facing proxy address.
    pub proxy: Address,
    /// Optional human-readable label.
    pub label: Option<String>,
    /// Optional base symbol.
    pub base: Option<String>,
    /// Optional quote symbol.
    pub quote: Option<String>,
    /// Freshness and answer validity policy.
    pub staleness: crate::StalenessPolicy,
    /// Best-known current aggregator.
    pub current_aggregator: Option<Address>,
    /// Best-known current aggregator layout evidence.
    pub aggregator_layout: Option<AggregatorLayoutEvidence>,
    /// Metadata loaded from the proxy.
    pub metadata: FeedMetadata,
    /// Network source and transform used by this registration.
    pub source: FeedSource,
    /// Feed/runtime readiness state.
    pub status: OracleFeedStatus,
}

/// Feed/runtime readiness surfaced by registry, warmup, build, repair, and health reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OracleFeedReadinessReport {
    /// Feed id when the feed reached registration or supplied an id before being skipped.
    pub id: Option<FeedId>,
    /// User-facing proxy/source address.
    pub proxy: Address,
    /// Feed/runtime readiness state.
    pub status: OracleFeedStatus,
    /// Optional machine-readable or human-readable diagnostic.
    pub reason: Option<String>,
}

/// Aggregator routing change found during reconciliation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AggregatorChange {
    /// Feed id whose aggregator changed.
    pub id: FeedId,
    /// Feed proxy.
    pub proxy: Address,
    /// Previous aggregator.
    pub old: Option<Address>,
    /// New aggregator.
    pub new: Option<Address>,
}

struct PreparedFeed {
    id: FeedId,
    config: FeedConfig,
    source: FeedSource,
    metadata: FeedMetadata,
    round: RoundData,
    current_aggregator: Option<Address>,
}

/// Registry plus latest proxy-read snapshots.
#[derive(Clone, Debug)]
pub struct OracleRegistry {
    registrations: BTreeMap<FeedId, FeedRegistration>,
    ids_by_proxy: BTreeMap<Address, FeedId>,
    snapshots_by_proxy: BTreeMap<Address, OracleSnapshot>,
    layouts_by_code_hash: BTreeMap<B256, AggregatorLayout>,
    now_timestamp: u64,
}

impl OracleRegistry {
    /// Create an empty registry with a fixed wall-clock timestamp for tests.
    pub fn new_at_timestamp(now_timestamp: u64) -> Self {
        Self {
            registrations: BTreeMap::new(),
            ids_by_proxy: BTreeMap::new(),
            snapshots_by_proxy: BTreeMap::new(),
            layouts_by_code_hash: BTreeMap::new(),
            now_timestamp,
        }
    }

    /// Register a Chainlink-compatible proxy and seed state from proxy reads.
    pub async fn register_chainlink_feed<P: ChainlinkFeedProvider>(
        &mut self,
        provider: &P,
        config: FeedConfig,
    ) -> Result<FeedId, OracleError> {
        let id = self.prepare_feed_id(&config)?;
        let source = FeedSource::Chainlink;
        let read_proxy = source.read_proxy(config.proxy);
        let metadata = FeedMetadata {
            decimals: provider.decimals(read_proxy).await?,
            description: provider.description(read_proxy).await?,
            version: provider.version(read_proxy).await?,
        };
        let round = source.normalize_round(provider.latest_round_data(read_proxy).await?);
        let current_aggregator = provider.aggregator(read_proxy).await.unwrap_or(None);
        self.insert_prepared_feed(
            provider,
            PreparedFeed {
                id,
                config,
                source,
                metadata,
                round,
                current_aggregator,
            },
        )
        .await
    }

    #[allow(dead_code)]
    pub(crate) async fn register_discovered_feed<P: ChainlinkFeedProvider>(
        &mut self,
        provider: &P,
        config: FeedConfig,
        source: FeedSource,
        metadata: FeedMetadata,
        round: RoundData,
        current_aggregator: Option<Address>,
    ) -> Result<FeedId, OracleError> {
        let id = self.prepare_feed_id(&config)?;
        self.insert_prepared_feed(
            provider,
            PreparedFeed {
                id,
                config,
                source,
                metadata,
                round,
                current_aggregator,
            },
        )
        .await
    }

    async fn insert_prepared_feed<P: ChainlinkFeedProvider>(
        &mut self,
        provider: &P,
        prepared: PreparedFeed,
    ) -> Result<FeedId, OracleError> {
        let PreparedFeed {
            id,
            config,
            source,
            metadata,
            round,
            current_aggregator,
        } = prepared;
        let aggregator_layout = self
            .detect_aggregator_layout(provider, current_aggregator, None)
            .await;

        let registration = FeedRegistration {
            id: id.clone(),
            proxy: config.proxy,
            label: config.label,
            base: config.base,
            quote: config.quote,
            staleness: config.staleness,
            current_aggregator,
            aggregator_layout,
            metadata: metadata.clone(),
            source,
            status: OracleFeedStatus::Ready,
        };
        let snapshot = OracleSnapshot::proxy_read(
            id.clone(),
            registration.proxy,
            current_aggregator,
            metadata,
            round,
            self.now_timestamp,
            &registration.staleness,
        );

        self.ids_by_proxy.insert(registration.proxy, id.clone());
        self.snapshots_by_proxy.insert(registration.proxy, snapshot);
        self.registrations.insert(id.clone(), registration);
        Ok(id)
    }

    fn prepare_feed_id(&self, config: &FeedConfig) -> Result<FeedId, OracleError> {
        if self.ids_by_proxy.contains_key(&config.proxy) {
            return Err(OracleError::DuplicateProxy(config.proxy));
        }

        let id = config
            .id
            .clone()
            .unwrap_or_else(|| derive_feed_id(config.label.as_deref(), config.proxy));
        if self.registrations.contains_key(&id) {
            return Err(OracleError::DuplicateFeedId(id.to_string()));
        }
        Ok(id)
    }

    /// Return a registration by id.
    pub fn registration(&self, id: FeedId) -> Option<&FeedRegistration> {
        self.registrations.get(&id)
    }

    /// Return feed/runtime readiness for all registered feeds.
    pub fn feed_readiness(&self) -> Vec<OracleFeedReadinessReport> {
        self.registrations
            .values()
            .map(|registration| OracleFeedReadinessReport {
                id: Some(registration.id.clone()),
                proxy: registration.proxy,
                status: registration.status,
                reason: None,
            })
            .collect()
    }

    /// Return the latest snapshot by proxy.
    pub fn latest(&self, proxy: Address) -> Option<&OracleSnapshot> {
        self.snapshots_by_proxy.get(&proxy)
    }

    pub(crate) fn now_timestamp(&self) -> u64 {
        self.now_timestamp
    }

    pub(crate) fn registrations_map(&self) -> &BTreeMap<FeedId, FeedRegistration> {
        &self.registrations
    }

    pub(crate) fn registrations_map_mut(&mut self) -> &mut BTreeMap<FeedId, FeedRegistration> {
        &mut self.registrations
    }

    pub(crate) fn snapshots_by_proxy_mut(&mut self) -> &mut BTreeMap<Address, OracleSnapshot> {
        &mut self.snapshots_by_proxy
    }

    pub(crate) fn id_by_proxy(&self, proxy: Address) -> Option<&FeedId> {
        self.ids_by_proxy.get(&proxy)
    }

    pub(crate) fn record_proxy_id(&mut self, proxy: Address, id: FeedId) {
        self.ids_by_proxy.insert(proxy, id);
    }

    pub(crate) fn replace_snapshot(&mut self, snapshot: OracleSnapshot) {
        self.snapshots_by_proxy.insert(snapshot.proxy, snapshot);
    }

    pub(crate) fn insert_seeded_registration(
        &mut self,
        registration: FeedRegistration,
        round: RoundData,
    ) -> Result<(), OracleError> {
        if self.ids_by_proxy.contains_key(&registration.proxy) {
            return Err(OracleError::DuplicateProxy(registration.proxy));
        }
        if self.registrations.contains_key(&registration.id) {
            return Err(OracleError::DuplicateFeedId(registration.id.to_string()));
        }

        let snapshot = OracleSnapshot::proxy_read(
            registration.id.clone(),
            registration.proxy,
            registration.current_aggregator,
            registration.metadata.clone(),
            registration.source.normalize_round(round),
            self.now_timestamp,
            &registration.staleness,
        );
        self.ids_by_proxy
            .insert(registration.proxy, registration.id.clone());
        self.snapshots_by_proxy.insert(registration.proxy, snapshot);
        self.registrations
            .insert(registration.id.clone(), registration);
        Ok(())
    }

    pub(crate) fn remove_registration_by_id(&mut self, id: FeedId) -> Option<FeedRegistration> {
        let registration = self.registrations.remove(&id)?;
        self.ids_by_proxy.remove(&registration.proxy);
        self.snapshots_by_proxy.remove(&registration.proxy);
        Some(registration)
    }

    pub(crate) fn remove_registration_by_proxy(
        &mut self,
        proxy: Address,
    ) -> Option<FeedRegistration> {
        let id = self.ids_by_proxy.get(&proxy)?.clone();
        self.remove_registration_by_id(id)
    }

    pub(crate) async fn detect_aggregator_layout<P: ChainlinkFeedProvider>(
        &mut self,
        provider: &P,
        aggregator: Option<Address>,
        block: Option<OracleBlockRef>,
    ) -> Option<AggregatorLayoutEvidence> {
        let aggregator = aggregator?;
        let code_hash = provider
            .aggregator_code_hash_at(aggregator, block)
            .await
            .unwrap_or(None);
        let type_and_version = provider
            .aggregator_type_and_version_at(aggregator, block)
            .await
            .unwrap_or(None);

        if let Some(type_and_version) = type_and_version {
            let layout = classify_type_and_version(&type_and_version);
            if let Some(code_hash) = code_hash
                && is_cacheable_layout(layout)
            {
                self.layouts_by_code_hash.insert(code_hash, layout);
            }
            return Some(AggregatorLayoutEvidence {
                aggregator,
                type_and_version: Some(type_and_version),
                code_hash,
                layout,
                confidence: AggregatorLayoutConfidence::TypeAndVersion,
            });
        }

        if let Some(code_hash) = code_hash {
            if let Some(layout) = self.layouts_by_code_hash.get(&code_hash).copied() {
                return Some(AggregatorLayoutEvidence::from_code_hash_cache(
                    aggregator, code_hash, layout,
                ));
            }
            return Some(AggregatorLayoutEvidence::unknown(
                aggregator,
                Some(code_hash),
            ));
        }

        Some(AggregatorLayoutEvidence::unknown(aggregator, None))
    }

    /// Return cloned registrations for routing rebuilds.
    ///
    /// Prefer [`Self::registrations_iter`] when read-only access is enough —
    /// this iterator clones every registration it yields.
    pub fn registrations(&self) -> impl Iterator<Item = FeedRegistration> + '_ {
        self.registrations_iter().cloned()
    }

    /// Iterate over registrations without cloning, in feed-id order.
    pub fn registrations_iter(&self) -> impl Iterator<Item = &FeedRegistration> {
        self.registrations.values()
    }
}

fn is_cacheable_layout(layout: AggregatorLayout) -> bool {
    matches!(
        layout,
        AggregatorLayout::ChainlinkOcr2V1
            | AggregatorLayout::ChainlinkOcr1V2
            | AggregatorLayout::ChainlinkOcr1V3
            | AggregatorLayout::ChainlinkOcr1V4
    )
}

pub(crate) fn snapshot_from_proxy_read(
    registration: &FeedRegistration,
    round: RoundData,
    aggregator: Option<Address>,
    now_timestamp: u64,
) -> OracleSnapshot {
    OracleSnapshot::proxy_read(
        registration.id.clone(),
        registration.proxy,
        aggregator,
        registration.metadata.clone(),
        round,
        now_timestamp,
        &registration.staleness,
    )
}

pub(crate) fn derive_feed_id(label: Option<&str>, proxy: Address) -> FeedId {
    if let Some(label) = label {
        let slug = label
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() {
                    ch.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect::<String>()
            .trim_matches('-')
            .to_string();
        if !slug.is_empty() {
            return FeedId::new(slug);
        }
    }

    FeedId::new(format!("{proxy:?}"))
}

#[allow(dead_code)]
fn _u256_is_part_of_public_metadata(_: U256) {}
