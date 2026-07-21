# Pending oracle integration

The compile-checked example is
[`pending_oracle_updates.rs`](pending_oracle_updates.rs). It demonstrates the
intended UX:

1. register each feed once through `OracleRuntime::builder`;
2. opt that same registry into the independent pending channel;
3. optionally start the Alchemy public-mempool and MEV-Share sources;
4. consume `PendingOracleEvent` without changing canonical oracle state; and
5. select the most actionable transmission for downstream simulation and bundle
   construction.

Run the deterministic offline path:

```sh
cargo run --example pending_oracle_updates --features pending-oracle-updates
```

Compile and enable the built-in transports:

```sh
cargo run --example pending_oracle_updates \
  --features pending-oracle-ethereum,pending-oracle-mev-share
```

Set `EVM_ORACLE_ALCHEMY_WS_URL` to attach the Alchemy full pending-transaction
stream. Set `EVM_ORACLE_ENABLE_MEV_SHARE=1` to attach the public Flashbots
MEV-Share stream. Those sources are best effort and Ethereum-mainnet-only; see
[`../docs/pending-oracle-updates.md`](../docs/pending-oracle-updates.md) for the
coverage and ordering limitations.
