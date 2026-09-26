#!/usr/bin/env python3
"""WHI-1413: which route classes do Merchant Moe V1 classic pools make reachable?

Pure. Enumerates WMNT settlement cycles of 2..3 hops over a universe CSV with
the same rules as evidence/gas/whi-1422/rank_topologies.py (no immediate
same-pool reversal, no repeated intermediate token), keeps the cycles with at
least one Moe V1 pool, and reports per topology:

* cycles, and how many have every V3 hop on the Agni factory (the only V3
  factory the WHI-501 executor can execute, hence the only one the fork
  campaign can measure — DI-50);
* the gas profile's statuses for that topology's bucket variants (any
  `approved` variant means the existing profile would price Moe V1 pools
  in that class, because RouteKey has no factory axis).

Usage: moe_v1_reach.py <universe_csv> <gas_profile_json>
"""
import collections
import csv
import json
import sys

WMNT = "0x78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8"
MOE_V1 = "0x5bef015ca9424a7c07b68490616a4c1f094bedec"
AGNI_V3 = "0x25780dc8fc3cfbd75f33bfdab65e969b603b2035"
KIND = {"agni-v2": "v2", "agni-v3": "v3", "moe": "moe"}


def main():
    rows = list(csv.DictReader(open(sys.argv[1])))
    profile = json.load(open(sys.argv[2]))
    status = collections.defaultdict(list)
    for p in profile["profiles"]:
        rk = p["route_key"]
        status["+".join(rk["protocols"])].append((p["status"] if isinstance(p["status"], str) else "unsupported", rk))
    factory = {r["pool"].lower(): r["factory"].lower() for r in rows}
    adj = collections.defaultdict(list)
    for r in rows:
        t0, t1, pool, k = r["token0"].lower(), r["token1"].lower(), r["pool"].lower(), KIND[r["protocol"]]
        adj[t0].append((t1, pool, k))
        adj[t1].append((t0, pool, k))
    cycles = []

    def dfs(tok, path, seen):
        if len(path) >= 3:
            return
        for nxt, pool, k in adj[tok]:
            if path and path[-1][0] == pool:
                continue
            if nxt == WMNT:
                if len(path) + 1 >= 2:
                    cycles.append(path + [(pool, k)])
                continue
            if nxt in seen:
                continue
            dfs(nxt, path + [(pool, k)], seen | {nxt})

    dfs(WMNT, [], {WMNT})
    per = collections.defaultdict(lambda: [0, 0])
    moe_pools = set()
    for c in cycles:
        if not any(factory[p] == MOE_V1 for p, _ in c):
            continue
        moe_pools |= {p for p, _ in c if factory[p] == MOE_V1}
        topo = "+".join(k for _, k in c)
        per[topo][0] += 1
        if all(factory[p] == AGNI_V3 for p, k in c if k == "v3"):
            per[topo][1] += 1
    print(f"total_cycles={len(cycles)} moe_v1_cycles={sum(v[0] for v in per.values())} moe_v1_pools_on_cycles={len(moe_pools)}")
    print(f"{'topology':<14} {'cycles':>6} {'agni_only':>9}  approved_variants")
    for topo, (n, agni) in sorted(per.items(), key=lambda x: -x[1][0]):
        approved = [s[1] for s in status[topo] if s[0] == "approved"]
        tag = ",".join(k for rk in approved for k in (["ticks=" + rk["v3_tick_crossings"]] if "v3_tick_crossings" in rk else []) + (["bins=" + rk["moe_bin_crossings"]] if "moe_bin_crossings" in rk else [])) or ("(bucketless)" if approved else "-")
        print(f"{'h' + str(topo.count('+') + 1) + ':' + topo:<14} {n:>6} {agni:>9}  {'APPROVED ' + tag if approved else '-'}")


if __name__ == "__main__":
    main()
