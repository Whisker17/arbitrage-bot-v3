#!/usr/bin/env python3
"""Offline MIRROR of the protocol-family hop_mix logic in
scripts/dunesql/00_qualified_arbs.sql.

This is NOT a Dune execution. It re-implements, in Python, the CASE logic of
00's `tx_legs` / `hop_mix` CTEs and applies it to a frozen, already-exported
Dune result (the Sept-28 concurrent-window export of query 8781229, whose
sha256 is pinned in evidence/peer-attribution/whi-1412/manifest.json).

The two mapping tables (`family_factory`, `pool_factory`) are parsed straight
out of the SQL file, so the mirror cannot drift from what Dune would run.

Modes:
  --emit-pool-values   print the generated `pool_factory` VALUES body built from
                       the committed repo evidence (paste between the markers).
  --check              verify the SQL tables against committed evidence:
                       every family_factory row matches evidence/venues/MATRIX.md,
                       the pool_factory block equals the regenerated one, keys are
                       unique (so the LEFT JOINs in 00 cannot multiply rows).
  --mirror CSV [--out JSON]
                       apply the mirrored logic (and the base-version count-bucket
                       CASE, for contrast) to the frozen export.
"""

import argparse
import collections
import csv
import hashlib
import json
import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[3]
SQL = REPO / "scripts/dunesql/00_qualified_arbs.sql"
MATRIX = REPO / "evidence/venues/MATRIX.md"

# Committed pool -> factory evidence (each row's factory was resolved on chain by
# universe_gen / the WHI-1413 census). Order is irrelevant; conflicts fail.
POOL_UNIVERSE_CSVS = [
    "data/pool_universe.csv",
    "evidence/gas/whi-1520/measurement-only/pool_universe.measurement-only.csv",
    "evidence/shadow/candidate-window-lowtvl/universe/pool_universe.csv",
    "evidence/universe/whi-1410/arb-bot-jp/pool_universe.csv",
    "evidence/universe/whi-1410/regen-100871945/with-v2-seed/pool_universe.csv",
    "evidence/venues/whi-1413/regen-98969898/repro/pool_universe.csv",
    "evidence/venues/whi-1413/regen-98969898/with-moe-v1/pool_universe.csv",
]
MOE_LB_LIST = "data/poolLists_moe.csv"
CENSUS_44 = "evidence/venues/whi-1413/denominator/census_44.json"

# MATRIX.md `family` column -> short hop_mix family label.
MATRIX_FAMILY = {"univ2_cpmm": "v2", "univ3_cl": "v3", "moe_lb": "lb", "algebra_cl": "algebra"}

SEPT28_CSV_SHA256 = "11ed6d1967a8f9a188c104a202975a925b3a3a757d60a9920af3c88a6d7f6cfe"

HEX = r"0x[0-9a-f]{40}"


def sql_block(name):
    text = SQL.read_text()
    m = re.search(rf"-- BEGIN {name}\n(.*?)-- END {name}\n", text, re.S)
    if not m:
        sys.exit(f"marker block {name} not found in {SQL}")
    return m.group(1)


def family_table():
    rows = re.findall(rf"\(({HEX}), '([a-z0-9]+)', '([a-z0-9_]+)'\)", sql_block("family_factory"))
    return {f: (fam, venue) for f, fam, venue in rows}, len(rows)


def pool_table():
    rows = re.findall(rf"\(({HEX}), ({HEX})\)", sql_block("pool_factory"))
    return dict(rows), len(rows)


def evidence_pools(factories):
    ev = collections.defaultdict(set)
    for f in POOL_UNIVERSE_CSVS:
        for r in csv.DictReader(open(REPO / f)):
            ev[r["pool"].lower()].add((r["factory"].lower(), f))
    for r in csv.DictReader(open(REPO / MOE_LB_LIST)):
        ev[r["pool"].lower()].add((r["factory"].lower(), MOE_LB_LIST))
    for p, v in json.load(open(REPO / CENSUS_44)).items():
        if v.get("factory"):
            ev[p.lower()].add((v["factory"].lower(), CENSUS_44))
    out = {}
    for pool, srcs in ev.items():
        facs = {s[0] for s in srcs}
        if len(facs) != 1:
            sys.exit(f"conflicting factory evidence for {pool}: {sorted(srcs)}")
        fac = facs.pop()
        if fac in factories:
            out[pool] = fac
    return out


