# Protocol-family `hop_mix` for the Dune competitor pack: offline verification

Scope: `scripts/dunesql/00_qualified_arbs.sql` now defines `hop_mix` as the ordered
protocol-family mix (`v2` / `v3` / `lb` / `algebra`, joined by `>` in `evt_index` order, or
`unknown`). The rule and its sources are in `scripts/dunesql/README.md` § "Protocol-family hop
mix".

**No Dune query was executed or modified for this change.** Everything here is offline.

| file | what it is |
| --- | --- |
| `hop_mix_mirror.py` | `--check` verifies `00`'s `family_factory` rows against `evidence/venues/MATRIX.md`, checks key uniqueness, and confirms the `pool_factory` block equals a fresh regeneration from committed pool evidence (`--emit-pool-values`). `--mirror CSV` is a **Python mirror** of the SQL logic, applied to a frozen export. It reads the mapping tables out of the SQL file itself. |
| `mirror_sept28.json` | Output of `--mirror` on the frozen 2026-09-28 export of query 8781229: 252 rows, sha256 `11ed6d1967a8f9a188c104a202975a925b3a3a757d60a9920af3c88a6d7f6cfe`. The export is external; its hash is pinned in `evidence/peer-attribution/whi-1412/manifest.json`. |
| `sql_logic_check.py` | Runs the **actual** `00` CTE text (mapping tables through `hop_mix`) on a mocked `dex.trades` in DuckDB, after a sqlglot Trino→DuckDB transpile. It covers mapped, reordered, unmapped, duplicate-`evt_index`, null-`evt_index`, >3-hop and empty-project cases. It passes at this HEAD and fails on the base version. |

Results (`mirror_sept28.json`):

* The base `hop_mix` is `2-hop` for all 115 two-leg txs.
* The new mix has 7 distinct two-leg values and 228 mapped / 24 `unknown` txs. The unknowns come from 20 pools that have no committed factory evidence.
* Equal hop count, different family:
  * `0x87d95c63…` is `v3>v3`;
  * `0xa4f87b88…` is `lb>v3`;
  * `0x9e1d0bc0…` is `v2>v3`.
* Same project sequence `merchant_moe;merchant_moe`, different family:
  * `0x48f9322d…` is `lb>lb` (Moe LB factory `0xa663…`);
  * `0x6eee6497…` is `v2>v2` (Moe V1 classic factory `0x5bef…`).
* Unmappable: `0x82523d55…` gives `lb;unknown` → `unknown`, because agni pool `0x928981fe…` has no committed factory evidence.

Limits:

* The export has legs already in `evt_index` order, without `evt_index` itself. The mirror therefore cannot exercise the order guard; `sql_logic_check.py` covers it on mock data.
* Coverage is bounded by the committed pool evidence.
* Dune semantics were not re-probed: `project_contract_address` is the pool, and `0x…` literals are varbinary. Both are in the post-publish validation proposed for the owner.

Reproduce:

```bash
python3 evidence/dunesql/whi-1545/hop_mix_mirror.py --check
python3 evidence/dunesql/whi-1545/hop_mix_mirror.py --mirror <RUN>/sept28-inputs/dune/arb_detail_feed_20260928.csv --out evidence/dunesql/whi-1545/mirror_sept28.json
uvx --with duckdb==1.1.3 --from sqlglot==25.24.0 python evidence/dunesql/whi-1545/sql_logic_check.py scripts/dunesql/00_qualified_arbs.sql
```
