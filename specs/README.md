# Specs 总览：Mantle 套利机器人恢复与迭代计划

> 生成日期：2026-07-18。基于对 `dev` 分支（commit `43efa87`）全量代码的审计。
> 所有 `file:line` 引用以该 commit 为准。

## 这套文档的结构

| 文档 | 内容 |
| --- | --- |
| [00-review-response.md](00-review-response.md) | **外部 review 核查记录**：逐条验证 GPT-5.6 review、修正了哪些结论 |
| [01-current-issues.md](01-current-issues.md) | 当前问题诊断：运行级阻断 + 正确性/安全/稳健性 |
| [02-architecture-refactor.md](02-architecture-refactor.md) | 设计缺陷与重构方案 |
| [03-performance.md](03-performance.md) | 性能问题与优化建议 |
| [04-missing-features.md](04-missing-features.md) | 对照生产级套利机器人的功能缺失清单 |
| [05-cleanup.md](05-cleanup.md) | 可删除的死代码/垃圾文件完整清单 |
| [06-milestones-and-issues.md](06-milestones-and-issues.md) | 将最终路线图拆成可写入 Linear 的 milestone / issue 草案 |
| [TODOs.md](TODOs.md) | 用户新增方向及 review 后契约：静态 pool/path manifest，以及可复用 Mantle Sepolia E2E 环境 |

> **审阅说明**：本套文档已经过多轮外部 review（GPT-5.6）核查并修正。最重要的新增是"运行级阻断"和实现契约——第一版偏静态代码审计，漏掉了若干让机器人"跑起来也空转/亏钱"的运行时故障，也没有封死快照一致性、协议状态覆盖和执行生命周期。核查全过程和修正清单见 [00-review-response.md](00-review-response.md)。

## 一句话现状

`cargo build` / `cargo check --lib` / 四个服务各自的 `cargo check --example` **都能过**；全量 target / test 仍不绿：Cargo.toml 有 9 个失效 example 路径，修完后还有 4 个过时 target 编译失败，库测试另有一个常量移位溢出。所以核心生产入口能编译，但不能把仓库描述成全量构建/测试通过。

**真正让机器人不工作的是一批运行时故障**，头号是 gas 配置：代码按 Mantle **旧 gas 参数**（gas limit 十亿级、base fee 近零）硬编码，而两个 RPC 在 2026-07-18T11:20:30Z 对同一历史块返回一致结果：**block 98121659, hash `0xc44bbf683065f56d14cfa1abf20fbdafdcc39e45e8071ea30e603a95228469ed`, gasLimit=60M、baseFee=50 gwei**。这是可复查的单块**观测值**，不是新的固定常量；正确做法是每块动态读取，见 P0-3。代码设的单笔 gas limit 超过该块上限、写死的 base fee 比该观测值低 2500 倍、费用上限低于该块 base fee——在该链参数下交易根本进不了块。这极可能就是"突然不工作"的直接原因。此外还有 V3 用启动时旧状态报价、Moe 执行无本金保护等（详见 01 第 0 节），以及一个与协议无关的资金安全隐患（executor 合约回调鉴权过弱）。

## 必须先理解的一个事实：库和服务是"两个世界"

这个仓库是 `amms-rs` 的 fork。真正的机器人**不在 `src/` 里，而在 `examples/` 里**——四个 1000~2000 行的 `*_monitor_executor_service` 文件才是生产入口：

- `examples/protocols/agni/v3_monitor_executor_service_1559.rs`（最新旗舰，Agni V3 + EIP-1559）
- `examples/protocols/agni/v3_monitor_executor_service.rs`（旧版，与上面 ~90% 相同）
- `examples/protocols/agni/v2_monitor_executor_service.rs`（Agni V2）
- `examples/protocols/moe/moe_monitor_executor_service.rs`（Merchant Moe LB）

这些服务**只用了库的一小部分**（AMM 模拟、`build_graph`、`PathFinder`；只有 1559 服务使用 `compute_fee_plan`，其余服务仍直接使用静态 gas schedule），并且**绕过**了库里的大量基础设施：

> **Active-entrypoint invariant**：在 P3 合并完成前，上述四个 service 都是生产行为面。P0–P2 的每个修复必须覆盖所有相关入口；不迁移的入口必须从部署清单和 Cargo target 中显式退役。不能只修 1559 服务，却用 milestone 完成状态掩盖另外三个仍在使用旧 gas/旧状态/阻塞 worker 的入口。

