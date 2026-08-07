# Observed on-chain arb coverage (WHI-906)

Offline intersection of real Mantle atomic arbitrages against the frozen
pool universe (`data/pool_universe.csv`). Selection metric is **fully
executable** path coverage — not TVL.

## Reproduce

The arb dataset and pool census live **outside** this repo (no RPC URLs or
credentials in any committed artifact):

```bash
# 30-day archive (headline acceptance numbers)
cargo run --release --bin arb_coverage -- \
  --universe data/pool_universe.csv \
  --arbs <external>/data/arbs_month.jsonl \
  --census <external>/data/pool_census.json \
  --out evidence/coverage/month30.json \
  --write-meta-coverage

# Recent window (re-extract paths from receipt shards first if needed)
cargo run --release --bin arb_coverage -- \
  --universe data/pool_universe.csv \
  --arbs <external>/data/arbs_window_15d.jsonl \
  --census <external>/data/pool_census.json \
  --out evidence/coverage/window_15d.json
```

`universe_gen` can attach the same report at generation time via
`--arb-arbs` / `--arb-census` (optional; no RPC).

## Committed reports

### Post WHI-910 multi-factory regenerate (current)

Universe: **137 pools** (agni-v2=5, agni-v3=94 across **7** drop-in factories, moe=38).  
Fingerprint: `0x3a7ba09e6463a302cb37bb2b38a1d4a0508a8e1bf2137bf5145a6af7a5a77f2e`  
Snapshot block: see `data/pool_universe.meta.json`.

| File | Window | Result |
| --- | --- | --- |
| `month30.json` | blocks **96,806,569–98,098,684** (10,501 arbs) | **38.1 %** fully executable (4,000); touching **92.4 %**; greedy top-12 → **59.3 %** |
| `window_15d.json` | blocks **98,112,058–98,759,634** (2,192 arbs) | **44.0 %** fully executable (965); touching **89.5 %** |

Emitted V3 factories (after TVL + ≤3-hop cycle filters): Agni 33, FusionX 14, Butter 16, Fluxion 14, Cleopatra 7, V3fork 4, Uniswap V3 6.

### Pre-regenerate baseline (59-pool universe)

Snapshot under `pre_regen_snapshot/` (59 pools, single Agni V3 factory).

| Metric (30d) | Old (59 pools) | New (137 pools) |
| --- | --- | --- |
| Fully executable | **4.4 %** (460) | **38.1 %** (4,000) |
| Touching ≥1 | 49.9 % | 92.4 % |
| Distinct pools covered | 44 / 390 | 117 / 390 |

The ~55 % “all drop-in V3” figure from WHI-906 comments is an **upper bound without TVL/cycle filters**. This regenerate applies the same 1000 WMNT floor + ≤3-hop WMNT cycle filter, so emitted coverage lands lower (~38 % on the 30d set).

Coverage is also recorded next to the universe fingerprint in
`data/pool_universe.meta.json` → `observed_arb_coverage`.

**How this universe was built:** expanded `data/poolLists.csv` with census pools
for all seven drop-in V3 factories (seed tags in `v3_venues.rs`), then
`universe_gen` seed mode (not `--discover` — full log scan is too slow on
creation_block=0 factories). Local `poolLists_v2.csv` is gitignored; copy it
into `data/` before regenerating if agni-v2 rows are required.

**Regen note:** bare `universe_gen` (without `--arb-arbs` / `--arb-census`)
writes `observed_arb_coverage: null`. After regenerating the universe, re-run
`arb_coverage --write-meta-coverage` (or pass the arb flags to `universe_gen`)
so successive fingerprints stay comparable on this metric.

## Reading the ranking

Each greedy step adds the non-held pool that unlocks the most additional
*fully covered* arbs. `adapter_class` follows the WHI-906 census `kind`
labels: `v2` / `v3` / `lb` → `drop_in`; `algebra` / `izi` / `solidly` →
`adapter_required`. (WHI-765 later reclassified some Algebra-tagged factories
as UniV3 drop-ins; filter the ranking by class + factory against
`evidence/venues/MATRIX.md` before admitting pools.)
