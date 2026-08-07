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

| File | Window | Result |
| --- | --- | --- |
| `month30.json` | blocks **96,806,569–98,098,684** (10,501 arbs) | **4.4 %** fully executable; greedy top-12 → **23.3 %** |
| `window_15d.json` | blocks **98,112,058–98,759,634** (2,192 arbs) | **4.1 %** fully executable; venue mix still algebra/izi-heavy |

Coverage is also recorded next to the universe fingerprint in
`data/pool_universe.meta.json` → `observed_arb_coverage`.

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