| 库中的组件 | 服务是否使用 | 状态 |
| --- | --- | --- |
| `StateSpaceManager`（订阅 + reorg 处理） | ❌ 服务自己裸写块循环 | 本身也是坏的（reorg 逻辑永不触发） |
| `PathOptimizer`（仓位寻优） | ❌ 服务自己写了爬山搜索 | 库版算法本身就是错的 |
| `ArbitrageMonitor`（机会扫描） | ❌ | 依赖坏掉的 PathOptimizer |
| `Executor`（执行封装） | ❌ 服务直接调合约 | 内部自相矛盾，用了必 revert |
| `NonceManager` | ❌ | 从未接线 |
| `execution/gas.rs` | ❌ | 已被注释弃用 |

**这是本次迭代最重要的架构决策点**：要么把服务里的好逻辑下沉进库、让服务变薄；要么承认库的这些组件已死、直接删除。维持现状（两套并行、一套是死的）是最差选择。详见 02。

## 迭代路线图（v2，经外部 review 重排）

第一版路线图把几个运行级致命项排晚了。新顺序把"资金安全 + gas + 状态新鲜度"提到最前，"合并/寻优优化"压到最后。

### P0 — 止血：资金安全优先，其次恢复能发单
1. **暂停所有真实执行并清理旧 executor 资产**。停本地 sender 不等于停掉链上 callback，漏洞仍可被外部调用：先立即提走已知高价值 token（尤其 WMNT），同时按资金/交易历史、已知 path token 和 Transfer 记录补全 ERC-20 inventory，再逐个 `withdraw(token)` 并核对余额；另查 native MNT。当前合约能接收 MNT 却没有 native withdrawal：若余额非零，该地址无法达到“全部余额归零”，必须永久停用；replacement 增加 cold-admin-only native withdrawal。撤资/永久退役与 replacement 实现是顺序依赖，但 **M0 不部署 replacement**
2. **完成最终 replacement executor 工件**：revocable hot executor、cold admin 和可选 pause-only guardian 分权；callback 按 selector/factory/init-code/pool key 鉴权；完整有序路径、方向、pool provenance 和 `poolType` 一致性校验；中间资产按本交易 balance delta 传递；所有 ERC-20 使用兼容 empty-return 的安全 transfer；ABI 删除 `_expectedStates/_validateStates`，改为显式 `minProfit + deadline`，最终统一检查 `balanceAfter >= balanceBefore + minProfit`。不新增通用 per-hop min-out 数组：在 canonical pool 的原子交易中，provenance + trade-local delta + final `minProfit` 已保护库存，逐跳 bound 仅是可选 early-abort/gas 优化；venue ABI 强制或语义已证明的原生 bound 仍保留。Moe 在余额缩小时必须完整重模拟，不能继续用全零 `_amountsOut` 或只改 input
3. **先冻结最终 bytecode，再测量并替换四个 active service 的 gas 模型**：M0-8/WHI-501 产出最终优化 runtime codehash/ABI；M0-9/WHI-546 只用该工件的 canonical receipts 与 hash-pinned fork/replay 生成版本化 measured profile，分别给出有长尾/holdout 证据的 `gas_limit` 和用于利润评估的 `expected_gas_used`；M0-2/WHI-502 启动时按 chain/executor code hash/schema/digest 验证并加载，生产 gas-sizing 热路径只做 O(1) 内存查表，未知 profile fail closed，不调用 `eth_estimateGas`。`baseFeePerGas`/block gas limit 从块入口缓存的当前 header 读取，不增加 per-candidate RPC；`max_fee` 覆盖当前 base fee，priority 策略在 F12 核实前保持保守可配置。M2 的 risk-tiered exact-request `eth_call` 是独立语义风险门，不负责定 gas。**这是恢复发单的前提**
4. 修 `Cargo.toml` 9 个失效 example 路径；删除/迁移或修复 4 个过时 target；修库测试溢出（`uint256x256_math.rs:185`）；`git submodule update --init`（forge-std 是 gitlink，gitignore 不阻塞初始化，只是有点混淆）
5. 从 canonical Moe factory 生成并验证专用 `data/poolLists_moe.csv`，删除回退 Agni CSV 的路径；M3 再由跨协议版本化 manifest 取代这个止血文件。提交 `Cargo.lock`（现在被 ignore，`alloy 1.0.25` 实际解析成 1.8.3）；精确 pin Rust/Solidity/Foundry 工具链并由 CI 校验

