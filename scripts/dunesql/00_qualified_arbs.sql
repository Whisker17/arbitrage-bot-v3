-- WHI-1406 · 00_qualified_arbs.sql
--
-- Shared qualification backbone for the Mantle competitor-monitoring pack.
-- NOT itself a deliverable — 01/02/03 all read this query's saved output via
-- Dune's "Query a Query" feature (`FROM "query_<id>(param='value', ...)"`).
-- There is no second copy of this qualification logic anywhere in the pack.
--
-- Saved on Dune as query id 8781215 (public):
--   https://dune.com/queries/8781215
-- Dashboard: https://dune.com/mantlexyz/whi-1406-mantle-competitor-monitoring-pack
--
-- Parameters (identical names/semantics across 00/01/02/03):
--   start_time, end_time   text, "YYYY-MM-DD HH:MM:SS" (UTC, half-open [start,end)).
--                          Always required — Dune partition-prunes dex.trades /
--                          tokens.transfers by block_time, so a real time bound
--                          must be present even in block-interval mode below.
--   from_block, to_block   number. Default -1/-1 = "use the time window only".
--                          Set both >=0 to additionally pin an exact block
--                          interval (e.g. a frozen WHI-955 ledger window);
--                          start_time/end_time must still be set to a safe
--                          superset for partition pruning.
--
-- Default window (trailing 3 calendar months, fixed UTC bounds, reproducible):
--   start_time = '2026-06-01 00:00:00', end_time = '2026-09-01 00:00:00'
--   from_block = -1, to_block = -1
--   Verified continuous (no daily gaps) in dex.trades / tokens.transfers for
--   this exact window at implementation time — see scripts/dunesql/README.md.
--
-- One row per qualified tx, keyed by tx_hash. A tx qualifies when:
--   * success = true (mantle.transactions).
--   * >= 2 swap-legs in the curated `dex.trades` table for
--     blockchain='mantle' (no hand-decoded swap topics; ordered by evt_index,
--     no dedup) -> hop_count (raw leg count) + hop_mix (bucketed category:
--     '2-hop' / '3-hop' / '>3-hop', or 'unknown' when any leg's `project` is
--     null/empty, i.e. protocol mapping is unavailable for that leg).
--   * msg.value (mantle.transactions.value) <= 1e18 wei (drops native
--     CEX-DEX-style directional settles).
--   * No detected liquidation: tx_hash does not appear in the LiquidationCall
--     events of the three Aave-fork lending markets with verified Mantle
--     coverage (aave_v3_mantle, lendle_mantle, aurelius_finance_mantle).
--   * Closed-cycle settlement: entity = {tx.from, tx.to} together (one bot,
--     counted once by tx.from — tx.to is often a shared router/executor with
--     zero legs of its own). Token nets computed from `tokens.transfers`
--     across that entity boundary using exact `amount_raw` (DECIMAL(38,0))
--     arithmetic; internal tx.from<->tx.to transfers cancel to a literal
--     exact 0 in the net sum (no separate dedup needed, no floating-point
--     dust risk). Qualifies only if: zero tokens net-negative, >=1 token
--     net-positive, and EXTERNAL gross outflow > 0 (entity actually sent
--     tokens to a non-entity recipient — kills both pure receive dust and
--     entity-internal-only shuffling). `settlement_asset` is the
--     net-positive token with the largest decimal-adjusted amount (tie-break:
--     token address ascending) — see the `entity_transfers` CTE below for
--     why the tie-break intentionally uses a different unit than the sign
--     check.
--
-- Execution metrics carried through unmodified by 01/02/03:
--   block_time, block_number, tx_type, gas_used, gas_price (wei/gas),
--   base_fee_per_gas, max_fee_per_gas, max_priority_fee_per_gas,
--   priority_fee_per_gas, tx_index (mantle.transactions.index — the tx's
--   zero-based position in the whole block, NOT a claim about racing
--   position among competing arbitrageurs; see 02 for the sort that uses it).
--
-- effective_tip_per_gas (type-aware, never invents a zero):
--   COALESCE(priority_fee_per_gas, gas_price - base_fee_per_gas when
--   gas_price >= base_fee_per_gas, else NULL).
--   Verified on live samples: for type='DynamicFee' (and 'EIP-7702', which
--   behaves the same way on Mantle), priority_fee_per_gas already equals
--   gas_price - base_fee_per_gas exactly, so the first branch is a no-op
--   identity check, not an assumption. For type='Legacy' (and 'AccessList'),
--   priority_fee_per_gas is null and the second branch computes the tip
--   directly. For the OP-stack deposit type ('126'), gas_price=0 <
--   base_fee_per_gas, so both branches fail closed to NULL — deposit txs
--   never get a fabricated $0 tip; NULL is honest ("this tx paid no gas").
--
-- JIT LP / sandwich / flash-loan (checked, not assumed — never `false` when
-- unverifiable):
--   * is_sandwich: 'unknown' unless `dex.sandwiches` / `dex.sandwiched` have
--     ANY row for blockchain='mantle' inside [start_time, end_time) (coverage
--     check re-run per invocation). At implementation time these tables have
--     real Mantle rows from 2024-03-27 to 2026-03-31 but ZERO rows in the
--     trailing-3-month default window above — so the default export is
--     'unknown' for every row, honestly, not silently 'false'. Use an older
--     window to get real true/false coverage from these tables.
--   * is_jit_lp: always 'unknown'. No verified Dune JIT-LP marker was found
--     for Mantle at implementation time (dex.sandwiches/sandwiched only cover
--     sandwich attacks, not JIT liquidity). Never labeled `false`.
--   * is_flash_loan: true only when tx_hash appears in the FlashLoan events
--     of the same three lending markets used for liquidation detection
--     (verified live on Mantle). `false` here means "no flash-loan marker
--     detected from these three protocols" — not an exhaustive proof of
--     self-funding across every possible lending market.
--
-- resolved_start_time / resolved_end_time / resolved_from_block /
-- resolved_to_block are echoed on every row so a downstream export always
-- carries its own window, per WHI-1406's "resolved bounds recorded with
-- every result/export" requirement.

