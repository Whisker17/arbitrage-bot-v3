# WHI-1520: pinned pool universe regenerated at a fresh, hash-pinned block

**Result.** The committed universe is now snapshot **101165208**. It has 124 pools and
fingerprint `0x4c2456ddbe945beb0ceabdd825b8329cd5f16216ab3c7d727a1e35631c1a6188`.

- CSV, meta, quarantine and `config/pool_universe.pin.json` are committed together.
- The mainnet gas profile is re-qualified on this universe. See `evidence/gas/whi-1520/REPORT.md`:
  - 6 Approved classes (base: 7; `h2:v2+moe:bins=0` is withheld);
  - `content_digest` `0x3d3244e391f4bfd33298435a51920dfea53027f1b32c2d05f4807e801a3df412`.
- Policy is unchanged:
  - venue set;
  - TVL floor;
  - `EFFECTIVE_MAX_HOPS`;
  - settlement asset;
  - `DEFAULT_UNIVERSE_MAX_AGE_BLOCKS = 250000`.

## Block time: 2.0 s/block, not 0.5 s/block

The issue text says 0.5 s/block and "250000 blocks ≈ 34.7 h". Both are wrong: the chain produces
**0.5 blocks/s**, which is 2.0 s/block. The issue's own numbers show this:

- 98969898 has timestamp 1786070108, and 101165754 has timestamp 1790461820.
- 4391712 s / 2195856 blocks = **2.0 s/block**.

The same rate was observed on the way:

- At 22:37:44Z the tip was 101165973 (ts 1790462258) and finalized was 101165208 (ts 1790460728): 765 blocks in 1530 s.
- At 23:16:27Z the tip was 101167134. That is 1926 blocks after B*, and 3856 s have passed since B*'s timestamp.

The `DEFAULT_UNIVERSE_MAX_AGE_BLOCKS` doc comment agrees: "~2 s/block … 250_000 ≈ ~5.8 days".
Consequences:

- **Freshness decays at ~1800 blocks/h.** The 250000-block budget is **≈ 138.9 h (≈ 5.8 days)**
  after B*'s timestamp (2026-09-26T22:12:08Z). It runs out around **2026-10-02 ~17:05Z**.
- The AC's "≤ 50000 blocks behind tip at PR time" is ≈ 27.8 h after B*, i.e. until about 2026-09-28 02:00Z.

## Block

B* = **101165208**.

- Hash: `0xa915e1b87da99988c368e479e652abf03e92dfb0a12d24f9fd04644374a6e5c1`.
- Timestamp: 1790460728 (2026-09-26T22:12:08Z).
- It was the `finalized` block at 22:37:44Z, when the tip was 101165973 (`0x5c472952…d944`).
- A finalized block was chosen so that the pinned hash cannot reorg away.
- The hash was read from `https://rpc.mantle.xyz`, chain id 5000. It was checked again on the local anvil fork
  (`cast block 101165208 --field hash`) before each campaign run.

## Commands

Binary:

- `cargo build --locked --release --bin universe_gen` at clean `2b13b8f`, which is `origin/dev`.
  None of the commits on this branch touch the generator.
- sha256 `0dbc3a27d9bf9f4898dba1710888830acd3afbdb6ff40eaa786b90e5285c9564`.

Seeds:

- Committed `data/poolLists.csv` and `data/poolLists_moe.csv`.
- The WHI-1410 operator v2 seed `evidence/universe/whi-1410/regen-100871945/inputs/poolLists_v2.csv`,
  sha256 `9cec28e8…901fd6`, which has no `Factory` column.
- Flags are exactly those of WHI-1413 (`evidence/venues/whi-1413/regen-98969898/COMMANDS.md`), with
  default floors. No `RPC_*`, `MANTLE_*`, `UNIVERSE_GEN_*` or `AGNI_*` env var was set.

```bash
NO_COLOR=1 ./universe_gen --rpc https://rpc.mantle.xyz --block 101165208 \
  --seed-v2 evidence/universe/whi-1410/regen-100871945/inputs/poolLists_v2.csv \
  --v2-venues moe-v1 --out <dir>/pool_universe.csv
```

| run | UTC | rc | stdout |
|---|---|---|---|
| 1 | 22:37:57 → 22:41:34 | 0 | `regen-101165208/run1/` |
| 2 | 22:41:34 → 22:44:55 | 0 | `regen-101165208/run2/` |
| 3 (`RUST_LOG=universe_gen=debug,info`, for per-pool TVL) | 23:23 → 23:26 | 0 | `regen-101165208/run3-debug/` |

