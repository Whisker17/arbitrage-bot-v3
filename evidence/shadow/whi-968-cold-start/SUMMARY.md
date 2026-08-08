# WHI-968 cold-start evidence

Config: RPC_HTTP_THROTTLE_RPS=4 (recommended for 137 pools)

## Run 1
1:=== cold start run 1 start 2026-08-08T10:13:34Z throttle=4 ===
7:2026-08-08T10:13:34.706429Z  INFO bot.live: building production HTTP provider with throttle/retry/timeout layers throttle_rps=4 max_retries=5 initial_backoff_ms=200 request_timeout_ms=30000 expected_chain_id=5000 http_source="MANTLE_RPC_URL" ws_source="MANTLE_RPC_WS_URL"
12:2026-08-08T10:13:38.649945Z  INFO bot.live: RPC throttle vs universe size (WHI-921/WHI-968); set RPC_HTTP_THROTTLE_RPS if mismatched throttle_rps=4 recommended_throttle_rps=4 pipelined_concurrency=4 pool_count=137
24:2026-08-08T10:13:39.162663Z  INFO amms.agni.sync: populating Agni fee/tick_spacing (pipelined eth_calls) pool_count=94 concurrency=4 throttle_rps=4
1149:2026-08-08T10:19:50.702620Z  INFO bot.live: synced pool state pools=137
4110:=== cold start run 1 end 2026-08-08T10:19:58Z ===

## Run 2
1:=== cold start run 2 start 2026-08-08T10:19:58Z throttle=4 ===
7:2026-08-08T10:19:58.894085Z  INFO bot.live: building production HTTP provider with throttle/retry/timeout layers throttle_rps=4 max_retries=5 initial_backoff_ms=200 request_timeout_ms=30000 expected_chain_id=5000 http_source="MANTLE_RPC_URL" ws_source="MANTLE_RPC_WS_URL"
12:2026-08-08T10:20:04.473637Z  INFO bot.live: RPC throttle vs universe size (WHI-921/WHI-968); set RPC_HTTP_THROTTLE_RPS if mismatched throttle_rps=4 recommended_throttle_rps=4 pipelined_concurrency=4 pool_count=137
35:2026-08-08T10:20:06.846432Z  INFO amms.agni.sync: populating Agni fee/tick_spacing (pipelined eth_calls) pool_count=94 concurrency=4 throttle_rps=4
1150:2026-08-08T10:26:12.179336Z  INFO bot.live: synced pool state pools=137
4111:=== cold start run 2 end 2026-08-08T10:26:17Z ===

## Run 3
1:=== cold start run 3 start 2026-08-08T10:26:17Z throttle=4 ===
7:2026-08-08T10:26:17.809094Z  INFO bot.live: building production HTTP provider with throttle/retry/timeout layers throttle_rps=4 max_retries=5 initial_backoff_ms=200 request_timeout_ms=30000 expected_chain_id=5000 http_source="MANTLE_RPC_URL" ws_source="MANTLE_RPC_WS_URL"
12:2026-08-08T10:26:23.239316Z  INFO bot.live: RPC throttle vs universe size (WHI-921/WHI-968); set RPC_HTTP_THROTTLE_RPS if mismatched throttle_rps=4 recommended_throttle_rps=4 pipelined_concurrency=4 pool_count=137
24:2026-08-08T10:26:23.884556Z  INFO amms.agni.sync: populating Agni fee/tick_spacing (pipelined eth_calls) pool_count=94 concurrency=4 throttle_rps=4
1149:2026-08-08T10:32:18.525538Z  INFO bot.live: synced pool state pools=137
4110:=== cold start run 3 end 2026-08-08T10:32:24Z ===

## Aggregate
run1 synced=1 timeouts=0 429s=0
run2 synced=1 timeouts=0 429s=0
run3 synced=1 timeouts=0 429s=0
