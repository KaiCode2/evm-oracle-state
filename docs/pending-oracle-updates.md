# Pending Oracle Updates

`evm-oracle-state` has an opt-in speculative channel for observing an oracle
transaction before it is canonically included. It is intended for searchers
that need time to build a disposable fork, simulate a liquidation or other
oracle-dependent action, sign their transaction, and submit through an ordering
system.

Pending data is not authoritative. It can disappear, be replaced, revert, land
without the expected oracle event, or arrive too late to act on. Enabling this
channel never changes `OracleTracker`, `latest_round`, confirmed hooks, or
reconciliation status.

## Feature gates

The domain model has no network I/O:

```toml
evm-oracle-state = { version = "0.3.0-alpha.1", features = ["pending-oracle-updates"] }
```

Add only the transports you use:

```toml
evm-oracle-state = { version = "0.3.0-alpha.1", features = [
  "pending-oracle-ethereum",
  "pending-oracle-mev-share",
] }
```

- `pending-oracle-ethereum` adds an Alchemy WebSocket source using
  `alchemy_pendingTransactions` with full transaction bodies.
- `pending-oracle-mev-share` adds the Flashbots MEV-Share SSE source.
- Neither is enabled by default.

## Register feeds once

Pending configuration projects the existing runtime registrations. Proxy,
aggregator, feed id, decimals, and metadata are not repeated:

```rust,ignore
let mut oracle = OracleRuntime::builder(provider)
    .feed(ChainlinkFeed::new(eth_usd_proxy).id("eth-usd"))
    .feed(ChainlinkFeed::new(btc_usd_proxy).id("btc-usd"))
    .pending_updates(
        PendingOracleConfig::ethereum_mainnet()
            .public_mempool()
            .mev_share()
            .registered_feeds(PendingFeedFilter::only(["eth-usd"])),
    )
    .build()
    .await?;

let mut pending = oracle.pending_updates().unwrap().subscribe();

oracle.start_pending_source(
    AlchemyPendingTransactionSource::new(alchemy_ws_url),
)?;
oracle.start_pending_source(
    MevSharePendingTransactionSource::ethereum_mainnet(),
)?;
```

Runtime feed registration, removal, and aggregator refresh update the pending
scope through the same mutation methods used by the confirmed runtime. A custom
pending adapter installed with `.pending_adapter(adapter)` receives the same
registration snapshot.

## Separate pipelines

Confirmed and speculative traffic deliberately use different receivers:

```text
one OracleRuntime registration set
  |-- confirmed subscriber -> committed logs -> OracleTracker -> confirmed hooks
  `-- pending sources -> PendingOracleAdapter -> pending tracker -> pending receiver
```

`register_subscriber` registers committed-log interests only. It does **not** ask
`evm-fork-cache::AlloySubscriber` for full pending transactions: version 0.3 of
that subscriber only supports pending hashes, which are insufficient for oracle
calldata decoding or exact simulation. Pending sources are started explicitly
and owned by `OracleRuntime`; dropping the runtime stops them.

This split lets a searcher consume pending events and run relatively expensive
simulations without delaying canonical reconciliation.

## Searcher handoff

Each `PendingOracleUpdate` contains one or more transport variants. A variant
separates report identity from transaction identity because the same signed OCR
report can be observed through multiple transmitters or sources.

The most important fields are:

- `update.id`: adapter-defined report/content identity;
- `update.feed_ids`: affected ids from the existing registry;
- `update.proposed_value`: decoded answer and oracle timestamp, when available;
- `transmission.id`: one transport variant;
- `transmission.ordering.transaction_hash()`: transaction or MEV-Share hash;
- `transmission.ordering`: exact signed bytes, MEV-Share hash, hash-only, or a
  future sequencer preconfirmation;
- `transmission.simulation`: the fidelity the disclosed material supports;
- `transmission.observed_at_head`: the local chain head, when the source knows it.

Typical consumer flow:

```rust,ignore
while let Ok(streamed) = pending.recv().await {
    let PendingOracleEvent::Observed(update) = streamed.event else { continue };
    let Some(tx) = update.transmissions
        .iter()
        .filter(|tx| tx.is_referenceable())
        .max_by_key(|tx| tx.actionability_rank())
    else { continue };

    // 1. Create a disposable fork at the observed head.
    // 2. Apply tx.signed_envelope when exact material is present, otherwise use
    //    the explicitly weaker answer-only projection.
    // 3. Simulate and sign the searcher's transaction.
    // 4. Submit for one target block with the ordering handle required by the relay.
}
```

The crate does not submit bundles. A transaction hash is exposed because it is
useful to downstream builders, but `TransactionHashOnly` does not claim that a
particular relay accepts it as an atomic ordering constraint. `MevShare { hash }`
and `RawEthereumTransaction` carry stronger, source-specific material.

The Alchemy source reconstructs the EIP-2718 signed envelope and verifies that
its Keccak hash equals the transaction hash. The Chainlink adapter advertises
`ExactSignedTransaction` only for hash-matching bytes; unverified bytes are
downgraded to `AnswerOnlyProjection`.

## Chainlink coverage

The built-in `ChainlinkPendingAdapter` currently recognizes Ethereum OCR2 calls:

- direct `transmit`;
- direct `transmitSecondary`;
- `forward(address,bytes)` wrapping either entry point;
- reports targeting a current aggregator inherited from a plain
  `FeedSource::Chainlink` registration.

It decodes the report timestamp and observations, derives the median answer, and
deduplicates the report across transaction variants. It does not currently cover
OCR1, unrecognized OCR report layouts, unregistered aggregators, or MEV-Share
hints that omit the calldata needed to decode the report.

Derived Aave, Morpho, and Euler registrations are intentionally excluded even
when they depend on a Chainlink aggregator. The underlying OCR answer is not
necessarily their user-facing value; those families need pending adapters that
apply their protocol-specific transform or recomputation.

A committed OCR2 `NewTransmission` automatically resolves a pending variant as
landed only when:

1. the committed transaction hash matches the variant's ordering handle;
2. the emitting aggregator matches the decoded report;
3. the confirmed answer and observations timestamp match; and
4. the log is canonical rather than removed.

Receipt-only success is not treated as oracle confirmation. Callers that also
monitor receipts can use `resolve_confirmed_transmission` to record reverted or
successful-without-update attempts explicitly.

## Generic adapter API

Pending support is not Chainlink-specific. Implement:

```rust,ignore
pub trait PendingOracleAdapter: Send + Sync + 'static {
    fn adapter_id(&self) -> OracleAdapterId;
    fn interests(&self, registrations: &[FeedRegistration])
        -> Vec<PendingOracleInterest>;
    fn decode(
        &self,
        candidate: &PendingTransportCandidate,
        registrations: &[FeedRegistration],
    ) -> Result<Vec<PendingOracleUpdate>, PendingOracleAdapterError>;
    fn confirms(&self, update: &PendingOracleUpdate, log: &Log) -> bool;
}
```

Then install it alongside pending configuration:

```rust,ignore
let oracle = OracleRuntime::builder(provider)
    .feed(existing_feed)
    .pending_updates(PendingOracleConfig::ethereum_mainnet().public_mempool())
    .pending_adapter(MyPendingAdapter)
    .build()
    .await?;
