//! Typed per-batch change digest for consumers.
//!
//! [`OracleBatchReport`] is the oracle analog of `evm-amm-state`'s
//! `AmmSyncBatchReport`: alongside the raw per-event hooks it carries a
//! digested per-feed change list ([`OracleFeedChange`]), typed continuity
//! incidents ([`OracleIncident`]), and a `requires_full_refresh` flag, so the
//! two crates express "what changed in this batch, and what should I do about
//! it" with the same shape:
//!
//! | evm-amm-state                 | evm-oracle-state          |
//! | ----------------------------- | ------------------------- |
//! | `AmmSyncBatchReport`          | [`OracleBatchReport`]     |
//! | `AmmSyncPoolChange`           | [`OracleFeedChange`]      |
//! | `AmmChangeImpact`             | [`OracleChangeImpact`]    |
//! | `AmmSyncIncident`             | [`OracleIncident`]        |
//! | `{state, quoteability, topology}` | `{value, actionability, routing}` |

use alloy_primitives::Address;

use crate::{FeedId, OracleHookEvent, OracleValueStatus};

/// Typed digest of one committed reactive batch.
///
/// Returned by [`crate::OracleRuntime::apply_batch_report`]. `events` is the
/// raw per-event hook stream (unchanged from earlier releases); the remaining
/// fields digest it per feed for consumers that want "what changed" without
/// re-deriving it from individual hooks.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OracleBatchReport {
    /// Raw typed hook events, in emission order.
    pub events: Vec<OracleHookEvent>,
    /// Digested per-feed changes, in event order.
    pub feed_changes: Vec<OracleFeedChange>,
    /// Continuity incidents observed while applying the batch.
    pub incidents: Vec<OracleIncident>,
    /// True when a routing-impact change occurred (an aggregator changed):
    /// rebuild reactive handlers/interests (for example through
    /// [`crate::OracleRuntime::refresh_handlers_live_only`]) before relying on
    /// event routing again.
    pub requires_full_refresh: bool,
}

impl OracleBatchReport {
    /// Digest a batch's typed hook events into per-feed changes, incidents,
    /// and the refresh flag.
    pub fn from_events(events: Vec<OracleHookEvent>) -> Self {
        let mut feed_changes = Vec::with_capacity(events.len());
        let mut incidents = Vec::new();
        let mut requires_full_refresh = false;
        for event in &events {
            match event {
                OracleHookEvent::PriceUpdate(update) => {
                    let repair_required = matches!(
                        update.value_status,
                        OracleValueStatus::RequiresRepair | OracleValueStatus::Unknown
                    );
                    if repair_required {
                        incidents.push(OracleIncident::Reorg {
                            feed: update.id.clone(),
                            proxy: update.proxy,
                        });
                        feed_changes.push(OracleFeedChange {
                            feed: update.id.clone(),
                            proxy: update.proxy,
                            kind: OracleFeedChangeKind::Degraded,
                            value_status: Some(update.value_status),
                            impact: OracleChangeImpact {
                                value: false,
                                actionability: true,
                                routing: false,
                            },
                        });
                    } else {
                        feed_changes.push(OracleFeedChange {
                            feed: update.id.clone(),
                            proxy: update.proxy,
                            kind: OracleFeedChangeKind::Updated,
                            value_status: Some(update.value_status),
                            impact: OracleChangeImpact {
                                value: true,
                                actionability: true,
                                routing: false,
                            },
                        });
                    }
                }
                OracleHookEvent::PriceConfirmed(confirmed) => {
                    feed_changes.push(OracleFeedChange {
                        feed: confirmed.id.clone(),
                        proxy: confirmed.proxy,
                        kind: OracleFeedChangeKind::Confirmed,
                        value_status: Some(confirmed.value_status),
                        impact: OracleChangeImpact {
                            value: false,
                            actionability: true,
                            routing: false,
                        },
                    });
                }
                OracleHookEvent::PriceCorrected(corrected) => {
                    feed_changes.push(OracleFeedChange {
                        feed: corrected.id.clone(),
                        proxy: corrected.proxy,
                        kind: OracleFeedChangeKind::Corrected,
                        value_status: Some(corrected.value_status),
                        impact: OracleChangeImpact {
                            value: true,
                            actionability: true,
                            routing: false,
                        },
                    });
                }
                OracleHookEvent::PriceStale(stale) => {
                    feed_changes.push(OracleFeedChange {
                        feed: stale.id.clone(),
                        proxy: stale.proxy,
                        kind: OracleFeedChangeKind::Degraded,
                        value_status: Some(stale.value_status),
                        impact: OracleChangeImpact {
                            value: false,
                            actionability: true,
                            routing: false,
                        },
                    });
                }
                OracleHookEvent::AggregatorChanged(change) => {
                    requires_full_refresh = true;
                    feed_changes.push(OracleFeedChange {
                        feed: change.id.clone(),
                        proxy: change.proxy,
                        kind: OracleFeedChangeKind::AggregatorChanged,
                        value_status: None,
                        impact: OracleChangeImpact {
                            value: false,
                            actionability: false,
                            routing: true,
                        },
                    });
                }
            }
        }
        Self {
            events,
            feed_changes,
            incidents,
            requires_full_refresh,
        }
    }

