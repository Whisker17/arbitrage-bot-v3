# WHI-976 — `amm_quotes` counter dead on NoOptimum

**Issue:** [WHI-976](https://linear.app/whisker-personal/issue/WHI-976)

## Settlement

| Possibility | Holds? |
| --- | --- |
| 1. Counter never incremented on unprofitable cycles | **Yes** |
| 2. Cycles counted as evaluated but short-circuited before `simulate_path` | **No** |

**Evidence:** `optimize_path` mapped `Ok((None, quotes))` → `OptimizeOutcome::NoOptimum` while **discarding `quotes`**. The discover loop only added quotes on `OptimizeOutcome::Ok`. Production quiet markets hit `NoOptimum` for every cycle → `cycles_evaluated > 0` with `amm_quotes = 0` even though the binary search called `simulate_path` many times per cycle.

Unit proof: `cycles_optimized_implies_amm_quotes_even_when_no_optimum` forces absurd gas → all `NoOptimum`, empty candidates, and asserts `amm_quotes >= cycles_optimized`.

WHI-940 incremental evaluation is **not** reopened.

## Fix

- Carry `quotes` on `NoOptimum` and add them into `amm_quotes`.
- Runtime invariant on `paths_quoted` (Ok/NoOptimum only): `paths_quoted > 0 && amm_quotes == 0` → `debug_assert!` + `ERROR` log.
- Fee-reject / pool-lookup skips never quote by design and are excluded from that invariant.

## Prior measurement validity

| Measurement | Prior reading | After WHI-976 |
| --- | --- | --- |
| WHI-862 class C (0 candidates) | market/strategy null | **Still valid** — sims ran; counter was wrong |
| WHI-886 Agni-22 null + rule-of-three | market null on subset | **Still valid** — same |

Annotations: `evidence/shadow/candidate-window/STATUS.md`, `evidence/shadow/agni-window/STATUS.md`.

## Operator follow-up (not required to land the code fix)

Re-run a short signerless `--watch` (≥200 blocks preferred) on the fixed binary and confirm every `block_summary` with `cycles_evaluated > 0` has `amm_quotes > 0`. Paste counts into the PR or this STATUS. Candidate-rate remeasure for capital remains WHI-955.
