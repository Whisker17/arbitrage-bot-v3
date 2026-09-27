#!/usr/bin/env bash
# WHI-1527: pinned offline replay baseline for DISCOVERY latency across commits.
#
#   scripts/replay_baseline.sh schedule   # ledger + block_summary log -> schedule.json
#   scripts/replay_baseline.sh capture    # one-off corpus capture (this checkout; archive RPC)
#   scripts/replay_baseline.sh arms       # detached worktrees + identical harness + --locked release builds
#   scripts/replay_baseline.sh run        # warmup + balanced interleaved repeats (no RPC)
#   scripts/replay_baseline.sh analyze    # per-arm tables, paired deltas, fidelity -> evidence dir
#   scripts/replay_baseline.sh clean      # remove only the worktrees/targets this script created
#
# Everything the script owns lives under $WORK (default /tmp/whi1527). The harness
# diff applied to historical worktrees is staged there too, never in the tracked tree.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="${WORK:-/tmp/whi1527}"
ARMS="${ARMS:-1bdac47 3cc962f fe4a574}"
REPEATS="${REPEATS:-5}"
LEDGER="${LEDGER:-$WORK/host/ledger.jsonl}"
SUMMARY_LOG="${SUMMARY_LOG:?set SUMMARY_LOG to the frozen rc2 signerless.log (block_summary lines)}"
WINDOW_LO="${WINDOW_LO:-101173341}"
WINDOW_HI="${WINDOW_HI:-101178398}"
BOOTSTRAP="${BOOTSTRAP:-101173340}"
CORPUS_DIR="${CORPUS_DIR:-$WORK/corpus}"
EVIDENCE="${EVIDENCE:-$REPO/evidence/replay/whi-1527}"
# rc2 discovery inputs (see STATUS.md "Config provenance").
MAX_INPUT_WEI="${MAX_INPUT_WEI:-10000000000000000000}"
MIN_PROFIT_WEI="${MIN_PROFIT_WEI:-10000000000000000}"
PRIORITY_FEE_WEI="${PRIORITY_FEE_WEI:-100000}"
BLOCK_GAS_RESERVE="${BLOCK_GAS_RESERVE:-1}"
PROFILE_DIGEST="${PROFILE_DIGEST:-0x3d3244e391f4bfd33298435a51920dfea53027f1b32c2d05f4807e801a3df412}"

sha() { shasum -a 256 "$1" | cut -d' ' -f1; }

stanza() {
  printf '\n[[example]]\nname = "discovery_replay"\npath = "examples/discovery_replay.rs"\n'
}

cmd_schedule() {
  mkdir -p "$WORK"
  python3 - "$LEDGER" "$SUMMARY_LOG" "$WORK/schedule.json" "$WINDOW_LO" "$WINDOW_HI" "$BOOTSTRAP" <<'PY'
import json, re, sys
from collections import Counter
ledger_path, log_path, out_path = sys.argv[1:4]
lo, hi, boot = int(sys.argv[4]), int(sys.argv[5]), int(sys.argv[6])
rows = {}
for line in open(ledger_path):
    r = json.loads(line)
    if r.get("row_type") == "observation" and lo <= r["snapshot_id"]["block_number"] <= hi:
        rows[r["snapshot_id"]["block_number"]] = r
ansi = re.compile(r"\x1b\[[0-9;]*m")
bs = re.compile(r"block_summary block=(\d+) .*?skip_reason=\"([^\"]+)\"")
rb = re.compile(r"pre-watch tip re-baseline complete from=(\d+) to=(\d+)")
events, seen = [], set()
for raw in open(log_path, errors="replace"):
    line = ansi.sub("", raw)
    m = rb.search(line)
    if m:
        to = int(m.group(2))
        for n in sorted(rows):  # the one-shot startup observation (no discovery object)
            if n < to and "discovery" not in rows[n] and n not in seen:
                events.append({"block": n, "kind": "startup_row",
                               "ledger_hash": rows[n]["snapshot_id"]["block_hash"]})
                seen.add(n)
        events.append({"block": to, "kind": "rebaseline"})
        continue
    if "re-baselining at observed tip" in line:
        sys.exit("mid-run re-baseline present: the schedule model does not cover it")
    m = bs.search(line)
    if not m:
        continue
    n, skip = int(m.group(1)), m.group(2)
    if not lo <= n <= hi:
        continue
    if skip == "-":
        r = rows.get(n)
        if r is None or "discovery" not in r:
            sys.exit(f"processed block {n} has no ledger discovery row")
        d = r["discovery"]
        events.append({"block": n, "kind": "processed", "ledger_hash": r["snapshot_id"]["block_hash"],
                       "ledger_scope": d["scope"], "ledger_dirty": d.get("dirty_pools", [])})
    else:
        events.append({"block": n, "kind": skip})
    seen.add(n)
missing = [n for n in rows if n not in seen]
if missing:
    sys.exit(f"ledger rows without schedule events: {missing[:10]}")
json.dump({"bootstrap_block": boot, "window": [lo, hi], "events": events}, open(out_path, "w"))
print(json.dumps({"events": len(events), "kinds": Counter(e["kind"] for e in events)}))
PY
}

