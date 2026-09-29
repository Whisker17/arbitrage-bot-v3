# WHI-1414: min_net_profit against the economics we observe (Release 0.2.2)

Analysis only. No threshold, code path, universe, gas profile, cap or dependency was changed.
All amounts are in WMNT/MNT (1e18 wei). WMNT and MNT are treated 1:1 (see §5).

**Our** figures are **modeled**. Every candidate was gate-blocked (`production_gate_blocked` →
ledger `outcome.kind = env_unsupported`), so none was sent, and none is realized or realizable PnL.
**Competitor** figures are **realized**, taken from on-chain receipts.

## 1. Verdict (AC3 / AC4)

- **Is `min_net_profit` consistent with the opportunity scale? Yes as a floor, but it does not
  protect against the real cost of winning.**
  - The effective floor is **0.01 MNT**.
  - It sits below the competitors' realized median net: 0.0198 across all 223 WMNT arbs, 0.0262
    across the 180 clean ones.
  - **149 / 223 (66.8 %)** of competitor arbs clear it on realized net, and **219 / 223** on gross.
    So the floor would not reject what the competition actually captures.
  - On our side the floor did not bind for the routes that mattered. The 7 pure-route episodes
    had modeled net between **0.036 and 0.544**, which is 3.5–54× the floor.
  - The floor is only close to binding on the cross-protocol v2+v3 routes (0.010–0.053). Those
    are not sendable under the canary policy anyway (§3).
  - Our modeled net leaves out three real costs:
    - the L1 data fee;
    - the Mantle operator fee;
    - any competitive priority fee (§5).
  - Adding only the L1 and operator fees pushes **9 / 26** day rows below the floor (115 / 124
    over the full run).
- **Is there a profitable operating point for this bot? No profitable operating point was shown.
  The evidence we do have is negative.**
  - All **6 / 6** pure-route opportunities we detected on 2026-09-28 were landed by the same
    competitor bot (`0x99bb…13ed`). Each landed in the block right after our last candidate
    block, which is two blocks after our first sighting.
  - That bot paid **51–80 %** of gross in fees (effective gas price 400–4,507 gwei, against a
    50 gwei base fee).
  - Our detection lag was **4–6 s** after the block timestamp. By then the competitor's block
    was already sealed or about to be. With 2 s blocks, we would have needed to detect and land
    within one block.
  - At current latency the expected capture on these opportunities is **0**. Every attempt would
    also have cost gas.
  - The rest of our candidates (110 / 124 rows, 14 / 26 on the day) are cross-protocol, which
    the canary does not send.
  - One of those routes, v2+v3 through Agni `0x2622…`, persisted for **92 blocks** on 09-27 with
    no taker in our evidence. That is unconfirmed as a real opportunity. We have no competitor
    feed for 09-27, and a quote or fee-model error is not excluded.
- **The competition is profitable, and concentrated.**
  - 197 / 223 WMNT arbs were net-positive after all fees.
  - Realized net was 51.43 WMNT on the day (39.18 without the single largest tx), on
    100.70 WMNT gross.
  - Two bots took 118 / 223 txs and 41.96 WMNT of the net.
  - Fees were a median **56 %** of gross (p90 106 %).
- **Scope limits.**
  - This is one UTC day of competitor data.
  - Our evaluation covered **1.35 %** of the dirty-cycle work: 19,326 of 1,427,914
    `cycles_optimized` passed route approval, and 98.65 % were `unapproved_route`.
  - That gives n = 6 same-route comparisons.
  - This is **not** evidence about the engine's achievable returns on routes it cannot yet
    evaluate. For those the answer is **insufficient evidence**.

## 2. Frozen inputs and provenance

