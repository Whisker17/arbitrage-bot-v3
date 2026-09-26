"""WHI-1422: render the per-class campaign tables from committed artifacts.

Inputs (all committed): config/gas_profiles/mantle_mainnet_v1.json and
evidence/gas/whi-1422/attempts.jsonl. Output: markdown on stdout, pasted into
REPORT.md. Run from the repo root: python3 evidence/gas/whi-1422/make_tables.py
"""
import collections, json

prof = json.load(open("config/gas_profiles/mantle_mainnet_v1.json"))
_raw = [json.loads(l) for l in open("evidence/gas/whi-1422/attempts.jsonl") if l.strip()]
# The file is append-only across --resume; the last record per attempt id wins.
_last = {}
for a in _raw:
    _last[(a["topology"], ">".join(a["cycle"]), a["amount_in_wmnt_milli"], a["lever"])] = a
attempts = list(_last.values())


def key_string(k):
    s = "h%d:%s" % (k["hop_count"], "+".join(k["protocols"]))
    if "v3_tick_crossings" in k:
        s += ":ticks=" + k["v3_tick_crossings"]
    if "moe_bin_crossings" in k:
        s += ":bins=" + k["moe_bin_crossings"]
    return s


by_key = {key_string(p["route_key"]): p for p in prof["profiles"]}
succ = collections.Counter(a["route_key"] for a in attempts if a["outcome"] == "success")
rev = collections.Counter(a["route_key"] for a in attempts if a["outcome"].startswith("reverted"))
levers = collections.defaultdict(set)
for a in attempts:
    if a["outcome"] == "success":
        levers[a["route_key"]].add(a["lever"])

print("### Every measured class (campaign successes or reverts > 0)\n")
print("| route key | campaign n | reverts | total n in profile | holdout n / max | gas_limit | expected | status / reason |")
print("|---|---:|---:|---:|---|---:|---:|---|")
for k in sorted(set(succ) | set(rev)):
    p = by_key.get(k)
    if p is None:
        print(f"| `{k}` | {succ[k]} | {rev[k]} | - | - | - | - | NOT IN PROFILE |")
        continue
    st = p.get("stats") or {}
    h = p.get("holdout") or {}
    hold = f'{h["holdout_count"]} / {h["holdout_max"]} ({"pass" if h.get("all_below_limit") else "FAIL"})' if h else "-"
    status = "**Approved**" if p["status"] == "approved" else "Unsupported: " + p.get("reason", "")[:110]
    print(f'| `{k}` | {succ[k]} | {rev[k]} | {st.get("sample_count", 0)} | {hold} | {p.get("gas_limit", "-")} | {p.get("expected_gas_used", "-")} | {status} |')

print("\n### Profile status summary per topology (all bucket variants)\n")
print("| topology | variants | approved | unsupported (open-ended) | unsupported (other) |")
print("|---|---:|---:|---:|---:|")
topo = collections.defaultdict(lambda: [0, 0, 0, 0])
for k, p in by_key.items():
    t = k.split(":")[0] + ":" + k.split(":")[1]
    row = topo[t]
    row[0] += 1
    if p["status"] == "approved":
        row[1] += 1
    elif p.get("reason", "").startswith("open-ended"):
        row[2] += 1
    else:
        row[3] += 1
for t in sorted(topo, key=lambda x: (len(x), x)):
    r = topo[t]
    print(f"| `{t}` | {r[0]} | {r[1]} | {r[2]} | {r[3]} |")

def category(o):
    for prefix, name in [
        ("success", "success"),
        ("reverted", "reverted (research_revert sample)"),
        ("skipped: lever cannot", "skipped: lever cannot settle"),
        ("skipped: open-ended", "skipped: open-ended bucket not measured"),
        ("skipped: simulate hop", "skipped: local simulation error"),
        ("rpc_error", "rpc_error (unmeasured)"),
        ("not measured", "not measured"),
    ]:
        if o.startswith(prefix):
            return name
    return "other"


outc = collections.Counter(category(a["outcome"]) for a in attempts)
print("\nAttempt outcomes (last record per attempt, %d unique of %d records):" % (len(attempts), len(_raw)))
for k, v in sorted(outc.items(), key=lambda kv: -kv[1]):
    print("- %s: %d" % (k, v))
print("\nUnmeasured attempts:")
for a in attempts:
    if category(a["outcome"]) in ("rpc_error (unmeasured)", "not measured", "other"):
        print("- %s %s mWMNT=%d lever=%s key=%s: %s" % (a["topology"], ">".join(a["cycle"]), a["amount_in_wmnt_milli"], a["lever"], a["route_key"], a["outcome"][:120]))
print("Approved keys:", sum(1 for p in prof["profiles"] if p["status"] == "approved"), "of", len(prof["profiles"]))
