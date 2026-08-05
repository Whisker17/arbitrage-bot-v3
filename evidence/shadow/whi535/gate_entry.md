# WHI-535 Phase A — Gate entry checklist

Pinned HEAD at authoring: `01141569d03479b5f17e26df36f883301b9e21a4`.

## 1. Blockers closed / rescope

| Placeholder / id | Status | Merge / note |
| --- | --- | --- |
| M3-1 WHI-527 | Done | stack through `6243d4c` |
| M3-3 WHI-529 | Done | `f357db1924be9574895e0a94ad95e00c6a493978` (#50) |
| M3-6 WHI-532 | Done | `a273b952588e1c3e5f72cfbe594728892a61958a` (#55) |
| G1 WHI-740 | Done | `9d67dcf8e5cfa1afb2efadd19dc3003328c5cbab` (#48) |
| G2 WHI-739 | Done | ledger emission on `bot` (shipped with continuous path) |
| G3 WHI-741 | Done | `38f7c3b6750bb7fb387cbdc4e89066bedd6f79fe` (#53) |
| RPC WHI-745 | Done (escalation) | `519a346` (#54) — no `qualified: true` |
| Market WHI-862 | Done | `01141569d03479b5f17e26df36f883301b9e21a4` (#65) class C |
| M3-5 WHI-531 | Dropped as blocker | rescope comment 2026-08-02 |
| M3-10 WHI-536 | Dropped as blocker | rescope; frozen universe + WHI-784/793 |

## 2. WHI-526 carry-over

See `STATUS.md` table. Signing principal re-scoped to artifact namespaces only; still unprovisioned.

## 3. G1–G4 inspection

### G1 — `REQUIRED_SHADOW_SERVICES`

```text
"v2_monitor_executor_service",
"v3_monitor_executor_service_1559",
"moe_monitor_executor_service",
"bot",
```

File: `src/execution/shadow_thresholds.rs`.

### G2 — ledger emission

`src/bin/bot.rs` calls `build_shadow_execution_context` when `--ledger` is set; rejects `--offline --ledger`.

### G3 — watch loop

`run_multi_protocol_watch_loop` entered when `--watch`; WHI-862 observed non-zero `blocks_processed`.

### G4 — launcher

```bash
RESOLVE_ONLY=1 SERVICES=bot ./scripts/shadow/run_continuous_mainnet.sh
# → bot|bin|bot|bot
```

Log: `resolve_bot.txt`.

## 4. Build baseline commands (operator re-run at GATE_COMMIT)

```bash
git rev-parse HEAD
git status --porcelain   # must be empty for a formal signed run
./scripts/check_toolchain.sh
cargo check --locked --all-targets
cargo test --locked
cargo test --locked --test bot_cross_protocol
cargo test --locked --test shadow_evidence
cargo test --locked --lib service::startup
./scripts/shadow/test_launcher_nosend.sh
```

Recorded in this package session:

- toolchain: OK  
- `service::startup`: 11/11  
- `test_launcher_nosend.sh`: 5/5 refuses  

## 5. Trust roots

| Path | State |
| --- | --- |
| `config/signers/allowed_signers` | header comments only — **no principals** |
| `config/signers/revoked_keys` | empty policy file present |

Digests at package time: `digests/pinned_files.json`.

## 6. Why no signed GatePlan in this PR

`shadow_gate_plan create|sign` requires a provisioned principal that verifies against committed trust roots. Creating a disposable key and committing only the public half is possible, but was **not** done here without explicit owner instruction (key custody is a human act). The recommended reject stands on market evidence alone.
