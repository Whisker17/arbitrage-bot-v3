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

## Round 2 verification

*(fill after ≥30-minute `--watch`)*

```
blocks=
heads=
halted_or_skipped=
skip_rate=                 # target < 0.05
pin_skips=
http_tip_timeouts=
pin_logs_timeouts=
pin_logs_visibility_p50_ms=
pin_logs_visibility_p95_ms=
pin_logs_visibility_p99_ms=
skip_ratio_warnings=
```
