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
  `data/pool_universe.csv` + `.meta.json` (+ quarantine). Optional
  `--arb-arbs` / `--arb-census` attach observed-arb coverage (WHI-906) next to
  the fingerprint. The live bot never discovers pools.
- **`src/bin/arb_coverage.rs`** (`cargo run --release --bin arb_coverage`) —
  offline observed-arb coverage vs a frozen universe (WHI-906). External arbs
  JSONL + census in; report + optional meta `observed_arb_coverage` out.
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

### RPC throttle vs pool-universe size (WHI-786 / WHI-862 / WHI-921 / WHI-925)

Production HTTP uses `ThrottleLayer` + retry-backoff + per-request timeout
(`src/service/rpc_provider.rs`). Knobs:

| Env | Default | Notes |
| --- | ---: | --- |
| `RPC_HTTP_THROTTLE_RPS` | **8** | requests/sec |
| `RPC_HTTP_RETRY_MAX` | 5 | |
| `RPC_HTTP_RETRY_INITIAL_BACKOFF_MS` | 200 | |
| `RPC_HTTP_REQUEST_TIMEOUT_MS` | 30000 | |

**Why 8, not 250.** WHI-862 measured **8 RPS** as sustainable on Mantle public
RPC for a **59-pool** universe (`evidence/shadow/candidate-window/run_plan.json`).
The old default of 250 relied on retries to absorb overflow; at **137 pools**
that overflow became a **429 storm** (WHI-921). The throttle change stands on
its own merits (429s 95 → 0) and stays at the measured 8.

**Two different `CreateContractSizeLimit` causes.** Do not conflate them:

* **Rate pressure (Moe, WHI-921):** concurrent Moe bin/slot0 CREATE eth_calls
  under 429 load. Recovery uses backoff / paced half; does **not** fan out to
  N singles while under pressure.
* **Payload size (V3 slot0, WHI-925):** hard-coded `step = 255` put 94
  `agni-v3` pools in one batch CREATE whose return exceeds EIP-170 (24 KB).
  Observed with **http_429 count = 0** — pure batch width. Recovery is
  size-derived chunking + **halve without time backoff**
  (`src/amms/batch_create.rs`). Backing off in time does not shrink a payload.

**Scaling.** `recommended_throttle_rps(pool_count)` scales inversely from the
WHI-862 reference (8 @ 59), floored at 4 and capped at 16. At 137 pools the
recommendation is **4**. Live startup logs `throttle_rps`,
`recommended_throttle_rps`, and `pool_count` together so a mismatch is visible
before state sync. Override with `RPC_HTTP_THROTTLE_RPS` when the endpoint
budget differs; do not silently restore 250 on a large universe.

**Moe CREATE recovery (WHI-921).** On `CreateContractSizeLimit`, Moe sync backs
off and retries (single-item floor) or halves the chunk with pace — it does
**not** fan out to N singles under recent 429 pressure.

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
  factories (slow). Supported labels: `agni-v2`, `agni-v3`, `moe`.
  **WHI-910:** `agni-v3` is the UniV3-family math label; each pool keeps its
  own `factory`. Six **loadable** drop-in V3 factories are enumerated (Agni,
  FusionX V3, Butter, Fluxion V3, V3fork-636ea2, Uniswap V3 Mantle). Cleopatra CL
  is **quarantined** (WHI-938: Agni tick-data batch CREATE reverts) — seed rows
  go to the quarantine file, not the emitted universe. Legacy seed maps
  `Protocol=Agni|FusionX` → those factories; other factories report **loud zeros**
  until seeded/discovered. Per-factory funnel counts are printed.
  CREATE2 deployers stay per-venue (never merged into Agni). Mantle V2 currently
  operated under `agni-v2` uses the FusionX V2 factory as an **interim** venue
  (per-venue V2 fees still out of scope — do not seed MantleSwap V2 / extra V2
  venues under hard-coded `V2_FEE = 300`).
- TVL floor defaults to **1000 WMNT** (WMNT-equivalent; no USD oracle) via
  `--min-tvl-wmnt-wei`. Unvalued pools go to the quarantine file, never silently
  kept or dropped.
- Keeps only pools on an ordered ≤3-hop WMNT settlement cycle
  (`EFFECTIVE_MAX_HOPS`), iterated to a fixed point after the TVL filter.
- Prints stage-by-stage funnel counts (including per-V3-factory).

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
  `PoolUniverseSource`, send-path gate (`production_send_allowed` defaults false;
  armed only via `--enable-sends` + WHI-860 preconditions), and merged-graph
  discovery used by `src/bin/bot.rs`.

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
