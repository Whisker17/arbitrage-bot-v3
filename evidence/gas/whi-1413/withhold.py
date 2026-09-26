"""WHI-1413: apply the WHI-1422 approval fences to the Moe V1 campaign's output.

Deterministic, idempotent edit of config/gas_profiles/pinned/generator_config.json.
Run from the repo root after the campaign finalize, then regenerate:

    python3 evidence/gas/whi-1413/withhold.py
    cargo run --locked --example generate_gas_profile -- --print-digest

Rules, unchanged from WHI-1422 fix round 1 (evidence/gas/whi-1422/REPORT.md):

- PR108-F2 / DI-50: a class that is newly approved here and has a V3 hop is forced
  Unsupported. RouteKey has no factory axis, so it would also price non-Agni V3
  pools that were never measured. This does not change the V3 factory policy; it
  only refuses to widen it.
- PR108-F3: a newly approved class must rest only on `v2_boost` campaign samples,
  a lever that leaves every V3/Moe hop on canonical state.

Rule added in fix round 1 (PR109-F1, DI-54), applied to **every** class that is
Approved (base classes included) or already withheld under it:

- PR109-F1: RouteKey has no venue axis, so a class with a V2 hop also prices every
  admitted V2 venue (factory) that occurs on the committed universe's WMNT cycles
  of its topology. Each such venue needs >= 2 fork samples of the **exact** class
  (same topology and crossing buckets) touching one of its pools. Samples from
  another bucket or another venue never count (no extrapolation). A class that
  fails is forced Unsupported. `tests/gas_profile_fork_provenance.rs`
  `committed_approved_classes_are_measured_on_every_v2_venue_they_price` enforces
  the same rule (plus every such sample <= the class's gas limit) in CI.

Classes Approved on the base profile (dev de735db, content_digest 0xa6bc9dac…3d8b)
keep their status unless PR109-F1 withholds them.
"""
import collections, csv, json, re

CONFIG = "config/gas_profiles/pinned/generator_config.json"
PROFILE = "config/gas_profiles/mantle_mainnet_v1.json"
SAMPLES = "config/gas_profiles/pinned/samples.jsonl"
BASE_APPROVED = {
    "h2:v2+v2", "h2:v2+v3:ticks=0", "h2:v2+moe:bins=0",
    "h3:v2+moe+moe:bins=0", "h3:moe+moe+v2:bins=0",
}
F2 = (
    "withheld (WHI-1413 under PR108-F2, DI-50): the Moe V1 campaign qualified this class "
    "numerically, but it has a V3 hop and RouteKey/runtime pricing has no factory axis, so "
    "approving it would also price the universe's non-Agni V3 pools, which were never measured "
    "and may not be executable (the executor implements only agniSwapCallback). Samples stay "
    "visible as stats (evidence/gas/whi-1413/REPORT.md)"
)
F3 = (
    "withheld (WHI-1413 under PR108-F3): its qualification samples include inflate/displace "
    "lever samples, which rewrite V3/Moe pool state (evidence/gas/whi-1413/REPORT.md)"
)
PREFIXES = ("withheld (PR108-", "withheld (WHI-1413 ")
F1_PREFIX = "withheld (WHI-1413 under PR109-F1, DI-54)"
UNIVERSE = "data/pool_universe.csv"
WMNT = "0x78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8"
KIND = {"agni-v2": "v2", "agni-v3": "v3", "moe": "moe"}
# service::v2_venues (admitted UniV2-family venues).
V2_VENUE_LABEL = {
    "0xe5020961fa51ffd3662cdf307def18f9a87cce7c": "fusionx-v2",
    "0x5bef015ca9424a7c07b68490616a4c1f094bedec": "moe-v1",
}
MIN_VENUE_SAMPLES = 2


def f1_reason(gaps):
    detail = "; ".join(
        "%s: %d exact-class fork samples, on %d of the universe's cycles of this topology" % g
        for g in gaps
    )
    return (
        F1_PREFIX + ": RouteKey/runtime pricing has no venue axis, so this class also prices "
        "every admitted V2 venue on the committed universe's cycles of its topology, and a venue "
        "below %d fork samples of this exact class (same topology and crossing buckets) is not "
        "shown to fit the limit. Samples from other buckets or other venues are not extrapolated. "
        "Gap: %s. Samples stay visible as stats (evidence/gas/whi-1413/REPORT.md)"
    ) % (MIN_VENUE_SAMPLES, detail)


