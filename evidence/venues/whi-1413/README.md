# WHI-1413: UniV2-family venue decision record (Merchant Moe V1 classic)

**Decision:** admit **Merchant Moe V1 classic** (factory `0x5bEf015C…EdEc`, fee `300 / 100_000`).
**Reject MantleSwap V2** (factory `0x5c84e5d2…fD2f`, measured fee `250 / 100_000`).
FusionX V2 (`0xE5020961…Cce7c`) is already loaded. Its fee is corrected from the hard-coded
300 to the measured `200 / 100_000`. The orchestrator approved this decision on 2026-09-26 in
reply to the implementer's need_decision.

Fee units are parts per `100_000`, the protocol-native V2 domain. They are never basis points.

## Denominator (frozen; like-for-like with the issue)

`denominator/` holds the 83-row Dune extract that WHI-1413 was cut from:

- query 8788689, execution `01M30WWX1XTF7EMWFQ81HVNKB7`;
- sha256 `db1593be…a7c9`;
- provenance in `denominator/PROVENANCE.md`.

Extraction rules: multi-hop means `hops >= 2`, which gives **44** arbs. "Fully covered" means every
swap-log pool is in the universe. `coverage_44.py` applies exactly these rules.

`build_inputs.py` turns the 44 into WHI-999 tool inputs (`arbs_44.jsonl`, `census_44.json`):

- **Settlement.** It is taken from each receipt's net ERC-20 flows into the tx's `to`, using the
  WHI-999 `pos` rule. All 44 settle in WMNT, and all have ≤ 3 hops, so **all 44 are in scope**.
- **Venue.** It is taken from the pool's own `factory()` and the factory registry, never from a
  census tag. Baseline reproduction: **17 / 44** (38.6%) against the committed 130-pool universe,
  the issue's figure.

Missing multi-hop pools: 21 in total.

| Venue | Pools |
| --- | --- |
| Moe V1 classic | 7 |
| MantleSwap V2 | 3 |
| FusionX V2 | 3 (these already load; they are excluded by the TVL floor) |
| Cleopatra CL | 2 |
| Uniswap V3 | 1 |
| Butter | 1 |
| V3fork-636e | 1 |
| Algebra-c848 | 1 |
| Solidly-class | 2 |

Correction to the orchestrator's pre-work: Moe V1 accounts for **7** pools, not 8. The two pools
whose `factory()` reverts are identified here as Solidly-class: `bb2f5148…` has `stable()=false`
and `abaff1d3…` has `stable()=true`.

## Fees, measured on chain (`fee_from_swaps.py`)

Method:

- Every `Swap` log of the pair, blocks 100756575..101156575, is paired with the `Sync` it emitted
  just before.
- Reserves before the swap = reserves after − in + out.
- The UniV2 K check then bounds the fee from above.
- Router-sized swaps match `getAmountOut` exactly.

| Pair | Venue | Swaps | K-check upper bound (min) | Exact matches at the fee | Fee |
| --- | --- | ---: | ---: | --- | ---: |
| `0x4e7685df…` USDT/WMNT | Moe V1 classic | 91 | 300 | 54 @300, 0 @200/250 | **300** |
| `0x3e5922cd…` USDT/WMNT | FusionX V2 | 246 | 200 | 196 @200 | **200** |
| `0x94c400b9…` WMNT/WETH | MantleSwap V2 | 94 | 250 | 51 @250, 8 @300 | **250** |

The router differential fixtures at block 98969898 infer the same unique fee over [0, 100000]:

- 5 FusionX V2 pools at 200;
- 5 Moe V1 pools at 300.

See `tests/fixtures/differential/uniswap_v2_*_98969898.json`.

## Is Moe V1 classic structurally drop-in UniV2?

Yes, and this was checked on chain.

**Pair code.** Each pair is a 95-byte immutable-args clone that delegates to implementation
`0x08477e01…c28b`, with `token0 ++ token1` as the immutable args.

**Implementation surface.** Its selectors are the standard UniV2 pair set: `swap(uint256,uint256,address,bytes)`,
`getReserves`, `token0/1`, `skim`, `sync`, `mint`, `burn`, `kLast`, `price{0,1}CumulativeLast`,
`permit`. The extras are `implementation()`, `initialize()` and one admin sweep.

**Behaviour.**

