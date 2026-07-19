# 01 · 当前问题诊断

按"能否运行 → 结果是否可信 → 资金是否安全"分层。每条都有 `file:line` 和修复方向。

> **v2 更新（经外部 review 核查）**：本文件新增了第 0 节"运行级阻断"，那些是比原 A~E 更致命的**运行时**故障（gas 配置发不出交易、V3 用陈旧状态报价等），第一版静态审计漏掉了。核查过程见 [00-review-response.md](00-review-response.md)。原 A~E 中被修正的结论已就地标注 `[已修正]`。

---

## 0. 运行级阻断（P0-RUNTIME）—— 比 A 节更致命

这一节是"代码能编译、也能跑起来，但一跑就是空转/亏钱/发不出单"的运行时故障。全部经代码 + 活链验证（见 00 号文件）。

### R1. 🔴🔴 gas 配置让交易根本发不出去 —— 头号根因

**这极可能就是"机器人突然不工作"的直接原因。** 代码是在 Mantle **旧 gas 参数**（gas limit 十亿级、base fee 近零）下写的，但 2026-07-18T11:20:30Z 的证据块 `98121659`（hash `0xc44bbf683065f56d14cfa1abf20fbdafdcc39e45e8071ea30e603a95228469ed`）显示链参数已经不匹配这些假设：

| | 代码假设 | 证据块 `98121659` 的值 |
| --- | --- | --- |
| 区块 gas limit | 隐含十亿级 | **60,000,000**（60M） |
| base fee | `MANTLE_BASE_FEE_WEI=20_000_000`（0.02 gwei，`executor.rs:64`） | **50,000,000,000**（50 gwei，差 2500 倍） |
| 单笔 gas limit | 300M–2.8B（`gas_schedule.rs:8-14`） | **超区块上限，单笔就爆 60M** |
| max_fee 上限 | 小额利润钳到 500M wei = 0.5 gwei（`executor.rs:82`） | **远低于 50 gwei base fee → 交易被拒** |

三重致命：单笔 gas limit 超区块上限；写死的 base fee 比真实低 2500 倍；费用上限低于 base fee。任何一条都足以让交易进不了块。

**修复（先测量，再生成 profile，热路径只查内存）**：
- **M0-8 → M0-9 / WHI-501 → WHI-546 数据交付**：先冻结 final optimized replacement runtime codehash/ABI，再从该 exact 工件的 canonical receipt 与 hash-pinned fork/replay 收集不同 ordered protocol、hop 数、V3 tick crossing 和 Moe bin crossing 的 `gasUsed` 分布，生成带样本数、p50/p95/p99/max、holdout、margin policy 和 digest 的版本化 artifact。旧 executor 样本只可帮助设计 bucket，不能资格化新 codehash；`gas_limit`（执行安全上限）与 `expected_gas_used`（利润评估）必须分开，不能用“平均值乘固定比例”替代长尾证明
- **M0-2 / WHI-502 运行时交付**：启动时验证 chain id、executor code hash、schema/tool version、digest 与 route-key coverage，加载 profile 到内存；未知/证据不足/版本不匹配一律 fail closed。生产 gas-sizing 路径用 O(1) profile lookup，**不调用 `eth_estimateGas`，也不把 `eth_call` 当作定 gas 手段**
- **`BlockFeeContext`**：`baseFeePerGas` 与 block gas limit 仍来自当前 block header；复用块入口已收到的 header，若入口只有块号则每块最多补取一次并缓存，绝不在每个 candidate/签名前增加 header RPC。当前 50 gwei/60M 的稳定抽样是 profile 证据，不是永久常量
- **`eth_call`**：只属于 M2-3 的 exact-request 语义 preflight 风险门，与 gas sizing 解耦；是否启用会单独计入 block-to-submit 延迟，不得在 M0 gas 模块里串行绑定 `eth_call + eth_estimateGas`

这个 P0 修复必须覆盖四个 active service：1559 当前走 `compute_fee_plan`，旧 V3/V2/Moe 仍直接用静态 schedule。M0-9 先产出可复现证据和 generator，M0-2 再把共享 profile loader/lookup 与 block fee context 接到四个入口；不迁移的入口必须同时从部署清单和 Cargo target 退役。

这是恢复运行的**第一优先级**——不修这个，编译好了也发不出单。

> ⚠️ 连带影响：第一版报告里多处"Mantle gas limit 十亿级"的表述基于旧模型，已失效。B8"三套 gas 表不一致"仍然成立；统一解法是让人工常量表退出生产路径，由 WHI-546 生成版本化 measured profile、WHI-502 统一消费，见 B8。

### R2. 🔴 V3 服务用启动时的池状态报价（每块重算，但用旧状态）

`find_profitable_candidates`（service 1559 :920）收了实时更新的 `pools` 参数，但 :935 实际调 `pools_for_path(path, &path_cache.state_pools)`——`state_pools` 是 :323 **启动时** `state.state.values().cloned()` 的快照。`apply_logs` 每块更新的实时 `pools` 根本没进模拟。

旧 V3 service 在 :913 走同样的 `path_cache.state_pools`。因此修复范围是两个 active V3 入口；若旧 V3 不迁移，必须从部署清单和 Cargo target 显式退役，不能只修 1559 后宣布 R2 关闭。