WITH params AS (
  SELECT
    TIMESTAMP '{{start_time}}' AS start_time,
    TIMESTAMP '{{end_time}}'   AS end_time,
    CAST({{from_block}} AS bigint) AS from_block,
    CAST({{to_block}}   AS bigint) AS to_block
),

swap_legs AS (
  SELECT
    d.tx_hash,
    d.block_number,
    d.block_time,
    d.tx_from,
    d.tx_to,
    d.project,
    d.project_contract_address,
    d.evt_index
  FROM dex.trades d
  CROSS JOIN params p
  WHERE d.blockchain = 'mantle'
    AND d.block_time >= p.start_time
    AND d.block_time <  p.end_time
    AND (p.from_block < 0 OR d.block_number BETWEEN p.from_block AND p.to_block)
),

tx_legs AS (
  SELECT
    tx_hash,
    arbitrary(block_number) AS block_number,
    arbitrary(block_time)   AS block_time,
    arbitrary(tx_from)      AS tx_from,
    arbitrary(tx_to)        AS tx_to,
    count(*)                AS hop_count,
    array_agg(project_contract_address ORDER BY evt_index) AS ordered_pools,
    array_agg(project ORDER BY evt_index)                  AS ordered_projects,
    bool_or(project IS NULL OR project = '')               AS has_unmapped_leg
  FROM swap_legs
  GROUP BY tx_hash
  HAVING count(*) >= 2
),

hop_mix AS (
  SELECT
    *,
    CASE
      WHEN has_unmapped_leg THEN 'unknown'
      WHEN hop_count = 2 THEN '2-hop'
      WHEN hop_count = 3 THEN '3-hop'
      WHEN hop_count > 3 THEN '>3-hop'
      ELSE 'unknown'
    END AS hop_mix_category
  FROM tx_legs
),

