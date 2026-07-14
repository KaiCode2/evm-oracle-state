# Changelog

All notable changes to `evm-oracle-state` are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.0] - 2026-07-14

### Added

- `OracleRuntime::with_storage_sync` preserves deployment-specific direct-write
  registries after cache-native/Aave discovery, so callers can use the
  high-level runtime, storage warmup, and typed batch digest without manually
  rebuilding reactive handlers.

### Changed

- Advanced the crate to `0.2.0` and aligned its public `evm-fork-cache`
  dependency with unreleased `0.3.0`. Because cache types appear in the public
  API, this keeps downstream type identity coherent with `evm-amm-state 0.2.0`
  and `evm-amm-search 0.1.0`.

## 0.1.0 - 2026-07-10

Initial development milestone. This version was not published to crates.io;
`0.2.0` is the first public release.

### Added

**Chainlink-compatible feed tracking** — register proxy feeds with typed
`Feed` / `ChainlinkFeed` builders. Registration reads `decimals()`,
`description()`, `version()`, `latestRoundData()`, and best-effort
`aggregator()` metadata (batched through direct Multicall3 `eth_call`s where
available), classifies aggregator layouts from `typeAndVersion()` plus cached
runtime code-hash evidence, and yields typed `OracleSnapshot`s with explicit
`round_status` / `value_status` / `source` classification.

**Direct OCR1/OCR2 storage sync** — for detected
`AccessControlledOffchainAggregator` 2.x/3.x/4.x (OCR1) and
`AccessControlledOCR2Aggregator 1.0.0`-style (OCR2) layouts, `NewTransmission`
events write the packed hot-vars fields and the `s_transmissions[roundId]`
mapping entry straight into the cache. Adapters only claim events when a
supported layout was detected and the packed hot slot is warm; everything else
keeps the conservative aggregator/proxy purge/refetch fallback.

**Dependency-aware reactive routing** — `OracleReactiveHandler` groups feed
registrations by their underlying `(aggregator, event family)` dependency key:
one log interest per dependency, one dependency-level cache effect per event,
and one normalized `OraclePriceUpdate` hook fanned out per affected user-facing
feed (a Morpho oracle and an Euler oracle sharing the same ETH/USD aggregator
subscribe once and each get their own normalized hook).

**Reconciliation lifecycle** — events are fast-path hints; proxy reads are
authoritative. `OracleReconciler` reads `latestRoundData()` through the proxy,
promotes `EventPending` snapshots to `Confirmed` or `Corrected`, emits
`OraclePriceStale` / `OracleAggregatorChanged` hook events, queues
event-specific block-identified reconciliation requests after committed
updates, and treats removed (reorged-out) event state conservatively.

**Typed price vocabulary** — `AssetId`, `Denomination`, `TokenAmount`, and
`PricePolicy`: promote raw oracle answers into policy-vetted `CheckedPrice`
values (`round_status` / `value_status` / `source` are evaluated in order)
before liquidation or valuation decisions, and value token amounts with
checked arithmetic through `CheckedPrice::value_of`.

**`OracleReadOverlay`** — serve registered oracle view calls
(`latestRoundData()`, `decimals()`, `description()`, `version()`, and adapter
reads such as Morpho `price()`, Euler `getQuote(...)`/`getQuotes(...)`, Pyth
`getPriceUnsafe(...)`/`getPriceNoOlderThan(...)`) from typed state, so a
simulation can observe an event price before a follow-up RPC read completes.

**Overlay-scoped price mocks** — `OracleOverlayMockExt` / `OraclePriceMock`
write an oracle price only into an `evm-fork-cache` mock overlay, using the
same storage slots a live event would update for detected OCR1/OCR2 feeds;
dropping the overlay discards the mock.

**Adapter plugins** — `OracleAdapterPlugin` is the extension seam for oracle
families that are not plain Chainlink proxies: cache-backed discovery returns
seeded registrations plus rounds, and the adapter owns its
`evm-fork-cache` reactive handler while reusing the standard hook, tracker,
price, and overlay surfaces.

**Feature-gated adapters** (each behind its own opt-in feature flag):

