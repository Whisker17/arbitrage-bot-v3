#!/usr/bin/env bash
# WHI-1572 AC10: paired estimator-off / estimator-on replay on the pinned WHI-1527
# corpus. Same binary, same corpus, same measured profile, same venue labels; the
# only difference between arms is `--estimator`.
#
#   ARTIFACTS=<whi1527-artifacts dir> BIN=<release discovery_replay> \
#     scripts/gas_estimate/paired_replay.sh run       # verify hashes, warmup + balanced repeats
#   scripts/gas_estimate/paired_replay.sh analyze     # tables -> $WORK/summary.json
#
# `run` refuses to start unless the corpus SHA256SUMS, corpus bytes and profile copy
# match the hashes WHI-1527 published (evidence/replay/whi-1527/manifest.json).
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WORK="${WORK:-/tmp/whi1572-replay}"
REPEATS="${REPEATS:-5}"
CORPUS_SHA=46c8420141a6dcd73307bff1df8b2fe706da42c7dc272c81db5ae4531b32fbe9
PROFILE_SHA=3e76b9cd368b5efaeb79877b6ab8ff8b17a86e6609542e689920bbaedd9f95ee
SUMS_SHA=501b15c7b38e3ef0cab38c7ecbde985986eae5f67d14f5a0c82c201a657ba775
ESTIMATOR="$REPO/config/gas_profiles/discovery_gas_estimator.mantle_mainnet.json"
UNIVERSE="$REPO/data/pool_universe.csv"

sha() { shasum -a 256 "$1" | cut -d' ' -f1; }

cmd_run() {
  : "${ARTIFACTS:?set ARTIFACTS to the WHI-1527 artifacts directory}"
  : "${BIN:?set BIN to the release discovery_replay binary}"
  mkdir -p "$WORK/runs"
  (cd "$ARTIFACTS" && shasum -a 256 -c SHA256SUMS >/dev/null)
  [[ "$(sha "$ARTIFACTS/SHA256SUMS")" == "$SUMS_SHA" ]] || { echo "SHA256SUMS mismatch" >&2; exit 1; }
  [[ "$(sha "$ARTIFACTS/corpus/corpus.jsonl")" == "$CORPUS_SHA" ]] || { echo "corpus mismatch" >&2; exit 1; }
  [[ "$(sha "$ARTIFACTS/corpus/mantle_mainnet_v1.json")" == "$PROFILE_SHA" ]] || { echo "profile mismatch" >&2; exit 1; }
  {
    echo "{"
    echo "  \"commit\": \"$(git -C "$REPO" rev-parse HEAD)\","
    echo "  \"dirty\": \"$(git -C "$REPO" status --porcelain | wc -l | tr -d ' ')\","
    echo "  \"binary_sha256\": \"$(sha "$BIN")\","
    echo "  \"estimator_sha256\": \"$(sha "$ESTIMATOR")\","
    echo "  \"universe_sha256\": \"$(sha "$UNIVERSE")\","
    echo "  \"corpus_sha256\": \"$CORPUS_SHA\","
    echo "  \"profile_sha256\": \"$PROFILE_SHA\","
    echo "  \"hw_model\": \"$(sysctl -n hw.model 2>/dev/null || uname -m)\","
    echo "  \"cpu\": \"$(sysctl -n machdep.cpu.brand_string 2>/dev/null || true)\","
    echo "  \"ncpu\": \"$(sysctl -n hw.ncpu 2>/dev/null || nproc)\","
    echo "  \"os\": \"$(sw_vers -productVersion 2>/dev/null || uname -r)\","
    echo "  \"power\": \"$(pmset -g batt 2>/dev/null | head -1 | tr -d '\"')\","
    echo "  \"started_at\": \"$(date -u +%FT%TZ)\""
    echo "}"
  } > "$WORK/host.json"
  one() { # arm label
    local extra=()
    [[ "$1" == on ]] && extra=(--estimator "$ESTIMATOR")
    local t0; t0=$(date -u +%FT%TZ)
    "$BIN" --corpus "$ARTIFACTS/corpus/corpus.jsonl" --profile "$ARTIFACTS/corpus/mantle_mainnet_v1.json" \
      --pool-universe "$UNIVERSE" --out "$WORK/runs/$2.jsonl" --label "$2" "${extra[@]}"
    echo "{\"arm\":\"$1\",\"label\":\"$2\",\"start\":\"$t0\",\"end\":\"$(date -u +%FT%TZ)\"}" >> "$WORK/order.jsonl"
  }
  : > "$WORK/order.jsonl"
  one off off-warmup
  one on on-warmup
  for ((r = 0; r < REPEATS; r++)); do
    if (( r % 2 == 0 )); then one off "off-rep$r"; one on "on-rep$r"
    else one on "on-rep$r"; one off "off-rep$r"; fi
  done
}

