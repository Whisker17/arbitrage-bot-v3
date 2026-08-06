# WHI-861 — Cold admin handoff runbook

Move the Mantle mainnet `ArbitrageExecutor` admin role off the plaintext deploy
key onto a cold key, register a separate hot executor, and strip admin material
from the bot host. No funding, no unpause, no redeploy.

| Field | Value |
| --- | --- |
| Executor | `0xDC9A6B8f7756860c0caC3e3573587D2CF9d0A4bF` |
| Chain | Mantle mainnet (`5000`) |
| Current admin (pre-handoff) | `0x6A00754e22A4fcde9B5290da7A3367dfF96f6486` |
| Codehash | `0xe1acd0f6ce3257330a9ef37cf7ff29867f3533178c6ccee1a4c3ac167ad3e699` |
| Issue | [WHI-861](https://linear.app/whisker-personal/issue/WHI-861) |

## Role decision (explicit)

| Role | Address | Choice |
| --- | --- | --- |
| **Cold admin** | *operator-provided* | Hardware wallet or offline keystore. Key **must never** exist on the bot host. |
| **Hot executor** | `0x6A00754e22A4fcde9B5290da7A3367dfF96f6486` (recommended default) | **Reuse the former deploy key** as execute-only after `transferAdmin`. Defensible because that key loses `onlyAdmin` powers once transfer lands. |
| **Guardian** | optional | Pause-only. Skip if unset. |

Override `HOT` if you generate a dedicated hot signer instead. Whatever you
choose, **hot ≠ cold** is mandatory.

## Why agents stop here

Generating and custodying the cold key is **owner-only**. An agent-reachable
cold key is not a cold key. Agents may prepare scripts, fork-rehearse, and
verify public post-state. The owner generates `COLD`, signs mainnet txs that
need the current admin key (or cold key for the usability proof), and updates
`.env`.

## Sequence (load-bearing order)

While the contract remains **paused** and **unfunded**:

1. `setHotExecutor(HOT, true)` — from current admin
2. `setGuardian(GUARDIAN)` — optional
3. `transferAdmin(COLD)` — **last**, one-way

Never reverse steps 1 and 3: after transfer the old key cannot register roles.

## 0. Generate cold admin (human)

Offline / hardware wallet. Record **only the address**. Fund that address with
enough MNT for a future incident tx (pause/withdraw/setGuardian) — the usability
proof in step 5 needs gas.

## 1. Unit tests (agent / CI)

```bash
cd contracts/executor
forge test --match-test whi861 -vv
```

Covers the handoff order and the "transfer first is unsafe" regression.

## 2. Fork rehearsal (agent or human; no mainnet write)

```bash
# Use the real COLD address once known, or a throwaway for a pure dry-run:
COLD=0x000000000000000000000000000000000000c01d \
  ./scripts/executor/rehearse_admin_transfer.sh

# Optional guardian:
COLD=0x... GUARDIAN=0x... ./scripts/executor/rehearse_admin_transfer.sh
```

Requires `MANTLE_RPC_URL` / `MANTLE_MAINNET_RPC_URL`. Spins up local anvil fork,
impersonates current admin, replays the three txs, checks post-state, tears down
anvil. Record the printed `fork_block` in evidence.

## 3. Mainnet execute (human; broadcasts)

```bash
COLD=0x<your-cold-admin> \
I_UNDERSTAND_MAINNET=1 \
  ./scripts/executor/execute_admin_transfer.sh
```

Signing: the script reads the **current** admin key from the environment
(`CURRENT_ADMIN_PRIVATE_KEY`, else legacy `MANTLE_PRIVATE_KEY` / …). It refuses
to run if:

- `I_UNDERSTAND_MAINNET` is not `1`
- chain id ≠ 5000
- contract is unpaused or holds native/WMNT balance
- derived key address ≠ on-chain `admin()`
- `COLD == HOT` or `COLD == current admin`

## 4. Public verify

```bash
COLD=0x<cold> HOT=0x6A00754e22A4fcde9B5290da7A3367dfF96f6486 \
  ./scripts/executor/verify_executor_roles.sh
```

Expect: `admin=COLD`, `isHotExecutor(HOT)=true`, `paused=true`, balances `0`.

## 5. Cold-key usability proof (human)

A cold admin that cannot sign under pressure is worse than a hot one. From the
cold key (ledger / interactive / offline-signed):

```bash
# Example: no-op setGuardian to the current guardian value
cast send 0xDC9A6B8f7756860c0caC3e3573587D2CF9d0A4bF \
  'setGuardian(address)' <current-or-same-guardian> \
  --rpc-url "$MANTLE_RPC_URL" \
  --ledger   # or --keystore / --interactive
```

Record that tx hash in evidence.

## 6. Strip admin powers from the bot host

Recommended after a successful handoff with hot = former deployer:

1. Move the deploy key material into `BOT_HOT_EXECUTOR_PRIVATE_KEY` only if the
   send path will use it (WHI-860). Document: **execute-only, not admin**.
2. Remove `MANTLE_PRIVATE_KEY` / `MANTLE_MAINNET_PRIVATE_KEY` / `PRIVATE_KEY`
   if they held admin powers.
3. Confirm:
   ```bash
   git check-ignore -v .env
   git log -p -- .env   # must be empty / never committed
   ```

## 7. Evidence

Fill `evidence/deployments/mantle-mainnet-executor-admin-transfer.md` with:

- Role addresses (no secrets)
- Fork rehearsal block + outcome
- Mainnet tx hashes + blocks for setHotExecutor / setGuardian / transferAdmin
- Cold usability proof tx
- Public post-state table
- Confirmation that `.env` no longer holds admin powers

Update `evidence/deployments/mantle-mainnet-executor.md` deviation 4 status.

## Out of scope

- Funding (WHI-548)
- Enabling sends (WHI-860)
- Redeploy
- Signed-decision ceremony / `allowed_signers` (WHI-535)
