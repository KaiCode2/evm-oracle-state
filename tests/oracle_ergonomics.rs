use alloy_primitives::{Address, I256, U256};
use evm_oracle_state::{
    AssetId, CheckedPrice, Denomination, Feed, FeedConfig, FeedId, FeedMetadata, FeedRegistration,
    FeedSource, OracleFeedStatus, OraclePrice, OracleRoundStatus, OracleRuntime, OracleTracker,
    OracleValueSource, OracleValueStatus, PricePolicy, RoundData, StalenessPolicy, TokenAmount,
};

fn metadata() -> FeedMetadata {
    FeedMetadata {
        decimals: 8,
        description: "ETH / USD".to_string(),
        version: U256::from(4_u64),
    }
}

fn round(id: u64, answer: i128, updated_at: u64) -> RoundData {
    RoundData {
        round_id: U256::from(id),
        answer: I256::unchecked_from(answer),
        started_at: updated_at.saturating_sub(60),
        updated_at,
        answered_in_round: U256::from(id),
    }
}

fn registration(proxy: Address, aggregator: Address) -> FeedRegistration {
    FeedRegistration {
        id: FeedId::new("eth-usd"),
        proxy,
        label: Some("ETH/USD".to_string()),
        base: Some("ETH".to_string()),
        quote: Some("USD".to_string()),
        staleness: StalenessPolicy::max_age(3_600),
        current_aggregator: Some(aggregator),
        aggregator_layout: None,
        metadata: metadata(),
        source: FeedSource::Chainlink,
        status: OracleFeedStatus::Ready,
    }
}

fn price_with(
    round_status: OracleRoundStatus,
    value_status: OracleValueStatus,
    source: OracleValueSource,
    answer: i128,
) -> OraclePrice {
    let proxy = Address::repeat_byte(0x10);
    let aggregator = Address::repeat_byte(0x20);
    let round = round(101, answer, 1_700_000_060);
    OraclePrice {
        id: FeedId::new("eth-usd"),
        proxy,
        aggregator: Some(aggregator),
        label: Some("ETH/USD".to_string()),
        base: Some("ETH".to_string()),
        quote: Some("USD".to_string()),
        raw_answer: round.answer,
        decimals: 8,
        round_id: round.round_id,
        updated_at: round.updated_at,
        round,
        round_status,
        value_status,
        source,
    }
}

#[test]
fn proxy_feed_builder_preserves_metadata_and_feed_config_compatibility() {
    let proxy = Address::repeat_byte(0x10);

    let feed = Feed::proxy(proxy)
        .id("eth-usd")
        .label("ETH/USD")
        .base(AssetId::symbol("ETH"))
        .quote(Denomination::Usd)
        .max_age_secs(3_600);

    assert_eq!(feed.proxy(), Some(proxy));
    assert_eq!(feed.id().expect("id").as_str(), "eth-usd");
    assert_eq!(feed.base(), Some(&AssetId::symbol("ETH")));
    assert_eq!(feed.quote(), Some(&Denomination::Usd));

    let config: FeedConfig = feed
        .clone()
        .try_into()
        .expect("proxy feed should remain compatible with FeedConfig");
    assert_eq!(config.proxy, proxy);
    assert_eq!(config.id.expect("config id").as_str(), "eth-usd");
    assert_eq!(config.label.as_deref(), Some("ETH/USD"));
    assert_eq!(config.base.as_deref(), Some("ETH"));
    assert_eq!(config.quote.as_deref(), Some("USD"));
}

#[test]
fn price_policy_promotes_only_matching_fresh_prices_to_checked_prices() {
    let proxy = Address::repeat_byte(0x10);
    let aggregator = Address::repeat_byte(0x20);
    let tracker = OracleTracker::from_registrations_at_timestamp(
        vec![(
            registration(proxy, aggregator),
            round(100, 200_000_000_000_i128, 1_700_000_000),
        )],
        1_700_000_100,
    )
    .expect("seed tracker");

    let price = tracker.price("eth-usd").expect("price");
    let checked: CheckedPrice = price
        .require(PricePolicy::liquidation().quote(Denomination::Usd))
        .expect("fresh confirmed USD price should satisfy liquidation policy");

    assert_eq!(checked.base(), &AssetId::symbol("ETH"));
    assert_eq!(checked.quote(), &Denomination::Usd);
    assert_eq!(
        checked.raw_answer(),
        I256::unchecked_from(200_000_000_000_i128)
    );

    let error = price
        .require(PricePolicy::liquidation().quote(Denomination::Eth))
        .expect_err("USD feed should not satisfy an ETH quote policy");
    assert!(
        error.to_string().contains("quote"),
        "wrong-quote error should be explicit: {error}"
    );
}

#[test]
fn checked_price_values_token_amounts_in_the_requested_denomination() {
    let proxy = Address::repeat_byte(0x10);
    let aggregator = Address::repeat_byte(0x20);
    let tracker = OracleTracker::from_registrations_at_timestamp(
        vec![(
            registration(proxy, aggregator),
            round(100, 200_000_000_000_i128, 1_700_000_000),
        )],
        1_700_000_100,
    )
    .expect("seed tracker");
    let price = tracker
        .price("eth-usd")
        .expect("price")
        .require(PricePolicy::liquidation().quote(Denomination::Usd))
        .expect("checked price");

    let two_eth = TokenAmount::new(
        AssetId::symbol("ETH"),
        U256::from(2_000_000_000_000_000_000_u128),
        18,
    );
    let value = price.value_of(two_eth, 18).expect("value two ETH in USD");

    assert_eq!(value.denomination(), &Denomination::Usd);
    assert_eq!(value.decimals(), 18);
    assert_eq!(
        value.raw(),
        I256::unchecked_from(4_000_000_000_000_000_000_000_i128)
    );
}

