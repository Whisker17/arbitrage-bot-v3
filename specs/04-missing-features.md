# 04 · 功能缺失清单（对照生产级套利机器人）

> **审阅说明**：本文件第一版基本是静态视角，与 00/01/README 的运行级修正有多处冲突，后续已更新 nonce 生命周期、分叉检测、动态 gas、私有通道证据边界、结算资产和路径去重。梯队排序保留作"功能维度"参考，但**实际执行顺序以 [README](README.md) 路线图为准**。

按"没有它就会实打实亏钱/错过钱"到"锦上添花"排序（功能维度；执行顺序见 README）。

## 第一梯队：不做就是在裸奔

### F1. 提交前预模拟（pre-flight simulation）
**现状**：完全没有。off-chain 算出正利润就直接 `send()`。
**问题**：off-chain 数学和链上有偏差（见 03 revm 段），"模拟盈利、上链 revert"是常态。证据块 `98121659` 的 base fee=50 gwei、gas limit=60M（见 01-R1）；这不是固定常量，但足以说明 revert 成本必须按动态链参数评估，预模拟拦截很有价值。
**做法**：M0-8 先删除链上的 `_validateStates`（`.sol:177`），用 provenance、交易局部 delta、显式 `minProfit/deadline` 构成实际不变量。余额调整、完整重模拟和最终 calldata 构造完成后，M2-3 再按风险等级决定是否用真实 authorized hot executor `from` 和相同 `to/data/value` 对 `pending`/`latest` 做一次 `eth_call`。E2E/shadow、新 codehash/未完整建模 venue 和 canary 强制；稳定 production profile 只有在有延迟/结果证据和人工批准后才可抽样或关闭。preflight 能免费拦截所选状态下必然 revert 的请求，但不保证 inclusion 或成交。

**分阶段边界**：P0 gas sizing 由 M0-8 final codehash → WHI-546 measured profile → WHI-502 current fee context 独立完成，不再为了 estimate fallback 强制引入 `eth_call`。P2/F1 再增加完整 `SnapshotId`、head 复核、pending/latest 能力策略、结果审计和发送状态机门禁；远程语义检查的延迟必须单独测量，并允许合格低风险热路径在批准后达到零 preflight RPC。

### F2. 断路器 / kill switch
**现状**：没有。旧合约只有一个 `onlyOwner` 同时承担执行与提款，没有 guardian/pause；运行时也无自动保护。
**做法**：
- 连续 N 次 revert → 暂停发单一段时间（可能是状态同步坏了或有人在针对你）
- 单位时间最大亏损超阈值 → 停机告警；按 canonical-included success/revert/cancel receipt 的实际 gas + realized trade delta 统计，同 nonce 未 inclusion 的 losing attempt 只记状态、不虚构 gas，也不重复计费
- 单笔最大仓位硬上限（防寻优/数据异常算出天量输入）
- M0-8 先做 cold admin、revocable hot executor、可选 pause-only guardian 分权：hot signer 只能执行，不能提款、改信任或 unpause；guardian 可暂停但不能恢复；只有 cold admin 可配置角色/信任、提款和 unpause
- 一个经过强鉴权、可审计、默认 fail-closed 的本地 pause 控制，并可请求 guardian/on-chain pause；不能把未鉴权的远程开关变成新的停机攻击面
- pause 时停止新 nonce reserve/sign/broadcast，并对所有可替换的 pending nonce 触发 best-effort cancel；cancel 失败时仍由链上 deadline 兜底

### F3. PnL 账本 / 持久化
**现状**：只往 CSV 追加日志（monitor.rs:129-320），没有已实现盈亏台账、没有 DB、重启后状态全丢。
**做法**：按 nonce intent + 每个 tx attempt/hash 记录发出、replacement、included、canonical finalized/revert、gas 和实际 settlement delta；只有达到配置确认/finality 策略且 receipt block hash 仍 canonical 的结果才进入 realized PnL。对账 executor token 变化和 sender native gas；reorg 回退未 finalized 记录。

