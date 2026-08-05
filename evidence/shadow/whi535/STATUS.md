# WHI-535 — Post-merge signerless requalify (M3-9)

**Issue:** [WHI-535](https://linear.app/whisker-personal/issue/WHI-535)  
**Subject:** merged multi-protocol binary `src/bin/bot.rs` (`cargo run --bin bot`)  
**Mode:** signerless only (`production_send_allowed() == false`)  
**Package role:** Market-reject evidence package + **human decision recommendation**  
(Full formal AC — signed GatePlan/Decision, multi-service R1–R5 co-run under a pre-signed plan — is **not** claimed complete here.)

---

## Decision (recommended)

| Field | Value |
| --- | --- |
| **Verdict** | **`reject`** — deny production signer access to the merged binary |
| **Unlock criteria** | `reject_blocks_go_live_and_m3` (serde: `RejectBlocksGoLiveAndM3`) |
| **Accountable approver** | repo owner (decision principal not yet provisioned in `config/signers/allowed_signers`) |
| **Production signer access** | **DENIED** |

### Why reject (not approve)

1. **Market class C on the committed universe (WHI-862, Done).**  
   Multi-hour signerless window on fingerprint  
   `0x19ce9ed1d60f12d4c577576912a79c859c4faac983d67b46d26355deff389c85`  
   (59 pools: agni-v2=4, agni-v3=18, moe=37) produced:
   - `candidates = 0`, `candidates/day = 0`
   - `blocks_processed ≥ 1` (pipeline alive — not a silent zero)
   - `send_capability = no_send`, `broadcast_count = 0`  
   Evidence: `evidence/shadow/candidate-window/STATUS.md` + `ledger.jsonl` + `analysis_main.json`.

2. **Approve criterion (rescope 2026-08-02 / WHI-862 step 7) requires a recorded non-zero candidate rate.**  
   That criterion is **not** met. Approving would spend custody risk against an unmeasured (actually measured-zero) expected value.

3. **`h2:v2+v2` topology requirement is retired** as structurally unattainable with 4 V2 pools (WHI-862). Carrying WHI-526’s key forward would deadlock this gate the same way.

4. **No production-qualified matched HTTP+WS pair** selected for a multi-service co-run (WHI-745 closed as escalation: no `qualified: true`). Free-tier observation is sufficient for class-C market measurement; it is **not** a basis for granting a signer.

### What this reject does *not* claim

- It does **not** claim the merged binary is broken. Offline cross-protocol discovery and signerless hard-false remain green.
- It does **not** claim RPC is unusable for dry-run observation (WHI-862 already ran live).
- It does **not** authorize WHI-534 dead-code deletion or any `production_send_allowed` flip.

### Ceremony status

| Artifact | Status |
| --- | --- |
| Gate-entry checklist | **Complete** (this package) |
| WHI-862-derived thresholds | **Committed** (`thresholds.json`; byte-equal to `candidate-window/proposed_whi535_thresholds.json`) |
| Thresholds schema validate | **OK** — `shadow_gate_plan create` accepted the file (plan bytes not committed; no mixed-commit illustration plan) |
| Signed `GatePlan` | **Blocked** — `config/signers/allowed_signers` has **zero** provisioned principals |
| Formal multi-service R1–R5 co-run under a pre-signed plan | **Not run** — market class C already falsifies Approve; co-run cannot invent candidates |
| Signed `Decision` via `shadow_decision` | **Blocked** on principal provisioning **or** explicit ceremony collapse |
| This STATUS.md | **Recommended reject** for owner confirmation |

**Owner choice to close the gate formally:**

1. **Collapse ceremony (recommended for a one-person org):** comment on WHI-535 that the verdict is `reject` with the reasons above; mark issue Done; leave this package as the evidence.  
2. **Full OpenSSH ceremony:** provision one principal restricted to  
   `namespaces="whisker-arb/gate-plan/v1,whisker-arb/gate-decision/v1"`,  
   then `shadow_gate_plan` / `shadow_decision` sign a `reject` against a post-plan window at a clean `GATE_COMMIT`.

---

## Gate entry (Phase A)

### Baseline pin

| Field | Value |
| --- | --- |
| Branch HEAD at package authoring | `01141569d03479b5f17e26df36f883301b9e21a4` (`feat(WHI-862): … #65`) |
| `git status --porcelain` at pin | empty on `origin/dev` before package files |
| Toolchain | `./scripts/check_toolchain.sh` → OK (Rust 1.95.0, Foundry 1.7.1, solc 0.8.26) |
| `cargo test --locked --lib service::startup` | 11/11 |
| `cargo test --locked --test bot_cross_protocol` | 13/13 |
| `cargo test --locked --test shadow_evidence` | 11/11 |

### Blockers (Linear + merge commits)

| Issue | Title | Status | Merge / note |
| --- | --- | --- | --- |
| WHI-527 | Merge four services into one multi-protocol binary | **Done** | PR stack ends `6243d4c` (WHI-729) |
| WHI-529 | Settlement semantics | **Done** | `f357db1` (#50) |
| WHI-532 | Prometheus metrics | **Done** | `a273b95` (#55) |
| WHI-739 | Emit shadow ledger from bot | **Done** | merged with #53 / #48 stack |
| WHI-740 | Register bot shadow identity | **Done** | `9d67dcf` (#48) |
| WHI-741 | Continuous `--watch` loop | **Done** | `38f7c3b` (#53) |
| WHI-745 | RPC qualification | **Done** | `519a346` (#54) — **escalation: no qualified pair** |
| WHI-862 | Candidate-rate measurement | **Done** | `0114156` (#65) — **class C** |
| WHI-531 | Canonical PnL reconciliation | **Dropped** as gate blocker (rescope 2026-08-02) | still useful post-canary |
| WHI-536 | Versioned manifest / promotion | **Dropped** as gate blocker (rescope 2026-08-02) | frozen universe + WHI-784/793 substitute |

### G1–G4 (re-verified at HEAD)

| Gap | Required | Verification |
| --- | --- | --- |
| G1 required-service set includes `bot` | `REQUIRED_SHADOW_SERVICES` length 4, last entry `bot` | `src/execution/shadow_thresholds.rs` — present |
| G2 bot emits shadow ledger | `build_shadow_execution_context` on `--ledger` | `src/bin/bot.rs` — present; fail-closed if thresholds path unset |
| G3 continuous watch | `run_multi_protocol_watch_loop` | `src/bin/bot.rs` — present; WHI-862 observed `blocks_processed > 0` |
| G4 launcher knows `bot` | `service_row bot\|bin\|bot\|bot` | `RESOLVE_ONLY=1 SERVICES=bot` → `bot\|bin\|bot\|bot` (log: `resolve_bot.txt`) |

### WHI-526 carry-over triage

| Blocker | Cleared for this gate? | Evidence |
| --- | --- | --- |
| Production-grade RPC multi-addr logs + stable receipts | **No full qualification** (WHI-745 escalation) | `evidence/rpc/STATUS.md` — do not start multi-service co-run on `owner-primary` as *qualified*; free-tier used only for class-C market sample |
| Topology `h2:v2+v2` | **Retired** (not “resolved by coverage”) | WHI-862: only 4 V2 pools; threshold notes retire the key |
| Operator signing principal disposable vs production send | **Re-scoped to artifact signing** | Artifact namespaces only; production send is what this decision denies. Principal still **unprovisioned** (file is comments only) |

---

## Thresholds (pre-declared vocabulary — not an Approve path)

Source: WHI-862 step 7 → `thresholds.json` (notes in `thresholds.notes.json`, which is **not** a loadable ShadowThresholds artifact).

| Metric | Value | Notes |
| --- | --- | --- |
| `required_services` | full canonical 4-list incl. `bot` | order-sensitive |
| `min_canonical_blocks` | `"25"` | near measured unique obs |
| `min_runtime_seconds` | `"1800"` | prefer multi-hour when RPC allows |
| `min_candidate_rows` | `"0"` | scaffolding only; **Approve still needs non-zero market rate** (human / rescope criterion — not encoded solely by this field) |
| `min_real_preflight_samples` | `"0"` | until candidates exist |
| `topology_coverage.required_route_keys` | `["measurement:class-c-no-route-required"]` | deliberate **non-route sentinel** (schema forbids empty list). Formal `shadow_report` topology coverage **fails** while class C holds. Not a claim of observed topology. `h2:v2+v2` **retired** |
| Profit / lifetime floors | zeros / soft | do not require positive net while class C |

These thresholds document the post–WHI-862 vocabulary. They do **not** make a formal multi-service run `verdict_eligible` under class C (topology sentinel + zero candidates). Approve remains blocked until a non-zero candidate rate is measured on the committed fingerprint.

---

## No-send proofs

| Check | Result |
| --- | --- |
| `./scripts/shadow/test_launcher_nosend.sh` | **ok** for all five forbidden vars (`test_launcher_nosend.txt`) |
| `cargo test --locked --lib service::startup` | 11/11 including `production_send_allowed_is_hard_false`, `no_signer_construction_in_startup_source` |
| WHI-862 ledger | `send_capability: no_send`, `broadcast_count: 0` (`analysis_main.json`) |
| Code | `production_send_allowed()` literal `false` in `src/service/startup.rs` |

---

## Replay matrix (R1–R5) status

| Row | Intent | Status |
| --- | --- | --- |
| R1–R3 | Same-window legacy v2 / v3-1559 / moe vs bot | **Not executed** — no qualified multi-service RPC envelope (WHI-745); market reject does not need R1–R3 to be decisive for **Approve denial** |
| R4–R5 | Changed-behaviour / impossible comparisons | Compensating market evidence: WHI-862 class C (bot-only ledger) |
| Cross-protocol fixture | Offline new-vs-new | `cargo test --test bot_cross_protocol` → 13/13 |

**Honest scope:** this package is sufficient to **deny** production signer access (class C fails Approve). It is **not** a complete formal replay-equivalence certificate. Re-run R1–R3 after class A appears (or after a qualified RPC pair exists) if an Approve is ever contemplated.

---

## Artifacts in this package

```text
evidence/shadow/whi535/
  STATUS.md                   # this file — plain-language DENY
  gate_entry.md               # detailed Phase A checklist
  thresholds.json             # WHI-862-derived acceptance thresholds (schema-valid)
  thresholds.notes.json       # human notes only — not loadable by validate
  digests/pinned_files.json   # keccak256 of relevant config files
  test_launcher_nosend.txt    # no-send launcher refusal transcript
  resolve_bot.txt             # RESOLVE_ONLY bot launcher row
```

Market measurement (sibling package, already committed):

```text
evidence/shadow/candidate-window/   # WHI-862 main window
evidence/shadow/candidate-window-lowtvl/
evidence/rpc/STATUS.md              # WHI-745 escalation
```

No private keys, mnemonics, or RPC URLs are stored in this tree.

---

## Follow-ups (out of scope for WHI-535 close)

| Work | Why |
| --- | --- |
| WHI-765 venue inventory / strategy response | Class C is a market/strategy/universe finding |
| Re-qualify a matched paid HTTP+WS pair | Needed before any multi-hour formal co-run |
| Provision artifact-signing principal | Needed for signed GatePlan/Decision tooling |
| WHI-860 send path | Must exist and be reviewed *before* any future Approve, not after |
| WHI-534 dead-code cleanup | Stays blocked while verdict is reject |

---

## One-line summary for Linear

**Production signer access for the merged binary is DENIED:** committed 59-pool universe is class C (zero candidates over a live multi-hour signerless window, WHI-862); Approve’s non-zero candidate criterion fails; `h2:v2+v2` retired; no qualified multi-service RPC envelope.
