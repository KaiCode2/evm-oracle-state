# Benchmarks & performance

This documents how `evm-oracle-state` performs across its two distinct
profiles:

- **Reactive hot path — local CPU:** decode an oracle event, serve typed
  overlay reads, dedupe warm slots, and plan exact direct storage writes.
  No network anywhere.
- **Cold-start / repair path — provider-bound:** discover feed metadata, read
  latest values, detect aggregator layouts, and prewarm hot storage slots over
  RPC.

> **Local-CPU numbers below are real**, produced by
> [`benches/oracle_hot_path.rs`](../benches/oracle_hot_path.rs) (criterion,
> `bench` profile) on the host below. The provider-bound table is a
> point-in-time sample against one commercial endpoint — re-run it on your own
> endpoint before making endpoint-specific claims.

## Methodology

- **Host:** Apple M1 Pro, `arm64-apple-darwin`. Measured 2026-07-10 on the
  v0.1.0 release candidate.
- **Local CPU (offline):** `cargo bench --bench oracle_hot_path` — criterion
  over fully synthetic, in-memory fixtures (mock-backed, no RPC, no env vars;
  nothing measured is feature-gated, so it builds with default features). The
  measured closures mirror the crate's example benches exactly
  ([`examples/oracle_cpu_overhead_bench.rs`](../examples/oracle_cpu_overhead_bench.rs)
  and the direct-sync microbench inside
  [`examples/oracle_storage_warmup_bench.rs`](../examples/oracle_storage_warmup_bench.rs)),
  so criterion medians and the examples' ns-per-iter output stay comparable.
- **Live RPC (provider-bound):** `oracle_storage_warmup_bench` against
  `ETH_RPC_URL` or `E2E_RPC_URL` with the curated Ethereum mainnet Chainlink
  fixture. Without an RPC env var the example still runs its offline
  direct-sync microbench and skips the live sections.
- **Reproduce:**

  ```sh
  # Fully offline; runs anywhere, including CI, for regression tracking.
  cargo bench --bench oracle_hot_path

  # The example harnesses the criterion medians are cross-checked against.
  cargo run --release --example oracle_cpu_overhead_bench
  cargo run --release --example oracle_storage_warmup_bench   # offline microbench, live parts skipped

  # Live cold-start/prewarm (the provider-bound rows).
  ETH_RPC_URL=<https-mainnet-rpc> \
  ORACLE_BENCH_FEED_SET=ethereum-mainnet-curated \
  ORACLE_BENCH_SKIP_FORK_BREAKDOWN=1 \
  ORACLE_BENCH_SKIP_SEQUENTIAL=1 \
  cargo run --release --example oracle_storage_warmup_bench --features aave,morpho,euler,pyth,redstone
  ```

### Feed fixtures

`oracle_storage_warmup_bench` supports both small smoke checks and larger
release benchmarks:

- `ORACLE_BENCH_FEED_SET=smoke`: the default 4-feed smoke fixture.
- `ORACLE_BENCH_FEED_SET=ethereum-mainnet-curated`: a larger best-effort
  Ethereum mainnet Chainlink fixture.
- `ORACLE_BENCH_FEEDS=0x...,0x...`: explicit comma-separated proxy addresses.
- `ORACLE_BENCH_FEEDS_FILE=feeds.csv`: one `proxy` per line, or
  `label,base,quote,proxy` per line.
- `ORACLE_BENCH_FEED_LIMIT=50`: truncate any fixture for 50-feed, 100-feed, or
  larger comparisons.

Use explicit feed files for publication-grade benchmark claims. The built-in
curated set is meant to make release regressions easy to spot, while downstream
users may prefer protocol-specific feed lists.

## Results

> Point-in-time medians from a single host and run (2026-07-10, the v0.1.0
> release candidate, the Methodology host above) — treat them as
> order-of-magnitude, and re-run the reproduce commands for numbers on your own
> machine. `cargo bench` prints Criterion's timing estimates
> (`time: [low mid high]` plus outlier counts) to stdout — add `-- --verbose`
> for mean/median/std-dev — and saves baseline data under `target/criterion/`.
> (This crate builds criterion without the `plotters` backend, so HTML reports
> are only generated if `gnuplot` is installed.)

### Reactive hot path — local CPU

From `cargo bench --bench oracle_hot_path` (fully offline):