#[test]
fn stale_prices_do_not_satisfy_liquidation_policy() {
    let proxy = Address::repeat_byte(0x10);
    let aggregator = Address::repeat_byte(0x20);
    let tracker = OracleTracker::from_registrations_at_timestamp(
        vec![(
            registration(proxy, aggregator),
            round(100, 200_000_000_000_i128, 1_700_000_000),
        )],
        1_700_004_000,
    )
    .expect("seed tracker");

    let price = tracker.price("eth-usd").expect("price");
    let error = price
        .require(PricePolicy::liquidation().quote(Denomination::Usd))
        .expect_err("stale price should not satisfy liquidation policy");
    assert!(
        error.to_string().contains("stale") || error.to_string().contains("Fresh"),
        "stale-policy error should explain freshness: {error}"
    );
}

#[test]
fn event_pending_prices_require_explicit_policy_opt_in() {
    let pending = price_with(
        OracleRoundStatus::Fresh,
        OracleValueStatus::EventPending,
        OracleValueSource::Event,
        201_000_000_000_i128,
    );

    let default_error = pending
        .require(PricePolicy::liquidation().quote(Denomination::Usd))
        .expect_err("default liquidation policy should reject event-pending prices");
    assert!(
        default_error.to_string().contains("pending"),
        "default rejection should explain pending value status: {default_error}"
    );

    let checked = pending
        .require(
            PricePolicy::liquidation()
                .quote(Denomination::Usd)
                .allow_event_pending(),
        )
        .expect("explicit policy should allow event-pending prices");
    assert_eq!(
        checked.raw_answer(),
        I256::unchecked_from(201_000_000_000_i128)
    );
}

#[test]
fn stale_authoritative_proxy_read_rejects_by_round_status() {
    let stale = price_with(
        OracleRoundStatus::Stale {
            age_secs: 3_900,
            max_age_secs: 3_600,
        },
        OracleValueStatus::Confirmed,
        OracleValueSource::Proxy,
        201_000_000_000_i128,
    );

    assert_eq!(stale.value_status, OracleValueStatus::Confirmed);
    assert_eq!(stale.source, OracleValueSource::Proxy);
    let error = stale
        .require(PricePolicy::liquidation().quote(Denomination::Usd))
        .expect_err("stale authoritative value should be rejected before lifecycle/source checks");
    assert!(
        error.to_string().contains("stale"),
        "round-status rejection should explain staleness: {error}"
    );
}

#[test]
fn corrected_event_is_authoritative_proxy_value_with_independent_round_status() {
    let corrected = price_with(
        OracleRoundStatus::Fresh,
        OracleValueStatus::Corrected,
        OracleValueSource::Proxy,
        201_000_000_000_i128,
    );

    let checked = corrected
        .require(PricePolicy::liquidation().quote(Denomination::Usd))
        .expect("fresh corrected proxy value should satisfy default liquidation policy");
    assert_eq!(
        checked.raw_answer(),
        I256::unchecked_from(201_000_000_000_i128)
    );
}

#[test]
fn invalid_authoritative_answer_rejects_by_round_status() {
    let invalid = price_with(
        OracleRoundStatus::InvalidAnswer,
        OracleValueStatus::Confirmed,
        OracleValueSource::Proxy,
        0,
    );

    let error = invalid
        .require(PricePolicy::liquidation().quote(Denomination::Usd))
        .expect_err("invalid authoritative round should be rejected");
    assert!(
        error.to_string().contains("invalid"),
        "round-status rejection should explain invalid answer: {error}"
    );
}

#[test]
fn requires_repair_diagnostic_values_are_rejected_by_default() {
    let diagnostic = price_with(
        OracleRoundStatus::Fresh,
        OracleValueStatus::RequiresRepair,
        OracleValueSource::Proxy,
        201_000_000_000_i128,
    );

    let error = diagnostic
        .require(PricePolicy::liquidation().quote(Denomination::Usd))
        .expect_err("repair-required diagnostic value should be rejected");
    assert!(
        error.to_string().contains("repair"),
        "lifecycle rejection should explain repair requirement: {error}"
    );
}

#[test]
fn mock_and_derived_sources_require_explicit_policy_opt_in() {
    for (source, policy) in [
        (
            OracleValueSource::Mock,
            PricePolicy::liquidation()
                .quote(Denomination::Usd)
                .allow_mock_source(),
        ),
        (
            OracleValueSource::Derived,
            PricePolicy::liquidation()
                .quote(Denomination::Usd)
                .allow_derived_source(),
        ),
    ] {
        let price = price_with(
            OracleRoundStatus::Fresh,
            OracleValueStatus::Confirmed,
            source,
            201_000_000_000_i128,
        );
        price
            .require(PricePolicy::liquidation().quote(Denomination::Usd))
            .expect_err("default policy should reject non-proxy sources");
        price
            .require(policy)
            .expect("explicit policy should allow source");
    }
}

#[test]
fn cache_native_tracker_can_build_reactive_runtime_facade() {
    let proxy = Address::repeat_byte(0x10);
    let aggregator = Address::repeat_byte(0x20);
    let tracker = OracleTracker::from_registrations_at_timestamp(
        vec![(
            registration(proxy, aggregator),
            round(100, 200_000_000_000_i128, 1_700_000_000),
        )],
        1_700_000_100,
    )
    .expect("seed tracker");

    let oracle = OracleRuntime::from_tracker(tracker);
    let readiness = oracle.feed_readiness();
    assert_eq!(readiness.len(), 1);
    assert_eq!(readiness[0].status, OracleFeedStatus::Ready);
    let reactive = oracle
        .reactive_runtime()
        .expect("reactive handler should install");

    assert_eq!(reactive.interests().len(), 1);
}