| input | sha256 |
| --- | --- |
| host `SHA256SUMS` (13/13 OK on 2026-09-29) | `5668e212fc7b17e129f921b1d321f4462b51d7dd9a959de586e548e536e4969e` |
| `ledger.jsonl` (run `0x70fc4d9b…`, fe4a574) | `cfa0419cfd3b3c53822cacf2485c587aeb84c62afb8463c42761d9c547a7c690` |
| `ledger_cut_20260928.jsonl` | `e53e709d73f20c314a12c1ddae5d156de0e5b5ad227baab0bed38e2221c36c84` |
| logs `signerless.log`, `.1`…`.5` | `c6e52136…c81f`, `8f63d76b…7e42`, `f577b481…baed`, `605cd985…1ec8`, `3944c87c…b962`, `2b736af3…22e1` |
| Dune `SHA256SUMS` (5/5 OK), query 8781229, execution `01M3NGSE7KAE0R0YM8B5HPQNKH` | `36246850ec18780a3ca438037d848b2daf670b19d322eec6a49004666e8bcb9e` |
| `arb_detail_feed_20260928.csv` (252 rows) | `11ed6d1967a8f9a188c104a202975a925b3a3a757d60a9920af3c88a6d7f6cfe` |
| `config/gas_profiles/mantle_mainnet_v1.json` (content_digest `0x3d3244e3…`) | `3e76b9cd368b5efaeb79877b6ab8ff8b17a86e6609542e689920bbaedd9f95ee` |
| RPC cache `SHA256SUMS` (1,145 files; external) | `3b55225b9d2eca3d189614d5c87310bfa4d4ae615f8a0348e6890db97b1cbddb` |
| `summary.json` (this dir; deterministic, re-run identical) | `f94ce28f1511c68e514f10a53a639dfd7aa841881be33703e9e803e5db11b913` |

- **RPC.**
  - All calls were read-only: `eth_getTransactionReceipt`, `eth_getTransactionByHash`,
    `eth_getBalance`, `eth_getBlockByNumber` and `eth_getTransactionCount`.
  - About 1,170 calls in total.
  - Endpoint: env `MANTLE_RPC_URL`, host `rpc-moon.mantle.xyz`. Its keyed path is redacted;
    URL sha256[:12] = `8e959670d88c`.
  - Raw receipts, balances and headers stay in the external cache (`RUN/whi1414-rpc-cache/`).
    None are committed.
- **Worktree base.** `merge-base HEAD origin/dev` = `origin/dev` = `c62ff0c`, and fe4a574 is an
  ancestor.
