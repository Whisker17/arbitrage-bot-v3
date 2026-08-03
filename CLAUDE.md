# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

An on-chain arbitrage bot for the **Mantle** chain, built on top of a fork of `amms-rs`
(the crate is still named `amms`). It monitors DEX pools (UniswapV2, UniswapV3, Agni,
Moe Liquidity Book), finds arbitrage paths across them, and executes them through an
on-chain `ArbitrageExecutor` contract. Most prose docs under `tech-docs/` and `docs/`
are written in Chinese.

## Build, test, run

```bash
cargo build --locked              # debug build (dev profile is opt-level=3 + LTO; uses Cargo.lock)
cargo test --locked               # unit tests + tests/moe_swap.rs
cargo test --locked <name>        # single test by substring
cargo test --locked --test moe_swap  # single integration test file
cargo bench                       # criterion benches (benches/uniswap_v2.rs, uniswap_v3.rs)
cargo run --example <name>        # run an example entrypoint (see below)
cargo run --bin bot -- --offline  # multi-protocol bot (WHI-728); no RPC
cargo run --release --bin universe_gen  # offline unified pool universe (WHI-793)
```

The crate is primarily a library. Runnable surfaces:

- **`src/bin/bot.rs`** (`cargo run --bin bot`) — signerless multi-protocol bot
  (Agni-V2 + Agni-V3 + Moe concurrently). Default `--protocols agni-v2,agni-v3,moe`.
  Use `--offline` for the built-in cross-protocol fixture (no RPC). Live mode loads
  the unified frozen pool universe (`--pool-universe` / `BOT_POOL_UNIVERSE`) and
  runs one merged discovery pass.
- **`src/bin/universe_gen.rs`** (`cargo run --release --bin universe_gen`) —
  offline multi-protocol pool-universe generator (WHI-793). Writes
  `data/pool_universe.csv` + `.meta.json` (+ quarantine). The live bot never
  discovers pools.
- **`src/bin/rpc_probe.rs`** (`cargo run --bin rpc_probe`) — Mantle HTTP+WS RPC
  qualification probe (WHI-744). Emits a fingerprint-only JSON report; exits
  non-zero when the endpoint pair is not qualified.
- **`[[example]]`s** under `examples/` (`examples/test/`, `examples/protocols/agni/`,
  `examples/protocols/moe/`). The three `*_monitor_executor_service` examples remain
  production-disabled replay references (untouched by the merge). Start from
  `cargo run --example mock_arbitrage` (offline, replays `logs/pool_updates.csv`) to
  exercise the older single-protocol mock pipeline without RPC.

## Toolchain pins (important)

Exact versions live in `toolchain.toml` (and `rust-toolchain.toml` for rustup). CI runs
`scripts/check_toolchain.sh` and fails on drift. After clone:

```bash
git submodule update --init contracts/lib/forge-std
# Rust: rustup follows rust-toolchain.toml
# Foundry: foundryup --install v1.7.1   # must match toolchain.toml [foundry].version
# solc: Foundry/svm installs 0.8.26 from solc_version in contracts/foundry.toml
```

Use `cargo build --locked` / `cargo test --locked` so the committed `Cargo.lock` is
honored. Do not delete `Cargo.lock` or `contracts/foundry.lock`.

## The forge / ABI build step (important)

`build.rs` regenerates Solidity batch-request ABIs into `src/amms/abi/`. It is gated by
`SKIP_FORGE`, which **defaults to skipping forge** — normal `cargo build` uses the ABI
JSON already committed in `src/amms/abi/`. You only need forge when contracts change:

```bash
SKIP_FORGE=0 cargo build          # runs `forge build` in contracts/, refreshes ABIs
```

This requires `forge` and solc **0.8.26** (version pin in `build.rs` / Foundry config — no
host-specific absolute paths). The Solidity side is a Foundry project in `contracts/` with
a `forge-std` git submodule (`git submodule update --init` after clone).
`contracts/executor/ArbitrageExecutor.sol` is the on-chain executor; deploy/fund scripts
are in `scripts/`.

## Runtime configuration

