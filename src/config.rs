use alloy_primitives::Address;

use crate::FeedId;

/// Feed registration configuration.
#[derive(Clone, Debug)]
pub struct FeedConfig {
    /// User-facing proxy address.
    pub proxy: Address,
    /// Optional stable id. If omitted, one is derived from the label or proxy.
    pub id: Option<FeedId>,
    /// Optional human-readable label.
    pub label: Option<String>,
    /// Optional base symbol.
    pub base: Option<String>,
    /// Optional quote symbol.
    pub quote: Option<String>,
    /// Staleness and answer validity policy.
    pub staleness: StalenessPolicy,
}

impl Default for FeedConfig {
    fn default() -> Self {
        Self {
            proxy: Address::ZERO,
            id: None,
            label: None,
            base: None,
            quote: None,
            staleness: StalenessPolicy::default(),
        }
    }
}

/// Policy used to classify round freshness and answer validity.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StalenessPolicy {
    max_age_secs: Option<u64>,
    allow_zero_or_negative_answer: bool,
}

impl StalenessPolicy {
    /// Create a policy with a max age.
    pub fn max_age(max_age_secs: u64) -> Self {
        Self {
            max_age_secs: Some(max_age_secs),
            allow_zero_or_negative_answer: false,
        }
    }

    /// Allow zero or negative answers for generic signed numeric feeds.
    pub fn allow_zero_or_negative_answer(mut self, allow: bool) -> Self {
        self.allow_zero_or_negative_answer = allow;
        self
    }

    pub(crate) fn max_age_secs(&self) -> Option<u64> {
        self.max_age_secs
    }

    pub(crate) fn allows_zero_or_negative_answer(&self) -> bool {
        self.allow_zero_or_negative_answer
    }
}
