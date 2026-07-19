# 00 · 对外部 review 的核查与修正记录

> 背景：第一版 specs（01–05）由静态代码审计生成。随后一份外部 review（GPT-5.6，审的是同一 commit `43efa87`）指出了若干**运行级故障**和对我结论的修正。本文件逐条记录我**对着代码/活链验证**的结果，供再次复查。
>
> 核查环境：`dev` 分支，commit `43efa87`，未修改任何源码。gas 相关数据来自 2026-07-18 对 `rpc.mantle.xyz` / `mantle.publicnode.com` 的实时查询。

## 结论一句话

外部 review 提出的 8 条 P0 运行级故障，**逐条验证全部属实**；对我结论的约 10 项修正也全部成立。第一版报告最大的盲区是：只做了静态结构审计，没有沿着服务的数据流验证运行时行为，也没有拿代码里的 Mantle 假设去对活链核对。据此已重写路线图并修正 01/02/03/04/05。

> **第二轮**：外部 review 又做了一轮只读复审，确认 8 条 P0 核查仍成立，但指出我第一轮**只补丁式改了部分文档**，留下跨文档不一致（尤其 04 基本还是 v1、01-B2 仍自相矛盾）。第二轮的 10 条 + 若干小问题**我也逐条验证并全部采纳**，详见文末[第二轮记录](#第二轮review的核查与修正)。

---

## 一、新增 P0（运行级阻断）—— 逐条已验证

| # | 声明 | 核查结果 | 证据 |
| --- | --- | --- | --- |
| R1 | **gas 配置导致交易发不出去** | ✅ 属实，且是最根本问题 | 活链证据：block `98121659`（2026-07-18T11:20:30Z），hash `0xc44bbf683065f56d14cfa1abf20fbdafdcc39e45e8071ea30e603a95228469ed`；两个 RPC 均返回 `gasLimit=60,000,000`、`baseFee=50,000,000,000 wei(50 gwei)`。代码：gas limit 300M–2.8B（`gas_schedule.rs:8-14`）**超过该块上限**；`MANTLE_BASE_FEE_WEI=20_000_000`（`executor.rs:64`）= 0.02 gwei，**比该块 base fee 低 2500 倍**；小额利润时 `effective_global_cap=500_000_000 wei`(0.5 gwei)（`executor.rs:82`）**远低于该块 base fee**，`max_fee_per_gas` 被钳到 0.5 gwei → 在该链参数下交易被拒 |
| R2 | **V3 服务用启动时的池状态报价** | ✅ 属实 | `find_profitable_candidates` 收了实时 `pools` 参数，但 `:935` 实际用 `path_cache.state_pools`——那是 `:323` 启动时的 `state.state.values().cloned()`。倒排索引 `pool_to_path_indices`（`:270`）建好了但从不读（对应 cargo 警告 "field `pool_to_path_indices` is never read"） |
| R3 | **V3 启动不同步历史 tick** | ✅ 属实 | 服务 `:806` 调 `init_basic`；`init_basic` 在 `agni/mod.rs:397-398` 明确 `tick_bitmap.clear(); ticks.clear()`。跨 initialized tick 的报价不可信，叠加 1e24 搜索上限风险高 |
| R4 | **所有服务故意落后一块** | ✅ 属实 | `:660` `target_number = number.saturating_sub(1)`，收到块 N 处理 N-1，凭空加 ~2 秒延迟 |
| R5 | **Moe 执行无本金保护** | ✅ 属实，推翻我原结论 | Moe `:1388` `amounts_out_with_slippage = vec![U256::ZERO; ...]`；`:1364/1379` 余额不足时 `adjusted = executor_balance` 直接当 input 用、**不重新模拟**。合约 `:169` `minExpected = balanceBefore - _amountIn + _amountsOut[last]`，`_amountsOut[last]=0` 时退化为"允许整笔 input 亏光"。**对比**：V3 服务 `:1051` 把 last 强制 `.max(candidate.input)`，所以保护是**分协议的**——V3 有、Moe 没有 |
| R6 | **Moe bin 重同步残留幽灵流动性** | ✅ 属实 | `sync_active_bins_batch`（`moe/mod.rs:525-630`）只在 `:596` `reserve_x>0 || reserve_y>0` 时 insert，**无 clear/remove**。链上已归零的旧 bin 仍留内存参与报价 |
| R7 | **候选生命周期逻辑永久错杀** | ✅ 属实 | `AppearanceTracker`（`:171`）`entry.1 += 1` 只在块号变化时累加 → 统计的是"出现过的不同块数"累计值，非注释所称"连续出现"；超 `MAX_APPEARANCES=3`（`:63`）**永久过滤**。且 `:641-643` 任何 `Err`（余额不足/RPC 错/发送失败）都 `mark_as_failed` → **永久黑名单** |
| R8 | **执行队列制造陈旧交易** | ✅ 属实 | `:585` 容量 64 队列，`:593` 单 worker `while let Some(job)=rx.recv()` 串行、每笔 `attempt_execution` 等链上确认才处理下一笔。应按状态版本失效 / TTL / latest-wins，而非排队跑旧机会。单 worker 也意味着 nonce 冲突不是首要问题（修正了我原 C3 的优先级判断） |

**R1 的深层含义**：代码是在 Mantle **旧 gas 参数**（gas limit 十亿级、base fee 近零）下写的，而上述证据块已经是 60M limit + 50 gwei base fee；第一版报告里反复出现的"Mantle gas limit 是十亿级"前提已经不适用于该块。60M/50 gwei 是带块号的观测，不是要写回代码的新常量；运行时必须动态读取。

---

## 二、对我原结论的修正 —— 逐条已验证

| 原结论 | 修正 | 核查 |
| --- | --- | --- |
| 01-A1："任何 cargo 命令都失败" | **表述过头**。`cargo build`、`cargo check --lib`、四个服务各自 `cargo check --example` 都**成功**；失败的只是 `cargo check --all-targets`、`cargo test --no-run` 和 9 个失效指定 target | ✅ 已复现：`--lib` 0 error |
| （漏报） | `cargo test --lib` 另有编译期溢出：`uint256x256_math.rs:185` 的 `1_u64 << 200`（u64 移位 200 位溢出，在 `#[test]` 里） | ✅ 已确认 |
| B3："misprice 是服务侧重复路径来源" | **服务侧其实恒为空**。服务配置 required start=end=WMNT，而 `find_two_pool_misprices` 产出 `A→node→B`（A≠B），二者矛盾 → 服务里该 finder 不产出任何路径。**库层 bug 成立，但在服务中是休眠的**，不是重复来源 | ✅ 逻辑确认 |
| 03：寻优用"黄金分割/三分" | **假设过强**。利润关于输入**不是严格凹/单峰**：V3 tick、LB bin、整数舍入、输入相关 gas 会造成分段甚至局部峰。应对数尺度粗采样定位多个峰区间，再局部优化 + 端点检查 | ✅ 采纳，已改 03 |
| 03：增量重估"全仓库缺失" | **不准确**。Moe 已有倒排索引 + 受影响路径筛选；V3 索引存在但**未接线**；V2 每块重建图。是"V3/V2 缺失"而非"全缺失" | ✅ 确认（Moe `pool_to_path` 在用，V3 的建了没用） |
| 02：WMNT 起止"在库层强制" | **归属错了**。WMNT 起止是策略/结算资产配置，应放策略引擎，不该硬编码进通用 `amms` 库 | ✅ 采纳，已改 02 |
| 02：Agni 直接复用 UniV3 struct | **过粗**。应共享内部 concentrated-liquidity 核心，但**保留各自 event/ABI adapter**（Agni 的 Swap 事件等真实差异不能被掩盖） | ✅ 采纳，已改 02 |
| 05："纯删除零风险"含 broadcast/lock/mock | **收紧**。`.bak`/`out/`/`cache/` 可删；但 `broadcast/` 是部署溯源、`foundry.lock` 是可复现资产，不是垃圾；`mock.rs` 是离线测试设施，不是死代码 | ✅ 采纳，已改 05 |
| C3：公开 mempool / 私有通道 | **缺当前证据**。Mantle 官方可查到的是 fair-sequencing 研究提案，不足以证明生产网络已有私有/优先通道。措辞改为"待核实" | ✅ 采纳 |
| （漏报） | **依赖不可复现**：`.gitignore:2` 忽略 `Cargo.lock`，`alloy="1.0.25"` 实际解析成 `1.8.3`。实盘 bot 应提交 lockfile 并明确升级流程 | ✅ 已确认 |

---

## 三、执行顺序（⚠️ 本节是第一轮的快照，已被第三轮取代）

> **以 [README](README.md) 路线图为准。** 下面是第一轮采纳时的顺序，其中第 1 条"只暂停 Moe 执行"已在第二轮修正为"**撤出 executor 全部资金 + 停所有执行**"（callback 漏洞与协议无关，见第二轮#1），第三轮继续细化实现契约。保留此节仅作演进记录，不要据此执行。

第一轮顺序（历史）：
1. ~~暂停真实 Moe 执行~~ → 现为"撤资 + 停全部执行"；修 callback 鉴权、最终最小回款（R5）
2. 动态读 base fee + block gas limit，`estimateGas` 定 gas / `eth_call` 语义 preflight（R1）
3. 修 V3 新鲜状态（R2）、完整 tick（R3）、当前块处理（R4）、候选生命周期（R7/R8）
4. 版本化 `MarketSnapshot → Candidate → Preflight → Submit`（第三轮补：快照一致性协议，非仅版本标签）
5. AMM 模拟 vs 链上 `eth_call` 差分测试
6. 合并单一多协议 binary，允许同时启用 V2/V3/Moe
7. 按 replay benchmark 延迟决定是否做多峰寻优、tree index、revm、reorg delta

---

## 四、第一版为何漏掉这些（供改进）

三个并行子 agent 分别审 amms/state_space、arbitrage/execution、examples，各自**在模块边界内**审得较细，但：
- 没有**跨模块追数据流**：R2（实时 pools 参数被传入却不用）要对比"传入参数 vs 实际读取"才看得出，单看函数签名会以为在用实时状态
- 没有**拿代码假设对活链核对**：R1 只有查了 Mantle 当前 `gasLimit/baseFee` 才暴露，纯读代码只会觉得"硬编码 Mantle 常量"是合理的
- 偏"这段代码对不对"，弱"这个系统跑起来会怎样"：R4/R7/R8 都是逻辑各自成立、组合起来才致命

下次审这类实盘系统，应固定加一步"活链参数核对 + 端到端数据流追踪"。

---

## 第二轮review的核查与修正

第二轮只读复审指出：8 条 P0 核查成立，但第一轮修正**不彻底**，留下跨文档不一致。10 条 + 小问题逐条核查如下，**全部采纳**。

### 前四条（跨文档不一致，已改）

| # | 问题 | 核查 | 修正落点 |
| --- | --- | --- | --- |
| 1 | **callback 止血范围过窄**：路线图只说"停 Moe"，但回调漏洞与协议无关，只要 executor 持币任意攻击者就能直接调回调掏走。应"停全部执行 + 撤出全部资金 → 修鉴权 → 测试 → 重部署 → 才注资" | ✅ 成立。`agniSwapCallback` 是独立 external 入口，不经 `onlyOwner` 的 executeArbitrage | README P0-1、01-C1 已改 |
| 2 | **状态新鲜度不完整**：WS 重连/块连续性/缺口回补被放 P3，但没它们不能宣称"状态是最新的"，应进 P1；影子模式应在版本化快照+缺口回补+preflight 后再跑 | ✅ 成立，属新鲜度非稳健性 | README P1-4、04-F4/F7 已改 |
| 3 | **02 保留旧框架**：仍称服务"实战打磨已验证"（但有 P0）、"凹函数三分/黄金分割"、"强制 WMNT"、"单选 --protocol" | ✅ 成立，四处冲突 | 02:35/45/67/68/78 已改 |
| 4 | **04 基本还是 v1**：nonce 冲突、动态 gas 第三梯队、公开 mempool、WMNT 库层强制、按池集合去重 | ✅ 成立，五处冲突 | 04 全面更新 |

### 后六条（约束收紧 + 事实纠正，已改）

| # | 问题 | 核查 | 修正 |
| --- | --- | --- | --- |
| 5 | 01-B2 仍称利润凹、荐三分/黄金分割，否定 00/03 的多峰修正 | ✅ 成立 | 01-B2 改为"目标错误+多峰粗采样" |
| 6 | Moe bin 修复需**原子替换**语义："重同步前 clear()"若入 `sync_active_bins_batch` 会让后批清前批 | ✅ 成立（同步是分批多次调用） | 01-R6 改为临时 snapshot + 显式删零 bin + 全成功才原子替换 |
| 7 | "加载附近 tick"仍可能假报价，需 coverage invariant | ✅ 成立 | 01-R3 加 `IncompleteState`/钳 max_input |
| 8 | 按"池子集合"去重不正确，不同顺序/方向是不同路径 | ✅ 成立 | 01-B3/04 改为有序 `(pool,in,out)` hop 序列 + 先证明重复来源 |
| 9 | `.gitignore` 与 submodule **无阻断性冲突**：forge-std 已是 gitlink，ignore 不阻止 init | ✅ 已实测：mode 160000、`git check-ignore` 返回空 | 01-A2、05-C 改为"非失败原因" |
| 10 | "未声明的根 examples 无法编译为 example"**错误**：Cargo 自动发现 | ✅ 已实测 `cargo metadata`：8 个全 AUTO-DISCOVERED | 05-D 改为"隐式 target，需实际删/移" |

### 小问题（已改）
- **`eth_call` vs `eth_estimateGas` 分工**：`eth_call` 做语义 preflight（会不会 revert），`eth_estimateGas` 定 gas。第一版混为一谈 → README P0-3、04-F10 已区分
- **50 gwei 是观测值非新常量**：应带块高/时间，正确做法是每块动态读 → README 已改措辞
- **revm 非"完全一致"**：state/block env 不完整时仍偏差，只能说"更高保真"；`eth_call` 也只证明所选 RPC 状态下的执行结果，不保证实际 inclusion → 03 已改
- **00 写"6 处修正"但实列约 10 项** → 已改为"约 10 项"
- **分支 main vs dev**：SHA 相同，统一为 `dev`（43efa87） → README 已改

### 第二轮未发现新的 P0 反例
复审确认核心 8 条 P0 证据无新反例，剩余是跨文档清理 + 约束收紧，已全部落地。

---

## 第三轮review的核查与修正

第三轮从"文档一致性"深入到"实现契约"层面，指出 9 条（4 类 P1 会直接影响实现正确性）。逐条对代码核查，**全部属实并采纳**。核查用了 `cargo metadata` 和活链查询（block `98121659`，2026-07-18T11:20:30Z，hash `0xc44bbf683065f56d14cfa1abf20fbdafdcc39e45e8071ea30e603a95228469ed`，gasLimit 60M，baseFee 50 gwei；两个 RPC 返回一致）。

| # | 问题 | 核查 | 修正落点 |
| --- | --- | --- | --- |
| 1 | **`MarketSnapshot` 只是版本标签，不是一致性协议**：需所有读取固定同一 block hash、number/hash/parent 链式校验（替换块无块号缺口，单靠缺口回补抓不到）、原子发布、分叉即停 | ✅ 成立 | 02 sync 模块新增"一致性协议"节；README P1-1 |
| 2 | **Moe 原子替换未解决未知覆盖区间**：`simulate_swap_precise`（`moe/mod.rs:1502-1569`）在 boundary/visited/`MAX_ITERATIONS` break 后仍 `Ok(amount_out)` 返回**部分输出** | ✅ 已读码确认 | 01-R6 加 `MoeSnapshot{...,queried_ranges,block_hash,block_timestamp}` + `IncompleteState` |
| 3 | **V3 coverage invariant 需更精确**：区分"word 已同步为零"vs"未同步"；`agni/mod.rs:270-274` 在 `initialized==true` 但 tick record 缺失时 `map_or(0,..)` **静默用 liquidity_net=0** | ✅ 已读码确认 | 01-R3 加强不变式 |
| 4 | **R7/R8 执行生命周期不完整**：appearance 抑制应彻底移除（不是改"连续 N 块"）；latest-wins 管不了在途交易（worker `pending_tx.watch().await?` @:1113 阻塞到确认），需发现/发送/receipt 分离的状态机 | ✅ 已读码确认 | 01-R7/R8 重写；README P2-2 |
| 5 | **gas 方案跨文档冲突**：01-B8 仍把静态表当真值、02 仍写"统一 gas 表"、01-R1 把 eth_call 和 estimate 合写"校准 gas" | ✅ 成立 | 01-R1/B8、02 统一为"estimate 定 gas / eth_call 语义 preflight / 静态表仅 fallback 且受区块 limit 硬约束" |
| 6 | **02 路径语义仍旧**：仍写死 WMNT 闭环、按"有序池子集合"去重 | ✅ 成立 | 02 engine 改 `settlement_asset` + 有序 hop 序列 |
| 7 | **第二轮声称清掉的措辞仍在**：00:49 仍写"只停 Moe"为现行；README 影子模式循环依赖；03 "无公开 gas 竞价" + "黄金分割"；04 eth_call="真能成交"；50 gwei 无块号 | ✅ 逐条成立 | 00 第三节标注"已被取代"；README 影子模式移到 P2.5 验证门；03:5/82、04:31 已改；补块号 |
| 8 | **增量重估漏非池状态依赖**：base fee/优先费/余额也改净利润与排序 | ✅ 成立 | 03-P1 加"毛报价缓存 + 每块按费用/余额重评分"两层 |
| 9 | **examples 清理小残留**：隐式 target 无声明可摘；Cargo 会发现 `<name>/main.rs`；游离数需重算 | ✅ 已 `cargo metadata` 核实 | 05-D 改准自动发现规则 |

### 第三轮未发现新的 P0 反例
核心 8 条 P0 依然成立。本轮补齐的是三个实现契约——**快照一致性、protocol coverage（V3 tick / Moe bin 都要 `IncompleteState` 而非部分报价）、执行状态机**——这些是从 spec 走向实现前必须钉死的。

---

## 最终轮review的核查与直接修正

最终轮不再增加新的运行级 P0；重点是把第三轮契约写成可实施的不变式，并删除仍在生效正文中的事实错误。已直接修正：

1. **快照身份和分叉判定**：`SnapshotId` 加 `chain_id + block_number + block_hash` 和 header timestamp；正常推进校验 number/parent，完全相同的 `(number,hash)` 重复通知幂等忽略，同高度不同 hash、回退高度或下一块 parent 不匹配均 fail closed。状态调用按 block hash 固定，logs 用 block-hash filter；RPC 不支持 hash pin 时采用读前/读后 hash 复核，不宣称 `.block(...)` 适用于 logs。
2. **Moe 时间上下文**：快照携带 block timestamp，报价不得继续把 `time_of_last_update` 当模拟时间。
3. **V3 coverage 范围**：不变式同时适用于 Agni/UniswapV3 的 mutable/immutable 模拟入口，不能等 P3 合并共享内核后再修。
4. **结算资产边界**：`settlement_asset` 属策略层，但当前 executor 与 gas 成本单位都绑定 WMNT；本部署必须校验 `settlement_asset == executor.WMNT`。若未来泛化，需同时改合约并定义 native gas 到结算资产的换算。
5. **执行前门禁**：candidate 携带 snapshot id 与 pool-universe/manifest identity；发送前任一版本失效即丢弃。余额调整后的最终 calldata 使用 owner `from` 和明确的 latest/pending 策略做 `eth_call`/`eth_estimateGas`；deadline 必须进入合约 ABI，不能只靠本地 pending timeout。
6. **增量重估边界**：base fee/priority 变化可对缓存的逐输入报价廉价重评分；余额上限或输入相关 gas 改变可行最优点时必须重新寻优，不能只重评分一个最优样本。
7. **恢复与清理事实**：checkpoint 加 chain/block/schema/config/pool-universe/coverage 校验；修正 Agni adapter、Moe `pair_parameters`、`data/contracts/moe`、过时 target 和测试清理的错误描述。

最终安全/目标复审又补了四个执行边界：旧 executor 只能逐 ERC-20 提现且没有 native MNT withdrawal，不能笼统声称“全部余额归零”；checkpoint 需要原子落盘、内容完整性和明确本地信任模型；gas fallback 只处理 transport/能力错误，不能吞 execution revert；`PathOptimizer` 仍被 mock 调用，必须先迁移 mock 再删。以上已同步到 README/01/02/04/05/06。

`06-milestones-and-issues.md` 在最终复审期间加入后又做了一次契约级对齐：影子模式移到 M2 的 P2.5 门，M3-4 改为完整性校验 checkpoint；M0 撤资区分 ERC-20 与不可从旧 ABI 提走的 native MNT，并拆成“先撤资/永久退役 M0-1 → replacement provenance M0-8”；M1 使用完整 snapshot identity/header/readiness 和两套 V3 mutable/immutable coverage；M2 固定 exact-final-request preflight、链上 deadline、nonce intent/attempt、receipt canonical finality/reorg 和 pending cancel；M3 增加 post-merge signerless gate；M4 删除不存在的 P8 映射，并补齐性能 P1–P5、mock optimizer 迁移和 Mantle sequencing 待核实事项。M0 的 build/test gate（M0-4）明确要求 `cargo test --all-targets`，其他 issue 按各自作用域给出验收，不再过度声称每张 issue 都重复该命令。

最终五路复核还补了三个跨模块风险：四个 active service 在 P3 合并前必须全部接入 P0–P2 修复或显式退役；executor 下一跳不能把中间 token 的合约总余额当 input，必须使用本 hop balance delta 并保持预存库存不减；production signer 从 P0 保持禁用到 P2.5 人工 gate，merged binary 还需单独 post-merge gate。并发新增的 `TODOs.md` 已纳入：离线生成静态、版本化的 pool/path topology manifest，但 pool state/gas 仍每块动态；candidate manifest 与 executor security allowlist 严格分层，周期脚本不得自动扩大链上信任。

终审期间又补齐了四类跨文档契约：P2.5 显式阻塞于 Moe 本金保护、V3 实时状态、余额上限、appearance 移除、WS 缺口回补和完整 P2；所有 M3 工程（除文档明确的两项非发送例外）只在 M2-8 **approve** 后启动；snapshot id 之外，`pool_universe_fingerprint/manifest_version`（或 readiness epoch）也进入 candidate/preflight/send identity，manifest 热切换会清空旧候选；四个入口的实际 gas API 现状改准为“仅 1559 使用 `compute_fee_plan`，其余直接使用静态 schedule”。

`TODOs.md` 随后新增 Mantle Sepolia 可复用 E2E 要求，也已纳入 F15/M2-7，使该冻结版本的规划增至 **44 个 issue**。旧 `execute_sepolia_arbitrage` 与历史硬编码地址仍按 A3 视为失效材料；新 gate 使用专用测试网 signer、幂等 bootstrap + 独立 trigger、带 codehash/owner/tx 的 deployment manifest，并验证真实 pipeline 到 canonical-finalized receipt 与 settlement/gas/PnL 对账。M2-7 通过后才进入 signerless M2-8，production signer 隔离不因此放宽。

### 上一冻结版本最终门禁（已被本轮判断性修订取代）

五路只读复核均基于同一冻结版本（`06` SHA256
`2341ab666b117cd3172479c3c1459b2889dc0e9da85dd890d0085dd8e834f066`，`TODOs`
SHA256 `bbe56fe38b4069b722bb336d7cf61e08524ea7630a50c2f95b21a2fef9ee237e`）完成：

| 门禁 | 结果 | 核查重点 |
| --- | --- | --- |
| Goal / scope | PASS | 两项 TODO、P0–P4/P2.5、该冻结版本的 44 个 issue 全覆盖，无遗漏或过度声称 |
| Implementation quality | PASS | 源码事实、state+topology identity、active entrypoints、gas/nonce/finality/E2E 契约可实施 |
| Docs QA | PASS | 9 份文档链接/围栏通过；44 个 index/body/title 对齐；84 条依赖边双向一致、无未知 id |
| Security | PASS | executor/allowlist/资金门禁完整；Sepolia key/network/address 与 production 强隔离 |
| Cross-doc context | PASS | README/00–06/TODOs 顺序、标签、依赖、旧 target 清理与新 E2E 替代关系一致 |

这里的 PASS 只表示**文档审计与实施契约闭合**，不表示源码问题已修复。已知
`--all-targets`/test 失败、合约漏洞和运行时 P0 仍须按 issue 实施与验证。

---

## Fable 5 判断性 review：采纳边界

Fable 5 对最终规划提出 5 条速度/严谨度取舍。本轮重新对照源码和路线图后，处理如下：

1. **不采纳 P0/P1 后默认开放主网 canary**。P0/P1 之后仍缺 per-hop delta/deadline、完整 exact-request preflight、nonce/finality、差分测试和真实 E2E；极小仓位只能限制某一笔经济损失，不能证明状态/执行链正确，也不能替代 signer 放行证据。production send 仍由 P2.5 人工 gate 控制。但采纳其“关键路径不应被任意日历时间拖长”的部分：M2-8 改为运行前定义 block/runtime/候选/preflight 样本与错误预算，按证据阈值完成，不把固定 1–2 周当作通过条件。
2. **采纳双 shadow 过重的意见，但不删除第二道 signerless gate**。M3-9 改为 deterministic replay equivalence 为主，只对跨协议路径、并发调度、manifest 集成和 intentional delta 补风险相称的 shadow 样本；可复用 M2-7/M2-8 证据，不默认再跑完整 1–2 周。
3. **采纳 manifest 不应阻塞开始合并**。M3-1 可先用逐行 provenance 校验、启动后冻结并发布 universe fingerprint 的 legacy CSV adapter 完成四服务合并；M3-10 随后通过统一 `PoolUniverseSource` 替换它，并继续阻塞 M3-9/merged signer 放行。这样缩短去重构的起跑路径，但不牺牲最终 topology/provenance 契约。
4. **部分采纳 UniV3 范围意见**。源码确认 UniV3 `init` 会同步 bitmap/tick，`simulate_swap_mut` 也会写回 sqrt price/tick/liquidity，因此不能把 Agni 的 tickless startup/no-op mutable bug推广给 UniV3。但两套模拟都存在 `initialized==true` 且 tick record 缺失时静默使用 `liquidity_net=0` 的 fallback；所以 M1-3 改成“Agni fix + UniV3 verify/harden”，M2-4 继续做两套实现的精确差分与 fail-closed 验证。
5. **采纳编号意见**：E2E 改为 M2-7，signerless P2.5 gate 改为 M2-8；所有索引、正文、依赖图和跨文档引用同步对调。

上述变更使上一冻结版本的 hash/门禁只保留为历史证据；本轮完成后以新的冻结 hash 和五路复核为准。

本轮修改仅涉及 `specs/*.md`，没有修改源码。

---

## Fable 5 反馈后的最终门禁

五路只读复核绑定到代码基线
`43efa87c1035b0c5638f092be2b268fac7af45a2`，以及按文件名排序后对每个
`filename + NUL + content + NUL` 取摘要得到的规范 bundle SHA256：
`4f75d12e087b3206bbaf3531d2b2018ce767d3255f61a8afaf7087d960e855c1`。

| 门禁 | 结果 | 核查重点 |
| --- | --- | --- |
| Goal / scope | PASS | 五项判断性反馈均按采纳边界落地；未把未采纳的 P0/P1 主网 canary 写回路线图 |
| Docs QA | PASS | 9 份文档、该冻结版本的 44 个 issue、85 条依赖边双向一致；链接、锚点和代码围栏通过 |
| Implementation quality | PASS | Agni/UniV3 边界、`mock.rs` optimizer 调用和 native MNT 合约事实与源码一致 |
| Security | PASS | M2-8/M3-9 signer 门、证据阈值、冻结 legacy adapter 和 native MNT 退役边界未被削弱 |
| Cross-doc context | PASS | README、00–06、TODOs 的编号、顺序、依赖和历史/现行语境一致 |

最终判断：**文档门禁通过，无阻断项**。这里的 PASS 只证明规划和实施契约闭合；
源码中的 P0/P1 问题仍未实施修复，production signer 仍须按 M2-8/M3-9 的适用范围
保持禁用。

---

## WHI-502 latency 设计修订（2026-07-19）

用户重新审视套利热路径后，明确要求“先做数据分析生成 gas profile，再修改运行时”，
不接受每笔交易串行 `eth_call + eth_estimateGas` 作为 P0 gas sizing 主路径。此前 review 中
“estimate 定 gas / 静态表 fallback”的结论因此保留为历史审计过程，**已被本节和当前
README/01/02/04/06/TODOs 的 measured-profile 方案取代**。

修订前做了两组 Mantle 主网只读抽样：最新连续 101 个区块的 base fee 均为 50 gwei、
block gas limit 均为 60M；另在 `97,158,262–98,158,262` 间隔抽样 21 个点，结果相同。
这支持“当前参数稳定”的分析前提，但不能证明永久协议常量，所以 runtime 仍从块入口已收到
的 current header 构建 `BlockFeeContext`，只是禁止 per-candidate header RPC。

Linear 新增 **M0-9 / WHI-546**（measured gas-profile dataset + deterministic generator），
原生阻塞 **M0-2 / WHI-502**。M0-9 用 canonical receipt + hash-pinned fork/replay 产出按
executor code hash、ordered protocols、hop 和必要 tick/bin crossing bucket 版本化的
profile；`gas_limit` 与 `expected_gas_used` 分离，并要求 p50/p95/p99/max、holdout、margin
policy、样本量和 digest。WHI-502 只负责启动时验证加载与 O(1) 内存查表；未知/证据不足/
版本不匹配 fail closed，production gas-sizing 不调用 `eth_estimateGas`。M2-3 的 exact-request
`eth_call` 仍是独立语义风险门，其延迟必须单独测量，不能再伪装成 gas sizing 的必要成本。

截至该次 WHI-502 修订，规划当时为 **45 个非重复 issue、86 条原生依赖边**；新增映射为 `M0-9→WHI-546`。随后合约/部署时序修订的最终计数见下一节。

---

## 合约 review 与部署时序修订（2026-07-19）

复核 `arbitrage-analysis/CONTRACT_REVIEW.md`、`BASE_BOT_REVERSE_ENGINEERING.md` 与当前
Solidity/Rust 调用方后，采纳了 callback provenance、SafeTransfer、角色分权、交易局部
balance delta、显式 `minProfit/deadline` 和移除 `_expectedStates/_validateStates`。不采纳
“V3 先把 input 转给 pool 再由 callback 支付”的描述：Uniswap V3 pool 的 swap 语义是先发
output、再在 callback 内收 input；正确优化是严格鉴权 callback，不是预转账。Base 单个样本的
库存/调用模式也不能直接决定 Mantle 的资金模型；本项目继续用有硬上限的 executor resident
WMNT inventory，以避免每笔资金往返增加热路径延迟。

issue 边界同步调整如下：

1. **M0-8 / WHI-501 只构建，不部署。** 它交付 final source、ABI、Rust callers、安全测试、
   reproducible optimized bytecode/codehash；callback、角色、SafeTransfer、provenance、delta、
   `minProfit/deadline` 全部在这个最终合约 issue 闭合。
2. 原 **M2-5 / WHI-523** 的有效范围已经完全并入 WHI-501，避免先做一个临时 ABI、随后再
   破坏 gas profile 和调用方；Linear 将其标为 Duplicate。
3. **M2-7 / WHI-525 是首次网络部署**，只在 credential-isolated Mantle Sepolia E2E 使用
   exact M0-8 artifact。M2-8 shadow approve 不直接部署或注资。
4. 新增 **M2-9** 人工 runbook：只有 M2-8 approve 后才能在 Mantle mainnet 部署一次 exact
   qualified artifact，核验 codehash/ABI/角色/配置，限额注资并进入受断路器保护的 canary。
5. **M2-3 / WHI-521 改为 risk-tiered preflight**：E2E/shadow、新 codehash/未完整建模 venue、
   canary 强制；已有证据且人工批准的稳定 production profile 可抽样或关闭，从而保持零
   `eth_estimateGas` 且允许零 preflight RPC 的最低延迟热路径。

本节取代上文“P0 replacement 重部署/重新注资”和“所有 production send 必须 preflight”的
历史措辞；最终 issue 数、Linear id 与依赖边数以 `06` 的最后一次回读记录为准。

上一轮 Linear 同步完成时的回读（已被下节部署拆分取代）：新增主网部署 runbook 为 **M2-9 / WHI-547**，仅被
WHI-526 approve 阻塞；WHI-523 已标 Duplicate 并只保留 `duplicateOf WHI-501`。项目现有
48 条记录，其中 45 条非重复（44 open/backlog + WHI-500 Done）、3 条 Duplicate，原生
`blocks` 图有 85 条唯一边，所有依赖 endpoint 均在项目内。

---

## Fable 5 部署拆分与 per-hop 复核（2026-07-19）

对 Fable 5 的五点后续意见逐条验证后，处理边界如下：

1. **采纳 WHI-547 风险阶段拆分。** 原 WHI-547 虽按步骤先核验再注资，但同一 issue 不能
   用原生依赖表达“部署完成后必须再次人工停顿”。M2-9/WHI-547 因而收窄为主网一次部署、
   codehash/ABI/角色/配置核验，强制终态 `paused + unfunded`；新增 M2-10，在重新核验 M2-9
   证据并记录第二次 go/no-go 后才允许限额 WMNT 注资和一笔强制 preflight 的 canary。
2. **确认不恢复通用 per-hop min-out。** 对 provenance 已验证的 canonical pool，交易原子性
   意味着任何最终 `minProfit` 失败都会回滚全部 hop；trade-local delta 另行保证预存中间
   token 不被当作本次 input。因此这三项保护库存，逐跳 bound 的新增价值主要是提前失败、
   可能减少后续 gas。M0-8 继续保留 venue ABI 强制或语义已证明的原生 bound，但不增加
   通用数组；测试同时覆盖 adverse 中间 hop 最终失败的全量回滚，以及跨跳输出漂移但最终
   盈利仍应成功，避免 stale hop bound 制造 false reject。
3. **不改 M0 milestone 名称。** milestone 名称是简短主题，不是验收清单；当前 description
   和 `Success =` 已明确包含 final replacement qualification，改名不会改变依赖或风险边界。
4. **澄清 canary 没有推翻旧决策。** 旧决策否决的是 P0/P1 后绕过 P2.5 直接主网 canary；
   当前 canary 位于 Sepolia E2E、signerless shadow approve、主网 paused/unfunded 核验和第二次
   人工批准之后，是已通过门禁入口的第一个受限生产阶段。
5. **不改 WHI-523 标题/milestone。** `Duplicate` 状态、零 blocks/blockedBy 和
   `duplicateOf WHI-501` 是 Linear 的规范生命周期表达；保留原标题能保存历史意图并便于
   搜索，增加自定义 `[DUP]` 前缀只会破坏统一标题约定。

本节取代上一小节把部署、注资和 canary 全归到 WHI-547 的描述；最终新 issue id、计数和
依赖边数以 `06` 的最后一次 Linear 回读为准。

Linear 同步后的最终状态：M2-9 继续使用 **WHI-547**，标题/正文已收窄为 unfunded
deploy-and-verify；M2-10 创建为 **WHI-548**，仅 blocked by WHI-547。项目现有 49 条记录，
其中 46 条非重复（45 open/backlog + WHI-500 Done）、3 条 Duplicate，原生 `blocks` 图为
86 条唯一边；M0/M1/M2/M3/M4 的非重复 issue 分布为 9/9/9/10/9。WHI-523 仍保持零
blocks/blockedBy，所有依赖 endpoint 均在本项目。