所以第一版说的"每块重算全部路径"，真相是"**每块用启动时的旧状态**重算全部路径"——价格永远停在启动那一刻，算出来的套利全是幻觉。而且倒排索引 `pool_to_path_indices`（:270）已经建好了，V3 却没用（对应 cargo 警告 "field `pool_to_path_indices` is never read"）。

**修复**：让模拟读实时 `pools`；接线已有的倒排索引，每块只重算受影响路径。

### R3. 🔴 Agni 启动不同步历史 tick；两套 V3 都需 coverage fail-closed

服务 :806 用 `AgniPool::new(addr).init_basic(...)`，而 `init_basic` 在 `agni/mod.rs:397-398` 明确 `tick_bitmap.clear(); ticks.clear()`。池子建出来只有 slot0，没有任何 tick 流动性分布。任何跨越 initialized tick 的大额 swap 模拟都会用错流动性，叠加寻优 1e24 的搜索上限，很容易算出天量假利润。

**修复**：启动时同步 active tick 附近一段 tick 数据（服务已有 `sync_tick_bitmaps`/`sync_tick_data` batch，只是 `init_basic` 没调）。**但只加载"附近"不够，必须定义精确的 coverage invariant**，而且要区分 bitmap 的稀疏性：

- **"未加载" ≠ "已加载且为零"**：tick_bitmap 是稀疏结构。模拟跨 tick 时要能分辨"这个 word 已同步、确实没有 initialized tick"和"这个 word 根本没同步"。只有后者才是覆盖不足。
- **强不变式**（两套 V3 的模拟 fallback 都违反）：Agni `agni/mod.rs:270-274` 和 UniswapV3 `uniswap_v3/mod.rs:363-368` 都会在 bitmap 标记 initialized、tick record 却缺失时静默使用 `liquidity_net = 0`。`initialized == true` 就必须 `ticks.contains_key(tick_next)`，否则返回 `IncompleteState`。这里不把 UniV3 的初始化/状态提交误报成 Agni 同类故障：UniV3 `init` 已同步 bitmap/tick，`simulate_swap_mut` 也会写回 sqrt price/tick/liquidity；它的范围是先用测试证明现有同步完整性，再补 coverage metadata 与 fail-closed invariant。
- 模拟一旦要跨出"已同步 word"的范围，返回 `IncompleteState`（而不是静默假报价）；或把该池 `max_input` 严格钳到覆盖范围内。推荐前者（显式失败比静默钳位安全）。

这个不变式必须先覆盖 **Agni 和 UniswapV3、mutable 和 immutable 两类模拟入口**，不能等 P3 合并共享数学内核后才补；但实现工作应区分“Agni 已证实修复”与“UniV3 verify-only 后的定点加固”，不重写已经正常工作的 UniV3 mutable 状态迁移。

### R4. 🔴 所有服务故意落后一块

service 1559 :660 `target_number = number.saturating_sub(1)`——收到块 N 处理 N-1。Mantle ~2 秒一块，等于套利开跑前先自加约 2 秒延迟。延迟竞赛里这是致命的。

**修复**：验证能否直接处理 N（WS 推来的块的 logs 应当已可查），或改订阅 pending/logs。若当初落后一块是为了绕过某个"logs 还没 ready"的问题，要找到真实原因而不是靠延迟掩盖。

### R5. 🔴 Moe 执行无本金保护（推翻"最终检查保证不亏"）

Moe 服务 :1388 把 `amounts_out_with_slippage` 全设成 `U256::ZERO`；:1364 余额不足时 `adjusted = executor_balance` 直接当新 input 用，**不重新模拟**路径。合约最终检查（`.sol:169`）：

```
minExpected = balanceBefore - _amountIn + _amountsOut[last]
```

`_amountsOut[last]=0` 时退化成 `balanceAfter >= balanceBefore - _amountIn`——**整笔 input 亏光也能通过**。

对比：V3 服务 :1051 把 last 强制 `.max(candidate.input)`，所以 V3 有真实保护。**保护是分协议的：V3 有、Moe 没有。** 我原报告 C2/README 里"最终余额检查保证整体不亏"只对 V3 成立，对 Moe 是错的。

**修复**：不再延续旧 `_amountsOut[last]` hack。M0-8 的 replacement ABI 接收协议无关的显式 `minProfit`，统一检查 `balanceAfter >= balanceBefore + minProfit`；M0-3 在余额不足导致 input 改变后必须从新 input 完整重模拟、重算 gas/net profit 并重建 calldata，不盈利就放弃。这是 Moe 自身的恢复门禁；全局止血仍按 C1 在 callback/path 鉴权修复前暂停**所有协议**真实执行，不能理解成只停 Moe。

### R6. 🔴 Moe bin 重同步残留幽灵流动性

`sync_active_bins_batch`（`moe/mod.rs:525`）只在 `:596` `reserve_x>0 || reserve_y>0` 时 `insert`，**从不删除**链上已归零的旧 bin。一个 bin 被 swap 抽干后，即使重新拉取，内存里的旧非零值还在，继续参与报价 → 拿不存在的流动性算套利。

