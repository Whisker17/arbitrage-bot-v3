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

### DI-1 — Moe pool-list on-chain validation + init are not multicall-batched
- **Severity:** Medium (startup latency / RPC pressure; no correctness impact)
- **Source:** WHI-507, PR #7 review (round 3)
- **Where:** `src/amms/moe/pool_list.rs` (`validate_on_chain` / `validate_entry_on_chain`);
  `initialize_moe_pools` in `examples/protocols/moe/{moe_monitor_executor_service,monitor_moe_lb_arbitrage}.rs`
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
  `examples/protocols/moe/monitor_moe_lb_arbitrage.rs` (and sibling examples)
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

### DI-5 — Dead-code warnings in the Moe module
- **Severity:** Low (nit; warnings only, no behavior impact)
- **Source:** WHI-505, PR #6 review (round 2)
- **Where:** `src/amms/moe/mod.rs` and its test helpers — unused `calc_base_fee` /
  `calc_variable_fee` / `calc_total_fee` / `calc_fee_amount` / `calc_fee_amount_from` /
  `calc_protocol_fee`; unused methods `total_fee` / `protocol_fee_amount` /
  `needs_reference_update`; never-read fields `fee_paid` / `protocol_fee`; unused
  `U256_ONE`; and a stray unused import in `tests/moe_swap.rs`.
- **What:** `cargo build`/`test` emit a batch of `dead_code`/`unused` warnings from the Moe
  module. They pre-date and are orthogonal to WHI-505's target repair.
- **Why deferred:** Out of WHI-505's scope (restore build/test green), and pruning risks
  touching helpers that upcoming Moe fee work may adopt.
- **Suggested fix:** Either wire the `calc_*` helpers into the live fee path or delete them,
  drop the dead fields/const, and remove the unused import — as a standalone cleanup.

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

### DI-11 — V3 quote cache data clump and duplicated service implementation
- **Severity:** Low (maintainability; no current correctness impact)
- **Source:** WHI-511, PR #12 review (Opus)
- **Where:** `examples/protocols/agni/v3_monitor_executor_service.rs` and
  `v3_monitor_executor_service_1559.rs` — `GrossCandidate` / `PositiveCandidate` and
  the quote-cache helpers/tests.
- **What:** Gross and positive candidates carry the same eleven fields and are copied
  field-by-field; the cache and quote-refresh implementation is also duplicated across
  the legacy and EIP-1559 service variants.
- **Why deferred:** The review identified a real maintenance smell, but not a runtime
  defect. WHI-511 requires live-state correctness in both entrypoints; introducing a
  shared quote module or changing candidate ownership would broaden this PR and make
  the execution-specific variants harder to audit.
- **Suggested fix:** Extract a shared V3 quote-cache module and represent the gross
  candidate as the reusable portion of a positive candidate, with focused parity tests
  for both execution variants.

## Design notes (intentional — do not "fix" without cause)

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

### DN-3 — Discovery Ready does not seed `last_tip` (resolved by WHI-516)
- **Source:** WHI-510, PR #9 review rounds 2–3 (Opus)
- **Where:** `SnapshotPublisher::publish_ready_awaiting_head` / `demote_ready_to_baseline`
  / `publish`; `StateSpaceBuilder::sync`
- **Note:** WHI-516 makes cold-start discovery seed `last_tip` and routes a first WS
  head gap through canonical header and hash-pinned log backfill. `demote_ready_to_baseline`
  still does not rewrite the continuity tip after a failed assembly.

---

## Resolved

_None yet. When an open entry is fixed, move it here with the resolving PR/commit._