def universe_venues_by_topology():
    """V2 factories on the universe's WMNT cycles (2..3 hops), per topology.

    Same enumeration rules as evidence/venues/whi-1413/moe_v1_reach.py and the
    production PathFinder: no immediate same-pool reversal, no repeated
    intermediate token. Returns ({topology tuple: Counter(factory -> cycles)},
    {pool -> factory}, total cycles)."""
    rows = list(csv.DictReader(open(UNIVERSE)))
    factory = {r["pool"].lower(): r["factory"].lower() for r in rows}
    kind = {r["pool"].lower(): KIND[r["protocol"]] for r in rows}
    adj = collections.defaultdict(list)
    for r in rows:
        t0, t1, pool = r["token0"].lower(), r["token1"].lower(), r["pool"].lower()
        adj[t0].append((t1, pool))
        adj[t1].append((t0, pool))
    cycles = []

    def dfs(tok, path, seen):
        if len(path) >= 3:
            return
        for nxt, pool in adj[tok]:
            if path and path[-1] == pool:
                continue
            if nxt == WMNT:
                if len(path) + 1 >= 2:
                    cycles.append(path + [pool])
                continue
            if nxt in seen:
                continue
            dfs(nxt, path + [pool], seen | {nxt})

    dfs(WMNT, [], {WMNT})
    by_topo = collections.defaultdict(collections.Counter)
    for c in cycles:
        topo = tuple(kind[p] for p in c)
        for f in {factory[p] for p in c if kind[p] == "v2"}:
            by_topo[topo][f] += 1
    return by_topo, factory, len(cycles)


def key_string(k):
    s = "h%d:%s" % (k["hop_count"], "+".join(k["protocols"]))
    if "v3_tick_crossings" in k:
        s += ":ticks=" + k["v3_tick_crossings"]
    if "moe_bin_crossings" in k:
        s += ":bins=" + k["moe_bin_crossings"]
    return s


def lever(sample):
    m = re.search(r"^\[whi-14(22|13|13-b)\] .*?lever=(\w+)", sample.get("notes") or "")
    return m.group(2) if m else None


def main():
    prof = json.load(open(PROFILE))
    levers = collections.defaultdict(set)
    fork = collections.defaultdict(list)
    for line in open(SAMPLES):
        s = json.loads(line)
        if s["source"] == "fork_replay" and lever(s):
            levers[key_string(s["route_key"])].add(lever(s))
        if s["source"] == "fork_replay" and s.get("outcome") != "reverted" and s["gas_used"] > 0:
            fork[key_string(s["route_key"])].append(s)
    cfg = json.load(open(CONFIG))
    forced = {key_string(f["route_key"]): f["reason"] for f in cfg["unsupported_route_classes"]}
    venues_by_topo, pool_factory, _ = universe_venues_by_topology()
    reasons = {}
    for p in prof["profiles"]:
        k = key_string(p["route_key"])
        prior = forced.get(k, "")
        if p["status"] != "approved" and not prior.startswith(PREFIXES[1]):
            continue
        if k not in BASE_APPROVED:
            if "v3" in p["route_key"]["protocols"]:
                reasons[k] = F2
                continue
            if levers[k] - {"v2_boost"}:
                reasons[k] = F3
                continue
        if p["status"] != "approved" and not prior.startswith(F1_PREFIX):
            continue
        gaps = []
        for f, n_cycles in sorted(venues_by_topo[tuple(p["route_key"]["protocols"])].items()):
            n = sum(
                1
                for s in fork[k]
                if any(v["protocol"] == "v2" and pool_factory.get(v["pool"].lower()) == f for v in s.get("venues") or [])
            )
            if n < MIN_VENUE_SAMPLES:
                gaps.append((V2_VENUE_LABEL.get(f, f), n, n_cycles))
        if gaps:
            reasons[k] = f1_reason(gaps)
    for k, r in reasons.items():
        assert k not in forced or forced[k].startswith(PREFIXES), (k, forced[k])
        forced[k] = r
    cfg["unsupported_route_classes"] = [
        {"route_key": rk, "reason": forced[key_string(rk)]}
        for rk in cfg["active_route_classes"]
        if key_string(rk) in forced
    ]
    with open(CONFIG, "w") as f:
        f.write(json.dumps(cfg, indent=2, ensure_ascii=False) + "\n")
    for k in sorted(reasons):
        tag = "F2" if reasons[k] == F2 else "F3" if reasons[k] == F3 else "F1"
        print(tag, k, reasons[k][len(F1_PREFIX):] if tag == "F1" else "")
    print("withheld:", len(reasons))


if __name__ == "__main__":
    main()