def emit_pool_values(factories):
    pools = evidence_pools(factories)
    items = sorted(pools.items(), key=lambda kv: (kv[1], kv[0]))
    lines = [f"    ({p}, {f})" for p, f in items]
    return ",\n".join(lines) + "\n"


def check():
    fams, nf = family_table()
    assert nf == len(fams), "duplicate factory in family_factory"
    matrix = MATRIX.read_text().lower()
    for fac, (fam, venue) in fams.items():
        row = next((ln for ln in matrix.splitlines() if ln.startswith("|") and fac in ln), None)
        assert row, f"{fac} ({venue}) not in {MATRIX}"
        mfam = row.split("|")[2].strip()
        assert MATRIX_FAMILY.get(mfam) == fam, f"{fac}: SQL family {fam} vs MATRIX {mfam}"
    pools, npools = pool_table()
    assert npools == len(pools), "duplicate pool in pool_factory"
    assert set(pools.values()) <= set(fams), "pool_factory references an unmapped factory"
    regenerated = emit_pool_values(fams)
    assert sql_block("pool_factory") == regenerated, "pool_factory block differs from regenerated evidence"
    by_fam = collections.Counter(fams[f][0] for f in pools.values())
    print(f"check OK: {nf} factories match MATRIX.md; {npools} pools, unique, regenerated identically; "
          f"pools by family {dict(sorted(by_fam.items()))}")


# ---- mirror of 00's tx_legs / hop_mix CASE logic ---------------------------

def base_hop_mix(projects, hop_count):
    """Base (f4264fd) 00:143-149, for contrast."""
    if any(p in (None, "") for p in projects):
        return "unknown"
    return {2: "2-hop", 3: "3-hop"}.get(hop_count, ">3-hop" if hop_count > 3 else "unknown")


def new_leg_family(pool, pools, fams):
    """00 swap_legs: COALESCE(ff.family, 'unknown') over pool_factory / family_factory LEFT JOINs."""
    fac = pools.get(pool)
    return fams[fac][0] if fac in fams else "unknown"


def new_hop_mix(families, order_determined=True):
    """00 hop_mix: unknown if any leg unmapped or evt_index order ambiguous, else '>'-joined."""
    if not order_determined or "unknown" in families:
        return "unknown"
    return ">".join(families)


def hop_count_bucket(n):
    return "2-hop" if n == 2 else "3-hop" if n == 3 else ">3-hop"


