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

## Round 2 verification (local, 2026-08-09)

Process ~35 min (`timeout 2100`), throttle **4**, dual WS+HTTP, `pinned_logs_retry_ms=1500`.
Watch loop ~27 min (12:43:23Z → 13:10:27Z).

```
blocks=20
heads=20
halted_or_skipped=0
skip_rate=0.0                 # 0% of observed heads
pin_skips=0
http_tip_timeouts=0
pin_logs_waits=20             # every head needed some getLogs re-poll
pin_logs_timeouts=0
tip_visibility_p50/p95/p99_ms = 247 / 481 / 640
pin_logs_visibility_p50/p95/p99_ms = 257 / 497 / 571
skip_ratio_warnings=0
mid_run_rebaselines=19
processed_with_universe_touch=4
```

**Pin-stage result:** among observed heads, skip_rate = **0%** and
`pin_logs_timeouts = 0`. Logs-stage p99 (**571 ms**) sits well under the **1500 ms**
budget — consistent with "header ready, getLogs lags a bit longer" and with the
budget raise fixing that stage.

**Sample-size caveat (important):** only **20 heads** were observed in ~27 min of
watch (operator re-check had **973**). `mid_run_rebaselines=19` means nearly every
head arrived after a large gap — processing is so slow that most chain heads never
enter `heads_observed` (WS buffer / sequential process). That is a **throughput**
issue, not a pin-skip counter issue. Operator should re-run a ≥30-min window and
paste `heads=` / `halted_or_skipped=` / `pin_logs_*` for the real AC call.

Log: `evidence/shadow/whi-977-skip-rate/logs/watch-r2-35m.log` (local).
