#!/usr/bin/env python3
"""WHI-1572: offline fit of the discovery-only gas estimator (stdlib only).

Two stages, run from the repository root:

  python3 scripts/gas_estimate/fit_discovery_estimator.py protocol
      Writes evidence/gas/whi-1572/protocol.json: input file hashes, the row
      hygiene rules, the predeclared ordered-pool-cycle train/calibration/test
      membership and every parameter convention. No model is fitted and no test
      row is evaluated. This file is committed BEFORE the fit stage runs.

  python3 scripts/gas_estimate/fit_discovery_estimator.py fit
      Re-derives the protocol and refuses to run if it differs from the
      committed protocol.json (inputs or membership changed). Fits the additive
      model on train rows only (exact rational least squares), rounds the
      coefficients to integers, calibrates the multiplier margins on calibration
      rows only against the deployed rounded integer model with ceiling
      arithmetic, then evaluates the untouched test groups and the leave-block-out
      diagnostics. Writes:
        config/gas_profiles/discovery_gas_estimator.mantle_mainnet.json
        evidence/gas/whi-1572/fit_report.json

Model: gas = b0 + b_v2*n_v2 + b_v3*n_v3 + b_moe*n_moe
             + s_tick*sum(v3 per-hop ticks) + s_bin*sum(moe per-hop bins)

All margins are NEW, UNVALIDATED parameters for signerless discovery ranking
only. Nothing here qualifies a route for sending.
"""

import csv
import hashlib
import io
import json
import math
import re
import subprocess
import sys
from fractions import Fraction
from pathlib import Path

TOOL_VERSION = "whi-1572-fit/1.0.0"
ROOT = Path(__file__).resolve().parents[2]
OUT_DIR = ROOT / "evidence/gas/whi-1572"
PROTOCOL_PATH = OUT_DIR / "protocol.json"
REPORT_PATH = OUT_DIR / "fit_report.json"
ARTIFACT_PATH = ROOT / "config/gas_profiles/discovery_gas_estimator.mantle_mainnet.json"
PROFILE_PATH = ROOT / "config/gas_profiles/mantle_mainnet_v1.json"
UNIVERSE_PATH = "data/pool_universe.csv"

# The named clean attempt files (issue WHI-1572, AC1). WHI-1413 run2 is not named
# and is not an input.
INPUT_FILES = [
    "evidence/gas/whi-1422/attempts.jsonl",
    "evidence/gas/whi-1520/attempts.jsonl",
    "evidence/gas/whi-1520/run2/attempts.jsonl",
    "evidence/gas/whi-1520/run3/attempts.jsonl",
    "evidence/gas/whi-1413/attempts.jsonl",
]

SCHEMA = "whisker-arb/discovery-gas-estimator/v1"
FEATURE_SCHEMA = "additive-v1:intercept,n_v2,n_v3,n_moe,sum_v3_ticks,sum_moe_bins"
CHAIN_ID = 5000
EXECUTOR_CODE_HASH = "0x50f51b776f893c4c86573ec5d669a1be3e84b3eef9817960376ba59ace2826ef"
CLEAN_LEVER = "v2_boost"  # inflate / displace are contaminated (PR108-F3, DI-51)

# Predeclared conventions (all recorded in protocol.json before any fit).
PARAMETERS = {
    "seed": "whi-1572-v1",
    "group_key": "ordered pool cycle: lower-case pool addresses joined by '>'",
    "group_order": "ascending sha256(seed + ':' + group_key) hex",
    "proportions": {"train": "floor(0.6*G)", "calibration": "floor(0.2*G)", "test": "rest"},
    "dedup_key": "(sample.block_hash, calldata_digest, lever_param); first occurrence in input-file order wins",
    "fit": "exact rational ordinary least squares on train rows, one weight per row",
    "coefficient_rounding": "nearest integer, ties toward +infinity (floor(x + 1/2))",
    "quantile_convention": "nearest-rank: q_p = sorted[ceil(p*n) - 1]",
    "ratio": "actual gas_used / predicted gas of the deployed rounded integer model (exact rational)",
    "expected_multiplier_bps": "ceil(10000 * calibration nearest-rank p90 ratio)",
    "limit_headroom": "5/4",
    "limit_multiplier_bps": "ceil(10000 * calibration max ratio * limit_headroom)",
    "limit_overhead_gas": 50000,
    "expected_gas_used": "ceil(pred * expected_multiplier_bps / 10000)  (conservative expected, not unbiased)",
    "limit_envelope": "ceil(pred * limit_multiplier_bps / 10000) + limit_overhead_gas",
    "extrapolation_envelope": "train rows only: hop counts, per-hop and summed V3 ticks / Moe bins, V2 presence, pool factories",
    "leave_block_out": "for each sample block: fit on the other block's rows (all splits), evaluate the held-out block with the calibrated margins; diagnostics only",
}