- **The earlier 09-27 frozen window** (the deployment issue's artifacts) was not used separately.
  The full rc2 ledger above already contains 09-27 from 02:40Z.

## 3. Thresholds and caps (what we compare against)

- **`min_net_profit` = 1e16 wei (0.01 MNT).**
  - How it is set:
    - `ServiceConfigOpts::agni_v3()` (`src/bin/bot.rs:498`);
    - `V3_MIN_PROFIT_FLOOR_WEI` (`src/service/config.rs:60`);
    - clamped by `read_min_profit_threshold` (`src/service/config.rs:514`);
    - net ≥ gross threshold (`src/service/config.rs:267`);
    - `discovery.min_profit = config.min_net_profit` (`src/bin/bot.rs:837`).
  - Admission is on **net**: `if net_profit < config.min_profit` (`src/service/path_index.rs:1132`).
  - The value is **inferred**. It relies on the host env-name inventory recorded in
    `evidence/replay/whi-1527/STATUS.md` §9, which shows no `MIN_NET_PROFIT_WEI`. The host `.env`
    was not read.
  - The data is consistent with it: the smallest candidate net over 124 rows is 0.010061.
- **The ledger `min_profit` field is the candidate's modeled net, not the floor.**
  `ExecutionAttempt::ProductionGateBlocked { min_profit: candidate.net_profit }`
  (`src/service/protocol.rs:263`).
  - It was cross-checked against the log `block_summary best_net` on the 30 candidate blocks the
    log covers: 30 / 30 equal.
- **Modeled gas.**
  - Formula: `expected_gas_used × (base_fee + priority)` (`src/execution/fee_context.rs:213`).
  - The route is costed at its gas bucket (`src/service/path_index.rs:1063`):
    - v2+v3 `ticks=0` = 236,620 (`mantle_mainnet_v1.json:487`; the only approved v2+v3 bucket);
    - v2×3 = 302,762 (`:323`).
  - Priority is 1e5 wei (`src/execution/types.rs:74`; no `EXECUTOR_PRIORITY_FEE_WEI`, inferred
    as above).
  - The base fee was 50 gwei on all 124 candidate blocks, read from headers.
  - Our modeled gross is therefore net + modeled gas.
- **Inventory and loss caps (the fund-and-canary issue).**
  - **No numeric value exists in the repo.**
  - `MAX_LOSS_PER_WINDOW_WEI`, `MAX_INPUT_PER_TX_WMNT_WEI` and `MAX_TOTAL_INVENTORY_WMNT_WEI` are
    mandatory with no default (`src/execution/breaker/config.rs:53`, `:57`, `:61`). The loss
    window is 300 blocks (`:42`).
  - The same three are unset in the template (`env.mainnet.example:99-101`).
  - They are to be fixed at the second human go/no-go (`specs/06-milestones-and-issues.md:1234`).
  - The only concrete figures on record:
    - the anvil rehearsal notional of 0.01 WMNT (`evidence/golive/fund-and-canary/REHEARSAL.md:13`);
    - the shadow assumed capital of 10 WMNT (`src/service/capital_bound.rs:48`; `run_plan.json`).
  - Comparison against the 10 WMNT shadow cap: **12 / 12** pure-route day rows ran at the cap.
    Competitors on the same triangle used 12.2–31.2 WMNT.
- **Canary sendability.**
  - Cross-protocol routes are statically ineligible (`src/service/eligibility.rs:31`, `:152`).
  - With the gate closed, only the top-1 candidate per block is attempted and recorded
    (`:236`).
  - The log shows 2 blocks with more than one candidate (101245536, 101245537). Candidates below
    top-1 are not in the ledger.

## 4. Our candidates: modeled, post-fix rc2 shadow (AC1)

**Coverage (exact denominators, from ledger counters).**
- **Full run.** 85,390 discovery observations.
  - `cycles_optimized` 2,418,350, of which `unapproved_route` 2,385,220 (98.63 %).
  - paths_quoted 19,966.
- **Day.** 42,528 observed blocks out of 43,200.
  - `cycles_optimized` 1,427,914, of which `unapproved_route` 1,408,588 (98.65 %).
  - paths_quoted 11,111.

**Ledger vs log block coverage, reconciled:**
- The chain-day bounds 101211644..101254843 were verified from headers (ts 1790553600 /
  1790639998; neighbours outside).
- The log names **43,199** day blocks by chain bounds. The provenance figure of 43,198 counts
  log lines by wall-clock date instead. It adds 2 blocks (101211642/643: chain time 09-27, logged
  after 00:00Z on 09-28) and drops the last 3 day blocks (logged after 00:00Z on 09-29).
- **671** blocks are in the log but not in the ledger. Every one is a skip summary with
  `candidates=0 cycles_evaluated=0`:
  - `processing_failed` 501;
  - `pinned_logs_unavailable` 153;
  - `pinned_header_unavailable` 17.
- **1** block (101238115) is in neither.
- The 672 unobserved blocks were therefore never evaluated, and no candidate was lost from the
  ledger.

**Distinct opportunities.**
- A route's candidate blocks are split into **state episodes**. A new episode starts when an
  intervening observed block re-evaluated the route (Full scope, or any route pool dirty) and
  recorded no candidate at all.
- Simple consecutive-block runs are also given.

| window | rows | distinct blocks | routes | state episodes (consecutive runs) | cross-protocol / pure rows |
| --- | ---: | ---: | ---: | ---: | ---: |
| full rc2 run (09-27 02:40Z → 09-29 02:47Z) | 124 | 124 | 5 | 14 (16) | 110 / 14 |
| UTC day 2026-09-28 | 26 | 26 | 4 | 10 (10) | 14 / 12 |

| modeled net, WMNT | n | min | p25 | p50 | p75 | p90 | max | sum |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| full, per row | 124 | 0.0101 | 0.0149 | 0.0149 | 0.0149 | 0.0532 | 0.5445 | 4.580 |
| full, best per episode | 14 | 0.0116 | 0.0209 | 0.0355 | 0.1431 | 0.2848 | 0.5445 | 1.578 |
| day, per row | 26 | 0.0101 | 0.0116 | 0.0209 | 0.1431 | 0.2848 | 0.5445 | 2.963 |
| day, best per episode | 10 | 0.0116 | 0.0188 | 0.1271 | 0.1461 | 0.2848 | 0.5445 | 1.446 |
| day, per row − L1 fee (competitor median 0.00077) − operator fee (gas × 1e10) | 26 | 0.0069 | 0.0085 | 0.0178 | 0.1393 | 0.2810 | 0.5407 | 2.873 |

Per route over the full run (`summary.json` → `ours.full.routes`):

| route | rows | episodes | modeled net min–max | amount_in |
| --- | ---: | ---: | --- | --- |
| v2 `0x3e59…` → v3 Agni `0x2622…` (mixed) | 106 | 5 (one of 92 blocks on 09-27) | 0.0101–0.0222 | 3.46–4.31 |
| v2×3 `0x7638…→0xb670…→0xefc3…` (pure) | 10 | 5 (all 09-28 00:03–00:05Z) | 0.1271–0.2848 | 10 (cap) |
| v2×3 reverse `0xefc3…→0xb670…→0x7638…` (pure) | 4 | 2 | 0.0355–0.5445 | 7.21–10 (2 of 4 at the cap) |
| v2 `0x3e59…` → v3 `0xf449…` (mixed) | 2 | 1 | 0.0532 | 5.40 |
| v2 `0x3e59…` → v3 `0xd08c…` (mixed) | 2 | 1 | 0.0188 | 4.10 |

Detection lag (ledger `recorded_at_unix − block_timestamp`):
- full run: p50 5 s, p90 19 s, max 33 s;
- day: p50 6 s, max 12 s.

## 5. Competitors: realized, same UTC day (AC2)

**Basis (same units as ours).**
- `gross` = settlement-asset Transfer logs into minus out of {executor (`tx.to`), sender EOA}.
- `fee_total` = `gas_used × effectiveGasPrice + l1Fee + gas_used × operatorFeeScalar × 100 +
  operatorFeeConstant`. All of it is paid in MNT.
- `net` = gross − fee_total, with WMNT treated as MNT 1:1. WMNT is the 1:1 wrapped native
  token, so the only caveat is the unwrap step: 0 WMNT Deposit/Withdrawal events appeared in any
  receipt.
- **L1 and operator fees on Mantle are included.** Both come from the receipt (`l1Fee` in MNT;
  `operatorFeeScalar` = 1e8 on every tx, i.e. 1e10 wei per gas).
- The fee formula matched the sender's native-balance drop **exactly** on **199 / 206** checkable
  txs (value = 0, one feed tx per sender per block). The other 7 are each explained by a second
  sender tx in the same block (`sender_nonce_delta_in_block = 2`).
- Dune `gas_used` and `gas_price` equal the receipt on 252 / 252.
- **Survivorship.** The feed holds only successful qualified arbs. The competitors' reverted or
  losing attempts, such as the extra same-block txs above, are **not** charged. Realized competitor
  net is therefore an upper bound on their strategy net.

**Exclusions (denominator 252).**
- 0 reverted.
- **29 non-WMNT settlement** are excluded, since there is no price basis: USDT `0x201e…` 10,
  `0x09bc…` 6, `0x779d…` 6, `0xcda8…` 4, native `0xdead…1111` 2, `0xe682…` 1.
- **223** WMNT-settled txs are included.
- 43 of those leave a non-zero delta in another token at executor/sender, all of them executor
  `0x5752…`. They are reported in "all" and excluded from "clean" (n = 180).
- 25 txs carry `value = 3 wei` (one bot). This is negligible and ignored.

| competitor, WMNT | n | p10 | p25 | p50 | p75 | p90 | max | sum |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| gross, all WMNT | 223 | 0.0199 | 0.0291 | 0.0464 | 0.1062 | 0.4728 | 24.786 | 100.700 |
| fee_total | 223 | 0.0172 | 0.0207 | 0.0287 | 0.0589 | 0.1191 | 12.537 | 49.267 |
| **net, all WMNT** | 223 | −0.0017 | 0.0040 | **0.0198** | 0.0442 | 0.3051 | 12.250 | **51.432** |
| net, clean | 180 | −0.0039 | 0.0151 | 0.0262 | 0.0634 | 0.3821 | 12.250 | 51.261 |

- **Fee components (all WMNT, p50):** L1 fee 0.00077, operator fee 0.00353, effective gas price
  55.5 gwei. The fee_total minimum is 0.0114.
- **Fee share of gross:** p50 56.3 %, p90 106 %.
- **Clearing our floor:**
  - net > 0 on 197 / 223;
  - net ≥ 0.01 on **149 / 223 (66.8 %)**; clean 146 / 180;
  - gross ≥ 0.01 on 219 / 223.
- **Gross per day:** 100.70 WMNT (75.91 without the largest tx).
- **Bots:** 31 bots settle in WMNT. The top two (`0x9979…`: 87 txs, net 27.89; `0x99bb…`: 31 txs,
  net 14.07) take 81.6 % of the net.
- **Hops:** 2-hop 104, 3-hop 76, 4+ 43.
- **Against the issue's reference figures.**
  - The issue's worked example (`0x2025c952…`, net ≈ 0.0545) is not in this day's feed.
  - On this day the median arb nets 0.0198. The mean is 0.231, skewed by two txs of 10.4 and
    12.2 WMNT.
  - The prior "$15/day" context cannot be reconciled here without a price source.

## 6. Like-for-like: the same routes, the same day

These are the competitor txs on the pool set of one of our candidate routes, matched on the same
trade direction (first pool to receive WMNT from the executor). Dune's `ordered_pools` does not
encode direction.

| block | competitor bot | amount_in | gross | fee | net | fee/gross | our candidate blocks | our modeled gross / net (10 WMNT cap) | our lag |
| --- | --- | ---: | ---: | ---: | ---: | ---: | --- | --- | ---: |
| 101211744 | 0x99bb…13ed | 19.72 | 0.3950 | 0.2106 | 0.1844 | 53 % | 742, 743 | 0.3000 / 0.2848 | 5 s |
| 101211761 | 0x99bb…13ed | 13.08 | 0.1706 | 0.0880 | 0.0827 | 52 % | 759, 760 | 0.1613 / 0.1461 | 6 s |
| 101211772 | 0x99bb…13ed | 12.20 | 0.1470 | 0.0761 | 0.0709 | 52 % | 770, 771 | 0.1422 / 0.1271 | 4 s |
| 101211781 | 0x99bb…13ed | 13.09 | 0.1675 | 0.0862 | 0.0813 | 52 % | 779, 780 | 0.1582 / 0.1431 | 6 s |
| 101211799 | 0x99bb…13ed | 12.86 | 0.1601 | 0.0852 | 0.0749 | 53 % | 797, 798 | 0.1522 / 0.1371 | 6 s |
| 101244098 | 0x99bb…13ed | 31.15 | 1.0291 | 0.8256 | 0.2035 | 80 % | 244096, 244097 (reverse route) | 0.5596 / 0.5445 | 6 s |

- **All 6 of our pure-route day episodes appear here, and all 6 were lost to the same bot.** Each
  loss landed at our last candidate block + 1.
- On these 6 the competitor realized 2.069 gross − 1.372 fee = **0.698 WMNT net**.
- On the same 6 our model said **1.383 WMNT net**, at the capped size and a 50 gwei + 1e5 wei
  price.
- The gap is the priority auction: they paid 400–4,507 gwei per gas.
- Paying the median competitor fee share (56 %) instead of our modeled gas, 21 / 26 day rows would
  still clear 0.01. That does not win this race, though: the same-route competitor paid 51–80 %.
- None of our 14 cross-protocol day rows has a competitor counterpart on the day.

## 7. Limits

- The competitor side covers one UTC day, and our side covers one ~48 h run. There are n = 6
  same-route comparisons, all against one competitor bot. No confidence bound is claimed.
- Our figures are modeled and gate-blocked. Some of them depend on inferred config:
  `min_net_profit`, the priority fee, and the gas bucket, with v2+v3 assumed `ticks=0`, the only
  approved bucket.
- The L1 fee for our rows uses the competitor median as a proxy. Our actual calldata size is
  unknown.
- The cross-protocol candidates are not sendable under canary policy. The 92-block episode on
  09-27 is unverified.
- Evaluation coverage is 1.35 % of `cycles_optimized` (98.65 % `unapproved_route`). Conclusions
  apply only to the approved route slice.
- There is no USD conversion. Non-WMNT competitor arbs (29 / 252) are excluded.

## 8. Reproduce

```bash
RUN=<orchestrator run dir>/release-022
# 1) read-only RPC → external cache (idempotent; skips cached files)
MANTLE_RPC_URL=… python3 evidence/economics/whi-1414/fetch_rpc.py \
  $RUN/sept28-inputs/dune/arb_detail_feed_20260928.csv $RUN/sept28-inputs/host/ledger.jsonl $RUN/whi1414-rpc-cache
# 2) offline analysis → summary.json (stdlib only; deterministic)
python3 evidence/economics/whi-1414/analyze.py $RUN/sept28-inputs/host \
  $RUN/sept28-inputs/dune/arb_detail_feed_20260928.csv $RUN/whi1414-rpc-cache \
  config/gas_profiles/mantle_mainnet_v1.json evidence/economics/whi-1414/summary.json
```