cmd_capture() {
  mkdir -p "$CORPUS_DIR"
  (cd "$REPO" && cargo build --locked --release --example replay_capture)
  local bin="$REPO/target/release/examples/replay_capture"
  cp "$REPO/config/gas_profiles/mantle_mainnet_v1.json" "$CORPUS_DIR/"
  sha "$bin" > "$CORPUS_DIR/capture_binary.sha256"
  "$bin" --schedule "$WORK/schedule.json" --out "$CORPUS_DIR" \
    --max-input-wei "$MAX_INPUT_WEI" --min-profit-wei "$MIN_PROFIT_WEI" \
    --priority-fee-wei "$PRIORITY_FEE_WEI" --block-gas-reserve "$BLOCK_GAS_RESERVE" \
    --profile-digest "$PROFILE_DIGEST" --capture-commit "$(git -C "$REPO" rev-parse HEAD)" \
    ${MAX_EVENTS:+--max-events "$MAX_EVENTS"} > "$CORPUS_DIR/capture.stdout.json" 2> "$CORPUS_DIR/capture.log"
  sha "$CORPUS_DIR/corpus.jsonl" > "$CORPUS_DIR/corpus.sha256"
}

cmd_arms() {
  local harness="$WORK/harness"
  mkdir -p "$harness"
  cp "$REPO/examples/discovery_replay.rs" "$harness/discovery_replay.rs"
  stanza > "$harness/stanza.toml"
  : > "$WORK/arms.tsv"
  for s in $ARMS; do
    local wt="$WORK/wt-$s"
    [ -d "$wt" ] || git -C "$REPO" worktree add --detach "$wt" "$s" >/dev/null
    git -C "$wt" checkout -q --detach "$s"
    git -C "$wt" checkout -q -- Cargo.toml
    cp "$harness/discovery_replay.rs" "$wt/examples/discovery_replay.rs"
    cat "$harness/stanza.toml" >> "$wt/Cargo.toml"
    git -C "$wt" add -N examples/discovery_replay.rs
    git -C "$wt" diff > "$WORK/harness-$s.diff"
    git -C "$wt" reset -q examples/discovery_replay.rs
    (cd "$wt" && rustc --version > "$WORK/rustc-$s.txt" && \
      CARGO_TARGET_DIR="$WORK/target-$s" cargo build --locked --release --example discovery_replay)
    local bin="$WORK/target-$s/release/examples/discovery_replay"
    printf '%s\t%s\t%s\t%s\t%s\t%s\n' "$s" "$(git -C "$wt" rev-parse HEAD)" "$(sha "$bin")" \
      "$(sha "$WORK/harness-$s.diff")" "$(sha "$harness/discovery_replay.rs")" "$(cat "$WORK/rustc-$s.txt")" >> "$WORK/arms.tsv"
  done
  cat "$WORK/arms.tsv"
}