```

Interests are source optimization hints; adapters must still validate calldata.
An empty interest address set means any destination. Adapter output is checked
against the inherited feed scope before it is published. `observe_candidate`
returns a `PendingOracleCandidateReport`: successful observations and
adapter-local failures are reported separately, and one decoder failure never
prevents another interested adapter from processing the same candidate.

## Source health and gaps

Each runtime-owned source emits:

- `SourceHealthChanged(Connecting | Ready | Degraded | Stopped)`;
- `CoverageGap` when a WebSocket or SSE interval is lost;
- timestamps for the last transport message and adapter candidate;
- a cumulative coverage-gap count and the latest error.

Pending traffic cannot be backfilled reliably, so reconnecting does not erase a
gap. Receivers are bounded and sequence numbered; a slow receiver gets
`PendingOracleRecvError::Lagged { missed }` rather than silently skipping data.

Provider terms, pending-stream availability, rate limits, and geography can all
affect coverage. Run at least two independent sources for serious use and treat
silence as meaningful only when their health is `Ready`.

## Chain boundaries

The built-in source presets use Ethereum mainnet semantics. Every candidate
must match the runtime's explicit chain id or it fails closed. A caller-owned
source for another chain opts in without relabeling its transport:

```rust,ignore
let source = PendingOracleSource::custom("base-flashblocks");
let config = PendingOracleConfig::for_chain(8453).source(source.clone());

// The source descriptor and every emitted candidate use the same `source` and
// chain id. A sequencer source can attach `SequencerPreconfirmation` ordering.
```

| Source | What is observable | Ordering/actionability | Status |
| --- | --- | --- | --- |
| Ethereum Alchemy pending stream | Publicly propagated full transactions | Hash-verified signed envelope | Experimental |
| Ethereum MEV-Share | Partial private-order-flow hints | MEV-Share hash when calldata is disclosed | Experimental |
| Base Flashblocks | Sequencer-preconfirmed prefixes | Requires a different source and ordering contract | Not implemented |
| OP Stack sequencer pending views | Provider-dependent pending traffic | No generic atomic-after guarantee | Not implemented |
| Arbitrum sequencer mempool | Not publicly exposed as an Ethereum-style mempool | Unsupported | Unsupported |

Do not instantiate an Ethereum source and relabel it as L2 support. L2 sources
can implement `PendingOracleCandidateSource`, use a caller-defined
`PendingOracleSource`, attach an honest `PendingOracleOrderingHandle`, and reuse
the same adapter/channel lifecycle. This extension seam does not itself make
Base, OP Stack, or Arbitrum observable; each still needs a real source and its
own coverage experiment.

## Release limitations

This is a prerelease surface. In particular:

- observation is best effort, never complete;
- public propagation can leave only roughly one or two seconds before inclusion;
- MEV-Share frequently discloses too little calldata to decode a report;
- replacement, dropped, and reverted attempts require receipt/block monitoring
  beyond the two source streams;
- exact simulation still requires a disposable fork at the correct block state;
- bundle construction and relay submission remain application responsibilities;
- the first shipped oracle-family adapter covers Chainlink OCR2 only.

See `examples/pending_oracle_updates.rs` for a compile-checked integration that
uses one registration set, an independent pending receiver, optional built-in
sources, and a searcher-oriented transaction-hash handoff.
