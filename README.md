# evm-oracle-state

`evm-oracle-state` tracks Chainlink-compatible, single-value oracle feeds
through their stable proxy addresses. It is a domain crate layered over
`evm-fork-cache`: `evm-fork-cache` owns generic cache invalidation, reactive
batching, and reorg reporting, while this crate owns oracle-specific metadata,
event decoding, snapshots, staleness policy, reconciliation, and feed lookup.

The central guarantee is:

> Events are fast-path hints; proxy reads are authoritative.

`AnswerUpdated` logs update typed state immediately. By default, the reactive
handler invalidates cached proxy and aggregator storage as a conservative
fallback. For detected OCR2 feeds, the handler listens to `NewTransmission`
instead and can emit exact cache state updates for the OCR2 aggregator. For
detected OCR1 feeds, the handler listens to the OCR1 `NewTransmission` event and
writes the common `AccessControlledOffchainAggregator` 2.x/3.x/4.x layout. The
crate detects supported aggregator layouts from `typeAndVersion()` and cached
runtime code-hash evidence. It can also serve registered oracle view calls from
typed state through `OracleReadOverlay`, so a simulation can observe the event
price before a follow-up RPC read completes. Startup, periodic repair,
aggregator changes, and post-reorg safety still use
`latestRoundData()` through the proxy as the source of truth.

## MVP Surface

- Register Chainlink-compatible proxy feeds with typed `Feed::proxy(...)`
  metadata.
- Read `decimals()`, `description()`, `version()`, `latestRoundData()`, and
  best-effort `aggregator()` metadata at registration.
- Look up typed snapshots by proxy address with freshness classification.
- Decode `AnswerUpdated(int256,uint256,uint256)`,
  `NewRound(uint256,address,uint256)`, OCR1 `NewTransmission`, and OCR2
  `NewTransmission` logs.
- Build `OracleReactiveHandler` log interests for current aggregators.
- Emit `PurgeScope::AllStorage` invalidations for both proxy and aggregator on
  `AnswerUpdated`.
- Emit metadata-rich `OraclePriceUpdate` hooks for immediate user callbacks.
- Decode generic hook signals through `OracleSignal::from_hook` and
  `OracleSignalKind` constants instead of string comparisons.
- Track snapshot lifecycle confidence separately from round validity:
  `EventPending`, `Confirmed`, `Corrected`, `Stale`, `Invalid`, and `Unknown`.
- Read typed prices with `OracleTracker::price`, `price_by_proxy`, and
  `latest_round` without ABI encoding.
- Promote raw oracle prices with `PricePolicy` before liquidation or market
  valuation decisions.
- Value `TokenAmount`s with checked arithmetic through `CheckedPrice::value_of`.
- Build a higher-level `OracleRuntime` with `ChainlinkFeed` registrations and
  optional typed event callbacks, or use the cache-native
  `OracleAdapter::builder().feed(Feed::proxy(...))` groundwork over `EvmCache`.
- With the `aave` feature, use `AaveV3OracleAdapter` and `AaveAsset` to
  discover Aave V3 oracle sources from `getSourceOfAsset`, including
  `PriceCapAdapterStable`, ratio-cap/CAPO, peg-to-base Chainlink
  synchronicity, and fixed-price sources.
- With the `morpho` feature, use `MorphoBlueOracleAdapter` to register Morpho
  Blue markets or direct `MorphoChainlinkOracleV2` addresses. Chainlink
  dependency events recompute the 36-decimal Morpho `price()` immediately, and
  `OracleReadOverlay` can serve `price()` from typed state.
- With the `euler` feature, use `EulerOracleAdapter` and `EulerQuotePair` to
  register direct Euler quote adapters or router-resolved pairs. The initial
  adapter supports `ChainlinkOracle`, `FixedRateOracle`, `RateProviderOracle`,
  and one-level `CrossAdapter` sources composed from those legs. Chainlink
  dependency events recompute the user-facing quote immediately, and
  `OracleReadOverlay` can serve `getQuote(...)` / `getQuotes(...)`.