- Router `getAmountsOut` equals the constant-product formula at fee 300, exactly, on 5 pools.
- The executor's `transfer + swap(amount0Out, amount1Out, this, "")` flow (empty data, so no
  callback) is exercised end to end by the fork campaign (`evidence/gas/whi-1413/`).

**CREATE2.**

- The address is `CREATE2(factory, keccak(token0 ++ token1), keccak(0x61005f3d81600a3d39f3 ++ runtime(implementation, token0, token1)))`.
- It reproduces `0x4E7685Df…d847`, which is also the factory's `getPair(USDT, WMNT)`.
- It also reproduces all 17 Moe V1 rows of the candidate universe.

**Naming.** `moe` stays Merchant Moe **Liquidity Book**. V1 classic rows use the shared UniV2
label `agni-v2`, their own `factory`, and venue label `moe-v1`. They never go through LB code.

## Executor: no contract change

`POOL_TYPE_V2` is a factory-agnostic `transfer` + `swap(…, "")`. `registerPool(pool, 0)` reads
`token0/token1` and calls `_maybeVerifyV2`, which only verifies when `venues[POOL_TYPE_V2].enabled`.
That check is a single-factory `_pairForV2` with one init-code hash. It could not verify Moe V1
clones at all, so Moe V1 registration is **allowlist-only, with the V2 venue check left disabled**.
`evidence/deployments/mantle-mainnet-executor.md` records the deployed executor as paused and
unfunded, with no venue registration. Nothing in `contracts/` changed.

## Ranked decision (WHI-999 methodology, frozen 44)

Setup:

- **Tool.** `missed_arb_universe` (`whi999/`), inputs `arbs_44.jsonl` + `census_44.json`.
- **Floor.** The generator's own 1000 WMNT TVL floor, valued with the generator's heuristic.
- **Marginal.** Arbs whose *every* missing pool would be admitted.
- **Script.** `venue_rank.py` does the per-venue split.
- **Admission cost.** The committed universe is augmented with the venue's admissible
  competitor pools, and the tool is re-run on it:
  - cycles: production `PathFinder`, WMNT, ≤ 3 hops;
  - cold start: linear WHI-936 estimate, 350 s per 130 pools.

| Rank | Venue | Fee | Marginal, floor @ window end 100902051 | Marginal, floor @ snapshot 98969898 | Marginal, no floor (upper bound) | Pools / cycles / cold start (snapshot-floor set) | Decision |
| ---: | --- | ---: | ---: | ---: | ---: | --- | --- |
| 1 | **Moe V1 classic** | 300 | **+1** (18/44) | **+3** (20/44) | +8 (25/44) | 134 / 7898 / 361 s (+4 pools, +936 cycles) | **admit**: highest marginal in every view; its fee matches the old default; drop-in math |
| 2 | MantleSwap V2 | 250 | +1 (18/44) | +1 (18/44) | +1 (18/44) | 133 / 7886 / 358 s (+3, +924) | **reject**: +1 in every view. The one arb, `0xbab90663…`, has swap legs WBTC/WETH on MantleSwap and WMNT/WETH on Butter. The factory has only 33 pairs, and its fee (250) would need its own registry entry, fixtures and a gas campaign for one arb in 7 days |
| — | both | — | +2 (19/44) | +4 (21/44) | +9 (26/44) | 137 / 8894 / 369 s | — |
| — | FusionX V2 extra pools (already loadable) | 200 | +0 | +0 | +5 | — | floor-excluded, not venue-excluded |

The cost columns for every view (window end, snapshot and no floor) are in `admission_cost.txt`, produced by `admission_cost.py`.

Baseline: 130 pools, 6962 cycles, 350 s.

**Whole-venue admission cost** is what adopting Moe V1 really costs, because the generator
enumerates the whole factory, not just the competitor's pools. The regen at 98969898
(`regen-98969898/`) enumerated 230 Moe V1 pairs:

- **17** pass the floor and the ≤3-hop WMNT cycle filter.
- The unchanged cycle filter also admits **4** Moe LB pools that now sit on cycles through them.
- The universe goes from 130 to **151 pools** and from 6962 to **8526 cycles** (1.22×, +1564, every
  one touching a Moe V1 pair; production `PathFinder`, from the post-adoption WHI-999 run).
- Estimated cold start goes from 350 s to **407 s**.