**部署与生产发送保持禁用**：M0-8 只交付 undeployed artifact。M2-7 的 Sepolia E2E 才是首次网络部署；P2.5 人工 go/no-go 的 approve 只解锁 M2-9，由它在主网部署/核验 exact codehash/ABI/角色后停在 paused + unfunded。M2-9 证据必须再经过第二次人工 go/no-go，M2-10 才能限额 WMNT 注资并执行一笔强制 preflight 的 canary。任何 P3 合并后的新 binary 仍必须重新通过无 signer 的 post-merge shadow gate，不能继承旧入口的 signer 资格。

### P1 — 状态新鲜度 + 快照一致性：让报价基于**当前且自洽**的链状态
1. **`MarketSnapshot` 一致性协议**（不只是版本标签，见 02 sync 模块）：快照身份包含 `chain_id + block_number + block_hash`，一个快照内所有读取固定到同一 canonical block hash；正常推进时同时校验 `number == last+1` 和 `parent_hash == last.hash`；完全相同的 `(number,hash)` 重复通知幂等忽略，同高度不同 hash、回退高度或下一块 parent 不匹配则按分叉处理。对外状态必须显式区分 `Ready(snapshot)` 与 `Syncing/Halted`；新 head 组装失败、回补或分叉时立即撤销发送资格，不能继续拿“上一个好快照”报价
2. V3 模拟改读实时 `pools`、接线已有倒排索引（R2）：倒排索引只允许跳过**未受影响路径的 AMM 毛报价重算**；base fee/priority 策略每块仍要更新净利润，余额边界变化可能要求重新寻优。在 P4 缓存优化落地前，宁可全量重评分/重算也不能复用错误的旧净利润
3. **Agni 修复、UniswapV3 验证并加固**精确 coverage invariant：已证实的启动清空 tick 是 Agni `init_basic` 专属；UniswapV3 的 full init 会同步 bitmap/tick，mutable 路径也会提交 post-swap 状态，不把它们误报成同类 bug。但两套实现都在 `initialized==true` 且 tick record 缺失时静默使用 `liquidity_net=0`，因此 mutable/immutable 入口都必须验证“word 已同步且为零”与“word 未同步”、缺 record/跨 coverage 返回 `IncompleteState`（R3）
4. 修复 Agni mutable swap 的 no-op 状态迁移（B7/M1-9）：成功时精确提交 post-swap sqrt price/tick/liquidity，失败或 coverage 不完整时不产生部分 mutation；在 M2 差分测试和 P3 shared-core 重构前完成
5. Moe 用 `MoeSnapshot{slot0,bins,queried_ranges,block_hash,block_timestamp}`：slot0 与 bins 一起原子提交；越出 `queried_ranges` 或结束仍有 `amount_left` 返回 `IncompleteState`；报价使用 snapshot header timestamp，不返回部分报价（R6/B4/B6）
6. 验证并取消"故意落后一块"（R4）
7. WS 断连重连 + 块号连续性 + 缺口回补（抓跳块，与 parent_hash 互补）
8. **彻底移除 appearance-based 执行抑制**（R7）：出现次数不再是资格条件；继续执行快照/coverage、新鲜净利润、余额与仓位、cooldown、断路器、deadline 和 exact preflight 等对应阶段已经定义的状态/经济/风险门禁。失败黑名单区分永久/临时（临时带 TTL）

### P2 — 执行状态机 + 版本化流程 + 差分测试
1. 基于 P1 快照的 `MarketSnapshot → Candidate → Preflight → Submit` 流程；执行身份必须同时包含完整 snapshot id 与 `pool_universe_fingerprint/manifest_version`（或等价单调 readiness epoch），发送前任一身份已不是当前 `Ready` 值就丢弃并重算
2. **执行状态机**（R8）：发现/发送/receipt 跟踪三者分离（不再阻塞 `watch`）；deadline 进入合约 ABI 并在链上校验。nonce intent 可有多个 tx attempt/hash，replacement 不是终态、cancel 只有取消交易 canonical finalized 才终态；每次替换绑定最新 fee context 与兼容 gas profile、重建 exact request、按 M2-3 策略 preflight，并受最新净利润/亏损预算约束。receipt 先进入 `included_unconfirmed`，确认其 block hash 仍 canonical 并达到配置的确认/finality 策略后才 `finalized`；reorg 时重开并重同步 nonce/余额
3. 在余额调整、完整重模拟和最终 calldata 构造完成后，按风险等级决定是否对 exact request 执行一次 `eth_call`：E2E/shadow、新 codehash 或未完整建模 venue、production canary 强制执行；稳定且已有证据的 production profile 经人工批准后可以抽样或关闭。gas sizing 始终使用 WHI-546/WHI-502 的已验证 profile，禁止 `eth_estimateGas`；preflight 通过只证明所选 `pending`/`latest` 状态下不 revert，不保证实际 inclusion
4. AMM Rust 模拟 vs 固定区块链上 `eth_call` 的**差分测试**：V2 rounding / V3 跨 tick / Moe 跨 bin
5. 断路器：连续 revert 熔断、单位时间最大亏损上限
6. 建立可复用的 Mantle Sepolia E2E gate：这是 M0-8 replacement 的**首次网络部署**；专用测试网 signer 与 mainnet 强隔离，幂等 bootstrap 测试 token/至少两个 production-compatible pool/hardened executor，独立 trigger swap 重复制造价差；真实 bot 必须完成 gas-profile lookup + current fee context、强制 exact-request preflight、broadcast、canonical-finalized receipt 与 settlement/gas/PnL 对账。现有失效 `execute_sepolia_arbitrage` 和旧硬编码部署指南不能算完成（F15/M2-7）