- **Aave V3** (`aave`) — `AaveV3OracleAdapter` discovers oracle sources from
  `getSourceOfAsset`, including `PriceCapAdapterStable`, ratio-cap/CAPO,
  peg-to-base Chainlink synchronicity, and fixed-price sources.
- **Morpho Blue** (`morpho`) — `MorphoBlueOracleAdapter` registers markets or
  direct `MorphoChainlinkOracleV2` addresses; Chainlink dependency events
  recompute the 36-decimal `price()` immediately.
- **Euler** (`euler`) — `EulerOracleAdapter` registers direct quote adapters
  or router-resolved pairs (`ChainlinkOracle`, `FixedRateOracle`,
  `RateProviderOracle`, one-level `CrossAdapter` composition).
- **Pyth EVM** (`pyth`) — `PythOracleAdapter` registers price ids under a
  shared Pyth contract with synthetic per-feed state keys; `PriceFeedUpdate`
  events update typed state and the overlay serves the Pyth read ABI.
- **RedStone** (`redstone`) — `RedstoneOracleAdapter` registers push
  price-feed wrappers; multi-feed `ValueUpdate` and merged Chainlink-shaped
  `AnswerUpdated` logs route through `RedstoneReactiveHandler` with direct
  writes for recognized hot slots.

**Storage warmup, cold start, and code warmup** — cache-native builds prewarm
declared OCR hot slots by default through `EvmCache::prewarm_slots` (bulk
`eth_call` storage extraction), so the direct-write path is ready before the
reactive loop starts; `storage_cold_start()` / `cold_start_storage` run
verification-grade warmup through `EvmCache::run_cold_start`; and
`code_seed` / `code_etch` install oracle bytecode up front under an
`OracleCodeWarmupPolicy` (verified once against chain code hashes, with
explicit etches reported separately).

### Pre-release hardening

Release-validation fixes that landed while preparing 0.1.0:

- Euler router multi-pair registration: each quote pair now gets a synthetic
  per-feed state key (multiple pairs on one router no longer collide), and
  cold start seeds a genuine `getQuote` read instead of a placeholder.
- Removed-log events (reorged-out logs) now always take the purge/refetch
  fallback — direct storage writes are never applied for log removals.
- Morpho `price()` recomputation widened to 512-bit `mulDiv`, so large
  intermediate products no longer overflow.
- `cargo test --all-features` passes offline: the RPC-gated live tests skip
  cleanly when no RPC endpoint is configured.
- Error taxonomy split into typed variants — policy, configuration, reactive,
  and unsupported-source failures are no longer mislabeled as `Provider`
  errors — with structured payloads: `Policy(PricePolicyViolation)` (typed
  per-check violations with fields), `Config(OracleConfigError)`,
  `FeedSkipped(Box<OracleFeedSkip>)` (feed id/label, proxy, classified skip
  reason), and `CodePolicy(Box<OracleCodePolicyViolation>)`.
- Derived-source reconciliation: Morpho/Euler dependency events queue
  `DerivedProtocolRead` requests; `OracleRuntime::reconcile_derived` (or
  `OracleTracker::reconcile_derived_pending_with` with a custom
  `OracleDerivedReader`) reads the protocol view call (`price()` /
  `getQuote`) to promote `EventPending` values to `Confirmed`/`Corrected`.
- Typed batch digest aligned with `evm-amm-state`:
  `OracleRuntime::apply_batch_report` now returns `OracleBatchReport`
  (raw `events` plus `feed_changes`, `incidents`, `requires_full_refresh`),
  with `OracleFeedChange`/`OracleChangeImpact` mirroring
  `AmmSyncPoolChange`/`AmmChangeImpact`.
- O(N) feed-id lookup index in the registry and reduced registration cloning
  on the hot registration paths.
- New offline examples: an overlay price-mock demo and a custom
  `OracleAdapterPlugin` demo.
- Criterion-based offline benchmark plus [`docs/benchmarks.md`](docs/benchmarks.md).
- Curated fixture fix: the reactive smoke's MATIC/USD entry pointed at a
  mistyped (codeless) address; corrected to the verified on-chain proxy.

[Unreleased]: https://github.com/KaiCode2/evm-oracle-state/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/KaiCode2/evm-oracle-state/releases/tag/v0.2.0