**修复（原子替换 + 覆盖区间，不能简单 `clear()`）**：Moe 的 bin 同步是**分批多次调用**的（服务里 `moe_monitor_executor_service.rs` 循环 `batch_idx` 挪动 `active_id` 偏移多次调 `sync_active_bins_batch`）。若把 `clear()` 直接塞进 `sync_active_bins_batch`，后一批会清掉前一批的结果。而且**光有 bins map 不够**——`MoeLbPair` 现在没有"哪些 bin 区间已完整查询"的信息，模拟器越出覆盖区或到 512 次上限时仍有 `amount_left` 也会**返回部分输出**（`moe/mod.rs:1502-1569`：boundary/visited/`MAX_ITERATIONS` 几处 `break` 后都 `Ok(amount_out)`，静默少报），和 V3 的 R3 是同一类问题。

正确做法是定义一个原子快照对象，把 slot0、bins、覆盖区间、块哈希绑在一起：
```
MoeSnapshot { slot0, bins, queried_ranges, block_hash, block_timestamp }
```
1. 一整轮同步累积到一个**临时 snapshot**（不就地改 `pair`）
2. 被查询、链上返回 0 的 bin 在 snapshot 里显式**删除**（覆盖"曾有流动性现归零"）；记录本轮真正查询到的 `queried_ranges`
3. **slot0 和 bins 必须一起提交**——不能像当前服务那样 slot0 失败还继续同步 bins
4. **所有 batch 成功后**才原子替换旧快照；任一失败整轮不提交
5. 模拟时：一旦要走出 `queried_ranges`，或结束时 `amount_left` 仍非零，返回 `IncompleteState`，**绝不返回部分报价**（部分报价 = 少算流动性 = 假利润）
6. 快照还必须携带证据块的 `timestamp` 并把它传给 Moe 报价；当前 `simulate_swap` 用 `time_of_last_update` 充当模拟时间（`moe/mod.rs:745-756`），会让随时间衰减的 fee 参数基于错误时刻计算

### R7. 🔴 候选生命周期逻辑永久错杀机会

`AppearanceTracker`（service 1559 :171）的 `entry.1 += 1` 只在 `entry.0 != block_number` 时累加——统计的是"**出现过的不同块数**"累计值，不是注释说的"连续出现"。超过 `MAX_APPEARANCES=3`（:63）就**永久过滤**。一个持续存在的真机会（正常套利路径会连续多块出现）反而在第 4 个块被永久拉黑。

雪上加霜：`:641-643` 任何执行 `Err`（余额不足、RPC 错误、发送失败）都 `mark_as_failed` 写进**永久黑名单** `FailedOpportunityStore`。一次临时余额不足就永久放弃这条路径。

**修复（appearance 抑制应彻底移除，不是改成"连续 N 块"）**：持续存在的机会**不是异常**——正常套利路径本来就会连续多块出现。用"出现次数"来决定**执行资格**这个思路本身就错了。正确做法：
- 该模式存在于 V2、旧 V3、1559 V3 和 Moe 四个 active service；以下修复必须全覆盖或显式退役未迁移入口
- **彻底移除 appearance-based 的执行抑制**。出现次数不参与执行资格；资格仍必须通过当前阶段定义的全部门禁，包括 snapshot/coverage 新鲜度、当前净利润、余额/仓位与亏损预算、成功后 cooldown、断路器、链上 deadline 和 exact preflight。P1 先移除错误抑制，P2 补齐状态机与风险门禁
- "连续 N 块没出现才清理"这类逻辑**只能用于 tracker 的内存回收**（防 HashMap 无限增长），不能参与是否发单的判断
- 失败黑名单区分**永久性失败**（路径结构不可行）和**临时性失败**（余额/RPC/nonce），后者带 TTL 退避而非永久拉黑

### R8. 🔴 执行生命周期：队列制造陈旧交易 + 无法处理在途交易

service 1559 :585 用容量 64 的 mpsc，:593 是**单 worker 串行**：`while let Some(job)=rx.recv()`，每笔 `attempt_execution` 里 `pending_tx.watch().await?`（:1113）**阻塞到链上确认**才处理下一个 job。旧 V3/V2/Moe 的执行路径也等待 receipt；机会在队列/worker 中停留时链上状态早变了。状态机必须覆盖四个 active service 或显式退役未迁移入口。

**修复（需要一个执行状态机，不只是队列失效）**：
- latest-wins/版本失效（完整 snapshot id 变了就丢弃旧 job）**只能清理还没被消费的队列**——对"worker 已经 `send` 出去、正在 `watch` 等确认"的在途交易无能为力
- 应把**发现、发送、receipt 跟踪三者分离**：发送后不阻塞等确认，交给独立的 receipt tracker
- 把 **deadline、同 nonce 替换/取消** 纳入同一个执行状态机。一个 nonce 对应一个 intent，可包含原始与多个 replacement attempt/hash；`replaced` 不是终态，cancel 只有取消交易在 canonical 链达到配置的确认/finality 策略后才终态
- `Candidate` 携带完整 snapshot id 与 gas-profile identity；进入发送门前再次确认它仍是当前 canonical snapshot、profile 与 `BlockFeeContext`，不一致就丢弃并重算。余额调整、完整重模拟和最终 calldata 构造完成后，M2-3 按风险策略决定是否用 authorized hot executor 地址作为 `from` 做一次独立 `eth_call` 语义 preflight；gas limit 始终来自已验证 profile
- deadline 必须进入 executor 合约 ABI 并在链上检查；本地 pending timeout 只决定何时替换/取消，不能阻止已经广播的旧交易稍后执行
- 每次 replacement 都绑定最新 `BlockFeeContext` 与兼容的 gas profile，重建 exact request，重新应用 M2-3 风险策略（需要时才 `eth_call`），并受最新净利润与最大亏损预算约束；不能重新 estimate gas 或盲目 bump 到利润之上
- receipt 先进入 `included_unconfirmed`，核对 receipt block hash 仍 canonical 并达到配置确认/finality 策略后才 `finalized`；reorg 移除 receipt 时重开 intent，重同步 pending/latest nonce、余额和候选状态
- **注**：当前单 worker 下 nonce 冲突不会发生（修正了原 C3 对 nonce 优先级的判断）；但一旦按上面拆成异步发送 + receipt 跟踪，nonce 生命周期管理（F5）就变成前置依赖

