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
    path — restart self-heal is not active in the four monitor services.
  - **Pause→pending cancellation (Spec-4 / PR headline):**
    `begin_pause_cancel_sweep` purges a `LatestWinsSlot` and returns cancel targets; it
    does **not** drive cancel prepare/sign/broadcast. No service loop invokes it —
    paused auto-withdraw of pending nonces is not live.

  Still incomplete vs Revision 5 fixtures:
  (1) coordinator `fsync` may run while the SM mutex is held — should queue off-lock;
  (2) pause→pending-cancel driver not auto-wired into the four services (see above);
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

### DI-14 — Legacy service discovery still uses the pre-WHI-502 gas schedule
- **Severity:** Medium (gas-model correctness; production sends remain fail-closed)
- **Source:** WHI-514, PR #19 follow-up review
- **Where:** `examples/protocols/legacy_service_support.rs`, consumed by the four
  `*_monitor_executor_service` examples
- **What:** The four migrated example services use a shared compatibility helper for
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

### DI-15 — `WHI501_EXECUTOR_CODEHASH` no longer matches the regenerated executor template
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

---

## Resolved

- **DN-3 — Discovery Ready does not seed `last_tip`** — resolved by WHI-516.
  Source: WHI-510, PR #9 review rounds 2–3 (Opus). Cold-start discovery now seeds
  `last_tip` and routes a first WS head gap through canonical header and hash-pinned log
  backfill. `demote_ready_to_baseline` still does not rewrite the continuity tip after a
  failed assembly.
