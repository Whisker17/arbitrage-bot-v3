# Mantle gas profiles (WHI-546 / M0-9)

Versioned, evidence-backed gas profiles for production arbitrage route classes.

## Layout

| Path | Role |
|------|------|
| `pinned/generator_config.json` | Chain id, WHI-501 codehash/ABI digest, active route classes, margin policy, fee analysis |
| `pinned/samples.jsonl` | Hash-pinned `fork_replay` qualification samples + research-only lines |
| `mantle_mainnet_v1.json` | Generated artifact (checked in; regenerate, do not hand-edit) |

## Regenerate

```bash
cargo run --example generate_gas_profile
# or
cargo run --example generate_gas_profile -- \
  --config config/gas_profiles/pinned/generator_config.json \
  --samples config/gas_profiles/pinned/samples.jsonl \
  --out config/gas_profiles/mantle_mainnet_v1.json
```

Two runs on identical pinned inputs must print the same `content_digest`.

## Rules

- **Qualification** requires `source=fork_replay` against the WHI-501 optimized runtime codehash.
- Current pinned samples come from **Foundry EVM replay** of that bytecode
  (`contracts/executor/test/GasProfileMeasure.t.sol`) on **mock pools**. Re-measure with:

  ```bash
  cd contracts/executor && forge test --match-contract GasProfileMeasureTest -vv
  ```

  Deep V3 tick / Moe bin buckets and multi-hop classes without a measurement remain
  **explicit Unsupported** (no silent generic limit). A Mantle **state-fork** suite is
  still required before promoting deep-crossing keys to Approved — tracked as
  `docs/DEFERRED_ISSUES.md` **DI-6**.
- **Historical / old-executor** samples are research-only and never approve production limits.
- **Reverts** are recorded separately and never mixed into success `gas_limit`s.
- **`gas_limit`** (execution) is separate from **`expected_gas_used`** (profitability).
- Limit policy: `ceil(max(max, p99) * (1 + margin_bps/10000)) + absolute_overhead` — not average × 1.2.
- Unknown route classes **fail closed** (no silent hop table fallback). Runtime consumption is WHI-502.
- Fee analysis records dated base-fee / block-limit observations; current stability is **not** a permanent constant.

## Runtime

M0-2 / WHI-502 loads and validates this artifact at startup and performs O(1) in-memory lookup.
This directory does not change the send path by itself.
