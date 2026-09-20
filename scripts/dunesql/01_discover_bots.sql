-- WHI-1406 · 01_discover_bots.sql — ask #1
--
-- "Which sender addresses have qualified arb activity and how many?"
--
-- One row per distinct qualified `tx.from` ("bot_address") for the window —
-- no minimum-activity threshold; single-transaction addresses are included.
-- Reads 00_qualified_arbs (Dune query id 8781215) via "Query a Query"; there
-- is no separate qualification logic here.
--
-- Saved on Dune as query id 8781227 (public):
--   https://dune.com/queries/8781227
-- Dashboard: https://dune.com/mantlexyz/whi-1406-mantle-competitor-monitoring-pack
--
-- Parameters: same start_time/end_time/from_block/to_block as 00 (see that
-- file's header). Default window: trailing 3 calendar months, fixed UTC
-- bounds `2026-06-01 00:00:00` – `2026-09-01 00:00:00`.
--
-- arb_tx_count is a DISTINCT qualified-transaction count (COUNT(DISTINCT
-- tx_hash)) — never a swap-row or transfer-row count. Because 00 is keyed by
-- tx_hash (one row per qualified tx), sum(arb_tx_count) across all rows here
-- always equals the total row count of 00 for the same parameters, and
-- equals the (unfiltered) row count of 02 for the same parameters — no join
-- in this pack ever multiplies transaction rows.
--
-- hop_count_distribution / hop_mix_distribution are raw Trino histogram()
-- maps (value -> count) over 00's hop_count / hop_mix columns, reused
-- unmodified from 00 (no re-derivation).
--
-- Sort: arb_tx_count DESC, bot_address ASC.

SELECT
  bot_address,
  COUNT(DISTINCT tx_hash) AS arb_tx_count,
  MIN(block_time) AS first_seen_time,
  MIN(block_number) AS first_seen_block,
  MAX(block_time) AS last_seen_time,
  MAX(block_number) AS last_seen_block,
  histogram(hop_count) AS hop_count_distribution,
  histogram(hop_mix) AS hop_mix_distribution,
  arbitrary(resolved_start_time) AS resolved_start_time,
  arbitrary(resolved_end_time) AS resolved_end_time,
  arbitrary(resolved_from_block) AS resolved_from_block,
  arbitrary(resolved_to_block) AS resolved_to_block
FROM "query_8781215(start_time='{{start_time}}', end_time='{{end_time}}', from_block='{{from_block}}', to_block='{{to_block}}')"
GROUP BY bot_address
ORDER BY arb_tx_count DESC, bot_address ASC
