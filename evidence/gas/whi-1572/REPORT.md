# WHI-1572 discovery gas estimator — fit report

**Scope.** A discovery-only gas estimate for route classes without a measured
profile, used by the signerless shadow bot to *rank* candidates. It never becomes a
`GasQuote`, never changes the measured profile, and never makes a candidate
send-eligible. Net profit priced with it is **L2-gas-only modeled net** (Mantle
operator and L1 fees are not modeled; separate issue). Nothing below is a validation
or send qualification of the model.

## Reproduce

```bash
python3 scripts/gas_estimate/fit_discovery_estimator.py protocol   # already committed (0a812e4), before any fit
python3 scripts/gas_estimate/fit_discovery_estimator.py fit        # refuses to run if protocol.json no longer matches
```

The fit stage writes `config/gas_profiles/discovery_gas_estimator.mantle_mainnet.json`
(the artifact) and `evidence/gas/whi-1572/fit_report.json` (every number in this
file, plus all 900 row identities with their split). It is deterministic (exact
rational arithmetic, stdlib only): re-running it reproduces the artifact byte for
byte. The runtime pins the artifact's keccak256,
`0x3cc808afe0331f2f6c3d4a2f3f20013b10ff0dd10ba6fc88068d45a8c2d634a2`
(`MAINNET_ESTIMATOR_DIGEST`, `src/service/gas_estimate.rs`).

| identity | value |
|---|---|
| tool version | `whi-1572-fit/1.0.0`, Python 3.12.7 |
| script sha256 | `e7987c637b653b7e50c62b05a56116504a5034ddf2a1adb479cbe34231504226` |
| protocol.json sha256 | `169c41ec53895cb8990dee653fa283d49059601c7f87c3526207445776b1c866` |
| measured profile | `config/gas_profiles/mantle_mainnet_v1.json`, content digest `0x3d3244e3…df412` (unchanged) |
| executor code hash | `0x50f51b77…26ef` (all input rows; = `WHI501_EXECUTOR_CODEHASH`) |

## Inputs and hygiene (AC1)

| file | sha256 |
|---|---|
| `evidence/gas/whi-1422/attempts.jsonl` | `6976538d…044e` |
| `evidence/gas/whi-1520/attempts.jsonl` | `d28d4e34…0216` |
| `evidence/gas/whi-1520/run2/attempts.jsonl` | `cd9952af…73c1` |
| `evidence/gas/whi-1520/run3/attempts.jsonl` | `1c2a5339…02b1de` |
| `evidence/gas/whi-1413/attempts.jsonl` | `a76882c3…bdb4b` |

