# evm-oracle-state

`evm-oracle-state` tracks single-value EVM oracle feeds through stable typed
registrations. Chainlink-compatible feeds are tracked through proxy addresses;
adapter-owned sources such as Pyth can use synthetic per-feed state keys while
storing their real on-chain source metadata. It is a domain crate layered over
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
- With the `pyth` feature, use `PythOracleAdapter` and `PythFeed` to register
  Pyth EVM price ids under a shared Pyth contract. `PriceFeedUpdate` events
  emit standard `OraclePriceUpdate` hooks immediately, update typed state by
  `(pyth contract, price id)`, and `OracleReadOverlay` can serve
  `getPriceUnsafe(bytes32)` / `getPriceNoOlderThan(bytes32,uint256)` from
  typed state.
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
- `pyth`: enables Pyth EVM price-id discovery, event routing, and overlay reads.
- `live-tests`: enables websocket dependencies used by RPC-gated live tests.

## Event to Simulation Flow

```text
AnswerUpdated, OCR1/OCR2 NewTransmission, or Pyth PriceFeedUpdate log
  -> adapter reactive handler decodes the event
  -> OraclePriceUpdate hook is emitted immediately
  -> OracleTracker stores an EventPending snapshot
  -> OracleReadOverlay serves registered oracle view calls from typed state
  -> OracleReconciler reads the proxy latestRoundData(), when source supports Chainlink reconciliation
  -> snapshot becomes Confirmed, Corrected, Stale, or Invalid
```

The cache policy is intentionally conservative:

- Typed state syncs the event answer immediately.
- Simulation reads can use the typed overlay immediately.
- Raw EVM storage is purged/refetched unless a detected layout adapter supports
  the feed.
- Pyth events currently use typed state plus conservative Pyth-contract storage
  invalidation; no generic Pyth storage writes are attempted.

## Dependency-Aware Reactive Routing

`OracleReactiveHandler` routes logs through an internal dependency key:

```text
(aggregator address, event family)
```

The event family is one of:

- `AnswerUpdated`
- OCR1 `NewTransmission`
- OCR2 `NewTransmission`

This is the first compatibility layer of the dependency-graph model. The public
registration APIs still describe user-facing feeds, but the reactive handler
groups those feeds by their underlying Chainlink event dependency before it
builds log interests or handles events.

That gives three distinct stages:

```text
Feed registrations
  -> dependency keys
  -> one log interest per dependency key
  -> one dependency-level cache effect per event
  -> one normalized OraclePriceUpdate hook per affected feed
```

For example, if a Morpho oracle and an Euler oracle both depend on the same
ETH/USD Chainlink aggregator, the handler subscribes once to the underlying
ETH/USD event. When the event lands, direct Chainlink storage sync, if available,
is applied once to that underlying aggregator state. The runtime then fans out
typed hooks to each user-facing oracle:

```text
ETH/USD NewTransmission
  -> one OCR storage write set for ETH/USD
  -> Euler WETH/USD OraclePriceUpdate
  -> Morpho WETH/USDC OraclePriceUpdate
```

The hooks remain feed-specific. Each registration still applies its own
normalization:

- Plain Chainlink feeds use the raw Chainlink answer.
- Aave capped or composed adapters apply the Aave source transform.
- Morpho Chainlink V2 sources recompute the 36-decimal Morpho `price()`.
- Euler quote and cross adapters recompute the requested quote output.

Fallback behavior is still conservative. If no verified OCR1/OCR2 storage layout
can produce exact cache writes, the handler keeps the existing aggregator/proxy
purge path. Wrapper contracts such as Aave, Morpho, and Euler are not written
directly unless a dedicated, verified adapter exists; they are updated through
typed state and read overlays.

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
supported layout and the packed hot-vars slot is hot in `evm-fork-cache`;
otherwise the existing purge/refetch fallback remains active. Cache-native
builds now prewarm those slots by default through `EvmCache::prewarm_slots`, so
the direct-write path is ready before the reactive loop starts:

```rust
let report = OracleAdapter::builder()
    .feed(Feed::proxy(eth_usd_proxy).id("eth-usd"))
    .build_report(&mut cache)
    .await?;

let oracle = OracleRuntime::from_tracker(report.tracker);
let warmup = oracle.prewarm_storage(&mut cache);
assert_eq!(warmup.failed_slots.len(), 0);
```

