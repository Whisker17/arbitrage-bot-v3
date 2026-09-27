"""WHI-1520: fail-closed withholds after re-qualifying the profile on the 124-pool universe.

Deterministic, idempotent edit of config/gas_profiles/pinned/generator_config.json.
Run from the repo root after the campaign finalize, then regenerate:

    python3 evidence/gas/whi-1520/withhold.py
    cargo run --locked --example generate_gas_profile -- --print-digest

The script only ever adds forced-Unsupported entries. It never approves anything,
and it keeps every existing forced entry (the WHI-1422 / WHI-1413 withholdings).
Rules, applied to the classes that the campaign's regenerated profile approves:

- SCOPE: WHI-1520 regenerates the universe and re-qualifies the classes that were
  already Approved. It does not widen the approved set. A class Approved now but not
  Approved on the base profile (dev 2b13b8f, content_digest 0x87f5d70a…109c) is
  forced Unsupported. Its samples stay visible as stats (a follow-up candidate).
- PR109-F1 (unchanged rule, evidence/gas/whi-1413/withhold.py): every admitted V2
  venue on the committed universe's WMNT cycles of the class's topology needs >= 2
  fork samples of the exact class touching one of its pools. Enforced in CI by
  `committed_approved_classes_are_measured_on_every_v2_venue_they_price`.
- MOE-V1 (unchanged rule): with Moe V1 rows in the universe, every Approved class with
  a V2 hop needs >= 2 fork samples touching one of the universe's Moe V1 pools, even
  when the class has no Moe V1 cycle. Enforced in CI by
  `committed_approved_v2_classes_are_measured_on_the_universes_moe_v1_pools`.
"""
import collections, importlib.util, json, os

HERE = os.path.dirname(os.path.abspath(__file__))
_spec = importlib.util.spec_from_file_location("w1413", os.path.join(HERE, "..", "whi-1413", "withhold.py"))
w = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(w)

BASE_APPROVED = {
    "h2:v2+v2", "h2:v2+v3:ticks=0", "h2:v2+moe:bins=0", "h3:v2+moe+moe:bins=0",
    "h3:moe+moe+v2:bins=0", "h3:v2+v2+v2", "h3:moe+v2+v2:bins=0",
}
MOE_V1 = "0x5bef015ca9424a7c07b68490616a4c1f094bedec"
MIN = 2
SCOPE = (
    "withheld (WHI-1520 scope): the WHI-1520 re-qualification campaign at block 101165208 "
    "qualified this class numerically, but it was not Approved on the base profile and a "
    "universe regeneration does not widen the approved set. Samples stay visible as stats; "
    "approving it is a follow-up decision (evidence/gas/whi-1520/REPORT.md)"
)
F1 = (
    "withheld (WHI-1520 under PR109-F1, DI-54): RouteKey/runtime pricing has no venue axis, so "
    "this class also prices every admitted V2 venue on the committed universe's cycles of its "
    "topology, and a venue below %d fork samples of this exact class (same topology and crossing "
    "buckets) is not shown to fit the limit. Gap in the 124-pool universe at block 101165208: %s. "
    "The WHI-1520 campaigns could not measure it (evidence/gas/whi-1520/REPORT.md)"
)
MV1 = (
    "withheld (WHI-1520 under the Moe V1 guard, DI-54): the committed universe holds Moe V1 "
    "classic rows and this class has a V2 hop, so it needs >= %d fork samples touching one of the "
    "universe's Moe V1 pools; it has %d (the universe has %d cycles of this topology). "
    "Evidence: evidence/gas/whi-1520/REPORT.md"
)


def main():
    prof = json.load(open(w.PROFILE))
    cfg = json.load(open(w.CONFIG))
    forced = {w.key_string(f["route_key"]): f["reason"] for f in cfg["unsupported_route_classes"]}
    fork = collections.defaultdict(list)
    for line in open(w.SAMPLES):
        s = json.loads(line)
        if s["source"] == "fork_replay" and s.get("outcome") != "reverted" and s["gas_used"] > 0:
            fork[w.key_string(s["route_key"])].append(s)
    venues_by_topo, pool_factory, _ = w.universe_venues_by_topology()
    cyc = _topology_cycle_counts()
    moe_v1_pools = {p for p, f in pool_factory.items() if f == MOE_V1}
    reasons = {}
    for p in prof["profiles"]:
        k = w.key_string(p["route_key"])
        if p["status"] != "approved" or k in forced:
            continue
        topo = tuple(p["route_key"]["protocols"])
        if k not in BASE_APPROVED:
            reasons[k] = SCOPE
            continue
        gaps = []
        for f, n_cycles in sorted(venues_by_topo[topo].items()):
            n = sum(
                1
                for s in fork[k]
                if any(v["protocol"] == "v2" and pool_factory.get(v["pool"].lower()) == f for v in s.get("venues") or [])
            )
            if n < MIN:
                gaps.append("%s: %d exact-class fork samples, on %d of the universe's cycles of this topology"
                            % (w.V2_VENUE_LABEL.get(f, f), n, n_cycles))
        if gaps:
            reasons[k] = F1 % (MIN, "; ".join(gaps))
            continue
        if moe_v1_pools and "v2" in topo:
            n = sum(1 for s in fork[k] if any(v["pool"].lower() in moe_v1_pools for v in s.get("venues") or []))
            if n < MIN:
                reasons[k] = MV1 % (MIN, n, cyc["+".join(topo)])
    forced.update(reasons)
    cfg["unsupported_route_classes"] = [
        {"route_key": rk, "reason": forced[w.key_string(rk)]}
        for rk in cfg["active_route_classes"]
        if w.key_string(rk) in forced
    ]
    with open(w.CONFIG, "w") as f:
        f.write(json.dumps(cfg, indent=2, ensure_ascii=False) + "\n")
    for k in sorted(reasons):
        print(reasons[k].split(")")[0] + ")", k)
    print("withheld:", len(reasons))


def _topology_cycle_counts():
    """Universe WMNT cycles per topology label, same enumeration as withhold.py."""
    import csv
    rows = list(csv.DictReader(open(w.UNIVERSE)))
    kind = {r["pool"].lower(): w.KIND[r["protocol"]] for r in rows}
    adj = collections.defaultdict(list)
    for r in rows:
        t0, t1, pool = r["token0"].lower(), r["token1"].lower(), r["pool"].lower()
        adj[t0].append((t1, pool))
        adj[t1].append((t0, pool))
    out = collections.Counter()

    def dfs(tok, path, seen):
        if len(path) >= 3:
            return
        for nxt, pool in adj[tok]:
            if path and path[-1] == pool:
                continue
            if nxt == w.WMNT:
                if path:
                    out["+".join(kind[p] for p in path + [pool])] += 1
                continue
            if nxt in seen:
                continue
            dfs(nxt, path + [pool], seen | {nxt})

    dfs(w.WMNT, [], {w.WMNT})
    return out


if __name__ == "__main__":
    main()
