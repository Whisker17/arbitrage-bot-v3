#!/usr/bin/env python3
"""WHI-1413: measure a UniV2-family pair's swap fee from its own on-chain logs.

Read-only. For every `Swap` log the pair emitted in a block range, the `Sync`
log it emitted immediately before (same tx, logIndex-1) gives the reserves
*after* the swap, so reserves *before* = after - in + out. Then, in the
protocol-native fee domain (parts per 100_000):

* `fee_upper_bound` = the largest fee f for which the UniV2 K check
  `(b0*1e5 - in0*f) * (b1*1e5 - in1*f) >= r0*r1*1e10` still holds. The true fee
  can be no larger than the minimum of this over all swaps.
* `exact_match[f]` = how many swaps paid out exactly
  `getAmountOut(in, rIn, rOut, f)` (router-sized swaps do). Only single-sided
  swaps are used for exact matching.

Usage: fee_from_swaps.py <rpc_url> <to_block> <span_blocks> <pool> [<pool> ...]
Output: one JSON object per pool on stdout.
"""
import json
import sys
import time

import requests

SWAP = "0xd78ad95fa46c994b6551d0da85fc275fe613ce37657fb8d5e3d130840159d822"
SYNC = "0x1c411e9a96e071241c2f21f7726b17ae89e3cab4c78be50e062b03a9fffbbad1"
D = 100_000
CANDIDATES = [100, 150, 200, 250, 300, 500]


SESSION = requests.Session()


def rpc(url, method, params, tries=6):
    last = None
    for i in range(tries):
        try:
            r = SESSION.post(url, json={"jsonrpc": "2.0", "id": 1, "method": method, "params": params}, timeout=60)
            if r.status_code == 200 and "error" not in r.json():
                return r.json()["result"]
            last = f"{r.status_code} {r.text[:300]}"
            if r.status_code == 400:
                break
        except requests.RequestException as e:  # transient transport error: retry
            last = repr(e)
        time.sleep(1 + 2 * i)
    raise RuntimeError(last)


def logs(url, pool, frm, to, chunk=10_000):
    out = []
    b = frm
    while b <= to:
        e = min(b + chunk - 1, to)
        out += rpc(url, "eth_getLogs", [{"address": pool, "fromBlock": hex(b), "toBlock": hex(e), "topics": [[SWAP, SYNC]]}])
        b = e + 1
    return out


def words(data):
    h = data[2:]
    return [int(h[i:i + 64], 16) for i in range(0, len(h), 64)]


def amount_out(ain, rin, rout, f):
    if ain == 0 or rin == 0 or rout == 0:
        return 0
    a = ain * (D - f)
    return a * rout // (rin * D + a)


def k_ok(r0, r1, in0, in1, out0, out1, f):
    b0 = r0 + in0 - out0
    b1 = r1 + in1 - out1
    return (b0 * D - in0 * f) * (b1 * D - in1 * f) >= r0 * r1 * D * D


def fee_upper_bound(r0, r1, in0, in1, out0, out1):
    if not k_ok(r0, r1, in0, in1, out0, out1, 0):
        return None  # inconsistent (fee-on-transfer, or misparsed pair)
    lo, hi = 0, D
    while lo < hi:
        mid = (lo + hi + 1) // 2
        if k_ok(r0, r1, in0, in1, out0, out1, mid):
            lo = mid
        else:
            hi = mid - 1
    return lo


def main():
    url, to_block, span = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
    for pool in sys.argv[4:]:
        ls = sorted(logs(url, pool, to_block - span, to_block), key=lambda l: (int(l["blockNumber"], 16), int(l["logIndex"], 16)))
        by_key = {(l["transactionHash"], int(l["logIndex"], 16)): l for l in ls}
        bounds, exact = [], {f: 0 for f in CANDIDATES}
        single, inconsistent = 0, 0
        for l in ls:
            if l["topics"][0] != SWAP:
                continue
            prev = by_key.get((l["transactionHash"], int(l["logIndex"], 16) - 1))
            if prev is None or prev["topics"][0] != SYNC:
                continue
            a0, a1 = words(prev["data"])[:2]
            in0, in1, out0, out1 = words(l["data"])[:4]
            r0, r1 = a0 - in0 + out0, a1 - in1 + out1
            ub = fee_upper_bound(r0, r1, in0, in1, out0, out1)
            if ub is None:
                inconsistent += 1
                continue
            bounds.append(ub)
            if (in0 > 0) != (in1 > 0) and (out0 > 0) != (out1 > 0):
                single += 1
                for f in CANDIDATES:
                    if in0 > 0 and out1 == amount_out(in0, r0, r1, f):
                        exact[f] += 1
                    elif in1 > 0 and out0 == amount_out(in1, r1, r0, f):
                        exact[f] += 1
        print(json.dumps({
            "pool": pool,
            "block_range": [to_block - span, to_block],
            "swaps_paired_with_sync": len(bounds) + inconsistent,
            "inconsistent_k_at_fee_0": inconsistent,
            "fee_upper_bound_min": min(bounds) if bounds else None,
            "fee_upper_bound_mode": max(set(bounds), key=bounds.count) if bounds else None,
            "single_sided_swaps": single,
            "exact_match_by_fee_per_100000": exact,
        }))


if __name__ == "__main__":
    main()