`prewarm_storage` uses `evm-fork-cache`'s installed storage batch fetcher. In
v0.2.1 that fetcher defaults to bulk `eth_call` storage extraction: target
contracts are temporarily run with a tiny extractor bytecode that performs raw
`SLOAD`s for the requested slots, and Multicall3 dispatches many target
contracts in a small number of calls. This is the fast path for known OCR hot
slots. The point-read `eth_getStorageAt` strategy is still available through
`EvmCacheBuilder::storage_fetch_strategy(StorageFetchStrategy::PointRead)` for
benchmarking or provider fallback.

Adapter-plugin runtimes expose the same work in their build report:

```rust
let report = OracleRuntime::cache_builder()
    .install_adapter(my_adapter)
    .storage_cold_start()
    .build_report(&mut cache)
    .await?;

println!(
    "oracle storage warmup: {}/{} slots",
    report.storage_warmup.loaded_slots,
    report.storage_warmup.requested_slots
);
```

Call `.disable_storage_warmup()` when a caller intentionally wants lazy
fallback behavior, or call `oracle.storage_warmup_slots()` to inspect the exact
declared hot slots. The default mode uses `EvmCache::prewarm_slots`; use
`.storage_cold_start()` or `oracle.cold_start_storage(&mut cache)` when the
caller wants v0.2.1 cold-start verification through `EvmCache::run_cold_start`.
Cold-start mode first verifies declared hot slots, then runs discovery calls
such as `latestRoundData()` and bulk-verifies any newly discovered slots in a
second round. Known storage goes through raw SLOAD extraction; dynamic
metadata/proxy composition that cannot be represented safely as fixed slots
stays on typed view calls.

Cache-native Chainlink registration prefers direct RPC Multicall3 for proxy
view reads, then falls back to fork-cache simulation or sequential cache calls
when direct RPC batching is unavailable:

```rust
let report = OracleAdapter::builder()
    .feeds(feeds)
    .multicall_reads(true) // default
    .build_report(&mut cache)
    .await?;
```

This batches `decimals()`, `description()`, `version()`, `latestRoundData()`,
and `aggregator()` for each proxy into one direct `eth_call` to Multicall3. It
then batches aggregator `typeAndVersion()` reads the same way, so layout
detection does not hydrate each proxy/aggregator through the local fork VM.
Known hot storage still loads through `EvmCache::prewarm_slots`, which uses
`evm-fork-cache`'s bulk SLOAD extraction path.

## Oracle Code Warmup

The cache-native runtime can also install oracle-related bytecode before
discovery. Canonical seeds are verified once against the chain code hash through
`evm-fork-cache`; explicit etches are available for local simulation code and
are reported separately:

```rust
let report = OracleRuntime::cache_builder()
    .code_seeds(verified_oracle_bytecodes)
    .code_etch(local_adapter, simulation_runtime_code)
    .code_warmup_policy(OracleCodeWarmupPolicy::strict())
    .install_adapter(my_adapter)
    .build_report(&mut cache)
    .await?;

assert!(report.code_warmup.mismatched.is_empty());
assert!(report.code_warmup.install_errors.is_empty());
```

Use `code_seed`/`code_seeds` for bytecode that must match the pinned chain
state. Use `code_etch`/`code_etches` only when local divergence is deliberate.
The default policy fails on install errors, hash mismatches, undeployed
addresses, and codeless accounts, but allows transient verification
uncertainty. `OracleCodeWarmupPolicy::strict()` also fails on unverifiable
claims and verification transport errors, while `best_effort()` records all
outcomes without failing the build.

## Reactive Lifecycle

The runtime can install its handlers into `evm-fork-cache`'s owner-scoped
`ReactiveEngine`, then remove them later without disturbing unrelated handlers:

```rust
let mut installed = oracle.install_handlers_live_only(&mut engine)?;
assert!(!installed.is_empty());

let removed = oracle.uninstall_installed_handlers(&mut engine, &mut installed);
assert_eq!(removed.removed_handler_ids.len(), removed.handler_ids.len());
```

