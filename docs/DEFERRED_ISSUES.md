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
  crate-private and unreachable from `examples/`, so the four monitor services derive the
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
  or the four services silently diverge from the on-chain encoding.
- **Why deferred:** The right fix is to expose the derivation from `src/execution` (a
  public params/min-out surface) and delete the example copies. That touches the
  production params builder's API and is broader than this review pass.
- **Suggested fix:** Promote `min_amount_out` + `mul_fraction` into a public helper in
  `src/execution` (alongside the already-public `pool_type_byte`) and have
  `intent_service_support.rs` call it, removing the hand-synced copies.

### DI-18 — Four monitor services use `StatusBoundIdentitySource`, not a live `SnapshotPublisher`-backed source
- **Severity:** Medium (identity revalidation is real but snapshot-status-derived, not
  independently sourced; production send gate stays closed so no live-send exposure yet)
- **Source:** WHI-553 implementation / PR review
- **Where:** `examples/protocols/intent_service_support.rs` (`StatusBoundIdentitySource`);
  `src/execution/identity.rs` (`LiveExecutionIdentitySource`);
  `v2_monitor_executor_service.rs`, `v3_monitor_executor_service.rs`,
  `v3_monitor_executor_service_1559.rs`, `moe_monitor_executor_service.rs` (their
  `run_pipeline_head_closed` wiring)
- **What:** WHI-553 wires all four services through `run_pipeline_head_closed` using
  `StatusBoundIdentitySource`, a minimal stand-in that derives validation directly from
  the `SnapshotStatus` already passed into `prepare_pipeline_head`. A full
  `LiveExecutionIdentitySource` backed by a real `SnapshotPublisher` subscription
  (independent of the caller-supplied status, per the original `ExecutionIdentitySource`
  design intent) is not wired into any service yet.
- **Why deferred:** WHI-553's scope is the wallet-free pipeline-head seam and wiring
  itself; the send gate (`production_send_allowed()`) remains false in all four services,
  so no live send currently depends on identity-source independence. Building the
  `SnapshotPublisher`-backed source is a separable follow-up.
- **Suggested fix:** Wire a real `LiveExecutionIdentitySource` from each service's
  existing `SnapshotPublisher`/`StateSpaceManager` subscription before the production
  send gate opens, and swap it in for `StatusBoundIdentitySource` at each
  `run_pipeline_head_closed` call site.
- **Note (WHI-549):** the shadow-mode counterpart, `execution::shadow::ShadowIdentitySource`,
  is implemented and unit-tested (it wraps a `LiveExecutionIdentitySource` for `validate`
  and always fails `acquire_send_lease` closed) but is likewise not wired into any of the
  four services' `run_pipeline_head_closed` calls, for the same reason: it needs the same
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
  `v3_monitor_executor_service_1559.rs` — `GrossCandidate` / `PositiveCandidate`, the
  quote-cache helpers/tests, and (per WHI-628/DI-19) the `mod tests` fixture builders
  `pool()` / `swap_log()`, which are also independently duplicated byte-for-byte.
- **What:** Gross and positive candidates carry the same eleven fields and are copied
  field-by-field; the cache and quote-refresh implementation is also duplicated across
  the legacy and EIP-1559 service variants. The test-only pool/log fixture builders are
  likewise hand-copied between the two files, so a struct-shape change (as happened in
  WHI-512, see DI-19) has to be applied to both independently.
- **Why deferred:** The review identified a real maintenance smell, but not a runtime
  defect. WHI-511 requires live-state correctness in both entrypoints; introducing a
  shared quote module or changing candidate ownership would broaden this PR and make
  the execution-specific variants harder to audit.
- **Suggested fix:** Extract a shared V3 quote-cache module and represent the gross
  candidate as the reusable portion of a positive candidate, with focused parity tests
  for both execution variants. Extract the `pool()` / `swap_log()` test fixtures into a
  shared test-support module alongside that work, so a future `AgniPool`/`AMM` field
  addition only needs updating once.

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

### DI-27 — `shadow/mod.rs` doesn't re-export ledger row types, forcing wire-mirror duplication
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
  small module (or promote them via the DI-27 `shadow/mod.rs` re-export fix, if that
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
  `examples/protocols/agni/v3_monitor_executor_service.rs`,
  `examples/protocols/agni/v3_monitor_executor_service_1559.rs`,
  `examples/protocols/moe/moe_monitor_executor_service.rs`, and
  `examples/e2e/e2e_run.rs`. So the deferral here rests only on scale, not on precedent:
  those support modules are shared by five *existing* monitor/executor services, whereas
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
