use std::future::Future;
use std::pin::Pin;

use alloy_primitives::{Address, B256, I256, U256};
use evm_oracle_state::{
    AggregatorLayout, AggregatorLayoutConfidence, ChainlinkFeedProvider, FeedConfig, FeedMetadata,
    FeedStatus, OracleError, OracleRegistry, RoundData, StalenessPolicy,
};

type ProviderFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, OracleError>> + Send + 'a>>;

#[derive(Clone, Debug)]
struct FeedFixture {
    proxy: Address,
    aggregator: Option<Address>,
    metadata: FeedMetadata,
    round: RoundData,
}

#[derive(Clone, Debug)]
struct MockProvider {
    feed: FeedFixture,
    type_and_version: Option<String>,
    code_hash: Option<B256>,
}

impl MockProvider {
    fn new(feed: FeedFixture) -> Self {
        Self {
            feed,
            type_and_version: None,
            code_hash: None,
        }
    }

    fn with_type_and_version(mut self, type_and_version: impl Into<String>) -> Self {
        self.type_and_version = Some(type_and_version.into());
        self
    }

    fn with_code_hash(mut self, code_hash: B256) -> Self {
        self.code_hash = Some(code_hash);
        self
    }
}

impl ChainlinkFeedProvider for MockProvider {
    fn decimals(&self, proxy: Address) -> ProviderFuture<'_, u8> {
        assert_eq!(proxy, self.feed.proxy);
        Box::pin(async move { Ok(self.feed.metadata.decimals) })
    }

    fn description(&self, proxy: Address) -> ProviderFuture<'_, String> {
        assert_eq!(proxy, self.feed.proxy);
        Box::pin(async move { Ok(self.feed.metadata.description.clone()) })
    }

    fn version(&self, proxy: Address) -> ProviderFuture<'_, U256> {
        assert_eq!(proxy, self.feed.proxy);
        Box::pin(async move { Ok(self.feed.metadata.version) })
    }

    fn latest_round_data(&self, proxy: Address) -> ProviderFuture<'_, RoundData> {
        assert_eq!(proxy, self.feed.proxy);
        Box::pin(async move { Ok(self.feed.round.clone()) })
    }

    fn aggregator(&self, proxy: Address) -> ProviderFuture<'_, Option<Address>> {
        assert_eq!(proxy, self.feed.proxy);
        Box::pin(async move { Ok(self.feed.aggregator) })
    }

    fn aggregator_type_and_version(
        &self,
        aggregator: Address,
    ) -> ProviderFuture<'_, Option<String>> {
        assert_eq!(Some(aggregator), self.feed.aggregator);
        let type_and_version = self.type_and_version.clone();
        Box::pin(async move { Ok(type_and_version) })
    }

    fn aggregator_code_hash(&self, aggregator: Address) -> ProviderFuture<'_, Option<B256>> {
        assert_eq!(Some(aggregator), self.feed.aggregator);
        let code_hash = self.code_hash;
        Box::pin(async move { Ok(code_hash) })
    }
}

fn metadata() -> FeedMetadata {
    FeedMetadata {
        decimals: 8,
        description: "ETH / USD".to_string(),
        version: U256::from(4_u64),
    }
}

fn round(answer: i64, updated_at: u64) -> RoundData {
    RoundData {
        round_id: U256::from(12_345_u64),
        answer: I256::unchecked_from(answer),
        started_at: updated_at.saturating_sub(60),
        updated_at,
        answered_in_round: U256::from(12_345_u64),
    }
}

fn fixture(round: RoundData) -> FeedFixture {
    fixture_for(
        Address::repeat_byte(0x10),
        Some(Address::repeat_byte(0x20)),
        round,
    )
}

fn fixture_for(proxy: Address, aggregator: Option<Address>, round: RoundData) -> FeedFixture {
    FeedFixture {
        proxy,
        aggregator,
        metadata: metadata(),
        round,
    }
}

#[tokio::test]
async fn register_chainlink_feed_loads_metadata_aggregator_and_initial_snapshot() {
    let now = 1_700_004_000;
    let feed = fixture(round(250_000_000_000, now - 120));
    let provider = MockProvider::new(feed.clone());
    let mut registry = OracleRegistry::new_at_timestamp(now);

    let id = registry
        .register_chainlink_feed(
            &provider,
            FeedConfig {
                proxy: feed.proxy,
                label: Some("ETH/USD".to_string()),
                staleness: StalenessPolicy::max_age(60 * 60),
                ..Default::default()
            },
        )
        .await
        .expect("registration should seed from proxy reads");

    let registration = registry.registration(id).expect("registered feed");
    assert_eq!(registration.proxy, feed.proxy);
    assert_eq!(registration.current_aggregator, feed.aggregator);
    assert_eq!(registration.metadata, metadata());

    let snapshot = registry.latest(feed.proxy).expect("initial snapshot");
    assert_eq!(snapshot.id, id);
    assert_eq!(snapshot.proxy, feed.proxy);
    assert_eq!(snapshot.aggregator, feed.aggregator);
    assert_eq!(
        snapshot.round.answer,
        I256::unchecked_from(250_000_000_000_i64)
    );
    assert_eq!(snapshot.status, FeedStatus::Fresh);
}

