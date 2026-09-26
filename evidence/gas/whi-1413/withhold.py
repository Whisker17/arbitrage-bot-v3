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

Classes Approved on the base profile (dev de735db, content_digest 0xa6bc9dac…3d8b)
keep their status. Their Moe V1 exposure is checked separately by moe_v1_gate.py and
by tests/gas_profile_fork_provenance.rs.
"""
import collections, json, re

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
    for line in open(SAMPLES):
        s = json.loads(line)
        if s["source"] == "fork_replay" and lever(s):
            levers[key_string(s["route_key"])].add(lever(s))
    cfg = json.load(open(CONFIG))
    forced = {key_string(f["route_key"]): f["reason"] for f in cfg["unsupported_route_classes"]}
    reasons = {}
    for p in prof["profiles"]:
        k = key_string(p["route_key"])
        if k in BASE_APPROVED:
            continue
        if p["status"] != "approved" and not forced.get(k, "").startswith(PREFIXES[1]):
            continue
        if "v3" in p["route_key"]["protocols"]:
            reasons[k] = F2
        elif levers[k] - {"v2_boost"}:
            reasons[k] = F3
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
        print(("F2 " if reasons[k] == F2 else "F3 ") + k)
    print("withheld:", len(reasons))


if __name__ == "__main__":
    main()