---

## A. 工程阻断层：全量 target / 测试当前不绿

### A1. `Cargo.toml` 有 9 个失效的 example 声明 [已修正表述]

**修正**：不是"任何 cargo 命令都失败"。实际上 `cargo build`、`cargo check --lib`、四个真实服务各自的 `cargo check --example <name>` **都能成功**。失败的只是 `cargo check --all-targets`、`cargo test --no-run`，以及直接 `cargo run --example` 那 9 个失效 target。核心库和生产入口本身可编译。

失效声明在 target 解析阶段报错：

```
error: can't find example `agni_pool_probe` at path `examples/test/agni_pool_probe.rs`
... 共 9 个
```

根因：8 个文件从 `examples/test/` 移到了 `examples/protocols/agni/`，但 `Cargo.toml` 里的 `path=` 没跟着改；第 9 个 `fetch_failed_pools_ticks` 已被删除但声明还在。

| Cargo name | 声明路径（不存在） | 实际位置 |
| --- | --- | --- |
| `agni_pool_probe` | `examples/test/...` | `examples/protocols/agni/agni_pool_probe.rs` |
| `list_mantle_agni_pools` | `examples/test/...` | `examples/protocols/agni/...` |
| `get_all_agni_pools` | `examples/test/...` | `examples/protocols/agni/...` |
| `multi_pool_swap` | `examples/test/...` | `examples/protocols/agni/...` |
| `simple_swap_test` | `examples/test/...` | `examples/protocols/agni/...` |
| `simple_swap_with_executor` | `examples/test/...` | `examples/protocols/agni/...` |
| `multi_pool_swap_with_executor` | `examples/test/...` | `examples/protocols/agni/...` |
| `monitor_pools_for_v2` | `examples/test/...` | `examples/protocols/agni/...` |
| `fetch_failed_pools_ticks` | `examples/protocols/agni/...` | **不存在，删声明** |

**修复**：改 8 个路径 + 删 1 个声明。它属于 README P0 的工程恢复项，但排在撤资止血、callback 修复和动态 gas 之后。

> 注：核心库 `cargo check --lib` 本身是过的（0 error，22 warning）。挡住一切的只是 example 声明。

### A2. forge-std submodule 未初始化 —— 合约编译失败

`.gitmodules` 声明了 `contracts/lib/forge-std`，但 `git submodule status` 显示 `-8bbcf6e…`（前导 `-` = 未初始化），目录为空。任何 forge 构建/部署会失败。

**[已修正] `.gitignore` 忽略 `contracts/lib/` 不是失败原因**：已验证该路径在 index 里是 gitlink（mode 160000），`git check-ignore` 对它返回空——gitignore 不阻止 submodule 初始化。未初始化只是因为没跑 init 命令。

**修复**：`git submodule update --init`。可顺手把 `.gitignore` 的 `contracts/lib/` 规则移除以减少混淆，但它不影响功能。

### A3. 4 个过时 target 编译失败

修好 A1 后，仍有 4 个 target 编译不过（都是被 alloy 升级甩下的老代码）：

- `examples/subscribe.rs:8` — `unresolved import dotenvy`（依赖里是 `dotenv` 不是 `dotenvy`）
- `examples/swap_calldata.rs:27` — 方法签名变了（5 参数传了 4 个）
- `examples/test/execute_sepolia_arbitrage.rs` — 13 个错误（老 API）
- `tests/moe_swap.rs` — 8 个错误：`alloy::primitive_types` 路径失效、`ProviderBuilder::timeout` 不存在、`tracing_subscriber::EnvFilter` 需要 feature、`block_timestamp` 未定义

**修复**：这些都不是生产入口。删除 `subscribe.rs`/`swap_calldata.rs` 和 `execute_sepolia_arbitrage`（后者同时摘声明）。`tests/moe_swap.rs` 不应为了全绿直接删：修到能编译，把 credentialed live-RPC case 显式 `#[ignore]`/feature-gate，并补最小离线确定性 fixture；等 M2 差分测试覆盖同一行为后才替换。

**另有一个第一版漏报的编译期错误**：`cargo test --lib`（跑库单测）会因 `src/amms/moe/math/uint256x256_math.rs:185` 的 `1_u64 << 200` 而失败——u64 左移 200 位溢出，Rust 常量求值直接编译错。它在一个 `#[test]` 里，所以只影响库测试构建，不影响 `--lib` check 和生产运行。修：改成 `U256::from(1u64) << 200` 或用 `U256` 字面量。

### A4. 缺失数据文件

