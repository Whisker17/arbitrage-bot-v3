# AGENTS.md

The single instruction entry for coding agents (Claude Code, Codex, pi, …) in this repo.
It holds the project facts and the load-bearing rules; details live in the linked docs —
read them when the task needs them. (`CLAUDE.md` only imports this file.)

## What this is

An on-chain arbitrage bot for the **Mantle** chain, built on top of a fork of `amms-rs`
(the crate is still named `amms`). It monitors DEX pools (UniswapV2, UniswapV3, Agni,
Moe Liquidity Book), finds arbitrage paths across them, and executes them through an
on-chain `ArbitrageExecutor` contract. Most prose docs under `tech-docs/` and `docs/`
are written in Chinese.

## Status

<!-- Keep current: what has landed, what is architected-for but NOT implemented yet.
Agents must not assume a module exists until its issue lands. -->

- Runs as a **signerless shadow** on Mantle mainnet (`src/bin/bot.rs --watch`, shadow
  ledger + daily Lark digest). The production send gate is closed:
  `production_send_allowed` defaults to false and is armed only via `--enable-sends` +
  WHI-860 preconditions.
- Release `0.2.2` is in progress (tag `v0.2.2-rc1` on `dev`). No release has been
  promoted to `main` yet, so the repo is in the Git workflow's **bootstrap state**
  (`docs/GIT_WORKFLOW.md` § Before the first production tag).
- Known wiring gaps (breaker/WAL service adoption, live identity source, deep gas
  buckets) are tracked in `docs/DEFERRED_ISSUES.md` — read the entry before assuming a
  seam is live.

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
- **`src/bin/ground_truth_collector.rs`** (`cargo run --release --bin ground_truth_collector`) —
  re-runnable Mantle arb-bot ground-truth collector (WHI-956). Dune CSV or
  arbs JSONL + block range → WHI-715 known-bots schema (+ WHI-957 fields).
  Real events stay external; commit aggregate reports only
  (`evidence/ground-truth/`).
- **`src/bin/peer_attribution.rs`** (`cargo run --release --bin peer_attribution`) —
  WHI-957 peer comparison: attribute every ground-truth arb to exactly one
  cause (not_in_universe / dirty_cycle_filter_skipped / evaluated_but_unprofitable
  / profitable_but_not_attempted / attempted_and_lost_race + separators).
  Optional concurrent ledger + `block_views` for dirty-cycle evidence.
- **`src/bin/missed_arb_universe.rs`** (`cargo run --release --bin missed_arb_universe`) —
  WHI-999 backwards universe selection: rank the pools we would have needed from
  the arbs we missed, by **marginal** in-scope arbs unlocked, with each candidate
  set's admission cost (pool count, production cycle count, estimated cold
  start). `--measure-tvl` adds a read-only valuation pass so the TVL floor can be
  attributed per pool (endpoint follows the chain-aware precedence below;
  `--rpc-url` overrides it and requires `--measure-tvl`). Reports in
  `evidence/missed-arbs/`.
- **`src/bin/rpc_probe.rs`** (`cargo run --bin rpc_probe`) — Mantle HTTP+WS RPC
  qualification probe (WHI-744). Emits a fingerprint-only JSON report; exits
  non-zero when the endpoint pair is not qualified.
- **`src/bin/lark_daily_digest.rs`** (`cargo run --bin lark_daily_digest`) —
  WHI-1407 one-shot daily Lark digest card for the signerless shadow-mode dry
  run. Reads the shadow ledger only (no new row type, no schema change);
  never embedded in `bot.rs --watch`. `--dry-run` renders with no network/state
  mutation; `--send-test` verifies the real webhook without consuming the
  daily marker; `--date <YYYY-MM-DD>` is the explicit recovery command.
  Scheduled via `scripts/systemd/lark-daily-digest.{service,timer}` (fixed
  00:10 UTC).
- **`[[example]]`s** under `examples/` (`examples/test/`, `examples/protocols/agni/`,
  `examples/protocols/moe/`). The three `*_monitor_executor_service` examples remain
  production-disabled replay references (untouched by the merge). Start from
  `cargo run --example mock_arbitrage` (offline, replays `logs/pool_updates.csv`) to
  exercise the older single-protocol mock pipeline without RPC.

### Checks (tiers per `docs/GIT_WORKFLOW.md` § 2 Implement)

- **Required project checks** (every merge; mirrors `.github/workflows/ci.yml`):
  `scripts/check_toolchain.sh`, `cargo build --locked`, `cargo test --locked --lib --tests`,
  no drift in `Cargo.lock` / `contracts/foundry.lock`, and — when `contracts/` changed —
  `forge build --skip test` in `contracts/` and `contracts/executor/` plus
  `SKIP_FORGE=0 cargo build --locked` leaving `src/amms/abi/` unchanged.