**Reproducible (AC2).** `cmp` shows all three runs byte-identical for `pool_universe.csv`,
`.meta.json` and `.quarantine.json`. They are also identical to the committed `data/` files.

| file | sha256 |
|---|---|
| `pool_universe.csv` | `702d7a321041ee7011511aaee7186ea9903578adde89f400956dfa23f61ccae0` (= pin `csv_sha256`) |
| `pool_universe.meta.json` | `98e4b9e5a569211466bcc810f4cbf5dc42a0215e8af8a5357a396d7568315521` |
| `pool_universe.quarantine.json` | `f1b5edea75888af71350730a5c331a9e54d18c752a31582b9c8cf4550fb7710a` (byte-identical to the 2b13b8f file) |

The runs' stdout differs only in the `--out` path and in log timestamps.

**Venue set and V2 factories.** Both are unchanged:

- There are 17 `agni-v2` rows: 15 Moe V1 classic (`0x5bEf…EdEc`, fee 300) and 2 FusionX V2 (`0xE502…Cce7c`, fee 200).
- Every row carries its registered factory; the generator aborts on an unregistered factory.
- `committed_config_covers_every_admitted_v2_venue_and_universe_v2_row` passes. Every V2 row
  CREATE2-derives under its own factory's `approved_pools` entry, and `approved_pools` is unchanged.

## Funnel (same inputs, same policy, different block)

| | enumerated | tvl_ok | quarantine | cycle_ok = emitted |
|---|---:|---:|---:|---:|
| 98969898 (WHI-1413, 151) | 654 (v2 250 / v3 212 / moe 192) | 224 (45 / 125 / 54) | 21 | 151 (22 / 87 / 42) |
| **101165208** | 654 (250 / 212 / 192) | 199 (37 / 112 / 50) | 21 | **124 (17 / 75 / 32)** |

Nothing changed in the inputs or the filters between the two runs:

- the enumerated set;
- the quarantine file;
- the meta `filter_policy` (floor `1000000000000000000000` wei WMNT, `max_hops` 3, same valuation method);
- the binary-equivalent flags.

The shrinkage is therefore **market movement only**: TVL at B* under the unchanged floor, and the
unchanged ≤3-hop WMNT cycle filter iterated to a fixed point.

## Membership diff vs the 151-pool universe (dev 2b13b8f)

The new universe is a **strict subset** of the old one: 27 pools removed, 0 added.

| venue | 151 | 124 | added | removed |
|---|---:|---:|---:|---:|
| agni-v3 | 33 | 29 | 0 | 4 |
| butter | 16 | 16 | 0 | 0 |
| fluxion-v3 | 14 | 13 | 0 | 1 |
| fusionx-v2 | 5 | 2 | 0 | 3 |
| fusionx-v3 | 14 | 9 | 0 | 5 |
| moe-lb | 42 | 32 | 0 | 10 |
| moe-v1 | 17 | 15 | 0 | 2 |
| uniswap-v3 | 6 | 6 | 0 | 0 |
| v3fork-636ea2 | 4 | 2 | 0 | 2 |
| **total** | **151** | **124** | **0** | **27** |

TVL of each removed pool at B* is the generator's own valuation, from the `snapshot valuation` lines
in `run3-debug/stdout.txt`:

- 18 fell below the 1000 WMNT floor;
- 9 still clear the floor but lost their last ≤3-hop WMNT cycle once the others were removed.

