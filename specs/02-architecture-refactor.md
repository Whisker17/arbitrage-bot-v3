# 02 · 架构缺陷与重构方案

> **v2**：经外部 review 修正了两处（Agni/UniV3 合并粒度应保留各自 adapter；WMNT 起止归属策略层而非库层），文中标 `[已按 review 修正]`。详见 [00-review-response.md](00-review-response.md)。

## 核心矛盾：库和服务是两套并行系统，其中一套是死的

这是整个仓库最大的设计问题。最终路线图已选择下面的方案 A；B/C 仅保留为审计时讨论过的历史备选，不再留给实现者二次选择。

### 现状图

```
src/ (库，"amms-rs" 的骨架)          examples/*_monitor_executor_service (真实机器人)
├── state_space/StateSpaceManager    自己裸写 subscribe_blocks + getLogs 块循环
│     订阅+reorg (❌ reorg 逻辑死的)   （不用 StateSpaceManager）
├── arbitrage/PathOptimizer          自己写 best_path_simulation 爬山
│     仓位寻优 (❌ 算法错的)          （不用 PathOptimizer）
├── arbitrage/ArbitrageMonitor       自己写块内扫描
│     机会扫描 (依赖坏的 optimizer)   （不用 ArbitrageMonitor）
├── execution/Executor               直接调 IArbitrageExecutor.executeArbitrage
│     执行封装 (❌ 自相矛盾必 revert)  （四个服务都不用 Executor）
├── execution/NonceManager (❌ 从未接线)
├── execution/gas.rs (❌ 已注释弃用)
│
└── 服务真正用到的：AMM::simulate_swap / build_graph / PathFinder；gas API 使用不统一
```

服务共同借用了库的"AMM 数学 + 图 + 寻路"；费用路径并不统一：只有 1559 V3
service 使用 `compute_fee_plan`，旧 V3、V2 和 Moe 直接调用静态
`gas_limit_for_hops`。其余流程全部自己重写。结果是：
- 库里一半代码是死的，还带着 bug，谁读谁误解
- 四个服务各自把"块循环 / 配置解析 / CSV 加载 / 执行 worker"复制了一遍（~5600 行，重复率极高）
- 想改一个行为要在 4 个文件里改 4 次

### 已决策：采用方案 A

**方案 A（已选择）：库变薄，服务下沉。**
把服务里被反复重写的逻辑（块循环、寻优、执行流程）下沉进库、逐个修好其中的 P0，删掉库里对应的死组件（`StateSpaceManager` 订阅循环、`PathOptimizer`、`ArbitrageMonitor`、旧 `Executor`、`gas.rs`）。`NonceManager` 不应一边列为删除、一边又要求接线：先评估其 API 是否满足新执行状态机，能复用就重构接线，不满足就由新实现替换后再删。四个服务塌缩成**一个** `src/bin/bot.rs`，通过 `Protocol` trait 支持**同时启用**多个协议（V2+V3+Moe 一起跑，才可能发现真正的跨 DEX 套利；不是单选其一）。库暴露：
- `PoolSyncer`（块订阅 + 连续性检查 + 缺口回补 + 简化 reorg）
- `PathEngine`（build_graph + find_cycles + 保序去重 + **多峰寻优**，见下）
- `Executor`（fee plan + 预模拟 + 提交 + nonce）

**方案 B（未选择，仅留历史）：承认库骨架是死的，只保留数学。**
如果不想动服务，那就把库**砍到只剩 AMM 数学 + batch-request 同步**（`amms/`、`state_space/filters` 里在用的部分），把 `arbitrage/`、`execution/`、`state_space/mod.rs` 里没被服务引用的全删。服务保持独立但至少抽出一个 `common` 模块消除四份重复。

**方案 C（明确拒绝）：** 维持现状。两套并行、一套是死的、四份重复。

