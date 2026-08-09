# WHI-977 — Watch-loop skip rate

## Problem

25-minute dual-provider `--watch` at `bc68ea5` (130-pool universe):

```
blocks=422 heads=548 halted_or_skipped=126   # 23.0% skip rate
```

WARN composition was dominated by hash-pin lag (33 HTTP tip timeouts) and a
spammy skip-ratio WARN (38 times) that never acted.

## Changes (code)

| Item | Decision |
| --- | --- |
| Default `http_tip_wait` | **800 ms → 1500 ms** |
| Override | `BOT_HTTP_TIP_WAIT_MS` |
| Lag-shaped RPC errors | Re-poll within deadline (`header not found`, `unknown block`, …) for the **announced hash only** |
| Hash-pinned `get_logs` | Bounded re-poll (400 ms) on lag-shaped errors; still no number-range fallback for state |
| Skip-ratio signal | Rising-edge WARN + periodic INFO (demotion rationale below) |
| Metrics | `skip_rate` / `skip_rate_bps`, tip-visibility p50/p95/p99, skip↔universe-touch correlation probe |
| Hash pin policy | **Unchanged** (WHI-762) |

## Deadline justification

Mantle block time ≈ 2 s. The previous 800 ms deadline left only the short lag
tail covered; WHI-977 evidence showed 33 deadline timeouts under dual WS+HTTP
providers. 1500 ms:

* covers a longer WS-announce → HTTP-has-hash lag tail while remaining under one block time;
* leaves ~500 ms for process/discovery before the next head is typical;
* is overridable when a co-located provider pair has different skew.

The loop records every successful visibility sample and emits
`tip_visibility_p50_ms` / `p95` / `p99` on exit so the next operator can re-justify
or retune `BOT_HTTP_TIP_WAIT_MS` from live data rather than guess.

## Skip-ratio demotion (not halt)

Dual-provider lag is an expected Mantle public-RPC mode (WS often lacks full
`eth_*` surface, so HTTP state + WS heads are split). Cold-start death is already
fail-closed via `skip_fatal_window` (WHI-792). Emitting WARN on every head while
over threshold produced 38 lines and changed nothing.

**Policy:** rising-edge WARN when the rolling window first crosses the threshold;
periodic INFO with the ratio every full window thereafter. Mid-run consecutive-skip
ERROR (WHI-792) remains for sustained unhealth.

## Correlation (skipped vs universe-touch)

On pin skips the loop runs a **metrics-only** number-range `eth_getLogs` for the
skipped height (never applied to state). Exit counters:

* `processed_with_universe_touch` — processed heads with `affected_pools > 0`
* `skip_correlation_probes` / `skip_with_universe_touch` — pin skips whose probe
  matched ≥1 filter log

Paste those counters from the verification run below.

## Same-provider note

`--head-source http-poll` already removes WS/HTTP skew for signerless dry runs
(single HTTP transport). Production still prefers WS for tip latency; elevating
the pin wait + lag re-poll is the dual-provider fix without weakening the pin.

## Verification run

Process wall-clock **31 min** (`timeout 1860`, 2026-08-09T08:21:13Z → 08:52:12Z).
Cold start ~5.5 min; watch loop **~25.3 min** with **748 heads** (more heads than the
original 548-head evidence window). 130-pool universe, dual WS+HTTP, `http_tip_wait_ms=1500`.

```
blocks=726
heads=748
halted_or_skipped=22
skip_rate=0.0294          # 2.94%  (was 23.0% on the filing window)
skip_rate_bps=294
pin_skips=21
http_tip_timeouts=0       # was 33 on the filing window
tip_visibility_samples=748
tip_visibility_p50_ms=377
tip_visibility_p95_ms=740
tip_visibility_p99_ms=1061
skip_ratio_warnings=0     # was 38
processed_with_universe_touch=34
skip_correlation_probes=21
skip_with_universe_touch=1
```

**Skip rate vs target:** `22/748 = 2.94% < 5%` ✅

**Deadline justification against live latency:** p99 tip-visibility is **1061 ms**
and p95 is **740 ms**. Default deadline **1500 ms** sits above p99 with ~440 ms
margin and still under Mantle ~2 s block time. Zero `http_tip_timeouts` on this
run confirms the raised budget covers dual-provider lag for this endpoint pair.

**Correlation (skipped vs universe-touch):**

| set | touch rate |
| --- | ---: |
| processed heads with `affected > 0` | 34 / 726 ≈ **4.7%** |
| pin-skipped heads with probe match | 1 / 21 ≈ **4.8%** |

Rates are essentially the same — **no evidence that skips preferentially hit
blocks that touch our universe** (or spare them). One of 21 pin-skipped heads
had filter-matching logs at that height (metrics-only number-range probe).

Log: `evidence/shadow/whi-977-skip-rate/logs/watch-30m.log` (local; large).
