"""WHI-1422 fix round 1: withhold approvals flagged by review PR108-F2 / PR108-F3.

Deterministic, idempotent edit of config/gas_profiles/pinned/generator_config.json
(the generator input). Run from the repo root, then regenerate the profile:

    python3 evidence/gas/whi-1422/fix1_withhold.py
    cargo run --locked --example generate_gas_profile -- --print-digest

Rules (orchestrator disposition of review-pr108, fix round 1):
- PR108-F2: a class the campaign would newly approve that has a V3 hop is forced
  Unsupported. RouteKey / runtime pricing has no factory axis, so such an approval
  would also price the committed universe's non-Agni V3 pools, which were never
  measured and may not be executable (DI-50). Approvals that already existed on
  the base profile (d2ab7ba) keep their status; their exposure is recorded in DI-50.
- PR108-F3: a class may stay Approved only if every campaign sample in its
  qualification set (train and holdout) ran every V3/Moe hop on unmodified pool
  state, i.e. used the `v2_boost` lever (it only overrides a V2 hop's token
  balance). A class resting on `inflate` / `displace` samples is forced Unsupported.

Candidates are read from the committed profile: classes that are Approved now, or
that already carry one of the reasons below (so a second run changes nothing).
"""
import collections, json, re

CONFIG = "config/gas_profiles/pinned/generator_config.json"
PROFILE = "config/gas_profiles/mantle_mainnet_v1.json"
SAMPLES = "config/gas_profiles/pinned/samples.jsonl"

# Approved on the base profile (dev d2ab7ba, content_digest 0xa18811da...): not
# changed by this PR, so not withheld here (PR108-F2 disposition).
BASE_APPROVED = {"h2:v2+v2", "h2:v2+v3:ticks=0", "h2:v2+moe:bins=0"}

F2 = (
    "withheld (PR108-F2, DI-50): the class has a V3 hop, and RouteKey/runtime pricing has no "
    "factory axis, so approving it would also price the committed universe's 54 of 87 V3 pools "
    "from non-Agni factories, which the campaign never measured and which may not be executable "
    "(the executor implements only agniSwapCallback). Its samples stay visible as stats; "
    "re-qualify once V3 venue eligibility is factory-aware (WHI-1422 fix round 1, "
    "evidence/gas/whi-1422/REPORT.md)"
)
F3 = (
    "; also PR108-F3: its qualification samples rest on the inflate/displace levers, which "
    "rewrite V3/Moe pool state (slot0, liquidity, bin words), and no canonical-state comparison "
    "shows they upper-bound canonical gas"
)
F3_ONLY = (
    "withheld (PR108-F3): its qualification samples rest on the inflate/displace levers, which "
    "rewrite V3/Moe pool state (slot0, liquidity, bin words), and no canonical-state comparison "
    "shows they upper-bound canonical gas (WHI-1422 fix round 1, evidence/gas/whi-1422/REPORT.md)"
)
WITHHELD_PREFIX = "withheld (PR108-"


def key_string(k):
    s = "h%d:%s" % (k["hop_count"], "+".join(k["protocols"]))
    if "v3_tick_crossings" in k:
        s += ":ticks=" + k["v3_tick_crossings"]
    if "moe_bin_crossings" in k:
        s += ":bins=" + k["moe_bin_crossings"]
    return s


def lever(sample):
    m = re.search(r"^\[whi-1422\] .*?lever=(\w+)", sample.get("notes") or "")
    return m.group(1) if m else None  # None: not a campaign sample (WHI-557)


def main():
    prof = json.load(open(PROFILE))
    levers = collections.defaultdict(set)
    for line in open(SAMPLES):
        s = json.loads(line)
        if s["source"] == "fork_replay" and lever(s):
            levers[key_string(s["route_key"])].add(lever(s))

    reasons = {}
    for p in prof["profiles"]:
        k = key_string(p["route_key"])
        candidate = p["status"] == "approved" or p.get("reason", "").startswith(WITHHELD_PREFIX)
        if not candidate or k in BASE_APPROVED:
            continue
        f2 = "v3" in p["route_key"]["protocols"]
        f3 = bool(levers[k] - {"v2_boost"})
        if f2:
            reasons[k] = F2 + (F3 if f3 else "")
        elif f3:
            reasons[k] = F3_ONLY

    cfg = json.load(open(CONFIG))
    forced = {key_string(f["route_key"]): f["reason"] for f in cfg["unsupported_route_classes"]}
    for k, r in reasons.items():
        # Never overwrite a different kind of forced reason (e.g. open-ended).
        assert k not in forced or forced[k].startswith(WITHHELD_PREFIX), (k, forced[k])
        forced[k] = r
    # Keep the generator's active-class order.
    cfg["unsupported_route_classes"] = [
        {"route_key": rk, "reason": forced[key_string(rk)]}
        for rk in cfg["active_route_classes"]
        if key_string(rk) in forced
    ]
    with open(CONFIG, "w") as f:
        f.write(json.dumps(cfg, indent=2, ensure_ascii=False) + "\n")
    for k in sorted(reasons):
        print(("F2+F3 " if F3 in reasons[k] else "F3    " if reasons[k] == F3_ONLY else "F2    ") + k)
    print("withheld:", len(reasons))


if __name__ == "__main__":
    main()