| # | Pool | Venue | TVL @101165208 (WMNT) | Removed by |
|---:|---|---|---:|---|
| 1 | `0x20b581f2ba0b5ab90799a0917ce94074767f2e73` | agni-v3 | 675.6 | TVL floor |
| 2 | `0x2bd0f40c241eabd326545a6467bb2da88bb46181` | agni-v3 | 23.1 | TVL floor |
| 3 | `0x5e91619cf346bf692287af1e18219ccffb641c6b` | agni-v3 | 4301.7 | cycle filter |
| 4 | `0x7b3a4b36b0c5c95142afcd1b883ed055aa166f85` | agni-v3 | 300.6 | TVL floor |
| 5 | `0x9da97193a7d0764ac597afe5b08e960426cb61af` | fluxion-v3 | 0.1 | TVL floor |
| 6 | `0x351f9beb9881316f25132bb389da91345d89fbff` | fusionx-v2 | 411.6 | TVL floor |
| 7 | `0x545c3e7c17891b5ad450cb3a2c3f78d310bbc243` | fusionx-v2 | 992.9 | TVL floor |
| 8 | `0xec3757666d6f218d9550976bcc7b7331d4dfd169` | fusionx-v2 | 531.9 | TVL floor |
| 9 | `0x01845ec86909006758de0d57957d88da10bf5809` | fusionx-v3 | 714.8 | TVL floor |
| 10 | `0x4a313244ccddd402ef8c3b2c0bcbbd31782a5f88` | fusionx-v3 | 156.9 | TVL floor |
| 11 | `0x7415a4ba496dea52202771fb6a477933c28cfa1f` | fusionx-v3 | 264.2 | TVL floor |
| 12 | `0xc3e2f59ee3ea98bbc62455786d1470db0e7902d9` | fusionx-v3 | 488.2 | TVL floor |
| 13 | `0xe87e42ff34d6baaf619eb91dd957e4ec45226894` | fusionx-v3 | 421.9 | TVL floor |
| 14 | `0x1f20f0895df44d33cf8144a52de811871a21ef4b` | moe-lb | 220623.8 | cycle filter |
| 15 | `0x5a59359a1ad9b0a59aa70145dfeceb6d9ee07253` | moe-lb | 1990.7 | cycle filter |
| 16 | `0x80e894772df557a26625125a3757585c07fd1ae2` | moe-lb | 928.7 | TVL floor |
| 17 | `0x86aa95df876b67373eba42763a9c601d9818cffc` | moe-lb | 182475311.1 | cycle filter |
| 18 | `0x9ee2a2f30932e3633f0e2fca91eff4cb0bf814c4` | moe-lb | 60046222872.7 | cycle filter |
| 19 | `0xaa5b9a9b7804d7748b385f758efb266aa780a982` | moe-lb | 240.4 | TVL floor |
| 20 | `0xcc631a92a5538646db422c9fffbc6ea48cd2243a` | moe-lb | 1273242857589.2 | cycle filter |
| 21 | `0xef61d2e796901c7137bb1952c787a8fe36ec00c1` | moe-lb | 23254988.0 | cycle filter |
| 22 | `0xf53b930d94d687b7de1562beedfe7e31934dbd6a` | moe-lb | 332574.7 | cycle filter |
| 23 | `0xf82ea495de6ac4e436898a726bfe5e271c3657aa` | moe-lb | 0.1 | TVL floor |
| 24 | `0x4e7685df06201521f35a182467feefe02c53d847` | moe-v1 | 815.6 | TVL floor |
| 25 | `0x677a472ddced659092d93882c500342c010ad3be` | moe-v1 | 8956.8 | cycle filter |
| 26 | `0x379ff10d8987ec1a74ea2a8ea5ae1bd6e886c020` | v3fork-636ea2 | 974.0 | TVL floor |
| 27 | `0x707121f3e8103b75c0d76b2be691b5f65067a8c0` | v3fork-636ea2 | 999.1 | TVL floor |

Some cycle-filtered Moe LB pools show astronomically large TVL figures. These come from the known
dust-price valuation heuristic (WHI-1410 side finding). That heuristic is out of scope here and was
not changed.

**Cycles.** Production `PathFinder` (`service::count_settlement_cycles`, WMNT, ≤ 3 hops) gives
8526 → **5562**. The count is asserted equal to the guard's enumerator in
`tests/gas_profile_fork_provenance.rs`.

**Estimated cold start.** The linear WHI-936 estimate is 350 s per 130 pools. It gives 407 s for
151 pools and **≈ 334 s for 124 pools**.

**One loss that matters.** Moe V1 USDT/WMNT `0x4e7685df…` fell below the floor, at 815.6 WMNT.
Two consequences:

- 297 of the Moe V1 gas samples touched it, so the gas guards had to be re-run. See the gas report
  and DI-56.
- The two 44-arb hits it carried were lost (below).

## Guards on the new universe (final HEAD)

