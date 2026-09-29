# WHI-1412: peer attribution after the route-key fix, and the scope of the earlier "engine is not the bottleneck" conclusion

**Window:** UTC day **2026-09-28** (Monday), chain blocks **101211644..101254843** (43,200 blocks).
**System:** the signerless shadow of `v0.2.2-rc2` = `fe4a574`. It ran the 124-pool universe
`0x4c2456dd…` @101165208 and the gas profile `0x3d3244e3…`. Sends were disabled.
**Peer set:** the Dune competitor-monitoring pack (`02_arb_detail_feed`, query 8781229, execution
`01M3NGSE7KAE0R0YM8B5HPQNKH`), **252** qualified arbs from 32 bot addresses.

This is a **scope correction, not a retraction**. The earlier attribution was not shown to be
wrong. It was measured on a slice that it did not state.

## 1. Headline: the six-way breakdown

- **Denominator:** 252 peer arbs − 0 collector exclusions = **252**.
- **Out of scope by design:** 72 of the 252 are outside the strategy.
- **In-scope denominator: 180.**

| # | Cause | Count | Share of in-scope (180) |
|---|---|---:|---:|
| 1 | absent pool (≥1 hop pool ∉ the 124-pool universe) | **39** | 21.67 % |
| 2 | route class unknown (no gas-profile entry at any bucket) | **0** | 0.00 % |
| 3 | route class unapproved (entries exist, none approved) | **135** | 75.00 % |
| 4 | evaluated and unprofitable | **0** | 0.00 % |
| 5 | profitable but not attempted — *modeled* on prior-block state, timely availability **unproven** (§1a) | **6** | 3.33 % |
| 6 | attempted and lost the race | **0** | 0.00 % |
| | residual: `block_skipped` / `dirty_cycle_filter_skipped` / `unattributable` | 0 / 0 / 0 | |
| | out of strategy scope: hop > 3 | 46 | |
| | out of strategy scope: settlement ≠ WMNT | 26 | |
| | out of strategy scope: aggregator / flash loan / adapter | 0 / 0 / 0 | |
| | **total** | **252** | |

Every event lands in exactly one row. The tool fails closed if the rows do not sum to the event
count (`build_six_way` in `src/bin/peer_attribution.rs`). Sources are `six_way.json` and
`aggregates.json → six_way_pre_state`.

**Route class unapproved (135), by ordered topology** (`six_way.json → topologies`):

| topology | count |
|---|---:|
| `h2:v3+v3` | 57 |
| `h3:v3+v3+v3` | 27 |
| `h2:moe+v3` | 14 |
| `h3:moe+v3+v3` | 8 |
| `h3:moe+moe+v3` | 7 |
| `h2:v3+moe` | 5 |
| `h3:v3+moe+v3` | 5 |
| `h3:v2+v3+v3` | 4 |
| `h2:moe+moe` | 3 |
| `h3:v2+v2+v3` | 3 |
| `h3:moe+moe+moe` | 2 |

Every one of these has profile entries, but none is approved. Of the six classes approved in
`0x3d3244e3…` (§3), only `h3:v2+v2+v2` occurs among the in-universe peer routes of the day.

**Profitable but not attempted (6): modeled opportunities on prior-block state.** All six are the same route, `h3:v2+v2+v2`, on three
**Merchant Moe V1 classic** pools (factory `0x5bef…EdEc`):

- the pools are `0x7638…e110`, `0xb670…e1d4`, `0xefc3…c953`, taken in both directions;
- one peer, `0x99bb…`, landed all six;
- each at **tx index 1**, the first user tx after the L1-attributes system deposit (tx 0 → `0x4200…0015`, checked by RPC);
- so its pre-state is exactly the post-(N−1) state.

For each event, the ledger holds `env_unsupported` candidates on the **exact ordered route** at
N−2 and N−1. The log's `block_summary` shows `attempt_outcome="production_gate_blocked"` at both
blocks. The modeled net is at the 10 WMNT capital cap. The peer's gross is read from its receipt
(WMNT net into its executor).

