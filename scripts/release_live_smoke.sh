#!/usr/bin/env bash
set -euo pipefail

if [[ -f .env ]]; then
  set -a
  # shellcheck disable=SC1091
  source .env
  set +a
fi

if [[ -z "${ETH_RPC_URL:-${E2E_RPC_URL:-}}" ]]; then
  echo "ETH_RPC_URL/E2E_RPC_URL is required for release live smoke."
  exit 1
fi

if [[ -z "${ETH_RPC_URL:-}" && -n "${E2E_RPC_URL:-}" ]]; then
  export ETH_RPC_URL="$E2E_RPC_URL"
fi

if [[ -z "${ETH_WS_RPC_URL:-}" && -n "${WS_RPC_URL:-}" ]]; then
  export ETH_WS_RPC_URL="$WS_RPC_URL"
fi

if [[ -z "${ETH_WS_RPC_URL:-}" && "${ETH_RPC_URL:-}" == https://* ]]; then
  export ETH_WS_RPC_URL="wss://${ETH_RPC_URL#https://}"
fi

echo "== Chainlink OCR2 historical replay =="
cargo test --features live-tests ocr2_listener_replays_real_eth_usd_update -- --nocapture

echo "== Chainlink cold-start benchmark =="
ORACLE_BENCH_FEED_SET="${ORACLE_BENCH_FEED_SET:-ethereum-mainnet-curated}" \
ORACLE_BENCH_SKIP_FORK_BREAKDOWN="${ORACLE_BENCH_SKIP_FORK_BREAKDOWN:-1}" \
ORACLE_BENCH_SKIP_SEQUENTIAL="${ORACLE_BENCH_SKIP_SEQUENTIAL:-1}" \
cargo run --release --example oracle_storage_warmup_bench --features aave,morpho,euler,pyth,redstone

echo "== Chainlink reactive smoke =="
SMOKE_DURATION_SECS="${SMOKE_DURATION_SECS:-60}" \
cargo run --example chainlink_reactive_smoke

echo "== Aave V3 cold smoke =="
AAVE_DEMO_DURATION_SECS="${AAVE_DEMO_DURATION_SECS:-0}" \
cargo run --features aave --example aave_v3_health_factor -- "${AAVE_SMOKE_USER:-0xcc144c2e9fc40f92257b2cd7cc43e1f82666f09a}"

if [[ -n "${PYTH_CONTRACT:-}" && -n "${PYTH_PRICE_ID:-}" ]]; then
  echo "== Pyth cold/live smoke =="
  PYTH_DEMO_DURATION_SECS="${PYTH_DEMO_DURATION_SECS:-0}" \
  cargo run --features pyth --example pyth_push_demo
else
  echo "== Pyth smoke skipped: set PYTH_CONTRACT and PYTH_PRICE_ID =="
fi

if [[ -n "${REDSTONE_PRICE_FEED:-}" ]]; then
  echo "== RedStone cold/live smoke =="
  REDSTONE_DEMO_DURATION_SECS="${REDSTONE_DEMO_DURATION_SECS:-0}" \
  cargo run --features redstone --example redstone_push_demo
else
  echo "== RedStone smoke skipped: set REDSTONE_PRICE_FEED =="
fi

if [[ -n "${MORPHO_MARKET_ID:-}" || -n "${MORPHO_ORACLE:-}" || -n "${EULER_ROUTER:-}" || -n "${EULER_ORACLE:-}" ]]; then
  echo "== Morpho/Euler cold/live smoke =="
  MORPHO_EULER_DEMO_DURATION_SECS="${MORPHO_EULER_DEMO_DURATION_SECS:-0}" \
  cargo run --features morpho,euler --example morpho_euler_reactive_value
else
  echo "== Morpho/Euler smoke skipped: set MORPHO_MARKET_ID/MORPHO_ORACLE or EULER_ROUTER/EULER_ORACLE =="
fi