tx_meta AS (
  SELECT
    t.hash AS tx_hash,
    t.value AS msg_value_wei,
    t.success,
    t.index AS tx_index,
    t.type AS tx_type,
    t.gas_used,
    t.gas_price,
    t.max_fee_per_gas,
    t.max_priority_fee_per_gas,
    t.priority_fee_per_gas,
    b.base_fee_per_gas
  FROM mantle.transactions t
  JOIN hop_mix h ON h.tx_hash = t.hash
  JOIN mantle.blocks b ON b.number = t.block_number
  CROSS JOIN params p
  WHERE t.block_time >= p.start_time
    AND t.block_time <  p.end_time
),

-- Entity = {tx.from, tx.to} together, counted once as tx.from ("bot_address").
-- Internal tx.from<->tx.to transfers cancel automatically in this net sum.
--
-- Sign classification (net_raw, gross_out_external_raw) uses amount_raw cast
-- to DECIMAL(38,0) — exact integer arithmetic, no floating-point rounding —
-- so an internal transfer's contribution to the "to" and "from" sums cancels
-- to a literal, exact 0, never floating dust. amount_raw is unsigned
-- (UINT256), so the cast-to-DECIMAL step (rather than staying in UINT256) is
-- what makes the subtraction itself safe: a plain UINT256 subtraction
-- underflows/errors the moment a net is negative (verified while building
-- this query), whereas DECIMAL(38,0) allows negative results directly.
-- DECIMAL(38,0) comfortably covers any realistic ERC-20 raw amount (up to
-- ~1e38), far above real token supplies, without UINT256's full 1e77 range.
--
-- gross_out_external_raw counts ONLY legs where the sender is in the entity
-- AND the recipient is NOT — i.e. genuine external outflow. A prior version
-- of this query included tx.from<->tx.to internal legs in gross_out, which
-- could let a tx that only ever shuffled tokens between its own from/to pass
-- the "entity actually sent tokens" check on a completely unrelated
-- net-positive external receipt of a different token. Fixed here.
entity_transfers AS (
  SELECT
    tr.tx_hash,
    tr.contract_address AS token,
    arbitrary(tr.symbol) AS symbol,
    SUM(CASE WHEN tr."to" IN (tr.tx_from, tr.tx_to) THEN CAST(tr.amount_raw AS DECIMAL(38, 0)) ELSE CAST(0 AS DECIMAL(38, 0)) END)
      - SUM(CASE WHEN tr."from" IN (tr.tx_from, tr.tx_to) THEN CAST(tr.amount_raw AS DECIMAL(38, 0)) ELSE CAST(0 AS DECIMAL(38, 0)) END) AS net_raw,
    SUM(CASE WHEN tr."from" IN (tr.tx_from, tr.tx_to) AND tr."to" NOT IN (tr.tx_from, tr.tx_to) THEN CAST(tr.amount_raw AS DECIMAL(38, 0)) ELSE CAST(0 AS DECIMAL(38, 0)) END) AS gross_out_external_raw,
    -- Decimal-adjusted magnitude, used ONLY to rank multiple net-positive
    -- legs against each other for the settlement_asset tie-break below —
    -- never for the qualification sign checks (those use net_raw). This is
    -- a deliberate, documented choice to avoid any USD/price comparison
    -- (explicitly out of scope): different tokens' decimal-adjusted amounts
    -- are not economically equivalent, but they are the best available
    -- like-for-like comparison without pricing data.
    SUM(CASE WHEN tr."to" IN (tr.tx_from, tr.tx_to) THEN tr.amount ELSE 0 END)
      - SUM(CASE WHEN tr."from" IN (tr.tx_from, tr.tx_to) THEN tr.amount ELSE 0 END) AS net_display_amount
  FROM tokens.transfers tr
  CROSS JOIN params p
  WHERE tr.blockchain = 'mantle'
    AND tr.block_time >= p.start_time
    AND tr.block_time <  p.end_time
    AND tr.tx_hash IN (SELECT tx_hash FROM hop_mix)
  GROUP BY tr.tx_hash, tr.contract_address
),

entity_summary AS (
  SELECT
    tx_hash,
    SUM(gross_out_external_raw) AS total_gross_out_external_raw,
    COUNT(*) FILTER (WHERE net_raw < 0) AS n_negative_tokens,
    COUNT(*) FILTER (WHERE net_raw > 0) AS n_positive_tokens
  FROM entity_transfers
  GROUP BY tx_hash
),