| event block | engine candidates at | modeled `best_net` at N−1 (WMNT) | peer gross (WMNT) | peer L2 gas (MNT) |
|---|---|---:|---:|---:|
| 101211744 | 101211742, 101211743 | 0.2848 | 0.3950 | 0.2072 |
| 101211761 | 101211759, 101211760 | 0.1461 | 0.1706 | 0.0847 |
| 101211772 | 101211770, 101211771 | 0.1271 | 0.1470 | 0.0729 |
| 101211781 | 101211779, 101211780 | 0.1431 | 0.1675 | 0.0831 |
| 101211799 | 101211797, 101211798 | 0.1371 | 0.1601 | 0.0821 |
| 101244098 | 101244096, 101244097 | 0.5445 | 1.0291 | 0.8219 |

Source: `aggregates.json → profitable_but_not_attempted_evidence`. The peer spent **50–80 % of
its gross on L2 gas**, which is priority bidding. Our model does not price that. So "profitable"
here means profitable **by the engine's own model at its own fee assumptions**. It is not a claim
that we would have won or netted that amount.

### 1a. Timing: the six are state matches, not proof of timely detection

"N−1" names the block **whose post-state was evaluated**, not the time the candidate existed.
Here the candidate was recorded against the peer block timestamp (`aggregates.json →
profitable_but_not_attempted_evidence[].timing`, `profitable_but_not_attempted_timing_summary`):

| peer block N | peer block ts (UTC) | N−2 candidate recorded | N−1 candidate recorded | N−1 `block_summary` log time |
|---|---|---:|---:|---|
| 101211744 | 00:03:20 | +3 s | +3 s | 00:03:23.389747 |
| 101211761 | 00:03:54 | +2 s | +4 s | 00:03:57.944170 |
| 101211772 | 00:04:16 | +2 s | +2 s | 00:04:18.720793 |
| 101211781 | 00:04:34 | 0 s | +4 s | 00:04:37.951634 |
| 101211799 | 00:05:10 | +1 s | +4 s | 00:05:14.280523 |
| 101244098 | 18:01:48 | +2 s | +4 s | 18:01:52.316609 |

The offsets are `candidate recorded_at_unix − peer block timestamp`, all on 2026-09-28.

- **Of the 12 candidate rows, none was recorded before the peer's block timestamp.** One falls
  in the same second, and 11 are after it.
- **Context for the whole day:** `observation.recorded_at_unix − header.block_timestamp` over all
  42,528 processed blocks has p10 3 s, p50 5 s, p90 6 s, p99 30 s (min 2, max 36).
  The candidates were recorded 4–7 s after their own state block.
- **Clock comparability.** `recorded_at_unix` and the log times come from the shadow host's clock.
  Block timestamps are sequencer-assigned. The frozen artifacts capture no offset bound between
  the two clocks, and both ledger fields are whole seconds. So this table **is not a latency
  measurement**, and it says nothing about a race: no sends occurred.

**What the six do and do not establish.** They show that the engine's **model detected those six
routes on the relevant pool state**, the post-(N−1) state the peer traded against. They do **not**
establish that the opportunity was available to us in time. Engine latency, execution and
conversion remain **unmeasured**.

**Attempted and lost the race = 0, and why that is structural.** No transaction was ever sent:

- the ledger `run_header` has `send_capability = "no_send"`;
- all **26** candidate rows of the day are `outcome.kind = env_unsupported` with detail `production_gate_blocked`;
- the chain-day `block_summary.attempt_outcome` is `production_gate_blocked` on 26 lines and `-` on the other 43,173;
- the ledger has **no `pass` row**, and the tool's `attempted_and_lost_race` requires a profitable `pass`.

So the race was never entered, and this bucket cannot be measured on a signerless shadow.

## 2. How each cause is decided (and what changed in the tool)

The priority order is fixed. Each event is assigned by the first rule that applies:

1. **Out of scope by design** (the existing tool rules): aggregator (hop > 50), flash loan,
   hop > 3, settlement ≠ WMNT, adapter.
2. **Absent pool:** some ordered pool is not in `data/pool_universe.csv`.
3. **Route class:** the ordered protocol topology is taken from the universe CSV `protocol`
   label (`agni-v2` → v2, which includes Moe V1 classic; `agni-v3` → v3; `moe` → Moe LB). It is
   then classified by the engine's own predicate,
   `amms::service::fee_scoring::topology_profile_support`, against
   `RuntimeGasProfile::load(config/gas_profiles/mantle_mainnet_v1.json, mantle_mainnet)`. That is
   the same load `bot.rs` uses, and it fails closed unless the digest is `0x3d3244e3…`. This is
   the WHI-1409 pre-simulation filter: a topology whose buckets are all unapproved is rejected
   before any simulation. So this rule sits **before** any block-level evidence.
   - `Unknown` → route class unknown.
   - `Unapproved` → route class unapproved.
   - `Supported` → continue to step 4.