Use `install_handlers(&mut engine)` for continuity-safe registration that
backfills from the runtime's last canonical block, or
`install_handlers_with_backfill(&mut engine, SubscriberBackfill::from_block(n))`
for an explicit historical anchor. This is the preferred v0.2.1 path for
registering/unregistering oracle tracking at runtime; the older
`register_subscriber` helper remains a full-replacement bootstrap path.

Oracle registrations can also change after startup. Register already-discovered
feeds with `register_seeded_feed`, or install another cache-native adapter on an
existing runtime:

```rust
let mutation = oracle
    .register_adapter(PythOracleAdapter::new().feed(btc_usd), &mut cache)
    .await?;
assert!(mutation.registered_feed_ids.iter().any(|id| id.as_str() == "btc-usd"));

let refreshed = oracle.refresh_handlers_live_only(&mut engine, &mut installed)?;
assert_eq!(installed.handler_ids, refreshed.installed_handler_ids);

let removed = oracle.unregister_feed_by_id(FeedId::new("btc-usd"))?;
assert_eq!(removed.removed_feed_ids.len(), 1);
oracle.refresh_handlers_live_only(&mut engine, &mut installed)?;
```

The refresh step is explicit because it is the boundary where the in-memory
typed registry is projected into a live subscriber. Passing the prior
`OracleReactiveInstallReport` lets the runtime unsubscribe stale adapter
handlers even when the current registration set no longer includes them.

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

### Overlay-Scoped Price Mocks

For liquidation and risk-engine tests, create an `evm-fork-cache` mock overlay
and write an oracle price only into that simulation layer:

```rust
use alloy_primitives::I256;
use evm_oracle_state::{OracleOverlayMockExt, OraclePriceMock};

let mut sim = cache.mock_overlay();
let report = sim.mock_oracle_price(
    &cache,
    &oracle,
    "eth-usd",
    OraclePriceMock::new(I256::unchecked_from(1_750_00000000_i64))
        .updated_at(1_700_000_200),
)?;
assert!(report.applied());

// run liquidation/search simulations through `sim`; dropping it discards mocks.
```

For detected Chainlink OCR1/OCR2 feeds, this applies the same storage slots that
a live oracle event would update, preserving packed hot-slot bits from the cache.
If the feed has no supported direct-storage layout or the required hot slot has
not been warmed, `applied()` is `false` and the cache remains untouched. Use
`oracle.prewarm_storage(&mut cache)` before creating the overlay when the runtime
was not built with storage warmup enabled.

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

With `--features pyth`, Pyth price ids can be registered against one shared Pyth
contract. Each feed gets its own synthetic state key, so multiple price ids on
the same Pyth contract do not collide in the tracker:

```rust
use evm_oracle_state::{PythFeed, PythOracleAdapter, OracleRuntime};

let mut oracle = OracleRuntime::cache_builder()
    .install_adapter(
        PythOracleAdapter::new().feed(
            PythFeed::new(pyth_contract, eth_usd_price_id)
                .id("pyth-eth-usd")
                .label("Pyth ETH/USD")
                .base("ETH")
                .quote("USD")
                .max_age_secs(300),
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
  refresh dependency routing; the current dependency-aware handler is internal
  and stateless, so a later stateful graph is still needed to remove that
  limitation.
- Direct storage writes are limited to detected OCR1/OCR2 layouts; unsupported
  or unknown layouts fall back to purge/refetch.
- Pyth support does not write Pyth contract storage. It decodes
  `PriceFeedUpdate`, updates typed snapshots, serves Pyth read overlay calls,
  and invalidates the Pyth contract as a conservative fallback.
- No in-cache generic call interceptor in the current `evm-fork-cache` surface;
  use `OracleReadOverlay` as the simulation read adapter.
- No trading, liquidation, or transaction submission behavior.
- Aggregator routing is best effort and should be rebuilt after reconciliation
  reports aggregator changes.
- Derived recomputation is still compatibility-eager for registered feeds. The
  next graph phase can add explicit hook/overlay/snapshot demand so unused
  derived feeds do not recompute.

## Demos

Start with the deterministic demos; they do not require an RPC URL and are safe
to run in CI or during PR review.

```sh
cargo run --example local_oracle_lifecycle
cargo run --example chainlink_storage_sync_demo
cargo run --features pyth --example pyth_push_demo
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
ETH_RPC_URL=https://example-rpc.invalid cargo run --example oracle_storage_warmup_bench
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