- `data/poolLists_moe.csv` — Moe 服务 (`moe_monitor_executor_service.rs:812`) 和 `tests/moe_swap.rs` 引用，不存在（服务会 warn 回退到 `poolLists.csv`，但那里是 Agni 池，Moe 拿去用是错的）
- `data/poolLists_v2.csv` — `examples/test/monitor_pools.rs:204` 引用，不存在

**修复**：M0 从 canonical Moe factory 生成并验证专用列表，删除错误的 Agni fallback。`poolLists_v2.csv` 只服务旧 manual example；在 M3-10 跨协议 manifest 落地前，该 target 对缺文件必须 fail loudly/保持非生产，不能伪造或静默回退。最终由版本化 static pool/path manifest 统一替代各协议手写 CSV（见 `TODOs.md`/06-M3-10）。

### A5. `build.rs` 默认跳过 forge

`build.rs` 只有在 `SKIP_FORGE=0` 时才编译 batch-request 合约，正常 `cargo build` 不会触发 solc/forge。这本身是有意的（避免每次构建都要 solc），但意味着如果 batch-request 合约的 `.sol` 改了，字节码不会自动更新——容易踩坑，需在文档里写清。此外 `foundry.toml` 里硬编码了 `solc = "/opt/homebrew/bin/solc"`（两个 foundry.toml 都是），换机器就废。

---

## B. 正确性层：能跑，但结果不可信（直接影响盈亏）

### B1. 🔴 最优仓位超过 executor 余额时，机会被直接丢弃

服务里仓位寻优的搜索区间是**写死的常量**，与 executor 合约实际持有的 WMNT 无关：

```rust
// v3_monitor_executor_service_1559.rs:1128
const MIN_INPUT: u128 = 1_000_000_000_000;              // 1e-6 WMNT
const MAX_INPUT: u128 = 1_000_000_000_000_000_000_000_000; // 1e6 WMNT
```

寻优找出的"最优输入"可能是 500 WMNT，但真正下单时才检查余额：

```rust
// :1034
if executor_balance < candidate.input {
    warn!("Executor contract balance insufficient");
    return Err(...);   // ← 整个机会被放弃
}
```

后果：只要最优仓位 > 注资额，机会**不是按可用余额缩小重算，而是整个扔掉**。如果你注资较少，这极可能就是"日志里看得到正利润路径、却从来不成交"的直接原因。**这是恢复运行后要验证的第一嫌疑。**

**修复**：`max_input = min(MAX_INPUT, executor_balance)`，在寻优**之前**就把余额作为上界传进 `best_path_simulation`。

### B2. 🔴 库里的仓位寻优算法是错的（`PathOptimizer::optimize`）

`src/arbitrage/optimizer.rs:45` 的 `optimize` 用的是**对布尔谓词做二分**（"利润 > 阈值吗"），profitable 时 `low=mid+1`、否则 `high=mid-1`（optimizer.rs:61-74）。这个搜索目标本身就是错的：它优化的是"是否盈利"这个布尔量，只会把 `low` 往区间顶端推，返回最后一个盈利样本，而**不是利润最大的仓位**。

而且 `high = initial_guess.min(max_input)`，`initial_guess=1e18` 写死、`max_input` 默认 1e24，所以 `high` 永远被卡在 1e18，配置的 `max_input` 是死的（optimizer.rs:56-58）。

好在生产服务**没用这个函数**，自己在 `best_path_simulation`（service 1559 :1156）写了爬山搜索（三点起始 + 步长减半爬山 64 次）。但库版留着是误导，未来谁引用谁踩坑。

**修复**：让服务、库和 `MockArbitrageContext` 共用同一份替代实现，通过 mock/replay 与差分测试后再删 `PathOptimizer::optimize`。当前 `mock.rs:260` 仍调用它，不能先删函数再保留“离线测试设施”。**注意新算法不能假设利润是凹/单峰函数**——V3 跨 tick、LB 跨 bin、整数舍入、输入相关 gas 会造成分段甚至局部峰，纯三分/黄金分割会收敛到局部最优。正确做法是：对数尺度粗采样定位候选峰区间 → 每个区间内局部优化 → 显式检查端点。详见 [03-performance.md](03-performance.md) P3。

### B3. 🔴 2-hop "misprice" 的利润是拿两种不同代币相减，无意义

`pathfinder.rs:126` 的 `find_two_pool_misprices` 产出的是 `A→node→B` 路径，**起点 A ≠ 终点 B**（不是环）。但 `optimizer.rs:132` 的 `simulate_path` 算 `profit = final_amount − amount_in`——拿 B 的数量减 A 的数量，两种代币、两种精度直接相减，纯粹是垃圾数字。这些数字还会流进 `opportunistic_scan` 当成机会（monitor.rs:104）。

**[已修正] 关于服务侧影响**：第一版说这是服务侧"大量重复路径"的来源——**不准确**。生产服务配置了 required start=end=WMNT，而 `find_two_pool_misprices` 产出的是 `A→node→B`（首尾必不同，pathfinder.rs 里还排除相同首尾邻居），二者矛盾，所以该 finder 在服务中**恒为空**、不产出任何路径。**库层的 bug 成立，但在服务里是休眠的。** "大量重复路径"（对应 `tech-docs/Agni/TODO.md`）的真实来源另需排查（更可能是 `find_cycles` 对同一池子集合产出多条方向/起点变体），不是 misprice。