4. **Ledger evidence, keyed by `--pre-state`:** each observed block B stands for the state a
   transaction could act on in block **B+1**, so a peer arb at N is judged against the engine's
   post-(N−1) state.
   - A candidate on the exact ordered route with `env_unsupported` / eligibility → profitable but not attempted.
   - A profitable `pass` → attempted and lost.
   - The dirty set touched the path with no candidate → evaluated and unprofitable.
   - Otherwise → the explicit residual.

**Tool change** (`src/bin/peer_attribution.rs` only; the library `src/execution/peer_attribution.rs` is untouched):

- `--gas-profile` enables the route-class split, and `--six-way-json-out` writes the breakdown.
- **Bare candidate rows.** The rc2 ledger writes `candidate` rows **without** the `context` rows
  that `ShadowLedgerIndex` joins on, so the unmodified tool saw **0 opportunities**.
  - Such rows are now ingested directly: block = the preceding `observation` row, ordered pools = the `signature=` segment of `detail`.
  - Cross-check: all 26 rows' preceding-observation blocks have `candidates > 0` in `block_summary` (`aggregates.json → candidate_block_crosscheck`).
  - A row counts as already joined only if a `context` row with the same digest is in the **same ledger**, which is exactly the library's join. The digest carries no block identity: the same route on 101211742 and 101211743 shares one digest. So a digest seen in another `--ledger` file never suppresses this file's occurrence, and the result does not depend on how rows are split across files or on file order (fix round 1; regression `bare_candidates_do_not_depend_on_ledger_split_or_order`).
- `--pre-state` re-keys observations, dirty-set views and candidates to `observed block + 1`. It
  also skips the WHI-715 bucket cross-check, which keys at the event block.
- Focused tests: `cargo test --locked --bin peer_attribution` (5 tests, listed in the PR).

**Why `--pre-state` matters.** This is a contrast, not a result. With the old same-block keying
(`aggregates.json → six_way_same_block_keying`), the six Moe V1 events come out as **evaluated
and unprofitable**:

- the peer's own swap makes its pools dirty at N;
- the engine then re-evaluates them on the **post-arb** state and finds nothing.

That keying makes "evaluated but unprofitable" true by construction for any route that is
evaluable at all.

## 3. Scope gate: is this the "representative post-expansion live run"?

**Post-expansion: yes, verified.**

- `git merge-base --is-ancestor` holds for all three changes below, each an ancestor of `fe4a574`:
  - `de735db`, the route-class approvals;
  - `2b13b8f`, Merchant Moe V1 classic venue support;
  - `fe4a574`, the regenerated universe and re-qualified profile.
- `config/gas_profiles/` and `data/pool_universe.*` are byte-identical between `fe4a574` and this
  base (`c62ff0c`).
- The universe venue mix, by `factory`:
  - Moe LB: 32;
  - UniV3 family: 75, across six factories (29 + 16 + 13 + 9 + 6 + 2);
  - **Moe V1 classic: 15**;
  - FusionX V2: 2.
- Profile `0x3d3244e3…` has 307 keys, of which **6 are approved**:
  - `h2:v2+v2`;
  - `h3:v2+v2+v2`;
  - `h2:v2+v3:ticks=0`;
  - `h3:v2+moe+moe:bins=0`;
  - `h3:moe+v2+v2:bins=0`;
  - `h3:moe+moe+v2:bins=0`.
- The deployment identity (pid, sha256 `f55b341b…621c`, run `0x70fc4d9b…`) is in
  `evidence/shadow/whi-1423/STATUS.md` and the frozen `PROVENANCE.md`.

**What "representative" covers:**

- the post-expansion configuration, run live on mainnet;
- the peer flow of **one full weekday UTC day**;
- the same blocks on both sides (concurrent);
- the whole day, with nothing cherry-picked.

**What it does not cover:**