def mirror(path, out):
    data = Path(path).read_bytes()
    digest = hashlib.sha256(data).hexdigest()
    if digest != SEPT28_CSV_SHA256:
        sys.exit(f"input sha256 {digest} != pinned {SEPT28_CSV_SHA256}")
    fams, _ = family_table()
    pools, _ = pool_table()
    rows = list(csv.DictReader(data.decode().splitlines()))
    results = []
    for r in rows:
        pl = r["ordered_pools"].split(";")
        pj = r["ordered_projects"].split(";")
        n = int(r["hop_count"])
        assert len(pl) == len(pj) == n
        legs = [new_leg_family(p, pools, fams) for p in pl]
        results.append({
            "tx_hash": r["tx_hash"],
            "block_number": int(r["block_number"]),
            "hop_count": n,
            "ordered_pools": r["ordered_pools"],
            "ordered_projects": r["ordered_projects"],
            "base_hop_mix": base_hop_mix(pj, n),
            "hop_count_bucket": hop_count_bucket(n),
            "ordered_families": ";".join(legs),
            "hop_mix": new_hop_mix(legs),
            "leg_venues": ";".join(fams[pools[p]][1] if pools.get(p) in fams else "unmapped" for p in pl),
        })

    # Examples: equal hop count, different family mix (first tx per mix, by block).
    by_bucket = collections.defaultdict(dict)
    for x in sorted(results, key=lambda x: (x["block_number"], x["tx_hash"])):
        if x["hop_mix"] != "unknown":
            by_bucket[x["hop_count_bucket"]].setdefault(x["hop_mix"], x)
    # Same Dune project sequence, different family (Moe V1 classic vs Moe LB).
    same_project = collections.defaultdict(dict)
    for x in results:
        if x["hop_mix"] != "unknown":
            same_project[x["ordered_projects"]].setdefault(x["hop_mix"], x)
    same_project = {k: v for k, v in same_project.items() if len(v) > 1}
    unmappable = [x for x in results if x["hop_mix"] == "unknown"]
    unmapped_pools = sorted({(p, pj) for x in unmappable
                             for p, pj, f in zip(x["ordered_pools"].split(";"),
                                                 x["ordered_projects"].split(";"),
                                                 x["ordered_families"].split(";")) if f == "unknown"})
    base_two_hop = collections.Counter(x["base_hop_mix"] for x in results if x["hop_count"] == 2)
    report = {
        "what": "OFFLINE MIRROR of 00_qualified_arbs.sql hop_mix logic over a frozen Dune export; NOT a Dune execution",
        "input": {"file": "arb_detail_feed_20260928.csv (external; Dune query 8781229, UTC day 2026-09-28)",
                  "sha256": digest, "rows": len(rows)},
        "mapping_tables": {"factories": len(fams), "pools": len(pools)},
        "limits": [
            "Export carries legs already ordered by evt_index but not evt_index itself: the order_determined "
            "guard (null/duplicate evt_index -> unknown) is not exercised by this mirror.",
            "Base qualification unchanged; the export's rows are the qualified population as published (v4).",
        ],
        "base_hop_mix_two_hop_values": dict(base_two_hop),
        "hop_mix_counts": dict(collections.Counter(x["hop_mix"] for x in results).most_common()),
        "hop_count_bucket_counts": dict(collections.Counter(x["hop_count_bucket"] for x in results)),
        "leg_counts_by_dune_project_and_mapped_venue": {
            f"{pj} -> {v}": n for (pj, v), n in sorted(collections.Counter(
                (pj, v) for x in results
                for pj, v in zip(x["ordered_projects"].split(";"), x["leg_venues"].split(";"))).items())},
        "unknown_tx": len(unmappable),
        "mapped_tx": len(results) - len(unmappable),
        "examples_equal_hop_different_family": {
            b: {m: {k: x[k] for k in ("tx_hash", "ordered_projects", "ordered_pools", "leg_venues",
                                         "base_hop_mix", "hop_mix")} for m, x in sorted(v.items())}
            for b, v in sorted(by_bucket.items()) if b in ("2-hop", "3-hop")
        },
        "examples_same_projects_different_family": {
            k: {m: {kk: x[kk] for kk in ("tx_hash", "ordered_pools", "leg_venues", "hop_mix")}
                for m, x in sorted(v.items())}
            for k, v in sorted(same_project.items()) if k.count(";") <= 2
        },
        "example_unmappable": {k: unmappable[0][k] for k in ("tx_hash", "ordered_projects", "ordered_pools",
                                                            "ordered_families", "base_hop_mix", "hop_mix")}
        if unmappable else None,
        "unmapped_pools": [{"pool": p, "dune_project": pj} for p, pj in unmapped_pools],
    }
    text = json.dumps(report, indent=1, sort_keys=False) + "\n"
    if out:
        Path(out).write_text(text)
    print(text)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--emit-pool-values", action="store_true")
    ap.add_argument("--check", action="store_true")
    ap.add_argument("--mirror")
    ap.add_argument("--out")
    a = ap.parse_args()
    if a.emit_pool_values:
        sys.stdout.write(emit_pool_values(family_table()[0]))
    if a.check:
        check()
    if a.mirror:
        mirror(a.mirror, a.out)


if __name__ == "__main__":
    main()
