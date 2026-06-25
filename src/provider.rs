use std::{future::Future, pin::Pin};

use alloy_primitives::{Address, B256, U256};

use crate::{OracleError, RoundData};

/// Boxed future used by the provider seam.
pub type ProviderFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, OracleError>> + Send + 'a>>;

/// Block identity for hash-pinned oracle reconciliation reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OracleBlockRef {
    /// Block number.
    pub number: u64,
    /// Block hash.
    pub hash: B256,
}

/// Minimal read seam for Chainlink-compatible proxy feeds.
///
/// The trait is intentionally small so tests and downstream systems can provide
/// their own provider, cache, or batched multicall implementation.
pub trait ChainlinkFeedProvider {
    /// Read `decimals()` from the proxy.
    fn decimals(&self, proxy: Address) -> ProviderFuture<'_, u8>;

    /// Read `description()` from the proxy.
    fn description(&self, proxy: Address) -> ProviderFuture<'_, String>;

    /// Read `version()` from the proxy.
    fn version(&self, proxy: Address) -> ProviderFuture<'_, U256>;

    /// Read `latestRoundData()` from the proxy.
    fn latest_round_data(&self, proxy: Address) -> ProviderFuture<'_, RoundData>;

    /// Read `latestRoundData()` from the proxy at a specific block, when supported.
    ///
    /// Providers that cannot pin by block may fall back to the unpinned read.
    fn latest_round_data_at(
        &self,
        proxy: Address,
        _block: Option<OracleBlockRef>,
    ) -> ProviderFuture<'_, RoundData> {
        self.latest_round_data(proxy)
    }

    /// Best-effort read of the current underlying aggregator address.
    fn aggregator(&self, proxy: Address) -> ProviderFuture<'_, Option<Address>>;

    /// Best-effort read of the underlying aggregator address at a specific block.
    ///
    /// Providers that cannot pin by block may fall back to the unpinned read.
    fn aggregator_at(
        &self,
        proxy: Address,
        _block: Option<OracleBlockRef>,
    ) -> ProviderFuture<'_, Option<Address>> {
        self.aggregator(proxy)
    }

    /// Best-effort read of the aggregator implementation `typeAndVersion()`.
    ///
    /// Providers that do not support implementation introspection may return
    /// `Ok(None)`. Registration and reconciliation treat missing or failed
    /// introspection as an unsupported layout and fall back to purge/refetch.
    fn aggregator_type_and_version(
        &self,
        _aggregator: Address,
    ) -> ProviderFuture<'_, Option<String>> {
        Box::pin(async { Ok(None) })
    }

    /// Best-effort block-pinned read of the aggregator `typeAndVersion()`.
    fn aggregator_type_and_version_at(
        &self,
        aggregator: Address,
        _block: Option<OracleBlockRef>,
    ) -> ProviderFuture<'_, Option<String>> {
        self.aggregator_type_and_version(aggregator)
    }

    /// Best-effort read of the aggregator runtime code hash.
    fn aggregator_code_hash(&self, _aggregator: Address) -> ProviderFuture<'_, Option<B256>> {
        Box::pin(async { Ok(None) })
    }

    /// Best-effort block-pinned read of the aggregator runtime code hash.
    fn aggregator_code_hash_at(
        &self,
        aggregator: Address,
        _block: Option<OracleBlockRef>,
    ) -> ProviderFuture<'_, Option<B256>> {
        self.aggregator_code_hash(aggregator)
    }
}