- **Other days.** Multi-day and weekday/weekend variance is not measured; this is n = 1 day.
- **The approved slice only.** The day's evaluation coverage was `paths_quoted / cycles_optimized` = 11,111 / 1,427,914 = **0.78 %**:
  - `unapproved_route` = 1,408,588 = **98.65 %** of evaluated paths;
  - `unknown_route` = 0;
  - Full passes quoted 9,282 / 661,878 = 1.40 %.

  So the six-way reflects the classes the engine *can* evaluate. It is not a statement about its
  enumerated cycle space.
- **Engine attempt quality.** There is no send path, so the race outcome is not measured (§1).
- **Profitability.** Modeled nets are at a 10 WMNT cap. Peer economics are shown only for the six.
- **Sandwich/JIT screening.** The Dune pack reports `is_jit_lp` and `is_sandwich` as `unknown` on
  all 252 rows, so those two collector exclusions were **not tested**. They parse as false, which
  is why there are 0 exclusions.

**Verdict:** the run satisfies the 2026-09-23 comment's requirement, within the limits above.

## 4. Block coverage: reconciling the ledger with the log

The 43,200 chain-day blocks break down as follows:

| part | blocks |
|---|---:|
| **processed** (each has exactly one ledger observation and one `block_summary` with `skip_reason="-"`) | **42,528** |
| **skipped**: `processing_failed` | 501 |
| **skipped**: `pinned_logs_unavailable` | 153 |
| **skipped**: `pinned_header_unavailable` | 17 |
| **never summarized** (block 101238115) | 1 |
| **total** | **43,200** |

The 671 skipped blocks are logged in `block_summary` but have no ledger row.

The log's "43,198 blocks dated 09-28" count is **wall-clock dated**. It equals the 43,199 chain-day
blocks with a summary, minus 3 tail blocks logged after 00:00Z 09-29, plus 2 previous-day blocks
(101211642/3) logged after 00:00Z 09-28.

The day bounds were checked by RPC:

| block | timestamp | time (UTC) |
|---|---|---|
| 101211643 | 1790553598 | before the day |
| 101211644 | 1790553600 | 00:00:00Z 09-28 |
| 101254843 | 1790639998 | last block of the day |
| 101254844 | 1790640000 | 00:00:00Z 09-29 |

Source: `aggregates.json → ledger_log_reconciliation`, `day_bounds_rpc_block_timestamps`.

No peer event on an unobserved block needed block-level evidence to be classified. (The six modeled positives do rest on block-level candidate evidence, at observed blocks.) Under same-block keying, 9 events fell
on blocks the engine did not observe; under pre-state keying, 7 had an unobserved N−1. All of them
are route-class unapproved, and that rule precedes block evidence.

## 5. The August baseline, identified from artifacts

| item | value | artifact |
|---|---|---|
| peer-attribution result | 10,501 arbs, blocks 96,806,569–98,098,684. `not_in_universe` 6,139 (58.5 % of 10,493 non-aggregator), `unattributable` 3,535 (33.7 %), `evaluated_but_unprofitable` 0, `dirty_cycle_evidence = not_measured` | `evidence/peer-attribution/offline_universe.json` @ `e574540` (2026-08-09) |
| universe | **130 pools, fingerprint `0x0ecceac8…df55a46`, snapshot 98,969,898** (5 `agni-v2` / 87 `agni-v3` / 38 `moe`), csv sha256 `c01c552c…`. WHI-999 names the same fingerprint and reproduces the counts exactly. | `e574540:data/pool_universe.meta.json`; `evidence/missed-arbs/README.md` @ `439afed` |
| how it was measured | **Offline, with no concurrent ledger and no gas profile input**. The README says "existing shadow ledgers sit at ~98.95M while GT spans 96.81M–98.10M". Every in-universe event fell to `unattributable` because there was no ledger. | `evidence/peer-attribution/README.md` |
| profile in force for any August engine build | content digest **`0xa18811da…`**: 24 keys, **3 approved** (`h2:v2+v2`, `h2:v2+v3:ticks=0`, `h2:v2+moe:bins=0`). The byte keccak `0xba041633…` is pinned in `evidence/shadow/whi535/digests/pinned_files.json` (2026-08-06 shadow requalify). The bytes are identical at `f7c8047`, `e574540`, `439afed` and `6dad49e` (= v0.2.2-rc1). `bot.rs` compiles in this path. | git objects + whi535 digest |
| August discovery gate | at `e574540`, `optimize_path` builds the **zero-bucket** topology key (`topology_route_key`) and rejects unless `fee_plan_cost` approves it (WHI-949, `f308927`, 2026-08-08). So only 2-hop routes whose **first** leg is v2 could be priced. | `e574540:src/service/path_index.rs` |
| a live run on that universe and profile | 130-pool / 3-protocol `--watch`: on a Full pass, **8 of 6,962** cycles reached the optimizer (**0.11 %**); across 20,886 evaluated cycles, 20,382 were `unknown_route` and 480 `unapproved_route`. This is exactly the issue's "≤ 8" upper bound. | `evidence/shadow/whi-1411-rejection-liveness/STATUS.md` |
| deployments at the time | 130-pool 40-min `--watch` (skip-rate round-1 re-check, 2026-08-09). Later, v0.2.2-rc1 on a host-generated 109-pool universe `0xee1d40b8…` with the same profile, where 100 % of discovery was rejected (`gas_profile`, 8.27 M). | `evidence/shadow/whi-977-skip-rate/STATUS.md`; `evidence/shadow/whi-1423/STATUS.md` §4c/§7 |

