-- WHI-956 · Mantle atomic-arb candidate discovery (Dune)
--
-- Run on Dune against Mantle decoded activity. Export the result as CSV and
-- feed it to:
--
--   cargo run --release --bin ground_truth_collector -- collect \
--     --input /path/to/export.csv \
--     --from-block {{from_block}} --to-block {{to_block}} \
--     --known-bots-out /tmp/known_bots.json \
--     --events-out /tmp/events.jsonl \
--     --report-out evidence/ground-truth/report.json
--
-- Parameterise the block window at the bottom. The collector re-applies the
-- acceptance heuristic and misclassification exclusions offline so this query
-- is allowed to be a *superset* of true arbs.
--
-- Expected CSV columns (extras ignored; see load_candidates_dune_csv):
--   block_number, tx_hash, bot_address, to, ordered_pools, n_swaps, kinds,
--   msg_value_wei, has_liquidation, has_jit_lp, is_sandwich, is_flash_loan,
--   settlement_asset, route, label, selector
--
-- Notes on Mantle table names: Dune's Mantle curated schema evolves. Prefer
-- the project's current `mantle.*` decoded tables; if a table is missing in
-- your workspace, substitute the equivalent decoded-logs view and keep the
-- SELECT column aliases stable.
--
-- Closed-cycle / settlement: the offline collector REQUIRES a non-empty
-- `settlement_asset` or `pos` leg. This query leaves settlement_asset empty
-- by default — join token-transfer nets (entity = from∪to, net>0 in ≥1 token
-- and net<0 in none) before export, or the collector will exclude every row as
-- `not_closed_cycle`.

WITH params AS (
  SELECT
    CAST({{from_block}} AS bigint) AS from_block,
    CAST({{to_block}}   AS bigint) AS to_block
),

-- Swap event topics across Mantle DEX families (same set as the offline extract).
swap_topics AS (
  SELECT topic FROM (VALUES
    (0xd78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822), -- UniV2-style
    (0xc42079f94a6350d7e6235f29174924f928cc2ac818eb64fed8004e115fbcca67), -- UniV3-style
    (0x19b47279256b2a23a1665c810c8d55a1758940ee09377d4f8d26497a3577dc83), -- Algebra / Agni
    (0xad7d6f97abf51ce18e17a38f4d70e975be9c0708474987bb3e26ad21bd93ca70), -- Moe LB
    (0x0fe977d619f8172f7fdbe8bb8928ef80952817d96936509f67d66346bc4cd10f), -- iZi
    (0xb3e2773606abfd36b5bd91394b3a54d1398336c65005baf7bf7a05efeffaf75b)  -- Solidly
  ) AS t(topic)
),

-- Liquidation-call topic used by the offline exclusion guard.
liq_topics AS (
  SELECT topic FROM (VALUES
    (0xe413a321e8681d831f4dbccbca790d2952b56f977908e45be37335533e005286)
  ) AS t(topic)
),

swap_logs AS (
  SELECT
    l.block_number,
    l.tx_hash,
    l.contract_address AS pool,
    l.topic0,
    l.index AS log_index
  FROM mantle.logs l
  CROSS JOIN params p
  WHERE l.block_number BETWEEN p.from_block AND p.to_block
    AND l.topic0 IN (SELECT topic FROM swap_topics)
),

tx_swaps AS (
  SELECT
    block_number,
    tx_hash,
    COUNT(*) AS n_swaps,
    array_agg(pool ORDER BY log_index) AS ordered_pools
  FROM swap_logs
  GROUP BY 1, 2
  HAVING COUNT(*) >= 2
),

tx_meta AS (
  SELECT
    t.block_number,
    t.hash AS tx_hash,
    t."from" AS bot_address,
    t."to"   AS to_address,
    t.value  AS msg_value_wei,
    substr(t.data, 1, 10) AS selector,
    t.success
  FROM mantle.transactions t
  CROSS JOIN params p
  WHERE t.block_number BETWEEN p.from_block AND p.to_block
    AND t.success = true
),

tx_liq AS (
  SELECT DISTINCT l.tx_hash
  FROM mantle.logs l
  CROSS JOIN params p
  WHERE l.block_number BETWEEN p.from_block AND p.to_block
    AND l.topic0 IN (SELECT topic FROM liq_topics)
),

-- Best-effort flash-loan markers (Aave-style FlashLoan / FlashLoanSimple topics).
tx_flash AS (
  SELECT DISTINCT l.tx_hash
  FROM mantle.logs l
  CROSS JOIN params p
  WHERE l.block_number BETWEEN p.from_block AND p.to_block
    AND l.topic0 IN (
      0xefefaba5e921573100900a3ad9cf29f222d995fb3b6045797e5794629f6fcfb,  -- FlashLoan
      0x631042c832b074fd3598b35f62b10a29396f889d4c1b7a6cbab0f6eef8a2d  -- placeholder; refine per venue
    )
)

SELECT
  s.block_number,
  lower(to_hex(s.tx_hash)) AS tx_hash,
  lower(to_hex(m.bot_address)) AS bot_address,
  lower(to_hex(m.to_address)) AS "to",
  array_join(transform(s.ordered_pools, x -> lower(to_hex(x))), ';') AS ordered_pools,
  s.n_swaps,
  '' AS kinds,                          -- filled offline from pool census when available
  CAST(m.msg_value_wei AS varchar) AS msg_value_wei,
  CASE WHEN liq.tx_hash IS NOT NULL THEN true ELSE false END AS has_liquidation,
  false AS has_jit_lp,                  -- refine with mint/burn join when needed
  false AS is_sandwich,                 -- multi-tx sandwich needs a separate pass
  CASE WHEN fl.tx_hash IS NOT NULL THEN true ELSE false END AS is_flash_loan,
  '' AS settlement_asset,               -- filled offline from token-transfer nets
  CAST(s.n_swaps AS varchar) AS route,
  lower(to_hex(m.to_address)) AS label,
  m.selector
FROM tx_swaps s
JOIN tx_meta m ON m.tx_hash = s.tx_hash AND m.block_number = s.block_number
LEFT JOIN tx_liq liq ON liq.tx_hash = s.tx_hash
LEFT JOIN tx_flash fl ON fl.tx_hash = s.tx_hash
WHERE m.msg_value_wei <= CAST(power(10, 18) AS uint256)  -- drop obvious CEX-DEX native funds
ORDER BY s.block_number, s.tx_hash;