-- settlement_asset = the net-positive token with the largest DECIMAL-ADJUSTED
-- magnitude (net_display_amount), tie-broken by token address ascending.
-- Candidate set (net_raw > 0) uses the exact integer sign; only the ranking
-- among candidates uses the decimal-adjusted amount (see comment above).
-- NULLS LAST on net_display_amount matters: `tr.amount` (the decimal-
-- adjusted double) is null when a token's decimals metadata is unknown to
-- Dune, even though `net_raw` (integer, decimals-independent) can still be
-- a real positive candidate. Without NULLS LAST such a token could win the
-- tie-break purely from a null sorting ahead of real numbers, not because
-- it is actually the largest leg.
settlement AS (
  SELECT
    et.tx_hash,
    et.token AS settlement_asset,
    et.symbol AS settlement_symbol,
    ROW_NUMBER() OVER (PARTITION BY et.tx_hash ORDER BY et.net_display_amount DESC NULLS LAST, et.token ASC) AS rn
  FROM entity_transfers et
  WHERE et.net_raw > 0
),

-- Verified live on Mantle at implementation time (real, recent rows):
-- aave_v3_mantle, lendle_mantle, aurelius_finance_mantle all have decoded
-- LiquidationCall / FlashLoan event tables with current activity.
--
-- The three-way UNION below repeats the same predicate per protocol
-- deliberately, not by oversight: DuneSQL has no dynamic/parameterized table
-- names, so naming three concrete decoded tables is the only way to union
-- them in static SQL. This is a fixed list of three verified sources, not
-- qualification logic that risks drifting out of sync with itself.
liquidation_txs AS (
  SELECT DISTINCT evt_tx_hash AS tx_hash FROM aave_v3_mantle.pool_evt_liquidationcall
    CROSS JOIN params p WHERE evt_block_time >= p.start_time AND evt_block_time < p.end_time
  UNION
  SELECT DISTINCT evt_tx_hash FROM lendle_mantle.lendingpool_evt_liquidationcall
    CROSS JOIN params p WHERE evt_block_time >= p.start_time AND evt_block_time < p.end_time
  UNION
  SELECT DISTINCT evt_tx_hash FROM aurelius_finance_mantle.lendingpool_evt_liquidationcall
    CROSS JOIN params p WHERE evt_block_time >= p.start_time AND evt_block_time < p.end_time
),

flash_loan_txs AS (
  SELECT DISTINCT evt_tx_hash AS tx_hash FROM aave_v3_mantle.pool_evt_flashloan
    CROSS JOIN params p WHERE evt_block_time >= p.start_time AND evt_block_time < p.end_time
  UNION
  SELECT DISTINCT evt_tx_hash FROM lendle_mantle.lendingpool_evt_flashloan
    CROSS JOIN params p WHERE evt_block_time >= p.start_time AND evt_block_time < p.end_time
  UNION
  SELECT DISTINCT evt_tx_hash FROM aurelius_finance_mantle.lendingpool_evt_flashloan
    CROSS JOIN params p WHERE evt_block_time >= p.start_time AND evt_block_time < p.end_time
),

-- Coverage-aware sandwich labeling: 'unknown' when dex.sandwiches/sandwiched
-- have zero rows for this exact window (true for the default 3-month window
-- at implementation time — see header note above).
sandwich_coverage AS (
  SELECT
    (SELECT count(*) FROM dex.sandwiches CROSS JOIN params p WHERE blockchain = 'mantle' AND block_time >= p.start_time AND block_time < p.end_time)
    + (SELECT count(*) FROM dex.sandwiched CROSS JOIN params p WHERE blockchain = 'mantle' AND block_time >= p.start_time AND block_time < p.end_time)
    AS n_covered_rows
),

