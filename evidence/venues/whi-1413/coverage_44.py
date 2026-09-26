#!/usr/bin/env python3
"""WHI-1413 AC4: like-for-like coverage on the frozen 44-arb denominator.

Exactly the WHI-1413 extraction rules (evidence/venues/whi-1413/denominator/PROVENANCE.md):
multi-hop = hops >= 2 over the frozen Dune extract; "fully covered" = every
swap-log pool address of the arb is in the universe's `pool` column. Pure; no
RPC. Prints one line per universe plus the arbs that changed state.

Usage: coverage_44.py <dune_csv> <universe_csv> [<universe_csv> ...]
"""
import csv
import sys


def main():
    rows = [r for r in csv.DictReader(open(sys.argv[1])) if int(r["hops"]) >= 2]
    results = []
    for path in sys.argv[2:]:
        pools = {r["pool"].lower() for r in csv.DictReader(open(path))}
        covered = {r["hash"] for r in rows if all(p.lower() in pools for p in r["pools"].split("|"))}
        results.append((path, len(pools), covered))
        print(f"{path}: pools={len(pools)} fully_covered={len(covered)}/{len(rows)} ({100 * len(covered) / len(rows):.1f}%)")
    base = results[0][2]
    for path, _, cov in results[1:]:
        for h in sorted(cov - base):
            print(f"  newly covered vs {results[0][0]}: {h}")
        for h in sorted(base - cov):
            print(f"  LOST vs {results[0][0]}: {h}")


if __name__ == "__main__":
    main()
