# Releasing `evm-oracle-state`

Maintainer checklist for cutting and publishing a release. The public
`evm-fork-cache 0.4.0` dependency must resolve from crates.io with no sibling
path override before packaging. Work the sections below in order.

## 1. Pre-publish verification

Run the full matrix locally (or confirm green CI on the release commit). These
mirror `.github/workflows/ci.yml`, which pins Rust **1.90.0** — the pinned
toolchain doubles as the MSRV gate for `rust-version = "1.90"`:

```bash
cargo fmt --check
cargo test --locked
cargo test --locked --features aave,morpho,euler,pyth,redstone,pending-oracle-updates,pending-oracle-ethereum,pending-oracle-mev-share
# --all-features additionally enables `live-tests`; the RPC-gated live tests
# skip cleanly when no RPC endpoint is configured, so this must pass offline:
cargo test --locked --all-features
cargo clippy --locked --all-targets --all-features -- -D warnings
# per-feature isolation (catches cfg-gating bugs the combined build masks —
# code that only compiles because a sibling feature happens to be on):
for f in aave morpho euler pyth redstone pending-oracle-updates pending-oracle-ethereum pending-oracle-mev-share; do
  cargo check --locked --no-default-features --features "$f"
done
cargo check --locked --examples --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --all-features
cargo package --locked
```

## 2. Live release smoke

The env-gated live smoke is the on-chain ground truth: it always replays the
pinned Chainlink OCR2 fork fixture and runs the cold-start benchmark, then
cold-starts the optional Pyth / RedStone / Aave / Morpho / Euler demos when
their environment variables are configured. Optional demos default to zero
live-subscription seconds, so the run validates discovery and registration
without waiting for a fresh oracle event.

```bash
set -a; . ./.env; set +a   # RPC URLs live in .env, gitignored; never commit or echo them
scripts/release_live_smoke.sh
```

| Env var | Required | Enables |
| --- | --- | --- |
| `ETH_RPC_URL` (or `E2E_RPC_URL`) | yes | all live smoke stages |
| `ETH_WS_RPC_URL` (or `WS_RPC_URL`) | no — derived from an `https://` URL when unset | websocket subscriptions |
| `PYTH_CONTRACT` + `PYTH_PRICE_ID` | no | Pyth cold/live smoke |
| `REDSTONE_PRICE_FEED` (optional `REDSTONE_ADAPTER`, `REDSTONE_DATA_FEED_ID`) | no | RedStone cold/live smoke |
| `MORPHO_MARKET_ID` or `MORPHO_ORACLE` | no | Morpho cold/live smoke |
| `EULER_ROUTER` or `EULER_ORACLE` | no | Euler cold/live smoke |

The same script runs in CI as the `live-smoke` job in
[`.github/workflows/ci.yml`](.github/workflows/ci.yml) — a
`workflow_dispatch`-only job fed by repository secrets of the same names (it
skips itself with a notice when the `ETH_RPC_URL` secret is unset). Trigger it
once against the release commit.

## 3. Package hygiene

- Confirm `Cargo.toml` metadata is release-ready: `version`, `license`
  (`MIT OR Apache-2.0`, with `LICENSE-APACHE` + `LICENSE-MIT` present),
  `description`, `repository`, `documentation`, `readme`, `keywords`,
  `categories`, `rust-version` (MSRV `1.90`).
- The `[package].exclude` list drops local runtime state (`.env*`), the test
  suite, the maintainer smoke script, this file, and CI config — src, runnable
  examples, the `docs/` guides, README, changelog, and licenses ship. **Keep
  `exclude` inside the `[package]` table** — writing it after a
  `[package.metadata.*]` header silently reparents it under that sub-table and
  Cargo ships everything.
- Inspect what would ship:
  ```bash
  cargo package --list --allow-dirty
  cargo publish --dry-run
  ```

## 4. docs.rs verification

docs.rs builds with `all-features` and `--cfg docsrs` (see
`[package.metadata.docs.rs]` in `Cargo.toml`). After publishing, verify the
feature-gated adapter and pending-oracle exports render and that the README
badges (crates.io version, docs.rs) resolve.

## 5. Version, changelog, tag & publish

- Bump `version` in `Cargo.toml`.
- In [`CHANGELOG.md`](CHANGELOG.md), promote the `[Unreleased]` section to
  `## [X.Y.Z] - <date>` and add a fresh empty `[Unreleased]`; update the link
  references at the bottom (`[X.Y.Z]` → `.../releases/tag/vX.Y.Z`,
  `[Unreleased]` → `.../compare/vX.Y.Z...HEAD`).
- Commit: `release: vX.Y.Z`.
- Tag and publish:
  ```bash
  cargo publish --locked
  git tag -s vX.Y.Z -m "Release evm-oracle-state vX.Y.Z"
  git push origin vX.Y.Z
  ```
- Verify the signed tag peels to the exact packaged release commit. Keep
  published tags immutable. If further fixes are needed, prepare a new version
  and repeat the release gates.
