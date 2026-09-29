#!/usr/bin/env python3
"""min_net_profit economics: our modeled candidates vs competitor realized arbs.

Offline, stdlib only. Inputs are frozen external files plus the RPC cache written
by fetch_rpc.py. Writes the compact summary JSON (committed) to stdout path arg.

Usage:
  analyze.py <host_dir> <dune_csv> <rpc_cache_dir> <gas_profile.json> <out_summary.json>

Units: every amount is wei (1e18 = 1 WMNT or 1 MNT). WMNT and MNT are treated
1:1 (WMNT is the wrapped native token; see STATUS.md caveat).
"""
import csv
import glob
import json
import os
import re
import sys
from datetime import datetime, timezone
from collections import Counter, defaultdict

WMNT = "0x78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8"
TRANSFER = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
DEPOSIT = "0xe1fffcc4923d04b559f4d29a8bfc6cda04eb5b0d3c460751c2402c5c5cc9109c"
WITHDRAWAL = "0x7fcf532c15f0a6db0bd6d0e038bea71d30d808c7d98cb3bf7268a95bf5081b65"
MIN_NET_PROFIT_WEI = 10**16  # V3_MIN_PROFIT_FLOOR_WEI, src/service/config.rs:60
PRIORITY_FEE_WEI = 100_000  # ExecutorConfig default, src/execution/types.rs:74
DAY_START, DAY_END = 1790553600, 1790640000  # 2026-09-28T00:00Z, half-open
DETAIL_RE = re.compile(r"production_gate_blocked amount_in=(\d+) min_profit=(\d+) signature=(\S+)$")
ANSI = re.compile(r"\x1b\[[0-9;]*m")


