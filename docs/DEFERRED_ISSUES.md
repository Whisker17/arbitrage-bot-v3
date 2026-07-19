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
  when the M3 config consolidation lands — see DI-5).

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

---

## Resolved

_None yet. When an open entry is fixed, move it here with the resolving PR/commit._