**修复**：库层——明确 misprice 语义，要么改成只产出策略配置的 `settlement_asset` 闭环、要么删掉。服务层去重——**去重 key 必须保序、保方向**：用有序的 `(pool, token_in, token_out)` hop 序列，而不是"池子集合"。同一组池的不同顺序/方向/token path 是**不同的经济路径**，按无序集合去重会误删真机会。对闭环只需做旋转（rotation）归一化；`settlement_asset` 固定起点后通常连旋转去重都不需要。**动手前先记录并证明"大量重复"的真实来源**（打点统计哪些 hop 序列真正重复），别凭猜测去重。

### B4. 🔴 Moe LB 定价用储备比 —— LB 池根本不这么定价

`moe/mod.rs:853-872` 的 `calculate_price` 用 `reserve_x/reserve_y`。但 Liquidity Book 的价格由 `active_id` 决定：`price = (1 + bin_step/1e4)^(active_id − 2^23)`，跟储备比没关系（尤其 bin 分布不对称时差得很远）。正确的 `get_price_from_id` 就在 `moe/mod.rs:340-345`，但没被 `calculate_price` 用。

### B5. 🔴 Moe LB 的 bins 在正常同步路径上永远是空的 → 精确 LB 数学是死的

- `MoeFactory::sync_all_pools`（`moe/mod.rs:944`）只同步 slot0 和 decimals，**从不填充 `bins`**
- 唯一能填 `bins` 的 `sync_active_bins_batch`（`moe/mod.rs:525`）在**库内部零调用**（只有 example 服务显式调用）
- `simulate_swap` 只在 `!bins.is_empty()` 时走精确路径（`moe/mod.rs:783`），否则回退到常量乘积

好消息：Moe 生产服务确实**显式调用了** `sync_active_bins_batch`（`moe_monitor_executor_service.rs:943`），所以服务侧不完全是死的。但——

### B6. 🔴 Moe 常量乘积回退的费率是错的 + Swap 事件不更新储备

即使 bins 为空回退时：
- `moe/mod.rs:802` 的 `total_fee_bps = bin_step + protocol_share_bps` 当成 `/10000`——`bin_step` 根本不是 bps 费率（LB 费是 `base_factor * bin_step * 1e10`），量纲就错了
- `moe/mod.rs:672-719` 的 `sync` 在 Swap 事件里**只更新 `active_id`，故意不更新储备**（代码里有长注释解释），配合空 bins 就是拿越来越陈旧的储备算

**修复**：确认 Moe 服务的 bins 同步节奏能覆盖每次报价（现在是在哪个时点同步的？块间会不会漏？）；修 `calculate_price` 用 `active_id`；要么修好回退费率、要么在 bins 缺失时直接跳过该池而不是用错误公式硬算。

### B7. 🔴 Agni `simulate_swap_mut` 是空操作 —— 多跳串联用的是陈旧状态

`agni/mod.rs:300-312`：clone 自己 → 调 `tmp.simulate_swap(&self)`（`simulate_swap` 收 `&self`，**不改 tmp**）→ 把没变的 `tmp.sqrt_price/tick/liquidity` 抄回 self。池状态从没被推进。对比 UniswapV3（`uniswap_v3/mod.rs:408-558`）是完整正确实现。任何依赖 `simulate_swap_mut` 做序列模拟的 Agni 路径都会用错状态。

需确认生产服务走的是 `simulate_swap`（每跳独立、不 mut）还是 `simulate_swap_mut`——服务 1559 :1150 用的是 `simulate_swap`，逐跳把上一跳输出当输入，所以服务侧**恰好绕开了**这个 bug。但库接口是坏的，要么修要么删。

**实施顺序**：把它作为 P1 的独立模拟正确性修复，在 M2 差分测试和 P3 共享内核重构之前完成。差分测试只能消费已经正确的 mutable 实现，不能把修复本身留给被差分测试阻塞的 P3。

### B8. 🔴 三套互相矛盾的 gas 表

| 来源 | 1 hop | 2 hop | 3 hop | 4 hop |
| --- | --- | --- | --- | --- |
| `arbitrage/gas.rs:55` | 300M | 900M | 1.5B | 2.8B |
| `execution/gas_schedule.rs:21` | 300M | 900M | 1.5B | 2.8B |
| `execution/executor.rs:67` `compute_fee_plan` | 600M* | **450M** | 600M* | **750M** |

`compute_fee_plan`（**这个是生产服务真正用的**，service 1559 :1073）只特判 2/4 hop，1/3 hop 落到默认 `config.gas_limit`（600M），结果 3-hop 拿到的 limit 比 2-hop 还少。三张表两两不一致，全是硬编码，没有任何 `estimate_gas` 调用。

**修复（与 R1 统一方案一致）**：删除三套人工常量和所有生产 `eth_estimateGas` gas-sizing 分支。WHI-546 用 receipt + fork/replay 生成按 executor/route complexity 版本化的 measured profile；WHI-502 启动时验证并加载，热路径内存查表，未知 key/code hash/schema/digest fail closed。`gas_limit` 取有 holdout 证明的长尾安全上限并受当前 block limit 约束；利润评估使用独立的保守 `expected_gas_used`。现有 300M–2.8B 均超过证据块的 60M，是坏值。

