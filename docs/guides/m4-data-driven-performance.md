# M4 · Data-Driven Performance

> 占位 `M4-1..9` = `WHI-537..545` · 正确性/可观测之后才做  
> **Success** = replay 给出 block→submit 的 p50/p95/p99；每项优化有测得收益依据。  
> **例外**（可不经 P2.5 开工，但禁止拿 signer / 改生产优先费）：`WHI-542`、`WHI-545`。

| 占位 | Linear | 标题 | Pri | Labels |
| --- | --- | --- | --- | --- |
| M4-1 | [WHI-537](https://linear.app/whisker-personal/issue/WHI-537) | Measure p50/p95/p99 block→submit latency | High | feature, needs-triage |
| M4-2 | [WHI-538](https://linear.app/whisker-personal/issue/WHI-538) | Cache per-input quotes; re-optimize bounds | Medium | feature, needs-triage |
| M4-3 | [WHI-539](https://linear.app/whisker-personal/issue/WHI-539) | Wire tree_math for Moe bin traversal | Medium | feature, needs-triage |
| M4-4 | [WHI-540](https://linear.app/whisker-personal/issue/WHI-540) | Multi-peak optimal-input search | Medium | feature, needs-triage |
| M4-5 | [WHI-541](https://linear.app/whisker-personal/issue/WHI-541) | Evaluate revm CacheDB for final sim | Low | research, needs-triage |
| M4-6 | [WHI-542](https://linear.app/whisker-personal/issue/WHI-542) | Lower dev profile opt-level | Low | chore, needs-triage |
| M4-7 | [WHI-543](https://linear.app/whisker-personal/issue/WHI-543) | Cache path pool-index mappings | Low | feature, needs-triage |
| M4-8 | [WHI-544](https://linear.app/whisker-personal/issue/WHI-544) | Evaluate inverse deltas for reorg buffer | Low | research, needs-triage |
| M4-9 | [WHI-545](https://linear.app/whisker-personal/issue/WHI-545) | Verify Mantle sequencing / priority-fee / private submit | Medium | research, needs-triage → human |

---

## WHI-537 · Latency benchmark harness (M4-1)

- **做什么**：录块回放 merged pipeline，报告 block→submit 各阶段 p50/p95/p99。
- **影响**：M4 其余优化的**数据门**；无数据不立项优化。
- **触及**：bench/replay harness、metrics 接线。

## WHI-538 · Incremental quote cache (M4-2)

- **做什么**：仅对池版本变化路径重算 AMM 曲线；fee 变只重算 gas；余额边界变触发再优化而非裁剪旧最优点。
- **影响**：热路径 CPU；机会净利判断是否跟上 fee/余额。
- **触及**：`src/arbitrage` 报价/优化缓存。

## WHI-539 · Moe tree_math bins (M4-3)

- **做什么**：用已 port 的 `tree_math` 位图树替代 ±1 线性扫 bin（≤512）。
- **影响**：Moe 大跨度路径延迟；差分测试守正确性。
- **触及**：`src/amms/moe/math/tree_math.rs`、`simulate_swap_precise`。

## WHI-540 · Multi-peak input search (M4-4)

- **做什么**：不假设凹性：log 粗采样找峰区间 → 局部优化 → 端点检查；替换旧 hill-climb / `PathOptimizer::optimize`。
- **影响**：多峰路径（V3 tick / LB bin）是否找对最优输入；生产与 mock 统一实现。
- **触及**：shared optimizer、`mock.rs`、生产 sizing。

## WHI-541 · revm CacheDB spike (M4-5)

- **做什么**：仅当 M4-1 显示 `eth_call` 是瓶颈时，评估本地 revm 模拟 vs RPC 的延迟/保真。
- **影响**：preflight 延迟上限；可能改变最终仿真路径（research 决策）。
- **触及**：execution preflight 实验代码。

## WHI-542 · Faster dev builds (M4-6)

- **做什么**：`[profile.dev]` 从 opt-level=3+LTO 降到 1、关 LTO；release 不变。
- **影响**：迭代速度；不改 runtime 行为。可与主路径并行。
- **触及**：`Cargo.toml`。

## WHI-543 · Path pool-index cache (M4-7)

- **做什么**：构图时把 hop→稳定 pool index 写入 path cache，去掉热路径 O(hops×pools) 地址扫描。
- **影响**：报价热路径；universe/manifest 变更时必须失效索引。
- **触及**：path cache、`PoolUniverse` fingerprint。

## WHI-544 · Inverse reorg deltas (M4-8)

- **做什么**：若 profiling 显示 V3/Moe clone 代价大，评估逆 delta reorg buffer；不得削弱 M1 原子快照语义。
- **影响**：reorg 内存/延迟；深度 reorg 仍 fail-closed resync。
- **触及**：`src/state_space` reorg buffer。

## WHI-545 · Mantle sequencing research (M4-9)

- **做什么**：用权威文档/受控实验核实排序、priority-fee、私有提交通道；写出 dated 决策记录。
- **影响**：未来 fee/私有路由调参依据；在证据前保持 M0-2 保守默认。
- **触及**：docs/决策 ADR；不改生产发送。
