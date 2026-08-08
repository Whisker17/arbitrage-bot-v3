# Signerless shadow launcher rehearsal (WHI-953)

**Date (UTC):** 2026-08-08  
**Script:** `scripts/golive/run_signerless_shadow.sh`  

The shadow launcher’s security contract is environmental, not on-chain: the
parent may hold signing keys; the child must not. Anvil is not required to
prove that boundary. The proofs below are the acceptance record.

## 1. Child env snapshot with parent keys present

```bash
MANTLE_PRIVATE_KEY=parent_holds_this \
CHILD_ENV_SNAPSHOT=1 \
scripts/golive/run_signerless_shadow.sh
```

Observed:

```
FORBIDDEN MANTLE_SEPOLIA_PRIVATE_KEY=absent
FORBIDDEN MANTLE_MAINNET_PRIVATE_KEY=absent
FORBIDDEN MANTLE_PRIVATE_KEY=absent
FORBIDDEN PRIVATE_KEY=absent
FORBIDDEN EXECUTION_PRIVATE_KEY=absent
SHADOW_MODE=1
BOT_ENABLE_SENDS=<unset>
child_env_snapshot_ok
```

## 2. Offline bot preflight under sanitized env (`no_send`)

```bash
MANTLE_PRIVATE_KEY=deadbeef \
PREFLIGHT_ONLY=1 \
BOT_BIN=./target/release/bot \
scripts/golive/run_signerless_shadow.sh
```

Bot starts with `SHADOW_MODE=1`, no `--enable-sends`, and prints
`production_send_allowed: false` (see offline report). Exit 0.

## 3. Direct `SHADOW_MODE=1` + forbidden var refuses

- `scripts/shadow/test_launcher_nosend.sh` (continuous launcher) — green
- `SHADOW_MODE=1 PRIVATE_KEY=deadbeef ./target/release/bot --offline` — non-zero
  (`ForbiddenEnvVarPresent`)

## 4. Shared preflight

Launcher always loads universe fingerprint from
`data/pool_universe.meta.json` before any child start
(`0x3a7ba09e6463a302cb37bb2b38a1d4a0508a8e1bf2137bf5145a6af7a5a77f2e` at
rehearsal time, 137 pools).

## Live watch (operator)

```bash
scripts/golive/run_signerless_shadow.sh
# → --watch --ledger <path>, never --enable-sends
```
