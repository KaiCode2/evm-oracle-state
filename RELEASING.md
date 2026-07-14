# Releasing `evm-oracle-state`

Maintainer checklist for cutting and publishing a release. During unreleased
cross-crate development, the `evm-fork-cache 0.3.0` dependency uses the sibling
path while retaining its version requirement. Before packaging, publish
`evm-fork-cache 0.3.0`, remove the path override, refresh the lockfile, and then
work the sections below in order.

## 1. Pre-publish verification

Run the full matrix locally (or confirm green CI on the release commit). These
mirror `.github/workflows/ci.yml`, which pins Rust **1.88.0** — the pinned
toolchain doubles as the MSRV gate for `rust-version = "1.88"`:

```bash
cargo fmt --check
cargo test --locked
cargo test --locked --features aave,morpho,euler,pyth,redstone
# --all-features additionally enables `live-tests`; the RPC-gated live tests
# skip cleanly when no RPC endpoint is configured, so this must pass offline:
cargo test --locked --all-features
cargo clippy --locked --all-targets --features aave,morpho,euler,pyth,redstone -- -D warnings
# per-feature isolation (catches cfg-gating bugs the combined build masks —
# code that only compiles because a sibling feature happens to be on):
for f in aave morpho euler pyth redstone; do
  cargo check --locked --no-default-features --features "$f"
done
cargo check --locked --examples --features aave,morpho,euler,pyth,redstone
RUSTDOCFLAGS="-D warnings" cargo doc --locked --no-deps --features aave,morpho,euler,pyth,redstone
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
  `categories`, `rust-version` (MSRV `1.88`).
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
`[package.metadata.docs.rs]` in `Cargo.toml`); the `doc_cfg` attribute in
`src/lib.rs` stamps "Available on crate feature … only" badges on the
`aave` / `morpho` / `euler` / `pyth` / `redstone`-gated items. After
publishing, verify the badges render on the gated adapters and that the README
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
- **If commits land on `main` after the tag exists**, re-point the tag at the
  final commit *before* `cargo publish` — a stale tag silently publishes old
  code's provenance (`.cargo_vcs_info.json`, the GitHub release link, and the
  changelog anchor all disagree with the crate contents):
  ```bash
  git push origin :refs/tags/vX.Y.Z
  git tag -fs vX.Y.Z -m "Release evm-oracle-state vX.Y.Z" <final-commit>
  git push origin vX.Y.Z
  ```
