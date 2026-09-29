#!/usr/bin/env python3
"""WHI-1412: derive the committed aggregates from the frozen external inputs.

Reads only external, hash-pinned inputs plus the committed repo artifacts, and
writes compact aggregates (no per-event rows, no raw ledger/log content):

  python3 derive.py --run <RUN dir> --scratch <scratch dir> --repo <worktree> --out aggregates.json

<scratch> must hold the outputs of the commands in COMMANDS.md
(events.jsonl, six_pre.json, six_same.json, block_summary.log).
"""
import argparse
import collections
import csv
import hashlib
import io
import json
import re
import subprocess

DAY_LO, DAY_HI = 101211644, 101254843  # UTC 2026-09-28, chain-timestamp bounds
PK = {"agni-v2": "v2", "agni-v3": "v3", "moe": "moe"}


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def git_show(repo, rev_path):
    return subprocess.run(["git", "-C", repo, "show", rev_path], check=True, capture_output=True).stdout


def universe(csv_bytes):
    rows = list(csv.DictReader(io.StringIO(csv_bytes.decode())))
    return {r["pool"].lower(): PK[r["protocol"]] for r in rows}, rows


def profile_status(profile_bytes):
    a = json.loads(profile_bytes)
    st = {}
    for p in a["profiles"]:
        k = p["route_key"]
        st[(tuple(k["protocols"]), k.get("v3_tick_crossings"), k.get("moe_bin_crossings"))] = p["status"]
    return a["content_digest"], st


def august_class(protocols, st):
    """Engine at the August commits (e574540 / 439afed, WHI-949 f308927):
    `topology_route_key` builds the ZERO-bucket key and `fee_plan_cost` rejects
    unless that exact key is approved (absent key -> unknown)."""
    key = (tuple(protocols), "0" if "v3" in protocols else None, "0" if "moe" in protocols else None)
    s = st.get(key)
    return "unknown" if s is None else ("supported" if s == "approved" else "unapproved")


WMNT = "0x78c1b0c915c4faa5fffa6cabf0219da63d7f4cb8"
TRANSFER = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"


def peer_receipt(path, executor):
    """Read-only receipt (cast receipt --json): WMNT net into the peer's executor and fees."""
    r = json.load(open(path))
    net = 0
    for log in r["logs"]:
        if log["address"].lower() == WMNT and log["topics"][0] == TRANSFER:
            frm, to = "0x" + log["topics"][1][-40:], "0x" + log["topics"][2][-40:]
            v = int(log["data"], 16)
            net += v if to == executor else 0
            net -= v if frm == executor else 0
    return {
        "peer_receipt_status": int(r["status"], 16),
        "peer_receipt_tx_index": int(r["transactionIndex"], 16),
        "peer_wmnt_net_into_executor_wei": str(net),
        "peer_l2_gas_cost_wei": str(int(r["gasUsed"], 16) * int(r["effectiveGasPrice"], 16)),
        "peer_l1_fee_wei": str(int(r.get("l1Fee") or "0x0", 16)),
    }


def unix_of_dune_time(t):
    # "2026-09-28 00:03:20.000 UTC"
    import datetime
    d = datetime.datetime.strptime(t.replace(" UTC", ""), "%Y-%m-%d %H:%M:%S.%f")
    return int(d.replace(tzinfo=datetime.timezone.utc).timestamp())