### B9. 🟠 精度/单位相关

- `execution/executor.rs:76`：`net_expected.to_string().parse::<u128>().unwrap_or(0)`——U256 转字符串再 parse，溢出时静默变 0
- `arbitrage/gas.rs:78,103`：利润（结算资产最小单位）和 gas 成本（MNT wei，18 位）直接相减/比较，**只有结算资产是 WMNT 时才同单位**。当前 executor 还用 immutable `WMNT` 做投入和最终余额检查，因此本部署必须验证 `settlement_asset == executor.WMNT == wrapped native gas asset`。若未来允许其他结算资产，必须同时泛化合约并定义 native gas 到结算资产的可靠换算，不能直接相减。
- Agni `fee_protocol` 用硬编码魔数表算出来（`agni/mod.rs:339-344`，值 `216272100` 等）却从不被读取——死字段还误导人。

---

## C. 安全层：资金风险

### C1. 🔴 executor 合约的 V3 回调鉴权过弱

`contracts/executor/ArbitrageExecutor.sol:109` 的 `agniSwapCallback` 只有 `require(msg.sender != tx.origin)`（:115）防 EOA 直调，但**没有校验 `msg.sender` 是不是真的 pool**（没有用 factory + initcode hash 反推 pool 地址）。任何合约都能调这个回调、自称是 pool，从 executor 骗走 `amountDelta` 的转账。

**⚠️ 这个洞与协议无关，只要 executor 持有代币就能被利用。** `agniSwapCallback` 是一个 external 函数，攻击者不需要经过 `onlyOwner` 的 `executeArbitrage`——可以**直接**调用回调、自称是 pool、把 `amountDelta` 转给自己。所以"因为 executeArbitrage 是 onlyOwner 所以攻击面受限"是**错的**：回调本身就是独立入口。只要 executor 里躺着 WMNT，任意攻击合约就能直接掏。

**因此止血范围不是"暂停 Moe 执行"，而是**：
1. 立即停 V2/V3/Moe 全部 sender，但明确这**不会**关闭链上 callback；先提走已知高价值 token（尤其 WMNT），同时根据注资记录、历史 path token 和 Transfer 记录补全旧 executor 的 ERC-20 inventory，再逐个调用 `withdraw(token)` 并核对余额
2. 单独查询 native MNT。合约 `receive()` 能收 MNT，但当前没有 native withdrawal；若 native balance 非零，它无法从旧地址取回，必须记录为滞留资金并永久停用该地址，不能声称“全部余额归零”
3. 新 executor 增加 owner-only native withdrawal；callback 校验 factory-derived pool 地址或 canonical allowlist，且执行路径对所有协议验证 pool provenance
4. M0-8 完成 callback/path provenance/角色/安全 transfer/交易局部余额/minProfit/deadline 测试，生成可复现的最终 bytecode/codehash、ABI 并迁移 Rust 调用方；**不在该 issue 部署或注资**
5. M2-7 在隔离的 Mantle Sepolia E2E 首次部署 exact M0-8 工件；M2-8 approve 后，M2-9 才在主网部署并验证 exact codehash/角色/配置，且必须停在 paused + unfunded；M2-9 证据经第二次人工 go/no-go 后，M2-10 才能限额注资和 canary

旧 executor 从第 1 步起不应再接收任何 ERC-20 或被用于执行——这优先于一切功能修复。M0-8 完成也**不等于已经部署**；P2.5 approve 本身也不改变链上状态，只允许 M2-9 部署/核验。资金暴露和 canary 需要 M2-10 的第二次人工批准；merged binary 还需 M3-9 独立放行。

### C2. 🟠 缺显式最终 minProfit/deadline，且中间资产按总余额传递

- 当前合约把 `_amountsOut[last]` 复用成最终 WMNT 下界，语义隐蔽且 Moe 调用方传全零；replacement ABI 改为显式 `minProfit`，并统一要求 `balanceAfter >= balanceBefore + minProfit`
- 当前 `executeArbitrage` 没有 deadline（`.sol:144`），`swap_executor.rs` 全传 `U256::MAX`（:186,289）；replacement 增加有限 `deadline` 并在第一次外部调用前检查
- 不把“所有协议每跳都必须人为 minOut”设成通用硬要求：V2 router 等协议原语保留必要字段，V3 callback/LB 路径以受信 provenance、交易局部 delta、最终 minProfit、deadline 和精确 off-chain 重模拟形成统一安全边界。EVM revert 会原子回滚所有 hop，因此 adverse 中间输出最终未达 `minProfit` 时损失的是 sender gas，不是 executor 库存；逐跳 bound 的附加价值是更早失败、可能少烧后续 gas。若某 venue 的协议语义强制或证明需要 hop-local bound，由该 adapter 明确提供；通用 early-abort 方案只能在测得净 gas/false-reject 收益后再加

另有独立库存风险：`_executeSwaps` 在下一跳把 `amountToSwap` 设为中间 token 的**合约总余额**（`.sol:239-242`），会把交易前已有的 dust/库存一起送入路径；最终只检查 WMNT，无法发现中间 token 损失。replacement 必须按本 hop 的 `tokenOut` 前后余额 delta（或协议精确返回值）得到下一跳 input，并保证每种中间 token 的交易前余额不下降。最简单的正确基线是所有 hop 先回到 executor、按 delta 传递；直接路由到下一池只能在保持等价 delta 计量并有差分测试后再优化。