sandwich_txs AS (
  SELECT DISTINCT tx_hash FROM dex.sandwiches CROSS JOIN params p WHERE blockchain = 'mantle' AND block_time >= p.start_time AND block_time < p.end_time
  UNION
  SELECT DISTINCT tx_hash FROM dex.sandwiched CROSS JOIN params p WHERE blockchain = 'mantle' AND block_time >= p.start_time AND block_time < p.end_time
),

qualified AS (
  SELECT
    h.tx_hash,
    h.block_number,
    h.block_time,
    h.tx_from AS bot_address,
    h.tx_to AS executor_address,
    h.hop_count,
    h.ordered_pools,
    h.ordered_projects,
    h.hop_mix_category AS hop_mix,
    m.msg_value_wei,
    m.tx_type,
    m.tx_index,
    m.gas_used,
    m.gas_price,
    m.base_fee_per_gas,
    m.max_fee_per_gas,
    m.max_priority_fee_per_gas,
    m.priority_fee_per_gas,
    CASE
      WHEN m.priority_fee_per_gas IS NOT NULL THEN m.priority_fee_per_gas
      WHEN m.gas_price IS NOT NULL AND m.base_fee_per_gas IS NOT NULL AND m.gas_price >= m.base_fee_per_gas
        THEN m.gas_price - m.base_fee_per_gas
      ELSE NULL
    END AS effective_tip_per_gas,
    s.settlement_asset,
    s.settlement_symbol,
    CASE WHEN fl.tx_hash IS NOT NULL THEN true ELSE false END AS is_flash_loan,
    'unknown' AS is_jit_lp,
    CASE
      WHEN (SELECT n_covered_rows FROM sandwich_coverage) = 0 THEN 'unknown'
      WHEN sw.tx_hash IS NOT NULL THEN 'true'
      ELSE 'false'
    END AS is_sandwich
  FROM hop_mix h
  JOIN tx_meta m ON m.tx_hash = h.tx_hash
  JOIN entity_summary es ON es.tx_hash = h.tx_hash
  LEFT JOIN settlement s ON s.tx_hash = h.tx_hash AND s.rn = 1
  LEFT JOIN liquidation_txs l ON l.tx_hash = h.tx_hash
  LEFT JOIN flash_loan_txs fl ON fl.tx_hash = h.tx_hash
  LEFT JOIN sandwich_txs sw ON sw.tx_hash = h.tx_hash
  WHERE m.success = true
    AND m.msg_value_wei <= CAST(POWER(10, 18) AS uint256)
    AND es.n_negative_tokens = 0
    AND es.n_positive_tokens >= 1
    AND es.total_gross_out_external_raw > 0
    AND l.tx_hash IS NULL
)
SELECT
  '0x' || lower(to_hex(tx_hash)) AS tx_hash,
  block_number,
  block_time,
  '0x' || lower(to_hex(bot_address)) AS bot_address,
  '0x' || lower(to_hex(executor_address)) AS executor_address,
  hop_count,
  array_join(transform(ordered_pools, x -> '0x' || lower(to_hex(x))), ';') AS ordered_pools,
  array_join(ordered_projects, ';') AS ordered_projects,
  hop_mix,
  CAST(msg_value_wei AS varchar) AS msg_value_wei,
  tx_type,
  tx_index,
  gas_used,
  CAST(gas_price AS varchar) AS gas_price,
  CAST(base_fee_per_gas AS varchar) AS base_fee_per_gas,
  CAST(max_fee_per_gas AS varchar) AS max_fee_per_gas,
  CAST(max_priority_fee_per_gas AS varchar) AS max_priority_fee_per_gas,
  CAST(priority_fee_per_gas AS varchar) AS priority_fee_per_gas,
  CAST(effective_tip_per_gas AS varchar) AS effective_tip_per_gas,
  '0x' || lower(to_hex(settlement_asset)) AS settlement_asset,
  settlement_symbol,
  is_flash_loan,
  is_jit_lp,
  is_sandwich,
  (SELECT start_time FROM params) AS resolved_start_time,
  (SELECT end_time FROM params) AS resolved_end_time,
  (SELECT from_block FROM params) AS resolved_from_block,
  (SELECT to_block FROM params) AS resolved_to_block
FROM qualified
ORDER BY block_number, tx_hash
