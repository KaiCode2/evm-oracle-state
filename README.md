# evm-oracle-state

`evm-oracle-state` tracks Chainlink-compatible, single-value oracle feeds
through their stable proxy addresses. It is a domain crate layered over
`evm-fork-cache`: `evm-fork-cache` owns generic cache invalidation, reactive
batching, and reorg reporting, while this crate owns oracle-specific metadata,
event decoding, snapshots, staleness policy, reconciliation, and feed lookup.

The central guarantee is:

> Events are fast-path hints; proxy reads are authoritative.

`AnswerUpdated` logs update typed state immediately and invalidate cached proxy
and aggregator storage as a conservative fallback. The crate can also serve
registered oracle view calls from typed state through `OracleReadOverlay`, so a
simulation can observe the event price before a follow-up RPC read completes.
Startup, periodic repair, aggregator changes, and post-reorg safety still use
`latestRoundData()` through the proxy as the source of truth.

## MVP Surface

- Register Chainlink-compatible proxy feeds with a `ChainlinkFeedProvider`.
- Read `decimals()`, `description()`, `version()`, `latestRoundData()`, and
  best-effort `aggregator()` metadata at registration.
- Look up typed snapshots by proxy address with freshness classification.
- Decode `AnswerUpdated(int256,uint256,uint256)` and
  `NewRound(uint256,address,uint256)` logs.
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
- Build a higher-level `OracleRuntime` with `ChainlinkFeed` registrations and
  optional typed event callbacks.
- Queue event-specific, block-identified proxy reconciliation requests after
  committed price updates.
- Serve simulation-facing `latestRoundData()`, `decimals()`, `description()`,
  and `version()` calls through `OracleReadOverlay` from typed state without
  writing Chainlink storage slots.
- Configure verified `OracleStorageAdapter`s. If an adapter supports a feed,
  events can emit direct `StateUpdate`s; otherwise the handler falls back to
  conservative proxy/aggregator purge.
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

## Event to Simulation Flow

```text
AnswerUpdated log
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
- Raw EVM storage is purged/refetched unless a verified deployment-specific
  storage adapter supports the feed.

## Ergonomic Runtime

```rust
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
let price = oracle.price("eth-usd")?;
```

## Limitations

- No durable database or indexer.
- No Chainlink Feed Registry integration.
- No direct storage-slot writes or Chainlink layout assumptions.
- No in-cache generic call interceptor in the pinned `evm-fork-cache` revision;
  use `OracleReadOverlay` as the simulation read adapter.
- No trading, liquidation, or transaction submission behavior.
- Aggregator routing is best effort and should be rebuilt after reconciliation
  reports aggregator changes.

## Example

The example is network-gated:

```sh
cargo run --example chainlink_live_probe
ETH_RPC_URL=https://example-rpc.invalid cargo run --example chainlink_live_probe
```

When `ETH_RPC_URL` is unset, it exits successfully without making network calls.
Downstream live integrations should provide a `ChainlinkFeedProvider` backed by
their preferred RPC, multicall, or cache layer and then register feeds with
`OracleRegistry`.