- Queue event-specific, block-identified proxy reconciliation requests after
  committed price updates.
- Serve simulation-facing `latestRoundData()`, `decimals()`, `description()`,
  and `version()` calls through `OracleReadOverlay` from typed state without
  writing Chainlink storage slots.
- Detect aggregator layouts from `typeAndVersion()` and runtime code-hash
  evidence.
- Use the built-in OCR2 adapter for direct cache writes against detected
  `AccessControlledOCR2Aggregator 1.0.0`-layout aggregators.
- Use the built-in OCR1 adapter for direct cache writes against detected
  `AccessControlledOffchainAggregator` 2.x, 3.x, and 4.x layout aggregators.
- Reconcile event prices through `OracleReconciler`, emitting
  `OraclePriceConfirmed`, `OraclePriceCorrected`, `OraclePriceStale`, and
  `OracleAggregatorChanged` hook events.
- Carry committed oracle updates through reactive hook payloads for
  `OracleTracker::apply_batch_report`, while preserving the legacy
  `OracleUpdate` payload for existing users.
- Reconcile all feeds from proxy reads and expose aggregator changes so callers
  can rebuild reactive routing.
- Treat removed event state conservatively by marking event-derived snapshots
  unknown and requiring reconciliation.

## Feature Flags

Core Chainlink-compatible registration, reactive handling, reconciliation,
overlay reads, and direct OCR1/OCR2 storage sync are always compiled. Optional
features currently add focused surfaces:

- `aave`: enables `AaveV3OracleAdapter` and `AaveAsset` discovery helpers.
- `morpho`: enables Morpho Blue oracle discovery helpers.
- `euler`: enables Euler quote-oracle discovery helpers.
- `live-tests`: enables websocket dependencies used by RPC-gated live tests.

## Event to Simulation Flow

```text
AnswerUpdated, OCR1 NewTransmission, or OCR2 NewTransmission log
  -> OracleReactiveHandler decodes the event
  -> OraclePriceUpdate hook is emitted immediately
  -> OracleTracker stores an EventPending snapshot
  -> OracleReadOverlay serves registered oracle view calls from typed state
  -> OracleReconciler reads the proxy latestRoundData()
  -> snapshot becomes Confirmed, Corrected, Stale, or Invalid
```

The cache policy is intentionally conservative:

- Typed state syncs the event answer immediately.
- Simulation reads can use the typed overlay immediately.
- Raw EVM storage is purged/refetched unless a detected layout adapter supports
  the feed.

## Direct Chainlink Storage Sync

For detected Chainlink OCR1 and OCR2 aggregators, callers get event-derived cache
writes from the default runtime/handler:

```rust
let mut oracle = OracleRuntime::builder(provider)
    .feed(ChainlinkFeed::new(eth_usd_proxy).id("eth-usd"))
    .build()
    .await?;
```

The OCR2 adapter writes the `s_hotVars.latestEpochAndRound` and
`s_hotVars.latestAggregatorRoundId` packed fields plus the
`s_transmissions[roundId]` mapping entry. For supported OCR2 feeds, the handler
subscribes to OCR2 `NewTransmission` instead of `AnswerUpdated`, so it can write
the median answer, `startedAt` from `observationsTimestamp`, `updatedAt` from the
block timestamp, and the latest epoch/round from the event.

The OCR1 adapter targets the common `AccessControlledOffchainAggregator`
2.x/3.x/4.x layout. It writes `s_hotVars.latestEpochAndRound`,
`s_hotVars.latestAggregatorRoundId`, and `s_transmissions[roundId]`. OCR1
`NewTransmission` does not carry a separate observations timestamp, so
`startedAt` and `updatedAt` both use the block timestamp, matching OCR1
`latestRoundData()` behavior.