### F4. 影子模式 / dry-run
**现状**：没有开关，要么发单要么不跑。
**[已修正] 前置条件**：影子模式必须在 P1 的完整 `MarketSnapshot` 一致性协议（hash-pinned 读取、number/parent/hash 分叉检测、原子发布、缺口回补）和 P2 的 candidate/preflight 流程就绪后再跑。否则它统计的是可能已漂移或内部不自洽的状态，得出的机会数/利润分布会误导决策；对应 README 的 P2.5 验证门。
**做法**：一个 `--dry-run` 模式，走完发现→模拟→定仓位→最终 calldata→`eth_call` preflight 全流程，但在签名之前由硬边界截断；影子进程不持有 signer/发送能力。把 full snapshot id + pool-universe/manifest identity、完整有序路径、最终请求摘要、preflight 状态/结果、预期利润和 header timestamp 记进账本。production signer 从 P0 起保持禁用，只有人工 go/no-go 通过才可为已验证入口恢复。运行前定义最小 block/时长/候选/preflight 样本和错误预算，以证据阈值而非固定 1–2 周判定完成。P3 merged binary 仍需独立 signerless requalification，但以 replay equivalence 为主，并只按重构风险补足必要 shadow 样本，不自动重复首轮完整窗口。回答三个问题：
1. 每天发现多少个正利润机会？利润分布？
2. 这些机会里有多少能通过 `eth_call` 预模拟？**注意**：`eth_call` 通过只证明"在所选 RPC 的那一刻状态下可执行、不 revert"，**不等于真能成交**（不保证实际 inclusion，也不保证你抢得过别人）。它是"排除必然失败"，不是"保证成功"
3. 机会从出现到消失（被别人吃掉/价格回归）的存活时间——决定你的延迟够不够快

**用这些数据决定要不要继续投 P3/P4 的工程量。** 这是最省钱的决策方式。

## 第二梯队：稳定运营需要

### F5. Nonce 生命周期管理
**现状**：`NonceManager` 写好了但没接线。靠 alloy 默认每笔查链。当前单 worker 串行时同块并发 nonce 竞争不会发生，所以它不是最初的 P0；但 R8 拆成异步发送 + receipt tracker 后，nonce 生命周期立即成为 P2 执行状态机的前置依赖。
**做法**：评估现有 `NonceManager` 后复用或替换；实现本地 nonce intent 预留。一个 intent 可追踪多个同 nonce attempt/hash；replacement 不是终态，cancel 只有取消 tx canonical finalized 才终态。处理 `nonce too low`、original/replacement 竞速、dropped tx、重启 resync 和 receipt reorg。

### F6. Stuck-tx 处理 / gas 替换
**现状**：不是"发出去就不管"，而是发送 worker 会阻塞在 `pending_tx.watch().await?` 直到确认；但没有超时、替换、取消或链上 deadline，既堵住后续机会，也不能阻止旧交易晚打包。
**做法**：receipt tracker 与发送分离；pending 超时后按同 nonce替换或取消。每次 replacement 都绑定最新 `BlockFeeContext` 与兼容 gas profile，重建 exact request，按 M2-3 策略重新 preflight，并受最新净利润/亏损预算约束；禁止回到 production `eth_estimateGas`。跟踪全部 hash。receipt 先 `included_unconfirmed`，核对 block hash canonical 并达到配置确认/finality 策略才终态；reorg 后重开 intent。executor ABI 增加 deadline 并在链上校验 `block.timestamp`；本地 timeout 不能替代链上 deadline。

### F7. 缺口回补 + reorg 恢复
**现状**：WS 丢块无回补（D3），reorg 处理死的（D1）。
**[已修正] 检测和 fail-closed 属 P1，完整 unwind 才属 P3**：正常推进同时验证 `number == last+1` 和 `parent_hash == last.hash`；完全相同的 `(number,hash)` 重复通知幂等忽略，同高度不同 hash、回退高度或下一块 parent 不匹配视为分叉，立即停报价并重同步。不能用"中心化 sequencer 下 reorg 罕见"作为推迟检测的依据。
**做法**：WS 重连或块号跳跃时补齐 header 链，逐块验证 number/hash/parent，再按 block hash 拉 logs；任一不一致不发布快照。完整增量 unwind 可后置，但检测与停止执行不可后置。

