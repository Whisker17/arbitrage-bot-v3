#!/usr/bin/env python3
"""Read-only RPC fetch for the min_net_profit economics analysis.

Fetches, into an EXTERNAL cache dir (raw receipts are never committed):
  - receipt + tx for every Dune day-feed arb tx;
  - sender native balance at block-1 and block (fee cross-check);
  - block headers for every block that carries one of our ledger candidates;
  - block headers bounding the UTC day.

Usage: MANTLE_RPC_URL=... fetch_rpc.py <dune_csv> <ledger.jsonl> <cache_dir>
Only eth_getTransactionReceipt / eth_getTransactionByHash / eth_getBalance /
eth_getBlockByNumber / eth_getTransactionCount are called. The endpoint URL is never printed.
"""
import csv
import json
import os
import sys
import time
import urllib.request
from concurrent.futures import ThreadPoolExecutor

URL = os.environ["MANTLE_RPC_URL"]
CALLS = 0


def rpc(method, params):
    global CALLS
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).encode()
    for attempt in range(6):
        try:
            req = urllib.request.Request(URL, body, {"Content-Type": "application/json"})
            with urllib.request.urlopen(req, timeout=30) as r:
                out = json.load(r)
            CALLS += 1
            if "error" in out:
                raise RuntimeError(f"{method}: {out['error']}")
            return out["result"]
        except Exception as e:  # noqa: BLE001 - retry transient errors, then fail loudly
            if attempt == 5:
                raise RuntimeError(f"{method} failed after retries: {type(e).__name__}: {e}") from None
            time.sleep(1.5 * (attempt + 1))


def cached(path, fn):
    if os.path.exists(path):
        return json.load(open(path))
    val = fn()
    json.dump(val, open(path, "w"))
    return val


def main():
    dune_csv, ledger, cache = sys.argv[1:4]
    os.makedirs(cache, exist_ok=True)
    rows = list(csv.DictReader(open(dune_csv)))

    def tx_job(r):
        h = r["tx_hash"]
        rc = cached(f"{cache}/receipt_{h}.json", lambda: rpc("eth_getTransactionReceipt", [h]))
        cached(f"{cache}/tx_{h}.json", lambda: rpc("eth_getTransactionByHash", [h]))
        b = int(rc["blockNumber"], 16)
        s = rc["from"]
        for blk in (b - 1, b):
            cached(f"{cache}/bal_{s}_{blk}.json", lambda blk=blk: rpc("eth_getBalance", [s, hex(blk)]))

    blocks = set()
    last_block = None
    for line in open(ledger):
        rec = json.loads(line)
        if rec["row_type"] == "observation":
            last_block = rec["snapshot_id"]["block_number"]
        elif rec["row_type"] == "candidate":
            blocks.add(last_block)
    blocks |= {101211643, 101211644, 101254843, 101254844}

    def blk_job(b):
        cached(f"{cache}/block_{b}.json", lambda: rpc("eth_getBlockByNumber", [hex(b), False]))

    with ThreadPoolExecutor(4) as ex:
        list(ex.map(tx_job, rows))
        list(ex.map(blk_job, sorted(blocks)))

    # Fee cross-check follow-up: where the sender's balance drop != receipt fee,
    # record the sender's nonce delta inside the block (a 2nd tx explains it).
    nonce = {}
    for r in rows:
        h = r["tx_hash"]
        rc = json.load(open(f"{cache}/receipt_{h}.json"))
        tx = json.load(open(f"{cache}/tx_{h}.json"))
        b, s, g = int(rc["blockNumber"], 16), rc["from"], int(rc["gasUsed"], 16)
        fee = (g * int(rc["effectiveGasPrice"], 16) + int(rc.get("l1Fee", "0x0"), 16)
               + g * int(rc.get("operatorFeeScalar", "0x0"), 16) * 100 + int(rc.get("operatorFeeConstant", "0x0"), 16))
        drop = int(json.load(open(f"{cache}/bal_{s}_{b - 1}.json")), 16) - int(json.load(open(f"{cache}/bal_{s}_{b}.json")), 16)
        if int(tx["value"], 16) == 0 and drop != fee:
            n0, n1 = (int(cached(f"{cache}/nonce_{s}_{x}.json", lambda x=x: rpc("eth_getTransactionCount", [s, hex(x)])), 16) for x in (b - 1, b))
            nonce[h] = n1 - n0
    json.dump(nonce, open(f"{cache}/feecheck_nonce.json", "w"))
    print(json.dumps({"txs": len(rows), "blocks": len(blocks), "rpc_calls_this_run": CALLS}))


if __name__ == "__main__":
    main()