> 选择 A **不是因为服务代码质量高**——服务本身有一堆 P0（R1~R8）。理由是：服务里那些**结构性决策**是对的（用自建块循环而非死掉的 StateSpaceManager、用有界搜索而非死掉的 binary-search optimizer、WMNT 起止过滤的意图），而库里对应的组件是从没跑起来的理论实现且带 bug。方案 A 是"以服务的结构为骨架、把里面的 P0 逐个修好、再下沉进库"，不是"照搬服务代码"。

**合并前的硬约束**：README 列出的四个 `*_monitor_executor_service` 都是活跃行为面。P0–P2 的修复必须覆盖每个相关入口；若某入口不迁移，必须从部署清单和 Cargo target 中显式退役，不能等到 P3 合并时再补。任何 milestone 的验收都以“全部 active entrypoint 已修或已退役”为准。

---

## 重构目标架构（方案 A）

```
src/
├── amms/           # 保留。AMM 数学 + 同步。见下方各协议清理
│   ├── amm.rs      # AutomatedMarketMaker trait + enum dispatch（保留，设计好）
│   ├── uniswap_v2, uniswap_v3
│   ├── agni/       # 保留协议 adapter；与 uniswap_v3 共享数学内核（见下）
│   └── moe/        # 保留 math，删死代码，修 bins/定价/费率
│
├── sync/           # 新（从 state_space + 服务块循环提炼）
│   ├── syncer.rs   # 块订阅 + 缺口回补 + reorg（真正接线 latest_block）
│   ├── cache.rs    # 保留 StateChangeCache，扩大 CAP，深 reorg 返 Err 不 panic
│   └── filters/    # 保留 blacklist/whitelist，删或修 ValueFilter
│
├── engine/         # 新（从 arbitrage + 服务扫描提炼）
│   ├── graph.rs    # build_graph（保留）
│   ├── pathfind.rs # find_cycles + 去重（删 misprice 或修正语义）
│   ├── optimize.rs # 多峰寻优：粗采样→区间局部优化→端点检查（非凹，见 03）
│   └── simulate.rs # 逐跳 simulate_swap；起止资产由策略层 settlement_asset 配置
│
├── exec/           # 新（从 execution + 服务执行 worker 提炼）
│   ├── gas_profile.rs # 版本化 measured profile：安全 limit + 成本估值，热路径内存查表
│   ├── fee.rs      # 当前 block header fee context + checked EIP-1559 费用
│   ├── nonce.rs    # 真正接线的 nonce 管理
│   ├── state_machine.rs # candidate 门禁 + send/replace/cancel 状态转换
│   ├── receipt.rs  # 独立跟踪在途交易，不阻塞发现/发送
│   ├── submit.rs   # 最终 calldata + measured gas profile → semantic preflight → 提交
│   └── contract.rs # IArbitrageExecutor 绑定
│
├── config.rs       # 统一 env 解析（现在四个服务各写一份 from_env）
└── bin/
    └── bot.rs      # 唯一入口；--protocols 可多选（如 agni-v3,agni-v2,moe 同时启用）
```

---

## 各模块具体重构项

### AMM 模块

**Agni 与 UniswapV3 合并（最大的重构收益，~900 行逐字复制）**
`agni/mod.rs` 几乎逐字复制 `uniswap_v3/mod.rs`：`Info`、`CurrentState`、`StepComputations`、整个 `simulate_swap` tick 循环、`modify_position/update_position/update_tick/flip_tick`、`tick_to_word`、所有 `sync_*`。真正的差异只有：
- 多了个没用的 `fee_protocol` 字段
- `simulate_swap_mut` 是坏的（B7）
- 用 saturating 算术（应改 checked）
- `flip_tick` 传 `self.tick_spacing`
- getLogs 窗口大小