#[tokio::test]
async fn register_detects_ocr2_layout_from_type_and_version() {
    let now = 1_700_004_000;
    let feed = fixture(round(250_000_000_000, now - 120));
    let code_hash = B256::repeat_byte(0x99);
    let provider = MockProvider::new(feed.clone())
        .with_type_and_version("AccessControlledOCR2Aggregator 1.0.0")
        .with_code_hash(code_hash);
    let mut registry = OracleRegistry::new_at_timestamp(now);

    let id = registry
        .register_chainlink_feed(
            &provider,
            FeedConfig {
                proxy: feed.proxy,
                label: Some("ETH/USD".to_string()),
                staleness: StalenessPolicy::max_age(60 * 60),
                ..Default::default()
            },
        )
        .await
        .expect("registration should detect OCR2 layout");

    let evidence = registry
        .registration(id)
        .expect("registered feed")
        .aggregator_layout
        .as_ref()
        .expect("layout evidence");
    assert_eq!(evidence.layout, AggregatorLayout::ChainlinkOcr2V1);
    assert_eq!(
        evidence.confidence,
        AggregatorLayoutConfidence::TypeAndVersion
    );
    assert_eq!(
        evidence.type_and_version.as_deref(),
        Some("AccessControlledOCR2Aggregator 1.0.0")
    );
    assert_eq!(evidence.code_hash, Some(code_hash));
}

#[tokio::test]
async fn register_can_use_cached_code_hash_layout_when_type_and_version_is_missing() {
    let now = 1_700_004_000;
    let code_hash = B256::repeat_byte(0x42);
    let first = fixture_for(
        Address::repeat_byte(0x10),
        Some(Address::repeat_byte(0x20)),
        round(250_000_000_000, now - 120),
    );
    let second = fixture_for(
        Address::repeat_byte(0x11),
        Some(Address::repeat_byte(0x21)),
        round(250_000_000_000, now - 120),
    );
    let mut registry = OracleRegistry::new_at_timestamp(now);

    registry
        .register_chainlink_feed(
            &MockProvider::new(first.clone())
                .with_type_and_version("AccessControlledOCR2Aggregator 1.0.0")
                .with_code_hash(code_hash),
            FeedConfig {
                proxy: first.proxy,
                label: Some("ETH/USD".to_string()),
                staleness: StalenessPolicy::max_age(60 * 60),
                ..Default::default()
            },
        )
        .await
        .expect("first registration seeds code-hash cache");

    let id = registry
        .register_chainlink_feed(
            &MockProvider::new(second.clone()).with_code_hash(code_hash),
            FeedConfig {
                proxy: second.proxy,
                label: Some("ETH/USD 2".to_string()),
                staleness: StalenessPolicy::max_age(60 * 60),
                ..Default::default()
            },
        )
        .await
        .expect("second registration should use cached code-hash layout");

    let evidence = registry
        .registration(id)
        .expect("registered feed")
        .aggregator_layout
        .as_ref()
        .expect("layout evidence");
    assert_eq!(evidence.layout, AggregatorLayout::ChainlinkOcr2V1);
    assert_eq!(
        evidence.confidence,
        AggregatorLayoutConfidence::CodeHashCache
    );
    assert_eq!(evidence.type_and_version, None);
    assert_eq!(evidence.code_hash, Some(code_hash));
}

#[tokio::test]
async fn staleness_and_invalid_rounds_are_classified_on_registration() {
    let now = 1_700_004_000;

    for (name, round, expected) in [
        (
            "stale",
            round(1, now - 7_200),
            FeedStatus::Stale {
                age_secs: 7_200,
                max_age_secs: 3_600,
            },
        ),
        ("incomplete", round(1, 0), FeedStatus::IncompleteRound),
        (
            "invalid non-positive answer",
            round(0, now - 10),
            FeedStatus::InvalidAnswer,
        ),
    ] {
        let feed = fixture(round);
        let provider = MockProvider::new(feed.clone());
        let mut registry = OracleRegistry::new_at_timestamp(now);

        registry
            .register_chainlink_feed(
                &provider,
                FeedConfig {
                    proxy: feed.proxy,
                    label: Some(name.to_string()),
                    staleness: StalenessPolicy::max_age(3_600),
                    ..Default::default()
                },
            )
            .await
            .expect("registration should retain typed status for bad rounds");

        assert_eq!(registry.latest(feed.proxy).unwrap().status, expected);
    }
}

#[tokio::test]
async fn policy_can_allow_zero_or_negative_generic_numeric_feeds() {
    let now = 1_700_004_000;
    let feed = fixture(round(-5, now - 10));
    let provider = MockProvider::new(feed.clone());
    let mut registry = OracleRegistry::new_at_timestamp(now);

    registry
        .register_chainlink_feed(
            &provider,
            FeedConfig {
                proxy: feed.proxy,
                label: Some("generic signed feed".to_string()),
                staleness: StalenessPolicy::max_age(3_600).allow_zero_or_negative_answer(true),
                ..Default::default()
            },
        )
        .await
        .expect("generic numeric feeds may opt into signed answers");

    assert_eq!(
        registry.latest(feed.proxy).unwrap().status,
        FeedStatus::Fresh
    );
}
