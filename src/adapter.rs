use std::{borrow::Cow, collections::BTreeMap, future::Future, pin::Pin, sync::Arc};

use alloy_network::Ethereum;
use alloy_primitives::{Address, I256};
use evm_fork_cache::{cache::EvmCache, reactive::ReactiveHandler};

use crate::{FeedRegistration, OracleAdapterFeedSkip, OracleError, OracleStorageSync, RoundData};

/// Boxed async result returned by oracle adapter extension points.
pub type AdapterFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, OracleError>> + Send + 'a>>;

/// Stable identifier for an oracle adapter family.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OracleAdapterId(Cow<'static, str>);

impl OracleAdapterId {
    /// Create a stable adapter id.
    pub fn new(id: impl Into<Cow<'static, str>>) -> Self {
        Self(id.into())
    }

    /// Borrow the adapter id string.
    pub fn as_str(&self) -> &str {
        self.0.as_ref()
    }
}

impl std::fmt::Display for OracleAdapterId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Open transform descriptor for non-Chainlink-native source adapters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OracleTransformDescriptor {
    /// Event/read answers are already in the feed's user-facing denomination.
    Identity,
    /// Clamp answers above a configured cap.
    PriceCap {
        /// Maximum answer exposed to users.
        cap: I256,
    },
    /// Adapter-owned transform identified by adapter and transform kind.
    Custom {
        /// Adapter responsible for interpreting this transform.
        adapter_id: OracleAdapterId,
        /// Adapter-local transform kind.
        kind: Cow<'static, str>,
        /// String metadata for adapter-owned transform parameters.
        metadata: BTreeMap<String, String>,
    },
}

impl Default for OracleTransformDescriptor {
    fn default() -> Self {
        Self::Identity
    }
}

/// Open descriptor for an adapter-owned oracle source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OracleSourceDescriptor {
    /// Adapter that owns source-specific semantics.
    pub adapter_id: OracleAdapterId,
    /// Adapter-local source kind.
    pub source_kind: Cow<'static, str>,
    /// User-facing oracle/proxy address.
    pub user_facing_proxy: Address,
    /// Address to read for authoritative round data, when applicable.
    pub read_proxy: Address,
    /// Address whose events should drive this feed.
    pub event_source: Address,
    /// User-facing answer transform.
    pub transform: OracleTransformDescriptor,
    /// String metadata for adapter-owned source parameters.
    pub metadata: BTreeMap<String, String>,
}

/// One feed discovered and seeded by an oracle adapter plugin.
#[derive(Clone, Debug)]
pub struct OracleDiscoveredFeed {
    /// Registration inserted into the oracle tracker.
    pub registration: FeedRegistration,
    /// Initial round used to seed typed state.
    pub round: RoundData,
}

/// Result of adapter discovery.
#[derive(Clone, Debug, Default)]
pub struct OracleDiscoveryReport {
    /// Feeds that were registered successfully.
    pub feeds: Vec<OracleDiscoveredFeed>,
    /// Optional feed skips reported by best-effort adapters.
    pub skipped: Vec<OracleAdapterFeedSkip>,
}

/// Cache-native discovery context passed to oracle adapter plugins.
pub struct OracleDiscoveryContext<'a> {
    /// Cache used for all on-chain reads during discovery.
    pub cache: &'a mut EvmCache,
    /// Wall-clock timestamp used to classify seeded feed state.
    pub now_timestamp: u64,
}

/// Public plugin interface for oracle families layered over `evm-fork-cache`.
pub trait OracleAdapterPlugin: Send + Sync {
    /// Stable adapter id.
    fn adapter_id(&self) -> OracleAdapterId;

    /// Discover and seed feed registrations through an `EvmCache`.
    fn discover<'a>(
        &'a self,
        ctx: OracleDiscoveryContext<'a>,
    ) -> AdapterFuture<'a, OracleDiscoveryReport>;

    /// Build the reactive handler that owns this adapter's event routing.
    fn reactive_handler(
        &self,
        registrations: Vec<FeedRegistration>,
        storage_sync: OracleStorageSync,
    ) -> Arc<dyn ReactiveHandler<Ethereum>>;
}
