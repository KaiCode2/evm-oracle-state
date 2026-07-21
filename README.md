# evm-oracle-state

[![crates.io](https://img.shields.io/crates/v/evm-oracle-state.svg)](https://crates.io/crates/evm-oracle-state)
[![docs.rs](https://img.shields.io/docsrs/evm-oracle-state)](https://docs.rs/evm-oracle-state)
[![CI](https://github.com/KaiCode2/evm-oracle-state/actions/workflows/ci.yml/badge.svg)](https://github.com/KaiCode2/evm-oracle-state/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![MSRV](https://img.shields.io/badge/MSRV-1.88-informational)](https://github.com/KaiCode2/evm-oracle-state/blob/main/Cargo.toml)

`evm-oracle-state` tracks single-value EVM oracle feeds through stable typed
registrations. Chainlink-compatible feeds are tracked through proxy addresses;
adapter-owned sources such as Pyth and Euler use synthetic per-feed state keys
while storing their real on-chain source metadata. It is a domain crate layered over
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

## Installation

```sh
cargo add evm-oracle-state
```

Chainlink-compatible registration, direct OCR1/OCR2 storage sync,
reconciliation, overlay reads, and the typed price vocabulary are always
compiled. The protocol adapters are opt-in features:

```sh
cargo add evm-oracle-state --features aave,morpho,euler,pyth,redstone
```

MSRV is 1.88. [`evm-fork-cache`](https://crates.io/crates/evm-fork-cache) is a
0.x public dependency released in lockstep and re-exported at the crate root
(`evm_oracle_state::evm_fork_cache`), so downstream crates can name exactly the
version this crate's signatures use.

## Quickstart

Track a Chainlink feed, react to its events, and value a position — the full
lifecycle in one snippet:

```rust,ignore
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

Events update typed state immediately (`EventPending`); authoritative proxy
reads confirm or correct them afterwards, and `PricePolicy` refuses to promote
a value your liquidation logic should not trust yet. When an `EvmCache` is
already present, use the cache-native `OracleRuntime::cache_builder()` /
`OracleAdapter::builder()` paths instead of the provider-backed builder — see
[Runtime Recipes](#runtime-recipes).

Run the zero-setup, compile-checked version of this flow (no RPC required):

```sh
cargo run --example local_oracle_lifecycle
```

### Experimental Pending-Update Channel

The `pending-oracle-updates` feature enables an opt-in speculative channel for
searchers that want to simulate against an oracle value before its transaction
lands. It reuses feeds already registered through
`OracleRuntime::builder` or `OracleRuntime::cache_builder`, while keeping pending
observations separate from authoritative snapshots and confirmed hooks. The
initial decoder recognizes direct and forwarded Chainlink OCR2 transmissions;
the public channel and event vocabulary remain oracle-agnostic.

The confirmed `register_subscriber` factory deliberately remains committed-log
only: `evm-fork-cache` 0.3 cannot hydrate the full pending transaction bodies
needed here. Enable the separate `pending-oracle-ethereum` and/or
`pending-oracle-mev-share` transport feature, then attach its runtime-owned source
with `start_pending_source`. Both sources reuse the feeds already registered on
the builder; users do not repeat a feed subscription. Neither source is a
completeness guarantee. The built-in source presets target Ethereum mainnet;
other chains require an explicit `PendingOracleConfig::for_chain(...)` and a
transport with honest chain-specific ordering semantics. Chain or source
mismatches fail closed.

See [Pending Oracle Updates](docs/pending-oracle-updates.md) for the implemented
surface, target capability model, and limitations. Run the offline,
compile-checked integration example with:

```bash
cargo run --example pending_oracle_updates --features pending-oracle-updates
```

## Benchmarks & Performance

Full methodology, results, comparisons, and caveats live in
[docs/benchmarks.md](docs/benchmarks.md). Regression tracking uses the
fully-offline criterion suite (no RPC, no env vars; runs anywhere, including
CI):

```sh
cargo bench --bench oracle_hot_path
```

Headline numbers (Apple M1 Pro, 2026-07-10, v0.1.0 release candidate):

**Reactive hot path — local CPU** (criterion medians, fully offline):

| Operation | Fixture | Median |
| --- | --- | ---: |
| Warm-slot dedupe | 1,024 OCR2 registrations | **~116 µs** |
| Pyth overlay lookup (`getPriceUnsafe`) | 1,024 feeds | **~173 ns** |
| Direct-sync cold fallback decision | hot packed word absent | **~12 ns** |
| Direct-sync warm exact write planning | hot packed word present | **~253 ns** |

**Cold-start / repair path — provider-bound** (point-in-time July 2026 samples,
curated 24-feed Chainlink fixture, commercial Ethereum mainnet endpoint):

| Operation | Feeds | Sample result |
| --- | ---: | ---: |
| Direct RPC Multicall core reads | 24 | **261 ms** |
| Direct RPC Multicall layout reads | 24 aggregators | **88 ms** |
| Cache-native registration with multicall | 24 | **428 ms** |
| Bulk hot-slot prewarm | 24 slots | **98 ms** |
| Point hot-slot prewarm | 24 slots | **160 ms** |

Direct event application is local-CPU cheap; cold start is provider-bound and
dominated by metadata/layout RPCs, not storage prewarm. See
[docs/benchmarks.md](docs/benchmarks.md) for reproduce commands, the
feed-fixture knobs (`ORACLE_BENCH_FEED_SET` / `ORACLE_BENCH_FEEDS` /
`ORACLE_BENCH_FEEDS_FILE` / `ORACLE_BENCH_FEED_LIMIT`), and interpretation.

## Examples

Every example is runnable from the repo; the deterministic ones need no RPC
and are safe for CI.

| Example | Needs | What it shows |
| --- | --- | --- |
| [local_oracle_lifecycle](examples/local_oracle_lifecycle.rs) | nothing | The quickstart flow end-to-end: register -> synthetic event -> hook -> overlay read -> reconciliation. |
| [chainlink_storage_sync_demo](examples/chainlink_storage_sync_demo.rs) | nothing | Exact OCR1/OCR2 hot-vars + transmission writes, and the purge/refetch fallback for a cold cache. |
| [oracle_price_mock_demo](examples/oracle_price_mock_demo.rs) | nothing | Overlay-scoped price mocks: write a price into a simulation layer only, base cache untouched. |
| [custom_oracle_adapter](examples/custom_oracle_adapter.rs) | nothing | Add a brand-new oracle family via `OracleAdapterPlugin` without touching the crate. |
| [chainlink_live_probe](examples/chainlink_live_probe.rs) | RPC | One-shot live registration + typed snapshot reads. |
| [chainlink_reactive_smoke](examples/chainlink_reactive_smoke.rs) | RPC | Live cold start, historical event replay control, and a timed reactive window. |
| [aave_v3_health_factor](examples/aave_v3_health_factor.rs) | RPC + `aave` | Cache-backed Aave V3 health factor recomputed on live oracle hooks, checked against `getUserAccountData`. |
| [morpho_euler_reactive_value](examples/morpho_euler_reactive_value.rs) | RPC + `morpho,euler` | Morpho/Euler discovery, live dependency events, and health-factor-style value math. |
| [pyth_push_demo](examples/pyth_push_demo.rs) | RPC + `pyth` | Pyth price-id registration and live `PriceFeedUpdate` hooks. |
| [redstone_push_demo](examples/redstone_push_demo.rs) | RPC + `redstone` | RedStone push-feed discovery and live `ValueUpdate` hooks. |
| [oracle_cpu_overhead_bench](examples/oracle_cpu_overhead_bench.rs) | nothing | Hot-path CPU micro-measurements (see docs/benchmarks.md). |
| [oracle_storage_warmup_bench](examples/oracle_storage_warmup_bench.rs) | RPC optional | Live cold-start/prewarm benchmark plus the offline direct-sync microbench. |

Start with the deterministic demos — none of these require an RPC URL, and
they are safe to run in CI or during PR review.

```sh
cargo run --example local_oracle_lifecycle
cargo run --example chainlink_storage_sync_demo
cargo run --example oracle_price_mock_demo
cargo run --example custom_oracle_adapter
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

`oracle_price_mock_demo` is the runnable companion to the "Overlay-Scoped
Price Mocks" section: it builds an offline cache with a detected OCR2 layout
and warmed hot slot, writes a caller-chosen price into a `mock_overlay()`
simulation layer, reads it back through the overlay's storage, shows the
honest `applied() == false` path for a cold slot, and proves the base cache
was never touched.

`custom_oracle_adapter` is the runnable companion to the "Oracle Adapter
Plugins" section: it registers a brand-new oracle family from outside the
crate via `OracleAdapterPlugin` and `FeedSource::custom`, seeds typed state
through discovery, routes a synthetic event through the reactive runtime into
standard `OraclePriceUpdate` hooks, and reads the updated price back under an
explicit `PricePolicy`.

The live examples are network-gated:

```sh
cargo run --example chainlink_live_probe
ETH_RPC_URL=https://example-rpc.invalid cargo run --example chainlink_live_probe
ETH_RPC_URL=https://example-rpc.invalid SMOKE_DURATION_SECS=600 cargo run --example chainlink_reactive_smoke
ETH_RPC_URL=https://example-rpc.invalid cargo run --example oracle_storage_warmup_bench
ETH_RPC_URL=https://example-http-rpc.invalid ETH_WS_RPC_URL=wss://example-ws-rpc.invalid AAVE_DEMO_DURATION_SECS=600 cargo run --features aave --example aave_v3_health_factor -- 0xcc144c2e9fc40f92257b2cd7cc43e1f82666f09a
ETH_RPC_URL=https://example-http-rpc.invalid ETH_WS_RPC_URL=wss://example-ws-rpc.invalid MORPHO_MARKET_ID=0x... EULER_ORACLE=0x... EULER_BASE=0x... EULER_QUOTE=0x... cargo run --features morpho,euler --example morpho_euler_reactive_value
ETH_RPC_URL=https://example-http-rpc.invalid ETH_WS_RPC_URL=wss://example-ws-rpc.invalid PYTH_CONTRACT=0x... PYTH_PRICE_ID=0x... cargo run --features pyth --example pyth_push_demo
ETH_RPC_URL=https://example-http-rpc.invalid ETH_WS_RPC_URL=wss://example-ws-rpc.invalid REDSTONE_PRICE_FEED=0x... cargo run --features redstone --example redstone_push_demo
```

When `ETH_RPC_URL` is unset, live examples exit successfully without making
network calls. Downstream live integrations should prefer the cache-backed
adapter path when an `EvmCache` is already present. `ChainlinkFeedProvider`
remains available as an advanced/testing seam for custom readers and legacy
provider-backed runtimes; new integrations should generally use
`OracleRuntime::cache_builder()`, `OracleAdapter::builder()`, and
`EvmCache::call_sol`-backed discovery.

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

`redstone_push_demo` is a cache-backed RedStone push-feed demo. It cold-starts
from `EvmCache`, installs `RedstoneOracleAdapter`, prints discovered feed state,
registers RedStone log interests with `AlloySubscriber`, and prints live
`OraclePriceUpdate` hooks as they arrive. Required env: `ETH_RPC_URL` and
`REDSTONE_PRICE_FEED`. Optional env: `ETH_WS_RPC_URL`/`WS_RPC_URL`,
`REDSTONE_ADAPTER`, `REDSTONE_DATA_FEED_ID`, `REDSTONE_FEED_ID`,
`REDSTONE_LABEL`, `REDSTONE_BASE`, `REDSTONE_QUOTE`, `REDSTONE_MAX_AGE_SECS`,
`REDSTONE_DEMO_DURATION_SECS`, and `REDSTONE_DEMO_IDLE_TIMEOUT_SECS`. When no
explicit websocket URL is set, the example derives one from `ETH_RPC_URL` by
replacing `https://` with `wss://` or `http://` with `ws://`.

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
- Track oracle values with explicit round validity, value lifecycle, and value
  source fields: `round_status`, `value_status`, and `source`.
- Surface feed/runtime readiness separately through `OracleFeedStatus` on
  registrations, warmup reports, build reports, mutation reports, reconcile
  reports, and `feed_readiness()` accessors.
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
  and one-level `CrossAdapter` sources composed from those legs. Each pair
  registers under its own synthetic state key, so any number of pairs can
  share one router (or one adapter serving both directions), and discovery
  seeds the cold-start value from a real `getQuote(one base unit)` read
  through the router/adapter. Chainlink dependency events recompute the
  user-facing quote immediately, and `OracleReadOverlay` serves
  `getQuote(...)` / `getQuotes(...)` at the real router or adapter address.
- With the `pyth` feature, use `PythOracleAdapter` and `PythFeed` to register
  Pyth EVM price ids under a shared Pyth contract. `PriceFeedUpdate` events
  emit standard `OraclePriceUpdate` hooks immediately, update typed state by
  `(pyth contract, price id)`, and `OracleReadOverlay` can serve
  `getPriceUnsafe(bytes32)` / `getPriceNoOlderThan(bytes32,uint256)` from
  typed state.
- With the `redstone` feature, use `RedstoneOracleAdapter` and `RedstoneFeed`
  to register RedStone push price-feed wrappers. Discovery reads
  `getDataFeedId()`, `getPriceFeedAdapter()`, Chainlink-shaped metadata, and
  `latestRoundData()` through `EvmCache::call_sol`; live handling subscribes to
  multi-feed `ValueUpdate(uint256,bytes32,uint256)` adapter logs and merged
  Chainlink-shaped `AnswerUpdated` logs from the price-feed address.
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
- `redstone`: enables RedStone push-feed discovery helpers, event decoding, and
  `RedstoneReactiveHandler`.
- `live-tests`: enables websocket dependencies used by RPC-gated live tests.

## Support Matrix

| Adapter | Status | Direct storage writes | Authoritative fallback | Caveats |
| --- | --- | --- | --- | --- |
| Chainlink OCR2 | Stable | Yes. `NewTransmission` updates OCR2 hot vars and `s_transmissions[roundId]` when layout is detected and hot slots are warmed. | Proxy `latestRoundData()` reconciliation; purge/refetch when slots are cold or layout is unsupported. | Targets detected `AccessControlledOCR2Aggregator 1.0.0`-style layouts. Event values stay `EventPending` until proxy confirmation. |
| Chainlink OCR1 | Stable | Yes. `NewTransmission` updates common OCR1 hot vars and `s_transmissions[roundId]` for detected `AccessControlledOffchainAggregator` 2.x/3.x/4.x layouts. | Proxy `latestRoundData()` reconciliation; purge/refetch fallback. | OCR1 events do not carry a separate observations timestamp, so direct writes use block timestamp for both `startedAt` and `updatedAt`. |
| Chainlink-compatible `AnswerUpdated` | Stable fallback | Only when a verified layout adapter can translate the event; otherwise no generic storage writes. | Proxy `latestRoundData()` is authoritative. | `AnswerUpdated` alone does not fully describe proxy-composed round ids or every aggregator layout. |
| Aave V3 oracle sources | Beta | Underlying Chainlink dependencies can use the Chainlink direct-write path. Aave wrapper state is not written generically. | Cache-backed Aave source/proxy reads and dependency reconciliation. | Supports plain Chainlink, `PriceCapAdapterStable`, ratio-cap/CAPO, peg-to-base synchronicity, and fixed-price sources. Several less-common Aave source families are still skipped. |
| Morpho Blue Chainlink oracles | Beta | Shared underlying Chainlink feeds can use the Chainlink direct-write path once for all dependents. Morpho wrapper storage is not written. | Protocol-read reconciliation: dependency events queue a `DerivedProtocolRead` request, and `reconcile_derived` reads `price()` through the cache to promote `EventPending` values to `Confirmed`/`Corrected`. | Correction does not refresh stored dependency baselines; subsequent events recompute from discovery baselines until re-discovery. |
| Euler quote oracles | Beta | Shared underlying Chainlink legs can use the Chainlink direct-write path. Euler wrapper/router state is not written. | Protocol-read reconciliation: dependency events queue a `DerivedProtocolRead` request, and `reconcile_derived` reads `getQuote(one base unit)` through the cache to promote `EventPending` values to `Confirmed`/`Corrected`. | Supports direct `ChainlinkOracle`, `FixedRateOracle`, `RateProviderOracle`, and one-level `CrossAdapter` composition. Correction does not refresh stored leg baselines until re-discovery. |
| Pyth EVM | Beta | No Pyth contract storage writes. | Pyth typed state plus overlay `getPriceUnsafe` / `getPriceNoOlderThan`; conservative Pyth-contract invalidation. | Cold start seeds a genuine `getPriceUnsafe` read (`Confirmed`/`Proxy`; freshness governed by `max_age`), while event-derived updates stay `EventPending` until the caller's own confirmation policy accepts them. Removed-event state is repair-oriented. |
| RedStone push feeds | Beta | Yes for recognized hot multi-feed and price-feed adapter slots. | Wrapper/adapter purge/refetch and typed overlay reads. | Direct-write provenance and cold-slot prewarming are not yet as strong as Chainlink OCR1/OCR2. |
| Custom `OracleAdapterPlugin` | Extensible | Adapter-owned handlers may emit direct `StateUpdate`s and standard `OraclePriceUpdate` hooks. | Adapter-owned discovery/reconciliation policy. | Custom adapters should populate `round_status`, `value_status`, and `source` honestly; cache effects should use `evm_fork_cache::reactive::StateEffectQuality`. |

## Oracle Value Vocabulary

Pricing and liquidation code should inspect three fields on user-facing values:

| Field | Answers | Examples |
| --- | --- | --- |
| `OracleRoundStatus` | Is the round usable under the feed policy? | `Fresh`, `Stale`, `IncompleteRound`, `InvalidAnswer`, `Unknown` |
| `OracleValueStatus` | Where is this value in the event/reconciliation lifecycle? | `EventPending`, `Confirmed`, `Corrected`, `RequiresRepair`, `Unknown` |
| `OracleValueSource` | Which source produced this value? | `Proxy`, `Event`, `Derived`, `Mock`, `Unknown` |

`OracleFeedStatus` is different: it is feed/runtime readiness, not price
freshness. It appears on registration, warmup, build, repair, mutation, and
health/reporting surfaces. Successful initialized feeds are `Ready`; warmup,
layout, repair, or reconciliation failures for registered feeds should be
reported as `Degraded`; unsupported discovered sources are reported as
`Unsupported`. `Ready` never means a current price is fresh. Freshness is only
`OracleRoundStatus::Fresh`.

Important combinations:

| Situation | `round_status` | `value_status` | `source` |
| --- | --- | --- | --- |
| Stale but authoritative proxy read | `Stale { .. }` | `Confirmed` or `Corrected` | `Proxy` |
| Fresh event before reconciliation | `Fresh` | `EventPending` | `Event` |
| Event value corrected by proxy read | Classified from corrected round | `Corrected` | `Proxy` |
| Diagnostic value that needs repair | Best available classification | `RequiresRepair` | Usually `Event` or `Unknown` |

`Confirmed` means the value came from the authoritative source path and no
event/proxy mismatch required replacement. It does not imply the round is fresh.
`Corrected` means an event-derived value was superseded by an authoritative
source read. It also does not imply the corrected round is fresh.
`RequiresRepair` means the value/cache path is not trusted as final and needs an
authoritative repair. A payload may still carry a best-available diagnostic
value, but `PricePolicy` rejects it by default.

`PricePolicy` evaluates values in this order:

1. `round_status`: only `Fresh` is accepted.
2. `value_status`: `Confirmed` and `Corrected` are accepted; `EventPending`
   requires `allow_event_pending()`; `RequiresRepair` and `Unknown` are rejected.
3. `source`: `Proxy` is accepted by default; `Event` requires event-pending
   opt-in; `Mock` requires `allow_mock_source()`; `Derived` requires
   `allow_derived_source()`; `Unknown` is rejected.

Reactive cache-effect reports continue to use
`evm_fork_cache::reactive::StateEffectQuality` so direct storage writes and
repair-required cache effects are not conflated with price lifecycle.

### Batch digest and evm-amm-state correspondence

`OracleRuntime::apply_batch_report` returns an `OracleBatchReport`: the raw
per-event hooks plus a digested `feed_changes` list (`OracleFeedChange { feed,
kind, value_status, impact }`), typed `incidents`, and a
`requires_full_refresh` flag — the same consumer shape as `evm-amm-state`'s
`AmmSyncBatchReport { pool_changes, incidents, requires_full_refresh }`.
`OracleChangeImpact { value, actionability, routing }` mirrors amm's
`AmmChangeImpact { state, quoteability, topology }`. The quality ladders
correspond as: `Confirmed`/`Corrected` ≈ amm `Exact` writes, `EventPending` ≈
amm `ExactIfApplied`, and `RequiresRepair` is shared verbatim; `routing: true`
(an aggregator change) is the oracle's topology change and means handlers must
be refreshed before relying on event routing.

## Event to Simulation Flow

```text
AnswerUpdated, OCR1/OCR2 NewTransmission, Pyth PriceFeedUpdate, or RedStone ValueUpdate log
  -> adapter reactive handler decodes the event
  -> OraclePriceUpdate hook is emitted immediately
  -> OracleTracker stores an EventPending snapshot
  -> OracleReadOverlay serves registered oracle view calls from typed state
  -> OracleReconciler reads the proxy latestRoundData() (Chainlink-shaped sources),
     or reconcile_derived reads the protocol view call (Morpho price(), Euler getQuote)
  -> snapshot becomes Confirmed, Corrected, Stale, or Invalid
```

With `--features redstone`, RedStone push feeds follow the same hook and tracker
path:

```text
RedStone ValueUpdate or merged AnswerUpdated log
  -> RedstoneReactiveHandler decodes and filters the event
  -> OraclePriceUpdate hook is emitted immediately
  -> OracleTracker stores an EventPending snapshot
  -> OracleReadOverlay serves latestRoundData() from typed state
  -> hot RedStone storage slots are written directly, or wrapper/adapter storage is purged/refetched
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

```rust,ignore
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

```rust,ignore
let report = OracleAdapter::builder()
    .feed(Feed::proxy(eth_usd_proxy).id("eth-usd"))
    .build_report(&mut cache)
    .await?;

let oracle = OracleRuntime::from_tracker(report.tracker)
    .with_storage_sync(custom_storage_sync);
let warmup = oracle.prewarm_storage(&mut cache);
assert_eq!(warmup.failed_slots.len(), 0);
```

`prewarm_storage` uses `evm-fork-cache`'s installed storage batch fetcher. In
v0.3.0 that fetcher defaults to bulk `eth_call` storage extraction: target
contracts are temporarily run with a tiny extractor bytecode that performs raw
`SLOAD`s for the requested slots, and Multicall3 dispatches many target
contracts in a small number of calls. This is the fast path for known OCR hot
slots. The point-read `eth_getStorageAt` strategy is still available through
`EvmCacheBuilder::storage_fetch_strategy(StorageFetchStrategy::PointRead)` for
benchmarking or provider fallback.

Adapter-plugin runtimes expose the same work in their build report:

```rust,ignore
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
caller wants v0.3.0 cold-start verification through `EvmCache::run_cold_start`.
Cold-start mode first verifies declared hot slots, then runs discovery calls
such as `latestRoundData()` and bulk-verifies any newly discovered slots in a
second round. Known storage goes through raw SLOAD extraction; dynamic
metadata/proxy composition that cannot be represented safely as fixed slots
stays on typed view calls.

Cache-native Chainlink registration prefers direct RPC Multicall3 for proxy
view reads, then falls back to fork-cache simulation or sequential cache calls
when direct RPC batching is unavailable:

```rust,ignore
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

```rust,ignore
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

```rust,ignore
let mut installed = oracle.install_handlers_live_only(&mut engine)?;
assert!(!installed.is_empty());

let removed = oracle.uninstall_installed_handlers(&mut engine, &mut installed);
assert_eq!(removed.removed_handler_ids.len(), removed.handler_ids.len());
```

Use `install_handlers(&mut engine)` for continuity-safe registration that
backfills from the runtime's last canonical block, or
`install_handlers_with_backfill(&mut engine, SubscriberBackfill::from_block(n))`
for an explicit historical anchor. This is the preferred v0.3.0 path for
registering/unregistering oracle tracking at runtime; the older
`register_subscriber` helper remains a full-replacement bootstrap path.

Oracle registrations can also change after startup. Register already-discovered
feeds with `register_seeded_feed`, or install another cache-native adapter on an
existing runtime:

```rust,ignore
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

## Runtime Recipes

Deeper registration patterns over the quickstart: overlay-scoped price mocks
for simulation tests, the cache-native registration path, and declarative
adapter registration for each protocol family.

### Overlay-Scoped Price Mocks

For liquidation and risk-engine tests, create an `evm-fork-cache` mock overlay
and write an oracle price only into that simulation layer:

```rust,ignore
use alloy_primitives::I256;
use evm_oracle_state::{OracleOverlayMockExt, OraclePriceMock};

let mut sim = cache.mock_overlay();
let report = sim.mock_oracle_price(
    &cache,
    &oracle,
    "eth-usd",
    OraclePriceMock::new(I256::unchecked_from(175_000_000_000_i64))
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

```rust,ignore
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

```rust,ignore
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

```rust,ignore
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

```rust,ignore
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

```rust,ignore
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

```rust,ignore
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
- Morpho and Euler dependency events queue `DerivedProtocolRead` reconciliation
  requests, satisfied by `OracleRuntime::reconcile_derived(&mut cache)` (or
  `OracleTracker::reconcile_derived_pending_with` with a custom
  `OracleDerivedReader`): the protocol's own view call (`price()` /
  `getQuote`) promotes `EventPending` values to `Confirmed` or `Corrected`.
  Correction does not refresh the dependency baselines stored in the feed's
  source legs — subsequent dependency events recompute from the original
  discovery baselines until the feed is re-discovered.
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
- Direct Chainlink storage writes are limited to detected OCR1/OCR2 layouts;
  unsupported or unknown layouts fall back to purge/refetch.
- RedStone support is still a feature-gated beta surface. Direct writes are
  supported for recognized hot multi-feed and price-feed adapter slots, but
  RedStone slot cold-start prewarming and layout/code-hash provenance are not
  yet as strong as the Chainlink OCR1/OCR2 path.
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
- The crate is still pre-1.0. The high-level runtime, tracker, price, hook, and
  adapter-plugin APIs are intended public surfaces; low-level layout and storage
  helper details may still change while release hardening continues.

## Release Validation

The full pre-publish matrix (fmt, tests, all-features offline tests, clippy,
per-feature isolation checks, examples, docs, packaging) and the release
checklist live in [RELEASING.md](RELEASING.md); run it before publishing or
cutting a release PR.

For live release smoke, set `ETH_RPC_URL` or `E2E_RPC_URL` and run:

```sh
scripts/release_live_smoke.sh
```

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
