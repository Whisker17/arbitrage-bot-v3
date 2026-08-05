#!/usr/bin/env python3
"""Analyze a WHI-862 shadow ledger for candidate-rate measurement.

Reads one or more ledger JSONL files and prints a report JSON (stdout or --out).
No credentials; pure offline analysis.
"""

from __future__ import annotations

import argparse
import json
import math
import statistics
import sys
from collections import Counter, defaultdict
from pathlib import Path
from typing import Any


def load_rows(path: Path) -> list[dict[str, Any]]:
    rows: list[dict[str, Any]] = []
    with path.open() as f:
        for line_no, line in enumerate(f, 1):
            line = line.strip()
            if not line:
                continue
            try:
                rows.append(json.loads(line))
            except json.JSONDecodeError as e:
                raise SystemExit(f"{path}:{line_no}: invalid JSON: {e}") from e
    return rows


def as_int(x: Any) -> int | None:
    if x is None:
        return None
    if isinstance(x, int):
        return x
    if isinstance(x, str) and x.isdigit():
        return int(x)
    try:
        return int(x)
    except (TypeError, ValueError):
        return None


def percentiles(values: list[int]) -> dict[str, int | None]:
    if not values:
        return {"p0": None, "p25": None, "p50": None, "p75": None, "p95": None, "p100": None}
    s = sorted(values)

    def pct(p: float) -> int:
        if len(s) == 1:
            return s[0]
        k = (len(s) - 1) * p
        f = math.floor(k)
        c = math.ceil(k)
        if f == c:
            return s[int(k)]
        return int(s[f] + (s[c] - s[f]) * (k - f))

    return {
        "p0": s[0],
        "p25": pct(0.25),
        "p50": pct(0.50),
        "p75": pct(0.75),
        "p95": pct(0.95),
        "p100": s[-1],
    }


def analyze(rows: list[dict[str, Any]], label: str) -> dict[str, Any]:
    by_type = Counter(r.get("row_type") for r in rows)
    headers = [r for r in rows if r.get("row_type") == "run_header"]
    observations = [r for r in rows if r.get("row_type") == "observation"]
    candidates = [r for r in rows if r.get("row_type") == "candidate"]
    contexts = [r for r in rows if r.get("row_type") == "context"]

    send_caps = {h.get("send_capability") for h in headers}
    services = {h.get("service") for h in headers}

    obs_blocks: list[int] = []
    for o in observations:
        sid = o.get("snapshot_id") or {}
        n = as_int(sid.get("block_number"))
        if n is not None:
            obs_blocks.append(n)
    obs_blocks.sort()

    # Pair candidate rows with nearest preceding context by digest.
    ctx_by_digest: dict[str, dict[str, Any]] = {}
    for c in contexts:
        d = c.get("digest")
        if isinstance(d, str):
            ctx_by_digest[d] = c

    outcomes = Counter()
    gross_pos = 0
    net_pos = 0
    net_profits: list[int] = []
    gross_profits: list[int] = []
    topology: Counter[str] = Counter()
    # Protocol mix inferred from ordered pool identities is not in context;
    # opportunity_id is a topology fingerprint. Count it as the route key.
    for cand in candidates:
        outcomes[str(cand.get("outcome"))] += 1
        digest = cand.get("digest")
        ctx = ctx_by_digest.get(digest) if isinstance(digest, str) else None
        if not ctx:
            continue
        g = as_int(ctx.get("gross_profit"))
        n = as_int(ctx.get("net_profit"))
        if g is not None:
            gross_profits.append(g)
            if g > 0:
                gross_pos += 1
        if n is not None:
            net_profits.append(n)
            if n > 0:
                net_pos += 1
        oid = ctx.get("opportunity_id")
        if isinstance(oid, str) and oid:
            topology[oid[:18] + "…"] += 1  # shortened for readability
        # Prefer explicit route if present
        rk = ctx.get("route_key") or ctx.get("route")
        if isinstance(rk, str):
            topology[rk] += 1

    started = None
    ended = None
    for h in headers:
        t = as_int(h.get("started_at_unix"))
        if t is not None:
            started = t if started is None else min(started, t)
    for r in rows:
        t = as_int(r.get("recorded_at_unix"))
        if t is not None:
            ended = t if ended is None else max(ended, t)

    runtime_s = None
    if started is not None and ended is not None and ended >= started:
        runtime_s = ended - started

    block_span = None
    if len(obs_blocks) >= 2:
        block_span = obs_blocks[-1] - obs_blocks[0]

    # Extrapolate candidates/day from observed runtime.
    cand_per_day = None
    if runtime_s and runtime_s > 0:
        cand_per_day = candidates.__len__() * 86400.0 / runtime_s

    # Three-outcome classification (WHI-862 step 5).
    if len(candidates) == 0 and len(contexts) == 0:
        outcome_class = "no_opportunities"
        outcome_note = (
            "No candidate/context rows while the ledger recorded observations "
            "(pipeline alive). Class C for this synced coverage: no sized "
            "opportunity reached preflight."
        )
    elif net_pos > 0:
        outcome_class = "opportunities_clear_gas"
        outcome_note = f"{net_pos} context row(s) with net_profit > 0 after gas."
    elif gross_pos > 0 or any(n is not None and n <= 0 for n in net_profits):
        outcome_class = "opportunities_exist_but_gas_eats_them"
        outcome_note = (
            f"gross_pos={gross_pos}, net_pos={net_pos}; paths appear but none clear gas."
        )
    else:
        outcome_class = "no_opportunities"
        outcome_note = "Candidate rows present but no positive gross/net profit recorded."

    return {
        "label": label,
        "row_counts": dict(by_type),
        "send_capability": sorted(x for x in send_caps if x is not None),
        "services": sorted(x for x in services if x is not None),
        "runtime_seconds": runtime_s,
        "started_at_unix": started,
        "ended_at_unix": ended,
        "observations": {
            "count": len(observations),
            "block_min": obs_blocks[0] if obs_blocks else None,
            "block_max": obs_blocks[-1] if obs_blocks else None,
            "block_span": block_span,
            "unique_blocks": len(set(obs_blocks)),
        },
        "candidates": {
            "count": len(candidates),
            "by_outcome": dict(outcomes),
            "per_day_extrapolated": cand_per_day,
        },
        "profit": {
            "gross_positive_count": gross_pos,
            "net_positive_count": net_pos,
            "gross_profit_wei_distribution": percentiles(gross_profits),
            "net_profit_wei_distribution": percentiles(net_profits),
            "gross_mean_wei": int(statistics.mean(gross_profits)) if gross_profits else None,
            "net_mean_wei": int(statistics.mean(net_profits)) if net_profits else None,
        },
        "topology_mix": dict(topology.most_common(50)),
        "three_outcome_class": outcome_class,
        "three_outcome_note": outcome_note,
        "broadcast_count": sum(
            1
            for r in rows
            if r.get("row_type") == "candidate"
            and str(r.get("outcome", "")).lower() in {"submitted", "broadcast"}
        ),
    }


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("ledgers", nargs="+", type=Path)
    ap.add_argument("--label", action="append", default=[])
    ap.add_argument("--out", type=Path)
    args = ap.parse_args()
    reports = []
    for i, path in enumerate(args.ledgers):
        label = args.label[i] if i < len(args.label) else path.stem
        reports.append(analyze(load_rows(path), label))
    out = {"reports": reports}
    text = json.dumps(out, indent=2) + "\n"
    if args.out:
        args.out.write_text(text)
        print(f"wrote {args.out}", file=sys.stderr)
    else:
        sys.stdout.write(text)


if __name__ == "__main__":
    main()