Adapters only claim events when registration or reconciliation has detected a
supported layout and the packed hot-vars slot is already hot in
`evm-fork-cache`; otherwise the existing purge/refetch fallback remains active.

## Ergonomic Runtime

```rust
use alloy_primitives::U256;
use evm_oracle_state::{AssetId, Denomination, PricePolicy, TokenAmount};

let mut oracle = OracleRuntime::builder(provider)
    .feed(
        ChainlinkFeed::new(eth_usd_proxy)
            .id("eth-usd")
            .label("ETH/USD")
            .base("ETH")
            .quote("USD")
            .max_age_secs(3600),
    )
    .on_price_update(|update| {
        // enqueue liquidation search, metrics, alerts, etc.
    })
    .build()
    .await?;

let handler = oracle.reactive_handler();
let price = oracle
    .price("eth-usd")?
    .require(PricePolicy::liquidation().quote(Denomination::Usd))?;
let two_eth = TokenAmount::new(
    AssetId::symbol("ETH"),
    U256::from(2_000_000_000_000_000_000_u128),
    18,
);
let usd_value = price.value_of(two_eth, 18)?;
```

When an `EvmCache` is already present, the cache-native adapter provides the
preferred lower-level registration path:

```rust
use evm_oracle_state::{AssetId, Denomination, Feed, OracleAdapter};

let tracker = OracleAdapter::builder()
    .feed(
        Feed::proxy(eth_usd_proxy)
            .id("eth-usd")
            .label("ETH/USD")
            .base(AssetId::symbol("ETH"))
            .quote(Denomination::Usd)
            .max_age_secs(3600),
    )
    .build(&mut cache)
    .await?;
```

With `--features aave`, Aave assets can be registered declaratively:

```rust
use evm_oracle_state::{AaveAsset, AaveV3OracleAdapter, AssetId, Denomination};

let tracker = AaveV3OracleAdapter::new(aave_oracle)
    .asset(
        AaveAsset::new(usdt)
            .id("aave-v3-usdt")
            .label("Aave V3 USDT")
            .base(AssetId::symbol("USDT"))
            .quote(Denomination::Usd)
            .max_age_secs(24 * 60 * 60),
    )
    .build(&mut cache)
    .await?;
```

With `--features morpho`, Morpho Blue market oracles can be registered from a
market id:

```rust
use evm_oracle_state::{AssetId, Denomination, MorphoBlueMarket, MorphoBlueOracleAdapter};

let mut oracle = OracleRuntime::cache_builder()
    .install_adapter(
        MorphoBlueOracleAdapter::new(morpho_blue)
            .market(
                MorphoBlueMarket::market_id(market_id)
                    .id("morpho-weth-usdc")
                    .label("Morpho WETH/USDC")
                    .base(AssetId::symbol("WETH"))
                    .quote(Denomination::from("USDC"))
                    .max_age_secs(3600),
            ),
    )
    .build(&mut cache)
    .await?;
```

With `--features euler`, Euler quote oracles can be registered from direct
adapters or router-resolved pairs:

```rust
use evm_oracle_state::{AssetId, Denomination, EulerOracleAdapter, EulerQuotePair};

let mut oracle = OracleRuntime::cache_builder()
    .install_adapter(
        EulerOracleAdapter::router(euler_router).quote_pair(
            EulerQuotePair::new(weth, usdc)
                .id("euler-weth-usdc")
                .label("Euler WETH/USDC")
                .base_label(AssetId::symbol("WETH"))
                .quote_label(Denomination::from("USDC"))
                .max_age_secs(3600),
        ),
    )
    .build(&mut cache)
    .await?;
```

## Oracle Adapter Plugins

Oracle families that are not plain Chainlink proxies can plug into the same
runtime by implementing `OracleAdapterPlugin`. Discovery receives a mutable
`EvmCache`, returns seeded `FeedRegistration` plus `RoundData` pairs, and the
adapter provides the `evm-fork-cache` reactive handler that owns its event
routing.

