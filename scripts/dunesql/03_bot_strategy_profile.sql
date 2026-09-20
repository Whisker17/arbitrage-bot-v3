-- WHI-1406 · 03_bot_strategy_profile.sql — ask #3
--
-- "Observed arbitrage strategy by sender address (tx.from)."
--
-- Explicitly NOT operator identification, NOT address clustering, and NOT a
-- profitability or race-outcome claim — a purely observational per-address
-- distribution profile over the same qualified population as 01.
--
-- One row per address from 01 for the same window (every discovered address,
-- not a hand-picked watchlist). Reads 01_discover_bots (Dune query id
-- 8781227) for the per-address fields the two share (arb_tx_count,
-- first/last seen, hop_count/hop_mix distributions) — 01 is the single
-- source for those, so this file does not recompute them from 00 a second
-- time. Reads 00_qualified_arbs (Dune query id 8781215) directly only for
-- the additional per-tx facts (route/settlement/tx_type, fee/gas/tx_index
-- percentiles) that 01 does not carry. No separate qualification logic
-- anywhere in this file. LEFT JOIN FROM 01's address set (not an INNER JOIN)
-- so 03's address set is structurally guaranteed to equal 01's — verified
-- empirically equal within one execution, but LEFT JOIN makes that a
-- structural property rather than an observation. In the (unobserved, but
-- theoretically possible) case where the 00-derived side is ever missing an
-- address that 01 has, the row is still emitted with that address's
-- shared.* fields intact and every extra.*-derived column (histograms,
-- percentiles, sample counts) simply NULL — a graceful, honest "no data
-- from this side" rather than a dropped row or a query error.
--
-- Saved on Dune as query id 8781231 (public):
--   https://dune.com/queries/8781231
-- Dashboard: https://dune.com/mantlexyz/whi-1406-mantle-competitor-monitoring-pack
--
-- Parameters: same start_time/end_time/from_block/to_block as 00/01 (see
-- 00_qualified_arbs.sql's header). Default window: trailing 3 calendar
-- months, fixed UTC bounds `2026-06-01 00:00:00` – `2026-09-01 00:00:00`.
--
-- Denominator / percentile method (defined once here, applies to every
-- distribution and percentile column below — no other query in this pack
-- redefines it):
--   * Every *_distribution map's percentage = count / arb_tx_count for that
--     row's address (arb_tx_count = COUNT(DISTINCT tx_hash), identical
--     definition and value to 01.arb_tx_count for the same address/window).
--   * hop_count_distribution / hop_mix_distribution: reused verbatim from
--     01 (which reused them verbatim from 00) — one histogram entry per
--     qualified tx (denominator = arb_tx_count).
--   * route_distribution: full histogram of the ';'-joined ordered_projects
--     string per address (e.g. "agni;fusionx"). Counts TRANSACTIONS, not
--     swap legs — a 3-hop tx contributes 1 to its route bucket, not 3. This
--     is the full per-address distribution (every observed combination, not
--     a hidden top-N); the highest-count key is the "top" route/venue
--     combination for that address. Top-category limit: none.
--   * settlement_asset_distribution: keyed by the settlement token ADDRESS
--     with the symbol appended for readability (e.g.
--     "0x78c1b0c9...b8 (WMNT)") — symbols are not guaranteed unique across
--     bridged/wrapped variants, so address is the correct identity key.
--   * tx_type_distribution: same count-and-% histogram shape, denominator =
--     arb_tx_count.
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

WITH shared AS (
  SELECT *
  FROM "query_8781227(start_time='{{start_time}}', end_time='{{end_time}}', from_block='{{from_block}}', to_block='{{to_block}}')"
),
extra AS (
  SELECT
    bot_address,
    histogram(ordered_projects) AS route_hist,
    -- COALESCE is defensive only: 00's qualification guarantees settlement_asset
    -- is non-null for every qualified row (>=1 net-positive leg is required to
    -- qualify at all), so the 'unknown' branch here is unreachable in practice.
    histogram(
      COALESCE(settlement_asset, 'unknown')
      || CASE WHEN settlement_symbol IS NOT NULL THEN ' (' || settlement_symbol || ')' ELSE '' END
    ) AS settlement_asset_hist,
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
    COUNT(tx_index) AS tx_index_n
  FROM "query_8781215(start_time='{{start_time}}', end_time='{{end_time}}', from_block='{{from_block}}', to_block='{{to_block}}')"
  GROUP BY bot_address
)
SELECT
  shared.bot_address,
  shared.arb_tx_count,
  shared.first_seen_time, shared.first_seen_block, shared.last_seen_time, shared.last_seen_block,
  transform_values(shared.hop_count_distribution, (k, v) -> CAST(ROW(v, CAST(v AS DOUBLE) / shared.arb_tx_count) AS ROW(count BIGINT, pct DOUBLE))) AS hop_count_distribution,
  transform_values(shared.hop_mix_distribution, (k, v) -> CAST(ROW(v, CAST(v AS DOUBLE) / shared.arb_tx_count) AS ROW(count BIGINT, pct DOUBLE))) AS hop_mix_distribution,
  transform_values(extra.route_hist, (k, v) -> CAST(ROW(v, CAST(v AS DOUBLE) / shared.arb_tx_count) AS ROW(count BIGINT, pct DOUBLE))) AS route_distribution,
  transform_values(extra.settlement_asset_hist, (k, v) -> CAST(ROW(v, CAST(v AS DOUBLE) / shared.arb_tx_count) AS ROW(count BIGINT, pct DOUBLE))) AS settlement_asset_distribution,
  transform_values(extra.tx_type_hist, (k, v) -> CAST(ROW(v, CAST(v AS DOUBLE) / shared.arb_tx_count) AS ROW(count BIGINT, pct DOUBLE))) AS tx_type_distribution,
  extra.gas_price_p10, extra.gas_price_p50, extra.gas_price_p90, extra.gas_price_n AS gas_price_sample_count,
  extra.tip_p10, extra.tip_p50, extra.tip_p90, extra.tip_n AS effective_tip_per_gas_sample_count,
  extra.gas_used_p10, extra.gas_used_p50, extra.gas_used_p90, extra.gas_used_n AS gas_used_sample_count,
  extra.tx_index_p10, extra.tx_index_p50, extra.tx_index_p90, extra.tx_index_n AS tx_index_sample_count,
  shared.resolved_start_time, shared.resolved_end_time, shared.resolved_from_block, shared.resolved_to_block
FROM shared
LEFT JOIN extra ON extra.bot_address = shared.bot_address
ORDER BY shared.arb_tx_count DESC, shared.bot_address ASC
