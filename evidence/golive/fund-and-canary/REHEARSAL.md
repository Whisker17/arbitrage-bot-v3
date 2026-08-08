# Fund-and-canary anvil fork rehearsal (WHI-953 / WHI-548)

**Date (UTC):** 2026-08-08  
**Mode:** `scripts/golive/fund_and_canary.sh --dry-run`  
**Fork:** same anvil instance as deploy-only rehearsal  
**Chain id:** 5000  

## Approval parameters (required — launcher refuses without them)

| Param | Value used in rehearsal |
| --- | --- |
| `APPROVAL_RECORD_ID` | `whi-953-anvil-rehearsal` |
| `NOTIONAL_CAP_WMNT_ETHER` | `0.01` |
| `CANARY_POOLS_FILE` | 1-line subset of `scripts/golive/pools.txt` (asserted ⊆ universe) |
| `EXECUTOR` | `0xBaDEA1Fc93a3a4c9d8680CF4FfaA065B0eb3A0d1` (from deploy-only) |

## Result

| Field | Value |
| --- | --- |
| Codehash re-check | `0xe2f8a1e096446aadf231ca7ea5d4771008d1c40fa4dded8b2d0681ad9afaeafd` |
| WMNT funded | `10000000000000000` (0.01 ether) |
| `paused()` after | **false** |
| Double-fund guard | refuses if executor WMNT ≠ 0 before fund |

## Missing-param gate (offline)

`scripts/golive/test_launchers.sh` asserts non-zero exit when any of
`APPROVAL_RECORD_ID`, `NOTIONAL_CAP_WMNT_ETHER`, `CANARY_POOLS_FILE`, or
`EXECUTOR` is absent.

## Command

```bash
APPROVAL_RECORD_ID=whi-953-anvil-rehearsal \
NOTIONAL_CAP_WMNT_ETHER=0.01 \
CANARY_POOLS_FILE=/path/to/canary_pools.txt \
EXECUTOR=0xBaDEA1Fc93a3a4c9d8680CF4FfaA065B0eb3A0d1 \
scripts/golive/fund_and_canary.sh --dry-run
```