- **Relevant issue checks:** the affected `cargo test --locked <name>` / `--test <file>`,
  the Foundry tests for a touched contract, and any targeted offline run
  (`cargo run --bin bot -- --offline`).
- **Full suite:** `cargo test --locked --all-targets` (builds examples; slow under LTO),
  plus `forge test` in `contracts/executor/` when contracts changed.
- **Process tooling:** `uv run --no-project --with pytest python -m pytest tests/test_agent_dispatch.py`
  when `scripts/agent-dispatch.sh` or `config/agent-roles.conf` changes.

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

## High-risk paths

"The funds path" — referenced by `docs/GIT_WORKFLOW.md`, the PR template and the
triage rules — means any change touching:

- `src/execution/` (the `Executor` / `SwapExecutor` / nonce / gas-profile path that
  submits on-chain transactions), or the send gate that arms it;
- `contracts/executor/ArbitrageExecutor.sol`;
- private-key or signer handling (`*_PRIVATE_KEY` env vars, signer construction);
- a mainnet gas profile (`config/gas_profiles/*mainnet*.json`), or any other behaviour
  that would run against real funds once sends are enabled (as opposed to Sepolia or
  shadow-only paths).

An agent never merges or deploys such a change: it stops at `In Review` for a human, even
with green checks. The issue defaults to `ready-for-human`.

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

- **`src/notify/`** — WHI-1407 daily Lark digest support library (used only by `src/bin/lark_daily_digest.rs`, never by `bot.rs`). `utc_date.rs` (dependency-free UTC calendar days), `ledger_window.rs` (chronological shadow-ledger reader), `digest.rs` (pure aggregation), `lark.rs` (card render + `reqwest::blocking` delivery), `state.rs` (day-keyed idempotency lock).

To add a protocol: implement `AutomatedMarketMaker`, add a variant to the `AMM` enum, and
implement its `Factory`. To add a filter: implement `AMMFilter`, add to `PoolFilter`.

## Git workflow (mandatory)

Full rules: **`docs/GIT_WORKFLOW.md`** (authoritative if this summary ever disagrees).

**One issue = one git worktree off the resolved base = one PR into that base.** Never
implement in the primary clone. **Never default to `dev`** — resolve the base; if the
issue fits not exactly one row, **stop and surface it**.

| Category | Recognised by | Worktree base | PR base |
| --- | --- | --- | --- |
| Hotfix | `hotfix` label | `origin/main` | `main` |
| Repo-wide governance | touches **only** the carve-out list (`docs/GIT_WORKFLOW.md` § Repo-wide governance carve-out) | `origin/dev` | `dev` |
| Version-scoped work | everything else | `origin/release/v{version}` | `release/v{version}` |

- **Bootstrap (current state):** no production tag has reached `main`, so the
  version-scoped row resolves to `dev` — a resolved value, not a default. It still needs
  its version signals. This ends with the first `release/v*` → `main` merge.
- **Version:** the title prefix `[X.Y.Z]`, cross-checked against the tracker Release. If
  they disagree, or one is missing while the other exists, refuse. Never infer it from a
  milestone or a legacy `[Mn]` / `[Go-Live]` tag (`docs/agents/issue-template.md`
  § Legacy titles).
- A missing `origin/release/v{version}` (once out of bootstrap) is a refusal. Cutting an
  integration branch is the owner's deliberate act, never a side effect of picking up a
  ticket.
- A mixed PR (carve-out + other files) is **split**; governance issues carry no version and
  no Release.
- Right after creating the worktree: `git merge-base HEAD origin/<base>` must equal
  `git rev-parse origin/<base>`; then `git config core.hooksPath .githooks` (per worktree).
- Checks come in three tiers (§ Checks above): required project checks always, relevant
  issue checks for every PR, and the full suite at release candidates and for PRs no
  release acceptance covers.
- The PR body states the resolved base and the signals it came from, the evidence
  (commit SHA, commands, results) and the role/model/effort used.
- Tracker moves with the PR: `In Progress` → `In Review` (PR open) → `Done` (merged and
  cleaned up). A report of "done" is not `Done`. Review findings consciously left unfixed
  go in `docs/DEFERRED_ISSUES.md` in the same PR.

### Merge authorization