**Counterfactual on the same peer flow.** The 180 in-scope arbs of 2026-09-28 were re-classified
under the August universe and the August predicate (`aggregates.json →
august_counterfactual_on_2026_09_28_peer_arbs`):

| August config | count |
|---|---:|
| absent pool | 45 |
| route class unknown | 73 |
| route class unapproved | 62 |
| evaluable | **0** |

The six Moe V1 events are "absent" under August: their pools were not in the 130-pool universe.

So on this day's flow, the August engine could have evaluated **nothing**. The post-expansion
engine can evaluate **6**. On all six, its model produced a gate-blocked candidate on the prior-block
state; whether in time is unproven (§1a).

## 6. Which earlier conclusions survive, and which were scope-limited

**Survive** (as measurements of their own window and universe):

1. The universe-membership share, **`not_in_universe` 58.5 %** of the 30-day set against the
   130-pool universe. It is a pool-membership count and does not depend on route classes.
   - On 2026-09-28 the comparable figure is 39 / 252 = 15.5 %, or 45 / 252 = 17.9 % under the
     August universe. The peer flow itself differs, so this is not a trend.
   - The expansion moved exactly the six Moe V1 arbs from absent to evaluable (45 → 39).
2. The WHI-999 **pool ranking**, answered as a universe-side question: "which pools would have been
   needed" for the in-scope arbs, ranked by marginal arbs.
3. **`residual_events_with_missing_pool = 0`**: the 3,535 residual all have every hop in the universe.

**Scope-limited** (true only for the route classes the engine could evaluate):

1. **`evaluated_but_unprofitable = 0`.**
   - In August it was **not measured at all**: there was no concurrent ledger, and the evaluable
     slice was 8 of 6,962 cycles.
   - On 2026-09-28 it is 0 again, but over a slice of 6 approvable arbs, all of which the engine's
     model priced as profitable on the prior-block state (timely availability unproven, §1a).
   - It is not evidence of a healthy engine across the cycle space or across competitor routes.
2. **The 3,535 `unattributable` residual.** It was read as "in-universe, cause pending". On a
   concurrent post-fix day, the in-universe in-scope residual resolves to **route class unapproved
   135 / 141** and modeled profitable but not attempted 6 / 141.
3. **"The engine is not the bottleneck; the universe is."** This conclusion is **not retained,
   not even narrowed to the approved slice.** The earlier evidence never tested the engine: there
   was no concurrent ledger, and 8 of 6,962 cycles were evaluable. The 2026-09-28 evidence cannot
   establish it either: engine latency, execution and conversion are unmeasured (§1a).
   - What does hold: *the attribution held for the route classes the engine could evaluate, and
     cannot be generalised to the full cycle space or to competitor routes.* The universe was the
     only cause the earlier work measured.
   - On 2026-09-28 the engine's model detected the six approvable peer routes on the relevant pool
     state (§1a). That is a detection-on-state result, not an engine-health or timeliness result.
   - On this post-expansion day, the largest in-scope cause is **route class unapproved (75.0 %)**,
     not absent pool (21.7 %). Route-class qualification, i.e. gas-profile approval of `v3+v3`,
     `v3+v3+v3` and the Moe-LB/V3 mixes, is at least as binding as the universe.
   - A pool added on an unapproved class unlocks nothing until that class is approved.