### F8. 指标 / 可观测性
**现状**：只有 tracing 日志 + CSV。
**做法**：Prometheus 或至少结构化指标：每块处理耗时、发现机会数、发单数、成功率、平均延迟、当前余额、累计 gas。跑实盘时没有这些就是盲飞。

### F9. 重启快速恢复（checkpoint）
**现状**：`Serialize/Deserialize` 都 derive 了，但没有 checkpoint 代码（state_space/mod.rs:88 TODO）。每次重启要全量重扫所有池的 tick/bin 数据，Mantle getLogs 限速下可能要几分钟。
**做法**：checkpoint 至少持久化 `chain_id + block_number + block_hash`、schema 版本、影响行为的 config fingerprint、pool-universe fingerprint，以及 V3 word/Moe queried-range coverage，并把这些元数据和完整 pools/ticks/bins payload 一起纳入内容 digest。

- 写入采用同目录临时文件 → flush/fsync 文件 → 原子 rename → 必要时 fsync 目录；崩溃时只接受最后一个完整 checkpoint
- 反序列化采用严格 schema 和池/tick/bin/文件大小上限，拒绝未知/重复字段、越界 coverage 和 digest 不匹配
- 明确信任模型：可信本地盘至少用 digest 检测损坏；若要防本机文件被恶意修改，使用独立密钥的 MAC/签名和最小文件权限，不能让攻击者同时改 payload 与裸 checksum
- 恢复时先校验 block hash 仍 canonical，再用链上读取验证会影响报价的 slot0/coverage 边界；无法建立足够链上信任就丢弃 checkpoint 全量同步。恢复内容在验证完成前不得发布为可报价 snapshot

### F10. measured gas profile / 当前 block fee / 优先费策略 — **[已修正] 这是 P0，不是第三梯队**
**现状**：全硬编码，且硬编码值与证据块 `98121659` 的 gas 参数不兼容，**会让交易发不出去**（R1）——这不是"提升竞争力"的优化，是阻断级故障。`MANTLE_BASE_FEE_WEI=20_000_000`（executor.rs:64）比该块 base fee 低 2500 倍；证据值不是新常量。
**做法**：先由 M0-9/WHI-546 对 canonical receipts 与 hash-pinned fork/replay 做可复现分析，按 executor code hash、ordered protocol、hop 和必要的 tick/bin crossing bucket 生成版本化 profile；每个 key 分开给出 `expected_gas_used` 与有长尾/holdout 证据的 `gas_limit`。M0-2/WHI-502 启动时验证并加载，生产 gas-sizing 热路径只查内存，未知 profile fail closed，不调用 `eth_estimateGas`。`baseFeePerGas`/block gas limit 从块入口缓存的当前 header 读取；`max_fee` 必须覆盖当前 base fee，优先费是否影响排序要等 F12 核实，未取得证据前使用保守可配置策略。

## 第三梯队：提升竞争力

### F11. 更多 DEX / 更长路径
**现状**：只有 Agni V2/V3 + Moe LB，路径 `MAX_HOPS` 有限。
**做法**：其他 DEX 接入能扩大机会面，但候选名单必须在实施时按当前 canonical deployment、流动性/成交量、可验证 ABI/factory 和可执行性重新调研，不在本 spec 里把历史名称当作仍活跃的事实。先让现有三个协议通过 P2.5，再决定是否扩面。

### F12. 私有提交通道
**现状 [已修正为待核实]**：提交走普通 provider `send()`。但"Mantle 生产网当前的实际排序机制、以及是否提供私有/优先通道"**缺乏权威证据**——官方能查到的是 fair-sequencing 研究提案，不等于已上线。
**做法**：先**核实** Mantle 当前 sequencer 的实际行为（是否 FCFS、是否有 priority gas auction、是否有私有通道），再决定是否需要私有提交。在核实前不假设"经典 MEV 抢跑弱所以优先级低"。

### F13. 闪电贷（免注资）
**现状**：executor 用自有库存（`.sol:161` 要求合约已持有 `_amountIn` WMNT），需要 `FundExecutor` 预注资。
**做法**：接闪电贷后无需锁定自有资金、仓位不受注资额限制（也就顺带解决 B1）。但闪电贷费会吃掉薄利套利的利润，且增加复杂度和 gas。**权衡**：小资金起步时闪电贷解放仓位上限有价值；但先确认 B1 修好后自有库存模式能否稳定盈利，再决定是否上闪电贷。