WITHHOLD_PATTERNS = [
    # (category, regex over the measured profile's prose reason)
    ("insufficient_samples", r"^insufficient qualification samples: \d+ < min_samples \d+$"),
    ("open_ended_bucket", r"^open-ended crossing bucket \(ticks=21\+ and/or bins=11\+\)"),
    ("failed_limit_gate", r"^holdout/train failed limit gate: "),
    ("venue_withhold_factory_axis", r"^withheld \((WHI-1413 under )?PR108-F2, DI-50\)"),
    ("venue_withhold_venue_axis", r"^withheld \(WHI-(1413|1520) under PR109-F1, DI-54\)"),
    ("scope_withhold", r"^withheld \(WHI-1520 scope\)"),
]


def sha256_file(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def fail(msg):
    sys.exit(f"fit_discovery_estimator: {msg}")


def load_rows():
    """Clean rows + exclusion counts, with row identities."""
    exclusions = {}
    rows = []
    seen = {}

    def exclude(reason):
        exclusions[reason] = exclusions.get(reason, 0) + 1

    for rel in INPUT_FILES:
        path = ROOT / rel
        for line_no, line in enumerate(path.read_text().splitlines(), start=1):
            if not line.strip():
                continue
            r = json.loads(line)
            if r.get("lever") != CLEAN_LEVER:
                exclude(f"lever != {CLEAN_LEVER}")
                continue
            if r.get("outcome") != "success":
                exclude("outcome != success")
                continue
            s = r.get("sample") or {}
            if s.get("source") != "fork_replay":
                exclude("sample.source != fork_replay")
                continue
            if s.get("outcome") != "success":
                exclude("sample.outcome != success")
                continue
            if s.get("chain_id") != CHAIN_ID:
                exclude("chain_id mismatch")
                continue
            if s.get("executor_code_hash") != EXECUTOR_CODE_HASH:
                exclude("executor_code_hash mismatch")
                continue
            protocols = r["topology"].split("+")
            crossings = r.get("per_hop_crossings") or []
            cycle = [p.lower() for p in r.get("cycle") or []]
            if (
                protocols != s["route_key"]["protocols"]
                or len(crossings) != len(protocols)
                or len(cycle) != len(protocols)
                or not isinstance(r.get("gas_used"), int)
                or r["gas_used"] != s.get("gas_used")
                or r["gas_used"] <= 0
            ):
                exclude("inconsistent row shape")
                continue
            key = (s["block_hash"], r["calldata_digest"], r["lever_param"])
            if key in seen:
                exclude("duplicate dedup key")
                continue
            seen[key] = True
            rows.append(
                {
                    "id": {
                        "file": rel,
                        "line": line_no,
                        "block_hash": key[0],
                        "calldata_digest": key[1],
                        "lever_param": key[2],
                    },
                    "block_number": s["block_number"],
                    "protocols": protocols,
                    "crossings": crossings,
                    "cycle": cycle,
                    "group": ">".join(cycle),
                    "gas_used": r["gas_used"],
                }
            )
    return rows, dict(sorted(exclusions.items()))


def split(rows):
    seed = PARAMETERS["seed"]
    groups = sorted(
        {r["group"] for r in rows},
        key=lambda g: hashlib.sha256(f"{seed}:{g}".encode()).hexdigest(),
    )
    n = len(groups)
    n_train = (6 * n) // 10
    n_cal = (2 * n) // 10
    return {
        "train": groups[:n_train],
        "calibration": groups[n_train : n_train + n_cal],
        "test": groups[n_train + n_cal :],
    }


def build_protocol():
    rows, exclusions = load_rows()
    return {
        "issue": "WHI-1572",
        "tool_version": TOOL_VERSION,
        "inputs": [{"path": p, "sha256": sha256_file(ROOT / p)} for p in INPUT_FILES],
        "measured_profile": {"path": str(PROFILE_PATH.relative_to(ROOT)), "sha256": sha256_file(PROFILE_PATH)},
        "row_filter": {
            "lever": CLEAN_LEVER,
            "outcome": "success",
            "source": "fork_replay",
            "chain_id": CHAIN_ID,
            "executor_code_hash": EXECUTOR_CODE_HASH,
        },
        "clean_rows": len(rows),
        "exclusions": exclusions,
        "parameters": PARAMETERS,
        "membership": split(rows),
    }


# --- model ---------------------------------------------------------------


def features(r):
    ps, cs = r["protocols"], r["crossings"]
    return [
        1,
        ps.count("v2"),
        ps.count("v3"),
        ps.count("moe"),
        sum(c for p, c in zip(ps, cs) if p == "v3"),
        sum(c for p, c in zip(ps, cs) if p == "moe"),
    ]


def ols(rows):
    xs = [features(r) for r in rows]
    ys = [r["gas_used"] for r in rows]
    k = len(xs[0])
    m = [
        [Fraction(sum(x[i] * x[j] for x in xs)) for j in range(k)]
        + [Fraction(sum(x[i] * y for x, y in zip(xs, ys)))]
        for i in range(k)
    ]
    for i in range(k):
        pivot = next((r for r in range(i, k) if m[r][i] != 0), None)
        if pivot is None:
            fail("singular design matrix (a feature never varies in train)")
        m[i], m[pivot] = m[pivot], m[i]
        for r in range(k):
            if r != i and m[r][i] != 0:
                f = m[r][i] / m[i][i]
                m[r] = [a - f * b for a, b in zip(m[r], m[i])]
    return [m[i][k] / m[i][i] for i in range(k)]


def round_half_up(x):
    return math.floor(x + Fraction(1, 2))


def predict(coef, r):
    return sum(c * f for c, f in zip(coef, features(r)))


def ceil_div(a, b):
    return -((-a) // b)


def nearest_rank(sorted_vals, p):
    return sorted_vals[max(0, math.ceil(p * len(sorted_vals)) - 1)]


def envelope(rows, universe):
    v3_hop = [c for r in rows for p, c in zip(r["protocols"], r["crossings"]) if p == "v3"]
    moe_hop = [c for r in rows for p, c in zip(r["protocols"], r["crossings"]) if p == "moe"]
    factories = sorted({universe[p] for r in rows for p in r["cycle"] if p in universe})
    unattributed = sorted({p for r in rows for p in r["cycle"] if p not in universe})
    return {
        "hop_counts": sorted({len(r["protocols"]) for r in rows}),
        "max_v3_ticks_per_hop": max(v3_hop, default=0),
        "max_moe_bins_per_hop": max(moe_hop, default=0),
        "max_sum_v3_ticks": max(features(r)[4] for r in rows),
        "max_sum_moe_bins": max(features(r)[5] for r in rows),
        "v2_free_rows": sum(1 for r in rows if "v2" not in r["protocols"]),
        "venue_factories": factories,
    }, unattributed


def evaluate(rows, coef, exp_bps, lim_bps, overhead):
    if not rows:
        return {"rows": 0}
    ratios, abs_rel, pad = [], [], []
    over_expected = over_limit = 0
    for r in rows:
        pred = predict(coef, r)
        actual = r["gas_used"]
        expected = ceil_div(pred * exp_bps, 10000)
        limit = ceil_div(pred * lim_bps, 10000) + overhead
        ratios.append(Fraction(actual, pred))
        abs_rel.append(Fraction(abs(actual - pred), actual))
        pad.append(Fraction(expected - actual, actual))
        over_expected += actual > expected
        over_limit += actual > limit
    ratios.sort(), abs_rel.sort(), pad.sort()
    q = lambda xs, p: round(float(nearest_rank(xs, p)), 4)
    return {
        "rows": len(rows),
        "groups": len({r["group"] for r in rows}),
        "abs_rel_error": {"p50": q(abs_rel, 0.5), "p90": q(abs_rel, 0.9), "p95": q(abs_rel, 0.95), "max": round(float(abs_rel[-1]), 4)},
        "actual_over_predicted": {"min": round(float(ratios[0]), 4), "p50": q(ratios, 0.5), "p90": q(ratios, 0.9), "max": round(float(ratios[-1]), 4)},
        "actual_exceeds_expected": over_expected,
        "actual_exceeds_limit": over_limit,
        "expected_padding_over_actual": {"p10": q(pad, 0.1), "p50": q(pad, 0.5), "p90": q(pad, 0.9)},
    }


def universe_factories():
    """pool -> factory over every committed revision of the universe CSV."""
    revs = subprocess.run(
        ["git", "log", "--format=%H", "--", UNIVERSE_PATH],
        cwd=ROOT, check=True, capture_output=True, text=True,
    ).stdout.split()
    mapping = {}
    for rev in revs:
        text = subprocess.run(
            ["git", "show", f"{rev}:{UNIVERSE_PATH}"],
            cwd=ROOT, check=True, capture_output=True, text=True,
        ).stdout
        for row in csv.DictReader(io.StringIO(text)):
            pool, factory = row["pool"].lower(), row["factory"].lower()
            if mapping.setdefault(pool, factory) != factory:
                fail(f"pool {pool} has two factories across universe revisions")
    return mapping, revs


def withhold_policy(profile):
    entries, counts = [], {}
    for p in profile["profiles"]:
        if p["status"] == "approved":
            continue
        if p["status"] != "unsupported":
            fail(f"unexpected profile status {p['status']}")
        reason = p.get("reason") or ""
        cats = [c for c, rx in WITHHOLD_PATTERNS if re.search(rx, reason)]
        if len(cats) != 1:
            fail(f"reason for {p['route_key']} matches {cats}: {reason!r}")
        counts[cats[0]] = counts.get(cats[0], 0) + 1
        entries.append({"route_key": p["route_key"], "category": cats[0], "estimation": "eligible"})

    def key_string(k):
        s = f"h{k['hop_count']}:{'+'.join(k['protocols'])}"
        if "v3_tick_crossings" in k:
            s += f":ticks={k['v3_tick_crossings']}"
        if "moe_bin_crossings" in k:
            s += f":bins={k['moe_bin_crossings']}"
        return s

    entries.sort(key=lambda e: key_string(e["route_key"]))
    return entries, dict(sorted(counts.items()))


def run_fit():
    committed = json.loads(PROTOCOL_PATH.read_text())
    fresh = build_protocol()
    if committed != fresh:
        fail("inputs or membership differ from the committed protocol.json; refusing to fit")
    rows, exclusions = load_rows()
    member = {g: s for s, gs in committed["membership"].items() for g in gs}
    by_split = {s: [r for r in rows if member[r["group"]] == s] for s in ("train", "calibration", "test")}

    exact = ols(by_split["train"])
    coef = [round_half_up(c) for c in exact]
    cal_ratios = sorted(Fraction(r["gas_used"], predict(coef, r)) for r in by_split["calibration"])
    if min(predict(coef, r) for r in rows) <= 0:
        fail("integer model predicts non-positive gas on an input row")
    p90 = nearest_rank(cal_ratios, 0.9)
    exp_bps = math.ceil(p90 * 10000)
    headroom = Fraction(5, 4)
    lim_bps = math.ceil(cal_ratios[-1] * headroom * 10000)
    overhead = PARAMETERS["limit_overhead_gas"]
    if not (exp_bps >= 10000 and lim_bps > exp_bps):
        fail(f"degenerate margins exp_bps={exp_bps} lim_bps={lim_bps}")

    universe, universe_revs = universe_factories()
    env, unattributed = envelope(by_split["train"], universe)
    profile = json.loads(PROFILE_PATH.read_text())
    policy, category_counts = withhold_policy(profile)

    names = ["b0", "b_v2", "b_v3", "b_moe", "s_tick", "s_bin"]
    artifact = {
        "schema": SCHEMA,
        "feature_schema": FEATURE_SCHEMA,
        "chain_id": CHAIN_ID,
        "executor_code_hash": profile["executor_code_hash"],
        "executor_abi_digest": profile["executor_abi_digest"],
        "measured_profile_digest": profile["content_digest"],
        "model": dict(zip(names, coef)),
        "margins": {
            "expected_multiplier_bps": exp_bps,
            "limit_multiplier_bps": lim_bps,
            "limit_overhead_gas": overhead,
            "validated": False,
            "source": "evidence/gas/whi-1572/fit_report.json calibration split (WHI-1572); new, unvalidated discovery-only parameters",
        },
        "training_envelope": env,
        "withhold_policy": policy,
        "venue_qualification_evidence": [],
        "provenance": {
            "tool_version": TOOL_VERSION,
            "protocol_sha256": sha256_file(PROTOCOL_PATH),
            "fit_report": "evidence/gas/whi-1572/fit_report.json",
            "note": "Estimated values rank signerless discovery only; they never become a GasQuote and no configuration makes an Estimated candidate send-eligible.",
        },
    }

    # Leave-block-out diagnostics (fit on one block, evaluate the other).
    lbo = {}
    for block in sorted({r["block_number"] for r in rows}):
        held = [r for r in rows if r["block_number"] == block]
        rest = [r for r in rows if r["block_number"] != block]
        try:
            c = [round_half_up(x) for x in ols(rest)]
            lbo[str(block)] = {"fit_rows": len(rest), "coefficients": dict(zip(names, c)), **evaluate(held, c, exp_bps, lim_bps, overhead)}
        except SystemExit as e:
            lbo[str(block)] = {"fit_rows": len(rest), "error": str(e)}

    report = {
        "issue": "WHI-1572",
        "tool_version": TOOL_VERSION,
        "script_sha256": sha256_file(Path(__file__)),
        "python": sys.version.split()[0],
        "protocol_sha256": sha256_file(PROTOCOL_PATH),
        "inputs": committed["inputs"],
        "clean_rows": len(rows),
        "exclusions": exclusions,
        "split_rows": {s: len(v) for s, v in by_split.items()},
        "split_groups": {s: len(v) for s, v in committed["membership"].items()},
        "exact_train_coefficients": {n: f"{c.numerator}/{c.denominator}" for n, c in zip(names, exact)},
        "deployed_coefficients": dict(zip(names, coef)),
        "calibration": {
            "ratio_p90": f"{p90.numerator}/{p90.denominator}",
            "ratio_max": f"{cal_ratios[-1].numerator}/{cal_ratios[-1].denominator}",
            "expected_multiplier_bps": exp_bps,
            "limit_multiplier_bps": lim_bps,
            "limit_overhead_gas": overhead,
        },
        "evaluation": {s: evaluate(v, coef, exp_bps, lim_bps, overhead) for s, v in by_split.items()},
        "leave_block_out": lbo,
        "coverage": {
            "train_envelope": env,
            "train_unattributed_pools": unattributed,
            "topologies_by_split": {s: sorted({"+".join(r["protocols"]) for r in v}) for s, v in by_split.items()},
            "universe_revisions_for_factories": universe_revs,
        },
        "withhold_policy_counts": category_counts,
        "withhold_policy_total": len(policy),
        "rows": [dict(r["id"], split=member[r["group"]]) for r in rows],
    }
    ARTIFACT_PATH.write_text(json.dumps(artifact, indent=2) + "\n")
    REPORT_PATH.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps({k: report[k] for k in ("deployed_coefficients", "calibration", "evaluation", "leave_block_out")}, indent=2))


def main():
    stage = sys.argv[1] if len(sys.argv) > 1 else ""
    if stage == "protocol":
        OUT_DIR.mkdir(parents=True, exist_ok=True)
        PROTOCOL_PATH.write_text(json.dumps(build_protocol(), indent=2) + "\n")
        print(f"wrote {PROTOCOL_PATH.relative_to(ROOT)}")
    elif stage == "fit":
        run_fit()
    else:
        fail("usage: fit_discovery_estimator.py protocol|fit")


if __name__ == "__main__":
    main()