**做法 [已按 review 修正]**：把 UniswapV3 的 tick-pool 数学核心抽成共享内核（泛型 `TickPool<Params>` 或内部 crate module），**但 Agni 和 UniV3 各自保留自己的 event 解码和 ABI adapter**。不要"Agni 直接复用同一个 UniV3 struct"——那会掩盖真实差异（Agni 的 Swap 事件字段、`protocolFeesToken0/1`、factory/initcode、log 窗口都和 UniV3 不同）。正确的分层是：共享 tick 穿越/流动性数学，不共享事件与链上接口。删掉的是 Agni 里逐字复制的数学，不是它的协议适配层。

共享内核属于 P3，但 coverage 修复不能等到 P3：先在 Agni 和 UniswapV3 的 mutable/immutable 模拟入口都执行同一不变式——未同步 bitmap word 返回 `IncompleteState`，且 `initialized == true` 必须存在对应 tick record。合并后再由共享内核统一承载该检查。

**Moe 模块（B4/B5/B6）**
- `calculate_price` 改用 `active_id`（现成的 `get_price_from_id` 在 :340）
- 确认 bins 同步节奏（服务侧调了 `sync_active_bins_batch`，但每报价前是否 fresh？）
- 修或删常量乘积回退（费率量纲错 + 储备不更新）——建议 bins 缺失时直接跳过该池
- 把 `tree_math`（正确的找下一个非空 bin）接进 bin 遍历，替换现在的 ±1 线性扫 512 次（moe/mod.rs:1502）
- `moe/mod.rs:40-71` 的 `calc_*` 当前零调用，可在差分测试保护下删除。`pair_parameters::Parameters` 虽与 `math/pair_parameter_helper.rs` 相似，但它已 alias 为 `MoeParameters` 并被精确模拟调用，不能按"重复实现"直接删除；若要收敛两套参数代码，先选择目标实现、迁移调用并通过 Moe 差分测试

### Sync 模块（从 state_space 提炼）

**核心：`MarketSnapshot` 是一致性协议，不只是版本标签。** 快照身份至少包含 `chain_id + block_number + block_hash`，header 上下文包含 `parent_hash + timestamp`；必须保证一个快照里所有数据来自**同一个 canonical block**，并原子发布。这是 P1/P2 的实现契约（不是可以推迟到 P3 的 reorg unwinding）：

1. **所有读取固定到同一 block hash**：slot0/tick/bin 等状态调用使用 EIP-1898 block hash（RPC 支持时带 `requireCanonical`）；logs 使用 `blockHash` filter，不能把状态调用的 `.block(...)` 写法套到 logs。若某 RPC 不支持 hash-pinned state call，退化方案必须把每个 state call 固定到目标 **block number**，并在读取前后重取该高度 hash、要求始终等于 `h`，否则整轮失败；绝不能在复核 hash 的同时让中间 call 读取 `latest`
2. **完整链式判定**：正常推进必须同时满足 `number == last.number + 1` 和 `parent_hash == last.hash`。完全相同的 `(number,hash)` 重复通知幂等忽略；收到同高度不同 hash、`number < last.number`、或下一块 parent 不等于 last hash，都按分叉处理。只比块号抓不到替换块；只写 parent_hash 也不能完整描述直接收到同高度替换通知的情形
3. **原子发布 + readiness**：对外暴露 `SnapshotStatus::Ready(Arc<MarketSnapshot>) | Syncing | Halted(reason)`（或等价 readiness token），而不是只有一个永远保留旧值的 `watch<Arc<_>>`。任一读取/同步失败、新 head 正在组装、缺口回补或分叉时立即离开 `Ready` 并撤销 candidate/send 资格；旧好快照可以留作恢复基线，但不得继续报价。只有整轮成功才原子发布新的 `Ready(snapshot)`
4. **发现分叉立即停止报价 + 重同步**：任一 number/hash/parent 分叉条件成立就暂停发单、从分叉点重新拉取
5. **时间上下文属于快照**：Moe 费率/波动参数随时间演化，所有 Moe quote 必须使用 snapshot header 的 `timestamp`；当前把 `time_of_last_update` 当模拟时间的行为必须修掉