cmd_analyze() {
  python3 - "$WORK" <<'PY'
import json, math, sys, statistics
from pathlib import Path
work = Path(sys.argv[1])
def load(label):
    return [json.loads(l) for l in open(work / "runs" / f"{label}.jsonl")]
reps = int(max(int(p.stem.split("rep")[1]) for p in (work / "runs").glob("off-rep*.jsonl"))) + 1
arms = {a: [load(f"{a}-rep{r}") for r in range(reps)] for a in ("off", "on")}
q = lambda xs, p: xs[max(0, math.ceil(p * len(xs)) - 1)]
def per_pass(runs, key):
    n = len(runs[0])
    return [statistics.median(key(run[i]) for run in runs) for i in range(n)]
disc = lambda r: sum(r["discovery_s"]) * 1e3
wall = lambda r: r["discover_call_wall_s"] * 1e3
cpu = lambda r: r["discover_call_cpu_s"] * 1e3
out = {"repeats": reps}
base = arms["off"][0]
evaluated = [i for i, r in enumerate(base) if r["stats"]["cycles_optimized"] > 0]
for arm, runs in arms.items():
    # determinism: counters identical across repeats
    sig = lambda run: [(r["stats"], r["opportunities"], r["reject_reasons"]) for r in run]
    det = all(sig(run) == sig(runs[0]) for run in runs)
    first = runs[0]
    totals = {}
    for k in ("cycles_optimized", "paths_quoted", "amm_quotes", "simulations", "fee_resolution_failures",
              "measured_resolutions", "estimated_resolutions", "candidates_measured", "candidates_estimated",
              "gas_rescores"):
        totals[k] = sum(r["stats"][k] for r in first)
    rej, tiers, reasons = {}, {}, {}
    for r in first:
        for k, v in r["stats"]["rejects"].items(): rej[k] = rej.get(k, 0) + v
        for k, v in (r["stats"]["search_tiers"] or {}).items(): tiers[k] = tiers.get(k, 0) + v
        for k, v in r["reject_reasons"].items(): reasons[k] = reasons.get(k, 0) + v
    partition_ok = all((r["stats"]["search_tiers"] is not None) and
                       sum(r["stats"]["search_tiers"].values()) == r["stats"]["paths_quoted"] for r in first)
    timing = {}
    for scope in ("all", "full", "touched"):
        idx = [i for i in evaluated if scope == "all" or base[i]["stats"]["scope"] == scope]
        for name, f in (("discovery_ms", disc), ("discover_wall_ms", wall), ("discover_cpu_ms", cpu)):
            v = per_pass(runs, f)
            xs = sorted(v[i] for i in idx)
            timing[f"{scope}/{name}"] = {"n": len(xs), "p50": round(q(xs, .5), 3), "p90": round(q(xs, .9), 3),
                                         "p99": round(q(xs, .99), 3), "sum": round(sum(xs), 1)}
    out[arm] = {"deterministic_across_repeats": det, "totals": totals, "rejects": rej, "search_tiers": tiers,
                "reject_reason_metric_increments": dict(sorted(reasons.items())), "partition_equals_paths_quoted": partition_ok,
                "opportunities": sum(r["opportunities"] for r in first), "timing_evaluated_passes": timing}
same_inputs = all(a["stats"]["cycles_optimized"] == b["stats"]["cycles_optimized"] and
                  a["stats"]["scope"] == b["stats"]["scope"] for a, b in zip(arms["off"][0], arms["on"][0]))
out["same_cycles_optimized_and_scope_per_pass"] = same_inputs
ratios = {}
for scope in ("all", "full", "touched"):
    for name in ("discovery_ms", "discover_wall_ms", "discover_cpu_ms"):
        k = f"{scope}/{name}"
        ratios[k] = round(out["on"]["timing_evaluated_passes"][k]["sum"] / max(out["off"]["timing_evaluated_passes"][k]["sum"], 1e-9), 2)
out["ratio_of_sums_on_over_off"] = ratios
(work / "summary.json").write_text(json.dumps(out, indent=2) + "\n")
print(json.dumps(out, indent=2))
PY
}

case "${1:-}" in
  run) cmd_run ;;
  analyze) cmd_analyze ;;
  *) echo "usage: $0 run|analyze" >&2; exit 2 ;;
esac
