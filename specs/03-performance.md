你。# 03 · 性能问题与优化

> **v2**：经外部 review 修正两处（寻优不能假设严格凹/单峰；增量重估非全缺失——Moe 已有、V3 索引建了没接线、V2 每块重建），文中标 `[已按 review 修正]`。详见 [00-review-response.md](00-review-response.md)。**性能优化整体压后**：先修 01 的运行级故障，再按 replay benchmark 的延迟数据决定做哪些。

套利是延迟竞赛。Mantle ~2 秒出块、中心化 sequencer，没有公开 gas 竞价，谁先把正确的交易送到 sequencer 谁赢。所以性能优化的目标不是"跑得快就好看"，而是**把"新块到达 → 交易送出"的端到端延迟压到 2 秒块窗口内、并且越靠前越好**。

## 当前热路径的成本分析

每个新块，服务大致做：

```
新块 → getLogs(该块) → 逐 pool.sync(log) 更新状态
     → 遍历 path_cache 里所有路径
         → 每条路径 pools_for_path() 线性扫全部池 O(P) × hops
         → best_path_simulation 爬山 ~64 次 × 逐跳 simulate_swap
     → 过滤正利润 → 排序 → 发最优
```

好的一面：`path_cache` 是**启动时构建一次**（service 1559 :568 `build_path_cache`），不是每块重建——这点比库里的 `ArbitrageMonitor::opportunistic_scan`（每块 `build_graph`，monitor.rs:99）强。

坏的一面：

### P1. 增量重估：Moe 已有、V3/V2 缺失 [已按 review 修正]

第一版说"全仓库不做增量"——**不准确**。实际状态是分协议的：
- **Moe** 服务已有 `pool_address → path` 倒排索引 + 受影响路径筛选 ✅
- **V3** 服务**建了** `pool_to_path_indices`（service 1559 :270）**但从不读**（对应 cargo 警告 "never read"），每块仍重估 `path_cache` 里所有路径
- **V2** 服务每块**重建整个图**，最粗糙

而且 V3 的重估还是用**启动时旧状态**（见 01-R2），所以 V3 是"每块用旧状态重估全部路径"，双重浪费。

**优化**：把 Moe 已验证的倒排索引模式推广到 V3/V2；V3 先接线自己已建好的索引（几乎零成本），每块只重估"含本块变动池"的路径。这仍是**投入产出比最高的性能优化**，只是工作量比第一版估计的小（Moe 有现成参考、V3 索引已建）。

### P2. `pools_for_path` 每跳线性扫全池 O(P)

`optimizer.rs:169-189`（以及服务里对应逻辑）为路径每一跳都线性遍历整个池数组找匹配。路径有 h 跳、总池数 P，单条路径就是 O(h·P)。

**优化**：路径缓存里直接存**池的索引/引用**，而不是每次按地址查找。构建 `path_cache` 时就把 `Vec<pool_index>` 固化进去，热路径零查找。

### P3. 寻优 64 次迭代，每次全程逐跳模拟

`best_path_simulation`（service 1559 :1156）爬山最多 64 轮，每轮对整条路径逐跳 `simulate_swap`。V3/Agni 的 `simulate_swap` 是 tick 穿越循环，单次不便宜。

**优化 [已按 review 修正寻优假设]**：
- **不能直接上纯三分/黄金分割**。套利利润关于输入**不是严格凹/单峰**：V3 跨 initialized tick 会让边际输出跳变、LB 跨 bin 同理、整数舍入制造小台阶、输入相关的 gas 让净利润函数分段——这些会产生分段甚至局部峰。纯三分/黄金分割在多峰上会收敛到局部最优。**稳妥做法**：先对数尺度粗采样（覆盖几个数量级）定位所有候选峰区间，再在每个区间内局部优化（三分或爬山），最后显式检查区间端点（最优常在 tick/bin 边界上）。当前爬山 64 次其实比黄金分割更抗多峰，但步长策略粗糙
- 缓存不变量：同一路径寻优过程中池状态不变，tick 数据、sqrt_price 边界可预取复用
- 早停：粗采样确认整段无正利润区间就跳过，不进精细搜索