Config is via environment variables / `.env` (see `env.mainnet.example`,
`env.sepolia.example`, `setup_env.sh`). Declare the expected chain with
`--chain-id` / `BOT_CHAIN_ID` (default `5000` mainnet); the bot fails closed if
either the connected HTTP or WS provider reports a different id (both are
probed at live startup — WHI-776). Endpoint selection is chain-aware — never
falls through a fixed list that can silently pick Sepolia when mainnet was
declared:

1. `RPC_HTTP_URL` / `RPC_WS_URL` (explicit override)
2. Chain-specific: `MANTLE_MAINNET_RPC_URL` (+ `_WS`) or `MANTLE_SEPOLIA_RPC_URL`
   (+ `_WS`)
3. Legacy mainnet aliases (mainnet only): `MANTLE_RPC_URL` / `MANTLE_RPC_WS_URL`
4. Legacy generic: `MANTLE_HTTP_URL` / `MANTLE_WS_URL`
5. Built-in default for that chain

Other keys: `MANTLE_SEPOLIA_PRIVATE_KEY` (no `0x`), `ARBITRAGE_EXECUTOR_ADDRESS`.
This is a **Mantle** deployment, so the base/gas token is WMNT, not WETH (the
code path is `WmntValueInPools`, replacing the upstream Weth variants).

## Frozen pool universe (live bot — WHI-784 / WHI-793)

The live binary **never** discovers pools from factories. One unified file under
`data/` is the source of truth: load once, fingerprint, fail closed when missing
or stale.

**Operator workflow** (stop → regenerate → start):

```bash
# One command regenerates the full multi-protocol universe
cargo run --release --bin universe_gen
# → data/pool_universe.csv + data/pool_universe.meta.json
# (+ data/pool_universe.quarantine.json for unvalued pools)

# Commit the CSV + meta, then start the bot
cargo run --bin bot -- --protocols agni-v2,agni-v3,moe --watch
```

Flags: `--pool-universe` / `BOT_POOL_UNIVERSE` (default `data/pool_universe.csv`).
The old per-protocol flags (`BOT_V2_POOL_LIST` / `BOT_V3_POOL_LIST` /
`BOT_MOE_POOL_LIST`) are **removed** and exit with a migration error.

Generator behaviour (WHI-793):

- Pins a block first (`--block head|<n>`); all valuation reads pin to that block.
- Seeds from legacy lists by default (fast); `--discover` re-enumerates from
  factories (slow). Supported labels: `agni-v2`, `agni-v3`, `moe`. FusionX V3
  rows are excluded; Mantle V2 currently operated under `agni-v2` uses the
  FusionX V2 factory as an **interim** venue (WHI-765 will reclassify).
- TVL floor defaults to **1000 WMNT** (WMNT-equivalent; no USD oracle) via
  `--min-tvl-wmnt-wei`. Unvalued pools go to the quarantine file, never silently
  kept or dropped.
- Keeps only pools on an ordered ≤3-hop WMNT settlement cycle
  (`EFFECTIVE_MAX_HOPS`), iterated to a fixed point after the TVL filter.
- Prints stage-by-stage funnel counts.

Live startup behaviour (fail closed — never falls back to factory discovery):

- **Missing / empty** `data/pool_universe.csv` → non-zero exit + regenerate hint.
- **Companion meta is required** (`data/pool_universe.meta.json`). Missing meta
  fails closed (no silent `snapshot_block=None`).
- Staleness: `snapshot_block` vs tip must be within
  `--universe-max-age-blocks` / `BOT_UNIVERSE_MAX_AGE_BLOCKS` (default 250_000).
- Legacy per-protocol CSVs (`data/poolLists.csv`, `data/poolLists_moe.csv`) remain
  as **seed inputs** for `universe_gen` only; the bot does not read them.
- Hot reload / promotion state machine remains out of scope (WHI-536 / M3-10).

## Architecture

Library modules (`src/lib.rs`) form a pipeline: blockchain events → state sync →
path finding → execution, plus multi-protocol service scaffolding. Each domain module
owns a typed `error.rs` (`thiserror`) where applicable.

- **`src/amms/`** — protocol abstraction. `AutomatedMarketMaker` trait + `AMM` enum
  unify pools across protocols; `AutomatedMarketMakerFactory` trait + `Factory` enum
  unify pool discovery. Per-protocol impls in `uniswap_v2/`, `uniswap_v3/`, `agni/`
  (V3-compatible), `moe/` (Liquidity Book, with its own `math/`). Pool state is read via
  batch-request contracts whose ABIs are in `abi/`. High-precision math uses `rug`/`U256`,
  not floats.