```rust
use std::sync::Arc;

use alloy_network::Ethereum;
use evm_fork_cache::reactive::ReactiveHandler;
use evm_oracle_state::{
    AdapterFuture, FeedRegistration, OracleAdapterId, OracleAdapterPlugin,
    OracleDiscoveredFeed, OracleDiscoveryContext, OracleDiscoveryReport,
    OracleRuntime, OracleStorageSync,
};

struct MyOracleAdapter;

impl OracleAdapterPlugin for MyOracleAdapter {
    fn adapter_id(&self) -> OracleAdapterId {
        OracleAdapterId::new("my-oracle")
    }

    fn discover<'a>(
        &'a self,
        ctx: OracleDiscoveryContext<'a>,
    ) -> AdapterFuture<'a, OracleDiscoveryReport> {
        Box::pin(async move {
            // Use ctx.cache for all chain reads, then seed typed state.
            Ok(OracleDiscoveryReport::new().with_feed(
                OracleDiscoveredFeed::new(registration, round),
            ))
        })
    }

    fn reactive_handler(
        &self,
        registrations: Vec<FeedRegistration>,
        storage_sync: OracleStorageSync,
    ) -> Arc<dyn ReactiveHandler<Ethereum>> {
        Arc::new(MyOracleReactiveHandler::new(registrations, storage_sync))
    }
}

let mut oracle = OracleRuntime::cache_builder()
    .install_adapter(MyOracleAdapter)
    .build(&mut cache)
    .await?;
```

Use `build_report(&mut cache).await?` for best-effort startup when adapter
skips should be inspected without failing the whole runtime. The stricter
`build(&mut cache).await?` path fails fast when any adapter reports a skipped
feed, including the feed id or label, proxy, and skip reason in the error.

Custom handlers should emit standard `OraclePriceUpdate` hook payloads in the
`evm-oracle-state` namespace. `OracleRuntime::apply_batch_report` will then
update `OracleTracker` state exactly like built-in Chainlink events. The
`FeedSource::custom(OracleSourceDescriptor::new(...))` keeps custom sources out
of the built-in Chainlink handler while preserving common price, overlay, and
hook APIs.

When a runtime is built with adapter plugins, prefer
`OracleRuntime::reactive_runtime()`, `register_subscriber(...)`,
`reactive_handlers()`, or `reactive_interests()` so built-in and adapter-owned
handlers are wired together. `reactive_handler()` intentionally returns only the
built-in Chainlink-compatible handler.

## Limitations

- No durable database or indexer.
- No Chainlink Feed Registry integration.
- Morpho and Euler adapter-specific reconciliation is not yet wired into
  `OracleReconciler`; dependency-event prices are marked `EventPending` and are
  intended for fast-path simulation via overlay until periodic protocol reads
  confirm them.
- Aave source discovery currently supports plain Chainlink-compatible sources,
  `PriceCapAdapterStable`, ratio-cap/CAPO, peg-to-base Chainlink
  synchronicity, and fixed-price sources. EUR cap variants, base-to-peg and
  fixed-ratio synchronicity, Pendle caps, and discounted MKR/SKY remain skipped
  until dedicated source models are added.
- Multi-dependency Aave peg-to-base synchronicity events are actionable
  immediately using the seeded companion dependency answer, and event-specific
  reconciliation reads both dependency proxies before confirming or correcting
  the derived price. Rebuild the reactive handler after reconciliation to
  refresh dependency routing; a later stateful dependency graph can remove that
  limitation.
- Direct storage writes are limited to detected OCR1/OCR2 layouts; unsupported
  or unknown layouts fall back to purge/refetch.
- No in-cache generic call interceptor in the current `evm-fork-cache` surface;
  use `OracleReadOverlay` as the simulation read adapter.
