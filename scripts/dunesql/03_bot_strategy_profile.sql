-- WHI-1406 · 03_bot_strategy_profile.sql — ask #3
--
-- "Observed arbitrage strategy by sender address (tx.from)."
--
-- Explicitly NOT operator identification, NOT address clustering, and NOT a
-- profitability or race-outcome claim — a purely observational per-address
-- distribution profile over the same qualified population as 01.
--
-- One row per address from 01 for the same window (every discovered address,
-- not a hand-picked watchlist). Reads 00_qualified_arbs (Dune query id
-- 8781215) via "Query a Query"; no separate qualification logic.
--
-- Saved on Dune as query id 8781231 (public):
--   https://dune.com/queries/8781231
-- Dashboard: https://dune.com/mantlexyz/whi-1406-mantle-competitor-monitoring-pack
--
-- Parameters: same start_time/end_time/from_block/to_block as 00 (see that
-- file's header). Default window: trailing 3 calendar months, fixed UTC
-- bounds `2026-06-01 00:00:00` – `2026-09-01 00:00:00`.
--
-- Denominator / percentile method (defined once here, applies to every
-- distribution and percentile column below — no other query in this pack
-- redefines it):
--   * Every *_distribution map's percentage = count / arb_tx_count for that
--     row's address (arb_tx_count = COUNT(DISTINCT tx_hash), identical
--     definition and value to 01.arb_tx_count for the same address/window).
--   * hop_count_distribution / hop_mix_distribution: one histogram entry per
--     qualified tx (denominator = arb_tx_count, same source columns as 01,
--     reused unmodified from 00).
--   * route_distribution: full histogram of the ';'-joined ordered_projects
--     string per address (e.g. "agni;fusionx"). Counts TRANSACTIONS, not
--     swap legs — a 3-hop tx contributes 1 to its route bucket, not 3. This
--     is the full per-address distribution (every observed combination, not
--     a hidden top-N); the highest-count key is the "top" route/venue
--     combination for that address.
--   * settlement_asset_distribution / tx_type_distribution: same
--     count-and-% histogram shape, denominator = arb_tx_count.
--   * gas_price / effective_tip_per_gas / gas_used / tx_index percentiles use
--     Trino approx_percentile (p10/p50/p90) over TRY_CAST(... AS double).
--     Each has its own explicit non-null sample count column
--     (*_sample_count) — the denominator for that specific metric, which can
--     be less than arb_tx_count when a value is null (e.g.
--     effective_tip_per_gas is null for OP-stack deposit-type txs). Small
--     per-address sample counts are reported as-is, never hidden or
--     silently reinterpreted as a stronger claim.
--
-- Sort: arb_tx_count DESC, bot_address ASC.

WITH base AS (
  SELECT *
  FROM "query_8781215(start_time='{{start_time}}', end_time='{{end_time}}', from_block='{{from_block}}', to_block='{{to_block}}')"
),
agg AS (
  SELECT
    bot_address,
    COUNT(DISTINCT tx_hash) AS arb_tx_count,
    MIN(block_time) AS first_seen_time,
    MIN(block_number) AS first_seen_block,
    MAX(block_time) AS last_seen_time,
    MAX(block_number) AS last_seen_block,
    histogram(hop_count) AS hop_count_hist,
    histogram(hop_mix) AS hop_mix_hist,
    histogram(ordered_projects) AS route_hist,
    histogram(COALESCE(settlement_symbol, 'unknown')) AS settlement_asset_hist,
    histogram(tx_type) AS tx_type_hist,
    approx_percentile(TRY_CAST(gas_price AS double), 0.1) AS gas_price_p10,
    approx_percentile(TRY_CAST(gas_price AS double), 0.5) AS gas_price_p50,
    approx_percentile(TRY_CAST(gas_price AS double), 0.9) AS gas_price_p90,
    COUNT(TRY_CAST(gas_price AS double)) AS gas_price_n,
    approx_percentile(TRY_CAST(effective_tip_per_gas AS double), 0.1) AS tip_p10,
    approx_percentile(TRY_CAST(effective_tip_per_gas AS double), 0.5) AS tip_p50,
    approx_percentile(TRY_CAST(effective_tip_per_gas AS double), 0.9) AS tip_p90,
    COUNT(TRY_CAST(effective_tip_per_gas AS double)) AS tip_n,
    approx_percentile(CAST(gas_used AS double), 0.1) AS gas_used_p10,
    approx_percentile(CAST(gas_used AS double), 0.5) AS gas_used_p50,
    approx_percentile(CAST(gas_used AS double), 0.9) AS gas_used_p90,
    COUNT(gas_used) AS gas_used_n,
    approx_percentile(CAST(tx_index AS double), 0.1) AS tx_index_p10,
    approx_percentile(CAST(tx_index AS double), 0.5) AS tx_index_p50,
    approx_percentile(CAST(tx_index AS double), 0.9) AS tx_index_p90,
    COUNT(tx_index) AS tx_index_n,
    arbitrary(resolved_start_time) AS resolved_start_time,
    arbitrary(resolved_end_time) AS resolved_end_time,
    arbitrary(resolved_from_block) AS resolved_from_block,
    arbitrary(resolved_to_block) AS resolved_to_block
  FROM base
  GROUP BY bot_address
)
SELECT
  bot_address,
  arb_tx_count,
  first_seen_time, first_seen_block, last_seen_time, last_seen_block,
  transform_values(hop_count_hist, (k, v) -> CAST(ROW(v, CAST(v AS DOUBLE) / arb_tx_count) AS ROW(count BIGINT, pct DOUBLE))) AS hop_count_distribution,
  transform_values(hop_mix_hist, (k, v) -> CAST(ROW(v, CAST(v AS DOUBLE) / arb_tx_count) AS ROW(count BIGINT, pct DOUBLE))) AS hop_mix_distribution,
  transform_values(route_hist, (k, v) -> CAST(ROW(v, CAST(v AS DOUBLE) / arb_tx_count) AS ROW(count BIGINT, pct DOUBLE))) AS route_distribution,
  transform_values(settlement_asset_hist, (k, v) -> CAST(ROW(v, CAST(v AS DOUBLE) / arb_tx_count) AS ROW(count BIGINT, pct DOUBLE))) AS settlement_asset_distribution,
  transform_values(tx_type_hist, (k, v) -> CAST(ROW(v, CAST(v AS DOUBLE) / arb_tx_count) AS ROW(count BIGINT, pct DOUBLE))) AS tx_type_distribution,
  gas_price_p10, gas_price_p50, gas_price_p90, gas_price_n AS gas_price_sample_count,
  tip_p10, tip_p50, tip_p90, tip_n AS effective_tip_per_gas_sample_count,
  gas_used_p10, gas_used_p50, gas_used_p90, gas_used_n AS gas_used_sample_count,
  tx_index_p10, tx_index_p50, tx_index_p90, tx_index_n AS tx_index_sample_count,
  resolved_start_time, resolved_end_time, resolved_from_block, resolved_to_block
FROM agg
ORDER BY arb_tx_count DESC, bot_address ASC
