# Mantle competitor-monitoring pack (WHI-1406)

Four DuneSQL files, one shared qualification backbone plus three public
deliverables mapped 1:1 to the operator's asks. **Not** a Rust crawler, not a
WHI-957 clone, not an operator-identity/clustering tool.

| File | Role | Dune query id (public) |
| --- | --- | ---: |
| `00_qualified_arbs.sql` | Shared qualification backbone — **not itself a deliverable** | [8781215](https://dune.com/queries/8781215) |
| `01_discover_bots.sql` | Ask #1 — which sender addresses, how many | [8781227](https://dune.com/queries/8781227) |
| `02_arb_detail_feed.sql` | Ask #2 — full per-tx detail feed, sorted by recency | [8781229](https://dune.com/queries/8781229) |
| `03_bot_strategy_profile.sql` | Ask #3 — observed per-address strategy profile | [8781231](https://dune.com/queries/8781231) |

Dashboard (all three public deliverables + methodology summary):
**<https://dune.com/mantlexyz/whi-1406-mantle-competitor-monitoring-pack>**

`01`/`02`/`03` all read `00`'s saved output via Dune's ["Query a
Query"](https://docs.dune.com/query-engine/query-a-query) feature
(`FROM "query_8781215(start_time='...', end_time='...', from_block='...',
to_block='...')"`). `03` additionally reads `01` (`query_8781227`) for the
per-address fields the two share (`arb_tx_count`, `first_seen`/`last_seen`,
`hop_count_distribution`, `hop_mix_distribution`) rather than recomputing
them from `00` a second time — `01` is the single source for those columns,
`00` remains the single source for qualification. There is exactly one copy
of the qualification logic in this pack.

## Parameters (identical across all four files)

| Param | Type | Default | Meaning |
| --- | --- | --- | --- |
| `start_time` | text, `YYYY-MM-DD HH:MM:SS` UTC | `2026-06-01 00:00:00` | Half-open window start. Always required for Dune partition pruning, even in block-interval mode. |
| `end_time` | text, `YYYY-MM-DD HH:MM:SS` UTC | `2026-09-01 00:00:00` | Half-open window end (exclusive). |
| `from_block` | number | `-1` | `-1` = time-window only. Set `>=0` with `to_block` to additionally pin an exact block interval (e.g. a frozen WHI-955 ledger window derived from that ledger's own min/max block — not the approximate JP start `100872173`). |
| `to_block` | number | `-1` | See above. |
| `bot_address_filter` (02 only) | text | `''` | Non-empty lowercased `0x...` restricts to one address; `''` returns every row. |

The default window above is a **trailing 3 calendar months with a fixed,
reproducible UTC end** (the most recent completed calendar-month boundary at
implementation time), not an approximated "90 days ago" placeholder. Every
result row carries `resolved_start_time` / `resolved_end_time` /
`resolved_from_block` / `resolved_to_block` so any export is self-describing.

**Maintenance note:** because "Query a Query" addresses the upstream query by
its numeric id (`query_8781215`, `query_8781227`), re-saving `00` or `01`
under a *new* query id (rather than updating the existing one in place) would
require updating that literal id in every downstream `.sql` file and this
README. This is a structural property of Dune's query-view mechanism, not a
choice made by this pack — DuneSQL has no macro/include system that would let
a stable logical name survive a query being re-created from scratch. Editing
an existing saved query's SQL in place (as this pack does) does not have this
problem; only replacing a query id would.

## Tables used and why

* **`dex.trades`** (curated Dune spell, `blockchain='mantle'`) — already
  decodes swaps per DEX. We never hand-decode swap topics. Observed `project`
  values on Mantle: `agni`, `fusionx`, `merchant_moe`, `uniswap`, `clipper`,
  `carbon_defi`, `swaap`, `tropicalswap`. No `tx_index` column (that comes
  from `mantle.transactions.index` instead). `project` is a brand label, not
  an AMM family: `merchant_moe` covers both Moe V1 classic (UniV2 pairs) and
  Moe Liquidity Book, and `fusionx` covers FusionX V2 and V3 — so `hop_mix`
  maps each leg by its pool (`project_contract_address`) instead (see
  "Protocol-family hop mix" below).
* **`tokens.transfers`** (curated Dune spell, `blockchain='mantle'`) — used
  for the entity-net qualification (see below and "Native/wrapped limits").
* **`mantle.transactions`** / **`mantle.blocks`** — `success`, `value`
  (msg.value), `gas_price`, `gas_used`, `gas_limit`, `max_fee_per_gas`,
  `max_priority_fee_per_gas`, `priority_fee_per_gas`, `index` (tx position),
  `type`; `base_fee_per_gas` from blocks.
* **`aave_v3_mantle` / `lendle_mantle` / `aurelius_finance_mantle`**
  `*_evt_liquidationcall` / `*_evt_flashloan` — decoded Aave-fork lending
  markets on Mantle, verified live (real, recent rows at implementation
  time) and used for the liquidation exclusion and the flash-loan marker.
  A fourth Mantle lending market, `init_capital_mantle`
  (`init_core_evt_liquidate`), was checked and has **zero** events in the
  trailing-100-day probe window — not included in the union (would be a
  no-op for this window; revisit if a future window shows activity there).
* **`dex.sandwiches` / `dex.sandwiched`** — checked live. They **do** cover
  `blockchain='mantle'` (521 / 290 rows total, 2024-03-27 to 2026-03-31), but
  have **zero** rows inside the trailing-3-month default window above. `00`
  re-checks this coverage per invocation and labels `is_sandwich = 'unknown'`
  for every row whenever the covered window has zero rows for the requested
  bounds — never a silent `'false'`. Use an older window (inside the
  2024-03-27..2026-03-31 span) to get real `'true'`/`'false'` sandwich
  labels from these tables.
* **JIT LP**: no verified Dune marker was found for Mantle JIT liquidity
  provision at implementation time. `is_jit_lp` is always `'unknown'`.

## Native/wrapped limits

`tokens.transfers` is an ERC-20/token-standard transfer feed — it does
**not** carry native-MNT value-transfer legs (`mantle.transactions.value`
movements are not token transfers). Consequences for the entity-net
qualification in `00`:

* A tx that settles an arb purely in **native MNT** (rather than WMNT or any
  ERC-20) would show **zero** `tokens.transfers` legs for that settlement
  leg. Since qualification requires `>=1 token net-positive` leg from
  `tokens.transfers`, such a tx is **excluded**, not misclassified — it fails
  closed (dropped from the qualified set) rather than being silently
  mislabeled with a wrong `settlement_asset`.
* Every `settlement_asset` this pack ever reports is therefore a token
  contract address (WMNT/USDT0/mETH/USDe/etc. observed in samples) — never
  the native MNT "address". This is a real coverage limit, not a bug: bots
  that settle exclusively in native MNT are invisible to `01`/`02`/`03`.
  Bots that settle in WMNT (the wrapped native token, an ERC-20) are fully
  covered, and WMNT is overwhelmingly the observed settlement asset in
  samples taken so far.
* `msg.value <= 1e18` (the CEX-DEX exclusion) is evaluated independently on
  `mantle.transactions.value` and is unaffected by this limit.

## History / continuity check (AC requirement)

Verified live via the Dune MCP before finalizing the pack — not assumed from
the earlier JP-window sample:

* `dex.trades` (blockchain='mantle'): continuous since **2023-07-02**,
  0 gap days in the trailing 100 days, and **92/92 days with data** for the
  exact default window `2026-06-01`–`2026-08-31` (inclusive).
* `tokens.transfers` (blockchain='mantle'): continuous since **2023-07-02**,
  same 92/92-day check passes for the default window.
* Both tables are partitioned by `block_month`/`blockchain` (and
  `tokens.transfers` also by `block_month`), so `00` filters on
  `blockchain='mantle'` + `block_time` range for partition pruning.

No non-overlapping partitioned export was needed — a single full-window
execution of `00` succeeded directly (see below).

## One successful execution window with real row counts

Default window `2026-06-01 00:00:00` → `2026-09-01 00:00:00` (UTC),
`from_block`/`to_block` = `-1`/`-1`:

| Query | Result |
| --- | ---: |
| `00_qualified_arbs` | **21,741** qualified transactions (block range 96,071,749–100,044,538) |
| `01_discover_bots` | **90** distinct qualified sender addresses; every `arb_tx_count` sums to the `00` row count |
| `02_arb_detail_feed` (unfiltered) | **21,741** rows — verified equal to `00`'s row count for the same parameters |
| `03_bot_strategy_profile` | **90** rows (same address set as `01`) |

Execution cost was cheap (<1 Dune credit for `00`'s full-window pass; low
single-digit credits for `01`/`02`/`03` via query-a-query) — well within the
enterprise credit budget.

**Honesty note on reproducibility:** re-running `00` for the identical frozen
window minutes apart during development produced counts varying by <0.1%
across back-to-back executions. Mantle's curated `dex.trades` /
`tokens.transfers` spellbook tables are live-refreshed and can very rarely
reprocess a historical partition (e.g. a late project-label decode fix)
between two executions of the same query. The qualification **logic** is
deterministic given fixed inputs; the **inputs** (Dune's curated tables) are
not perfectly immutable for historical windows on a live workspace. This is
disclosed rather than hidden — do not expect bit-for-bit identical row counts
across arbitrary re-runs days apart; do expect them to agree to within
noise. The `events_fingerprint` produced by `ground_truth_collector` from a
single **frozen exported CSV** (not two independent Dune executions) is
exactly reproducible — see below.

## Manual verification sample (JP probe window)

Small sample cross-checked directly against raw-table recomputation
(independent of `00`'s own SQL), for the JP probe window
`[100872173, 100876184]` (66 qualified rows post-fix, 27 bots — see "Round-1
correctness fixes" below for why this is lower than the original 68/29):

* Tx `0xf2c275ca888778661457d6a738cf1b14024014459695d6880651780a7b3f881c`:
  `00` reports `hop_count=3` (merchant_moe → uniswap → agni),
  `settlement_asset` = mETH, `effective_tip_per_gas=115000330000`. Recomputed
  directly from `dex.trades` (3 legs, same projects/order),
  `mantle.transactions`/`mantle.blocks` (`priority_fee_per_gas=115000330000`
  exactly), and `tokens.transfers` (mETH net `+0.0000228628...` raw-exact,
  every other touched token net exactly `0` in `DECIMAL(38,0)` raw
  arithmetic — WETH, MOE, WMNT) — all fields match exactly, before and after
  the round-1 raw-amount fix (this tx's own external outflow was already
  correctly attributed).

Limitations: this is a structural/aggregate cross-check (raw-table
recomputation), not a re-simulation of trade profitability. Full raw event
dumps stay external per repo convention; only this aggregate note and the
committed SQL are checked in.

**What was and was not executed (reconciliation with the original
acceptance wording).** The pack's acceptance asked for a small sample checked
against **receipts** + token transfers.

* **Executed for this pack:** the recomputation of one tx above, from Dune's
  raw tables (`dex.trades`, `mantle.transactions`/`mantle.blocks`,
  `tokens.transfers`).
* **Not executed for this pack:** an RPC receipt (`eth_getTransactionReceipt`
  / log) comparison, as literally specified.
* **Later receipt checks (not part of this pack's sample):** the 2026-09-28
  concurrent-window rows of `02` were checked against read-only RPC receipts
  in `evidence/economics/whi-1414/STATUS.md` §5: Dune `gas_used`/`gas_price`
  equal the receipt on 252/252, and settlement gross comes from receipt
  Transfer logs. `evidence/peer-attribution/whi-1412/STATUS.md` reads six
  peers' receipts. These are later, stronger cross-checks of the same query's
  output. They do not retroactively make the original sample a receipt
  comparison.

**Why the JP-window rate (66 txs / ~2.2h ≈ 30/hour) looks higher than the
default-window average (21,741 / 92 days ≈ 9.8/hour):** the JP window was
chosen because it corresponds to a known active trading period for the
signerless shadow watch, not a randomly sampled hour. Bursty, time-of-day-
and volatility-dependent arb activity is expected for real on-chain
competition; a single active window running ~3x the 92-day average is not by
itself evidence of a bug and is consistent with real market microstructure
(this pack makes no claim that activity is uniform across the day).

## Round-1 correctness fixes (post-initial-implementation)

Two review findings changed `00`'s qualification results and are recorded
here rather than silently folded into "how it always worked":

1. **Entity-net sign classification now uses `tokens.transfers.amount_raw`
   cast to `DECIMAL(38,0)`, not the decimal-adjusted `amount` (double).**
   Floating-point summation does not guarantee an internal `tx.from<->tx.to`
   transfer cancels to a literal `0` when other legs are also present (the
   individual value cancels exactly, but SQL summation order across many
   terms is not float-associative) — the exact-integer path removes this
   risk entirely. The decimal-adjusted `amount` is still used, deliberately,
   for the settlement-asset tie-break ranking across different tokens (see
   `00`'s header comment) since that is a display/ranking choice, not a
   qualification sign check, and using it there avoids introducing any
   USD/price comparison (explicitly out of scope).
2. **`total_gross_out` now counts only genuinely external outflow**
   (`from IN entity AND to NOT IN entity`), not any leg where the sender is
   in the entity. The previous version could let a tx that only ever
   shuffled tokens internally between its own `tx.from`/`tx.to` pass the
   "entity actually sent tokens" check on an unrelated net-positive external
   receipt of a different token — i.e. a "free tokens, sent nothing out"
   case could slip through. Fixed by requiring the outflow itself to leave
   the entity.

Effect on the full default-window numbers: qualified txs dropped from
21,920 → **21,741** (-0.8%) and distinct bot addresses from 104 → **90**
(several addresses had *only* the now-excluded internal-gross-out pattern).
The JP-window sample dropped from 68/29 → 66/27 for the same reason. This is
the qualification logic becoming stricter and more correct, not new scope.

## Definitions carried unmodified from `00` into every downstream query

* `hop_count` — raw integer count of `dex.trades` legs for the tx (no dedup,
  ordered by `evt_index`).
* `hop_count_bucket` — `'2-hop'` / `'3-hop'` / `'>3-hop'` from `hop_count`
  alone (00, 02). A count bucket only; it says nothing about venues.
* `hop_mix` — the **ordered protocol-family mix** of the legs, e.g.
  `'lb>v3'`, `'v2>v2>v3'`, or `'unknown'`. Defined once in `00` (see
  "Protocol-family hop mix" below); `01.hop_mix_distribution` and
  `03.hop_mix_distribution` histogram it unmodified. `ordered_families`
  (00, 02) is the per-leg family list (`;`-joined, `unknown` for an unmapped
  leg). Venue labels stay available as `ordered_pools`/`ordered_projects`
  (00, 02) and `route_distribution` (03).
* `effective_tip_per_gas` — see `00_qualified_arbs.sql`'s header comment for
  the exact type-aware formula and the live verification of the
  `priority_fee_per_gas == gas_price - base_fee_per_gas` identity for
  `DynamicFee`/`EIP-7702` transactions.
* `tx_index` — `mantle.transactions.index`, the transaction's zero-based
  position in the **whole block**. This pack never claims `tx_index` proves
  a won race, or that a paid tip proves sequencer policy (see WHI-545,
  explicitly out of scope here).

## Protocol-family hop mix

`hop_mix` was a hop-count bucket until the 0.2.2 release review, which found
that it did not implement the agreed protocol-family mapping (two-leg V2/V2
and V3/LB routes both read `2-hop`). It now is the ordered family mix below;
`hop_count_bucket` keeps the old count bucket as a separate field. Population
counts and qualification are unchanged (the mapping only adds `LEFT JOIN`s on
unique keys to `00`'s swap legs).

**Rule (in `00` only).** Each `dex.trades` leg's family comes from its pool:
`project_contract_address` → factory (`pool_factory`) → family
(`family_factory`). Legs are ordered by `evt_index`; `hop_mix` joins the
per-leg families with `>` without collapsing repeats. `hop_mix = 'unknown'`
when any leg's pool is not in `pool_factory`, or when `evt_index` is null or
duplicated in the tx. It is never guessed from `project` or `hop_count`.

**Families and their sources.** `family_factory` has one row per factory. The
family is the `family` column of `evidence/venues/MATRIX.md` (on-chain
accessor probes): `univ2_cpmm` → `v2`, `univ3_cl` → `v3`, `moe_lb` → `lb`,
`algebra_cl` → `algebra`.

| factory | family | venue | also in |
| --- | --- | --- | --- |
| `0x5bef015c…bedec` | v2 | Merchant Moe V1 classic | `src/service/v2_venues.rs` `MOE_V1` |
| `0xe5020961…cce7c` | v2 | FusionX V2 | `src/service/config.rs` `INTERIM_V2_FACTORY`, `v2_venues.rs` `FUSIONX_V2` |
| `0x5c84e5d2…6fd2f` | v2 | MantleSwap V2 (classified, not admitted) | `evidence/venues/mantleswap-v2/` |
| `0x25780dc8…b2035` | v3 | Agni V3 | `src/service/v3_venues.rs` `AGNI_V3` |
| `0x530d2766…9ad71` | v3 | FusionX V3 | `v3_venues.rs` `FUSIONX_V3` |
| `0x0d922fb1…74df9` | v3 | Uniswap V3 | `v3_venues.rs` `UNISWAP_V3_MANTLE` |
| `0xeeca0a86…c2644` | v3 | Butter | `v3_venues.rs` `BUTTER` |
| `0xf883162e…9b737c` | v3 | Fluxion V3 | `v3_venues.rs` `FLUXION_V3` |
| `0x636ea278…4ee3da0` | v3 | V3 fork `0x636ea2` | `v3_venues.rs` `V3FORK_636EA2` |
| `0xaaa32926…320c42` | v3 | Cleopatra CL (quarantined for the bot) | `v3_venues.rs` `CLEOPATRA_CL` |
| `0xa6630671…104054` | lb | Merchant Moe Liquidity Book | `src/amms/moe/pool_list.rs` `CANONICAL_MOE_FACTORY` |
| `0xc848bc59…553913` | algebra | Algebra-class `0xc848bc` | `evidence/venues/algebra-c848/` |

A family is an AMM-math class, not venue admission: the bot does not quote
MantleSwap V2, Cleopatra CL or the Algebra factory, but their family is known.

**Pools.** `pool_factory` (320 pools: 192 lb, 94 v3, 33 v2, 1 algebra) is
generated from committed pool evidence whose factory was resolved on chain:
`data/pool_universe.csv` and the committed `pool_universe*.csv` snapshots
under `evidence/`, `data/poolLists_moe.csv` (Moe LB factory enumeration), and
`evidence/venues/whi-1413/denominator/census_44.json`. No pool has
conflicting factory evidence. A pool outside this set maps to `unknown`, so
coverage is bounded by the repo's pool evidence and does not grow with new
pools until the table is regenerated. `evidence/dunesql/whi-1545/hop_mix_mirror.py
--check` verifies the family rows against `MATRIX.md`, key uniqueness, and
that the pool block equals a fresh regeneration.

**Moe V1 classic vs Moe LB** is decided only by the pool's factory
(`0x5bef…` vs `0xa663…`), never by `project = 'merchant_moe'`.

**Offline verification (not a Dune execution).** `hop_mix_mirror.py --mirror`
mirrors the SQL logic in Python over the frozen 2026-09-28 export of `02`
(252 rows, sha256 `11ed6d19…6cfe`; external, pinned in
`evidence/peer-attribution/whi-1412/manifest.json`). Output:
`evidence/dunesql/whi-1545/mirror_sept28.json`. In it:

* the base `hop_mix` is `2-hop` for all 115 two-leg txs;
* the new `hop_mix` has 7 distinct two-leg mixes, e.g. `v3>v3`
  (`0x87d95c63…`, agni;fusionx), `lb>v3` (`0xa4f87b88…`, merchant_moe;agni),
  `v2>v3` (`0x9e1d0bc0…`, merchant_moe;fusionx), `v3>v2`, `v3>lb`, `lb>lb`,
  `v2>v2`;
* the same project sequence gets different families. `merchant_moe;merchant_moe`
  is `lb>lb` (`0x48f9322d…`) and `v2>v2` (`0x6eee6497…`).
  `merchant_moe;merchant_moe;merchant_moe` is `lb>lb>lb` and `v2>v2>v2`
  (`0xe25f876d…`). That last route is the Moe V1 classic `h3:v2+v2+v2` route
  that `evidence/peer-attribution/whi-1412/STATUS.md` identifies independently;
* unmappable example: `0x82523d55…` (merchant_moe;agni), where the agni pool
  `0x928981fe…` is not in the pool evidence, gives `ordered_families =
  lb;unknown` and `hop_mix = unknown`;
* 228 txs mapped, 24 `unknown` (20 distinct unmapped pools). Every mapped leg
  agrees with its Dune project: agni → agni_v3, fusionx → fusionx_v3,
  uniswap → uniswap_v3, merchant_moe → moe_lb (343 legs) or moe_v1_classic
  (55 legs).

The frozen export carries legs already in `evt_index` order but not
`evt_index` itself. That means the mirror does not exercise the
null/duplicate-`evt_index` guard.
`evidence/dunesql/whi-1545/sql_logic_check.py` covers the guard: it runs the
actual `00` CTE text on mocked `dex.trades` rows in local DuckDB (still not
Dune).

**Publication status.** The saved queries 8781215 (`00`) and 8781229 (`02`)
still run the earlier SQL. The pack has not been republished. Republishing
needs explicit owner authorization, and until then it is **pending**.
`01`/`03` SQL is unchanged. They read `00` through "Query a Query", so they
pick up the new `hop_mix` once `00` is republished. The row counts under "One
successful execution window" predate this change. Qualification is
unchanged, so they are expected to stay the same. Re-verifying them is part of
the post-publish validation.

## `03`'s distribution/percentile method

The canonical statement of this method lives in `03_bot_strategy_profile.sql`'s
own header comment (kept next to the SQL that implements it, so the two
cannot drift independently); this section is a summary for readers who start
from the README instead of the SQL file.

* **Denominator for every `*_distribution` map:** `arb_tx_count` for that
  row's address (identical definition/value to `01.arb_tx_count`: `COUNT(
  DISTINCT tx_hash)`). Each map value is `row(count, pct)` where
  `pct = count / arb_tx_count`.
* **`route_distribution` counts transactions, not swap legs** — a 3-hop tx
  contributes 1 to its `';'`-joined route bucket (e.g. `"agni;fusionx;agni"`)
  , not 3.
* **`settlement_asset_distribution` is keyed by the settlement token address
  with the symbol appended for readability** (e.g.
  `"0x78c1b0c9...b8 (WMNT)"`), not by symbol alone — symbols are not
  guaranteed unique across bridged/wrapped variants, so keying by address is
  the correct identity; the symbol suffix is cosmetic.
* **Top-category limit: none.** `route_distribution` and the other
  `*_distribution` maps are the **full** per-address distribution, not
  truncated to a top-N. The highest-count key in a map is that address's
  "top" category; sort/limit client-side if a top-N view is wanted. This
  mirrors `02`'s "no hidden top-N truncation" principle rather than
  contradicting it.
* **Percentile method:** Trino `approx_percentile` (p10/p50/p90) over
  `TRY_CAST(... AS double)`. Each of `gas_price`, `effective_tip_per_gas`,
  `gas_used`, `tx_index` has its own `*_sample_count` column — the exact
  non-null denominator for that metric, which can be less than
  `arb_tx_count` (e.g. `effective_tip_per_gas` is null for OP-stack
  deposit-type txs, though such txs are extremely unlikely to also be
  qualified arbs). Small per-address sample counts are reported as-is,
  never hidden or reinterpreted as a stronger claim than the sample
  supports.
* **Join shape:** `03` `LEFT JOIN`s `01`'s address set (not an `INNER JOIN`),
  so `03`'s address set is structurally guaranteed to equal `01`'s rather
  than merely observed to match — verified empirically equal (27/27/27,
  zero mismatches) for the JP probe window in one execution, but the `LEFT
  JOIN` makes that a property of the query, not a coincidence of one run.

## Collector compatibility (WHI-955 export path)

`02_arb_detail_feed` output is collector-compatible: `0x`-prefixed
hashes/addresses, `;`-joined ordered pools, and a SQL-qualified
`settlement_asset` (no offline enrichment needed, unlike the retired
`dune_atomic_arbs.sql`). Verified end-to-end against the JP probe window
(`[100872173, 100876184]`, standing in for a frozen WHI-955-style interval —
**the real WHI-955 interval is not yet available**: it must be derived from
that ledger's own min/max block once WHI-955 produces it, and documented in
that issue's own evidence, not backfilled here with the approximate JP
start):

```bash
# 1) Run 02_arb_detail_feed.sql with start_time/end_time set to a safe
#    superset of the frozen ledger's block range, and from_block/to_block
#    pinned to the ledger's exact [min_block, max_block].
# 2) Export CSV. Rename the `executor_address` column to `to` (or `executor`)
#    so the collector's alias matching picks it up — see
#    load_candidates_dune_csv in src/service/ground_truth.rs.
# 3) Run the collector:
cargo run --release --bin ground_truth_collector -- collect \
  --input <external>/dune_export.csv \
  --from-block 100872173 --to-block 100876184 \
  --known-bots-out <external>/known_bots.json \
  --events-out <external>/events.jsonl \
  --report-out <external>/report.json \
  --md-out <external>/report.md
```

**What was and was not executed (reconciliation with the original
acceptance wording).** The acceptance named the export for the frozen
requalification ledger's exact interval.

* **Not executed:** that literal interval was never exported by this pack. The
  JP probe window above stood in for it.
* **Executed later:** a real concurrent-window export: `02` (query 8781229,
  v4, execution `01M3NGSE7KAE0R0YM8B5HPQNKH`) for UTC day 2026-09-28, 252
  rows. It was reconciled through `ground_truth_collector`: 252/252 accepted,
  0 excluded (`evidence/peer-attribution/whi-1412/ground_truth_report.json`,
  input hashes in its `manifest.json`).
* **What this proves:** collector compatibility at a real concurrent window.
  It is not a historical export of the originally named interval, and that
  gap stays open.

Result on the JP window (post round-1 fix): **66/66 candidates accepted, 0
exclusions** (`00` already qualifies/excludes upstream, so the collector's
own structural re-check is a no-op reconciliation, not a second independent
filter). Re-running the identical frozen CSV export twice produced the
byte-identical `events.jsonl` and the same `events_fingerprint`
(`0x2a581cf47633253472590c228b86e931e541f9176a5eacd174a2d1129a8d6c72`) —
frozen-export reproducibility is exact, as required.

The CSV loader still synthesizes `pos=[[settlement_asset, "1"]]` and
`neg=[]` from the `settlement_asset` column alone (see
`src/service/ground_truth.rs`) — it does not re-derive token nets from raw
transfers. That is expected and fine here: **qualification (including the
real token-net computation) already happened in `00`**, before the CSV ever
reaches the collector. The collector's role for a Dune-sourced export is
schema translation + an idempotent reconciliation pass, not re-qualification.

**Known interaction, deliberately not fixed here (see `docs/DEFERRED_ISSUES.md`
DI-37 for the precise accepted token list):** the pre-existing collector CSV
loader's `is_sandwich` boolean parser only recognizes a fixed set of
true/false tokens (case-insensitive); `00`'s honest `'unknown'` string does
not match any of them and defaults to `false` (not excluded) inside the
existing, out-of-scope `ground_truth.rs` code. This means a collector run
against a Dune export from a window where `is_sandwich` is genuinely
`'unknown'` (e.g. this pack's default 3-month window, per the
sandwich-coverage note above) will not exclude any tx on sandwich grounds,
even though the underlying data genuinely doesn't support a `false`
determination either. Changing the collector's Rust default is out of scope
for this issue (no Rust crawler/collector changes) and is recorded as
deferred debt rather than silently relied upon.

## Retirement of `scripts/ground_truth/dune_atomic_arbs.sql`

That script left `settlement_asset` empty for offline enrichment and relied
on a hand-maintained swap-topic list across DEX families. `00_qualified_arbs`
supersedes it completely: curated `dex.trades` instead of hand-decoded swap
topics, and real SQL-computed `settlement_asset` via the token-net join
(never empty for a qualified row) instead of an offline join step. The old
file has been removed; `evidence/ground-truth/README.md` points here.

## Out of scope (unchanged from the Linear issue)

Rust/on-chain crawler work, operator clustering or sender/executor identity
merging beyond the `{tx.from, tx.to}` qualification entity, USD/profit
estimation from activity counts, the WHI-957 universe join, and
alerts/scheduled ingest as a service are all explicitly out of scope for this
pack. `02`'s columns intentionally go slightly beyond the ticket's literal
list (`settlement_symbol`, `is_flash_loan`, `is_jit_lp`, `is_sandwich`,
`priority_fee_per_gas`) because these are core per-tx attributes `00` already
computes as part of qualification and are directly relevant to a competitor
*detail* feed; none of them touch a stated out-of-scope item (no USD/profit,
no clustering, no race-outcome claim). This is a deliberate, documented
addition, not silent scope creep.
