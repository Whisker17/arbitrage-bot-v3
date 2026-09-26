#!/usr/bin/env python3
"""WHI-1413: augmented universes for the per-venue admission cost.

Writes <out_dir>/<venues>__<view>.csv = the committed pre-WHI-1413 universe
(regen-98969898/repro, byte-identical to it) plus the venue's competitor pools
that the view admits (TVL >= floor at the report's block, or every pool for
`nofloor`). Then, per file:

  ./target/release/missed_arb_universe --universe <file> \\
    --arbs denominator/arbs_44.jsonl --census denominator/census_44.json \\
    --set-sizes 1 --top-n 1 --json-out <file>.json

and read `inputs.universe_pool_count`, `baseline.reachable_now` and
`hop_cap.cycle_count_at_cap` (production PathFinder). Cold start = pools * 350/130 s.
Results: admission_cost.txt.

Usage: admission_cost.py <out_dir>
"""
import csv, json, os, sys

HERE = os.path.dirname(os.path.abspath(__file__))
out = sys.argv[1]
os.makedirs(out, exist_ok=True)
c = json.load(open(os.path.join(HERE, "denominator/census_44.json")))
rows = list(csv.DictReader(open(os.path.join(HERE, "regen-98969898/repro/pool_universe.csv"))))
views = {v: json.load(open(os.path.join(HERE, f"whi999/baseline_tvl_at_{v}.json"))) for v in ("window_end", "snapshot")}
for vname, rep in list(views.items()) + [("nofloor", None)]:
    tvl = {m["pool"]: int(m["tvl_wmnt_wei"] or 0) for m in rep["missing_pools"]} if rep else {}
    for combo in (["moe-v1"], ["mantleswap-v2"], ["moe-v1", "mantleswap-v2"]):
        add = [p for p, e in sorted(c.items()) if not e["held"] and e["venue"] in combo and (rep is None or tvl.get(p, 0) >= 10**21)]
        with open(os.path.join(out, f"{'+'.join(combo)}__{vname}.csv"), "w", newline="") as f:
            w = csv.DictWriter(f, fieldnames=rows[0].keys())
            w.writeheader()
            w.writerows(rows)
            for p in add:
                e = c[p]
                w.writerow({"protocol": "agni-v2", "factory": e["factory"], "pool": p, "token0": e["t0"], "token1": e["t1"], "fee_tier": "", "bin_step": "", "creation_block": ""})
        print(f"{'+'.join(combo)}__{vname}: +{len(add)} pools")
