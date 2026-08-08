# WHI-951 / first funded canary: pure routes only

## Decision

The first funded canary (`WHI-548` after requalification approve) validates
**pure-protocol** send paths only:

* A candidate whose hops span more than one `ProtocolKind` is **observed**
  (discovery, ranking, per-block summary) but **never sent**.
* Mixed-route send capability is **explicitly out of scope** for this canary
  and for the G-8 signerless requalification window.

This is the conservative option from `specs/07-go-live-hardening.md` §8.1
item 2 (r3). Cross-DEX discovery remains the point of the merged binary
(`WHI-727` / `WHI-728`); only the **armed send** surface is pure-only until a
later issue owns mixed execution.

## Operator-visible behaviour

| Field / path | Meaning |
| --- | --- |
| `mixed_skipped_count` | Cross-protocol candidates seen this block (summary only) |
| `best_mixed_net` | Highest net PnL among those mixed candidates (not a sum) |
| `eligible` | Statically sendable pure candidates (caps + gas profile + mix) |
| Armed attempt plan | Eligible pure candidates in net-PnL order, under `BOT_ATTEMPT_BUDGET_MS` |
| Gate closed (`production_send_allowed = false`) | Unchanged historical top-1 (may be mixed → typed gate block) |

Static filters (applied before any send attempt when armed):

1. Protocol mix is pure (not cross-protocol).
2. Route bucket has an approved gas profile (fail closed).
3. `amount_in` within per-tx / optional balance bounds (`BreakerConfig` caps).
4. Inventory precondition: if balance already exceeds
   `MAX_TOTAL_INVENTORY_WMNT_WEI`, the block is unsendable.

Dynamic preflight failures may advance to the next eligible candidate only
inside the attempt budget (`DEFAULT_ATTEMPT_BUDGET` = 400 ms, override
`BOT_ATTEMPT_BUDGET_MS`). Successful broadcast stops immediately. No
per-skipped-candidate info log lines (G-5 bounded logging).

## Canary decision record

When approving G-8 / `WHI-548`:

* Require at least one **pure** sendable, preflight-passing candidate in the
  shadow / requal window (gate A).
* Do **not** treat mixed candidates as evidence of send readiness.
* Attribute peer-comparison misses that were filtered for mix to G-4
  eligibility, not to discovery failure.

## Related

* Code: `src/service/eligibility.rs`, `src/service/block_loop.rs` (armed plan)
* Spec: `specs/07-go-live-hardening.md` G-4 / G-5 / §8.1
* Linear: WHI-951