    /// True when the batch produced no typed oracle activity at all.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

/// One digested per-feed change inside an [`OracleBatchReport`].
///
/// Shape-compatible with `evm-amm-state`'s `AmmSyncPoolChange`
/// (`{pool, kind, impact, source}` there; `{feed, kind, value_status, impact}`
/// here).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OracleFeedChange {
    /// Stable feed id.
    pub feed: FeedId,
    /// Feed state key (registration proxy).
    pub proxy: Address,
    /// What happened to the feed in this batch.
    pub kind: OracleFeedChangeKind,
    /// Resulting value lifecycle, when the change carries one
    /// ([`OracleFeedChangeKind::AggregatorChanged`] does not).
    pub value_status: Option<OracleValueStatus>,
    /// Downstream consequences of this change.
    pub impact: OracleChangeImpact,
}

/// Classified per-feed change kind.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OracleFeedChangeKind {
    /// An event updated the feed's typed value (usually `EventPending`).
    Updated,
    /// Reconciliation confirmed the event value.
    Confirmed,
    /// Reconciliation replaced the event value with the authoritative one.
    Corrected,
    /// The feed's value degraded: stale under policy, or marked
    /// repair-required (for example by a removed log).
    Degraded,
    /// The proxy now reports a different aggregator; event routing must be
    /// rebuilt.
    AggregatorChanged,
}

/// Downstream consequences of a feed change, mirroring `evm-amm-state`'s
/// `AmmChangeImpact { state, quoteability, topology }`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct OracleChangeImpact {
    /// The typed value changed (analog of amm `state`).
    pub value: bool,
    /// Policy acceptance of the value may have changed (analog of amm
    /// `quoteability`).
    pub actionability: bool,
    /// Event routing/interests must be rebuilt (analog of amm `topology`).
    pub routing: bool,
}

/// Continuity incident observed while applying a batch.
///
/// Mirrors `evm-amm-state`'s `AmmSyncIncident` vocabulary. The enum is
/// `#[non_exhaustive]`: gap/coverage-gap incidents are reserved for when the
/// runtime can observe them.
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OracleIncident {
    /// A removed (reorged-out) log rolled back event state; the feed's
    /// snapshot is repair-required until reconciliation.
    Reorg {
        /// Affected feed id.
        feed: FeedId,
        /// Affected feed state key.
        proxy: Address,
    },
}

#[cfg(test)]
mod tests {
    use alloy_primitives::{I256, U256};

    use super::*;
    use crate::{AggregatorChange, OraclePriceUpdate, OracleRoundStatus, OracleValueSource};

    fn price_update(id: &str, value_status: OracleValueStatus) -> OracleHookEvent {
        OracleHookEvent::PriceUpdate(OraclePriceUpdate {
            id: FeedId::new(id),
            proxy: Address::repeat_byte(0x11),
            aggregator: Address::repeat_byte(0x22),
            label: None,
            base: None,
            quote: None,
            raw_answer: I256::unchecked_from(42_i64),
            decimals: 8,
            event_round_id: U256::from(7_u64),
            started_at: 1_700_000_000,
            updated_at: 1_700_000_000,
            block_number: Some(1),
            block_hash: None,
            log_index: Some(0),
            round_status: OracleRoundStatus::Fresh,
            value_status,
            source: OracleValueSource::Event,
        })
    }

    #[test]
    fn digest_classifies_kinds_incidents_and_refresh() {
        let events = vec![
            price_update("fresh", OracleValueStatus::EventPending),
            price_update("rolled-back", OracleValueStatus::RequiresRepair),
            OracleHookEvent::AggregatorChanged(AggregatorChange {
                id: FeedId::new("rotated"),
                proxy: Address::repeat_byte(0x33),
                old: Some(Address::repeat_byte(0x44)),
                new: Some(Address::repeat_byte(0x55)),
            }),
        ];

        let report = OracleBatchReport::from_events(events);

        assert_eq!(report.events.len(), 3);
        assert_eq!(report.feed_changes.len(), 3);
        assert_eq!(report.feed_changes[0].kind, OracleFeedChangeKind::Updated);
        assert!(report.feed_changes[0].impact.value);
        assert!(!report.feed_changes[0].impact.routing);
        assert_eq!(report.feed_changes[1].kind, OracleFeedChangeKind::Degraded);
        assert!(!report.feed_changes[1].impact.value);
        assert_eq!(
            report.feed_changes[2].kind,
            OracleFeedChangeKind::AggregatorChanged
        );
        assert!(report.feed_changes[2].impact.routing);
        assert_eq!(report.feed_changes[2].value_status, None);
        assert_eq!(report.incidents.len(), 1);
        assert!(matches!(
            &report.incidents[0],
            OracleIncident::Reorg { feed, .. } if feed.as_str() == "rolled-back"
        ));
        assert!(
            report.requires_full_refresh,
            "aggregator changes require handler refresh"
        );
        assert!(!report.is_empty());
        assert!(OracleBatchReport::from_events(Vec::new()).is_empty());
    }
}
