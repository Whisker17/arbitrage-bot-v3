#!/usr/bin/env python3
"""Local logic check of scripts/dunesql/00_qualified_arbs.sql's hop_mix — NOT a Dune execution.

Cuts the ACTUAL CTE text of 00 (mapping tables .. hop_mix), transpiles it Trino -> DuckDB with
sqlglot, and runs it on a mocked dex.trades. Harness-only rewrites: 0x varbinary literals become
strings, dex.trades becomes a local table. Exits non-zero on any mismatch (it fails on the base
count-bucket version, whose hop_mix is '2-hop' for every two-leg tx).

  uvx --with duckdb==1.1.3 --from sqlglot==25.24.0 python \
    evidence/dunesql/whi-1545/sql_logic_check.py scripts/dunesql/00_qualified_arbs.sql
"""
import re,sys,sqlglot,duckdb
src=open(sys.argv[1]).read()
start=src.find("family_factory (factory, family, venue) AS (")
if start < 0:
    start=src.index("swap_legs AS (")
end=src.index("tx_meta AS (")
ctes=src[start:end].rstrip().rstrip(",")
ctes=re.sub(r"\b(0x[0-9a-f]{40})\b",r"'\1'",ctes)
mock="""params AS (SELECT TIMESTAMP '2026-09-28 00:00:00' AS start_time, TIMESTAMP '2026-09-29 00:00:00' AS end_time,
  CAST(-1 AS bigint) AS from_block, CAST(-1 AS bigint) AS to_block),
"""
q="WITH "+mock+ctes+"\nSELECT tx_hash, hop_count, hop_mix_category FROM hop_mix ORDER BY tx_hash"
q=q.replace("FROM dex.trades d","FROM trades d")
duck=sqlglot.transpile(q,read="trino",write="duckdb")[0]
con=duckdb.connect()
T="TIMESTAMP '2026-09-28 01:00:00'"
LB='0x1606c79be3ebd70d8d40bac6287e23005cfbefa2'; V1='0x4e7685df06201521f35a182467feefe02c53d847'
AG='0x8fb12e957edbefd0105857fef675d621c8629a71'; FX='0x262255f4770aebe2d0c8b97a46287dcecc2a0aff'; UNK='0x928981fe5a4c005a126662d2bd84fbf139b51876'
rows=[ # tx, project, pool, evt_index
 ('a','merchant_moe',LB,1),('a','agni',AG,2),          # lb>v3
 ('b','merchant_moe',V1,5),('b','fusionx',FX,3),       # order by evt_index: v3>v2
 ('c','merchant_moe',LB,1),('c','agni',UNK,2),         # unmapped -> unknown
 ('d','agni',AG,1),('d','agni',AG,1),                  # duplicate evt_index -> unknown
 ('e','agni',AG,None),('e','fusionx',FX,2),            # null evt_index -> unknown
 ('f','merchant_moe',V1,1),('f','merchant_moe',V1,2),('f','merchant_moe',LB,3),('f','agni',AG,4), # >3-hop v2>v2>lb>v3
 ('g','',AG,1),('g',None,FX,2),                        # empty/null project, mapped pools -> v3>v3
 ('h','agni',AG,1),                                    # single leg: dropped by HAVING
]
con.execute("CREATE TABLE trades(blockchain VARCHAR, block_time TIMESTAMP, block_number BIGINT, tx_hash VARCHAR, tx_from VARCHAR, tx_to VARCHAR, project VARCHAR, project_contract_address VARCHAR, evt_index BIGINT)")
for t,p,pool,e in rows:
    con.execute(f"INSERT INTO trades VALUES ('mantle',{T},1,?,'s','x',?,?,?)",[t,p,pool,e])
con.execute("INSERT INTO trades VALUES ('mantle',TIMESTAMP '2026-09-30 00:00:00',1,'z','s','x','agni',?,1),('mantle',TIMESTAMP '2026-09-30 00:00:00',1,'z','s','x','agni',?,2)",[AG,AG]) # out of window
exp={'a':('2-hop','lb>v3'),'b':('2-hop','v3>v2'),'c':('2-hop','unknown'),'d':('2-hop','unknown'),'e':('2-hop','unknown'),
     'f':('>3-hop','v2>v2>lb>v3'),'g':('2-hop','v3>v3')}
got=con.execute(duck).fetchall()
for r in got: print(r)
got_mix={r[0]:r[2] for r in got}
exp_mix={k:v[1] for k,v in exp.items()}
if got_mix!=exp_mix:
    sys.exit(f"FAIL: hop_mix mismatch:\n got      {got_mix}\n expected {exp_mix}")
if "hop_count_bucket" not in ctes:
    sys.exit("FAIL: no hop_count_bucket field")
got_b=dict(con.execute(duck.replace("hop_count, hop_mix_category", "hop_count_bucket")).fetchall())
exp_b={k:v[0] for k,v in exp.items()}
if got_b!=exp_b:
    sys.exit(f"FAIL: hop_count_bucket mismatch: got {got_b} expected {exp_b}")
print("OK: actual 00 CTE SQL (transpiled trino->duckdb) matches expected on", len(exp), "mocked txs")
