#!/usr/bin/env python3
"""WHI-1413: is each approved V2-containing route class conservative for Moe V1 pools?

RouteKey has no factory axis, so a class approved on FusionX V2 samples also
prices Moe V1 classic pools once they are in the universe. For every class
with a `v2` hop, this lists the campaign's Moe V1 samples (fork_replay
successes whose cycle contains a Moe V1 pool of the given universe), their
max gas, and whether all of them sit below the class's gas limit in the given
profile(s).

Usage: moe_v1_gate.py <attempts.jsonl> <universe_csv> <profile.json> [<profile.json> ...]
"""
import collections
import csv
import json
import sys

MOE_V1 = "0x5bef015ca9424a7c07b68490616a4c1f094bedec"


def main():
    moe = {r["pool"].lower() for r in csv.DictReader(open(sys.argv[2])) if r["factory"].lower() == MOE_V1}
    last = {}
    for line in open(sys.argv[1]):
        if line.strip():
            a = json.loads(line)
            last[(tuple(a["cycle"]), a["amount_in_wmnt_milli"], a["lever"])] = a
    by_key = collections.defaultdict(list)
    levers = collections.defaultdict(set)
    for a in last.values():
        if a["outcome"] != "success" or not any(p in moe for p in a["cycle"]):
            continue
        by_key[a["route_key"]].append(a["gas_used"])
        levers[a["route_key"]].add(a["lever"])
    profiles = [json.load(open(p)) for p in sys.argv[3:]]
    print(f"moe_v1_pools={len(moe)} attempts={len(last)}")
    hdr = "route key".ljust(34) + " moe_v1_n  max_gas   levers        " + "  ".join(f"status/limit[{i}]" for i in range(len(profiles)))
    print(hdr)
    keys = set(by_key)
    for prof in profiles:
        keys |= {p_key(p) for p in prof["profiles"] if p["status"] == "approved" and "v2" in p["route_key"]["protocols"]}
    for k in sorted(keys):
        g = by_key.get(k, [])
        cols = []
        for prof in profiles:
            p = next((p for p in prof["profiles"] if p_key(p) == k), None)
            if p is None:
                cols.append("-")
                continue
            st = p["status"] if isinstance(p["status"], str) else "unsupported"
            lim = p.get("gas_limit")
            ok = "" if lim is None or not g else (" all<=limit" if max(g) <= lim else " EXCEEDS")
            cols.append(f"{st}/{lim}{ok}")
        print(f"{k:<34} {len(g):>8}  {max(g) if g else '-':>8}   {','.join(sorted(levers[k])) or '-':<12}  " + "  ".join(cols))


def p_key(p):
    rk = p["route_key"]
    s = f"h{rk['hop_count']}:" + "+".join(rk["protocols"])
    if "v3_tick_crossings" in rk:
        s += ":ticks=" + rk["v3_tick_crossings"]
    if "moe_bin_crossings" in rk:
        s += ":bins=" + rk["moe_bin_crossings"]
    return s


if __name__ == "__main__":
    main()
