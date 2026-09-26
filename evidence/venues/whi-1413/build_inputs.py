#!/usr/bin/env python3
"""WHI-1413: turn the frozen 44-arb denominator into WHI-999 tool inputs.

Read-only RPC. Inputs: the frozen Dune extract (q8788689 exec
01M30WWX1XTF7EMWFQ81HVNKB7) and the committed universe CSV. Outputs, next to
this script unless --out-dir is given:

* `arbs_44.jsonl` — the multi-hop rows (hops >= 2, the issue's extraction
  rule), in the `missed_arb_universe --arbs` shape: block, hash, path (pools in
  log-index order), nSwaps (= hops), pos (net token flows into the tx's `to`
  contract, from the receipt's ERC-20 Transfer logs — the settlement asset is
  the largest positive leg, WHI-999's rule).
* `census_44.json` — every pool on those paths, in the `--census` shape. The
  factory is the universe CSV's for held pools and the pool's own `factory()`
  otherwise; `kind` comes from the factory registry below (never a census tag),
  and pools whose `factory()` reverts are probed for a Solidly `stable()`.

Usage: build_inputs.py <rpc_url> <dune_csv> <universe_csv> [--out-dir DIR]
"""
import csv
import json
import os
import sys
import time

import requests

TRANSFER = "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef"
SESSION = requests.Session()

# factory (lowercase) -> (label, kind). Kind vocabulary is WHI-999's
# (`v2` / `v3` / `lb` / `algebra` / `solidly`); labels follow evidence/venues/MATRIX.md.
FACTORIES = {
    "0x25780dc8fc3cfbd75f33bfdab65e969b603b2035": ("agni-v3", "v3"),
    "0x530d2766d1988cc1c000c8b7d00334c14b69ad71": ("fusionx-v3", "v3"),
    "0xeeca0a86431a7b42ca2ee5f479832c3d4a4c2644": ("butter", "v3"),
    "0xf883162ed9c7e8ef604214c964c678e40c9b737c": ("fluxion-v3", "v3"),
    "0x636ea278699a300d3a849ab2ce36c891c4ee3da0": ("v3fork-636ea2", "v3"),
    "0x0d922fb1bc191f64970ac40376643808b4b74df9": ("uniswap-v3", "v3"),
    "0xaaa32926fce6be95ea2c51cb4fcb60836d320c42": ("cleopatra-cl", "v3"),
    "0xc848bc597903b4200b9427a3d7f61e3ff0553913": ("algebra-c848", "algebra"),
    "0xa6630671775c4ea2743840f9a5016dcf2a104054": ("moe-lb", "lb"),
    "0xe5020961fa51ffd3662cdf307def18f9a87cce7c": ("fusionx-v2", "v2"),
    "0x5bef015ca9424a7c07b68490616a4c1f094bedec": ("moe-v1", "v2"),
    "0x5c84e5d27fc7575d002fe98c5a1791ac3ce6fd2f": ("mantleswap-v2", "v2"),
}


def rpc(url, method, params, tries=6):
    last = None
    for i in range(tries):
        try:
            r = SESSION.post(url, json={"jsonrpc": "2.0", "id": 1, "method": method, "params": params}, timeout=60)
            j = r.json()
            if "error" not in j:
                return j["result"]
            last = j["error"]
            if "revert" in str(last).lower():
                return None
        except (requests.RequestException, ValueError) as e:
            last = repr(e)
        time.sleep(1 + 2 * i)
    raise RuntimeError(last)


def call(url, to, selector):
    return rpc(url, "eth_call", [{"to": to, "data": selector}, "latest"])


def addr(word):
    return None if not word or len(word) < 66 else "0x" + word[-40:].lower()


def symbol(url, token):
    out = call(url, token, "0x95d89b41")
    if not out or out == "0x":
        return None
    h = out[2:]
    try:
        if len(h) == 64:  # bytes32 symbol
            return bytes.fromhex(h).rstrip(b"\0").decode()
        ln = int(h[64:128], 16)
        return bytes.fromhex(h[128:128 + 2 * ln]).decode()
    except (ValueError, UnicodeDecodeError):
        return None


def main():
    url, dune_csv, universe_csv = sys.argv[1:4]
    out_dir = sys.argv[sys.argv.index("--out-dir") + 1] if "--out-dir" in sys.argv else os.path.dirname(os.path.abspath(__file__))
    held = {r["pool"].lower(): r for r in csv.DictReader(open(universe_csv))}
    rows = [r for r in csv.DictReader(open(dune_csv)) if int(r["hops"]) >= 2]

    arbs, pools = [], []
    for r in rows:
        path = [p.lower() for p in r["pools"].split("|")]
        pools += [p for p in path if p not in pools]
        tx = rpc(url, "eth_getTransactionByHash", [r["hash"]])
        rc = rpc(url, "eth_getTransactionReceipt", [r["hash"]])
        bot = tx["to"].lower()
        net = {}
        for lg in rc["logs"]:
            if lg["topics"] and lg["topics"][0] == TRANSFER and len(lg["topics"]) == 3:
                amt = int(lg["data"], 16) if lg["data"] not in ("0x", "") else 0
                tok = lg["address"].lower()
                if addr(lg["topics"][2]) == bot:
                    net[tok] = net.get(tok, 0) + amt
                if addr(lg["topics"][1]) == bot:
                    net[tok] = net.get(tok, 0) - amt
        pos = [[t, str(a)] for t, a in sorted(net.items()) if a > 0]
        arbs.append({"block": int(r["block_number"]), "hash": r["hash"], "path": path, "nSwaps": int(r["hops"]), "pos": pos})

    census = {}
    for p in pools:
        if p in held:
            factory = held[p]["factory"].lower()
            t0, t1 = held[p]["token0"].lower(), held[p]["token1"].lower()
        else:
            factory = addr(call(url, p, "0xc45a0155"))  # factory()
            t0 = addr(call(url, p, "0x0dfe1681")) or addr(call(url, p, "0x05e8746d"))  # token0() | getTokenX()
            t1 = addr(call(url, p, "0xd21220a7")) or addr(call(url, p, "0xda10610c"))  # token1() | getTokenY()
        label, kind = FACTORIES.get(factory or "", (None, None))
        if factory is None:
            stable = call(url, p, "0x22be3de1")  # stable()
            kind = "solidly" if stable and stable != "0x" else None
            label = f"solidly-class, factory() reverts (stable={int(stable, 16) if kind else '?'})" if kind else "unknown"
        census[p] = {"kind": kind, "factory": factory, "t0": t0, "t1": t1,
                     "s0": symbol(url, t0) if t0 else None, "s1": symbol(url, t1) if t1 else None,
                     "venue": label, "held": p in held}

    with open(os.path.join(out_dir, "arbs_44.jsonl"), "w") as f:
        for a in arbs:
            f.write(json.dumps(a) + "\n")
    with open(os.path.join(out_dir, "census_44.json"), "w") as f:
        json.dump(census, f, indent=1, sort_keys=True)
        f.write("\n")
    print(f"arbs={len(arbs)} pools={len(pools)} held={sum(1 for p in pools if p in held)}")


if __name__ == "__main__":
    main()