**The TVL floor is the real lever, and it is out of scope.** With the floor ignored, Moe V1 alone
would unlock +8. With it, the prompting arb (`0x2025c952…`, WMNT→USDT→ENA→WMNT, all Moe V1) stays
uncovered, because its three pools are below 1000 WMNT:

| Pool | TVL @ window end (WMNT) | TVL @ snapshot (WMNT) |
| --- | ---: | ---: |
| ENA/WMNT | 637 | 512 |
| USDT/ENA | 556 | 2526 |
| USDT/WMNT | 877 | 1064 |

WHI-999 advises against lowering the floor. WHI-1413 does not change it.

## Post-adoption coverage (AC4, same 44, same rules)

The universe regenerated at 98969898 is now committed as `data/pool_universe.csv`:

- 151 pools;
- fingerprint `0x2adb7cd6…e20e50`;
- csv sha256 `b9bee5d6…8c10`.

`config/pool_universe.pin.json` is updated with it. It was committed only after gas
qualification covered every Approved class that Moe V1 reaches (`evidence/gas/whi-1413/REPORT.md`).

`python3 coverage_44.py denominator/dune_8788689_exec_01M30WWX1XTF7EMWFQ81HVNKB7.csv regen-98969898/repro/pool_universe.csv data/pool_universe.csv`
(output in `coverage_44.txt`):

| Universe | Pools | Fully covered (issue rule) |
| --- | ---: | ---: |
| before: committed `0x0ecceac8…` = `regen-98969898/repro` (byte-identical) | 130 | **17 / 44** (38.6%) |
| after: `0x2adb7cd6…` (with Moe V1 classic) | 151 | **20 / 44** (45.5%) |

**Newly covered arbs.** `0x22712df7…`, `0x86c23826…` and `0x958538d2…`. No arb lost coverage.
The +3 equals the snapshot-floor view of the ranking, which cross-checks it.

**Stricter view** (next to the issue rule, not instead of it): WHI-999 re-run on the
new universe, TVL at the window end, in `whi999/post_adoption_tvl_at_window_end.md`.

- **Reachable in scope: 20 / 44.** Moe V1 classic is now `loadable_drop_in`.
- Of the 18 missing pools, 10 are below the TVL floor, including the remaining Moe V1
  pools and the prompting arb's pools.
- The other 8 are on venues that still need an adapter or a fee: MantleSwap V2 (3),
  Cleopatra CL (2), Solidly-class (2) and Algebra (1).

**Loadable and quotable.** Every Moe V1 and FusionX V2 row builds with its measured fee.
Router fixtures confirm the quotes exactly at 98969898 (`tests/differential.rs`
`whi1413_v2_fixtures_quote_exactly_through_the_production_fee_registry`).

**Priced.** Every Approved class carries exact-class fork samples, within its limit,
on every admitted V2 venue it prices in this universe (fix round 1, PR109-F1).
There are 2 new Approved classes, `h3:v2+v2+v2` and `h3:moe+v2+v2:bins=0`.
`h3:v2+moe+v2:bins=0` is withheld: it has no FusionX samples of that exact class,
yet FusionX occurs on 4 of its 12 cycles.

**Still not priced.** The newly covered arbs are counted by pool presence. Whether one
of them is *priced* depends on its route class: V3-hop classes stay Unsupported under
DI-50.

## Effects elsewhere

- **WHI-1423 (deploy).** Merging this PR changes what gets deployed:
  - a 151-pool universe with a new pin;
  - a new gas profile digest `0x87f5d70a…` (fix round 1; `0x6d96ee51…` at 45c0bd9);
  - Moe V1 rows that the executor must register allowlist-only, with the V2 venue
    CREATE2 check left disabled.

  The regen is at the **same** snapshot block 98969898, so it does **not** fix WHI-1423's
  max-age staleness: `DEFAULT_UNIVERSE_MAX_AGE_BLOCKS = 250000`. That remains a separate
  human decision (fresh-block regen or a recorded override). Nothing was deployed or
  registered.
- **WHI-1412 / WHI-1414 (live windows).** Their windows would run on the widened universe
  once deployed. They stay blocked on WHI-1423.
- **DI-50.** The pre-existing `h2:v2+v3:ticks=0` exposure grows from 4/6 to 11/16
  non-Agni `v2+v3` cycles. This is recorded in DI-50, and the V3 factory policy is
  unchanged.
- **New deferred items.** DI-54 records that V2 route classes have no venue axis.
  DI-55 records that the competitor's pools sit below the TVL floor.