### P2.5 — 影子验证门

在 P1/P2（包括独立测试网 signer 完成的 Mantle Sepolia E2E）就绪后进入 signerless 影子模式，验证快照连续性、preflight 通过率、机会寿命和利润分布。运行前先定义最小 canonical block/运行时长、候选与 preflight 样本、连续性/coverage 错误预算；达到证据阈值才交人工 go/no-go，**不把固定 1–2 周本身当成通过条件**。approve 只解锁 M2-9 的主网部署/核验，终态必须 paused + unfunded；第二次人工批准后 M2-10 才能限额注资和 canary。reject 保持 signer 禁用并阻塞两步上线链。这里的 canary 是 shadow 后的首个受限生产阶段，**不是**此前已否决的“P0/P1 后绕过 P2.5 直接 canary”。是否继续投入 P3/P4 是同一证据的另一个决策，但 M3 merged binary 不继承既有入口的放行。

例外只有不影响生产行为的两项：降低 dev profile 优化级别，以及核实 Mantle sequencer/priority/private submission 的证据研究；它们可提前进行，但不得改变 production fee policy 或获得 signer。

### P3 — 架构合并 + 稳健性
1. 先把四个服务合并为一个多协议 binary，**配置允许同时启用 V2/V3/Moe**。合并阶段可读取经过 provenance 校验、启动后冻结的 legacy CSV adapter，但它必须生成 pool-universe fingerprint 并禁止热更新；因此 M3-10 不再阻塞“开始合并”
2. 再接入版本化静态 pool/path manifest（canonical factory、≤3-hop settlement cycles、TVL+可执行深度、deterministic diff），替换 legacy adapter；运行时只静态化拓扑，池状态/gas 仍每块更新。manifest promotion 触发 readiness 失效和全量原子重建，且绝不自动扩大 executor allowlist。**M3-10 仍阻塞 post-merge signer 放行**，只是不会拖住消除四份重复代码
3. Agni 与 UniV3 共享 concentrated-liquidity 内核，但**各自保留 event/ABI adapter**（不是共用同一 struct）
4. 完整 reorg unwinding；PnL 账本、指标、告警
5. 完整性校验 checkpoint：原子落盘、受限反序列化、canonical block + 链上关键状态复核；未验证的恢复状态不得发布为可报价快照
6. 合并后以 deterministic replay equivalence 为主门禁，再按变更风险补一个无 signer 的 post-merge shadow 样本；可复用 M2-7/M2-8 的 fixture、阈值和证据，不默认重复完整 1–2 周。涉及路径语义、并发调度或协议组合的新行为必须获得对应 shadow 样本；merged binary 过门前不得获得 production signer/send capability

### P4 — 按数据决定的性能优化
通过 P2.5 影子验证门后，再跑 replay benchmark 拿 p50/p95/p99 延迟，决定是否做：增量重估细化（费用变化重评分、余额边界变化重寻优，见 03）、path→pool index 热路径缓存、Moe tree_math、多峰寻优（非凹，不能纯黄金分割）、revm CacheDB、reorg delta。**不要在没有机会与延迟数据前投入这些。**

## 关于"能不能赚钱"的现实提醒

代码修好只是必要条件。Mantle 是中心化 sequencer、约 2 秒出块，套利竞争本质是**延迟竞赛**。P2 就绪后进入 P2.5 影子验证门（只模拟不下单），统计机会数量、模拟利润分布和机会寿命，用数据决定是否继续投入 P3/P4。影子模式依赖 P1/P2 的快照一致性和 preflight 才有可信数据，04 文档里有对应设计。

> Mantle 当前 sequencer 的实际排序机制（是否 FCFS、是否有 gas 竞价/私有通道）**尚待核实**，不预设"没有公开 gas 竞价"（见 04-F12）。
