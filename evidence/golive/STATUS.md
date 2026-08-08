# WHI-953 — Go-live launcher split

**Date:** 2026-08-08  
**Issue:** [WHI-953](https://linear.app/whisker-personal/issue/WHI-953)  
**Spec:** `specs/07-go-live-hardening.md` G-6  

## Launchers

| Launcher | Script | Contract |
| --- | --- | --- |
| Signerless shadow | `scripts/golive/run_signerless_shadow.sh` | `SHADOW_MODE=1`, no `--enable-sends`, sanitized child env |
| Deploy-only | `scripts/golive/deploy_only.sh` | Steps 1–5; ends **paused + unfunded**; addresses only |
| Fund-and-canary | `scripts/golive/fund_and_canary.sh` | Requires second-approval params; funds + unpauses |

Shared preflight lives in `scripts/lib/golive_common.sh` (chain id, codehash, universe fingerprint, registry ⟷ universe).

Retired: `scripts/golive/deploy_and_arm.sh` (exits 1 with migration hint).  
`scripts/golive/run_live.sh` is the **production** `--enable-sends` supervisor only; refuses `SHADOW_MODE=1`.

## Offline acceptance

```bash
./scripts/golive/test_launchers.sh
# + scripts/shadow/test_launcher_nosend.sh (invoked inside)
```

## Anvil fork rehearsals

| Launcher | Record |
| --- | --- |
| Deploy-only | [deploy-only/REHEARSAL.md](deploy-only/REHEARSAL.md) |
| Fund-and-canary | [fund-and-canary/REHEARSAL.md](fund-and-canary/REHEARSAL.md) |
| Signerless shadow | [signerless-shadow/REHEARSAL.md](signerless-shadow/REHEARSAL.md) |