| Operation | Fixture | Median | Notes |
| --- | --- | ---: | --- |
| Warm-slot dedupe | 1,024 OCR2 registrations | **~116 µs** | one pass over every registration × storage adapter with BTreeSet dedupe of the hot slots to prewarm — ~114 ns/feed, re-runnable on every registry change |
| Pyth overlay lookup (`getPriceUnsafe`) | 1,024 feeds, worst-case (last) price id | **~173 ns** | ABI decode + typed round lookup + ABI re-encode, served entirely from in-memory tracker state |
| Direct-sync cold fallback decision | hot packed word absent | **~12 ns** | both layout adapters decline; the caller falls back to purge/refetch |
| Direct-sync warm exact write planning | hot packed word present | **~253 ns** | plans the masked OCR2 `s_hotVars` round-id write plus the packed `s_transmissions[round_id]` mapping-slot write |

The bench mirrors the example harnesses, and the same session's example output
agrees: `oracle_cpu_overhead_bench` printed `ns_per_iter=123954` (dedupe) and
`ns_per_call=172` (overlay), and the `oracle_storage_warmup_bench` microbench
printed `cold_fallback_ns=8` / `warm_exact_ns=263`. At the cold-fallback's
tens-of-nanoseconds scale, harness overhead and machine load dominate the
spread — criterion medians across repeated runs ranged ~11–20 ns.

### Cold-start / repair path — provider-bound

Point-in-time samples (July 2026) from `oracle_storage_warmup_bench` against a
commercial Ethereum mainnet endpoint with the curated Chainlink fixture. These
rows time live RPC round-trips, not local CPU:

| Operation | Feeds | Sample result | Notes |
| --- | ---: | ---: | --- |
| Direct RPC Multicall core reads | 24 | **261 ms** | `latestRoundData`, metadata, and aggregator reads. |
| Direct RPC Multicall layout reads | 24 aggregators | **88 ms** | Batched `typeAndVersion()` layout detection. |
| Cache-native registration with multicall | 24 | **428 ms** | Registered 24 feeds, 0 skipped. Provider-bound. |
| Bulk hot-slot prewarm | 24 slots | **98 ms** | `evm-fork-cache` bulk storage extraction path. |
| Point hot-slot prewarm | 24 slots | **160 ms** | Point-read comparison path. |

Interpretation:

- Cold start is dominated by metadata/layout RPCs, not storage-slot prewarm.
- Bulk storage extraction is the preferred storage prewarm path, but provider
  jitter can make small slot counts noisy; use larger feed files for stable
  provider comparisons.

## How this compares

Three regimes exist for keeping simulated oracle state current, and the tables
above quantify the first two ends:

- **Direct event application (this crate's hot path):** when a detected layout
  adapter supports the feed and its hot packed word is warm, applying an oracle
  event costs **~253 ns** of local write planning plus exact cache writes, and
  typed overlay reads answer registered view calls in **~173 ns** — effectively
  free next to an EVM liquidation or pricing simulation.
- **Purge/refetch round-trip (the conservative fallback):** when no adapter
  supports the feed, deciding that costs **~12 ns**, but the repair itself is a
  provider round-trip — tens to hundreds of milliseconds per batch (see the
  provider-bound table). That is five to six orders of magnitude above the
  direct path, which is why layout detection and hot-slot prewarm exist.
- **Always-RPC reads (no local state at all):** reading `latestRoundData()`
  through `eth_call` on every simulation pays a network round-trip per read,
  is rate-limited, and cannot run offline. The cold-start tables put one
  batched 24-feed read round at ~260 ms — a budget the local hot path clears
  millions of times over.

## Caveats

- Single machine (M1 Pro), one run, synthetic fixtures at one size (1,024
  feeds for dedupe/overlay). Treat the numbers as order-of-magnitude and
  re-run `cargo bench --bench oracle_hot_path` on your own hardware.
- The cold-fallback decision sits at tens of nanoseconds, where criterion
  harness overhead and background load visibly move the estimate (~11–20 ns
  across runs on a loaded host); the plain-loop example reports 8 ns/iter.
- The provider-bound rows are point-in-time (July 2026) samples against one
  commercial endpoint; provider jitter is significant at small slot counts.
  Re-run `oracle_storage_warmup_bench` on your own endpoint before making any
  endpoint-specific claims.
- The built-in curated fixture is intentionally conservative. Use explicit
  feed files (`ORACLE_BENCH_FEEDS_FILE`) for protocol-specific or larger
  benchmark claims.