Filter: `lever == v2_boost`, `outcome == success`, `sample.source == fork_replay`,
`sample.outcome == success`, `chain_id == 5000`, the executor code hash above, and a
consistent row shape (topology = sample route key protocols, one crossing count and
one pool per hop, `gas_used` positive and equal to the sample's). Dedup key
`(block_hash, calldata_digest, lever_param)`, first occurrence wins.

- **900 clean rows** (fork `eth_estimateGas` of the real executor calldata, two blocks:
  98969898 and 101165208).
- Excluded: `lever != v2_boost` 878 (the contaminated `inflate` / `displace` levers,
  PR108-F3 / DI-51), `outcome != success` 1426, duplicate dedup key 3. No row failed the
  source, chain, executor or shape checks.

## Model and predeclared calibration (AC3)

`gas = b0 + b_v2·n_v2 + b_v3·n_v3 + b_moe·n_moe + s_tick·Σv3_ticks + s_bin·Σmoe_bins`,
per-hop counts from the same simulation that prices the amount (Agni initialized
ticks crossed; Moe non-empty bins consumed beyond the active bin — the campaign's
`per_hop_crossings`, produced by the same `simulate_swap_with_crossing_evidence`).

Split by ordered pool cycle (120 cycles), seed `whi-1572-v1`, 60/20/20 → 72 / 24 / 24
groups = 544 train / 176 calibration / 180 test rows. Membership and every convention
were committed in `protocol.json` before this fit ran.

| coefficient | exact train OLS (rounded) | deployed integer |
|---|---|---|
| b0 | −779.1 | −779 |
| b_v2 | 111 504 | 111 504 |
| b_v3 | 134 804 | 134 804 |
| b_moe | 178 326 | 178 326 |
| s_tick (per V3 tick) | 24 697 | 24 697 |
| s_bin (per Moe bin) | 15 527 | 15 527 |

Margins, calibrated on the **calibration groups only**, against the deployed integer
model, ceiling arithmetic:

- `expected_gas_used = ceil(pred × 11591 / 10000)` — calibration nearest-rank p90 of
  actual/predicted (1.1590). **Conservative, not unbiased.**
- `limit_envelope = ceil(pred × 15223 / 10000) + 50 000` — calibration max ratio
  (1.2178) × predeclared headroom 5/4, plus the predeclared absolute overhead.
- Runtime invariant `0 < expected < limit < block_gas_limit − reserve`, checked
  arithmetic; a violation is `invalid_estimate`, an overflow `gas_arithmetic`, only an
  available-gas excess `gas_reserve`. No bound is ever clipped.
- All three margins are **new, unvalidated parameters** (`margins.validated = false` in
  the artifact; the loader rejects `true`).

## Results

| split | rows / groups | abs rel error p50 / p90 / p95 / max | actual/pred p50 / p90 / max | actual > expected | actual > limit | expected padding over actual p10 / p50 / p90 |
|---|---|---|---|---|---|---|
| train | 544 / 72 | 5.8% / 14.2% / 20.9% / 42.5% | 1.012 / 1.089 / 1.303 | 6 | 0 | 6.5% / 14.5% / 32.4% |
| calibration | 176 / 24 | 6.6% / 13.7% / 17.9% / 28.4% | 1.034 / 1.159 / 1.218 | 13 | 0 | 0.0% / 11.8% / 23.3% |
| **untouched test** | 180 / 24 | 3.8% / 13.7% / 14.7% / 26.5% | 0.981 / 1.111 / 1.279 | **8 (4.4%)** | **0** | 4.4% / 18.0% / 29.5% |

Leave-block-out diagnostics (fit on one block, evaluate the other, same margins):

| held-out block | fit rows | eval rows / groups | abs rel error p50 / p95 / max | actual/pred max | actual > expected | actual > limit |
|---|---|---|---|---|---|---|
| 98969898 | 138 | 762 / 97 | 4.9% / 20.7% / 48.4% | 1.255 | 8 | 0 |
| 101165208 | 762 | 138 / 23 | 9.5% / 22.7% / 27.1% | 1.198 | 15 (10.9%) | 0 |

Held-out block 101165208 exceeds the p90 expected margin on 10.9% of rows — the margin
is not a guarantee off the calibration distribution.

## Coverage limits and extrapolation

- Training envelope (train rows): hop counts {2, 3}; per-hop V3 ticks ≤ 74, Σ ≤ 118;
  per-hop Moe bins ≤ 28, Σ ≤ 28; **every row contains a V2 hop** (0 V2-free rows);
  pool factories Agni V3 `0x2578…`, Moe V1 `0x5bef…`, Moe LB `0xa663…`, FusionX V2
  `0xe502…` (one train pool, `0xe1c4…260d`, is absent from every committed universe
  revision and is unattributed).
- Per candidate the runtime labels `v2_free_topology`, `hop_count_out_of_training`,
  `v3_ticks_out_of_training`, `moe_bins_out_of_training` and `unseen_venue_family`
  (a pool factory outside the training set, or unattributed). A label never rejects.
- So every V2-free topology (`v3+v3`, `moe+moe`, `v3+moe`, …), every open-ended bucket
  beyond the training crossings, and every non-Agni V3 venue is an **extrapolation**.
  Non-Agni V3 pools are possibly non-executable (the executor implements only
  `agniSwapCallback`, DI-50); a venue is `qualified` only with explicit evidence
  (the artifact ships none, so all venues are `unverified`).

## Audited withhold policy (AC2)

All **301** static `Unsupported` entries of the measured profile are mapped, by
predeclared regexes over their prose reasons (any unmatched or doubly matched reason
aborts the fit): `insufficient_samples` 152, `open_ended_bucket` 118,
`venue_withhold_factory_axis` (DI-50) 25, `venue_withhold_venue_axis` (DI-54) 2,
`scope_withhold` (WHI-1520) 2, `failed_limit_gate` 2. All are declared
estimation-eligible for **shadow ranking only**, venue withholds included (they carry
venue labels). The runtime refuses an artifact whose map does not cover exactly the
profile's `Unsupported` set, has an unknown category, or names an approved class.
Runtime invalidation, `ResearchOnly`, poisoned state, malformed keys and arithmetic
failures never fall back to an estimate.

## Economic false negatives

`expected_gas_used` is padded (untouched test: median +18% over actual, p90 +29.5%),
so a candidate whose true L2-gas-only net margin is below that padding × its gas cost
is rejected. The modeled net also omits the Mantle operator fee (observed 1e10
wei/gas) and the L1 fee, so it overstates net for every tier. No claim is made that no
opportunity is missed, and nothing here qualifies any route for sending.
