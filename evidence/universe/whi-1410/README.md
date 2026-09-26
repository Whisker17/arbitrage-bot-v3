# WHI-1410: production universe provenance and the missing `agni-v2` pools

On 2026-09-21 02:01 (+0800) `arb-bot-jp` ran a regenerated 109-pool universe
(fingerprint `0xee1d40b8…`, snapshot 100871945, no `agni-v2`) instead of the
committed 130-pool universe (`0x0ecceac8…`, snapshot 98969898, 5 `agni-v2`).
This directory holds the evidence for issue WHI-1410.

| path | what |
| -- | -- |
| `arb-bot-jp/` | AC-2: the host's CSV + meta + quarantine, copied read-only and byte-identical. Hashes and commands are in `arb-bot-jp/PROVENANCE.md`. |
| `regen-100871945/no-v2-seed/` | Committed generator at the host's block, v2 seed absent. Reproduces the host universe byte for byte. |
| `regen-100871945/with-v2-seed/` | Same block and binary, with the operator-local v2 seed. Counterfactual and per-pool snapshot TVL. |
| `regen-100871945/inputs/poolLists_v2.csv` | The v2 seed used above: the operator workstation's untracked file, copied as is. |
| `regen-100871945/archive-reads/` | Independent `cast` reads at 100871945 and 98969898. |

## Answer: why the regeneration dropped all 5 `agni-v2` pools

**agni-v2 was never scanned.** No TVL filter or quarantine removed these pools.
The generator's v2 seed file does not exist on the host.

1. `universe_gen` seeds agni-v2 only from `--seed-v2`, which defaults to
   `data/poolLists_v2.csv` (`src/bin/universe_gen.rs`, `seed_v2` arg). When
   that file is missing it logs `seed v2 missing — agni-v2 will be empty` and
   continues with zero v2 candidates (`universe_gen.rs`, the `args.seed_v2.exists()`
   branch). It does not fail.
2. `data/poolLists_v2.csv` has **never been committed**. `.gitignore:33` `*.csv`
   ignores it (`git check-ignore -v data/poolLists_v2.csv` →
   `.gitignore:33:*.csv`), and `git log --all -- data/poolLists_v2.csv` is
   empty. The other seeds (`poolLists.csv`, `poolLists_moe.csv`) and the
   universe CSV are force-tracked. The v2 seed exists only as an untracked file
   in the operator's primary clone (3000 bytes, mtime 2026-07-30, sha256
   `9cec28e8…901fd6`, 20 rows). A deploy of the tracked tree cannot include it.
3. On the host, `ls /opt/arbitrage-bot-v3/data/poolLists_v2.csv` →
   `No such file or directory`. The v3 and Moe seeds there are byte-identical
   to the repo (`arb-bot-jp/PROVENANCE.md`).
4. `build_meta` (`src/service/unified_universe.rs`) filled `per_protocol` only
   from the kept pools, so a protocol with zero candidates had no key at all.
   That explains the missing key in the host meta. This PR fixes it (AC-4):
   every `SelectedProtocol` label is now listed, zeros included.
5. **Byte-exact reproduction.** The generator from this branch's clean commit
   `fa45119` (code identical to `origin/dev` apart from the meta zero-count
   fix and a debug log line) was run at the host's block with the v2 seed absent:

   ```bash
   cargo build --locked --release --bin universe_gen
   NO_COLOR=1 ./target/release/universe_gen --rpc https://rpc.mantle.xyz \
     --block 100871945 --seed-v2 /tmp/whi1410/run/does-not-exist.csv \
     --out /tmp/whi1410/run/no-v2/pool_universe.csv
   ```

   (2026-09-26T03:01:56Z → 03:07:29Z, rc=0.) Output:
   - CSV sha256 `bbd60ac3…ce8067`: **identical** to the host CSV.
   - quarantine sha256 `d4ad9c88…6d6028`: **identical** to the host file.
   - meta: identical to the host meta except one added line,
     `"agni-v2": 0,`, from the AC-4 fix (`diff` output below). Fingerprint
     `0xee1d40b8…` is unchanged.

   ```
   $ diff arb-bot-jp/pool_universe.meta.json regen-100871945/no-v2-seed/pool_universe.meta.json
   9a10
   >     "agni-v2": 0,
   ```

   So the host's inputs were the committed v3 and Moe seeds, block 100871945,
   the default 1000 WMNT floor, and **no v2 seed**. The host command line itself
   is not recoverable: there is no log or shell history on the host.

The other 16 removed pools (11 `agni-v3`, 5 `moe`) are unrelated to the v2
seed. The with-v2 run below keeps exactly the host's 76 `agni-v3` and 33 `moe`
rows. They are what the committed seeds give at the later block (TVL and cycle
filters at 100871945 vs 98969898).

## Snapshot-block TVL for each of the 5 committed `agni-v2` pools (block 100871945)

