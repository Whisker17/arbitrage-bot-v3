# WHI-1413 universe regeneration at block 98969898 (candidate)

Binary: `cargo build --locked --release --bin universe_gen` at commit `8d859ef` (clean tree).
RPC: `https://rpc.mantle.xyz` (public mainnet default, read-only, chain 5000). Block 98969898,
hash `0x77e9802850e09580e8527b3ea75ed93510331e91339b8272af4ed5660ef76374`: the committed
universe's own snapshot block and the WHI-1422 fork block.
v2 seed: the WHI-1410 operator copy `evidence/universe/whi-1410/regen-100871945/inputs/poolLists_v2.csv`
(comment 3c7e6f4e: `data/poolLists_v2.csv` is gitignored).

```bash
# repro/ — committed seeds, no new venue (2026-09-26T18:36Z → 18:41:57Z, rc=0, 321 s)
NO_COLOR=1 ./target/release/universe_gen --rpc https://rpc.mantle.xyz --block 98969898 \
  --seed-v2 evidence/universe/whi-1410/regen-100871945/inputs/poolLists_v2.csv \
  --out /tmp/whi1413/regen/repro/pool_universe.csv
# with-moe-v1/ — same + Merchant Moe V1 classic allPairs at the same block (→ 18:49:55Z, rc=0, 510 s)
NO_COLOR=1 ./target/release/universe_gen --rpc https://rpc.mantle.xyz --block 98969898 \
  --seed-v2 evidence/universe/whi-1410/regen-100871945/inputs/poolLists_v2.csv \
  --v2-venues moe-v1 --out /tmp/whi1413/regen/moe/pool_universe.csv
```

Results:
- `repro/pool_universe.csv` sha256 `c01c552c…4b17` is **byte-identical** to the committed
  `data/pool_universe.csv`, with the same fingerprint `0x0ecceac8…df55a46`. Meta differs only in the
  absent `observed_arb_coverage` block (the committed meta attached WHI-906 coverage from an
  external dataset). The quarantine file additionally lists the Cleopatra CL venue-quarantine rows
  (WHI-938). The committed quarantine predates that and pool membership is unaffected.
- `with-moe-v1/`: 151 pools, fingerprint `0x2adb7cd6…e20e50`. It is a strict superset of the 130:
  +17 Moe V1 classic pairs (230 enumerated → TVL ≥ 1000 WMNT and on a ≤3-hop WMNT cycle), plus
  **4 Moe LB pools** that the unchanged cycle filter now admits because they sit on cycles through
  the new Moe V1 pairs.
