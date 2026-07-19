# AGENTS.md

This file provides guidance to Codex (Codex.ai/code) when working with code in this repository.

## What this is

An on-chain arbitrage bot for the **Mantle** chain, built on top of a fork of `amms-rs`
(the crate is still named `amms`). It monitors DEX pools (UniswapV2, UniswapV3, Agni,
Moe Liquidity Book), finds arbitrage paths across them, and executes them through an
on-chain `ArbitrageExecutor` contract. Most prose docs under `tech-docs/` and `docs/`
are written in Chinese.

## Build, test, run

```bash
cargo build                       # debug build (note: dev profile is opt-level=3 + LTO, so builds are slow)
cargo test                        # unit tests + tests/moe_swap.rs
cargo test <name>                 # single test by substring
cargo test --test moe_swap        # single integration test file
cargo bench                       # criterion benches (benches/uniswap_v2.rs, uniswap_v3.rs)
cargo run --example <name>        # run an entrypoint (see below)
```

There is **no binary target** — the crate is a library. All runnable programs are
`[[example]]`s registered in `Cargo.toml`. Entrypoints live under `examples/`
(`examples/test/`, `examples/protocols/agni/`, `examples/protocols/moe/`). Start from
`cargo run --example mock_arbitrage` (offline, replays `logs/pool_updates.csv`) to
exercise the pipeline without RPC.

## The forge / ABI build step (important)

`build.rs` regenerates Solidity batch-request ABIs into `src/amms/abi/`. It is gated by
`SKIP_FORGE`, which **defaults to skipping forge** — normal `cargo build` uses the ABI
JSON already committed in `src/amms/abi/`. You only need forge when contracts change:

```bash
SKIP_FORGE=0 cargo build          # runs `forge build` in contracts/, refreshes ABIs
```

This requires `forge` and `solc` (hardcoded to `/opt/homebrew/bin/solc` in `build.rs`).
The Solidity side is a Foundry project in `contracts/` with a `forge-std` git submodule
(`git submodule update --init` after clone). `contracts/executor/ArbitrageExecutor.sol`
is the on-chain executor; deploy/fund scripts are in `scripts/`.

## Runtime configuration

Config is via environment variables / `.env` (see `env.sepolia.example`, `setup_env.sh`).
Keys are chain-prefixed, e.g. `MANTLE_SEPOLIA_RPC_URL`, `MANTLE_SEPOLIA_RPC_WS_URL`,
`MANTLE_SEPOLIA_PRIVATE_KEY` (no `0x`), `ARBITRAGE_EXECUTOR_ADDRESS`. This is a
**Mantle** deployment, so the base/gas token is WMNT, not WETH (the code path is
`WmntValueInPools`, replacing the upstream Weth variants).

## Architecture

Four library modules (`src/lib.rs`) form a pipeline: blockchain events → state sync →
path finding → execution. Each module owns a typed `error.rs` (`thiserror`).

- **`src/amms/`** — protocol abstraction. `AutomatedMarketMaker` trait + `AMM` enum
  unify pools across protocols; `AutomatedMarketMakerFactory` trait + `Factory` enum
  unify pool discovery. Per-protocol impls in `uniswap_v2/`, `uniswap_v3/`, `agni/`
  (V3-compatible), `moe/` (Liquidity Book, with its own `math/`). Pool state is read via
  batch-request contracts whose ABIs are in `abi/`. High-precision math uses `rug`/`U256`,
  not floats.

- **`src/state_space/`** — `StateSpaceManager` maintains a live `StateSpace` of pool
  state from a subscribed event stream, with a fixed-size `StateChangeCache` (ring buffer,
  `CACHE_SIZE = 30`) enabling reorg rollback. `PoolFilter` / `AMMFilter` prune pools
  (blacklist / whitelist / value). Built via `StateSpaceBuilder`.

- **`src/arbitrage/`** — opportunity discovery. `PoolGraph` (petgraph) models the
  token/pool graph; `PathFinder` finds cycles and two-pool misprices; `PathOptimizer`
  binary-searches optimal input size (`OptimizationConfig`/`OptimizationResult`);
  `ArbitrageMonitor` (`MonitorConfig`) drives scanning. `mock.rs` provides an offline
  `MockArbitrageContext` for deterministic testing.

- **`src/execution/`** — `Executor` builds and submits transactions to the
  `ArbitrageExecutor` contract; `SwapExecutor` handles single swaps, `nonce.rs` manages
  nonces, `gas.rs`/`gas_schedule.rs` compute gas. Includes pre-flight checks and
  non-negative-profit enforcement.

To add a protocol: implement `AutomatedMarketMaker`, add a variant to the `AMM` enum, and
implement its `Factory`. To add a filter: implement `AMMFilter`, add to `PoolFilter`.

## Git workflow

`main` (stable, PR-only) + `dev` (active integration, default PR target) + short-lived
`feat/*` `fix/*` `chore/*` `hotfix/*` branches. Branch from latest `dev`, not `main`. PRs
are **squash-merged** and the remote branch is auto-deleted. To promote to `main`, cut a
temporary `release/*` branch from `dev` (never open a PR with `dev` as head — it would be
deleted on merge). Full details: `docs/GIT_WORKFLOW.md`.

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
