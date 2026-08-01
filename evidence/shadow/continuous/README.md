# Continuous Mantle mainnet shadow (WHI-715)

Independent of the WHI-526 gate ledgers under `evidence/shadow/whi526-*`.

## Layout

```text
evidence/shadow/continuous/
  STATUS.md                 # written by the launcher
  v2/ledger.jsonl           # v2_monitor_executor_service (legacy example)
  v3-1559/ledger.jsonl      # v3_monitor_executor_service_1559 (legacy example)
  moe/ledger.jsonl          # moe_monitor_executor_service (legacy example)
  bot/ledger.jsonl          # merged multi-protocol [[bin]] bot (WHI-740)
  logs/<svc>.log
  run/pids/
  benchmark_report.json     # optional comparator output
  benchmark_report.md
```

## Start / stop

From the repo root (requires `MANTLE_RPC_URL` + `MANTLE_RPC_WS_URL` in `.env`).
The launcher defaults `MANTLE_MAINNET_SHADOW_THRESHOLDS_PATH` to
`config/gas_profiles/shadow_thresholds.mantle_mainnet.json` when unset:

```bash
./scripts/shadow/run_continuous_mainnet.sh
./scripts/shadow/status_continuous_mainnet.sh
./scripts/shadow/stop_continuous_mainnet.sh
./scripts/shadow/test_launcher_nosend.sh   # no_send env refusal smoke
```

Subset of services (useful on small VPS hosts):

```bash
SERVICES=v2,v3-1559 ./scripts/shadow/run_continuous_mainnet.sh
SERVICES=bot ./scripts/shadow/run_continuous_mainnet.sh
```

Resolve service keys without RPC (dry-run; useful for CI / acceptance):

```bash
RESOLVE_ONLY=1 SERVICES=bot ./scripts/shadow/run_continuous_mainnet.sh
# → bot|bin|bot|bot
#   cargo: cargo run --locked --bin bot -- --ledger <path>
```

Prebuilt binaries (skip `cargo run` on the host):

```bash
cargo build --locked --release \
  --example v2_monitor_executor_service \
  --example v3_monitor_executor_service_1559 \
  --example moe_monitor_executor_service \
  --bin bot
# Examples live under target/release/examples/; the bot bin lives one level up.
# The launcher resolves bin targets relative to CARGO_BIN_DIR when it ends in
# /examples.
CARGO_BIN_DIR=target/release/examples ./scripts/shadow/run_continuous_mainnet.sh
```

## Known-bot comparator

Operator supplies a real bot/tx list (Part B). Schema matches
`config/shadow/known_bots.example.json`.

```bash
cargo run --locked --example shadow_bot_benchmark -- compare \
  --ledger evidence/shadow/continuous/v2/ledger.jsonl \
  --ledger evidence/shadow/continuous/v3-1559/ledger.jsonl \
  --ledger evidence/shadow/continuous/moe/ledger.jsonl \
  --known-bots path/to/known_bots.json \
  --json-out evidence/shadow/continuous/benchmark_report.json \
  --md-out evidence/shadow/continuous/benchmark_report.md
```

Buckets:

1. `missed_detection` — no shadow candidate at that block/route
2. `unprofitable_or_revert` — candidate exists but preflight was not a profitable Pass
3. `would_have_been_profitable` — candidate Pass with net_profit > 0

## Safety

- `SHADOW_MODE=1` and the launcher refuse any `*_PRIVATE_KEY` env var.
- Ledgers must declare `send_capability=no_send` (comparator rejects otherwise).
- No secrets belong in this directory; keep RPC URLs in gitignored `.env` only.
