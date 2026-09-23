# Deferred issues registry

A living log of issues that were **surfaced during review but consciously not fixed**
in the PR that found them. This is not a bug tracker for open work — it is the record of
*known, accepted debt*: things we decided to defer, so a future change touching the same
area starts from knowledge instead of rediscovery.

## How to use this file

- **Add an entry** whenever a review turns up a real issue that a PR deliberately leaves
  unfixed (scope, risk, or priority). Record it here in the same PR that defers it.
- **Reference it** before working on the affected area — check whether the thing you are
  about to "discover" is already logged, and whether a listed fix is now in scope.
- **Close an entry** by moving it to *Resolved* (bottom) with the PR/commit that fixed it,
  rather than deleting — the history is useful.
- Keep entries short. Link the originating Linear issue / PR and the code symbol so the
  entry stays findable as the code moves.

Severity is the reviewer's judgement at defer time: **High** (correctness/safety, fix
soon), **Medium** (operational/perf, fix when convenient), **Low** (nit/consistency).

---

## Open

### DI-44 — WHI-1409 route-key contract fix: profile expansion deferred + no hot-path cache yet
- **Severity:** Medium (correctness for the route-key *construction* bug is fixed; the
  two follow-ups below are explicitly permitted by WHI-1409's own spec, not bugs)
- **Source:** WHI-1409 code review (Standards + Spec axes, rounds 1-2)
- **Where:** `src/service/path_index.rs` — `RouteAwareFeeCost::fee_cost`,
  `topology_never_approved_reason`; `config/gas_profiles/mantle_mainnet_v1.json`
- **What:** WHI-1409 fixed the producer/consumer route-key *contract* bug (optimize no
  longer guesses a zero-crossing-bucket key; it prices every candidate with the route
  key the same simulation materialize uses). Two items from WHI-1409's own acceptance
  criteria are **not** closed by that fix alone:
  1. **Profile expansion (AC-2/AC-3).** "A test enumerates the topology classes the
     current universe generates ... zero `UnknownRoute` results" and "3-hop v3/moe
     routes resolve rather than returning `UnknownRoute`" are only partially true: mixed
     V3+Moe topologies (e.g. `['v3','moe','v3']`) have **no profile entry at any
     crossing bucket at all** and still resolve `UnknownRoute` —
     `topology_never_approved_reason_finds_existing_nonzero_bucket_entries` asserts this
     directly. Closing it requires *approving* new route classes via real Mantle fork
     gas measurement (WHI-557 methodology) — already tracked as DI-10
     ("Mantle state-fork deep tick/bin + multi-hop gas qualification"). WHI-1409 did not
     add or promote any profile entry.
  2. **No hot-path cache (spec: "cache or bound the hot-path cost after correctness is
     established").** `RouteAwareFeeCost::fee_cost` independently re-runs
     `simulate_mixed_path_with_route_key` for every candidate `amount_in` instead of
     sharing the quote closure's simulation of that same input — chosen deliberately in
     round 1 to remove a `RefCell`-based ordering coupling the round-1 review flagged as
     fragile. This roughly doubles AMM simulation work per optimizer sample (the
     `amm_quotes` counter, which counts only the quote closure's calls, now undercounts
     actual simulation work by about 2x) and defers the "bound the cost" half of the
     spec's own permission.
- **Why deferred:** (1) requires live Mantle RPC / fork infrastructure this session does
  not have, and DI-10 already owns exactly that campaign — duplicating it here would be
  scope creep into already-tracked work. (2) is an explicit, reviewed correctness-over-
  latency tradeoff the spec itself sanctions ("do not trade correctness for latency
  here"); caching requires either a per-path amount-keyed cache invalidated with the
  existing gross-quote cache, or restructuring the quote/fee split to share one
  simulation per sample — either is a real design decision better made once real
  profitable-candidate latency data exists, not speculatively.
- **Suggested fix:** (1) Run the DI-10 fork-measurement campaign, then add
  `Approved`/explicit `Unsupported` profile entries for the route classes the reconciled
  optimizer actually emits (including mixed V3+Moe multi-hop); re-run
  `topology_never_approved_reason_finds_existing_nonzero_bucket_entries`-style coverage
  against the expanded profile. (2) Once real per-block latency under the reconciled
  path is measured, either cache `simulate_mixed_path_with_route_key`'s result per
  `(path, amount_in)` for the duration of one optimize call (shared between the quote
  closure and the fee model), or fold gross-quote and fee-cost computation into a single
  quote-closure return value and drop the separate `FeeCostModel` call for this path.

### DI-43 — WHI-1411 discovery reject reasons stay `&'static str`, not a typed enum
- **Severity:** Low (design consistency; no observed correctness impact — the catch-all
  bucket is intentional, not accidental)
- **Source:** WHI-1411 code review (Standards axis)
- **Where:** `src/service/path_index.rs` — `materialize_from_cache` returns
  `Result<DiscoveredOpportunity, &'static str>`; `DiscoveryRejectCounts::record(&str)`
  re-parses the reason string with a `_ => other` fallback arm.
- **What:** The reviewer flagged that a raw `&'static str` error channel, re-matched by
  string value, does not fail closed at compile time the way a `thiserror` enum would
  (CLAUDE.md: "Each domain module owns a typed `error.rs` (`thiserror`) where
  applicable"): a typo in a future reason constant, or a new reason nobody adds a
  bucket for, silently lands in `other`.
- **Why deferred:** `src/metrics/reject_reason` was **already** a plain `&'static str`
  constant module before this PR (`POOL_LOOKUP`, `NO_OPTIMUM`, `ZERO_PROFIT`,
  `GROSS_UNDERFLOW`, `HOP_CAP`, `GAS_SCREEN`, `GAS_RESERVE`, `NET_PROFIT`,
  `EXPECTED_STATES`, `MIXED_SIM_ERROR`, `OPTIMIZE_ERROR` all predate WHI-1411); this PR
  added three more constants (`UNKNOWN_ROUTE`, `UNAPPROVED_ROUTE`, and
  `ROUTE_KEY_CONSTRUCTION_ERROR`, the last added in round-3 review to stop a defensive
  route-key-construction failure from being misclassified as `UNKNOWN_ROUTE`) and one new
  string-keyed consumer (`DiscoveryRejectCounts::record`) that follows the same
  pre-existing convention. `ROUTE_KEY_CONSTRUCTION_ERROR` is in fact a live example of
  the exact risk this entry names: it is deliberately *not* given an explicit match arm
  in `DiscoveryRejectCounts::record`, so it silently falls into `other` by design — the
  string-matching approach makes "new reason, no bucket" indistinguishable from "typo in
  a reason constant" at compile time, both landing in the same catch-all. In this one
  case that's the intended behavior (the spec's own `other` catch-all), but it is the
  same mechanism that would hide a genuine typo. Converting the whole reject-reason
  system to an enum touches ~13 call sites across `path_index.rs`, `fee_scoring.rs`, and
  `metrics/record.rs` that this issue did not otherwise modify — a substantially larger,
  unrelated refactor than WHI-1411's stated scope ("this issue ensures the next
  occurrence is loud", not a reject-reason type-system rewrite).
- **Suggested fix:** If/when `reject_reason` is converted to a `thiserror`-style enum
  repo-wide, thread the same enum through `materialize_from_cache`'s return type and
  `DiscoveryRejectCounts::record` in the same change so the two stay in lockstep.

### DI-42 — WHI-1411 reject-reason field list is hand-enumerated in four places
- **Severity:** Low (readability/maintenance; compiler catches missing fields on the
  struct itself, just not at every call site)
- **Source:** WHI-1411 code review (Standards axis)
- **Where:** `src/service/path_index.rs::DiscoveryRejectCounts` — the six fields
  (`unknown_route`, `unapproved_route`, `pool_lookup`, `no_optimum`, `zero_profit`,
  `other`) are individually listed in `total()`, in the `tracing::error!` liveness-alarm
  call, in `BlockSummary::emit`'s structured fields, and in each test's field-presence
  assertions.
- **What:** Adding a seventh reject bucket in the future means touching all four sites
  by hand; nothing enforces they stay in sync beyond code review.
- **Why deferred:** `DiscoveryRejectCounts` is a small, low-churn struct (six known
  causes tied 1:1 to the discovery pipeline's actual rejection points, matching the
  spec's own "at minimum" list verbatim); introducing a derive-macro or
  reflection-based iteration for six fields would add machinery disproportionate to
  the problem, and Rust's struct-literal field-count check already forces every
  construction site to list all six explicitly — a missing field is a compile error,
  not a silent gap. The remaining risk (three *usage* sites drifting, not the struct
  definition) is a readability nit, not a correctness one.
- **Suggested fix:** If a seventh bucket is ever added and drift becomes a real
  maintenance cost, consider a small `for (name, value) in rejects.iter_named()`-style
  helper (hand-written, not macro-derived) that `total()`, the alarm log, and
  `BlockSummary::emit` can all share.

### DI-41 — WHI-1411 "pipeline dead / liveness-unknown" rule is expressed twice (live engine vs. offline digest)
- **Severity:** Low (independently tested in both modules; divergence would be visible
  immediately in either module's own test suite)
- **Source:** WHI-1411 code review (Standards axis, updated round-3)
- **Where:** `src/service/path_index.rs::DiscoveryEngine::discover` computes a single
  `liveness_alarm` boolean from live per-pass counters (`cycles_optimized`,
  `paths_quoted`, `consecutive_dead_heads`); `src/notify/digest.rs::build_operational_activity`
  separately computes a **tri-state** result (`is_pipeline_dead`,
  `pipeline_liveness_unknown`, healthy) from aggregated ledger rows read back after the
  fact, including a per-row `has_paths_quoted_coverage_gap` concept
  (`cycles_optimized > 0 && paths_quoted == None` on that same row) that the live engine
  has no counterpart for at all — the live engine always has `paths_quoted` for its own
  in-process pass, so "was this row's field ever recorded" is a question that only makes
  sense for the offline reader. As of round-3 review these are no longer "the same idea,
  computed twice" but two related, differently-shaped models over different data.
- **What:** Both still encode a shared underlying idea ("cycles were evaluated but zero
  paths reached the optimizer"), so a future change to that core definition could update
  one and miss the other; but the offline model now has additional states (unknown /
  partial-coverage) that do not map onto the live engine's single boolean at all, so a
  literal shared-function unification is no longer even structurally possible without
  first deciding what the live engine's equivalent of "unknown" would mean.
- **Why deferred:** The two live in different layers with genuinely different data
  models and time semantics by design — `path_index.rs` is live, in-process,
  per-discovery-pass state with a multi-pass sustained-window counter and no concept of
  a missing field (its own current pass always has the data); `src/notify/digest.rs` is
  a pure, stateless aggregator over historical ledger rows for one UTC calendar day that
  must additionally handle schema evolution (older ledger rows that predate this PR and
  never recorded `paths_quoted` at all), which is precisely why it needs a third state
  the live engine does not. Per this repo's own module doc (`src/notify/mod.rs`: "used
  only by `lark_daily_digest.rs`, never by `bot.rs`"), `src/notify` is intentionally
  decoupled from the live discovery engine — zero compile-time dependency today.
  Unifying the two would now require the live engine to grow a currently-meaningless
  "unknown" state just to share a function signature with the offline reader, which is
  net new complexity in the live hot path to serve an offline-only need.
- **Suggested fix:** If a bug is ever found where the two disagree on the *same*
  underlying data (both looking at a window where paths_quoted was always present),
  revisit whether a small shared pure function over that common subset
  (`fn is_pipeline_dead(cycles_optimized: u64, paths_quoted: u64) -> bool`) is worth the
  indirection. Do not attempt to unify the "unknown" / coverage-gap state into the live
  engine — that state is inherent to reading historical, potentially-pre-this-PR data,
  not to live discovery.

### DI-40 — WHI-1407 acceptance items requiring live host/webhook access are unverified in this PR
- **Severity:** High (go-live gate: two of the issue's acceptance checkboxes cannot
  be ticked from this environment; the operator must complete them before treating
  the digest as production-ready)
- **Source:** WHI-1407 code review (Spec axis) / this PR's own implementation session
  (no SSH reachability to any deploy host — `whi715-vps`, `arb-bot-vps`, `arb-bot-jp`
  all timed out from the sandbox that wrote this code; no real `LARK_WEBHOOK_URL`
  secret available either).
- **Where:** WHI-1407 acceptance criteria: "Actual JP retained-ledger history is
  checked to cover the reporting window before this issue is called done" and
  "First real card generated against the live JP ledger reviewed by the operator".
- **What:** `scripts/golive/check_ledger_retention.sh` (read-only retention check)
  and `scripts/systemd/README.md` steps 3–5 (`--send-test`, retention check,
  `--dry-run` operator review) give the operator the exact commands to close both
  items, but **no one has actually run them against the live deployment** as of
  this PR. Do not read the runbook's existence as evidence the checks passed.
- **Why deferred:** Outside what an agent without host/secret access can complete;
  genuinely requires a human operator with SSH access and the real webhook.
- **Suggested fix:** Operator runs, on the deploy host: `scripts/golive/
  check_ledger_retention.sh <ledger-path> <YYYY-MM-DD>` for a representative day,
  then `cargo run --release --bin lark_daily_digest -- --send-test` against the
  real webhook, then `--dry-run` against the live ledger for operator sign-off
  before enabling the timer. Close this entry once done.

### DI-39 — WHI-1407 mock-HTTP-server test helper duplicated across two compilation units
- **Severity:** Low (test-only; no production impact)
- **Source:** WHI-1407 code review (Standards axis)
- **Where:** `src/notify/lark.rs`'s `#[cfg(test)] mod tests` and
  `tests/lark_daily_digest.rs` each carry their own byte-similar
  `find_double_crlf` / `parse_content_length` / mock-webhook-server helper
  (~60 lines).
- **What:** Lib unit tests and an integration test file are separate compilation
  units; a `#[cfg(test)]`-gated helper in the lib is invisible to `tests/*.rs`
  (which link the lib built *without* `cfg(test)`), so genuine sharing would need
  an always-compiled test-support module (or a Cargo feature gate) rather than a
  simple extraction.
- **Why deferred:** Same accepted-duplication shape as DI-28's precedent (cross-
  compilation-unit test helpers); the cost of a `test-support` feature/module for
  ~60 lines used by exactly two call sites was judged not worth it for this PR.
- **Suggested fix:** If a third test file ever needs the same mock server, add a
  small `#[cfg(feature = "test-support")] pub mod test_support` to the lib crate
  (matching this repo's existing test-only-feature convention — see
  `signing-test-util` / `e2e-test-util` in `Cargo.toml`) and migrate all three call
  sites onto it.

### DI-38 — `LedgerDiscoveryView.skipped`/`skip_reason` mirrored but always-false on observation rows (WHI-1407)
- **Severity:** Low (dead weight, not a correctness risk)
- **Source:** WHI-1407 code review (Standards axis)
- **Where:** `src/notify/ledger_window.rs` — `DiscoveryRecord.{skipped,skip_reason}`,
  populated from the wire `discovery` view on every `observation` row.
- **What:** Per this issue's own Context note, `BotWatchHooks::on_block_ready` only
  writes an `observation` row for **processed** heads and always sets
  `discovery.skipped=false` on it — a genuinely skipped head bypasses the hook
  entirely and never becomes a row this reader can see. So `skipped`/`skip_reason`
  are always `false`/`None` on every row this digest will ever read today; keeping
  them is a faithful 1:1 mirror of the wire shape, not a fabricated field, but the
  digest itself never consumes them.
- **Why deferred:** Removing them narrows `DiscoveryRecord` for no present benefit
  and would need touching several existing tests; kept for wire fidelity in case a
  future schema revision ever makes this field meaningful on an observation row.
- **Suggested fix:** If `DiscoveryRecord` grows more unused fields over time,
  revisit removing this pair rather than accreting more dead wire mirrors.

### DI-37 — WHI-1406 Dune export `is_sandwich='unknown'` silently defaults to non-excluded in the existing collector
- **Severity:** Medium (correctness of a downstream reconciliation, not of the
  Dune qualification itself; no production/execution-path impact)
- **Source:** WHI-1406 code review (Standards axis), round 1.
- **Where:** `src/service/ground_truth.rs` — `load_candidates_dune_csv`'s
  `parse_bool` helper for the `is_sandwich`/`sandwich` CSV column, and
  `classify_candidate`'s `is_sandwich: c.is_sandwich.unwrap_or(false)`
  default in `StructuralFlags`. Interacts with
  `scripts/dunesql/00_qualified_arbs.sql`'s `is_sandwich` output, which is a
  three-value string (`'true'`/`'false'`/`'unknown'`), not a boolean.
- **What:** The collector's CSV loader only recognizes
  `"1"/"true"/"t"/"yes"` and `"0"/"false"/"f"/"no"` (case-insensitive) for
  boolean-ish columns. WHI-1406's `is_sandwich='unknown'` (the honest,
  coverage-aware default whenever `dex.sandwiches`/`dex.sandwiched` have zero
  rows for the requested window — true for this pack's default trailing
  3-month window) does not match either set, so `parse_bool` returns `None`,
  and the collector's existing default (`unwrap_or(false)`) treats it as
  "not a sandwich" — i.e. never excludes on sandwich grounds for an
  `'unknown'` row, even though the underlying data genuinely doesn't support
  a `false` verdict either. A tx that actually was a sandwich, in a window
  where the sandwich tables have no coverage, would pass through the
  collector's structural check uncontested.
- **Why deferred:** WHI-1406 is scoped to Dune SQL only ("Not a Rust
  crawler... no broad Rust changes" per the issue). Changing the collector's
  boolean-defaulting behavior, or widening its schema to a tri-state
  sandwich flag, is a `src/service/ground_truth.rs` change outside that
  scope. Documented in `scripts/dunesql/README.md`'s "Collector
  compatibility" section so it is not silently relied upon.
- **Suggested fix:** Either (a) teach `load_candidates_dune_csv` to treat an
  unrecognized/`'unknown'` sandwich value as fail-closed (`Some(true)`,
  i.e. exclude) rather than `None`, when an explicit WHI-1406-style export is
  detected, or (b) add a dedicated tri-state field to `DiscoveryCandidate`
  (`sandwich_verdict: unknown|true|false`) so the ambiguity is representable
  end-to-end instead of collapsing to a boolean at the CSV boundary.

### DI-36 — `batch_create` counter tests race on a process-wide atomic (pre-existing, surfaced by WHI-999)
- **Severity:** Medium (test-suite reliability; no production impact — the counter
  itself is only observability)
- **Source:** WHI-999 review round 3 / full-suite run. Surfaced by, **not caused
  by**, that PR: `src/amms/batch_create.rs` is byte-identical to `dev`.
- **Where:** `src/amms/batch_create.rs` — `BATCH_CREATE_CALLS` (process-wide
  `AtomicU64`) with `batch_create_call_count()`; asserted by
  `with_create_size_split_records_each_attempt`,
  `record_batch_create_call_increments_counter`,
  `create_size_limit_halves_down_to_floor`, `scales_to_synthetic_400_pools`, and
  `tick_data_batch_over_limit_halves_and_completes`.
- **What:** Each of those tests snapshots `before = batch_create_call_count()`,
  drives some CREATEs, then asserts `count() == before + N`. They run in parallel
  in one test binary and all increment the same global, so any interleaving makes
  the delta too large (`left: 31, right: 30`). Reproduced 38 failures in 40 runs
  when those four are selected together; passes when the module's 16 tests run
  alone, which is why it survived until a PR that added tests changed the
  scheduling. The counter's own doc comment recommends a before/after delta
  "so concurrent tests do not clobber each other" — but a delta is exactly what
  concurrency breaks; only a reset under a shared lock, or a per-call sink, is
  sound.
- **Why deferred:** `src/amms/batch_create.rs` is outside WHI-999's scope (one
  Linear issue = one PR, `CLAUDE.md`), and WHI-999 touches no `amms/` file. Fixing
  it here would mean a research PR editing the WHI-921/WHI-925 CREATE recovery
  path.
- **Suggested fix:** Give the assertions a `#[serial]`-style shared mutex, or
  better, have `with_create_size_split` accept an optional per-call attempt sink
  so tests count their own attempts instead of reading a global. Keep the global
  for production observability only, and drop the delta assertions.

### DI-35 — WHI-957 concurrent shadow + block_views dirty-cycle measurement (operator evidence)
- **Severity:** High (go-live gate: tooling is in, but the WHI-940 equivalence
  claim — `dirty_cycle_filter_skipped == 0` over real arbs — is still
  **`not_measured`**)
- **Source:** WHI-957 Spec / operator brief (attribution taxonomy comment)
- **Where:** operator concurrent run: signerless `--watch` ledger +
  `block_discovery.jsonl` (`dirty_pools`, `skipped`, `scope`) + WHI-956
  `ground_truth_collector` over the **same** block range; then
  `cargo run --release --bin peer_attribution -- --ledger … --block-views …`
- **What:** Offline pass on the 30d GT set (`evidence/peer-attribution/offline_universe.*`)
  attributes 10,501 events (not_in_universe 6139, oos hops/settlement 819,
  aggregator 8, unattributable 3535) but cannot separate dirty-cycle vs evaluate
  without concurrent dirty sets. Existing ledgers (full-universe ~98.95M,
  agni-window, …) do **not** overlap the GT window (96.81M–98.10M) and carry no
  dirty-pool rows. This PR also embeds `discovery` on new observation rows
  (`dirty_pools`, `scope`, cycle counts) so the next ledger is self-sufficient.
- **Why deferred:** Requires a wall-clock concurrent mainnet session (RPC +
  external events path). Code path is unit-tested: dirty-cycle is never
  inferred from "no candidate" alone; synthetic fixtures separate dirty-cycle
  from evaluated_but_unprofitable; `block_skipped` is a separate counter.
- **Suggested fix:** After merge, run concurrent window with the fixed binary
  (ledger observations carry `discovery`), collect GT for the same range, run
  `peer_attribution --ledger … --summary-only`, commit summary with
  `dirty_cycle_evidence=measured_zero|measured_nonzero`, reopen WHI-940 only if
  `measured_nonzero`.

### DI-34 — WHI-980 post-fix ≥30-min `--watch` eth_getLogs cross-check (operator evidence)
- **Severity:** High (go-live gate: code fix is in, but AC still requires a live window
  proving `affected>0` / `dirty_pools>0` / `cycles_optimized>0` on blocks with real
  universe Swaps)
- **Source:** WHI-980 Spec review (Round 1)
- **Where:** operator `--watch` run; independent `eth_getLogs` over the same range
  filtered to universe pools + V2 Sync / V3 Swap topics; compare to bot `affected`
- **What:** WHI-980 fixed dual Swap topics + canary + unit regressions, but did not
  re-run the ≥30-minute watch experiment from the issue AC (needs live RPC + wall time).
  WHI-886 agni-window is annotated invalid for watch-mode stats; no replacement package.
- **Why deferred:** Out of scope for a pure code fix PR; requires a long-lived mainnet
  session and human-held credentials. Code path is covered by unit tests (topic set,
  UniV3-family Swap apply, empty-filter fail-closed, empty-log canary).
- **Suggested fix:** After merge, run ≥30 min `--watch` on the fixed binary, paste
  independent getLogs counts vs bot `affected`, and attach under `evidence/shadow/`.

### DI-32 — WHI-860 send path uses BoundSendIdentity + local pool params (not live ParamsBuilder)
- **Severity:** Medium (canary correctness: identity/params lag tip by design of the
  status-bound path; on-chain minProfit + deadline remain the principal backstop)
- **Source:** WHI-860 review (Standards / Spec round 1)
- **Where:** `src/service/send_path.rs` (`BoundSendIdentitySource`,
  `execution_params_inputs_from_pools`); previously DI-21 / DI-18 for example services
- **What:** Production send prepares FinalRequest from block-synced local AMM state and a
  fabricated Ready `MarketSnapshot` (empty pool map) rather than `ParamsBuilder` live
  reads + `LiveExecutionIdentitySource` over `SnapshotPublisher`. Shadow ledger records
  Submitted via the gate-blocked row helper until a dedicated submitted schema exists.
  No anvil fork E2E in-tree for enable-sends → receipt confirm.
- **Why deferred:** Full live-identity + ParamsBuilder wiring needs SnapshotPublisher
  fee/route invalidation on every head (watch loop already publishes snapshots) and a
  durable submitted-row schema; both are follow-ups, not blockers for the arm/gate
  plumbing. Fork rehearsal remains an operator runbook step (see WHI-860 Testing).
- **Suggested fix:** After WHI-548 funding, bind `LiveExecutionIdentitySource` to the
  watch-loop publisher, replace local params with `ParamsBuilder` at the candidate tip
  hash, add `record_submitted` ledger row + ignored `#[cfg]` fork test.

### DI-26 — `ledger.rs` serde-mirror types duplicate `preflight`'s enums by hand
- **Severity:** Low (nit/consistency — each mirror is a small, mechanically-verified
  `From` impl; the risk is drift between the two definitions, not a correctness bug today)
- **Source:** WHI-549 PR review (Opus 5 escalation pass)
- **Where:** `src/execution/shadow/ledger.rs` (`LedgerPolicyKey`, `LedgerBlockTag`,
  `LedgerRpcErrorClass`, `LedgerOutcome`, each with a hand-written `From<preflight::X>`)
- **What:** `preflight::PolicyKey`, `BlockTag`, `RpcErrorClass`, and `PreflightOutcome`
  have no `Serialize`/`Deserialize` (WHI-521 never needed one), so `ledger.rs` owns a
  parallel "Middle Man" enum per type purely to give the ledger's JSONL rows a wire
  format, plus a manual conversion keeping each pair in sync by hand.
- **Why deferred:** The honest fix is deriving `Serialize`/`Deserialize` upstream on the
  `preflight` types directly, but that touches three otherwise-unrelated call sites
  (`preflight.rs`'s own public API, and anything else matching on those enums) beyond
  WHI-549's scope. The mirrors are exhaustively matched (a new upstream variant fails to
  compile here, it doesn't silently serialize wrong), so the drift risk is caught at
  compile time, not silently absorbed.
- **Suggested fix:** Add `#[derive(Serialize, Deserialize)]` directly to `PolicyKey`,
  `BlockTag`, `RpcErrorClass`, and `PreflightOutcome` in `preflight.rs` (with
  `#[serde(rename_all = "snake_case")]` to match the ledger's existing wire format), then
  delete the four `Ledger*` mirror types and their `From` impls in favor of serializing
  the real types directly.

### DI-23 — `abort_prepare` + `reconcile` cleanup pairing is duplicated across four sites
- **Severity:** Low (each occurrence is a two-line, well-understood idiom; a shared
  helper would be a pure refactor with no behavior change)
- **Source:** WHI-555 PR review (rounds 2-3)
- **Where:** `src/execution/pipeline.rs` (`impl Drop for PreparedPipelineHead`,
  `prepare_pipeline_head`'s `cleanup` closure, `into_closed_outcome`) and
  `src/execution/e2e/capability.rs` (`impl Drop for E2eSignPermit`)
- **What:** All four call the same pair — `sm.abort_prepare(nonce)` then
  `sm.reconcile(chain.clone())`, both best-effort (errors dropped or merged into a single
  log line) — independently, each with its own local rationale comment.
- **Why deferred:** `PreparedPipelineHead`'s three call sites already had this
  duplication before WHI-555; `E2eSignPermit`'s `Drop` is a fourth copy mirroring the
  existing pattern rather than introducing a new one. Extracting a shared
  `IntentStateMachine` method (e.g. `release_reserved(nonce, chain)`) touches
  `pipeline.rs`'s and `intent.rs`'s established public surface beyond a single-issue PR
  and is better done as its own focused refactor.
- **Suggested fix:** Add `IntentStateMachine::release_reserved(&self, nonce: u64, chain:
  ChainNonceView)` wrapping the abort+reconcile pair (best-effort, matching current
  behavior), and have all four call sites use it.

### DI-24 — `mint_action_permit`'s `(action, digest)` pairing isn't type-enforced
- **Severity:** Low (nit/consistency)
- **Source:** WHI-555 PR review (round 2)
- **Where:** `src/execution/e2e/capability.rs` (`VerifiedE2eManifest::mint_action_permit`,
  `mint_trigger_permit`, `mint_cancel_permit`)
- **What:** `mint_trigger_permit`/`mint_cancel_permit` each compute a digest with the
  domain-specific function for their own action, then pass `(action, digest.0)` to the
  shared `mint_action_permit` helper — nothing at the type level stops a future edit from
  passing a mismatched `(action, digest)` pair (e.g. `Trigger` action with a
  cancel-domain digest). Separately, `TriggerRequestDigest`/`CancelRequestDigest` are
  unwrapped to a raw `B256` at the `mint_action_permit` boundary, while
  `BootstrapActionPermit` keeps its typed `BootstrapRequestDigest` all the way through.
- **Why deferred:** Both call sites are two lines apart in the same file and construct
  the pairing correctly today; the risk is latent, not active, and a fully type-safe
  fix (e.g. an enum carrying its own typed digest variant) is a larger refactor than
  this PR's scope.
- **Suggested fix:** Replace the `(E2eSignAction, B256)` parameter pair with a small enum
  (`TriggerDigest(TriggerRequestDigest) | CancelDigest(CancelRequestDigest)`) that
  determines `action` itself, removing the possibility of mismatch.

### DI-22 — `PreparedPipelineHead::Drop` cleanup is best-effort and unobservable to the caller
- **Severity:** Low (the head now owns its SM handle, so cleanup always targets the right
  SM; only the error channel is lossy)
- **Source:** WHI-553 PR review follow-up
- **Where:** `src/execution/pipeline.rs` (`impl Drop for PreparedPipelineHead`)
- **What:** Dropping a prepared head without a continuation runs `abort_prepare` +
  `reconcile` and logs at error level, but `Drop` cannot return a `Result`. If both
  cleanup calls fail (e.g. a poisoned SM mutex), the only signal is the log line.
- **Why deferred:** Dropping a head is already a caller bug; the supported paths are
  `into_closed_outcome` (returns the cleanup error) and WHI-525's E2E continuation. A
  louder mechanism (panic-on-drop, or a shared "leaked intents" counter surfaced to the
  breaker) is a policy decision that belongs with the send-enablement work (WHI-526).
- **Suggested fix:** When the send gate opens, feed drop-time cleanup failures into the
  breaker/operator-alert path instead of a bare `tracing::error!`.

### DI-21 — Example services derive `ExecutionParams` registration fields from local pool state, not fresh on-chain reads
- **Severity:** Medium (up to one block of staleness in `pool_tokens` /
  `expected_reserves_u112`; production send gate stays closed, so nothing is submitted)
- **Source:** WHI-553 implementation / PR review (the code comment previously pointed at
  docs/DEFERRED_ISSUES.md without a matching entry)
- **Where:** `examples/protocols/intent_service_support.rs`
  (`execution_params_inputs_from_pools`); `src/execution/params.rs`
  (`ParamsBuilder::build`, crate-private)
- **What:** The production builder resolves `pool_types` / `pool_tokens` /
  `expected_reserves_u112` with live `detect_pool_meta` + `getReserves` reads. It is
  crate-private and unreachable from `examples/`, so the three monitor services derive the
  same fields from their already block-synced local `AMM` state, which can lag on-chain
  state by up to one block.
- **Why deferred:** Exposing (or re-hosting) the production derivation is the same
  refactor as DI-20 and is out of WHI-553's scope; with `production_send_allowed() ==
  false` no request built from these fields is ever signed or broadcast.
- **Suggested fix:** Expose a public, provider-driven params-derivation entry point from
  `src/execution` and have the services call it before the send gate opens.

### DI-20 — `min_amount_out` / `mul_fraction` / pool-type-byte mapping are duplicated in `examples/`
- **Severity:** Low (consistency; two copies of one derivation kept in sync by hand)
- **Source:** WHI-553 PR review
- **Where:** `examples/protocols/intent_service_support.rs`
  (`min_amount_out_from_plan`, `mul_fraction`, the `AMM` → pool-type-byte match in
  `execution_params_inputs_from_pools`) vs `src/execution/params.rs`
  (`ParamsBuilder::build`'s inline `min_amount_out` derivation, `mul_fraction`) and
  `src/execution/executor.rs` (`pool_type_byte`)
- **What:** The example-side helpers re-implement crate-private production logic verbatim.
  A change to slippage/non-loss policy or the pool-type byte constants must be made twice
  or the three services silently diverge from the on-chain encoding.
- **Why deferred:** The right fix is to expose the derivation from `src/execution` (a
  public params/min-out surface) and delete the example copies. That touches the
  production params builder's API and is broader than this review pass.
- **Suggested fix:** Promote `min_amount_out` + `mul_fraction` into a public helper in
  `src/execution` (alongside the already-public `pool_type_byte`) and have
  `intent_service_support.rs` call it, removing the hand-synced copies.

### DI-18 — Three monitor services use `StatusBoundIdentitySource`, not a live `SnapshotPublisher`-backed source
- **Severity:** Medium (identity revalidation is real but snapshot-status-derived, not
  independently sourced; production send gate stays closed so no live-send exposure yet)
- **Source:** WHI-553 implementation / PR review
- **Where:** `examples/protocols/intent_service_support.rs` (`StatusBoundIdentitySource`);
  `src/execution/identity.rs` (`LiveExecutionIdentitySource`);
  `v2_monitor_executor_service.rs`, `v3_monitor_executor_service_1559.rs`,
  `moe_monitor_executor_service.rs` (their `run_pipeline_head_closed` wiring)
- **What:** WHI-553 wires all three services through `run_pipeline_head_closed` using
  `StatusBoundIdentitySource`, a minimal stand-in that derives validation directly from
  the `SnapshotStatus` already passed into `prepare_pipeline_head`. A full
  `LiveExecutionIdentitySource` backed by a real `SnapshotPublisher` subscription
  (independent of the caller-supplied status, per the original `ExecutionIdentitySource`
  design intent) is not wired into any service yet.
- **Why deferred:** WHI-553's scope is the wallet-free pipeline-head seam and wiring
  itself; the send gate (`production_send_allowed()`) remains false in all three services,
  so no live send currently depends on identity-source independence. Building the
  `SnapshotPublisher`-backed source is a separable follow-up.
- **Suggested fix:** Wire a real `LiveExecutionIdentitySource` from each service's
  existing `SnapshotPublisher`/`StateSpaceManager` subscription before the production
  send gate opens, and swap it in for `StatusBoundIdentitySource` at each
  `run_pipeline_head_closed` call site.
- **Note (WHI-549):** the shadow-mode counterpart, `execution::shadow::ShadowIdentitySource`,
  is implemented and unit-tested (it wraps a `LiveExecutionIdentitySource` for `validate`
  and always fails `acquire_send_lease` closed) but is likewise not wired into any of the
  three services' `run_pipeline_head_closed` calls, for the same reason: it needs the same
  live `SnapshotPublisher` this issue is about, which none of the services construct yet.
  It is not dead code — it is the shadow-mode type ready to swap in for
  `StatusBoundIdentitySource` once this issue's fix lands.

### DI-12 — WHI-524 remaining operational wiring
- **Severity:** Medium (core ledger/pause/WAL land; service adoption incomplete)
- **Source:** WHI-524 implementation / PR #25 review
- **Where:** `src/execution/breaker/`; `examples/protocols/intent_service_support.rs`;
  monitor services; `examples/pause_control.rs`
- **What:** Library seams landed and unit-tested: `BreakerConfig`, WAL V1 + per-tag/chain
  golden vectors, exclusive store, ledger/streak, `PauseController`, signed operator
  commands with non-zero `InitAnchor`, hash-pinned `revalidate_against_chain` (anchor +
  loss-window + streak + nonce-gap), typed `CoordinatorError` / `AccountingCommit`,
  `guardian()` binding + role checks, `AccountingCommit` before receipt terminalization,
  `WalDurableHook`, shared balance helper, `begin_pause_cancel_sweep` (purge queue +
  enumerate cancel targets), `pause_control` CLI.

  **Not production-wired yet (logic + test seams only — do not read as live runtime
  guarantees):**
  - **Restart hash-pinned revalidation (Spec-1):** `CanonicalChainView` /
    `revalidate_against_chain` exist and are covered by coordinator unit tests
    (`MockChain` only). No provider impl and **no live caller** on the service startup
    path — restart self-heal is not active in the three monitor services.
  - **Pause→pending cancellation (Spec-4 / PR headline):**
    `begin_pause_cancel_sweep` purges a `LatestWinsSlot` and returns cancel targets; it
    does **not** drive cancel prepare/sign/broadcast. No service loop invokes it —
    paused auto-withdraw of pending nonces is not live.

  Still incomplete vs Revision 5 fixtures:
  (1) coordinator `fsync` may run while the SM mutex is held — should queue off-lock;
  (2) pause→pending-cancel driver not auto-wired into the three services (see above);
  (3) control-inbox poller not started by services (Init must supply RPC-sourced
      `InitAnchor`; inbox JSON does not yet carry codehash/block/nonce baselines);
  (4) full crash-injection matrix at every write/fsync/rename boundary;
  (5) inventory over-cap does not yet auto-pause via `AlertSink` in the live loops
      (helper exists: `check_inventory_cap`);
  (6) store security fixtures beyond lock-rejection (symlink/wrong-owner/mode/deletion)
      remain thin.
- **Why deferred:** Vertical slice delivers durable accounting, restart-revalidation and
  pause-cancel **APIs** with tests; remaining items are service-loop adoption, inbox
  payload enrichment, and extra crash/security fixtures that can land without redesigning
  the WAL.
- **Suggested fix:** Open a follow-up ticket (or extend WHI-524) to wire
  `BreakerRuntime` + `revalidate_against_chain(provider)` at startup, inbox poller +
  pause-cancel driver in `intent_service_support`, enrich `control.inbox.json` with
  `InitAnchor` fields, and add crash-point tests around
  `SecureStore::{append_wal,atomic_write}`.

### DI-5 — WHI-519 compile-fail permit opacity test not wired
- **Severity:** Low (visibility enforced by types; no trybuild harness yet)
- **Source:** WHI-519, PR #22 review (round 2)
- **Where:** `src/execution/types.rs` (`ExecutionPermit`); `src/execution/nonce.rs` (`NonceManager`)
- **What:** Acceptance asked for a compile-fail test that external crates cannot construct
  `ExecutionPermit` without `IntentAuthority`, and that `NonceManager` is not reachable
  outside the intent module. Runtime opacity is already enforced (`IntentAuthority` private,
  `NonceManager` is `pub(super)` and not re-exported). A dedicated trybuild / compiletest
  harness is not present in this crate.
- **Why deferred:** Adding trybuild is a build-system change outside the SM correctness fix;
  the type system already fails closed. Documented here so a future test harness can claim it.
- **Suggested fix:** Add an optional `trybuild` dev-dep with negative fixtures under
  `tests/ui/` once the workspace accepts UI tests.


### DI-1 — Moe pool-list on-chain validation + init are not multicall-batched
- **Severity:** Medium (startup latency / RPC pressure; no correctness impact)
- **Source:** WHI-507, PR #7 review (round 3)
- **Where:** `src/amms/moe/pool_list.rs` (`validate_on_chain` / `validate_entry_on_chain`);
  `initialize_moe_pools` in `examples/protocols/moe/moe_monitor_executor_service.rs`
- **What:** Fail-closed startup issues ~768 individual `eth_call`s (192 pools × 4 getters:
  `getFactory`/`getTokenX`/`getTokenY`/`getBinStep`), plus `init_basic` re-reads similar
  data. Calls run at concurrency 8 but are capped by `ThrottleLayer(40)` (40 req/s), so the
  validation phase alone takes ~19s and total startup can run to tens of seconds.
- **Why deferred:** The retry/throttle layer (added in the same PR) makes this safe and
  correct; the cost is only startup time. Batching is a clean, self-contained follow-up.
- **Suggested fix:** Batch the four getters (and the `init_basic` reads) via a multicall
  aggregator so validation + init collapse to a handful of round-trips.

### DI-2 — Redundant `validate_offline` passes in the Moe pool-list load path
- **Severity:** Low (in-memory only, 192 rows; negligible cost)
- **Source:** WHI-507, PR #7 review (rounds 2–3)
- **Where:** `src/amms/moe/pool_list.rs` — `parse_csv` → `load_path` (with meta) →
  `load_and_validate_on_chain` → `validate_on_chain`
- **What:** `validate_offline` runs 2–3 times per load (once in `parse_csv` without meta,
  again in `load_path` once meta is attached, and again inside `validate_on_chain`).
- **Why deferred:** Idempotent and cheap; not worth churning the load path for now.
- **Suggested fix:** Run the offline checks once, after meta is attached, and have the
  on-chain path assume they already passed.

### DI-3 — Example RPC env-var naming diverges from the documented convention
- **Severity:** Low (consistency)
- **Source:** WHI-507, PR #7 review (round 3)
- **Where:** `resolve_http_endpoint` / `resolve_ws_endpoint` in
  `examples/protocols/moe/moe_monitor_executor_service.rs` and sibling
  `*_monitor_executor_service` examples
- **What:** These read `RPC_HTTP_URL` / `MANTLE_HTTP_URL`, whereas `CLAUDE.md` documents a
  chain-prefixed convention (`MANTLE_SEPOLIA_RPC_URL`, `MANTLE_SEPOLIA_RPC_WS_URL`, …).
  Pre-existing example style, not introduced by this PR.
- **Why deferred:** Cosmetic; touches several example entrypoints and their run docs.
- **Suggested fix:** Unify example env-var names with the chain-prefixed scheme (ideally
  when the M3 config consolidation lands — see DN-2).

### DI-4 — Moe swap on-chain differential is never actually run in CI
- **Severity:** Medium (correctness verification gap; low residual risk)
- **Source:** WHI-505, PR #6 review (round 2)
- **Where:** `tests/moe_swap.rs::test_moe_swap_simulation_matches_onchain` (`#[ignore]`);
  the math it exercises lives in `src/amms/moe/math/{tree_math,fee_helper,bit_math,safe_cast}.rs`
- **What:** The equivalence of the Moe LB math (tree traversal, fee, bit ops) with the
  on-chain reference is currently established only *statically* — by matching the Rust to
  `data/contracts/moe/libraries/**` and by offline unit tests — plus the offline
  deterministic fixture `test_moe_swap_simulation_offline_fixture`. The one true differential
  against live `getSwapOut` is `#[ignore]`d (needs RPC/credentials) and so never runs in CI.
- **Why deferred:** Correctly gated per WHI-505 (no silent dependence on local credentials);
  static + offline coverage is sufficient to restore a green tree. End-to-end confirmation is
  deferred, not skipped.
- **Suggested fix:** Run `cargo test --test moe_swap -- --ignored` against Mantle RPC once
  (ideally wired into an opt-in CI job with a funded/rate-limited endpoint) to confirm the
  simulation matches on-chain within tolerance, then record the result here.

### DI-5 — Remaining dead-code warnings in the Moe module
- **Severity:** Low (nit; warnings only, no behavior impact)
- **Source:** WHI-505, PR #6 review (round 2)
- **Where:** `src/amms/moe/mod.rs` and its test helpers — unused methods
  `total_fee` / `protocol_fee_amount` / `needs_reference_update`; never-read fields
  `fee_paid` / `protocol_fee`; unused `U256_ONE`; and a stray unused import in
  `tests/moe_swap.rs`.
- **What:** `cargo build`/`test` emit a batch of `dead_code`/`unused` warnings from the Moe
  module. WHI-508 removed the confirmed-dead `calc_*` helpers; the remaining warnings
  pre-date and are orthogonal to WHI-505's target repair.
- **Why deferred:** The remaining methods and fields may be adopted by upcoming Moe fee
  work, so pruning them is still deferred.
- **Suggested fix:** Drop the remaining dead fields/const and remove the unused import
  after the Moe fee design is settled, or wire the retained methods into that fee path.

### DI-6 — Moe principal AC1 relies on WHI-501 forge suite (no Moe-service → ABI replay in-diff)
- **Severity:** Low (gate already enforced on-chain; coverage lives in another PR)
- **Source:** WHI-503, PR #8 review
- **Where:** `contracts/executor/test/ArbitrageExecutor.t.sol` (e.g.
  `testFuzz_positive_min_profit_on_breakeven_reverts`); Moe planner unit tests in
  `src/execution/principal.rs`
- **What:** Acceptance criterion #1 asks for a test/replay where final WMNT delta below
  explicit `minProfit` reverts the M0-8 balance gate. PR #8 unit-tests the off-chain
  planner; the atomic on-chain revert is covered by the WHI-501 forge suite, not by a
  Moe-service calldata replay in this diff.
- **Why deferred:** Hardened executor + forge suite already landed under WHI-501; duplicating
  that gate test in the Rust service layer adds little until M2-7 Sepolia E2E.
- **Suggested fix:** M2-7 E2E gate should assert a below-`minProfit` Moe (or multi-venue)
  request reverts with `InsufficientProfit`.

### DI-7 — Principal gas check uses static `GasConfig`, not live basefee
- **Severity:** Medium (faithful to current discovery filter; not true “current gas”)
- **Source:** WHI-503, PR #8 review
- **Where:** `attempt_execution` in `moe_monitor_executor_service.rs`;
  `GasConfig::default().calculate_gas_cost`
- **What:** Spec wording “clears current gas” is implemented with the same static Mantle
  estimate used at discovery time, not block basefee / measured profiles.
- **Why deferred:** Matches discovery; live fee context is the M0-2 / WHI-502 / WHI-546 gas
  profile track, not M0-3 scope.
- **Suggested fix:** After measured profiles land, pass `BlockFeeContext` into
  `plan_resized_execution` and size gas cost from the same plan used at send.

### DI-8 — ProtocolCoverage is a placeholder fingerprint (not fail-closed coverage)
- **Severity:** Medium (coverage completeness not enforced at snapshot publish)
- **Source:** WHI-510, PR #9 review (Opus)
- **Where:** `src/state_space/snapshot/types.rs` (`ProtocolCoverage`); publish sites in
  `StateSpaceBuilder::sync` / `StateSpaceManager::subscribe`
- **What:** Snapshots always attach `ProtocolCoverage::default()` (empty fingerprint).
  Identity/header/hash pinning is enforced; V3 word/tick and Moe queried-range coverage
  are not yet validated before `Ready`. Discovery now publishes Ready immediately
  with its canonical discovery tip, so `allows_execution()` can be true at cold start
  with empty coverage — the exposure window starts earlier than “first WS head only”.
- **Why deferred:** Explicitly owned by WHI-512 (V3 tick coverage) and WHI-513
  (MoeSnapshot coverage). M1-1 only reserves the field so the snapshot identity contract
  stays stable for downstream consumers.
- **Suggested fix:** Populate coverage during V3/Moe assembly and refuse `publish` when
  required ranges are incomplete (`IncompleteState` / identity halt).

### DI-9 — No integration test drives the live `subscribe` stream
- **Severity:** Low (unit coverage exists; end-to-end path untested in CI)
- **Source:** WHI-510, PR #9 review (Opus)
- **Where:** `StateSpaceManager::subscribe` (`src/state_space/mod.rs`)
- **What:** Continuity, pin, publisher, and `apply_logs_atomically` are unit-tested in
  isolation. There is no offline mock-provider test that runs the full subscribe
  assemble → publish / fail_read loop without a live WS RPC.
- **Why deferred:** Requires a mock `Provider`/`subscribe_blocks` harness not yet in
  the crate; existing live-RPC tests remain `TEST_RPC_WS_URL`-gated.
- **Suggested fix:** Add a mock block/log stream provider and assert Ready/Halted
  transitions without network (good companion to M1-7 gap recovery tests).

### DI-13 — Cold-start gap backfill can delay readiness
- **Severity:** Medium (startup latency / RPC pressure; correctness is fail-closed)
- **Source:** WHI-516, PR #17 review (Opus)
- **Where:** `StateSpaceBuilder::sync` and `StateSpaceManager::subscribe`
- **What:** Discovery publishes its canonical tip so the first WS head can recover every
  block missed between discovery and subscription. A large startup gap therefore performs
  sequential header and log reads before the first new snapshot becomes Ready.
- **Why deferred:** Removing the backfill would silently lose updates. Optimizing it needs
  a batch-header or range-replay design with the same per-block identity guarantees.
- **Suggested fix:** Add a bounded/batched cold-start replay path with measured RPC limits,
  while preserving atomic publication and canonical branch verification.

### DI-10 — Mantle state-fork deep tick/bin + multi-hop gas qualification (WHI-546)
- **Severity:** High (production gas limits for deep V3/Moe routes remain Unsupported)
- **Source:** WHI-546, PR #11 review (Opus code-review round)
- **Where:** `config/gas_profiles/`; `contracts/executor/test/GasProfileMeasure.t.sol`;
  `src/execution/gas_profile.rs` crossing-bucket evidence
- **What:** Approved profiles are hash-pinned Foundry EVM `gasleft()` measurements of the
  WHI-501 optimized runtime against **mock pools** at a fixed amount grid. That is real
  bytecode measurement, not an arithmetic ramp, but it does **not** exercise live Mantle
  pool storage, V3 tick traversal, or Moe bin crossing. Deep tick/bin buckets and multi-hop
  classes are therefore explicit **Unsupported** with recorded gap text — multi-modality is
  neither measured nor disproved on Mantle state. Fee-window `start_block_hash` for
  97,158,262 is an analysis-window identifier (padded block number), not a live eth_getBlock
  hash fetch of that historical tip.
- **Why deferred:** State-fork suite needs Mantle RPC, pool fixtures, and a dedicated
  measurement campaign; out of scope for the schema/generator PR once honest fail-closed
  Unsupported coverage is in place. Runtime still must fail closed (WHI-502) for Unsupported.
- **Suggested fix:** Add anvil/forge Mantle state-fork harness that re-executes WHI-501
  calldata against real pools with recorded tick/bin crossings; replace Unsupported deep
  buckets with Approved profiles only when holdout/fork-replay clears the derived limit;
  pin true start/end block hashes from the measurement window headers.

### DI-12 — Define a meaningful block gas-limit reserve policy
- **Severity:** Low (policy/spec gap; current strict-bound check remains safe)
- **Source:** WHI-502, PR #14 review (Opus)
- **Where:** `src/execution/types.rs::ExecutorConfig::block_gas_limit_reserve`
- **What:** The default reserve is `1`, which enforces `gas_limit < block_gas_limit` but does not
  document or guarantee operational headroom beyond that one-unit strictness.
- **Why deferred:** WHI-502 has no evidence-backed chain-specific reserve value or percentage to
  adopt; choosing an arbitrary larger constant would change policy without a measured basis.
- **Suggested fix:** Define and document a chain-specific or percentage-based reserve policy,
  then add boundary tests and update the runtime profile qualification rule accordingly.

### DI-11 — V3 quote cache data clump in the surviving Agni V3 service
- **Severity:** Low (maintainability; no current correctness impact)
- **Source:** WHI-511, PR #12 review (Opus); narrowed by WHI-726 (legacy
  `v3_monitor_executor_service.rs` deleted); schema counts updated by WHI-729
- **Where:** `examples/protocols/agni/v3_monitor_executor_service_1559.rs` —
  local `GrossCandidate` / `PositiveCandidate` copies, the quote-cache helpers/tests,
  and (per WHI-628/DI-19) the `mod tests` fixture builders `pool()` / `swap_log()`.
  Canonical types now live in `src/service/shadow_row.rs` /
  `src/service/protocol.rs` (`Candidate`).
- **What:** The legacy EIP-1559 service still owns in-file copies of the candidate
  clump and two-tier quote cache. Canonical field counts after WHI-729 unification:
  **14** fields on `GrossCandidate` (no `net_profit`) and **15** on
  `PositiveCandidate` / `service::Candidate` (adds `net_profit`; v2 gains `roi`,
  moe gains `amounts_out`/`expected_states`). CSV positive/best-path logs stay at
  **9** columns (`POSITIVE_PATH_LOG_HEADERS`); the old v2 8-field
  `OpportunityCsvLogger` header set is retired from `service::shadow_row`.
  Adopting the two-tier gross-quote cache for Moe remains deferred to **M4-2**.
- **Why deferred:** The review identified a real maintenance smell in the legacy
  example, but not a runtime defect on the merged binary path. Pointing the
  example at the shared types (or deleting the example under WHI-534 / M3-9)
  would broaden a follow-up beyond schema unification.
- **Suggested fix:** Point the surviving EIP-1559 example at
  `service::GrossCandidate` / `service::Candidate` (or delete the example under
  WHI-534). Extract `pool()` / `swap_log()` test fixtures into a shared
  test-support module if a second V3 entrypoint reappears.

### DI-15 — `signing-test-util` feature does not exclude examples
- **Severity:** Medium (trust-boundary claim is weaker than documented; no production
  code path affected today)
- **Source:** WHI-552, PR review
- **Where:** `Cargo.toml` (`[features] signing-test-util`, self dev-dependency
  `amms = { path = ".", features = ["signing-test-util"] }`);
  `src/signing/mod.rs::verify_with_paths`
- **What:** The self dev-dependency trick enables `signing-test-util` for *all*
  dev-dependency consumers, and Cargo builds examples with dev-dependencies. So
  `signing::verify_with_paths` — the seam that lets a caller choose its own
  `allowed_signers`/`revoked_keys` trust roots — is callable from every entrypoint under
  `examples/`, which is where all this crate's runnable programs live (there is no
  binary target). That contradicts the `Cargo.toml` comment claiming the gate keeps the
  path-injection capability away from anything but `tests/signing.rs`.
- **Why deferred:** The correct fixes are build-structure changes, not local edits:
  either move the signing integration tests into `src/signing/` as `#[cfg(test)]`
  modules and make the seam `pub(crate)`/`#[cfg(test)]` (dropping the feature and the
  self dev-dependency entirely), or split the signing module into its own workspace
  crate so examples are not dev-dependency consumers. Both reshape the crate layout and
  would balloon this PR, which is the first of three dependent merges.
- **Suggested fix:** Drop the `signing-test-util` feature + self dev-dependency, move
  `tests/signing.rs` into `src/signing/tests.rs` under `#[cfg(test)]`, and demote
  `verify_with_paths` to `#[cfg(test)] pub(crate)`. Note `pub(super)` was already
  applied to `ssh::verify_detached` in this PR, so the remaining exposure is
  `verify_with_paths` alone.

### DI-16 — Signing trust-root paths are baked to the build machine's absolute path
- **Severity:** High (any deployment outside the build tree fails every `verify()`)
- **Source:** WHI-552, PR review
- **Where:** `src/signing/config.rs:7,15` — `allowed_signers_path()` /
  `revoked_keys_path()`, both `PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(...)`
- **What:** `env!("CARGO_MANIFEST_DIR")` is resolved at *compile* time, so the
  production trust-root paths are the absolute path of whatever directory the binary was
  built in (currently, for this branch, a path inside `.claude/worktrees/`). A binary
  run from any other tree — a container, a release artifact, an operator's machine —
  will point at a non-existent `config/signers/allowed_signers`, and `ssh-keygen -Y
  verify` will fail with an opaque `SshVerifyFailed { stderr }` rather than a clear
  "trust roots not found" error. Note this fails *closed*, so it is a availability /
  diagnosability problem, not a bypass.
- **Why deferred:** Fixing it requires deciding the deployment story (embed the
  allowed-signers/revoked-keys contents via `include_str!` and write them to a temp file
  per verification? resolve relative to the executable? a required, validated env var
  with a code-constant default? a build-time-embedded fallback plus operator override?),
  and each option changes the "code-constant trust root" property the spec asks for in
  a different way. That decision belongs with the first real consumer
  (WHI-521 / WHI-554), which will define how artifacts and signer config ship together.
- **Suggested fix:** Pick a deployment model, then (a) resolve trust roots through it,
  and (b) add an explicit existence/readability precheck in `verify_impl` that returns a
  distinct typed error (e.g. `SigningError::TrustRootUnavailable { path }`) instead of
  letting a missing file surface as a generic ssh-keygen stderr string.
- **Partial mitigation (WHI-554):** `examples/shadow_gate_plan.rs`,
  `examples/shadow_report.rs`, and `examples/shadow_decision.rs` each add a local
  `require_trust_roots()` precheck before their `sign`/`verify` paths, so a missing
  `config/signers/{allowed_signers,revoked_keys}` fails with an operator-facing message
  naming the expected path instead of an opaque ssh-keygen stderr string. This is
  example-local (not in `src/signing/`) and does not change where the paths resolve to
  — the underlying deployment-model question above is still open.

### DI-31 — `shadow/mod.rs` doesn't re-export ledger row types, forcing wire-mirror duplication
- **Severity:** Low (nit/consistency — the duplication is mechanically verified by
  serde, not a correctness bug today)
- **Source:** WHI-554, design phase
- **Where:** `src/execution/shadow/mod.rs` (`mod ledger;`, private) and
  `src/execution/shadow/ledger.rs` (`LedgerRunHeader`, `LedgerRow`,
  `LedgerCandidateRow`, `LedgerContextRow`, `LedgerProvenanceRow`, all
  `pub(crate)`)
- **What:** `shadow_report.rs` (a sibling of `shadow`, not a descendant) can't name
  these types at all — `ledger` is a private submodule of `shadow`, so its `pub(crate)`
  items aren't reachable outside `shadow` and its descendants. `shadow_report.rs`
  therefore defines its own local `Wire*` mirror types matching `ledger.rs`'s field
  names and serde tags by hand (reusing the two genuinely-`pub` types,
  `shadow::ProfitBasis` and `shadow::{PoolProvenanceOutcome, Create2Proof}`, directly).
- **Why deferred:** The issue that introduced this (WHI-554) was explicitly told not to
  touch `src/execution/shadow/` (WHI-549's module). The real fix — making `ledger.rs`'s
  row types `pub` and re-exporting them from `shadow/mod.rs` — is a one-line change but
  belongs to a change that owns that module.
- **Suggested fix:** In a WHI-549-scoped change, make the ledger row types `pub` and add
  them to `shadow/mod.rs`'s `pub use ledger::*;`, then delete `shadow_report.rs`'s
  `Wire*` mirrors in favor of the real types.
- **Renumber note (WHI-741):** previously duplicated the DI-27 id used by the
  continuous `--watch` deferral; renumbered to DI-31 when that entry was resolved.

### DI-28 — `SigningFixture`/`generate_ed25519_keypair`/`build_fixture` test helpers are duplicated across three compilation units
- **Severity:** Low (nit/consistency — test-only code, mechanically identical, no
  production risk)
- **Source:** WHI-554 PR review (round 1)
- **Where:** `src/execution/shadow_gate_plan.rs` (`#[cfg(test)] mod tests`),
  `src/execution/shadow_decision.rs` (`#[cfg(test)] mod tests`), and
  `tests/shadow_evidence.rs` (as `KeyFixture`, same shape, different name)
- **What:** All three independently define a tempdir-backed fixture struct holding an
  ed25519 keypair path plus hand-written `allowed_signers`/`revoked_keys` files, a
  `generate_ed25519_keypair` helper that spawns `ssh-keygen -t ed25519`, and a
  `build_fixture(principal, namespace)` constructor. The library-side copies
  (`shadow_gate_plan.rs`, `shadow_decision.rs`) are unit-test modules inside the same
  crate and could in principle share a `#[cfg(test)]` helper module; the integration
  test (`tests/shadow_evidence.rs`) is a separate compilation unit (its own test binary)
  and cannot see `#[cfg(test)]` items in `src/` at all, so it would need a `pub(crate)`
  seam gated behind a feature (mirroring the existing `signing-test-util` feature used
  for `verify_with_paths` — see DI-15) rather than a plain `#[cfg(test)]` module.
- **Why deferred:** The `to_hex0x`/digest-hashing duplication in this same review round
  was fixed directly (real library code, one obvious home in
  `shadow_gate_plan::digest_bytes`). This one is different: fixing it properly means
  either adding a new Cargo feature purely to expose test-fixture-building code across
  crate/binary boundaries, or accepting three ~50-line copies of tempdir/ssh-keygen
  scaffolding. Given it's test-only and each copy is mechanically identical (drift would
  be caught immediately by a failing test, not a silent bug), introducing a new feature
  flag for this felt like disproportionate machinery for the WHI-554 scope.
- **Suggested fix:** If a future change already needs a shared test-support seam across
  `src/` unit tests and `tests/` integration tests (e.g. extending DI-15's
  `signing-test-util` feature), fold `SigningFixture`/`generate_ed25519_keypair`/
  `build_fixture` into it and delete all three local copies at once.

### DI-14 — Legacy service discovery still uses the pre-WHI-502 gas schedule
- **Severity:** Medium (gas-model correctness; production sends remain fail-closed)
- **Source:** WHI-514, PR #19 follow-up review
- **Where:** `examples/protocols/legacy_service_support.rs`, consumed by the three
  `*_monitor_executor_service` examples
- **What:** The three migrated example services use a shared compatibility helper for
  discovery-time profitability and gas-limit calculations. Its hop schedule is the
  pre-WHI-502 legacy model and is not the measured `RuntimeGasProfile` used by the
  current library executor.
- **Why deferred:** WHI-514 is limited to snapshot-bound input sizing. The services
  remain fail-closed at the M1 production gate, and replacing discovery economics with
  measured profiles requires route-key and V3/Moe crossing-bucket wiring that belongs
  to the execution/profile migration rather than this cap fix.
- **Suggested fix:** Load the validated `RuntimeGasProfile` at service startup and use
  route-local `GasQuote` values for candidate economics and transaction gas limits;
  fail closed for unsupported route or crossing-bucket classes.

### DI-13 — Concentrated-liquidity coverage and sync test logic is duplicated
- **Severity:** Low (maintainability; no current correctness impact)
- **Source:** WHI-512, PR #16 review (Opus)
- **Where:** `src/amms/agni/mod.rs` and `src/amms/uniswap_v3/mod.rs` —
  `ensure_tick_bitmap_coverage`, bitmap sync chunking, and concentrated-liquidity
  coverage tests
- **What:** The Agni and Uniswap V3 adapters contain near-identical coverage checks,
  sync chunking logic, and regression fixtures.
- **Why deferred:** The duplication is a maintainability smell rather than a runtime
  defect. Extracting shared helpers while closing the tick-coverage correctness gap
  would broaden WHI-512 and make protocol-specific sync behavior harder to audit.
- **Suggested fix:** Extract a shared concentrated-liquidity bitmap coverage helper and
  common fixture utilities after both adapters' sync contracts stabilize, retaining
  protocol-specific tests for their distinct batch request paths.

### DI-17 — `WHI501_EXECUTOR_CODEHASH` no longer matches the regenerated executor template
- **Severity:** Medium (provenance clarity; no correctness impact — the value it's
  actually checked against, the frozen gas-profile artifact, is unaffected)
- **Source:** WHI-551 implementation / code review
- **Where:** `src/execution/gas_profile.rs` (`WHI501_EXECUTOR_CODEHASH`),
  `contracts/executor/artifacts/`
- **What:** WHI-551 regenerated `contracts/executor/artifacts/` (`--skip test`, plus
  `ast`/`storageLayout` output). Rebuilding `ArbitrageExecutor.sol` — completely
  unchanged since WHI-501 — with today's pinned toolchain (solc 0.8.26, forge 1.7.1)
  produces a **different** template hash (`0x50f51b77…`, 10156 bytes) than the one
  `WHI501_EXECUTOR_CODEHASH` still pins (`0x8cbcdb37…`, 10211 bytes). Confirmed this is
  pure toolchain/environment drift since WHI-501/WHI-546, not caused by `--skip test`:
  rebuilding with the *original* `foundry.toml` (no `--skip test`, no `ast`/
  `extra_output`) reproduces the same new `0x50f51b77…` hash.
  `WHI501_EXECUTOR_CODEHASH` is deliberately left unchanged because it is a frozen pin
  for `config/gas_profiles/mantle_mainnet_v1.json`'s `executor_code_hash` field (that
  gas-profile data was measured against the old build and regenerating it is WHI-557,
  out of scope here) — `gas_runtime_tests.rs`'s
  `runtime_profile_returns_the_approved_quote_for_a_pinned_route` already asserts that
  pairing stays consistent. What's newly true is that
  `contracts/executor/artifacts/ArbitrageExecutor.codehash.txt` (the committed template
  evidence WHI-551's `runtime_identity.rs` derives `template_hash` from) and
  `WHI501_EXECUTOR_CODEHASH` now name two different builds, with no automated check
  linking (or distinguishing) them.
- **Why deferred:** Reconciling them means either re-running the mainnet gas
  qualification against the newly-rebuilt template (WHI-557's job) or pinning the old
  toolchain byte-for-byte (root cause not fully diagnosed — solc claims byte-determinism
  per version, so this may point at a subtler drift, e.g. a solc patch republish).
  Out of scope for a runtime-identity derivation/verification API.
- **Suggested fix:** When WHI-557 requalifies the mainnet gas profile on a canonical
  fork, regenerate `config/gas_profiles/mantle_mainnet_v1.json` against the current
  template and retire `WHI501_EXECUTOR_CODEHASH` in favor of a single source of truth
  (e.g. `config/executor_identity.json`'s `template_hash`).

### DI-29 — `digest_bytes` lives in `shadow_gate_plan.rs`, the "more primitive" `shadow_thresholds.rs` imports it upward
- **Severity:** Low (nit/consistency — no correctness impact, both modules are siblings
  under `src/execution/` with no cyclic dependency)
- **Source:** WHI-554 PR review (round 2)
- **Where:** `src/execution/shadow_gate_plan.rs` (`pub fn digest_bytes`),
  `src/execution/shadow_thresholds.rs` (imports it), `src/execution/shadow_report.rs`
  and `src/execution/shadow_decision.rs` (also import it)
- **What:** `digest_bytes` (a `keccak256`-then-hex-encode helper) is defined in
  `shadow_gate_plan.rs`, but `shadow_thresholds.rs`'s own module doc-comment describes
  itself as intentionally more primitive than the gate-plan/report/decision layer ("this
  module never depends on anything `pub(crate)` inside `shadow`"), and conceptually the
  digest helper is lower-level than a gate-plan-specific concern — `shadow_thresholds`
  importing *from* `shadow_gate_plan` reads backwards. Note this helper is also **not**
  the crate's only implementation of this pattern: `gas_profile::bytes_to_hex` and
  `breaker::coordinator::encode_hex` are pre-existing, near-duplicate
  `to_hex0x(keccak256(...))`-shaped reimplementations elsewhere in the crate (round-3
  review finding — `shadow_gate_plan.rs`'s doc comment previously overclaimed this was
  "the single shared implementation"; corrected in round 3).
- **Why deferred:** This is a pure module-organization nit at this point in the review
  loop (round 2 of the bounded 3-round loop) with four call sites already depending on
  the current home (`shadow_thresholds.rs`, `shadow_report.rs`, `shadow_decision.rs`,
  plus `tests/shadow_evidence.rs`). Moving it to a new shared location (e.g. a small
  `src/execution/shadow_digest.rs`) this late risks touching every one of those files
  again for a purely cosmetic win, with no behavior change and no bug it fixes.
- **Suggested fix:** If a future shadow-evidence change already needs to touch all four
  call sites, extract `digest_bytes`/`digest_file_bytes`/`to_hex0x` into their own
  small module (or promote them via the DI-31 `shadow/mod.rs` re-export fix, if that
  lands first) and update all imports in one pass. Consider consolidating with
  `gas_profile::bytes_to_hex`/`breaker::coordinator::encode_hex` at the same time,
  since all three are the same hex-encoding shape.

### DI-30 — `require_trust_roots()`/`cmd_sign`/`ScopeArgs` shape duplicated across three example CLIs
- **Severity:** Low (nit/consistency — example-binary code, not library code; mechanically
  identical across copies, no production risk)
- **Source:** WHI-554 PR review (round 2)
- **Where:** `examples/shadow_gate_plan.rs`, `examples/shadow_report.rs`,
  `examples/shadow_decision.rs` (each defines its own `require_trust_roots()` and a
  `cmd_sign` with the same overwrite-guard/`sign_envelope`/rewrite-canonical-file shape)
- **What:** All three example binaries independently define a `require_trust_roots()`
  that resolves `signing::config::allowed_signers_path()`/`revoked_keys_path()` and
  checks both exist (the DI-16 partial mitigation), and a `cmd_sign` that guards against
  overwriting an existing `--sig-out` without `--force`, calls `signing::sign_envelope`,
  and rewrites the input file to its canonical form. The three copies are structurally
  identical modulo the payload type (`GatePlanPayload` / `ShadowReport` /
  `DecisionPayload`) and domain constant.
  The round-3 Opus escalation pass added a **third** item to this list: a
  `#[derive(clap::Args)] struct ScopeArgs { chain_id, git_commit, services }` plus
  `into_scope() -> Result<ShadowGateScope>`, now defined once per example CLI. That pass
  fixed the *worse* smell it replaced — the same `(chain_id, git_commit, services)` triple
  had been re-declared across five subcommand variants, re-destructured in five `run()`
  arms, threaded through five function signatures as three separate parameters, and
  hand-assembled into a `ShadowGateScope` at five call sites (Fowler's Data Clumps, with
  the bundling type, `ShadowGateScope`, already existing in the library). Collapsing that
  to one `#[command(flatten)]` per subcommand also retired two
  `#[allow(clippy::too_many_arguments)]` attributes. `ScopeArgs` cannot live in the
  library next to `ShadowGateScope`: `clap` is a **dev-dependency only** (`Cargo.toml`
  line 89), so a `clap::Args` derive in `src/` would mean adding a CLI arg parser to the
  library's dependency graph for every downstream consumer.
- **Why deferred:** `autoexamples = false` means every example is its own standalone
  binary crate, but this repo does have precedent for factoring shared logic into a
  `path`-included support module across multiple examples — round-3 review corrected an
  earlier version of this entry that claimed no such precedent existed:
  `examples/protocols/intent_service_support.rs` and
  `examples/protocols/legacy_service_support.rs` are both shared via
  `#[path = "..."] mod ...;` from `examples/protocols/agni/v2_monitor_executor_service.rs`,
  `examples/protocols/agni/v3_monitor_executor_service_1559.rs`,
  `examples/protocols/moe/moe_monitor_executor_service.rs`, and
  `examples/e2e/e2e_run.rs`. So the deferral here rests only on scale, not on precedent:
  those support modules are shared by four *existing* monitor/executor services, whereas
  this PR's `require_trust_roots()`/`cmd_sign` duplication is three *new* CLIs introduced
  in this same PR, each copy under ~40 lines and structurally simple enough that drift
  would surface immediately as a compile or test failure, not a silent bug. Factoring out
  a shared module for three same-PR call sites with no independent history is premature
  relative to the `intent_service_support.rs`/`legacy_service_support.rs` precedent, which
  was extracted only once real duplication had accumulated across separately-landed
  services.
- **Suggested fix:** If a fourth shadow-evidence-style example CLI is added later,
  factor `require_trust_roots()`, the sign-overwrite-guard logic, and `ScopeArgs`/
  `into_scope()` into a small `examples/shadow_cli_support.rs`, `path`-included the same
  way `intent_service_support.rs`/`legacy_service_support.rs` are today, shared by all of
  them at that point. (`autoexamples = false` means such a support file is not itself
  built as an example target, so no `[[example]]` block is needed for it.)

## Design notes (intentional — do not "fix" without cause)

### DN-4 — V3/Agni tick-cross field stores local consumption, not QuoterV2
- **Source:** WHI-522, PR #23 review (Opus re-review of `54f8cb6`)
- **Where:** `tests/differential.rs` (`SwapCase::expected_local_ticks_crossed`);
  `tests/fixtures/differential/{uniswap_v3_fusionx_wmnt_weth_2500,agni_usde_wmnt_2500}.json`
- **Note:** Capture still uses QuoterV2 as the independent amount-out / sqrt-price oracle.
  The optional tick-cross count is deliberately the **local** initialized-tick consumption
  count for that same swap path, not QuoterV2's `initializedTicksCrossed`. On the UniV3
  fixture, quoter reports 2 while the local path consumes 1; amount-out still matches
  exactly, so the model is right and the quoter counter over-counts a reached-but-not-
  consumed boundary. Offline asserts recompute the local count as a regression guard for
  fail-closed tick-record consumption; they do not re-assert the quoter counter.
- **Do not "fix" by:** restoring the quoter counter into this field, or renaming it back
  to imply quoter semantics. If an independent quoter-counter check is needed later, add a
  separate optional field.


### DN-1 — `meta.snapshot_block` is deliberately not pinned to a constant
- **Source:** WHI-507, PR #7
- **Where:** `MoePoolList::validate_offline` (`src/amms/moe/pool_list.rs`)
- **Note:** Only `factory_creation_block` is pinned to `CANONICAL_MOE_FACTORY_CREATION_BLOCK`
  (the factory is deployed once, so it is a true constant). `snapshot_block` is intentionally
  left flexible — a regenerated list may legitimately snapshot at a newer block — and is only
  constrained to be `>= max(creation_block)` across rows. Do not add an equality check against
  `COMMITTED_MOE_POOL_LIST_SNAPSHOT_BLOCK`; it would break legitimate regeneration.

### DN-2 — Moe pool list is a single-protocol M0 stopgap
- **Source:** WHI-507 (spec §A4); blocks/superseded-by M3-10
- **Note:** `data/poolLists_moe.csv` + `.meta.json` use a Moe-only schema on purpose.
  Cross-protocol pool-list schema unification is explicitly deferred to the **M3 config
  consolidation**, not chosen ad hoc here. DI-3 (env naming) is a natural companion to that
  work.

### DN-5 — `build_info_digest` is not a digest of Foundry's `out/build-info/*.json`
- **Source:** WHI-551 implementation / code review (Opus + external GPT review)
- **Where:** `src/execution/runtime_identity.rs` (`build_info_digest` computation in
  `resolve_immutable_plan`); `contracts/executor/scripts/export_artifacts.sh`
- **Note:** WHI-551's spec text lists "build-info, AST, metadata, and storage layout"
  as evidence to commit with digests. Foundry's own `out/build-info/*.json` was
  measured at **18.7 MB** for this project (it inlines every forge-std source file)
  and its `id`/`input.sources` keys are absolute-checkout-path-dependent — committing
  it raw is impractical, and hashing it raw would defeat the checkout-path
  reproducibility this same issue requires. `build_info_digest` is deliberately defined
  instead as `keccak256({solc_long_version, language, ast})` — the AST already fully
  represents "what was compiled" (parsed source structure), and pairing it with
  compiler-identity strings covers the "build info" evidence category in spirit without
  the 18.7 MB file. `AST` and `storage layout` are separately committed in full inside
  `contracts/executor/artifacts/ArbitrageExecutor.full.json`; `metadata` likewise.
- **Do not "fix" by:** committing the raw Foundry build-info file, or hashing it
  as-is (non-reproducible across checkouts). If a stricter, literal reading of the
  acceptance criterion is wanted, the alternative is a *normalized* build-info
  artifact (strip absolute paths from `source_id_to_path`/`input.sources`, drop
  `input.sources` file contents already tracked in git) — a real chunk of new work,
  not attempted here.

---

## Resolved

- **DI-33 — WHI-951 static eligibility omits live balance** — balance half resolved
  by WHI-950 / G-3. Strategy A pins one hash-pinned `balanceOf` per head
  (`SendRuntime::executor_wmnt_balance_bound` → `SnapshotBoundBalance`), feeds
  `classify_with_send_runtime(..., Some(balance))`, and reuses the pin in
  `submit_opportunity`. Inventory precondition is enforced before discovery
  (`CapitalDomain::BlockUnsendable` → empty candidates). Freshness/coverage still
  fail closed only at snapshot publish + send identity (not separate static
  eligibility reasons); tip lag remains the attempt budget — that residual is
  intentional, not re-opened here.

- **DI-27 — Continuous multi-protocol `--watch` block loop not enabled in WHI-728**
  — resolved by WHI-741. `src/service/block_loop.rs` now owns
  `run_multi_protocol_watch_loop` / `process_observed_head`: one WS
  `subscribe_blocks` stream drives all selected protocols against a shared
  `StateSpace` (StateChangeCache shallow reorg path + SnapshotPublisher halt for
  deep forks/gaps; full deep-reorg recovery remains WHI-533). `src/bin/bot.rs
  --watch` replaces the fail-closed bail with this loop, handles SIGINT/SIGTERM
  for clean ledger flush, and keeps `--once` as the one-shot path. The duplicate
  DI-27 id on the shadow re-export entry was renumbered to **DI-31**.

- **DI-19 — Pre-existing live-pool-state test failures in the two V3 monitor services**
  — resolved by WHI-628. Root cause: WHI-512 added `AgniPool::tick_bitmap_coverage` and
  a hard `ensure_tick_bitmap_coverage` gate at the top of the swap-step loop, but the
  hand-built `pool()` test fixture in both `v3_monitor_executor_service.rs` and
  `v3_monitor_executor_service_1559.rs` (written earlier, in WHI-511) never populated
  it, so every simulated swap on a fixture pool unconditionally returned
  `AMMError::IncompleteState` regardless of liquidity/price/amount. Fixed by extending
  `pool.tick_bitmap_coverage` with `-10i16..=10i16` in both fixtures — mirroring the
  `test_pool()` helper's own convention in `src/amms/agni/mod.rs` — which covers the
  bitmap word around tick 0 that every test's mocked pools and swap logs operate on.
  All 3 listed tests pass in both files; `cargo test --locked --all-targets` is green
  with no pre-existing red. The fix duplicates the same one-line change into both
  files' independent `pool()` copies rather than introducing a shared fixture builder;
  that duplication is pre-existing accepted debt already tracked as **DI-11**, not new
  scope from this fix.

- **DN-3 — Discovery Ready does not seed `last_tip`** — resolved by WHI-516.
  Source: WHI-510, PR #9 review rounds 2–3 (Opus). Cold-start discovery now seeds
  `last_tip` and routes a first WS head gap through canonical header and hash-pinned log
  backfill. `demote_ready_to_baseline` still does not rewrite the continuity tip after a
  failed assembly.

- **DI-25 — Shadow-mode CREATE2 pool-address verification is unimplemented for
  V2/V3/Agni pools** — resolved by WHI-549. The full-rework pass added a committed
  approved-registration config (`approved_pools.rs`, mirroring `moe_allowlist.rs`'s
  load/digest pair) and wired `check_pool_provenance` to call
  `create2::expected_pool_address` for UniswapV2/V3/Agni pools, returning genuine
  `Verified`/`Rejected` outcomes. `PoolProvenanceOutcome::Create2CheckSkipped` and its
  now-dead test were removed entirely — every pool type gets a real provenance check
  today, none fall back to a skip. Verified end-to-end by `tests/shadow_runtime.rs`,
  which asserts a real CREATE2 proof object (`factory`, `init_code_hash`, `protocol`,
  `salt`) nested under the ledger's `verified` outcome key for a UniswapV2 pool.