> 完整的 reorg **unwind**（用 `StateChangeCache` 回滚增量）可留 P3；但上面 1~4 的"快照一致性 + 分叉检测"是 P1/P2 的硬要求，因为没有它们，报价就可能建立在自相矛盾或已被 reorg 掉的状态上。

其余：
- **深 reorg 不 panic**（D2）：`cache.rs:47` 改成返回 `Err`，上层重新全量同步
- **扩大缓冲**：`CACHE_SIZE` 30 → 至少 128（Mantle 2 秒块，128 块 ≈ 4 分钟）
- **缺口回补**（D3）：块号跳跃时先按 header 链补齐 `[last+1, new]`，逐块验证 hash/parent 后再拉对应 block-hash logs；它与替换块/回退高度检测互补
- **统一 getLogs 策略**（D4）：Agni 90000 和 Moe 无界范围都改成可配置分块；遇 provider 的 range/result 限制时二分缩小并重试、记录 endpoint 能力。live snapshot 按 block hash 拉 logs；不要把代码注释里的 10000 当作链级永久安全值

### Engine 模块（从 arbitrage 提炼）

- **替换后再删 `PathOptimizer::optimize`**（B2）：先让服务和 `MockArbitrageContext` 都迁到新的多峰实现，并用 mock/replay + 差分测试验证；当前 `mock.rs:260` 仍调用旧 optimizer，不能一边保留 mock 一边先删它的依赖。**注意寻优算法不能假设严格凹/单峰**——见 03
- **misprice 语义**（B3）：要么删 `find_two_pool_misprices`，要么改成只产出以 `settlement_asset` 起止的闭环
- **路径去重**（对应你的 TODO）：去重 key 用**完整有序的 `(pool, token_in, token_out)` hop 序列**，不是"池子集合"（同组池的不同顺序/方向是不同经济路径，见 01-B3）；闭环只做旋转归一化
- **[已按 review 修正] 起止资产归属**：起止资产是**策略/结算配置**（一个 engine 层的 `settlement_asset`），**不要硬编码进通用 `amms` 库**。但当前 `ArbitrageExecutor` 用 immutable `WMNT` 做投入/最终余额检查，gas 也以 MNT wei 计价，所以本部署启动时必须验证 `settlement_asset == executor.WMNT == wrapped native gas asset`。若未来支持其他资产，必须同时泛化 executor，并为 gas 成本定义可靠的 native→settlement 换算
- **执行路径的双重校验**：final calldata builder 和 on-chain executor 都校验首尾等于部署的结算资产，并逐 hop 验证 token pair 与 pool provenance。token0/token1（Moe 为 tokenX/tokenY）相等只证明交易对，不证明 pool 可信；还必须由 factory + initcode 推导 canonical address，或命中 owner-controlled canonical pool allowlist
- **决策**：是否引入 revm CacheDB 做本地精确模拟（你 TODO 里记的方向）——见 03 性能篇

### Exec 模块（从 execution 提炼）

