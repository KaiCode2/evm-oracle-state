use std::{convert::TryFrom, marker::PhantomData, ops::Deref};

use alloy_primitives::Address;

use crate::{AssetId, Denomination, FeedConfig, FeedId, OracleError, StalenessPolicy};

/// Marker for finalized typed feed registrations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedReady;

/// Marker for builder-stage typed feed registrations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedBuilderState;

/// Builder-stage typed feed registration.
pub type FeedBuilder = Feed<FeedBuilderState>;

/// Typed feed registration primitive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Feed<State = FeedReady> {
    access: FeedAccess,
    _state: PhantomData<State>,
}

/// Read-only accessors for finalized typed feed registrations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedAccess {
    inner: FeedInner,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FeedInner {
    kind: FeedKind,
    id: Option<FeedId>,
    label: Option<String>,
    base: Option<AssetId>,
    quote: Option<Denomination>,
    staleness: StalenessPolicy,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum FeedKind {
    Proxy { proxy: Address },
}

impl Feed<FeedBuilderState> {
    /// Register a Chainlink-compatible proxy feed.
    pub fn proxy(proxy: Address) -> Self {
        Self {
            access: FeedAccess {
                inner: FeedInner {
                    kind: FeedKind::Proxy { proxy },
                    id: None,
                    label: None,
                    base: None,
                    quote: None,
                    staleness: StalenessPolicy::default(),
                },
            },
            _state: PhantomData,
        }
    }

    /// Set a stable feed id.
    pub fn id(mut self, id: impl Into<String>) -> Self {
        self.access.inner.id = Some(FeedId::new(id));
        self
    }

    /// Set a stable feed id.
    pub fn feed_id(mut self, id: FeedId) -> Self {
        self.access.inner.id = Some(id);
        self
    }

    /// Set a human-readable label.
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.access.inner.label = Some(label.into());
        self
    }

    /// Set the base asset.
    pub fn base(mut self, base: AssetId) -> Self {
        self.access.inner.base = Some(base);
        self
    }

    /// Set the quote denomination.
    pub fn quote(mut self, quote: Denomination) -> Self {
        self.access.inner.quote = Some(quote);
        self
    }

    /// Set a max-age staleness policy and finalize this feed.
    pub fn max_age_secs(mut self, max_age_secs: u64) -> Feed {
        self.access.inner.staleness = StalenessPolicy::max_age(max_age_secs);
        self.build()
    }

    /// Set the full staleness policy and finalize this feed.
    pub fn staleness(mut self, staleness: StalenessPolicy) -> Feed {
        self.access.inner.staleness = staleness;
        self.build()
    }

    /// Finalize this feed with its current settings.
    pub fn build(self) -> Feed {
        Feed {
            access: self.access,
            _state: PhantomData,
        }
    }
}

impl Deref for Feed {
    type Target = FeedAccess;

    fn deref(&self) -> &Self::Target {
        &self.access
    }
}

impl FeedAccess {
    /// Return the proxy address for proxy feeds.
    pub fn proxy(&self) -> Option<Address> {
        match self.inner.kind {
            FeedKind::Proxy { proxy } => Some(proxy),
        }
    }

    /// Return the configured stable feed id.
    pub fn id(&self) -> Option<FeedId> {
        self.inner.id.clone()
    }

    /// Return the configured base asset.
    pub fn base(&self) -> Option<&AssetId> {
        self.inner.base.as_ref()
    }

    /// Return the configured quote denomination.
    pub fn quote(&self) -> Option<&Denomination> {
        self.inner.quote.as_ref()
    }

    /// Return the configured label.
    pub fn label(&self) -> Option<&str> {
        self.inner.label.as_deref()
    }
}

impl TryFrom<Feed> for FeedConfig {
    type Error = OracleError;

    fn try_from(feed: Feed) -> Result<Self, Self::Error> {
        feed_config_from_inner(feed.access.inner)
    }
}

impl TryFrom<FeedBuilder> for FeedConfig {
    type Error = OracleError;

    fn try_from(feed: FeedBuilder) -> Result<Self, Self::Error> {
        feed_config_from_inner(feed.access.inner)
    }
}

fn feed_config_from_inner(feed: FeedInner) -> Result<FeedConfig, OracleError> {
    let FeedKind::Proxy { proxy } = feed.kind;
    Ok(FeedConfig {
        proxy,
        id: feed.id,
        label: feed.label,
        base: feed.base.map(String::from),
        quote: feed.quote.map(String::from),
        staleness: feed.staleness,
    })
}
