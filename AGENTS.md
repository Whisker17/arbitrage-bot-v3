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

## Toolchain pins (important)

Exact versions live in `toolchain.toml` (and `rust-toolchain.toml` for rustup). CI runs
`scripts/check_toolchain.sh` and fails on drift. After clone:

```bash
git submodule update --init contracts/lib/forge-std
# Rust: rustup follows rust-toolchain.toml
# Foundry: foundryup --install 1.7.1   # must match toolchain.toml [foundry].version
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