- **删死代码**：旧 `Executor` struct（内部 V2 数学 + V3 poolType 矛盾必 revert，B 系列没细说但 executor.rs:194 vs :285 是死的自相矛盾）和 `gas.rs`。`NonceManager` 按前述决策复用或替换，不在接线前先删
- **统一 gas 方案**（B8/R1）：M0-9/WHI-546 从 canonical receipt + hash-pinned fork/replay 生成按 executor code hash / ordered protocols / hop / tick-bin crossing 版本化的 measured profile，分别保存 `gas_limit` 与 `expected_gas_used`；M0-2/WHI-502 启动时验证并加载，生产热路径内存查表，未知 key/版本/digest fail closed。删除三套人工表和生产 `eth_estimateGas` gas-sizing；base fee/block limit 从块入口缓存的当前 header 取得
- **接线 nonce**（并入执行状态机，见 R8）；发送、receipt 跟踪分离，nonce 的每个在途状态及 replace/cancel 转换必须显式
- **replacement executor 契约**：M0-8 删除 `_expectedStates/_validateStates` 和复用 `_amountsOut[last]` 的隐式利润语义，ABI 改为显式 `minProfit + deadline`；强制 ordered path/provenance/poolType/方向一致，callback 按 selector + factory/init-code/pool key 鉴权；中间 token 只按本交易 delta 传递，ERC-20 使用兼容 empty-return 的安全 transfer。cold admin、revocable hot executor、可选 pause-only guardian 分权，hot signer 不能提款、改信任或 unpause。通用 per-hop min-out 不作为本金不变量：最终失败原子回滚，venue-native bound 仅在 ABI 强制/语义已证明时保留，跨 venue early-abort 必须由 gas 与 false-reject 数据驱动
- **发送门禁**：`Candidate` 携带 snapshot id、`pool_universe_fingerprint/manifest_version`、gas-profile identity 与 fee-context block identity；发送前任一身份已不是当前 `Ready` 值就丢弃。先按当前余额调整输入并完整重模拟，构造带显式 `minProfit/deadline` 的最终 calldata；gas limit 查已验证 profile，M2-3 再按 risk-tiered 策略决定是否对同一 request（authorized hot executor `from`、正确 `to/data/value`）做一次 `eth_call`
- **preflight 状态策略**：E2E/shadow、新 codehash/未完整建模 venue 与 canary 强制；稳定 production profile 只有在记录延迟/结果证据并经人工批准后才能抽样或关闭。需要时优先用可靠支持的 `pending`，否则明确 `latest`；它只证明所选 RPC 状态下不 revert，不保证 inclusion。定 gas 是 measured profile 的职责，语义检查是可选风险门；不通过 `eth_estimateGas` 串行耦合
- **链上 deadline/minProfit**：executor 在第一次外部调用前校验有限 deadline，结束时统一校验 `balanceAfter >= balanceBefore + minProfit`；本地 pending timeout 只用于触发同 nonce 替换/取消，不能使已广播 calldata 自动失效
- **nonce intent/attempt 模型**：一个 intent 可有多个同 nonce tx attempt/hash；replacement 不是终态，cancel 只有取消 tx canonical finalized 才终态。每次 replacement 使用当前 fee context 与 gas profile、重建 exact request，按 M2-3 风险策略 preflight，并重新通过净利润/亏损预算门禁；跟踪所有 hash，处理 original/replacement 竞速和重启 resync
- **部署边界**：M0-8 只冻结可复现的 final source/ABI/Rust callers/optimized bytecode；M0-9 对该 codehash 定标。M2-7 首次部署到隔离测试网，M2-8 approve 后只有独立人工 M2-9 可在主网部署/验证并停在 paused + unfunded；M2-9 证据经第二次人工 go/no-go 后，M2-10 才能限额注资和 canary。runtime 继续使用预存且有上限的 WMNT 库存，不为每笔交易增加 native funding 往返
- **receipt canonicality/finality**：`included_unconfirmed` 保存 receipt block hash；只有它仍 canonical 且达到配置的确认/finality 策略才进入 `finalized` 并计 realized PnL。reorg 后回退 intent，重同步 pending/latest nonce、余额和 snapshot；不能把首次 `mined` 当永久终态
- **kill switch 对在途交易的语义**：pause 后停止新 reserve/sign/broadcast，并对所有可替换 pending nonce触发 best-effort cancel；取消失败时仍由链上 deadline 兜底
- **删死配置字段**：`ExecutorConfig::global_fee_hard_cap_wei`、`fixed_gas_price_wei`（types.rs:46,57，声明了从不用）

### 配置统一

