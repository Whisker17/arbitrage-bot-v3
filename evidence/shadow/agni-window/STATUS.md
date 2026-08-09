# WHI-886 — 4-hour Agni-only observation window (statistical power)

**Issue:** [WHI-886](https://linear.app/whisker-personal/issue/WHI-886)  
**Purpose:** Produce the project's first candidate-rate measurement with real statistical power on the **Agni-only subset** (22 pools), signerless. Feeds re-evaluation of WHI-535 class-C claims for this subset.  
**Mode:** Signerless start-to-finish (`production_send_allowed() == false`, ledger `send_capability=no_send`, `--enable-sends` not passed).

> **Scope bound (mandatory):** this window covers **22 of 59** pools (`agni-v2` + `agni-v3` only). It says **nothing** about Moe or Moe-inclusive cross-protocol cycles. Moe coverage waits on WHI-885.

## Universe under test

| Field | Value |
| --- | --- |
| Path | `data/pool_universe.csv` + `.meta.json` (committed on `dev`) |
| Full-universe fingerprint | `0x19ce9ed1d60f12d4c577576912a79c859c4faac983d67b46d26355deff389c85` |
| Full-universe pool count | **59** (`agni-v2`=4, `agni-v3`=18, `moe`=37) |
| Agni-filtered load | `pool_count=22` |
| Agni-filtered fingerprint (logged) | `0x40244b934f8120249b84c373c497bdf7cf17a3202f48948a3ab43079c816b422` |
| Snapshot block | **98,795,302** |
| Filter | TVL floor **1000 WMNT**, ≤3-hop WMNT settlement cycles |

## Liveness gate (before treating the window as a market sample)

1. Pre-flight `./target/release/bot --protocols agni-v2,agni-v3 --once` exited 0; `synced pool state pools=22`.
2. Watch start: `enable_sends=false`, `head_source="ws"`, shadow ledger `send_capability=no_send`.
3. `StateSpaceBuilder::sync` succeeded (`synced pool state pools=22`) at window start.
4. Watch loop entered (`entering multi-protocol --watch`).
5. First `blocks_processed ≥ 1` within ~80 s of start (first progress block ≈ 98,932,949).
6. Process stayed alive for the full ≥4 h target; clean SIGTERM exit (no mid-window restart).

## Window parameters

| Field | Value |
| --- | --- |
| Started (UTC) | **2026-08-06T06:02:12Z** |
| Stopped (UTC) | **2026-08-06T10:04:05Z** (SIGTERM at T+14400s) |
| Wall duration | **≥ 4 h** (target 14400 s; clean exit ~2 s after signal) |
| Protocols | `agni-v2,agni-v3` |
| Head source | `ws` (default) |
| RPC throttle | default **250** rps (not the WHI-862 Moe throttle of 8) |
| Ledger | `ledger.jsonl` |
| Thresholds | `evidence/shadow/candidate-window/thresholds.json` (pin-only scaffold) |
| `production_send_allowed` | **false** |
| Broadcast | **0** |

## Results

Source: `analysis.json` + clean exit log line.

| Metric | Value |
| --- | --- |
| Ledger runtime span | ~14407 s (`runtime_seconds` from ledger timestamps) |
| Observations / unique blocks | **6326 / 6326** |
| Block range (min→max observed) | **98932948 → 98940165** (span **7217** chain blocks) |
| `heads_observed` (log) | **7192** |
| `blocks_processed` (log) | **6325** |
| Coverage `blocks_processed / heads_observed` | **87.94%** |
| Discovery passes with `opportunities>0` | **0** |
| Candidates (ledger) | **0** |
| Gross-profitable count | **0** |
| Net-profitable-after-gas count | **0** |
| Topology mix | *(empty — no candidate/context rows)* |
| Net-profit distribution | *(n/a — no samples)* |
| `send_capability` | `no_send` |
| `broadcast_count` | **0** |

### Loop health (exit counters)

| Counter | Value |
| --- | --- |
| `halted_or_skipped` | 867 (all `pin_skips`) |
| `http_tip_timeouts` | 47 |
| `cold_start_rebaselines` | 0 |
| `mid_run_rebaselines` | 0 |
| `skip_ratio_warnings` | 0 |
| Startup `http_429` retries | 123 (clustered at initial sync; did not climb mid-window) |
| Notable errors | 1× WS connection reset mid-run (recovered); process never died |

### Coverage finding vs 97.3% probe baseline

A 300 s Agni-only probe before this issue reported **97.3%** coverage. This 4 h window finished at **87.9%**. That is a **material gap** relative to the probe, not a silent dead loop:

- Loop remained live (`blocks_processed` climbed continuously; checkpoints every ~30 min).
- Skips are explained by exit counters: `pin_skips=867`, `http_tip_timeouts=47`.
- Do **not** treat 87.9% as equivalent to the short probe; report it as a secondary finding for endpoint/pin behaviour under multi-hour load.

### Three-outcome classification (same shape as WHI-862)

| Class | Holds? |
| --- | --- |
| A — opportunities exist and clear gas | **No** |
| B — opportunities exist but gas eats them | **No** (no gross-positive paths either) |
| **C — no opportunities observed in this pipeline/universe** | **Yes (for Agni-22 only)** |

**Pipeline-alive zero:** ledger has continuous observations and `blocks_processed=6325`. This is not a silent dead loop.

## Statistical-power statement (required deliverable)

With **0** candidates over **N = 6326** unique observation blocks, the **rule-of-three ~95% upper bound** on the per-block candidate rate is:

\[
\hat{p}_{95} \approx \frac{3}{N} = \frac{3}{6326} \approx 4.74 \times 10^{-4}\ \text{per block}.
\]

At Mantle's ~**43,200 blocks/day** (~2 s blocks), that bounds the opportunity rate at roughly:

\[
4.74 \times 10^{-4} \times 43200 \approx \mathbf{20.5\ opportunities/day}
\]

on the **Agni-22 subset / this optimizer / this synced state**.

| Comparison | N blocks | ~95% UB opportunities/day |
| --- | ---: | ---: |
| WHI-862 (main + low-TVL, sparse) | ~55 | ~**2,350**/day |
| **This window (Agni-22)** | **6,326** | ~**20.5**/day |
| Tightening vs WHI-862 | — | **~115×** |

A bare "0 candidates" without this bound would repeat WHI-862's error and is **not** the deliverable.

### What this does *not* prove

- Does **not** prove the full 59-pool market is class C (Moe excluded).
- Does **not** prove competitors found nothing in the same span (no on-chain competitor cross-check in this issue; recommended follow-up).
- Does **not** re-decide WHI-535 — it is an **input** for the Agni subset only.
- Class C here means: no sized opportunity reached preflight/ledger under current radius, hop cap, and Agni-only universe — not a mathematical proof that no profitable path exists under alternate sizing/state.

## Artifacts (no credentials)

```text
evidence/shadow/agni-window/
  STATUS.md                 # this file
  run_plan.json
  analysis.json             # ledger analysis + statistical power + exit stats
  ledger.jsonl              # full shadow ledger (send_capability=no_send)
  checkpoints.log           # 30-min liveness checkpoints
  start_utc.txt
  DONE                      # monitor completion marker
  logs/window.scrubbed.log  # head + sampled key lines + tail; URLs redacted
  # logs/window.log         # NOT committed (~1.8 GiB local full log; underflow WARN noise)
```

## Safety

- No private keys in process env for sends; `--enable-sends` not passed.
- Ledger `send_capability=no_send` for the entire run.
- `production_send_allowed()` hard-false; `broadcast_count == 0`.
- RPC URLs / API keys / private keys never written into committed artifacts.
- Full raw log kept local only; committed log is scrubbed and truncated.

## Implication for capital / WHI-535

For the **Agni-22** subset, a multi-hour high-coverage sample still found **zero** candidates, now with a ~**21/day** 95% upper bound rather than thousands. That **tightens** the prior WHI-862 null result for this subset, but:

1. Still does not authorize funding (no class-A evidence).
2. Still incomplete without Moe (WHI-885) and without competitor ground-truth cross-check on the same block range.
3. WHI-535 re-decision should wait for those inputs rather than treating this alone as full-market class C.

### Post-hoc validity (WHI-976 + WHI-980)

Two later defects touch how to read this window. They do **not** cancel each other:

| Defect | What it invalidates | What it leaves standing |
| --- | --- | --- |
| **WHI-976** (`amm_quotes` dead on `NoOptimum`) | Nothing about “never simulated” | Null result as *evaluated but unprofitable / no candidate* if discovery actually ran |
| **WHI-980** (Agni-native Swap topic only; drop-in UniV3 Swap missed) | **All watch-mode opportunity / dirty-set / candidate-rate claims** | `--once` one-shots (full rescan, no log filter) |

WHI-980 root cause: `AgniPool::sync_events` only subscribed to the Agni-native Swap topic0 (extra protocol-fee fields). Drop-in UniV3-family venues emit the standard UniV3 Swap topic. Mismatched filter → `logs=0` / empty dirty set under `--watch`.

| Window class | Validity under WHI-980 |
| --- | --- |
| This Agni-22 `--watch` window | **Invalid for watch-mode statistics** (do not re-use the rule-of-three bound for capital). Re-run after dual-topic fix. |
| Full-universe / multi-factory V3 `--watch` (Fluxion-heavy etc.) | **Invalid** — drop-in venues never entered the dirty set. |
| `--once` one-shots | **Still valid** — full rescan does not depend on the log filter. |

**Decision (WHI-980 AC):** WHI-886 is **annotated invalid** (not re-run here). Replacement: post-fix ≥30-minute `--watch` with independent `eth_getLogs` cross-check (DI-33).