cmd_run() {
  local out="$WORK/runs"
  mkdir -p "$out"
  read -r -a arms <<< "$ARMS"
  local n=${#arms[@]}
  : > "$out/order.tsv"
  one() { # arm tag
    local bin="$WORK/target-$1/release/examples/discovery_replay"
    local t0 t1
    t0=$(python3 -c 'import time;print(time.time())')
    "$bin" --corpus "$CORPUS_DIR/corpus.jsonl" --profile "$CORPUS_DIR/mantle_mainnet_v1.json" \
      --out "$out/$1.$2.jsonl" --label "$1.$2"
    t1=$(python3 -c 'import time;print(time.time())')
    printf '%s\t%s\t%s\t%s\n' "$1" "$2" "$t0" "$t1" >> "$out/order.tsv"
  }
  for s in "${arms[@]}"; do one "$s" warmup; done
  # Balanced Latin rotation: round r starts at arm r mod n.
  for ((r = 0; r < REPEATS; r++)); do
    for ((k = 0; k < n; k++)); do one "${arms[(r + k) % n]}" "rep$r"; done
  done
  { sysctl -n machdep.cpu.brand_string hw.ncpu hw.memsize; sw_vers; uptime; pmset -g batt | head -2; } > "$out/host.txt" 2>&1 || true
}

cmd_analyze() {
  mkdir -p "$EVIDENCE"
  REPO="$REPO" WORK="$WORK" CORPUS_DIR="$CORPUS_DIR" EVIDENCE="$EVIDENCE" LEDGER="$LEDGER" ARMS="$ARMS" \
    SUMMARY_LOG="$SUMMARY_LOG" WINDOW_LO="$WINDOW_LO" WINDOW_HI="$WINDOW_HI" \
    python3 - <<'PY'
import json, os, re, statistics, hashlib
from collections import Counter, defaultdict

WORK = os.environ.get("WORK", "/tmp/whi1527")
CORPUS_DIR = os.environ.get("CORPUS_DIR", WORK + "/corpus")
EVIDENCE = os.environ["EVIDENCE"]
LEDGER = os.environ["LEDGER"]
SUMMARY_LOG = os.environ["SUMMARY_LOG"]
ARMS = os.environ.get("ARMS", "1bdac47 3cc962f fe4a574").split()
BEFORE, AFTER, DEPLOYED = ARMS[0], ARMS[1], ARMS[-1]
RUNS = os.path.join(WORK, "runs")
STAGE_BUCKETS = [0.0001, 0.00025, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.0, 5.0]
PCTS = (50, 90, 95, 99)
OPT_MIN_N = 30  # predeclared reporting convention, not a sufficiency claim

def sha(p):
    h = hashlib.sha256()
    with open(p, "rb") as f:
        for c in iter(lambda: f.read(1 << 20), b""):
            h.update(c)
    return h.hexdigest()

def nearest_rank(xs, p):
    s = sorted(xs)
    if not s:
        return None
    k = max(1, -(-p * len(s) // 100))  # ceil(p*n/100)
    return s[k - 1]

def bucket_interp(xs, p):
    """Prometheus histogram_quantile over STAGE_BUCKETS (linear within bucket)."""
    if not xs:
        return None
    counts = [sum(1 for x in xs if x <= b) for b in STAGE_BUCKETS] + [len(xs)]
    rank = p / 100 * len(xs)
    prev_b, prev_c = 0.0, 0
    for b, c in zip(STAGE_BUCKETS + [float("inf")], counts):
        if c >= rank:
            if b == float("inf"):
                return STAGE_BUCKETS[-1]
            return prev_b + (b - prev_b) * ((rank - prev_c) / max(c - prev_c, 1e-12))
        prev_b, prev_c = b, c

def ms(v):
    return None if v is None else round(v * 1000, 4)

def summary(xs):
    return {"n": len(xs), **{f"p{p}_ms": ms(nearest_rank(xs, p)) for p in PCTS},
            "mean_ms": ms(statistics.fmean(xs)) if xs else None,
            "bucket_interp": {f"p{p}_ms": ms(bucket_interp(xs, p)) for p in PCTS}}

# ---------- load runs ----------
runs = defaultdict(dict)  # arm -> tag -> [pass records]
for fn in sorted(os.listdir(RUNS)):
    m = re.match(r"(\w+)\.(warmup|rep\d+)\.jsonl$", fn)
    if not m:
        continue
    arm, tag = m.groups()
    runs[arm][tag] = [json.loads(l) for l in open(os.path.join(RUNS, fn))]
reps = {a: sorted(t for t in runs[a] if t != "warmup") for a in ARMS}
npass = len(runs[ARMS[0]][reps[ARMS[0]][0]])
for a in ARMS:
    for t in runs[a]:
        assert len(runs[a][t]) == npass, (a, t)

COUNTERS = ("cycles_total", "cycles_optimized", "dirty_pools", "paths_quoted", "amm_quotes",
            "gas_rescores", "scope", "rejects", "liveness_alarm", "fee_resolution_failures_debug")
def counters(rec):
    s = rec["stats"]
    return {k: s[k] for k in COUNTERS} | {"opportunities": rec["opportunities"],
                                         "optimize_n": len(rec["optimize_s"]),
                                         "discovery_n": len(rec["discovery_s"])}

# ---------- determinism (all runs incl. warmup) ----------
determinism = {}
for a in ARMS:
    base = [counters(r) for r in runs[a]["warmup"]]
    diffs = 0
    for t, recs in runs[a].items():
        diffs += sum(1 for i, r in enumerate(recs) if counters(r) != base[i])
    determinism[a] = {"runs": len(runs[a]), "passes": npass, "passes_differing_from_warmup": diffs}

ref = {a: runs[a]["warmup"] for a in ARMS}

# ---------- input-workload check ----------
workload = {}
for a in ARMS[1:]:
    mism = [i for i in range(npass) if ref[a][i]["stats"]["cycles_optimized"] != ref[ARMS[0]][i]["stats"]["cycles_optimized"]]
    workload[f"{ARMS[0]}_vs_{a}"] = {"passes": npass, "cycles_optimized_mismatch": len(mism),
                                      "first_mismatches": [ref[a][i]["block"] for i in mism[:10]]}
    for k in ("cycles_total", "dirty_pools", "scope"):
        workload[f"{ARMS[0]}_vs_{a}"][f"{k}_mismatch"] = sum(
            1 for i in range(npass) if ref[a][i]["stats"][k] != ref[ARMS[0]][i]["stats"][k])

# ---------- per-arm counter totals ----------
def totals(recs):
    t = Counter()
    for r in recs:
        s = r["stats"]
        for k in ("cycles_optimized", "paths_quoted", "amm_quotes", "gas_rescores"):
            t[k] += s[k]
        for k, v in s["rejects"].items():
            t["reject_" + k] += v
        t["fee_resolution_failures_debug"] += s["fee_resolution_failures_debug"] or 0
        t["optimize_samples"] += len(r["optimize_s"])
        t["opportunities"] += r["opportunities"]
        t["passes_evaluated"] += s["cycles_optimized"] > 0
        t["passes_quoted"] += s["paths_quoted"] > 0
        t["liveness_alarm_passes"] += bool(s["liveness_alarm"])
    return dict(t)
counter_totals = {a: totals(ref[a]) for a in ARMS}

# per-pass counter differences between arms (reported, never patched)
pair_diffs = {}
for a, b in ((BEFORE, AFTER), (AFTER, DEPLOYED)):
    d = Counter()
    for i in range(npass):
        x, y = ref[a][i]["stats"], ref[b][i]["stats"]
        for k in ("paths_quoted", "amm_quotes"):
            d[k + "_passes_differ"] += x[k] != y[k]
        d["rejects_passes_differ"] += x["rejects"] != y["rejects"]
    pair_diffs[f"{a}_vs_{b}"] = dict(d)

# ---------- DISCOVERY ----------
def per_pass_median(a, i, key="discovery_s"):
    vals = [runs[a][t][i][key][0] for t in reps[a] if runs[a][t][i][key]]
    return statistics.median(vals) if vals else None

evaluated = [ref[BEFORE][i]["stats"]["cycles_optimized"] > 0 for i in range(npass)]
scope_of = [ref[BEFORE][i]["stats"]["scope"] for i in range(npass)]
def selection(sel_scope, only_eval):
    return [i for i in range(npass) if (sel_scope == "all" or scope_of[i] == sel_scope) and (evaluated[i] or not only_eval)]

discovery = {}
for a in ARMS:
    discovery[a] = {}
    for sc in ("all", "full", "touched"):
        for ev in (False, True):
            idx = selection(sc, ev)
            uniq = [per_pass_median(a, i) for i in idx]
            uniq = [u for u in uniq if u is not None]
            pooled = [runs[a][t][i]["discovery_s"][0] for t in reps[a] for i in idx if runs[a][t][i]["discovery_s"]]
            discovery[a][f"{sc}/{'evaluated' if ev else 'all_passes'}"] = {
                "unique_passes_median_of_repeats": summary(uniq),
                "pooled_repeats": summary(pooled) | {"repeats": len(reps[a])},
            }

def paired(a, b):
    out = {}
    for sc in ("all", "full", "touched"):
        for ev in (False, True):
            idx = selection(sc, ev)
            d, ra, rb = [], [], []
            for i in idx:
                x, y = per_pass_median(a, i), per_pass_median(b, i)
                if x is None or y is None:
                    continue
                d.append(y - x); ra.append(x); rb.append(y)
            key = f"{sc}/{'evaluated' if ev else 'all_passes'}"
            out[key] = {"n_pairs": len(d),
                        **{f"delta_p{p}_ms": ms(nearest_rank(d, p)) for p in PCTS},
                        "delta_median_ms": ms(statistics.median(d)) if d else None,
                        "sum_before_ms": ms(sum(ra)), "sum_after_ms": ms(sum(rb)),
                        "ratio_of_sums": round(sum(rb) / sum(ra), 3) if ra and sum(ra) > 0 else None,
                        "passes_after_slower": sum(1 for x in d if x > 0)}
    return out
paired_deltas = {f"{BEFORE}->{AFTER}": paired(BEFORE, AFTER), f"{AFTER}->{DEPLOYED}": paired(AFTER, DEPLOYED),
                 f"{BEFORE}->{DEPLOYED}": paired(BEFORE, DEPLOYED)}

# ms per amm_quote, quotes > 0 only
ms_per_quote = {}
for a in ARMS:
    per, tot_s, tot_q = [], 0.0, 0
    for i in range(npass):
        q = ref[a][i]["stats"]["amm_quotes"]
        m_ = per_pass_median(a, i)
        if q > 0 and m_ is not None:
            per.append(m_ / q); tot_s += m_; tot_q += q
    ms_per_quote[a] = {"passes_with_quotes": len(per),
                       **{f"p{p}_ms_per_quote": ms(nearest_rank(per, p)) for p in (50, 90, 99)},
                       "aggregate_ms_per_quote": ms(tot_s / tot_q) if tot_q else None}

# ---------- OPTIMIZE ----------
optimize = {}
for a in ARMS:
    samples_unique = []  # per (pass, sample index) median across repeats
    for i in range(npass):
        k = len(ref[a][i]["optimize_s"])
        for j in range(k):
            samples_unique.append(statistics.median(runs[a][t][i]["optimize_s"][j] for t in reps[a]))
    t = counter_totals[a]
    n = len(samples_unique)
    optimize[a] = {"n_unique_samples": n,
                   "outcomes_per_run": {"ok_optimum(=OPTIMIZE samples)": t["optimize_samples"],
                                        "no_optimum": t.get("reject_no_optimum", 0),
                                        "zero_profit": t.get("reject_zero_profit", 0),
                                        "rejected_unknown_route": t.get("reject_unknown_route", 0),
                                        "rejected_unapproved_route": t.get("reject_unapproved_route", 0),
                                        "pool_lookup": t.get("reject_pool_lookup", 0),
                                        "other(incl optimize_error, mixed_sim_error, materialize rejects)": t.get("reject_other", 0)},
                   "latency": summary(samples_unique) if n >= OPT_MIN_N else f"insufficient (n={n})"}

# ---------- fidelity vs rc2 ledger (fe4a574 arm) ----------
ledger = {}
for line in open(LEDGER):
    r = json.loads(line)
    if r.get("row_type") == "observation" and "discovery" in r:
        ledger[r["snapshot_id"]["block_number"]] = r["discovery"]
ansi = re.compile(r"\x1b\[[0-9;]*m")
live_quotes = {}
for raw in open(SUMMARY_LOG, errors="replace"):
    m = re.search(r"block_summary block=(\d+) .*?amm_quotes=(\d+)", ansi.sub("", raw))
    if m:
        live_quotes[int(m.group(1))] = int(m.group(2))
emu = [json.loads(l) for l in open(os.path.join(CORPUS_DIR, "emulation.jsonl"))]
emu_by_block = {e["block"]: e for e in emu if e["kind"] == "processed"}
prev_kind = {}
last = None
for e in emu:
    if e["kind"] == "processed":
        prev_kind[e["block"]] = last
    last = e["kind"]
fid_rows, fid_counts = [], Counter()
for i in range(npass):
    r = ref[DEPLOYED][i]; n = r["block"]; s = r["stats"]; L = ledger.get(n)
    if L is None:
        fid_counts["no_ledger_row"] += 1; continue
    fields = {"scope": (s["scope"], L.get("scope")), "cycles_optimized": (s["cycles_optimized"], L.get("cycles_optimized")),
              "cycles_total": (s["cycles_total"], L.get("cycles_total")), "paths_quoted": (s["paths_quoted"], L.get("paths_quoted")),
              "rejects": (s["rejects"], L.get("rejects")), "fee_resolution_failures": (s["fee_resolution_failures_debug"], L.get("fee_resolution_failures")),
              "amm_quotes(log)": (s["amm_quotes"], live_quotes.get(n))}
    bad = [k for k, (x, y) in fields.items() if x != y]
    if not bad:
        fid_counts["match_all"] += 1; continue
    fid_counts["mismatch"] += 1
    e = emu_by_block.get(n, {})
    fid_rows.append({"pass": i, "block": n, "fields": bad, "replay": {k: fields[k][0] for k in bad},
                     "ledger": {k: fields[k][1] for k in bad}, "inputs_dirty_match": e.get("ledger_dirty_match"),
                     "gap_range": e.get("gap_range"), "prev_event": prev_kind.get(n), "scope": s["scope"]})

res = {
    "corpus": {"sha256": sha(os.path.join(CORPUS_DIR, "corpus.jsonl")), "passes": npass},
    "arms": ARMS, "repeats": {a: len(reps[a]) for a in ARMS},
    "percentile_method": "nearest-rank on raw samples; bucket_interp = Prometheus-style linear interpolation over STAGE_BUCKETS",
    "determinism": determinism, "input_workload_check": workload, "counter_totals_per_run": counter_totals,
    "per_pass_counter_differences": pair_diffs, "discovery": discovery, "paired_deltas": paired_deltas,
    "ms_per_amm_quote": ms_per_quote, "optimize": optimize,
    "fidelity_vs_rc2_ledger": {"counts": dict(fid_counts), "mismatches": fid_rows},
}
os.makedirs(EVIDENCE, exist_ok=True)
json.dump(res, open(os.path.join(EVIDENCE, "results.json"), "w"), indent=1)
print(json.dumps({k: res[k] for k in ("corpus", "determinism", "input_workload_check", "counter_totals_per_run")}, indent=1))
print(json.dumps(res["fidelity_vs_rc2_ledger"]["counts"]))

# ---------- compact per-pass table (evaluated passes) + repeat spread ----------
rows, spread = [], {a: [] for a in ARMS}
for i in range(npass):
    if not evaluated[i]:
        continue
    row = {"pass": i, "block": ref[BEFORE][i]["block"], "scope": scope_of[i],
           "cycles_optimized": ref[BEFORE][i]["stats"]["cycles_optimized"]}
    for a in ARMS:
        s = ref[a][i]["stats"]
        v = [runs[a][t][i]["discovery_s"][0] for t in reps[a]]
        spread[a].append((max(v) - min(v)) / statistics.median(v))
        row[a] = {"discovery_ms_median": ms(statistics.median(v)), "paths_quoted": s["paths_quoted"],
                  "amm_quotes": s["amm_quotes"], "rejects": s["rejects"], "optimize_n": len(ref[a][i]["optimize_s"])}
    rows.append(row)
json.dump({"note": "evaluated passes (cycles_optimized > 0); DISCOVERY = median over the timed repeats", "passes": rows},
          open(os.path.join(EVIDENCE, "per_pass_evaluated.json"), "w"), separators=(",", ":"))
res["repeat_spread_evaluated"] = {a: {"median_rel_range": round(statistics.median(v), 3),
                                      "p90_rel_range": round(nearest_rank(v, 90), 3)} for a, v in spread.items()}
json.dump(res, open(os.path.join(EVIDENCE, "results.json"), "w"), indent=1)
print(json.dumps(res["repeat_spread_evaluated"]))

# ---------- manifest ----------
REPO = os.environ["REPO"]
arms_rows = [l.rstrip("\n").split("\t") for l in open(os.path.join(WORK, "arms.tsv"))]
cap = json.load(open(os.path.join(CORPUS_DIR, "capture.json")))
meta = json.loads(open(os.path.join(CORPUS_DIR, "corpus.jsonl")).readline())
order = [l.rstrip("\n").split("\t") for l in open(os.path.join(RUNS, "order.tsv"))]
window_lines = [l for l in open(LEDGER) if json.loads(l).get("row_type") == "run_header" or
                (json.loads(l).get("snapshot_id", {}).get("block_number", 0) <= int(os.environ["WINDOW_HI"]) and json.loads(l).get("row_type") == "observation")]
manifest = {
    "issue": "WHI-1527",
    "schema": "whi-1527/replay-manifest/v1",
    "window": [int(os.environ["WINDOW_LO"]), int(os.environ["WINDOW_HI"])],
    "bootstrap": meta["bootstrap"],
    "corpus": {"sha256": sha(os.path.join(CORPUS_DIR, "corpus.jsonl")),
               "bytes": os.path.getsize(os.path.join(CORPUS_DIR, "corpus.jsonl")),
               "schema": meta["schema"], "passes": cap["passes"], "stored": "outside git (re-derivable: schedule + archive RPC + capture source)"},
    "capture": {"commit_base": "fe4a574f7f4a60a27cd059f1c585435c554c2ae0",
                "capture_commit_field": meta["capture_commit"],
                "binary_sha256": open(os.path.join(CORPUS_DIR, "capture_binary.sha256")).read().split()[0],
                "rpc_endpoint_fingerprint": {"host": "rpc.mantle.xyz", "scheme": "https", "api_key": "none (public endpoint)"},
                "report": cap},
    "schedule": {"sha256": sha(os.path.join(WORK, "schedule.json")),
                 "sources": {"rc2_ledger_copy_sha256": sha(LEDGER),
                             "rc2_ledger_window_rows_sha256": hashlib.sha256("".join(window_lines).encode()).hexdigest(),
                             "block_summary_log_sha256": sha(SUMMARY_LOG),
                             "block_summary_log_origin": "deployment issue's local read-only freeze of the rc2 signerless.log (cut 101179163)"}},
    "universe": {"fingerprint": meta["universe_fingerprint"], "pool_count": meta["universe_pool_count"],
                 "csv_sha256": sha(os.path.join(REPO, "data/pool_universe.csv")),
                 "meta_sha256": sha(os.path.join(REPO, "data/pool_universe.meta.json"))},
    "gas_profile": {"digest": meta["profile_digest"], "file_sha256": sha(os.path.join(CORPUS_DIR, "mantle_mainnet_v1.json"))},
    "discovery_config": meta["discovery_config"],
    "host_inputs_read_only": {n: sha(os.path.join(WORK, "host", n)) for n in ("run_plan.json", "capital_evidence.json", "ledger.jsonl")},
    "arms": [{"sha": r[0], "commit": r[1], "binary_sha256": r[2], "harness_diff_sha256": r[3],
              "harness_file_sha256": r[4], "rustc": r[5]} for r in arms_rows],
    "runs": {"order": [{"arm": o[0], "tag": o[1], "start_unix": float(o[2]), "end_unix": float(o[3])} for o in order],
             "host": open(os.path.join(RUNS, "host.txt")).read().splitlines()},
}
json.dump(manifest, open(os.path.join(EVIDENCE, "manifest.json"), "w"), indent=1)
print("manifest written")
PY
}

cmd_clean() {
  for s in $ARMS; do
    [ -d "$WORK/wt-$s" ] && git -C "$REPO" worktree remove --force "$WORK/wt-$s"
    rm -rf "$WORK/target-$s"
  done
  git -C "$REPO" worktree prune
}

case "${1:-}" in
  schedule) cmd_schedule ;;
  capture) cmd_capture ;;
  arms) cmd_arms ;;
  run) cmd_run ;;
  analyze) cmd_analyze ;;
  clean) cmd_clean ;;
  *) sed -n '2,12p' "$0"; exit 2 ;;
esac