| Work | Before merge | Agent may merge? |
| --- | --- | --- |
| Ordinary version issue → existing integration branch, under `/orchestrate` | Issue acceptance, required project checks and relevant issue checks on the final HEAD; orchestrator verified the evidence | Yes — review happens at release level |
| Bootstrap issue → `dev` (no production tag yet), under `/orchestrate` | Same | Yes — first release still gets a full release review |
| Governance → `dev`; standalone `/implement` | Required project checks, relevant checks and the full suite, plus one independent PR review passed on the final commit | Yes, then fan out |
| Touches **the funds path** (§ High-risk paths) | Its lane's checks + documented verification | **No** — human |
| `hotfix/*` → `main` | Required and relevant checks, full suite, independent review | **No** — human |
| Finished `release/v*` → `dev` | Full suite (complete acceptance) + passed release review on the current SHA | **No** — human |
| `release/*` → `main` | Release flow | **No** — human |

No waiver of the human rows is in force; a waiver takes the shape in
`docs/GIT_WORKFLOW.md` § Waiving an exception. A tracker label alone waives nothing.

### After a merge

Follow `docs/GIT_WORKFLOW.md` § Post-merge cleanup from the primary clone: remove the
worktree and local branch, fast-forward the base, and **whenever `dev` advanced, fan out
`dev` into every live `release/v*` in the same session** — a governance rule is in force
only on branches that carry it. Merge strategy is per lane: squash into `dev` and
`release/v*`; merge commit into `main` and for a finished integration branch → `dev`.
`main` equals production; deploy **only from a tag**. Promote via a temporary
`release/vX.Y.Z` cut from `dev`, never a `dev` → `main` PR. Production broken while `dev`
holds unshippable work → hotfix lane (`docs/GIT_WORKFLOW.md` § Choosing a promotion lane).
Never commit feature work directly to `main`, `dev` or a `release/v*`; the only direct
pushes are the three documented merges (fan-out, hotfix backmerge, first push of a new
cut), with `ALLOW_DIRECT_PUSH=1`.

## Agent runtime

Runtime-neutral: skills name a **role** — `ORCHESTRATOR`, `IMPLEMENTER`, `REVIEWER` — and
an effort (`medium` | `high`); `config/agent-roles.conf` maps each to a runtime, an exact
model ID and effort values, and `scripts/agent-dispatch.sh` dispatches them. Contract:
**`docs/agents/runtime.md`**. Current mapping: all roles on `pi`; orchestrator and
implementer `claude/claude-opus-5-5`, reviewer `mantle/gpt-6-astra`.

- Independent review runs in a **different context** from the implementation, preferably
  another vendor. Self-review in the implementing context never satisfies a review rule.
- `--probe` checks configuration only; a real one-line call proves a role works.
- A failed or missing role fails closed — no model substitution, no lower effort.
- When a model generation turns over, edit `config/agent-roles.conf` and nothing else.

## Agent skills

Skills live in `.claude/skills/<name>/SKILL.md` (`.agents/skills` is a symlink to the
same directory). Runtimes that auto-discover them expose `/<name>`; **with no skill
loader, read the file directly** — a skill is just markdown.

| Skill | Use |
| --- | --- |
| `/grill-me` | Clarify goal, constraints and acceptance by interview |
| `/to-spec` | Turn the discussion into a spec (`docs/DESIGN.md` by default) |
| `/to-tickets` | Publish issues from `docs/agents/issue-template.md`, with complexity, scope and native dependencies |
| `/implement` | One issue: worktree, ponytail, relevant checks, PR, handoff |
| `/orchestrate` | A release: schedule, integrate, release review, bounded fixes |
| `/code-review` | Independent review of a PR range or a release snapshot |
| `/handoff` | Short handoff that points at durable evidence |
| `/ponytail` | Write the least code that meets the spec |

### Issue tracker

Issues and specs live in **Linear** (project `Mantle Arbitrage bots v2`, team
`Whisker-Personal`, key `WHI`; Releases in the `arbitrage-bot-v3` release pipeline),
reached via MCP, else the GraphQL API with `LINEAR_API_KEY`. No tracker reachable = stop
and report; never a shadow tracker. Issue shape: `docs/agents/issue-template.md` (English).
Access, release binding and state ownership: `docs/agents/issue-tracker.md`. Triage labels:
`docs/agents/triage-labels.md`.

### Domain docs

Spec of record: `docs/DESIGN.md` — currently an index into `specs/` and `tech-docs/`
(`docs/agents/domain.md`) — plus `docs/adr/` for narrower later decisions. Known,
accepted debt and intentional designs: `docs/DEFERRED_ISSUES.md`. Process traps worth
knowing, read on demand: `docs/TRAPS.md`.

## Template feedback loop

This repo adopted the process layer of the shared project template
(`https://github.com/Whisker17/code-template`, v0.2.0, WHI-1497). When work here surfaces
a **template-layer** improvement — a workflow rule that bit us, a skill or config fix, a
doc convention worth standardizing — tell the user so they can port it back (and record
it in the template's `CHANGELOG.md`). Project-specific learnings stay.