- No trading, liquidation, or transaction submission behavior.
- Aggregator routing is best effort and should be rebuilt after reconciliation
  reports aggregator changes.

## Demos

Start with the deterministic demos; they do not require an RPC URL and are safe
to run in CI or during PR review.

```sh
cargo run --example local_oracle_lifecycle
cargo run --example chainlink_storage_sync_demo
```

`local_oracle_lifecycle` is the compile-checked quickstart. It walks through the
core runtime flow with a deterministic mock provider and synthetic event:

```text
mock proxy read
  -> ChainlinkFeed builder registration
  -> synthetic AnswerUpdated log
  -> immediate OraclePriceUpdate hook
  -> EventPending typed state
  -> OracleReadOverlay latestRoundData()
  -> proxy reconciliation
  -> OraclePriceConfirmed hook
```

`chainlink_storage_sync_demo` shows the direct state-write policy for detected
Chainlink OCR1 and OCR2 aggregators. It prints the exact hot-vars and
transmission writes when the layout is known and the hot-vars slot is already in
cache, then shows the fallback purge/refetch path for a cold cache.

The live examples are network-gated:

```sh
cargo run --example chainlink_live_probe
ETH_RPC_URL=https://example-rpc.invalid cargo run --example chainlink_live_probe
ETH_RPC_URL=https://example-rpc.invalid SMOKE_DURATION_SECS=600 cargo run --example chainlink_reactive_smoke
ETH_RPC_URL=https://example-http-rpc.invalid ETH_WS_RPC_URL=wss://example-ws-rpc.invalid AAVE_DEMO_DURATION_SECS=600 cargo run --features aave --example aave_v3_health_factor -- 0xcc144c2e9fc40f92257b2cd7cc43e1f82666f09a
ETH_RPC_URL=https://example-http-rpc.invalid ETH_WS_RPC_URL=wss://example-ws-rpc.invalid MORPHO_MARKET_ID=0x... EULER_ORACLE=0x... EULER_BASE=0x... EULER_QUOTE=0x... cargo run --features morpho,euler --example morpho_euler_reactive_value
```

When `ETH_RPC_URL` is unset, live examples exit successfully without making
network calls. Downstream live integrations should prefer the cache-backed
adapter path when an `EvmCache` is already present. `ChainlinkFeedProvider`
remains available for tests, custom RPC/multicall readers, and advanced
integration seams.

`aave_v3_health_factor` is a cache-backed Ethereum Aave V3 lending-market demo.
It loads the user's active reserves through `EvmCache`, registers
Aave oracle sources with the cache-native reader, prints the reactive oracle log
interests, computes collateral/debt/health-factor math locally, and compares the
result with Aave `getUserAccountData` read through the same cache. Build it with
`--features aave` to discover Aave sources from asset addresses, including
`PriceCapAdapterStable`, ratio-cap/CAPO, peg-to-base synchronicity, and
fixed-price sources backed by cache-native reads. It then registers the oracle
interests with `evm-fork-cache`'s `AlloySubscriber`; every `OraclePriceUpdate`
hook prints the new oracle answer and recomputes the health factor from the
event-mutated cache. It also prints the DeBank CLI command used for an
off-chain portfolio sanity check.

`morpho_euler_reactive_value` is a cache-backed Morpho/Euler oracle tracking
demo. It discovers a Morpho Blue market or direct Morpho oracle, discovers an
Euler quote oracle, installs both adapters into `OracleRuntime`, registers the
resulting interests with `evm-fork-cache`'s `AlloySubscriber`, and prints
health-factor-style value math from typed oracle prices on cold start and after
live `OraclePriceUpdate` hooks. The Morpho calculation uses the market LLTV; the
Euler calculation uses `EULER_LTV_WAD` because exact Euler account health is
vault/controller specific. When `ETH_WS_RPC_URL` is unset, the example derives a
websocket URL from `ETH_RPC_URL` by replacing `https://` with `wss://`.