**[已修正] 关于"最终检查保证不亏"**：旧 ABI 只在调用方正确构造 `_amountsOut[last]` 时生效。V3 服务用 `.max(candidate.input)`，确实挡住本金损失但没有表达正利润阈值；Moe 传全零，允许整笔 input 亏光。M0-8 把这两个分支合并成协议无关的显式 `minProfit` 契约；M0-3 还必须在余额缩小时用新 input 完整重模拟后才构造请求。

### C3. 🟠 提交通道 / nonce 管理

- 所有提交走普通 provider `send()`（executor.rs:345 等）。是否存在私有/优先通道 **[已修正为待核实]**：Mantle 是中心化 sequencer，经典 mempool 抢跑弱；但"Mantle 生产网是否提供私有/优先提交通道"缺乏权威证据（官方能查到的是 fair-sequencing 研究提案，不等于已上线能力）。这一项在确认 Mantle 当前实际排序机制前不下定论
- `NonceManager`（nonce.rs）写好了但**从没接线**，靠 alloy 默认每笔查链填 nonce
- **[已修正] nonce 优先级下调**：因为执行是**单 worker 串行**（见 R8），同块并发发多笔的 nonce 竞争当前**不会发生**，所以 nonce 冲突不是首要问题。真正缺的是"nonce too low"重发、gas bump 替换、dropped-tx 处理——这些在把执行改成并发/带替换之后才需要

---

## D. 稳健性层：会导致静默错误/漏机会

### D1. 🔴 reorg 处理是死的（`StateSpace.latest_block` 永远为 0）

代码里有**两个**叫 `latest_block` 的 `Arc<AtomicU64>`：
- `StateSpaceManager.latest_block`（mod.rs:43）每块更新（:80）
- `StateSpace.latest_block`（mod.rs:248）由 `default()` 建出来永远是 0，`StateSpace::sync` 读它做 reorg 判断（:262）

`StateSpace::sync` **从不 store 自己的 latest_block**，所以 `latest` 永远是 0，unwind 分支 `if latest >= block_number`（:272）永不触发，缓存快照永远用不上。**reorg 回滚实际被禁用了。** 两个 Arc 也从没被连起来。

不过生产服务根本没用 `StateSpaceManager`，是自己裸写块循环，所以这属于"库里的死设施"，见 02 的架构决策。

### D2. 🔴 深 reorg 会 panic + 缓冲太薄

`cache.rs:47-49`：`block_to_unwind < oldest_block` 时直接 `panic!`。`CACHE_SIZE=30`（mod.rs:38）+ Mantle 2 秒块 = 只存 ~60 秒历史，超过 30 块的 reorg 直接 panic 而不是报错。

### D3. 🔴 WS 块流丢块无回补

`subscribe`（mod.rs:74）只为 WS 推来的那个块拉 logs。公链 WS 断连重连会跳块，中间的 logs 永不补拉——**状态静默漂移**。

### D4. 🟠 eth_getLogs 窗口大小不统一且部分超限

- UniswapV3：1000 块窗口（uniswap_v3/mod.rs:773，代码注释声称 endpoint 限 10000）；这是当前实现经验值，不是所有 provider 的链级常量
- Agni：**90000 块**步长（agni/mod.rs:528）——远大于另外两处策略，容易触发 provider 的 range/result 限制
- Moe：一次性无分块从 `creation_block` 拉到链顶（moe/mod.rs:931）——老 factory 极易触发 range/result 限制

**修复**：历史 discovery/backfill 使用可配置初始窗口，并在 provider 返回范围/结果数限制时二分缩小重试；记录实际 endpoint 能力。live snapshot 优先逐 block hash 拉 logs，不把“10000 块”写成协议保证。

### D5. 🟠 Moe 各种"合理性"钳位会静默返回 0

`moe/mod.rs:366-391` 若输出 > 储备 / > `amount_in*1000` / > `amount_in*100`(小额) 就返回 `U256::ZERO`；`MAX_REASONABLE_RESERVE=1e30`（:34）超了也当 0。Mantle 高名义供应量代币可能真的超 1e30，导致**合法池/合法大额套利被静默当成 0**，既掩盖真机会也掩盖真 bug。

**修复**：删除没有协议/链上数学依据的 “reasonable” 阈值；使用 checked `U256`、ABI 字段范围、真实储备/coverage 和协议不变量判定。只有真实算术溢出、协议不变量破坏或 coverage 不完整才返回 typed error；一个储备 >1e30 但字段/协议均合法的 fixture 必须正常报价，不能仅把旧阈值的零返回改成错误返回。

---

## E. 编译告警（非阻断但值得清）

`cargo check --lib` 有 22 个 warning，`--all-targets` 更多。集中在：
- 大量 `unused import` / `never used` 函数（Moe `calc_*` 6 个、`price_liquidity`、`price_from_id` 等）
- `moe/math/mod.rs:20,24` ambiguous glob re-export（`decode`/`encode` 被多个模块重复导出）
- `unused_crate_dependencies`：`chrono`、`once_cell`、`serde_json` 在库里没用到
- `moe/mod.rs:1538` 赋值 `amount_left` 后从未读取

这些不影响运行，但数量大到会淹没真正重要的 warning，建议清理（详见 05）。