四个服务各写一份 `ServiceConfig::from_env`，且 env 变量名混乱（`RPC_WS_URL`/`MANTLE_WS_URL`、`EXECUTION_PRIVATE_KEY`/`PRIVATE_KEY`/`MANTLE_SEPOLIA_PRIVATE_KEY`、executor 地址有 4 个别名）。硬编码也散落：
- `WMNT_ADDRESS` 在 1559 和 moe 里各 `const` 一份，v2 里当 fallback，还带个硬编码 executor fallback `0x59E5…b80f`（v2:271,276）
- `chain_id=5000` 硬编码（execution/types.rs:62）

**做法**：一个 `config.rs`，统一 env 名（保留别名兼容），把链相关常量（WMNT、chain_id、factory 地址）收进一个 `MantleChain` 常量结构或从配置读。

### 静态 pool/path universe（来自 TODOs）

热路径不应每块重新做 factory discovery/图拓扑枚举。新增离线 generator，从配置的 canonical factories 枚举并验证 pools，构建最多 3-hop(依据 ARB_PATHS_MANTLE.md,2–3 跳=93.5%)、以 `settlement_asset` 闭环的完整有序 path union，输出版本化 manifest。manifest 包含 chain/block hash/header、factory/config fingerprint、TVL/可执行深度策略、确定性排序 payload 和 digest；相同输入产生稳定 diff。

运行时只把**拓扑**静态化：启动时验证 manifest 后构建 pool→path index；reserves/ticks/bins/balance/base fee 仍属于每块 `MarketSnapshot`。manifest identity 必须进入 `Ready`/candidate/send 的执行身份。promotion/reload 必须使 `SnapshotStatus` 离开 `Ready`、清空旧候选/队列，对新 universe 全量同步 coverage、原子重建 snapshot/path cache 后才能以新身份恢复。candidate discovery manifest 与 executor 安全 allowlist 是两套权限域：周期脚本只能产出待审核 candidate diff，不能自动授予 callback/提款相关的链上信任；若部署采用 allowlist，runtime 只启用 manifest 与 owner-controlled allowlist 的交集，未授权 pool 保留为不可执行候选或 fail closed。

M3 合并本身不必等待完整 generator：M3-1 可先通过一个只在启动时加载、逐行验证 provenance、发布稳定 universe fingerprint 且禁止热更新的 legacy CSV adapter 接入统一 `PoolUniverseSource`。M3-10 在 merged binary 上替换该 adapter、加入 deterministic generation/promotion；它不再阻塞开始合并，但在 M3-9 signerless requalification 和任何 merged-binary signer 放行前必须完成。

TVL 过滤需记录 decimals、估值源/版本/时间和阈值，并辅以标准 probe size 下的可执行深度/price impact；未知或不可验证估值 fail closed/进入人工审核，不用无来源 USD 数字决定安全或完整性。详见 `TODOs.md` 和 06-M3-10。

### 合约层

- **合并重复脚本目录**：`contracts/script/`（Sepolia）与 `contracts/executor/script/`（Mainnet）各有一份 `DeployArbitrageExecutor.s.sol` + `FundExecutor.s.sol`，只差网络目标。活的构建在 `contracts/executor/`（有自己的 foundry.toml，`src="."`）。`contracts/script/` 是残留，合并成一份用参数区分网络
- **回调鉴权加固**（C1）：`agniSwapCallback` 校验 pool 地址
- **删 `ArbitrageExecutor.sol_bak`**（旧版残留）

---

## trait 设计层面的小问题

- `amm.rs:16` `#[allow(async_fn_in_trait)]` 的 `init`——public trait 里的 async fn 会钉死 Send 保证，重构时考虑改 `BoxFuture` 或 `#[async_trait]`（虽然性能略差但更灵活）
- `AMM` 的 `Hash/PartialEq/Eq` 只按 `address()`（amm.rs:157）——这是对的（同地址即同池），但要在文档里写明，否则有人往 HashSet 里塞不同状态的同址池会困惑
- `Serialize/Deserialize` 全 derive 了但没有 checkpoint 代码（state_space/mod.rs:88 "TODO: create a checkpoint"）——要么实现 checkpoint（重启不用全量重扫，见 04），要么删 derive
