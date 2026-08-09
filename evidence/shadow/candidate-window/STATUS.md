# WHI-862 — Candidate rate on the frozen 59-pool universe

**Issue:** [WHI-862](https://linear.app/whisker-personal/issue/WHI-862)  
**Purpose:** Measure whether arbitrage opportunities exist (and at what rate) on the committed multi-protocol universe **before** committing custody risk. Feeds WHI-535 threshold design.  
**Mode:** Signerless start-to-finish (`production_send_allowed() == false`, ledger `send_capability=no_send`).

## Universe under test (main)

| Field | Value |
| --- | --- |
| Path | `data/pool_universe.csv` + `.meta.json` (committed on this branch) |
| Fingerprint | `0x19ce9ed1d60f12d4c577576912a79c859c4faac983d67b46d26355deff389c85` |
| Snapshot block | 98,795,302 |
| Pool count | **59** (`agni-v2`=4, `agni-v3`=18, `moe`=37) |
| Filter | TVL floor **1000 WMNT**, ≤3-hop WMNT settlement cycles |

## Liveness gate (before treating the window as a market sample)

Separate from the final `blocks_processed` total. Confirmed on the same binary/universe before relying on zeros:

1. Unified universe load: 59 pools, fingerprint above.
2. `StateSpaceBuilder::sync` succeeded (`synced pool state pools=59`) on a one-shot and again at window start.
3. Watch loop entered (`entering multi-protocol --watch`).
4. First `blocks_processed ≥ 1` observed early in the window (log progression from 1 upward); final log max was **29**.
5. Shadow ledger opened with `send_capability=no_send`, service=`bot` (run_header before any market claim).

**Pipeline fixes required to get here** (same PR):

- Soft-skip `AMMError::MoeError(IncompleteState)` in path simulation (was only matching top-level `IncompleteState`).
- Discovery continues on optimize errors instead of aborting the whole pass.
- Moe bin CREATE eth_calls: bounded concurrency + CreateContractSizeLimit retry.
- `MOE_BINS_RADIUS` aligned to snapshot default **50** (was 200 → ~7 min/block tip-refresh on free-tier RPC).

**Attempt 1 (aborted):** radius=200 tip-refresh too slow; archived under `attempt1-slow/`.

## Window parameters (main)

| Field | Value |
| --- | --- |
| Duration | **~2.2 h** ledger wall (`runtime_seconds=7891`); active discovery ticks concentrated in the first ~30 min then sparse due to long tip-refresh / re-baseline gaps on free-tier RPC |
| Justification | Multi-hour signerless observation, far beyond WHI-526’s 202-block gate sample in wall time; effective processed heads limited by per-block multi-protocol tip refresh (~1 min when healthy) |
| Protocols | `agni-v2,agni-v3,moe` (merged graph) |
| Head source | `http-poll` |
| Ledger | `ledger.jsonl` |
| `production_send_allowed` | **false** |
| Broadcast | **0** |

## Results — main (1000 WMNT floor)

Source: `analysis_main.json`.

| Metric | Value |
| --- | --- |
| Runtime (ledger span) | 7891 s |
| Observations / unique blocks | **30 / 30** |
| Block span (min→max observed) | 98899018 → 98902828 (**3810** chain blocks) |
| `blocks_processed` (log) | **29** |
| Discovery passes with `opportunities>0` | **0 / 29** |
| Candidates (ledger) | **0** |
| Candidates / day (extrapolated) | **0** |
| Gross-profitable count | **0** |
| Net-profitable-after-gas count | **0** |
| Topology mix | *(empty — no candidate/context rows)* |
| Net-profit distribution | *(n/a — no samples)* |
| Per-protocol candidates | **Merged bot only** — no per-protocol ledger identity. Protocol-subset attribution: **agni-v2=0, agni-v3=0, moe=0, cross=0** (no candidate rows to attribute). |
| Skip / re-baseline | 36 log hits for re-baseline / small-gap backfill when tip-refresh lagged `CACHE_SIZE=30` |
| `send_capability` | `no_send` |
| `broadcast_count` | **0** |

### Three-outcome classification (step 5)

| Class | Holds? |
| --- | --- |
| A — opportunities exist and clear gas | **No** |
| B — opportunities exist but gas eats them | **No** (no gross-positive paths either) |
| **C — no opportunities at all** | **Yes** |

**Conclusion (main):** On the committed 59-pool universe, over a multi-hour signerless window with a live pipeline (`blocks_processed=29`, fingerprint pinned), the bot found **zero** sized opportunities. This is a **market/strategy/universe** measurement (class C), not a silent zero from a dead watch loop.

### WHI-976 instrumentation note (post-hoc)

Later live `--watch` summaries showed `cycles_evaluated > 0` with **`amm_quotes = 0`** on every evaluating block. WHI-976 settled that as a **dead counter**, not skipped simulation: `optimize_path` discarded quote counts on `NoOptimum` (the common unprofitable path). Simulations still ran; only the work counter was wrong.

**Validity of this class-C result:** **still valid** as “no sized opportunity found after optimize + mix-sim.” It must **not** be re-read as “we never simulated.” Re-measure candidate rate on the fixed counter for operator confidence (WHI-955); do not throw out the market zero solely because of WHI-976.

**Coverage bound:** Tip refresh loads Moe bins within `MOE_BINS_RADIUS=50` (snapshot default). Incomplete-state paths soft-skip. Class C is therefore “no opportunity within the synced radius / current optimizer,” not a proof that a wider bin window or different sizing would never find arb.

## Lower-TVL comparison (step 6)

Experiment only — does **not** change the committed default 1000 WMNT floor.

| Field | Main | Low-TVL experiment |
| --- | --- | --- |
| Floor | 1000 WMNT | **100 WMNT** (`1e20` wei) |
| Universe path | `data/pool_universe.csv` | `candidate-window-lowtvl/universe/pool_universe.csv` |
| Fingerprint | `0x19ce9ed1…` | `0x17493475…` |
| Snapshot block | 98795302 | 98902895 |
| Pool count | 59 (v2=4, v3=18, moe=37) | **64** (v2=**0** seed missing, v3=20, moe=44) |
| Funnel note | — | enumerated 213 → tvl_ok 89 → cycle_ok 64 |
| Window | ~2.2 h ledger span (`runtime_seconds=7891`) | ~31 min ledger span (`runtime_seconds=1857`) |
| Observations | 30 | **25** |
| `blocks_processed` (log) | 29 | **23** |
| Candidates | **0** | **0** |
| Candidates/day | 0 | 0 |
| Per-protocol candidates | all 0 | all 0 (no V2 pools in this universe) |
| Three-outcome class | **C** | **C** |

**Filter hypothesis:** Lowering the TVL floor 10× **adds ~5 pools** (mostly Moe) but **does not** produce candidates in a comparable multi-block sample. The 1000 WMNT floor is **not** the sole reason for zero candidates in this window. (Caveat: low-TVL run had no V2 pools because `data/poolLists_v2.csv` was missing at generation time — WHI-863 seed hygiene.)

## Proposed WHI-535 thresholds (step 7)

| Threshold | Proposal | Reused / new | Notes |
| --- | --- | --- | --- |
| `required_services` | full canonical 4-service list incl. `bot` | **Reused** (WHI-740) | Order-sensitive |
| `min_canonical_blocks` | **≥ 25** (near measured unique obs) | **New** | Do not require hundreds if tip-refresh rate cannot deliver on available RPC |
| `min_runtime_seconds` | **≥ 1800** (prefer 3600) | Reused shape | Prefer multi-hour when RPC allows |
| `min_candidate_rows` | **0 for eligibility scaffolding; gate Approve must not require positive candidates until class A is observed** | **New** | Measured rate is 0/day |
| `min_real_preflight_samples` | **0** until candidates exist | **New** | |
| `topology_coverage.required_route_keys` | **Retire `h2:v2+v2`** | **Retired** | Only **4** V2 pools; pure V2+V2 2-hop is structurally unrealistic. Replace with observed keys when class A appears, or drop topology hard-fail |
| Profit distribution | Do **not** require positive net fraction while class C | **New** | |
| Continuity | Allow re-baseline events on free-tier RPC | Softened | |

### Explicitly retired

- **`h2:v2+v2` required topology** — with 4 V2 pools (and 0 V2 on the low-TVL seed), carrying WHI-526’s key forward would deadlock WHI-535 the same way zero candidates did for WHI-526.

### Implication for capital

Class C means **do not fund** the strategy/universe as currently measured. WHI-535 should treat positive candidate thresholds as **aspirational only after** a non-zero measurement (or after venue/inventory changes per WHI-765), not as Approve criteria that fail closed forever.

## Artifacts (no credentials)

```text
evidence/shadow/candidate-window/
  STATUS.md
  run_plan.json
  thresholds.json          # pin-only, not WHI-535 gate criteria
  ledger.jsonl
  analysis_main.json
  analyze_ledger.py
  logs/window-main.log     # truncated head+tail
  attempt1-slow/           # aborted radius=200 attempt
evidence/shadow/candidate-window-lowtvl/
  run_plan.json
  ledger.jsonl
  analysis.json
  universe/pool_universe.csv + .meta.json
  logs/window.log          # truncated
  logs/universe_gen.log
```

## Safety

- No private keys in process env (`SHADOW_MODE=1` + unset `*_PRIVATE_KEY`).
- Ledger `send_capability=no_send` on both windows.
- `production_send_allowed()` hard-false; `broadcast_count == 0`.
- RPC URLs / keys never written into this tree.