Nothing here says the earlier attribution had an implementation bug. It counted what it could see.
The scope it could see was not stated.

## 7. Non-claims

- No profitability, PnL or capital claim. Modeled nets are at the 10 WMNT shadow cap. The peers'
  gas spend (50–80 % of gross) shows the modeled net is not what a race would net.
- No claim about winning or losing races. There were no sends, so bucket 6 is structurally 0.
- No claim of timely detection or of engine latency. The six modeled positives were all recorded
  at or after the peer's block timestamp, and the host/sequencer clock offset is unbounded (§1a).
- No claim that "the engine is not the bottleneck", in any scope.
- No claim about days other than 2026-09-28, and no claim beyond the approved slice (0.78 % evaluation coverage).
- The candidate rows the engine produced with no peer on the same route in the next block
  (`h3:v2+v2+v2` 6, `h2:v2+v3` 14; `aggregates.json`) are **not** claimed as missed-by-peers
  opportunities. The `h2:v2+v3` ones were `eligible=0` in `block_summary`.
- The universe, gas profile, thresholds, send logic, contracts and dependencies are unchanged.

## 8. Reproduce

Inputs and hashes are in `manifest.json`. External scratch lives at
`RUN/whi1412-scratch/` (`SHA256SUMS`). `RUN` is the orchestrator run dir named in the manifest.

```bash
cargo build --locked --release --bin ground_truth_collector --bin peer_attribution
H=$RUN/sept28-inputs/host; D=$RUN/sept28-inputs/dune; S=/tmp/whi1412
sed '1s/executor_address/to/' $D/arb_detail_feed_20260928.csv > $S/feed_collector.csv
target/release/ground_truth_collector collect --input $S/feed_collector.csv \
  --from-block 101211644 --to-block 101254843 --known-bots-out $S/known_bots.json \
  --events-out $S/events.jsonl --report-out $S/gt_report.json
(head -1 $H/ledger.jsonl; cat $H/ledger_cut_20260928.jsonl) > $S/ledger_day_with_header.jsonl
for m in same pre; do target/release/peer_attribution --events $S/events.jsonl \
  --universe data/pool_universe.csv --ledger $S/ledger_day_with_header.jsonl \
  --gas-profile config/gas_profiles/mantle_mainnet_v1.json $([ $m = pre ] && echo --pre-state) \
  --json-out $S/pa_$m.json --six-way-json-out $S/six_$m.json; done
for f in signerless.log.5 signerless.log.4 signerless.log.3 signerless.log.2 signerless.log.1 signerless.log; do
  LC_ALL=C sed 's/\x1b\[[0-9;]*m//g' $H/logs/$f | LC_ALL=C grep -a "block_summary block="; done > $S/block_summary.log
# read-only RPC: day-bound block timestamps -> $S/day_bounds.txt ("<block> <ts>" lines);
# `cast receipt <tx> --json` for the six profitable_but_not_attempted peer txs -> $S/rcpt_<tx>.json
python3 evidence/peer-attribution/whi-1412/derive.py --run $RUN --scratch $S --repo . \
  --out evidence/peer-attribution/whi-1412/aggregates.json
# committed tool outputs: same `pre` invocation + --summary-only, written into this directory
```

## Files

| file | content |
|---|---|
| `six_way.json` | tool six-way aggregate (`--summary-only`, no events) |
| `peer_attribution.{json,md}` | tool report, library causes + six-way section (`--pre-state --summary-only`) |
| `ground_truth_report.json` | collector aggregate (252 accepted, 0 exclusions, fingerprint `0x8d0e8665…`) |
| `aggregates.json` | coverage, reconciliation, per-event evidence for the six (no tx hashes), August counterfactual |
| `derive.py` | builds `aggregates.json` from the external inputs |
| `manifest.json` | external input hashes, frozen `SHA256SUMS` digests, tool/binary hashes, RPC fingerprint |