| guard | result |
|---|---|
| `committed_universe_matches_launcher_pin` | pass |
| `committed_universe_topologies_have_zero_unknown_route` | pass (36 topologies, 0 unknown, 306 keys explicit) |
| `committed_approved_classes_are_measured_on_every_v2_venue_they_price` | pass (after re-qualification + withholds) |
| `committed_approved_v2_classes_are_measured_on_the_universes_moe_v1_pools` | pass (after re-qualification + withholds) |
| `committed_profile_approvals_respect_pr108_factory_and_lever_fences` | pass |
| `venue_guard_rejects_the_45c0bd9_approval_of_h3_v2_moe_v2_bins_0` | pass, unchanged (still exactly the FusionX 4-cycle gap) |
| `committed_config_covers_every_admitted_v2_venue_and_universe_v2_row` | pass |
| `replaying_production_universe_fails_closed_under_v3_moe_protocols` (WHI-1408 gate over the committed universe) | pass |

On the first run, both gas guards failed on the new universe. They were re-qualified by three fork
campaigns at B*, and one class, `h2:v2+moe:bins=0`, was withheld fail-closed. This used three
orchestrator-approved deviations:

- a wider harness balance-slot probe;
- a measurement-only cycle file for `h2:v2+v2`;
- test fixtures that follow the profile.

Details are in `evidence/gas/whi-1520/REPORT.md`.

## Startup gates

- `cargo run --locked --bin bot -- --offline`: rc=0 at the final HEAD.
- **Live startup gates against the committed files.** `startup-gate/whi1520_startup_gate_scratch.rs`
  was run as a scratch integration test and is not kept in `tests/`. It calls the library
  functions `bot.rs` calls at live startup:
  - `UnifiedPoolUniverseSource` (all protocols);
  - `enforce_universe_freshness` with `DEFAULT_UNIVERSE_MAX_AGE_BLOCKS`;
  - `assert_universe_gas_profile_compatibility` with the committed mainnet profile.

  It was run twice with a tip observed read-only from `https://rpc.mantle.xyz`. The first run
  used the interim profile; the second used the final committed profile:

  ```
  2026-09-26T23:16:27Z  cast block-number → 101167134   (interim profile 0xde16710c…)
  GATES OK pools=124 … snapshot_block=101165208 tip=101167134 age_blocks=1926 remaining_blocks=248074
  2026-09-26T23:58:59Z  cast block-number → 101168410   (final profile)
  GATES OK pools=124 fingerprint=0x4c2456ddbe945beb0ceabdd825b8329cd5f16216ab3c7d727a1e35631c1a6188 snapshot_block=101165208 tip=101168410 age_blocks=3202 max_age=250000 remaining_blocks=246798 profile=0x3d3244e391f4bfd33298435a51920dfea53027f1b32c2d05f4807e801a3df412
  test result: ok. 1 passed
  ```

  The same test asserts that the gate rejects snapshot + 250001. For comparison, the old snapshot 98969898 is
  2197236 blocks behind the first tip and would fail.

## Freshness budget

The budget is 250000 blocks at 2.0 s/block ≈ **138.9 h**. It decays at **~1800 blocks/h**.

| observed | tip | age (blocks) | remaining (blocks) | remaining (h @ 2.0 s) |
|---|---:|---:|---:|---:|
| 2026-09-26T23:16:27Z | 101167134 | 1926 | 248074 | ≈ 137.8 |
| 2026-09-26T23:58:59Z | 101168410 | 3202 | 246798 | ≈ 137.1 |

The PR-time observation is recorded in the PR body and the handoff. The universe must reach a tagged
deploy before about **2026-10-02 17:05Z**; after that it has to be regenerated again.

## Descriptive 44-arb coverage (not a gate)

The frozen WHI-1413 denominator was re-run with the same rules (`coverage_44.txt`):

`python3 evidence/venues/whi-1413/coverage_44.py evidence/venues/whi-1413/denominator/dune_8788689_exec_01M30WWX1XTF7EMWFQ81HVNKB7.csv <dev 2b13b8f data/pool_universe.csv> data/pool_universe.csv`

| Universe | Pools | Fully covered |
|---|---:|---:|
| before: `0x2adb7cd6…` @98969898 | 151 | 20 / 44 (45.5%) |
| after: `0x4c2456dd…` @101165208 | 124 | **18 / 44 (40.9%)** |

Lost: `0x86c23826…` and `0x958538d2…`. Both were lost only because Moe V1 USDT/WMNT `0x4e7685df…` is
now below the TVL floor. No arb was gained.