def pct(vals, p):
    """Nearest-rank percentile on a sorted copy (p in 0..100)."""
    s = sorted(vals)
    if not s:
        return None
    k = max(0, min(len(s) - 1, -(-p * len(s) // 100) - 1))
    return s[k]


def dist(vals):
    if not vals:
        return {"count": 0}
    return {
        "count": len(vals),
        "sum": sum(vals),
        "min": min(vals),
        "p10": pct(vals, 10),
        "p25": pct(vals, 25),
        "p50": pct(vals, 50),
        "p75": pct(vals, 75),
        "p90": pct(vals, 90),
        "max": max(vals),
    }


def load_json(path):
    with open(path) as f:
        return json.load(f)


def parse_signature(sig):
    """'v2:tokA->tokB/pool|v3:...' -> (protocols, pools lower-case)."""
    protos, pools = [], []
    for hop in sig.split("|"):
        proto, rest = hop.split(":", 1)
        protos.append(proto)
        pools.append(rest.split("/")[1].lower())
    return protos, pools


def gas_expected(profile, protos):
    """expected_gas_used of the only approved bucket for this protocol order."""
    hits = [
        p for p in profile["profiles"]
        if p["route_key"]["protocols"] == protos and p["status"] == "approved"
    ]
    if len(hits) != 1:
        raise SystemExit(f"expected exactly one approved gas bucket for {protos}, got {len(hits)}")
    return hits[0]["expected_gas_used"], hits[0]["route_key"]


def read_ledger(path):
    """Candidate rows joined to the observation row that precedes them."""
    last, cands, obs = None, [], []
    with open(path) as f:
        for line in f:
            r = json.loads(line)
            if r["row_type"] == "observation":
                last = r
                d = r.get("discovery") or {}  # the first observation carries no discovery
                obs.append((r["snapshot_id"]["block_number"], r["header"]["block_timestamp"],
                            d.get("scope"), set(d.get("dirty_pools") or [])))
            elif r["row_type"] == "candidate":
                m = DETAIL_RE.match(r["detail"])
                if not m or last is None or last["sequence"] != r["sequence"] - 1:
                    raise SystemExit(f"unparseable/unjoined candidate row seq={r['sequence']}")
                cands.append({
                    "sequence": r["sequence"],
                    "block": last["snapshot_id"]["block_number"],
                    "block_timestamp": last["header"]["block_timestamp"],
                    "amount_in": int(m[1]),
                    "net_modeled": int(m[2]),
                    "signature": m[3],
                    "outcome": r["outcome"]["kind"],
                    "recorded_at_unix": r["recorded_at_unix"],
                    "detect_lag_s": r["recorded_at_unix"] - last["header"]["block_timestamp"],
                })
    return cands, obs


def read_log_summaries(log_dir):
    """block_summary lines: block -> dict of key fields (latest wins)."""
    out = {}
    for fn in sorted(glob.glob(os.path.join(log_dir, "signerless.log*"))):
        with open(fn, errors="replace") as f:
            for line in f:
                if "block_summary" not in line:
                    continue
                line = ANSI.sub("", line)
                if "block_summary block=" not in line:
                    continue
                kv = dict(re.findall(r'(\w+)=("[^"]*"|\S+)', line))
                b = int(kv["block"])
                out[b] = {
                    "ts": line.split()[0],
                    "candidates": int(kv["candidates"]),
                    "best_net": kv["best_net"].strip('"'),
                    "attempt_outcome": kv["attempt_outcome"].strip('"'),
                    "skip_reason": kv.get("skip_reason", "-").strip('"'),
                    "cycles_evaluated": int(kv.get("cycles_evaluated", "0")),
                }
    return out


def discovery_totals(path):
    """Sum the per-block discovery counters (exact denominators for coverage)."""
    tot = Counter()
    with open(path) as f:
        for line in f:
            r = json.loads(line)
            d = r.get("discovery") if r["row_type"] == "observation" else None
            if d:
                tot["observations"] += 1
                tot["cycles_total"] += d["cycles_total"]
                tot["cycles_optimized"] += d["cycles_optimized"]
                tot["paths_quoted"] += d["paths_quoted"]
                for k, v in d["rejects"].items():
                    tot["reject_" + k] += v
    tot = dict(tot)
    co = tot["cycles_optimized"]
    # Two distinct metrics (do not conflate): share of dirty-cycle selections not
    # rejected as unapproved_route, and share that completed an optimizer search.
    tot["approval_share_pct"] = round(100 * (co - tot["reject_unapproved_route"]) / co, 6)
    tot["completed_search_coverage_pct"] = round(100 * tot["paths_quoted"] / co, 6)
    return tot


def episodes(cands, obs):
    """Split each route's candidate blocks into distinct opportunities.

    Rule: a new episode starts when, between two candidate blocks of the same
    route, there is an observed block that re-evaluated the route (Full scope or
    any route pool dirty) and recorded NO candidate at all (so the top-1 cannot
    hide it). Also reported: the naive 'consecutive block' runs.
    """
    cand_blocks = defaultdict(set)
    any_cand = set()
    for c in cands:
        cand_blocks[c["signature"]].add(c["block"])
        any_cand.add(c["block"])
    res = {}
    for sig, blocks in cand_blocks.items():
        _, pools = parse_signature(sig)
        pools = set(pools)
        bl = sorted(blocks)
        state_eps, run_eps = [[bl[0]]], [[bl[0]]]
        for prev, cur in zip(bl, bl[1:]):
            closed = any(
                prev < b < cur and b not in any_cand and (scope == "full" or dirty & pools)
                for b, _, scope, dirty in obs
            )
            (state_eps.append([cur]) if closed else state_eps[-1].append(cur))
            (run_eps[-1].append(cur) if cur == prev + 1 else run_eps.append([cur]))
        res[sig] = {"state": state_eps, "consecutive": run_eps}
    return res


def competitor(rows, cache):
    txs, excluded = [], Counter()
    fee_check = {"checked": 0, "exact": 0, "mismatch": []}
    for r in rows:
        h = r["tx_hash"]
        rc = load_json(f"{cache}/receipt_{h}.json")
        tx = load_json(f"{cache}/tx_{h}.json")
        execu = r["executor_address"].lower()
        sender = rc["from"].lower()
        if rc["to"].lower() != execu:
            raise SystemExit(f"executor mismatch {h}")
        gas_used = int(rc["gasUsed"], 16)
        egp = int(rc["effectiveGasPrice"], 16)
        l1 = int(rc.get("l1Fee", "0x0"), 16)
        op = gas_used * int(rc.get("operatorFeeScalar", "0x0"), 16) * 100 + int(rc.get("operatorFeeConstant", "0x0"), 16)
        l2 = gas_used * egp
        fee = l2 + l1 + op
        blk = int(rc["blockNumber"], 16)
        # sender native balance delta cross-check (valid when sender has one tx in the block)
        b0 = int(load_json(f"{cache}/bal_{rc['from']}_{blk - 1}.json"), 16)
        b1 = int(load_json(f"{cache}/bal_{rc['from']}_{blk}.json"), 16)
        value = int(tx["value"], 16)
        # settlement-asset flows
        asset = r["settlement_asset"].lower()
        me = {execu, sender}
        delta, other_tokens, wrap_events = 0, defaultdict(int), 0
        amount_out, first_pool = 0, None
        for lg in rc["logs"]:
            t = lg["topics"]
            addr = lg["address"].lower()
            if not t:
                continue
            if t[0] == TRANSFER and len(t) == 3:
                frm, to = "0x" + t[1][-40:], "0x" + t[2][-40:]
                amt = int(lg["data"], 16) if lg["data"] not in ("0x", "") else 0
                sgn = (to in me) - (frm in me)
                if sgn == 0:
                    continue
                if addr == asset:
                    delta += sgn * amt
                    if sgn < 0:
                        amount_out += amt
                        first_pool = first_pool or to
                else:
                    other_tokens[addr] += sgn * amt
            elif addr == WMNT and t[0] in (DEPOSIT, WITHDRAWAL):
                wrap_events += 1
        residual = {k: v for k, v in other_tokens.items() if v != 0}
        rec = {
            "tx": h,
            "block": blk,
            "block_time": r["block_time"],
            "block_timestamp": int(datetime.strptime(r["block_time"], "%Y-%m-%d %H:%M:%S.%f UTC").replace(tzinfo=timezone.utc).timestamp()),
            "bot": r["bot_address"].lower(),
            "executor": execu,
            "hops": int(r["hop_count"]),
            "pools": r["ordered_pools"].lower().split(";"),
            "asset": asset,
            "status": int(rc["status"], 16),
            "gas_used": gas_used,
            "effective_gas_price": egp,
            "l2_fee": l2,
            "l1_fee": l1,
            "operator_fee": op,
            "fee_total": fee,
            "gross_settlement": delta,
            "amount_in": amount_out,
            "first_pool": first_pool,
            "net": delta - fee if asset == WMNT else None,
            "residual_other_token_deltas": len(residual),
            "wrap_events": wrap_events,
            "dune_gas_used_match": int(r["gas_used"]) == gas_used,
            "dune_gas_price_match": int(r["gas_price"]) == egp,
            "sender_balance_delta": b1 - b0,
            "value": value,
        }
        txs.append(rec)
        same_sender_same_block = sum(1 for x in rows if x["block_number"] == r["block_number"] and x["bot_address"].lower() == r["bot_address"].lower())
        if value == 0 and same_sender_same_block == 1:
            fee_check["checked"] += 1
            if b0 - b1 == fee:
                fee_check["exact"] += 1
            else:
                fee_check["mismatch"].append({"tx": h, "balance_drop": b0 - b1, "fee_total": fee})
    return txs, fee_check


def main():
    host, dune_csv, cache, profile_path, out_path = sys.argv[1:6]
    profile = load_json(profile_path)

    # ---------- our side ----------
    cands, obs = read_ledger(os.path.join(host, "ledger.jsonl"))
    cut_cands, cut_obs = read_ledger(os.path.join(host, "ledger_cut_20260928.jsonl"))
    cut_seqs = {c["sequence"] for c in cut_cands}
    day_cands = [c for c in cands if DAY_START <= c["block_timestamp"] < DAY_END]
    if {c["sequence"] for c in day_cands} != cut_seqs:
        raise SystemExit("day subset by block_timestamp != ledger_cut candidate rows")

    headers = {}
    for c in cands:
        hdr = load_json(f"{cache}/block_{c['block']}.json")
        if int(hdr["timestamp"], 16) != c["block_timestamp"]:
            raise SystemExit(f"header timestamp mismatch block {c['block']}")
        headers[c["block"]] = int(hdr["baseFeePerGas"], 16)

    # day bounds from chain headers
    ts = {b: int(load_json(f"{cache}/block_{b}.json")["timestamp"], 16) for b in (101211643, 101211644, 101254843, 101254844)}
    day_bounds_ok = ts[101211643] < DAY_START <= ts[101211644] and ts[101254843] < DAY_END <= ts[101254844]

    logs = read_log_summaries(os.path.join(host, "logs"))
    ledger_blocks = {b for b, *_ in obs}
    day_ledger_blocks = {b for b, t, *_ in cut_obs}
    log_day_by_chain = {b for b in logs if 101211644 <= b <= 101254843}
    log_day_by_wall = {b for b, v in logs.items() if v["ts"].startswith("2026-09-28")}

    for c in cands:
        protos, pools = parse_signature(c["signature"])
        g, rk = gas_expected(profile, protos)
        base = headers[c["block"]]
        gas_cost = g * (base + PRIORITY_FEE_WEI)
        c.update({
            "protocols": protos,
            "pools": pools,
            "cross_protocol": len(set(protos)) > 1,
            "gas_route_key": rk,
            "expected_gas_used": g,
            "base_fee": base,
            "gas_cost_modeled": gas_cost,
            "gross_modeled": c["net_modeled"] + gas_cost,
            "log_best_net": logs.get(c["block"], {}).get("best_net"),
            "log_candidates": logs.get(c["block"], {}).get("candidates"),
        })

    eps = episodes(cands, obs)

    # ---------- competitor side ----------
    rows = list(csv.DictReader(open(dune_csv)))
    txs, fee_check = competitor(rows, cache)
    nonce_path = f"{cache}/feecheck_nonce.json"
    nonce_chk = load_json(nonce_path) if os.path.exists(nonce_path) else {}
    wm = [t for t in txs if t["asset"] == WMNT and t["status"] == 1]
    clean = [t for t in wm if t["residual_other_token_deltas"] == 0 and t["wrap_events"] == 0]

    # Operator-fee scalar and L1-fee scale observed on the same day (proxy for unmodeled costs)
    l1_fees = [t["l1_fee"] for t in txs]
    op_per_gas = sorted({t["operator_fee"] // t["gas_used"] for t in txs})
    egps = [t["effective_gas_price"] for t in txs]
    base_fees_seen = sorted(set(headers.values()))

    def our_block(sel, label):
        nets = [c["net_modeled"] for c in sel]
        gross = [c["gross_modeled"] for c in sel]
        med_l1 = pct(l1_fees, 50)
        opg = op_per_gas[0] if len(op_per_gas) == 1 else None
        adj = [c["net_modeled"] - med_l1 - c["expected_gas_used"] * opg for c in sel] if opg is not None else None
        # "competitive inclusion": pay the competitor median effective gas price instead of base+1e5
        med_egp = pct(egps, 50)
        share = pct([t["fee_total"] * 10000 // t["gross_settlement"] for t in txs if t["asset"] == WMNT and t["gross_settlement"] > 0], 50)
        shared = [c["gross_modeled"] * (10000 - share) // 10000 for c in sel]
        comp = [c["gross_modeled"] - c["expected_gas_used"] * med_egp - med_l1 - c["expected_gas_used"] * (opg or 0) for c in sel]
        by_sig = defaultdict(list)
        for c in sel:
            by_sig[c["signature"]].append(c)
        routes = []
        for sig, cs in sorted(by_sig.items(), key=lambda kv: -len(kv[1])):
            blocks = {c["block"] for c in cs}
            st = [e for e in eps[sig]["state"] if set(e) & blocks]
            ru = [e for e in eps[sig]["consecutive"] if set(e) & blocks]
            routes.append({
                "signature": sig,
                "protocols": cs[0]["protocols"],
                "pools": cs[0]["pools"],
                "cross_protocol": cs[0]["cross_protocol"],
                "gas_route_key": cs[0]["gas_route_key"],
                "expected_gas_used": cs[0]["expected_gas_used"],
                "rows": len(cs),
                "state_episodes": len(st),
                "consecutive_block_runs": len(ru),
                "episode_blocks": [[e[0], e[-1], len(e)] for e in st],
                "net_modeled": dist([c["net_modeled"] for c in cs]),
                "amount_in": dist([c["amount_in"] for c in cs]),
                "amount_in_at_cap_10wmnt": sum(1 for c in cs if c["amount_in"] == 10 * 10**18),
            })
        ep_best = []
        for r in routes:
            for first, last, _ in r["episode_blocks"]:
                ep_best.append(max(c["net_modeled"] for c in by_sig[r["signature"]] if first <= c["block"] <= last))
        return {
            "label": label,
            "rows": len(sel),
            "distinct_blocks": len({c["block"] for c in sel}),
            "distinct_routes": len(by_sig),
            "state_episodes": sum(r["state_episodes"] for r in routes),
            "consecutive_block_runs": sum(r["consecutive_block_runs"] for r in routes),
            "rows_cross_protocol": sum(1 for c in sel if c["cross_protocol"]),
            "rows_pure": sum(1 for c in sel if not c["cross_protocol"]),
            "outcomes": dict(Counter(c["outcome"] for c in sel)),
            "net_modeled_per_row": dist(nets),
            "gross_modeled_per_row": dist(gross),
            "net_modeled_best_per_episode": dist(ep_best),
            "detect_lag_s_per_row": dist([c["detect_lag_s"] for c in sel]),
            "rows_net_ge_min_net_profit": sum(1 for n in nets if n >= MIN_NET_PROFIT_WEI),
            "rows_net_lt_1_5x_floor": sum(1 for n in nets if n < 15 * 10**15),
            "net_after_unmodeled_l1_and_operator_fee": dist(adj) if adj else None,
            "rows_below_floor_after_unmodeled_fees": sum(1 for a in adj if a < MIN_NET_PROFIT_WEI) if adj else None,
            "rows_clear_floor_after_unmodeled_fees": sum(1 for a in adj if a >= MIN_NET_PROFIT_WEI) if adj else None,
            "rows_positive_after_unmodeled_fees": sum(1 for a in adj if a > 0) if adj else None,
            "net_if_paying_competitor_median_gas_price": dist(comp),
            "rows_positive_if_paying_competitor_median_gas_price": sum(1 for a in comp if a > 0),
            "competitor_median_fee_share_of_gross_bps": share,
            "net_if_fee_equals_competitor_median_share_of_gross": dist(shared),
            "rows_clear_floor_if_fee_equals_competitor_median_share": sum(1 for a in shared if a >= MIN_NET_PROFIT_WEI),
            "routes": routes,
        }

    ours_full = our_block(cands, "full rc2 run 2026-09-27T02:40Z..2026-09-29T02:47Z")
    ours_day = our_block(day_cands, "UTC day 2026-09-28 (ledger_cut_20260928)")

    # log reconciliation for candidate blocks
    log_cov = [c for c in cands if c["log_best_net"] is not None]
    log_match = sum(1 for c in log_cov if int(c["log_best_net"]) == c["net_modeled"])
    multi = [c["block"] for c in log_cov if c["log_candidates"] and c["log_candidates"] > 1]

    # competitor distributions
    def comp_block(sel):
        nets = [t["net"] for t in sel]
        gross = [t["gross_settlement"] for t in sel]
        fees = [t["fee_total"] for t in sel]
        per_bot = defaultdict(lambda: {"txs": 0, "gross": 0, "fee": 0, "net": 0})
        for t in sel:
            b = per_bot[t["bot"]]
            b["txs"] += 1
            b["gross"] += t["gross_settlement"]
            b["fee"] += t["fee_total"]
            b["net"] += t["net"]
        return {
            "count": len(sel),
            "gross_wmnt": dist(gross),
            "fee_total_mnt": dist(fees),
            "l2_fee": dist([t["l2_fee"] for t in sel]),
            "l1_fee": dist([t["l1_fee"] for t in sel]),
            "operator_fee": dist([t["operator_fee"] for t in sel]),
            "gas_used": dist([t["gas_used"] for t in sel]),
            "effective_gas_price": dist([t["effective_gas_price"] for t in sel]),
            "net": dist(nets),
            "fee_share_of_gross_bps": dist([t["fee_total"] * 10000 // t["gross_settlement"] for t in sel if t["gross_settlement"] > 0]),
            "amount_in": dist([t["amount_in"] for t in sel]),
            "net_positive": sum(1 for n in nets if n > 0),
            "net_ge_min_net_profit": sum(1 for n in nets if n >= MIN_NET_PROFIT_WEI),
            "gross_ge_min_net_profit": sum(1 for g in gross if g >= MIN_NET_PROFIT_WEI),
            "gross_le_zero": sum(1 for g in gross if g <= 0),
            "hops": dict(sorted(Counter(t["hops"] for t in sel).items())),
            "per_bot": dict(sorted(per_bot.items(), key=lambda kv: -kv[1]["net"])),
        }

    comp_all = comp_block(wm)
    comp_clean = comp_block(clean)

    # like-for-like overlap: competitor arbs on exactly one of our candidate routes' pool sets
    by_set = defaultdict(list)
    for sig in {c["signature"] for c in cands}:
        by_set[frozenset(parse_signature(sig)[1])].append(sig)
    overlap = []
    for t in txs:
        sigs = by_set.get(frozenset(t["pools"]))
        if not sigs:
            continue
        same_dir = [s_ for s_ in sigs if parse_signature(s_)[1][0] == t["first_pool"]]
        s_ = same_dir[0] if same_dir else None
        sig_blocks = {c["block"]: c for c in cands if c["signature"] == s_} if s_ else {}
        prior = [c for b, c in sorted(sig_blocks.items()) if t["block"] - 3 <= b < t["block"]]
        overlap.append({
            "tx": t["tx"], "block": t["block"], "block_time": t["block_time"], "bot": t["bot"],
            "first_pool": t["first_pool"],
            "our_same_direction_signature": s_,
            "our_routes_on_same_pool_set": len(sigs),
            "competitor_amount_in": t["amount_in"], "competitor_gross": t["gross_settlement"],
            "competitor_fee_total": t["fee_total"], "competitor_net": t["net"],
            "competitor_effective_gas_price": t["effective_gas_price"],
            "our_candidate_blocks_in_prior_3": [c["block"] for c in prior],
            "our_modeled_net_last_prior": prior[-1]["net_modeled"] if prior else None,
            "our_modeled_gross_last_prior": prior[-1]["gross_modeled"] if prior else None,
            "our_amount_in_last_prior": prior[-1]["amount_in"] if prior else None,
            "our_detect_lag_s_last_prior": prior[-1]["detect_lag_s"] if prior else None,
            # earliest chance to act: first candidate row of the episode
            "our_first_candidate_block": prior[0]["block"] if prior else None,
            "our_first_candidate_recorded_at_unix": prior[0]["recorded_at_unix"] if prior else None,
            "our_first_candidate_detect_lag_s": prior[0]["detect_lag_s"] if prior else None,
            "our_first_candidate_log_block_summary_utc": logs.get(prior[0]["block"], {}).get("ts") if prior else None,
            "competitor_block_timestamp_unix": t["block_timestamp"],
            "our_first_recorded_minus_competitor_block_ts_s": (prior[0]["recorded_at_unix"] - t["block_timestamp"]) if prior else None,
            "block_observed_in_ledger": t["block"] in ledger_blocks,
        })

    exclusions = {
        "dune_rows": len(rows),
        "reverted": sum(1 for t in txs if t["status"] != 1),
        "non_wmnt_settlement": sum(1 for t in txs if t["asset"] != WMNT),
        "non_wmnt_settlement_by_asset": dict(Counter(t["asset"] for t in txs if t["asset"] != WMNT)),
        "wmnt_included": len(wm),
        "wmnt_with_residual_other_token_delta": sum(1 for t in wm if t["residual_other_token_deltas"]),
        "wmnt_with_wrap_unwrap_event": sum(1 for t in wm if t["wrap_events"]),
        "wmnt_clean": len(clean),
        "nonzero_tx_value": sum(1 for t in txs if t["value"] != 0),
    }

    summary = {
        "schema": "whi-1414/economics-summary/v1",
        "units": "wei (1e18 = 1 WMNT = 1 MNT, WMNT~MNT 1:1)",
        "config": {
            "min_net_profit_wei": MIN_NET_PROFIT_WEI,
            "min_net_profit_source": "established code floor: src/service/config.rs:60 V3_MIN_PROFIT_FLOOR_WEI via ServiceConfigOpts::agni_v3 (bot.rs:498); env can only raise it (config.rs:267,514); effective runtime value inferred (host .env not read), bounded by effective_threshold_bound_wei",
            "priority_fee_per_gas_modeled": PRIORITY_FEE_WEI,
            "assumed_capital_cap_wei": 10 * 10**18,
            "base_fees_seen_on_candidate_blocks": base_fees_seen,
            # The code floor is established; the effective runtime threshold is inferred and
            # bounded by [code floor, smallest admitted candidate net] (fixed runtime config).
            "min_admitted_candidate_net_wei": min(c["net_modeled"] for c in cands),
            "effective_threshold_bound_wei": [MIN_NET_PROFIT_WEI, min(c["net_modeled"] for c in cands)],
            "competitor_wmnt_net_ge_bound": [
                sum(1 for t in wm if t["net"] >= b) for b in (MIN_NET_PROFIT_WEI, min(c["net_modeled"] for c in cands))],
        },
        "day_bounds": {"chain_blocks": [101211644, 101254843], "verified_from_headers": day_bounds_ok, "timestamps": {str(k): v for k, v in ts.items()}},
        "discovery_totals": {
            "full": discovery_totals(os.path.join(host, "ledger.jsonl")),
            "day": discovery_totals(os.path.join(host, "ledger_cut_20260928.jsonl")),
        },
        "coverage": {
            "ledger_observations_full": len(obs),
            "ledger_day_observed_blocks": len(day_ledger_blocks),
            "day_blocks_total": 101254843 - 101211644 + 1,
            "log_block_summary_blocks_total": len(logs),
            "log_day_blocks_by_wall_clock": len(log_day_by_wall),
            "log_day_blocks_by_chain_bounds": len(log_day_by_chain),
            "log_day_chain_blocks_not_in_ledger": len(log_day_by_chain - day_ledger_blocks),
            "log_only_blocks_by_skip_reason": dict(Counter(logs[b]["skip_reason"] for b in log_day_by_chain - day_ledger_blocks)),
            "log_only_blocks_with_candidates_or_evaluation": sum(
                1 for b in log_day_by_chain - day_ledger_blocks if logs[b]["candidates"] or logs[b]["cycles_evaluated"]),
            "day_blocks_in_neither": sorted(set(range(101211644, 101254844)) - log_day_by_chain - day_ledger_blocks),
            "ledger_day_blocks_not_in_log": len(day_ledger_blocks - log_day_by_chain),
            "wall_clock_only_blocks": sorted(log_day_by_wall - log_day_by_chain)[:10],
            "wall_clock_minus_chain_count": len(log_day_by_wall - log_day_by_chain),
            "chain_minus_wall_clock_count": len(log_day_by_chain - log_day_by_wall),
        },
        "ours": {"full": ours_full, "day": ours_day},
        "log_reconciliation": {
            "candidate_rows_with_log_block_summary": len(log_cov),
            "best_net_equals_ledger_net": log_match,
            "blocks_with_more_than_one_candidate": multi,
            # Ledger rows are the recorded top-1 per block (gate closed); the log counts all.
            "day_log_candidate_appearances": sum(logs[b]["candidates"] for b in log_day_by_chain),
            "day_ledger_top1_rows": len(day_cands),
            "day_unrecorded_lower_ranked_appearances": sum(logs[b]["candidates"] for b in log_day_by_chain) - len(day_cands),
        },
        "competitor": {
            "exclusions": exclusions,
            "fee_formula": "fee_total = gas_used*effectiveGasPrice + l1Fee + gas_used*operatorFeeScalar*100 + operatorFeeConstant",
            "fee_check_vs_sender_balance": {
                "checked": fee_check["checked"], "exact": fee_check["exact"],
                "mismatch": [dict(m, sender_nonce_delta_in_block=nonce_chk.get(m["tx"])) for m in fee_check["mismatch"]],
            },
            "operator_fee_per_gas_seen": op_per_gas,
            "dune_gas_used_match": sum(1 for t in txs if t["dune_gas_used_match"]),
            "dune_gas_price_match": sum(1 for t in txs if t["dune_gas_price_match"]),
            "all_252_fee_total": dist([t["fee_total"] for t in txs]),
            "wmnt": comp_all,
            "wmnt_clean": comp_clean,
        },
        "overlap_with_our_routes": overlap,
    }
    with open(out_path, "w") as f:
        json.dump(summary, f, indent=1, sort_keys=False)
        f.write("\n")
    # per-row detail (external, not committed)
    detail = os.environ.get("WHI1414_DETAIL_OUT")
    if detail:
        with open(detail, "w") as f:
            json.dump({"candidates": cands, "competitor": txs}, f, default=list)


if __name__ == "__main__":
    main()
