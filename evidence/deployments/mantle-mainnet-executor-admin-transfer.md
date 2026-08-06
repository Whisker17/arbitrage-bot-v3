# Mantle mainnet — ArbitrageExecutor admin → cold key (WHI-861)

**Status:** agent preparation complete; **mainnet handoff not yet executed**  
**Date (rehearsal):** 2026-08-06  
**Chain:** Mantle mainnet, chain id **5000**  
**Executor:** `0xDC9A6B8f7756860c0caC3e3573587D2CF9d0A4bF`  
**Issue:** WHI-861  
**Runbook:** `docs/runbooks/WHI-861-cold-admin-handoff.md`

> Generating and custodying the cold admin key is **owner-only**. This file
> records the agent-owned preparation (unit tests, fork rehearsal, scripts) and
> leaves blanks for the mainnet outcome the owner fills after signing.

## Explicit role decision

| Role | Address | Decision |
| --- | --- | --- |
| **Cold admin** | *TBD by owner* | Hardware wallet or offline keystore. Key must never land on the bot host. |
| **Hot executor** | `0x6A00754e22A4fcde9B5290da7A3367dfF96f6486` | **Reuse former deploy key** as execute-only after `transferAdmin`. Stated explicitly (not a silent default). |
| **Guardian** | *optional / TBD* | Pause-only. Rehearsal exercised a non-zero guardian; live may skip. |

Hot **must** differ from cold.

## Pre-handoff mainnet state (public reads)

Captured immediately before fork rehearsal (2026-08-06):

| Check | Observed |
| --- | --- |
| `chain_id` | `5000` |
| tip (approx) | `98928129` |
| `admin()` | `0x6A00754e22A4fcde9B5290da7A3367dfF96f6486` |
| `guardian()` | `0x0000000000000000000000000000000000000000` |
| `paused()` | `true` |
| `isHotExecutor(deployer)` | `false` |
| native balance | `0` |
| WMNT balance | `0` |
| codehash | `0xe1acd0f6ce3257330a9ef37cf7ff29867f3533178c6ccee1a4c3ac167ad3e699` |

Matches `evidence/deployments/mantle-mainnet-executor.md`.

## Unit tests

```text
forge test --match-test whi861 -vv
[PASS] test_whi861_custody_handoff_sequence()
[PASS] test_whi861_transfer_admin_before_set_hot_is_unsafe_order()
```

Anchors: `contracts/executor/test/ArbitrageExecutor.t.sol` (WHI-861 section).

## Fork rehearsal

**Command** (no private keys; anvil impersonation of current admin):

```bash
COLD=0x000000000000000000000000000000000000c01d \
GUARDIAN=0x00000000000000000000000000000000000061A1 \
  ./scripts/executor/rehearse_admin_transfer.sh
```

| Field | Value |
| --- | --- |
| Script | `scripts/executor/rehearse_admin_transfer.sh` |
| Mainnet tip at start (first OK run) | `98928129` |
| Anvil fork block (first OK run) | `98928130` |
| Anvil fork block (post-review re-run) | `98928345` |
| Rehearsal COLD (throwaway) | `0x000000000000000000000000000000000000c01d` |
| Rehearsal GUARDIAN (first run) | `0x00000000000000000000000000000000000061A1` |
| HOT | `0x6A00754e22A4fcde9B5290da7A3367dfF96f6486` |
| Result | **OK** (twice) |

### Fork post-state

| Check | Observed |
| --- | --- |
| `admin()` | `0x000000000000000000000000000000000000C01D` |
| `guardian()` | `0x00000000000000000000000000000000000061A1` |
| `paused()` | `true` |
| `isHotExecutor(HOT)` | `true` |
| native / WMNT | `0` / `0` |
| former admin `transferAdmin` | **rejected** |

### Rehearsal notes

1. **Do not pin `--fork-block-number` on non-archive Mantle RPCs** — anvil fails
   with "older block with a non-archive node" even for the current tip. The
   rehearsal script forks latest only and records the anvil block number after
   ready.
2. Order is load-bearing: `setHotExecutor` → optional `setGuardian` →
   `transferAdmin` last. The unit test proves reverse order leaves the former
   admin unable to register as hot.
3. No mainnet write was performed. Throwaway COLD/GUARDIAN are not production
   addresses.

## Mainnet execution (owner — pending)

Fill after broadcast:

| Step | Tx hash | Block |
| --- | --- | --- |
| `setHotExecutor(HOT, true)` | _TBD_ | _TBD_ |
| `setGuardian(...)` (if any) | _TBD_ / skipped | _TBD_ |
| `transferAdmin(COLD)` | _TBD_ | _TBD_ |
| Cold usability proof (`setGuardian` no-op or equivalent) | _TBD_ | _TBD_ |

### Post-state (public reads after mainnet)

| Check | Expected | Observed |
| --- | --- | --- |
| `admin()` | cold address | _TBD_ |
| `isHotExecutor(HOT)` | `true` | _TBD_ |
| `admin() != HOT` | true | _TBD_ |
| `paused()` | `true` | _TBD_ |
| native / WMNT | `0` / `0` | _TBD_ |
| codehash | unchanged | _TBD_ |

```bash
COLD=0x... HOT=0x6A00754e22A4fcde9B5290da7A3367dfF96f6486 \
  ./scripts/executor/verify_executor_roles.sh
```

## Host key hygiene (owner — pending)

- [ ] `.env` no longer holds a key with admin powers
- [ ] If former deploy key is retained, it lives only as
      `BOT_HOT_EXECUTOR_PRIVATE_KEY` (execute-only; WHI-860) — not as admin
- [ ] `git check-ignore -v .env` succeeds; `git log -p -- .env` empty

## Scripts added

| Path | Purpose |
| --- | --- |
| `scripts/executor/rehearse_admin_transfer.sh` | Anvil fork rehearsal |
| `scripts/executor/execute_admin_transfer.sh` | Gated mainnet broadcast (`I_UNDERSTAND_MAINNET=1`) |
| `scripts/executor/verify_executor_roles.sh` | Public post-state checks |
| `docs/runbooks/WHI-861-cold-admin-handoff.md` | Full operator runbook |

## Secrets

This file contains **no** private keys and **no** RPC credentials.
