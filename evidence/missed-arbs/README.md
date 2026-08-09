# Backwards universe selection from missed arbs (WHI-999)

WHI-906 asked the **forward** question — given a candidate universe, what share of
observed arbs can we execute? This inverts it: start from the 10,501 verified
arbs (WHI-956), keep only the ones the strategy could actually take, and ask
**which pools we would have needed**, ranked by *marginal* arbs unlocked with the
full admission cost of each candidate set.

## Do not confuse with WHI-906 / WHI-957

| Issue | Question | Metric |
| --- | --- | --- |
| WHI-906 | Forward coverage of a candidate universe | `fully_executable` over **all** 10,501 arbs → 37.9% |
| WHI-957 | Why was each missed arb missed? | one cause per event → `not_in_universe` 58.5% |
| **WHI-999** | Which pools would have been needed, at what cost? | marginal arbs unlocked over **in-scope** arbs only |

"In-scope" drops what the strategy can never take (hop > 3, non-WMNT settlement,
aggregator noise), so **9,674** of 10,501 events. WHI-906's number is higher
because it credits coverage of arbs we could not execute anyway.

## Tool

```bash
# Offline: ranking + production cycle counts, candidate TVL left unmeasured
cargo run --release --bin missed_arb_universe -- \
  --arbs <external>/arbs_month.jsonl \
  --census <external>/pool_census.json \
  --json-out evidence/missed-arbs/report.json \
  --md-out evidence/missed-arbs/report.md

# With candidate TVL (read-only RPC, no signer) so the floor can be attributed
cargo run --release --bin missed_arb_universe -- \
  --arbs <external>/arbs_month.jsonl \
  --census <external>/pool_census.json \
  --measure-tvl \
  --chain-gross-usd-per-day 15 \
  --json-out … --md-out …
```

Datasets stay **external** (same contract as WHI-906 / WHI-956); only aggregate
reports are committed. Without `--measure-tvl` every affected pool carries
`tvl_not_measured` and the header stamps `tvl_measured=false`, so an unmeasured
valuation can never read as "cleared the floor". The valuation endpoint follows
the chain-aware precedence in `CLAUDE.md` (`--rpc-url` overrides it) and its
chain id is asserted against `--chain-id`. `--tvl-block` defaults to the
universe's own `snapshot_block`.

Rows the loader cannot decode are counted, never dropped silently: the header
reports `source_rows` and `skipped_empty_path`. On this dataset all 10,501 rows
carried a decodable path, so nothing is excluded.

## Committed reports

Both cover blocks **96,806,569 – 98,098,684** (10,501 arbs) against the frozen
130-pool universe, fingerprint `0x0ecceac8…df55a46`. They differ **only** in
which block candidate TVL was read at — see "the floor is a moving target" below.

| File | TVL read at |
| --- | --- |
| `month30_tvl_at_snapshot_block.{json,md}` | 98,969,898 — the universe's own snapshot block |
| `month30_tvl_at_window_end.{json,md}` | 98,098,684 — the last block of the arb window |

## Headline

| Metric | Value |
| --- | ---: |
| In-scope arbs | **9,674** |
| Reachable on today's 130 pools | **3,535** (36.5%) |
| Blocked by missing pools | **6,139** |
| Distinct pools in in-scope arbs | 361 (94 held, **267 missing**) |
| Best 50-pool set on **loadable** venues | **5,382** (55.6%) |
| Best 50-pool set ignoring venue support | 8,106 (83.8%) |

The cause recount reproduces WHI-957 **exactly** — 6,139 / 3,535 / 678 / 141 / 8,
`not_in_universe` = 58.5% — and the cycle count for the current universe comes
out at **6,962**, matching the figure quoted on the issue. Both are computed
independently here (the cycle count runs the production `arbitrage::pathfinder`
enumerator, not a re-implementation), so the agreement is a real cross-check.

## The residual is bounded, and it is not a floor

WHI-957's 3,535 `unattributable` events **all have every hop pool inside the
universe**. This is a property of the cause ordering, not a measurement: the
attribution tests `not_in_universe` at priority 6 and reaches the residual only
at priority 11, so a residual event with a missing pool is impossible by
construction. The report's `residual_events_with_missing_pool: 0` is a
regression guard on that invariant — it would only fire if the two
classification paths disagreed — not independent evidence for it.

So 58.5% is a **point estimate** for the universe question, not a floor. The
residual is unclassified only as to *which in-universe cause* applies
(dirty-cycle vs unprofitable vs lost race), which still needs a concurrent
ledger (DI-35). Bounding it does not move the ranking. The issue's premise that
the residual "plausibly hides more `not_in_universe`" does not hold.

