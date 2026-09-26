# WHI-1413 44-arb denominator — frozen provenance (orchestrator 3eed43fd, 2026-09-26)

Source: the analysis session that created WHI-1413 (pi session 2026-09-21T01-58-30-602Z_01a0c1af…).
- Dune query **8788689** ("WHI - mantle arb bot 0x99Bb per-tx pool sets (7d)"), private. Execution **01M30WWX1XTF7EMWFQ81HVNKB7** (run 2026-09-21 ~02:32Z). Results were re-fetched read-only on 2026-09-26 via Dune MCP getExecutionResults: totalRowCount 83, COMPLETED.
- The SQL, verbatim from that session:
  - `txs` = `mantle.transactions` where `block_date >= CURRENT_DATE - INTERVAL '7' DAY` AND `"from" = 0x99Bb2996bCC38555e794D8FcAF99924b8D4C13ED` AND `success = true`;
  - swap legs = `mantle.logs` with the same block_date filter and topic0 IN
    - `0xd78ad95f…d822` (UniV2 Swap)
    - `0xc42079f9…ca67` (UniV3 Swap)
    - `0xad7d6f97…ca70` (Moe LB Swap)
  - rows: `hash, block_number, hops = count(*)`, and `pools = array_join(array_agg(contract_address ORDER BY l.index), ',')`, grouped per tx.
  - The window resolves to block_date >= 2026-09-14 up to execution time. The rows span blocks 100614637..100902051.
- The extraction rules in the WHI-1413 issue text were applied to these rows:
  - 83 txs with swap logs (119 successful txs in total per the companion query 8788673).
  - **multi-hop = hops >= 2 → 44**.
  - 45 distinct pools.
  - "fully covered" = every swap-log pool address is present in the universe pool column.
- Orchestrator reproduction against the committed `data/pool_universe.csv` (130 pools) @ dev de735db: **17 / 44**, which matches the issue's 17/44. The issue's 16/44 was computed against the production 109-pool universe fetched over ssh from arb-bot-jp. That universe is not in the repo.
- File: `dune_8788689_exec_01M30WWX1XTF7EMWFQ81HVNKB7.csv` (pools pipe-separated, in log-index order). sha256 db1593bef0950dcd134c2400c3f0609f6f63c5b3f642fd3af8649a81b55ba7c9.
- `missing_pool_factories_rpc_mantle_xyz.txt`: `factory()` for the 21 missing multi-hop pools, read-only eth_call against https://rpc.mantle.xyz (latest block, 2026-09-26). "-" means factory() reverted. Two such pools remain unclassified: bb2f5148…, abaff1d3….
- `marginal_by_venue_prelim.txt`: the orchestrator's **preliminary upper bound**. It assumes every missing pool of a venue would be admitted and ignores TVL, the admission floor and hop/settlement scope. It is NOT the decision record. The implementer must redo it with the WHI-999 tool and methodology, including admission cost.