Block 100871945: hash `0x3e32a75c761ba93c6945730a41e88c64510b3fd848cc28eff46ed26813dc34f5`,
timestamp 1789874202. Archive reads came from `https://rpc.mantle.xyz`
(chain id 5000). That is the built-in mainnet default. No `RPC_*`/`MANTLE_*` env var was set, so
the chain-aware precedence resolves to it. The endpoint served the historical
block with no error.

- **Generator TVL**: `universe_gen`'s own valuation (`value_pools_wmnt`,
  `wmnt_reserve_balance_heuristic`) at the pinned block. It comes from the
  with-v2 run (`RUST_LOG=universe_gen=debug`, `snapshot valuation` lines in
  `with-v2-seed/stdout.txt`).
- **Raw balances**: `cast call <token> balanceOf(pool) --block 100871945`
  (`archive-reads/cast_pools_100871945.txt`). They equal `getReserves()` for
  every pool.
- **Market estimate**: raw balances × direct WMNT-pair prices from the deepest
  pairs. USDT = 1.63931 WMNT from `0x3e5922cd` (590.24 WMNT / 360.05 USDT).
  WETH = 4235.93 WMNT from V2 `0x585ec64f` (161.97 WMNT / 0.038237 WETH).
  mETH = 4607.29 WMNT from V2 `0xe1c44356`. USDC is assumed = USDT (the
  USDC/USDT pool holds them ~1:1). The estimate is a sanity check, not a
  project method.

| pool | pair | raw balances @100871945 | generator TVL (WMNT) | market est. (WMNT) | ≥ 1000 floor (generator) | with v2 seed, same block |
| -- | -- | -- | --: | --: | -- | -- |
| `0x351f9beb9881316f25132bb389da91345d89fbff` | USDC/WETH | 15.235540 USDC, 0.005873056 WETH | 868.400044 | 49.85 | no | TVL-filtered |
| `0x3e5922cd0cec71dc2d60ec8b36aa4c05b7c1672f` | USDT/WMNT | 360.053355 USDT, 590.240250 WMNT | 1180.480501 | 1180.48 | yes | **kept** |
| `0x545c3e7c17891b5ad450cb3a2c3f78d310bbc243` | USDT/WETH | 15.500516 USDT, 0.005836917 WETH | 1421.064765 | 50.13 | yes | **kept** |
| `0xd0415fa1725c9274d85b07ec2ceec9551c8dc027` | USDT/mETH | 0.001401 USDT, 0.000000772945 mETH | 1855798.009622 | 0.0059 | yes | **kept** |
| `0xec3757666d6f218d9550976bcc7b7331d4dfd169` | USDC/USDT | 11.138264 USDC, 11.189229 USDT | 541.025385 | 36.60 | no | TVL-filtered |

The counterfactual with the v2 seed present (same binary, same block,
2026-09-26T03:01:56Z → 03:07:50Z, rc=0) gives 112 pools, fingerprint
`0xa43208702a9b3a1aaf6d27fe59a130dde652fa18e70b3c739c4c96ef56eb4dd3`,
`per_protocol` `{agni-v2: 3, agni-v3: 76, moe: 33}`. The funnel for agni-v2 is
enumerated 20 → TVL-surviving 4 → emitted 3. Emitting the v2 seed would have kept
3 of the 5 committed pools and TVL-filtered 2. Dropping **all** 5 needs the
missing seed.

The issue's premise holds at the snapshot block too. `0x3e5922cd` held 590.24
WMNT (TVL 1180.48 WMNT). A TVL floor could not have removed it; it was simply
never enumerated.

### Side finding (not fixed here): the valuation heuristic overprices dust-priced legs

For every pool **without** a WMNT leg, the generator's TVL is 15× to 3×10⁸×
the market estimate. Examples: `0xd0415fa1` holds under 0.01 WMNT of value but is
valued at 1.86 M WMNT, and `0x545c3e7c` holds ~50 WMNT but is valued at 1421.
`value_pools_from_inputs` (`src/service/valuation.rs`) keeps the **highest**
price across all direct WMNT pairs (`if px > *p { *p = px }`), although the
comment there says "prefer the deeper WMNT side". So a single dust WMNT pair can
set a token's price. This is consistent with thin pools clearing the 1000 WMNT
floor. It is likely also how the four thin v2 pools got into the committed
universe at 98969898: their raw balances were just as small there
(`archive-reads/cast_pools_98969898.txt`). Which pools belong in the universe is
out of this issue's scope (coverage / WHI-1413), so this is reported, not fixed.

## Reproducing

- The generator was built from clean commit `fa45119716d5819d8cb9029b853252d81df1f67e`
  on `fix/whi-1410-pin-production-universe`, with `cargo build --locked --release --bin universe_gen`.
- with-v2 command: as above, plus `RUST_LOG=universe_gen=debug,info` and
  `--seed-v2 regen-100871945/inputs/poolLists_v2.csv` (run from `/tmp` copy,
  same sha256 `9cec28e82b5ab3e72c2efc71a754e634ce57489c70a8fa995dbaf9fb3e901fd6`).
- The no-v2 CSV and quarantine are not duplicated here: they are byte-identical to
  `arb-bot-jp/`. `no-v2-seed/` keeps the meta, stdout and rc.