def timing(n, peer_block_time, same_route, obs, summ):
    """Wall-clock evidence for a prior-block-state match. Candidate / log times are
    the shadow host's clock; block timestamps are the sequencer's. No offset bound
    between the two clocks is captured, and both ledger fields are whole seconds."""
    peer_ts = unix_of_dune_time(peer_block_time)
    assert peer_ts == 1790553600 + 2 * (n - DAY_LO), (n, peer_ts)  # 2 s blocks, RPC-checked day bounds
    rows = []
    for c in sorted(same_route, key=lambda c: c["obs_block"]):
        b = c["obs_block"]
        rows.append({
            "state_block": b,
            "offset": f"N-{n - b}",
            "state_block_timestamp": obs[b]["header"]["block_timestamp"],
            "observation_recorded_at_unix": obs[b]["recorded_at_unix"],
            "candidate_recorded_at_unix": c["recorded_at_unix"],
            "block_summary_log_time": summ[b]["log_time"],
            "candidate_recorded_minus_state_block_s": c["recorded_at_unix"] - obs[b]["header"]["block_timestamp"],
            "candidate_recorded_minus_peer_block_s": c["recorded_at_unix"] - peer_ts,
        })
    return {"peer_block_timestamp": peer_ts, "state_blocks": rows}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--run", required=True)
    ap.add_argument("--scratch", required=True)
    ap.add_argument("--repo", required=True)
    ap.add_argument("--out", required=True)
    a = ap.parse_args()
    host = f"{a.run}/sept28-inputs/host"
    dune = f"{a.run}/sept28-inputs/dune"
    S = a.scratch

    out = collections.OrderedDict()
    out["schema_version"] = "whisker-arb/whi-1412-aggregates/v1"

    # ---- ledger day cut: observations, coverage, candidates -------------------
    obs = {}
    cands = []
    last = None
    ev_sum = collections.Counter()
    scope = collections.Counter()
    with open(f"{host}/ledger_cut_20260928.jsonl") as f:
        for line in f:
            r = json.loads(line)
            if r["row_type"] == "observation":
                b = r["snapshot_id"]["block_number"]
                obs[b] = r
                last = b
                d = r.get("discovery") or {}
                scope[d.get("scope")] += 1
                ev_sum["cycles_optimized"] += d.get("cycles_optimized") or 0
                ev_sum["paths_quoted"] += d.get("paths_quoted") or 0
                for k, v in (d.get("rejects") or {}).items():
                    ev_sum["reject_" + k] += v
                if (d.get("cycles_optimized") or 0) > 0:
                    ev_sum["blocks_cycles_optimized_gt0"] += 1
                    ev_sum[f"blocks_evaluated_{d.get('scope')}"] += 1
                    if d.get("scope") == "full":
                        ev_sum["full_cycles_optimized"] += d["cycles_optimized"]
                        ev_sum["full_paths_quoted"] += d.get("paths_quoted") or 0
            elif r["row_type"] == "candidate":
                sig = re.search(r"signature=(\S+)", r["detail"]).group(1)
                pools = [h.rsplit("/", 1)[1].lower() for h in sig.split("|")]
                cands.append({"obs_block": last, "pools": pools, "kind": r["outcome"]["kind"],
                              "recorded_at_unix": r["recorded_at_unix"],
                              "reason": r["detail"].split()[0], "protos": re.findall(r"(v2|v3|moe):0x", sig)})
    blocks = set(obs)
    unobserved = [b for b in range(DAY_LO, DAY_HI + 1) if b not in blocks]
    ev = dict(ev_sum)
    ev["paths_quoted_over_cycles_optimized"] = ev["paths_quoted"] / ev["cycles_optimized"]
    ev["unapproved_over_cycles_optimized"] = ev["reject_unapproved_route"] / ev["cycles_optimized"]
    ev["full_paths_quoted_over_full_cycles"] = ev["full_paths_quoted"] / ev["full_cycles_optimized"]
    out["ledger_day"] = {
        "chain_day_blocks": DAY_HI - DAY_LO + 1,
        "observed_blocks": len(blocks),
        "unobserved_blocks": len(unobserved),
        "observation_rows_by_scope": dict(scope),
        "evaluation": ev,
        "candidate_rows": len(cands),
        "candidate_outcomes": dict(collections.Counter(f'{c["kind"]}/{c["reason"]}' for c in cands)),
    }

    # ---- ledger vs log block coverage reconciliation --------------------------
    pat = re.compile(r'block_summary block=(\d+) .*?candidates=(\d+) .*?best_net="([^"]*)" attempt_outcome="([^"]*)" skip_reason="([^"]*)"')
    summ = {}
    wall_0928 = set()
    with open(f"{S}/block_summary.log") as f:
        for line in f:
            m = pat.search(line)
            b = int(m.group(1))
            if b in summ:
                raise SystemExit(f"duplicate block_summary for {b}")
            summ[b] = {"candidates": int(m.group(2)), "best_net": m.group(3), "attempt": m.group(4), "skip": m.group(5),
                       "log_time": line.split()[0]}
            if line.startswith("2026-09-28"):
                wall_0928.add(b)
    day_summ = {b: v for b, v in summ.items() if DAY_LO <= b <= DAY_HI}
    skip = collections.Counter(v["skip"] for v in day_summ.values())
    log_only = [b for b in day_summ if b not in blocks]
    assert all(day_summ[b]["skip"] != "-" for b in log_only)
    assert all(b in day_summ and day_summ[b]["skip"] == "-" for b in blocks)
    never = [b for b in range(DAY_LO, DAY_HI + 1) if b not in summ]
    out["ledger_log_reconciliation"] = {
        "log_wall_clock_dated_2026_09_28_distinct_blocks": len(wall_0928),
        "log_wall_clock_dated_range": [min(wall_0928), max(wall_0928)],
        "log_chain_day_distinct_blocks": len(day_summ),
        "log_chain_day_by_skip_reason": dict(skip),
        "processed_blocks_all_have_exactly_one_ledger_observation": True,
        "log_only_blocks_all_skipped": len(log_only),
        "chain_day_blocks_without_any_block_summary": never,
        "identity": f"{DAY_HI - DAY_LO + 1} = {len(blocks)} processed + {len(log_only)} skipped + {len(never)} never summarized",
        "wall_vs_chain": f"{len(wall_0928)} = {len(day_summ)} chain-day - {len([b for b in day_summ if b not in wall_0928])} logged after 00:00Z 09-29 + {len([b for b in wall_0928 if b < DAY_LO])} previous-day blocks logged after 00:00Z 09-28",
    }
    out["block_summary_attempt_outcome_chain_day"] = dict(collections.Counter(v["attempt"] for v in day_summ.values()))
    # candidate block cross-check: every candidate row's preceding observation block
    # has candidates>0 in its block_summary line
    out["candidate_block_crosscheck"] = {
        "candidate_rows": len(cands),
        "rows_whose_preceding_observation_block_summary_has_candidates_gt0":
            sum(1 for c in cands if summ[c["obs_block"]]["candidates"] > 0),
    }

    # ---- tool outputs ----------------------------------------------------------
    six_pre = json.load(open(f"{S}/six_pre.json"))
    six_same = json.load(open(f"{S}/six_same.json"))
    events = {json.loads(l)["tx_hash"]: json.loads(l) for l in open(f"{S}/events.jsonl")}
    dune_rows = {r["tx_hash"].lower(): r for r in csv.DictReader(open(f"{dune}/arb_detail_feed_20260928.csv"))}
    assert len(events) == len(dune_rows) == six_pre["event_count"] == 252
    out["six_way_pre_state"] = {k: v for k, v in six_pre.items() if k != "events"}
    out["six_way_same_block_keying"] = {k: six_same[k] for k in ("buckets", "residual", "out_of_strategy_scope")}

    # bucket x tx_index (tx_index 1 = first user tx after the system deposit: its
    # pre-state IS the post-(N-1) state)
    byb = collections.defaultdict(collections.Counter)
    for e in six_pre["events"]:
        byb[e["bucket"]]["tx_index_1" if dune_rows[e["tx_hash"]]["tx_index"] == "1" else "tx_index_gt1"] += 1
    out["bucket_by_tx_index"] = {k: dict(v) for k, v in byb.items()}

    # profitable_but_not_attempted evidence, per event (no tx hashes committed)
    pbna = []
    for e in six_pre["events"]:
        if e["bucket"] != "profitable_but_not_attempted":
            continue
        n = e["block_number"]
        r = dune_rows[e["tx_hash"]]
        same_route = [c for c in cands if c["pools"] == events[e["tx_hash"]]["ordered_pools"] and n - 3 <= c["obs_block"] < n]
        pbna.append({
            "event_block": n,
            "tx_index": int(r["tx_index"]),
            "peer": r["bot_address"][:10] + "…",
            "topology": e["topology"],
            "candidate_obs_blocks": sorted(c["obs_block"] for c in same_route),
            "candidate_outcome": sorted({f'{c["kind"]}/{c["reason"]}' for c in same_route}),
            "block_summary_best_net_wei_at_N_minus_1": summ[n - 1]["best_net"],
            "block_summary_candidates_at_N_minus_1": summ[n - 1]["candidates"],
            **peer_receipt(f"{S}/rcpt_{e['tx_hash']}.json", r["executor_address"]),
            "timing": timing(n, r["block_time"], same_route, obs, summ),
        })
    out["profitable_but_not_attempted_evidence"] = pbna
    offs = [t["candidate_recorded_minus_peer_block_s"] for p in pbna for t in p["timing"]["state_blocks"]]
    out["profitable_but_not_attempted_timing_summary"] = {
        "candidate_rows_compared": len(offs),
        "recorded_strictly_before_peer_block_timestamp": sum(1 for o in offs if o < 0),
        "recorded_same_second_as_peer_block_timestamp": sum(1 for o in offs if o == 0),
        "recorded_after_peer_block_timestamp": sum(1 for o in offs if o > 0),
        "n_minus_1_offsets_s": [p["timing"]["state_blocks"][-1]["candidate_recorded_minus_peer_block_s"] for p in pbna],
        "n_minus_2_offsets_s": [p["timing"]["state_blocks"][0]["candidate_recorded_minus_peer_block_s"] for p in pbna],
        "reading": "timely availability UNPROVEN: no candidate row is recorded before the peer's block timestamp; "
                   "host clock (recorded_at_unix, block_summary log time) vs sequencer block timestamps has no "
                   "captured offset bound and 1 s quantization, so this is not a latency measurement either",
    }
    lag = sorted(o["recorded_at_unix"] - o["header"]["block_timestamp"] for o in obs.values())
    q = lambda f: lag[min(len(lag) - 1, int(f * len(lag)))]
    out["observation_recorded_minus_block_timestamp_s"] = {
        "n": len(lag), "min": lag[0], "p10": q(0.10), "p50": q(0.50), "p90": q(0.90), "p99": q(0.99), "max": lag[-1],
        "note": "host clock minus chain timestamp over every processed block of the day (same clock caveat)",
    }

    # candidate rows not matched to any peer event on the same route in the next block
    ev_keys = {(e["block_number"], tuple(e["ordered_pools"])) for e in events.values()}
    unmatched = collections.Counter()
    for c in cands:
        if (c["obs_block"] + 1, tuple(c["pools"])) not in ev_keys:
            unmatched["h%d:%s" % (len(c["protos"]), "+".join(c["protos"]))] += 1
    out["candidate_rows_without_peer_event_next_block"] = dict(unmatched)

    # absent pools: which Dune project sits at the missing hop
    u_now, _ = universe(open(f"{a.repo}/data/pool_universe.csv", "rb").read())
    proj = collections.Counter()
    distinct_missing = set()
    for e in six_pre["events"]:
        if e["bucket"] != "absent_pool":
            continue
        r = dune_rows[e["tx_hash"]]
        for p, pj in zip(r["ordered_pools"].split(";"), r["ordered_projects"].split(";")):
            if p.lower() not in u_now:
                proj[pj] += 1
                distinct_missing.add(p.lower())
    out["absent_pool_missing_hops_by_dune_project"] = dict(proj)
    out["absent_pool_distinct_missing_pools"] = len(distinct_missing)

    # out-of-scope cross-tab: would these also have an absent pool?
    oos_abs = collections.Counter()
    for e in six_pre["events"]:
        if e["bucket"].startswith("out_of_scope"):
            pools = events[e["tx_hash"]]["ordered_pools"]
            oos_abs[e["bucket"] + ("/some_pool_absent" if any(p not in u_now for p in pools) else "/all_pools_in_universe")] += 1
    out["out_of_scope_by_pool_membership"] = dict(oos_abs)

    # ---- August counterfactual on the same 252 peer arbs ----------------------
    u_aug, _ = universe(git_show(a.repo, "e574540:data/pool_universe.csv"))
    aug_digest, aug_st = profile_status(git_show(a.repo, "e574540:config/gas_profiles/mantle_mainnet_v1.json"))
    cf = collections.Counter()
    cf_topo = collections.defaultdict(collections.Counter)
    for e in six_pre["events"]:
        if e["bucket"].startswith("out_of_scope"):
            cf["out_of_strategy_scope"] += 1
            continue
        pools = events[e["tx_hash"]]["ordered_pools"]
        if any(p not in u_aug for p in pools):
            cf["absent_pool"] += 1
            continue
        protos = [u_aug[p] for p in pools]
        c = "route_class_" + august_class(protos, aug_st)
        cf[c] += 1
        cf_topo[c]["h%d:%s" % (len(protos), "+".join(protos))] += 1
    out["august_counterfactual_on_2026_09_28_peer_arbs"] = {
        "universe_fingerprint": json.loads(git_show(a.repo, "e574540:data/pool_universe.meta.json"))["fingerprint"],
        "profile_content_digest": aug_digest,
        "predicate": "zero-bucket topology key must be approved (e574540 topology_route_key + fee_plan_cost)",
        "counts": dict(cf),
        "topologies": {k: dict(v) for k, v in cf_topo.items()},
    }

    out["day_bounds_rpc_block_timestamps"] = {
        int(b): int(t) for b, t in (l.split() for l in open(f"{S}/day_bounds.txt"))
    }
    json.dump(out, open(a.out, "w"), indent=1, sort_keys=False)
    open(a.out, "a").write("\n")


if __name__ == "__main__":
    main()