**One hole, stated rather than papered over.** The attribution also routes an
event whose `ordered_pools` is *empty* to `unattributable`, and such an event
cannot be tested for universe membership at all. The claim above is therefore
scoped to events with a decoded path. On this dataset that costs nothing —
`skipped_empty_path` is 0 of 10,501 rows — but on a re-run with a lossier
extract the residual should be read as "+`skipped_empty_path` unknown".

## Which filter is actually costing us

Per-filter exclusion of the 267 missing pools, in admission order (a pool blocked
by its venue is never also blamed on TVL):

| Cause | Pools | In-scope arbs touched |
| --- | ---: | ---: |
| `venue_not_loadable` | **176** | 4,260 |
| `below_tvl_floor` | **73** | 2,560 |
| `cycle_filter_rejected` | 17 | 180 |
| `admissible_but_absent` | 1 | 51 |

A fifth cause, `pool_tokens_unknown`, exists for a pool that clears venue and TVL
but has no census token pair — the cycle test cannot run over it, so convicting
the cycle filter would invent an exclusion count. It is **0** here: every
venue-and-TVL-admissible pool had a token pair, so the 17 `cycle_filter_rejected`
are genuinely off every ordered ≤3-hop WMNT cycle.

Each row also carries the pool's **hop positions** (`position:count` over the
ordered path), so a first-hop-only gap is distinguishable from a mid-cycle one —
the top missing pool `0x98d1e9…` sits at hop 1 in 223 arbs, hop 2 in 137 and hop
3 in 231, i.e. it is a general-purpose leg rather than an entry point.

