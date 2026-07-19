# M1 · State Freshness + Snapshot Consistency

> Linear milestone · 占位 `M1-1..9` = `WHI-510..518`  
> **Success** = 每次报价来自同一 canonical snapshot（chain/number/hash + parent/timestamp）；V3/Moe coverage 不全则 `IncompleteState`，从不静默半报价；仓位受 executor 余额约束。

| 占位 | Linear | 标题 | Pri | Labels |
| --- | --- | --- | --- | --- |
| M1-1 | [WHI-510](https://linear.app/whisker-personal/issue/WHI-510) | MarketSnapshot consistency + readiness protocol | Urgent | feature, ready-for-agent |
| M1-2 | [WHI-511](https://linear.app/whisker-personal/issue/WHI-511) | Quote V3 from live pool state + inverted index | Urgent | bug, ready-for-agent |
| M1-3 | [WHI-512](https://linear.app/whisker-personal/issue/WHI-512) | Fix Agni tick coverage; harden UniV3 coverage | Urgent | bug, ready-for-agent |
| M1-4 | [WHI-513](https://linear.app/whisker-personal/issue/WHI-513) | Atomic timestamped MoeSnapshot + explicit coverage | Urgent | bug, ready-for-agent |
| M1-5 | [WHI-514](https://linear.app/whisker-personal/issue/WHI-514) | Cap optimal-input at executor WMNT balance | High | bug, ready-for-agent |
| M1-6 | [WHI-515](https://linear.app/whisker-personal/issue/WHI-515) | Remove appearance-count suppression | High | bug, ready-for-agent |
| M1-7 | [WHI-516](https://linear.app/whisker-personal/issue/WHI-516) | WS gap recovery with header + log backfill | High | feature, ready-for-agent |
| M1-8 | [WHI-517](https://linear.app/whisker-personal/issue/WHI-517) | Process current block N (drop N-1 lag) | High | bug, ready-for-agent |
| M1-9 | [WHI-518](https://linear.app/whisker-personal/issue/WHI-518) | Repair mutable V3 swap state transitions | Urgent | bug, ready-for-agent |

---

## WHI-510 · MarketSnapshot protocol (M1-1)

- **做什么**：引入真正的一致性协议：读操作钉在单一 block hash；完整 chain/header identity；原子发布；显式 Ready/Syncing/Halted，替换/回滚/缺口恢复期间禁止继续报旧快照。
- **影响**：M1 其余 8 条 + 几乎所有 M2 执行/pipeline 的地基。不关 = 混块报价、分叉静默。
- **触及**：`src/state_space/**`、下游所有读池/报价入口。

## WHI-511 · Live V3 quote + inverted index (M1-2)

- **做什么**：V3 候选发现改用每块更新的 live pool，而非启动时 clone；用已有 inverted index 只重算受影响路径，缓存候选仍走当前 fee/balance 利润判断。
- **影响**：V3 机会是否真实；阻塞 Sepolia E2E / shadow gate。
- **触及**：`src/arbitrage/**`、V3 monitor services。

## WHI-512 · V3 tick coverage fail-closed (M1-3)

- **做什么**：修 Agni tickless startup；核对/硬化 UniV3 full-sync；覆盖外一律 `IncompleteState`，禁止零流动性静默报价。
- **影响**：V3 数学可信度；阻塞差分测试（WHI-522）与 shared-core 重构。
- **触及**：`src/amms/agni`、`src/amms/uniswap_v3`。

## WHI-513 · MoeSnapshot + coverage (M1-4)

- **做什么**：Moe 原子快照（slot0+bins+coverage+hash+timestamp）；越界/`amount_left` 返回 IncompleteState；价格用 `active_id` 而非 reserve 比。
- **影响**：Moe 报价正确性与本金相关路径；阻塞差分测试与 checkpoint。
- **触及**：`src/amms/moe/**`。

## WHI-514 · Cap input by executor WMNT (M1-5)

- **做什么**：最优输入搜索上界 = `min(MAX, executor WMNT)`（与 snapshot 同块读取）；超资金融资机会改为 resize 而非提交时丢弃。
- **影响**：可执行机会数量与 sizing 一致性；E2E/shadow 必须同契约。
- **触及**：四个 service 的 search 调用点、余额读取。

## WHI-515 · Drop appearance-count blacklist (M1-6)

- **做什么**：去掉 `AppearanceTracker` 永久过滤；保留 snapshot/coverage/利润/冷却/断路器等门禁；瞬时失败 TTL、结构失败永久。
- **影响**：持续真实机会不再被“出现 3 次就黑”；资格逻辑与 M0 gas 门禁衔接。
- **触及**：四个 service 的 eligibility。

## WHI-516 · WS gap backfill (M1-7)

- **做什么**：重连后按 full SnapshotId 连续性 + hash 钉死的 log backfill 补洞，再处理新 head；分叉/超限 fail-closed。
- **影响**：状态是否静默漂移；E2E/shadow 观察窗有效性。
- **触及**：`src/state_space` 订阅/回补。

## WHI-517 · Process block N (M1-8)

- **做什么**：去掉故意的 N-1 滞后，收到 canonical N 即处理 N（若 log 未就绪则等 N，不退回 N-1）。
- **影响**：发现延迟少一块；阻塞执行状态机（WHI-519）。
- **触及**：四个 service 的 block target。

## WHI-518 · Agni mutable swap state (M1-9)

- **做什么**：`simulate_swap_mut` 必须写入真实 post-swap sqrt/tick/liquidity，禁止 clone 后原样拷回。
- **影响**：连续模拟/路径 sizing 正确；差分测试与 M3 shared-core 的前置。
- **触及**：`src/amms/agni/mod.rs`。