### F14. 版本化静态 pool/path manifest（热路径不做 topology discovery）
**现状**：各 service 读取各自 CSV，Moe 甚至缺文件后回退到错误协议；没有跨协议 canonical factory 枚举、≤3-hop settlement cycle 生成、TVL/depth policy、deterministic diff 或原子 reload 契约。

**做法**：离线周期脚本按一个 canonical block/factory set 枚举并验证 pools，输出带 chain/header/factory/config/policy fingerprint 与 digest 的确定性 pool + 有序 path manifest。TVL 必须记录 decimals/估值源/版本/时间，并辅以标准 probe size 的可执行深度/price impact。运行时只静态化拓扑，pool state/balance/base fee 仍每块动态；promotion 使 snapshot readiness 失效并对新 universe 全量同步/原子重建。自动发现 manifest 只是候选交易 universe，不能直接更新 executor 的 owner-controlled 安全 allowlist。详见 `TODOs.md` 与 06-M3-10。

## P2 验证基础设施（不是第三梯队优化）

### F15. 可复用 Mantle Sepolia 全流程 E2E 环境

**现状**：仓库虽有 Sepolia 部署/触发脚本和旧指南，但 `execute_sepolia_arbitrage`
使用过时 API、当前不能编译；历史硬编码 DEX 地址也没有当前 provenance/codehash 复核。
因此不能把旧材料视为已经存在的 E2E。

**做法**：在 M2 建一个独立、credentialed testnet gate。用专用 Sepolia key 和强制
chain-id/mainnet 隔离；一次性幂等 bootstrap 测试 token、至少两个可组成 WMNT 闭环的
production-compatible venue/pool、初始流动性、**M0-8 exact hardened executor** 与测试库存，并输出含
address/deployment tx/runtime codehash/owner/schema/config fingerprint 的 deployment manifest。
公开 canonical 部署若在实施时可验证就复用，否则部署 repo-owned protocol fixture 并明确
验证边界。独立 trigger 脚本可重复 swap 制造价差；真实 bot 必须走完 gas-profile lookup +
current fee context、exact-request preflight、broadcast、canonical-finalized receipt 和 settlement/gas/PnL 对账。
live testnet 用显式/定时凭证门运行，普通 CI 用 deterministic offline counterpart。详见
`TODOs.md` 与 06-M2-7。这是 replacement 的首次网络部署；M0-8 不部署。M2-8 shadow
approve 之后先由 M2-9 人工 runbook 完成主网部署/角色核验并停在 paused + unfunded；
第二次人工 go/no-go 后，M2-10 才执行限额 WMNT 注资和强制 preflight 的单笔 canary。

## 一个功能层面的产品决策

`tech-docs/Agni/TODO.md` 记录的两条你自己的意图，建议正式纳入本次迭代：
1. **限定 WMNT 为唯一 input/output**——服务侧已经做了（WMNT 起止过滤）。起止资产归属策略层，做成 `settlement_asset` 配置项，不要污染通用 `amms` 库；但当前 executor 用 immutable `WMNT` 做投入/最终余额检查，gas 也以 MNT wei 计，因此本部署必须校验 `settlement_asset == executor.WMNT == wrapped native gas asset`。支持其他资产需要同时泛化合约和 gas 成本换算。
2. **路径去重**——**[已修正] 去重 key 必须保序保方向**（有序 `(pool, token_in, token_out)` hop 序列），不能按无序"池子集合"去重（同组池的不同顺序/方向是不同经济路径）。闭环只做旋转归一化；settlement asset 固定起点后通常连旋转都不需要。**先打点证明"大量重复"的真实来源**再动手（见 01-B3）。

这两条不是可选优化，是让日志可读、让利润数字有意义的前提。路径约束还必须由 final calldata builder 和链上 executor 双重执行：首尾等于部署结算资产，每个 pool 的 token pair 与有序 hop 一致，并通过 factory-derived 地址或 canonical allowlist 验证 pool provenance；当前合约只检查数组长度，不足以构成该不变式。
