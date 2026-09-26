#!/usr/bin/env python3
"""WHI-1413: rank UniV2-family venues by marginal arbs unlocked (WHI-999 rule).

Pure: reads the frozen denominator (`arbs_44.jsonl`), its census and one or
more `missed_arb_universe` JSON reports (for per-pool TVL at the report's
block). An arb counts as covered only when **every** swap-log pool is held.

For each venue subset S of the unregistered UniV2-family venues, two views:

* `ignore_floor` — every competitor pool on S is admitted (the orchestrator's
  preliminary upper bound, reproduced as a cross-check);
* `floor@<block>` — only pools the generator would admit: TVL >= the report's
  floor at that block (the WHI-999 / universe_gen admission rule). Pools on
  venues that already load (FusionX V2 interim, registered V3 factories) are
  admitted under the same rule in every row, so the marginal is attributable
  to S alone.

Usage: venue_rank.py <arbs_44.jsonl> <census_44.json> <report.json> [<report.json> ...]
"""
import itertools
import json
import sys

V2_VENUES = ["moe-v1", "mantleswap-v2"]


def main():
    arbs = [json.loads(l) for l in open(sys.argv[1]) if l.strip()]
    census = json.load(open(sys.argv[2]))
    held = {p for p, e in census.items() if e["held"]}
    base = sum(all(p in held for p in a["path"]) for a in arbs)
    print(f"denominator={len(arbs)} baseline_fully_covered={base}")

    views = [("ignore_floor", None, None)]
    for path in sys.argv[3:]:
        rep = json.load(open(path))
        floor = int(rep["inputs"].get("min_tvl_wmnt_wei") or 10**21)
        tvl = {m["pool"]: int(m["tvl_wmnt_wei"]) if m.get("tvl_wmnt_wei") else None for m in rep["missing_pools"]}
        views.append((f"floor@{rep['inputs'].get('tvl_block')}", floor, tvl))

    def admitted(pool, venues, floor, tvl):
        e = census[pool]
        loadable_today = e["venue"] in ("fusionx-v2", "agni-v3", "fusionx-v3", "butter", "fluxion-v3", "v3fork-636ea2", "uniswap-v3")
        if not (loadable_today or e["venue"] in venues):
            return False
        if floor is None:
            return e["venue"] in venues  # upper bound isolates the venue itself
        t = tvl.get(pool)
        return t is not None and t >= floor

    for name, floor, tvl in views:
        print(f"\n## view {name}")
        ref = None
        for r in range(0, len(V2_VENUES) + 1):
            for combo in itertools.combinations(V2_VENUES, r):
                add = {p for p in census if p not in held and admitted(p, set(combo), floor, tvl)}
                cov = sum(all(p in held or p in add for p in a["path"]) for a in arbs)
                if ref is None:
                    ref = cov
                venue_pools = sorted(p for p in add if census[p]["venue"] in combo)
                print(f"{'+'.join(combo) or '(none: loadable venues only)':<32} covered={cov:>2}/44 "
                      f"marginal_vs_none={cov - ref:+d} vs_baseline={cov - base:+d} "
                      f"venue_pools_admitted={len(venue_pools)} {[census[p]['s0'] + '/' + census[p]['s1'] for p in venue_pools]}")
        # fusionx-v2 upper bound, for the orchestrator's prelim cross-check
        if floor is None:
            for extra in (["fusionx-v2"], ["fusionx-v2", "moe-v1"], ["fusionx-v2", "moe-v1", "mantleswap-v2"]):
                add = {p for p in census if p not in held and census[p]["venue"] in extra}
                cov = sum(all(p in held or p in add for p in a["path"]) for a in arbs)
                print(f"{'+'.join(extra):<32} covered={cov:>2}/44 vs_baseline={cov - base:+d} (prelim cross-check)")


if __name__ == "__main__":
    main()
