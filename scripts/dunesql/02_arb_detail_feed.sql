-- WHI-1406 · 02_arb_detail_feed.sql — ask #2
--
-- "A complete per-transaction detail feed sorted by recency."
--
-- Thin projection/filter over 00_qualified_arbs (Dune query id 8781215) —
-- no separate qualification logic. One row per qualified tx; every tx in the
-- window is included by default (no watchlist, no hidden top-N truncation).
-- Optional bot_address filter (pass '' / omit to disable).
--
-- Saved on Dune as query id 8781229 (public):
--   https://dune.com/queries/8781229
-- Dashboard: https://dune.com/mantlexyz/whi-1406-mantle-competitor-monitoring-pack
--
-- Parameters: same start_time/end_time/from_block/to_block as 00 (see that
-- file's header), plus:
--   bot_address_filter   text, default ''. When non-empty, restricts to that
--                         one lowercased 0x address; '' returns every row.
--
-- Sort: block_number DESC, tx_index DESC, tx_hash ASC. tx_index is
-- mantle.transactions.index — the tx's zero-based position in the WHOLE
-- block, not a claim about racing position among competing arbitrageurs
-- (see WHI-545 for that separate, unresolved question).

SELECT
  block_time,
  block_number,
  tx_hash,
  bot_address,
  executor_address,
  hop_count,
  hop_mix,
  ordered_pools,
  ordered_projects,
  settlement_asset,
  settlement_symbol,
  gas_used,
  gas_price,
  base_fee_per_gas,
  max_fee_per_gas,
  max_priority_fee_per_gas,
  priority_fee_per_gas,
  effective_tip_per_gas,
  tx_type,
  tx_index,
  is_flash_loan,
  is_jit_lp,
  is_sandwich,
  resolved_start_time,
  resolved_end_time,
  resolved_from_block,
  resolved_to_block
FROM "query_8781215(start_time='{{start_time}}', end_time='{{end_time}}', from_block='{{from_block}}', to_block='{{to_block}}')"
WHERE ('{{bot_address_filter}}' = '' OR bot_address = lower('{{bot_address_filter}}'))
ORDER BY block_number DESC, tx_index DESC, tx_hash ASC
