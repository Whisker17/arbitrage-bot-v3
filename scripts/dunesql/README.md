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
  from `mantle.transactions.index` instead).
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
* `hop_mix` — bucketed category of `hop_count`: `'2-hop'` / `'3-hop'` /
  `'>3-hop'`, or `'unknown'` when any leg's `project` is null/empty
  (protocol mapping unavailable for that leg). This is a deliberate reading
  of the spec's "`hop_count` + `hop_mix` category (2/3/>3, ...)" language as
  a count-bucket, distinct from — and not a substitute for — actual venue
  mix. **Venue mix itself is carried separately**, unmodified from `00`, as
  `ordered_pools`/`ordered_projects` (00, 02) and as `route_distribution`
  (03, built directly from `ordered_projects`). `hop_count`/`hop_mix` alone
  never stand in for that venue-level information; they are always
  accompanied by it.
* `effective_tip_per_gas` — see `00_qualified_arbs.sql`'s header comment for
  the exact type-aware formula and the live verification of the
  `priority_fee_per_gas == gas_price - base_fee_per_gas` identity for
  `DynamicFee`/`EIP-7702` transactions.
* `tx_index` — `mantle.transactions.index`, the transaction's zero-based
  position in the **whole block**. This pack never claims `tx_index` proves
  a won race, or that a paid tip proves sequencer policy (see WHI-545,
  explicitly out of scope here).

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