- **`src/state_space/`** — `StateSpaceManager` maintains a live `StateSpace` of pool
  state from a subscribed event stream, with a fixed-size `StateChangeCache` (ring buffer,
  `CACHE_SIZE = 30`) enabling reorg rollback. `PoolFilter` / `AMMFilter` prune pools
  (blacklist / whitelist). Built via `StateSpaceBuilder`.

- **`src/arbitrage/`** — opportunity discovery. `PoolGraph` (petgraph) models the
  token/pool graph; `PathFinder` finds closed settlement cycles; `PathOptimizer`
  binary-searches optimal input size (`OptimizationConfig`/`OptimizationResult`);
  `ArbitrageMonitor` (`MonitorConfig`) drives scanning. `mock.rs` provides an offline
  `MockArbitrageContext` for deterministic testing.

- **`src/execution/`** — `Executor` builds and submits transactions to the
  `ArbitrageExecutor` contract; `SwapExecutor` handles single swaps, `nonce.rs` manages
  nonces, `gas_profile.rs`/`gas_runtime.rs` compute gas. Includes pre-flight checks and
  non-negative-profit enforcement.

- **`src/service/`** — multi-protocol service scaffolding (WHI-727/728). `Protocol`
  trait + Agni-V2/Agni-V3/Moe impls, `ServiceConfig`, job-slot block-loop primitives,
  `PoolUniverseSource`, signerless startup (`production_send_allowed` hard-false), and
  merged-graph discovery used by `src/bin/bot.rs`.

- **`src/signing/`** — commit-signature verification for trusted tooling paths.

To add a protocol: implement `AutomatedMarketMaker`, add a variant to the `AMM` enum, and
implement its `Factory`. To add a filter: implement `AMMFilter`, add to `PoolFilter`.

## Git workflow (mandatory)

**One Linear issue = one git worktree off latest `origin/dev` = one PR into `dev`.**
Do **not** implement issues in the primary clone working tree.

1. `git fetch` + create worktree/branch from `origin/dev`
   (`fix/whi-NNN-topic` or `feat/whi-NNN-topic`).
2. Implement only that issue; Linear state → **`In Progress`**.
3. `gh pr create --base dev` (title/body include `WHI-NNN`); Linear → **`In Review`**.
   Any review finding you intentionally leave unfixed goes in `docs/DEFERRED_ISSUES.md`
   as part of this PR — see that file for the format.
4. After the PR is approved, run the **post-merge cleanup** below.

### Post-merge cleanup (mandatory, in order)

Drive these from the **primary clone**; never commit to `dev` directly.

0. **If the PR is CONFLICTING** (`dev` advanced since you branched): inside the feature
   worktree, `git merge origin/dev`, resolve, then `cargo check` + run the affected
   tests, and `git push`. The PR must read **MERGEABLE / CLEAN** before you merge.
1. **Squash-merge + drop the remote branch:** `gh pr merge <N> --squash --delete-branch`.
2. **Remove the worktree:** `git worktree remove <worktree-path>` then `git worktree prune`.
3. **Delete the local branch:** `git branch -D fix/whi-NNN-topic`
   (this fails while the worktree still holds the branch — do step 2 first).
4. **Fast-forward local `dev`:** `git fetch origin --prune` then
   `git merge --ff-only origin/dev` (must fast-forward — do not create commits on `dev`).
5. **Linear → `Done`.**

Never open a PR with `dev` as head into `main` (branch would be auto-deleted). Promote
via temporary `release/*` from `dev`. Full rules: `docs/GIT_WORKFLOW.md`.

## Agent skills

### Issue tracker

Issues and PRDs live in **Linear**, accessed via the Linear MCP tools (through the
`slim-tools` gateway). External PRs are not a triage surface. See
`docs/agents/issue-tracker.md`.

### Triage labels

Canonical role names (`needs-triage`, `needs-info`, `ready-for-agent`,
`ready-for-human`, `wontfix`) used verbatim as Linear labels. See
`docs/agents/triage-labels.md`.

### Domain docs

Single-context — one `CONTEXT.md` + `docs/adr/` at the repo root. See
`docs/agents/domain.md`.