### P4. Moe bin 遍历线性扫 512 次

`moe/mod.rs:1502,1572-1591`：找下一个非空 bin 是 ±1 逐个试，最多 `MAX_ITERATIONS=512`。不仅慢，还会**漏掉 512 个 bin 以外的流动性**。

**优化**：接入已经 port 好但没用的 `tree_math`（`moe/math/tree_math.rs`），用位图树 O(log) 找下一个非空 bin。这是 Trader Joe 官方合约的做法，代码都在，只是没接线。

### P5. reorg 缓冲同步开销

每次 `StateSpace::sync`（若接线了 reorg）会把受影响 AMM 快照进 `StateChangeCache`（mod.rs 里的 snapshot 逻辑）。`AMM` 有 `Clone`，V3/Agni 池带 tick_bitmap/ticks 的 HashMap，clone 不便宜。深度快照每块做一次，池多时有成本。

**优化**：只快照"本块真正被 mutate 的池"（已经是这样，但确认粒度）；或改成记录反向 diff（delta）而不是全量 clone。优先级低于 P1-P3。

## 编译期/构建配置

`Cargo.toml` 已经开了 `opt-level=3` + `lto=true` + `codegen-units=1` + `panic=abort`（release）——这是对的。但 `[profile.dev]` 也开了满优化（opt-level=3 + lto），导致**开发迭代编译极慢**。建议 dev profile 降到 `opt-level=1`、关 lto，只在跑 bench/实盘用 release。

## 关于 revm CacheDB（你 TODO 里的方向）

`tech-docs/Agni/TODO.md` 提到集成 revm CacheDB。这值得做，但要想清楚定位：

**它解决的问题**：当前 off-chain `simulate_swap` 是各协议数学的 Rust 重实现，和链上合约行为可能有偏差（Moe 的 hook ~200K gas + 输出偏差，`moe/mod.rs:766` 注释已警告；V2 每跳 1 wei 误差就可能 K-check revert，`.sol:295`）。用 revm 在本地 fork 状态上真正 EVM 执行一遍 `executeArbitrage`，能得到**和链上完全一致**的输出和 gas，杜绝"模拟盈利、上链 revert"。

**代价**：revm 执行比纯 Rust 数学慢（要跑 EVM 字节码 + 访问 state）。所以定位应该是：**用快的 Rust 数学做粗筛（找出候选路径 + 大致仓位），只对"要真发的那一条"用 revm 精确验证 + 定 minOut/gas**。不要用 revm 跑全量寻优，会拖垮延迟。

这其实和 C2 的"提交前 eth_call 预模拟"是同一目标的两种实现：`eth_call` 走 RPC（有网络往返延迟，但零本地维护成本），revm 本地跑（零网络延迟，但要自己维护 state fork 的正确性）。**建议先用 `eth_call` 预模拟（简单、够用），确认瓶颈真在这一步再上 revm。**

## 优先级排序

| 优化 | 收益 | 成本 | 建议 |
| --- | --- | --- | --- |
| P1 增量路径重估（倒排索引） | 🔥🔥🔥 | 中 | 先做 |
| P2 路径缓存存池索引 | 🔥🔥 | 低 | 先做 |
| dev profile 降优化 | 🔥（开发体验） | 极低 | 顺手做 |
| P3 黄金分割 + 早停 | 🔥🔥 | 中 | 做 |
| P4 Moe tree_math | 🔥（正确性+性能） | 低（代码已有） | 做 |
| 提交前 eth_call 预模拟 | 🔥🔥（省 revert gas） | 低 | 做 |
| revm CacheDB | 🔥 | 高 | 数据证明需要后再做 |
| P5 reorg 快照优化 | 🔥 | 中 | 最后 |