Venue support blocks the most pools; the **TVL floor blocks the most valuable
ones**. Of the top 10 loadable-venue candidates, every one was excluded by the
floor, several by a hair — 918.7, 937.6, 802.1, 739.7, 732.6 WMNT against a
1,000 WMNT floor (the report's table truncates to whole WMNT).

The 176 venue-blocked pools split by the *class of work* required — "needs an
adapter" and "needs a registry entry" are not the same cost:

| Venue status | Pools | Work required |
| --- | ---: | --- |
| `unregistered_v2_family_factory` | 99 | per-venue V2 fees first (`V2_FEE` is hard-coded — WHI-910 out of scope) |
| `unsupported_math_family` | 33 | a new AMM adapter (`izi`, `algebra`, `solidly`) |
| `unregistered_v3_family_factory` | 29 | a `v3_venues` registry entry + WHI-938-style batch validation |
| `quarantined_adapter_required` | 15 | tick-data batch adapter (Cleopatra CL, WHI-938) |

**Venue ABI compatibility is respected, not silently ranked.** The two
highest-value pools in the whole dataset (`0x98d1e9…` +365, `0x1da092…` +280) are
iZi pools on unregistered factory `0x45e5…c218` with no adapter. They appear only
in the `any_venue` ranking, which the report labels as an upper bound; the
`loadable_only` ranking is what is actionable today.

Those two also show the limit of the per-pool detail the acceptance criteria ask
for: the census carries no symbols or token addresses for them, so their **token
pair reads `—` and their TVL is absent**. Both are consequences of the venue
being unsupported — without a token pair the pool cannot be valued through the
WMNT-pair basis, and it cannot enter the cycle graph either. Flagged here rather
than left as a blank cell.

**`venue_status` is the only venue verdict in these reports.** WHI-906's
`adapter_class`, derived from the census `kind` string, is deliberately *not*
carried on WHI-999 rows: WHI-765 reclassified several Algebra-tagged factories as
UniV3 drop-ins, so ~40 Agni-V3 / FusionX-V3 pools we load today still read
`kind: "algebra"` in the census and would come out `adapter_required`. Emitting
both fields would put `adapter_class: adapter_required` next to
`venue_status: loadable_drop_in` on the same row and invite the conclusion that
half the actionable list needs an adapter. `venue_status` resolves the factory
registry first and is the field every count here uses.

## The floor is a moving target — the real gap is snapshot staleness

Reading TVL at the two different blocks moves only **2** pools, but they are the
#1 and #3 ranked loadable candidates:

| Pool | Pair | TVL at window end | TVL at snapshot block |
| --- | --- | ---: | ---: |
| `0x361052be…` | USD1/USDT0 | **220,886 WMNT** | 0 WMNT |
| `0xeafc4d6d…` | USDe/WMNT | **45,796 WMNT** | 104 WMNT |

Both were far above the floor while the arbs were happening and have since
drained. `admissible_but_absent` therefore covers **558** in-scope arbs when read
at the window end versus 51 at the snapshot block.

That reframes the finding. The generator is not mis-filtering: it correctly
applied a 1,000 WMNT floor to the state in front of it. The cost comes from the
universe being a **point-in-time snapshot with no refresh** — a pool that carried
220k WMNT and 260 arbs during the window is judged by its balance 870,000 blocks
later. Chasing the floor value is the wrong lever; refresh cadence (and the
promotion state machine deferred in WHI-536 / M3-10) is the right one.

## Admission cost of each candidate set

| Restriction | Size | Arbs unlocked | Reachable after | Pools | Cycles | Cycle × | Cold start |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `loadable_only` | 10 | +1,133 | 4,668 (48.3%) | 140 | 10,936 | 1.57× | 377 s |
| `loadable_only` | 25 | +1,653 | 5,188 (53.6%) | 155 | 20,490 | 2.94× | 417 s |
| `loadable_only` | 50 | +1,847 | 5,382 (55.6%) | 180 | 31,058 | 4.46× | 485 s |
| `any_venue` | 10 | +1,833 | 5,368 (55.5%) | 140 | 8,510 | 1.22× | 377 s |
| `any_venue` | 25 | +3,233 | 6,768 (70.0%) | 155 | 14,558 | 2.09× | 417 s |
| `any_venue` | 50 | +4,571 | 8,106 (83.8%) | 180 | 30,920 | 4.44× | 485 s |

Cycle counts are exact production enumeration. Cold start is modelled **linearly**
from WHI-936's measurement (350 s at 130 pools, one pool per batch CREATE), so it
scales with pool count; fixing WHI-936 moves every one of these numbers down.
Note the cycle count is not monotone in the restriction — the loadable pools
happen to be higher-degree in the token graph, so 10 of them add more cycles than
10 unrestricted ones.

**Diminishing returns are steep.** Going 10 → 50 loadable pools costs 2.8× the
cycle set for +714 more arbs. The first 10 pools do 61% of the work the first 50
do. By rank 40 the greedy has run out of pools that complete an arb on their own:
that step is a `frequency_fallback` with `marginal_arbs_unlocked: 0`, so the
50-pool set is really 49 pools that pay plus one that closes half of a
multi-pool gap.

## Hop cap: quantified, and not worth it

| Metric | Value |
| --- | ---: |
| Arbs above the cap | 678 |
| …at exactly 4 hops (all a 3→4 change admits) | **155** |
| …of those, already fully in universe | **78** |
| …with the largest loadable set added | 85 |
| Cycles at cap 3 | **6,962** |
| Cycles at cap 4 | **134,096** (**19.3×**) |
| Cold-start impact | none (same pools) |

Raising `EFFECTIVE_MAX_HOPS` to 4 buys **78 arbs over 30 days** — under 3 per day
— for a **19.3× larger cycle set** that every dirty-cycle pass must re-optimize.
The remaining 523 arbs above the cap sit at 5+ hops and a one-step change does
not touch them. This is the clearest "do not do this" in the report. (Changing
the cap remains out of scope here by design; this prices it.)

## Verdict

**Yes, a reachable universe exists** — 50 pools on already-loadable venues take
in-scope reachability from 36.5% to **55.6%** (5,382 arbs, ~180/day) with no new
adapter work. That answers the acceptance criterion.

But **count is not value.** Against the ~$15/day chain-wide atomic-arb gross
measured across all 68 bots, 55.6% coverage implies roughly **$8/day gross before
gas** — and only if every reachable arb were also *won*, which the race data does
not support. This report sizes coverage; it does not price the opportunity, and
nothing here makes the economics work.

The actionable conclusions, in order:

1. **Do not raise the hop cap.** 19.3× cycles for <3 arbs/day.
2. **Do not lower the TVL floor.** The pools it excluded were genuinely thin *at
   the snapshot block*; the loss came from snapshot staleness, not the threshold.
3. **The cheapest real win is refresh cadence**, not more pools — two of the top
   three candidates were rich during the window and empty by the snapshot.
4. **Adapter work has the largest ceiling** (83.8% vs 55.6%) and the largest
   cost: 33 pools need new math adapters and 99 need per-venue V2 fees.
5. **None of that changes the funding question**, which turns on a ~$15/day
   chain-wide pie and is not resolved by this issue.

## Reproducing

Identical inputs + block range → identical report. The analysis is pure
(`src/service/missed_arbs.rs`); only candidate valuation touches RPC
(`src/service/valuation.rs`, shared with `universe_gen` so the heuristic cannot
drift from the one the universe was filtered by).

Unit coverage: `cargo test --locked --lib missed_arbs::` and
`cargo test --locked --lib valuation::` — scope priority vs `peer_attribution`,
marginal-unlock vs frequency-fallback labelling, cycle counting against the
production enumerator, venue classification (registry ahead of census `kind`),
and TVL floor / quarantine separation.
