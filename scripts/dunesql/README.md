# Mantle competitor-monitoring pack (WHI-1406)

Four DuneSQL files, one shared qualification backbone plus three public
deliverables mapped 1:1 to the operator's asks. **Not** a Rust crawler, not a
WHI-957 clone, not an operator-identity/clustering tool — see
`docs/DEFERRED_ISSUES.md`-adjacent scope notes below and the Linear issue.

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
to_block='...')"`). There is exactly one copy of the qualification logic in
this pack.

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

## Tables used and why

* **`dex.trades`** (curated Dune spell, `blockchain='mantle'`) — already
  decodes swaps per DEX. We never hand-decode swap topics. Observed `project`
  values on Mantle: `agni`, `fusionx`, `merchant_moe`, `uniswap`, `clipper`,
  `carbon_defi`, `swaap`, `tropicalswap`. No `tx_index` column (that comes
  from `mantle.transactions.index` instead).
* **`tokens.transfers`** (curated Dune spell, `blockchain='mantle'`) — used
  for the entity-net qualification (see below).
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
| `00_qualified_arbs` | **21,920** qualified transactions (block range 96,071,749–100,044,538) |
| `01_discover_bots` | **104** distinct qualified sender addresses; every `arb_tx_count` sums to the `00` row count |
| `02_arb_detail_feed` (unfiltered) | one row per `00` row (same population, thin projection) |
| `03_bot_strategy_profile` | **104** rows (same address set as `01`) |

Execution cost was cheap (<1 Dune credit for `00`'s full-window pass; low
single-digit credits for `01`/`02`/`03` via query-a-query) — well within the
enterprise credit budget.

**Honesty note on reproducibility:** re-running `00` for the identical frozen
window minutes apart produced counts of 21,920 / 21,921 / 21,916 across three
back-to-back executions (<0.03% variance). Mantle's curated `dex.trades` /
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
`[100872173, 100876184]` (68 qualified rows, 29 bots):

* Tx `0xf2c275ca888778661457d6a738cf1b14024014459695d6880651780a7b3f881c`:
  `00` reports `hop_count=3` (merchant_moe → uniswap → agni),
  `settlement_asset` = mETH, `effective_tip_per_gas=115000330000`. Recomputed
  directly from `dex.trades` (3 legs, same projects/order),
  `mantle.transactions`/`mantle.blocks` (`priority_fee_per_gas=115000330000`
  exactly), and `tokens.transfers` (mETH net `+0.0000228628...`, every other
  touched token net `0` — WETH, MOE, WMNT) — all fields match exactly.

Limitations: this is a structural/aggregate cross-check (raw-table
recomputation), not a re-simulation of trade profitability. Full raw event
dumps stay external per repo convention; only this aggregate note and the
committed SQL are checked in.

## Collector compatibility (WHI-955 export path)

`02_arb_detail_feed` output is collector-compatible: `0x`-prefixed
hashes/addresses, `;`-joined ordered pools, and a SQL-qualified
`settlement_asset` (no offline enrichment needed, unlike the retired
`dune_atomic_arbs.sql`). Verified end-to-end against the JP probe window
(`[100872173, 100876184]`, standing in for a frozen WHI-955-style interval —
the real WHI-955 interval must be derived from that ledger's own min/max
block, not this approximate JP start):

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

Result on the JP window: **68/68 candidates accepted, 0 exclusions**
(`00` already qualifies/excludes upstream, so the collector's own structural
re-check is a no-op reconciliation, not a second independent filter).
Re-running the identical frozen CSV export twice produced the byte-identical
`events.jsonl` and the same `events_fingerprint`
(`0x6242dea0e42c09966bfad17ae1e5bf4d70187e5e8b4b525540ebf190ad2d4782`) —
frozen-export reproducibility is exact, as required.

The CSV loader still synthesizes `pos=[[settlement_asset, "1"]]` and
`neg=[]` from the `settlement_asset` column alone (see
`src/service/ground_truth.rs`) — it does not re-derive token nets from raw
transfers. That is expected and fine here: **qualification (including the
real token-net computation) already happened in `00`**, before the CSV ever
reaches the collector. The collector's role for a Dune-sourced export is
schema translation + an idempotent reconciliation pass, not re-qualification.

`is_sandwich='unknown'` (our honest default for the default window) is not a
recognized boolean token for the collector's `is_sandwich` CSV column, so it
parses as `None` → defaults to `false` (not excluded) in the existing
collector code. This out-of-scope interaction is disclosed here rather than
silently relied upon; changing collector defaulting behavior is out of scope
for this issue (no Rust changes were made).

## Retirement of `scripts/ground_truth/dune_atomic_arbs.sql`

That script left `settlement_asset` empty for offline enrichment and relied
on a hand-maintained swap-topic list across DEX families. `00_qualified_arbs`
supersedes it completely: curated `dex.trades` instead of hand-decoded swap
topics, and real SQL-computed `settlement_asset` via the token-net join
(never empty for a qualified row) instead of an offline join step. The old
file has been removed; `evidence/ground-truth/README.md` points here.

## Definitions carried unmodified from `00` into every downstream query

* `hop_count` — raw integer count of `dex.trades` legs for the tx (no dedup,
  ordered by `evt_index`).
* `hop_mix` — bucketed category of `hop_count`: `'2-hop'` / `'3-hop'` /
  `'>3-hop'`, or `'unknown'` when any leg's `project` is null/empty
  (protocol mapping unavailable for that leg). `hop_count` alone never
  stands in for `hop_mix` downstream — both are always carried together.
* `effective_tip_per_gas` — see `00_qualified_arbs.sql`'s header comment for
  the exact type-aware formula and the live verification of the
  `priority_fee_per_gas == gas_price - base_fee_per_gas` identity for
  `DynamicFee`/`EIP-7702` transactions.
* `tx_index` — `mantle.transactions.index`, the transaction's zero-based
  position in the **whole block**. This pack never claims `tx_index` proves
  a won race, or that a paid tip proves sequencer policy (see WHI-545,
  explicitly out of scope here).

## Out of scope (unchanged from the Linear issue)

Rust/on-chain crawler work, operator clustering or sender/executor identity
merging beyond the `{tx.from, tx.to}` qualification entity, USD/profit
estimation from activity counts, the WHI-957 universe join, and
alerts/scheduled ingest as a service are all explicitly out of scope for this
pack.
