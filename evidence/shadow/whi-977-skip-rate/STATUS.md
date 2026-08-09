# WHI-977 — Watch-loop skip rate

## Problem

Dual-provider `--watch` skips a large fraction of heads. Round 1 (PR #95 /
`2de3fde`) fixed the **header** wait and demoted skip-ratio spam, but operator
re-check showed **skip_rate still ~11%**.

## Round 1 — what to keep

| Item | Status |
| --- | --- |
| Tip-visibility p50/p95/p99 (timeouts in samples) | **keep** |
| Skip-ratio rising-edge WARN + periodic INFO | **keep** |
| `skip_correlation_probes` / `skip_with_universe_touch` | **keep** |
| Header wait 1500 ms (`http_tip_timeouts=0`) | **keep** (correct for that stage) |
| Skip rate itself | **not fixed** |

## Round 1 post-merge re-check (AC fail)

40-minute `--watch`, 130-pool, throttle 4, `2de3fde`:

```
blocks_processed=865  heads_observed=973  halted_or_skipped=108
skip_rate=0.111                       # acceptance: < 5%. NOT MET
pin_skips=108
http_tip_timeouts=0                   # raised header wait never expired
tip_visibility_p50/p95/p99 = 386 / 899 / 1493
```

**Diagnosis:** header at the announced hash is served; `eth_getLogs` at **that
same hash** is not. Round 1 only gave getLogs a **400 ms** lag re-poll after
header ready — too short. Extending the header wait further cannot move the rate.

## Round 2 fix

| Item | Decision |
| --- | --- |
| `DEFAULT_PINNED_LOGS_RETRY` | **400 ms → 1500 ms** |
| Override | `BOT_PINNED_LOGS_RETRY_MS` |
| Hash pin | **unchanged** — same hash filter only |
| Logs-stage latency | `pin_logs_visibility_p50/p95/p99` + `pin_logs_timeouts` |

## Round 2 verification (AC, 2026-08-09)

Throttle **4**, dual WS+HTTP, `pinned_logs_retry_ms=1500`.
Watch ~**79 min** (loop start ≈13:21 → SIGTERM 14:40 UTC). Comparable sample
to the operator re-check (973 heads).

```
blocks=1313
heads=1323
halted_or_skipped=10
skip_rate=0.00756             # 0.76%  ← AC < 5%  PASS (was 11.1% post r1)
skip_rate_bps=75
pin_skips=2                   # was 108
http_tip_timeouts=0
pin_logs_waits=1314
pin_logs_timeouts=1           # was essentially all 108 pin skips at getLogs
tip_visibility_p50/p95/p99_ms = 388 / 987 / 1540
pin_logs_visibility_p50/p95/p99_ms = 387 / 1005 / 1407
skip_ratio_warnings=0
mid_run_rebaselines=1
processed_with_universe_touch=97
skip_correlation_probes=2
skip_with_universe_touch=0
```

**Skip reasons:** processing_failed=8, pinned_logs_unavailable=1, pinned_header_rpc_error=1.

**AC:** `10/1323 = 0.76% < 5%` ✅  
**getLogs budget:** p99 logs lag **1407 ms** < **1500 ms** default (1 timeout).

Log: `evidence/shadow/whi-977-skip-rate/logs/watch-r2-ac-20260809T131901Z.log` (local).
